//! Shared `Setting`-table access for the admin's config surfaces (site settings and
//! plugin config). Both persist typed JSON values into the key/value `Setting`
//! singleton (`key @unique`, `value: String` holding JSON), so the read-one and
//! upsert logic lives here once rather than being duplicated per surface.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use ferropress_core::SETTING_TYPE;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};

use super::AdminError;
use crate::AppState;

/// Fetch the `Setting` row for `key` (`@unique`, so at most one). There is no
/// get-by-key store verb; filter on the indexed key and take the first.
pub(crate) async fn load_setting(
    state: &AppState,
    key: &str,
) -> Result<Option<Object>, AdminError> {
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
/// true. A create is only reached when no row exists; `key` is `@unique`, so a
/// concurrent double-create of the SAME absent key would collide — acceptable for the
/// single-admin surface (a lost concurrent write is recovered by re-saving), and the
/// common path is update.
pub(crate) async fn upsert_setting(
    state: &AppState,
    key: &str,
    value: &JsonValue,
) -> Result<(), AdminError> {
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
