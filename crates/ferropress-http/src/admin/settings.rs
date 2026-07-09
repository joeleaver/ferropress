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

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use ferropress_core::role::Capability;
use ferropress_render_form::{FormSchema, SettingRefs, schema_for_settings};

use super::setting_refs::build_settings_dto;
use super::setting_store::upsert_setting;
use super::{AdminError, AdminJson, AuthedUser};
use crate::AppState;

/// `GET`/`PUT` response: the declarative schema, the current value for every key
/// (defaults overlaid with whatever is stored), and the resolved `refs` the
/// id-valued widgets (`MediaPicker`/`EntityRef`) need to render (see
/// [`SettingRefs`]). Returning all three on write lets the client re-sync from one
/// round-trip. `refs` is empty for a schema that uses neither widget.
#[derive(Serialize)]
pub struct SettingsDto {
    pub schema: FormSchema,
    pub values: serde_json::Map<String, JsonValue>,
    pub refs: SettingRefs,
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
    Ok(Json(build_settings_dto(&state, schema, values).await?))
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
    Ok(Json(build_settings_dto(&state, schema, values).await?))
}

/// The current value for every schema key: the schema defaults overlaid with any
/// stored `Setting`. Delegates to the shared serve-side loader
/// ([`ferropress_serve::load_values`]) so the admin form and the public read path
/// resolve a stored setting by exactly ONE overlay rule — the form and the served
/// page can never disagree about what a setting means.
async fn read_values(state: &AppState) -> Result<serde_json::Map<String, JsonValue>, AdminError> {
    Ok(ferropress_serve::load_values(&state.store).await?)
}
