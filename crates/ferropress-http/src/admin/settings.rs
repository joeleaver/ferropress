//! Site settings: read + write the well-known `Setting` keys the admin's Settings
//! page exposes. Gated on [`Capability::ManageSettings`] (Administrator only).
//!
//! The wire shape is the declarative [`FormSchema`] (from `ferropress-render-form`)
//! plus a `key -> JSON value` map. The SAME schema is used to (a) render the form
//! in the wasm SPA and (b) validate a submission here — so an untrusted `PUT` can
//! only ever write keys the schema declares, with values coerced to each widget's
//! type. This is the settings counterpart to the posts handlers: state is reached
//! only through the injected [`RhypeStore`](ferropress_core::store::RhypeStore).
//!
//! `Setting` is a plain key/value singleton (`key @unique`, `value: String`,
//! `autoload: Bool`). The typed value is JSON, stored as a String — encode on
//! write, parse on read. There is no store `upsert` verb, so a write is
//! filter-by-key then branch update-vs-create.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use ferropress_core::SETTING_TYPE;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::role::Capability;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};
use ferropress_render_form::{FormSchema, schema_for_settings};

use super::{AdminError, AdminJson, AuthedUser};
use crate::AppState;

/// `GET`/`PUT` response: the declarative schema plus the current value for every
/// key (defaults overlaid with whatever is stored). Returning both on write lets
/// the client re-sync from one round-trip.
#[derive(Serialize)]
pub struct SettingsDto {
    pub schema: FormSchema,
    pub values: serde_json::Map<String, JsonValue>,
}

/// `PUT` body: a sparse map of `key -> value` edits. Unknown keys are ignored (the
/// schema is the whitelist); known keys are type-checked + normalized.
#[derive(Deserialize)]
pub struct PutSettingsRequest {
    #[serde(default)]
    pub values: serde_json::Map<String, JsonValue>,
}

/// `GET /admin/api/settings` — the settings schema + current values.
pub async fn get(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<SettingsDto>, AdminError> {
    who.require(Capability::ManageSettings)?;
    let schema = schema_for_settings();
    let values = read_values(&state).await?;
    Ok(Json(SettingsDto { schema, values }))
}

/// `PUT /admin/api/settings` — validate a submission against the schema and persist
/// each changed key. Returns the fresh schema + values so the client re-syncs.
pub async fn put(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<PutSettingsRequest>,
) -> Result<Json<SettingsDto>, AdminError> {
    // Authorize BEFORE touching or validating anything.
    who.require(Capability::ManageSettings)?;

    let schema = schema_for_settings();
    // Validate + normalize against the schema. Unknown keys dropped; a type
    // mismatch / unsafe URL / out-of-vocabulary choice is a 400 (never persisted).
    let clean = schema.coerce_values(&body.values).map_err(|errs| {
        let detail = errs
            .iter()
            .map(|e| format!("{}: {}", e.key, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        AdminError::BadRequest(format!("invalid settings — {detail}"))
    })?;

    for (key, value) in &clean {
        upsert_setting(&state, key, value).await?;
    }

    let values = read_values(&state).await?;
    Ok(Json(SettingsDto { schema, values }))
}

/// The current value for every schema key: the schema defaults overlaid with any
/// stored `Setting`. Delegates to the shared serve-side loader
/// ([`ferropress_serve::load_values`]) so the admin form and the public read path
/// resolve a stored setting by exactly ONE overlay rule — the form and the served
/// page can never disagree about what a setting means.
async fn read_values(state: &AppState) -> Result<serde_json::Map<String, JsonValue>, AdminError> {
    Ok(ferropress_serve::load_values(&state.store).await?)
}

/// Fetch the `Setting` row for `key` (`@unique`, so at most one). There is no
/// get-by-key store verb; filter on the indexed key and take the first.
async fn load_setting(state: &AppState, key: &str) -> Result<Option<Object>, AdminError> {
    Ok(state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: Compare::Eq,
            value: Value::String(key.to_owned()),
            limit: Some(1),
        })
        .await?
        .into_iter()
        .next())
}

/// Write one setting: update the existing row, or create it. `value` is stored as a
/// JSON-encoded String (the SDL column is `String`, not `Json`); `autoload` is set
/// true (site settings are read on most renders). A create is only reached when no
/// row exists; `key` is `@unique`, so a concurrent double-create of the SAME absent
/// key would collide — acceptable for the single-admin surface (a lost concurrent
/// write is recovered by re-saving), and the common path is update.
async fn upsert_setting(state: &AppState, key: &str, value: &JsonValue) -> Result<(), AdminError> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| AdminError::BadRequest(format!("value for {key:?} is not serializable")))?;

    match load_setting(state, key).await? {
        Some(existing) => {
            let mut patch: FieldMap = HashMap::new();
            patch.insert("value".to_owned(), Value::String(encoded));
            state
                .store
                .update(&TypeName::from(SETTING_TYPE), existing.id, patch)
                .await?;
        }
        None => {
            let mut fields: FieldMap = HashMap::new();
            fields.insert("key".to_owned(), Value::String(key.to_owned()));
            fields.insert("value".to_owned(), Value::String(encoded));
            fields.insert("autoload".to_owned(), Value::Bool(true));
            let _: ObjectId = state
                .store
                .create(&TypeName::from(SETTING_TYPE), fields)
                .await?;
        }
    }
    Ok(())
}
