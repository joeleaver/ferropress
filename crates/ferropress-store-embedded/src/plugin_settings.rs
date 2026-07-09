//! `impl PluginSettingsReader for EmbeddedStore` — the synchronous
//! `plugin_settings` capability backend the plugin host exposes (as the
//! `fp_get_setting` host function) to plugins granted `plugin_settings`.
//!
//! SYNCHRONOUS by design, exactly like [`content_reader`](crate::content_reader)
//! and [`content_writer`](crate::content_writer): the plugin host calls this from
//! inside a synchronous WASM host function, so it drives the engine's
//! `filter_scan_str` `Setting`-key index DIRECTLY on the calling thread rather than
//! through the async [`RhypeStore`] `spawn_blocking` path.
//!
//! A plugin reads only its OWN configuration: the host passes the calling plugin's
//! id as `namespace`, and this resolves the single `Setting` at
//! `plugin.{namespace}.{key}` (via [`plugin_setting_key`]). Reading a core `site.*`
//! setting or another plugin's namespace is structurally impossible — the caller
//! never supplies the `plugin.<id>.` prefix.

use ferropress_core::SETTING_TYPE;
use ferropress_core::entity::plugin_setting_key;
use ferropress_core::error::Result as CoreResult;
use ferropress_core::plugin_caps::PluginSettingsReader;
use ferropress_core::query::Compare;
use ferropress_core::value::Value;

use crate::{AdapterError, EmbeddedStore, convert};

impl PluginSettingsReader for EmbeddedStore {
    fn get_setting(&self, namespace: &str, key: &str) -> CoreResult<Option<serde_json::Value>> {
        if namespace.is_empty() || key.is_empty() {
            return Ok(None);
        }
        // `key @unique`, so at most one row answers the fully-qualified key.
        let full = plugin_setting_key(namespace, key);
        let op = convert::to_compare_op(Compare::Eq);
        let hit = self
            .db()
            .filter_scan_str(SETTING_TYPE, "key", op, &full, Some(1))
            .map_err(AdapterError::from)?
            .into_iter()
            .next()
            .map(convert::from_db_object);

        // The `Setting.value` column is a JSON-encoded String (SDL `value: String`).
        // A stored value that fails to parse reads as unset (the host then falls back
        // to the schema default) rather than erroring the guest.
        match hit.as_ref().and_then(|obj| obj.get("value")) {
            Some(Value::String(raw)) => Ok(serde_json::from_str(raw).ok()),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferropress_core::SETTING_TYPE;
    use ferropress_core::entity::plugin_setting_key;
    use ferropress_core::plugin_caps::PluginSettingsReader;
    use ferropress_core::store::RhypeStore;
    use ferropress_core::value::{FieldMap, TypeName, Value};

    use crate::EmbeddedStore;

    async fn put_setting(store: &EmbeddedStore, key: &str, json: &str) {
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("key".to_owned(), Value::String(key.to_owned()));
        fields.insert("value".to_owned(), Value::String(json.to_owned()));
        fields.insert("autoload".to_owned(), Value::Bool(true));
        RhypeStore::create(store, &TypeName::from(SETTING_TYPE), fields)
            .await
            .expect("seed setting");
    }

    #[tokio::test]
    async fn reads_own_namespace_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));

        // A plugin's own setting, a DIFFERENT plugin's, and a core key.
        put_setting(
            &store,
            &plugin_setting_key("callout", "default_variant"),
            "\"warning\"",
        )
        .await;
        put_setting(
            &store,
            &plugin_setting_key("other", "default_variant"),
            "\"danger\"",
        )
        .await;
        put_setting(&store, "site.title", "\"My Site\"").await;

        // Reads its own value (JSON-parsed).
        assert_eq!(
            store.get_setting("callout", "default_variant").expect("ok"),
            Some(serde_json::json!("warning"))
        );
        // The same bare key in another plugin's namespace is a DIFFERENT row.
        assert_eq!(
            store.get_setting("other", "default_variant").expect("ok"),
            Some(serde_json::json!("danger"))
        );
        // An unset key is None (the host overlays the schema default).
        assert_eq!(store.get_setting("callout", "nope").expect("ok"), None);
        // Empty namespace/key never looks up (can't reach a core `site.*` key).
        assert_eq!(store.get_setting("", "title").expect("ok"), None);
        assert_eq!(store.get_setting("callout", "").expect("ok"), None);
    }
}
