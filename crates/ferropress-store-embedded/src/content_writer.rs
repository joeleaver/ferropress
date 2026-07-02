//! `impl ContentWriter for EmbeddedStore` — the synchronous `content:write`
//! capability backend the plugin host exposes (as the `fp_create_page_stub` /
//! `fp_set_meta` host functions) to plugins granted `write_store`.
//!
//! SYNCHRONOUS by design, exactly like [`content_reader`](crate::content_reader):
//! the plugin host calls this from inside a synchronous WASM host function, so it
//! drives the engine (`create` / `get` / `update` / `filter_scan_str`) DIRECTLY on
//! the calling thread rather than through the async [`RhypeStore`] `spawn_blocking`
//! path.
//!
//! The surface is DELIBERATELY TIGHT (see [`ContentWriter`]): create a *draft*
//! stub Page, or set one key inside a Post/Page `meta` JSON object. No core field
//! (`slug`/`status`/…) is ever writable through here, and only Post/Page accept a
//! `meta` write, so a `write_store` grant can't corrupt the content model.
//!
//! FEED-LOOP: a write here commits and emits a `ChangeEvent`; the action-hook
//! bridge must not re-dispatch a plugin's own write (see rhypedb#13). This backend
//! is therefore NOT wired into the production composition root until that guard
//! exists — it is exercised only in isolation (deny-by-default holds: an un-backed
//! `write_store` plugin fails to instantiate).

use ferropress_core::block::BlockTree;
use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::plugin_caps::ContentWriter;
use ferropress_core::query::Compare;
use ferropress_core::value::{FieldMap, Value, now_millis};
use ferropress_core::{PAGE_TYPE, POST_TYPE, Status};

use crate::{AdapterError, EmbeddedStore, convert};

impl ContentWriter for EmbeddedStore {
    fn create_page_stub(&self, slug: &str, title: &str) -> CoreResult<u64> {
        if slug.is_empty() {
            return Err(CoreError::Store(
                "create_page_stub: slug must not be empty".to_owned(),
            ));
        }

        // Dedup: if a Page already occupies this slug, return it rather than mint a
        // duplicate (slugs are the permalink key; two pages at one slug is a bug).
        let op = convert::to_compare_op(Compare::Eq);
        if let Some(existing) = self
            .db()
            .filter_scan_str(PAGE_TYPE, "slug", op, slug, Some(1))
            .map_err(AdapterError::from)?
            .into_iter()
            .next()
        {
            return Ok(existing.id);
        }

        // A stub is a DRAFT with an empty body — never published, so auto-creation
        // can never publicly expose content a human didn't approve.
        let empty_body = BlockTree::from_blocks(Vec::new()).to_json_value()?;
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
        fields.insert("title".to_owned(), Value::String(title.to_owned()));
        fields.insert(
            "status".to_owned(),
            Value::String(Status::Draft.as_str().to_owned()),
        );
        fields.insert("block_tree".to_owned(), Value::Json(empty_body));
        fields.insert("created_at".to_owned(), Value::DateTime(now_millis()));

        let obj = self
            .db()
            .create(PAGE_TYPE, convert::to_db_fields(fields))
            .map_err(AdapterError::from)?;
        Ok(obj.id)
    }

    fn set_meta(
        &self,
        type_name: &str,
        id: u64,
        key: &str,
        value: serde_json::Value,
    ) -> CoreResult<()> {
        // Tight surface: only Post/Page meta is writable. (User/Setting/etc. carry
        // meta too, but keeping the write target to permalinked content shrinks the
        // blast radius of a write grant; widen deliberately if ever needed.)
        if type_name != POST_TYPE && type_name != PAGE_TYPE {
            return Err(CoreError::Store(format!(
                "set_meta: type `{type_name}` is not writable (only {POST_TYPE}/{PAGE_TYPE})"
            )));
        }
        if key.is_empty() {
            return Err(CoreError::Store(
                "set_meta: key must not be empty".to_owned(),
            ));
        }

        // Read-modify-write the `meta` JSON object ONLY. Every other field is left
        // untouched, so a plugin can't reach a core/indexed field through here.
        let obj =
            convert::from_db_object(self.db().get(type_name, id).map_err(AdapterError::from)?);
        let mut meta = match obj.get("meta") {
            Some(Value::Json(serde_json::Value::Object(m))) => m.clone(),
            _ => serde_json::Map::new(),
        };
        meta.insert(key.to_owned(), value);

        let mut patch: FieldMap = FieldMap::new();
        patch.insert(
            "meta".to_owned(),
            Value::Json(serde_json::Value::Object(meta)),
        );
        self.db()
            .update(type_name, id, convert::to_db_fields(patch))
            .map_err(AdapterError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferropress_core::plugin_caps::ContentWriter;
    use ferropress_core::store::RhypeStore;
    use ferropress_core::value::{FieldMap, TypeName, Value};
    use ferropress_core::{PAGE_TYPE, POST_TYPE, Status};

    use crate::EmbeddedStore;

    #[tokio::test]
    async fn create_page_stub_makes_a_draft_and_dedups_on_slug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));

        let id = store
            .create_page_stub("orphan", "Orphan")
            .expect("create stub");

        // It exists, is a Page, and is a DRAFT (never auto-published).
        let obj = RhypeStore::get(
            store.as_ref(),
            &TypeName::from(PAGE_TYPE),
            ferropress_core::value::ObjectId(id),
        )
        .await
        .expect("get");
        assert!(matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Draft.as_str()));
        assert!(matches!(obj.get("title"), Some(Value::String(s)) if s == "Orphan"));
        // block_tree round-trips as native JSON (not a String).
        assert!(matches!(obj.get("block_tree"), Some(Value::Json(_))));

        // A second call for the same slug returns the SAME id (no duplicate).
        let again = store
            .create_page_stub("orphan", "Orphan (dup)")
            .expect("dedup");
        assert_eq!(again, id, "same slug must not mint a second page");
    }

    #[tokio::test]
    async fn set_meta_merges_one_key_and_leaves_other_fields_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));

        // Seed a Post via the async write path.
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("slug".to_owned(), Value::String("target".to_owned()));
        fields.insert("title".to_owned(), Value::String("Target".to_owned()));
        fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
        fields.insert(
            "status".to_owned(),
            Value::String(Status::Published.as_str().to_owned()),
        );
        let id = RhypeStore::create(store.as_ref(), &TypeName::from(POST_TYPE), fields)
            .await
            .expect("seed post");

        // Set a meta key.
        store
            .set_meta(
                POST_TYPE,
                id.0,
                "backlinks",
                serde_json::json!(["/a", "/b"]),
            )
            .expect("set_meta");

        // Merge a SECOND key — the first must survive (read-modify-write, not replace).
        store
            .set_meta(POST_TYPE, id.0, "note", serde_json::json!("hi"))
            .expect("set_meta 2");

        let obj = RhypeStore::get(store.as_ref(), &TypeName::from(POST_TYPE), id)
            .await
            .expect("get");
        let meta = match obj.get("meta") {
            Some(Value::Json(serde_json::Value::Object(m))) => m.clone(),
            other => panic!("meta must be a JSON object, got {other:?}"),
        };
        assert_eq!(
            meta.get("backlinks"),
            Some(&serde_json::json!(["/a", "/b"]))
        );
        assert_eq!(meta.get("note"), Some(&serde_json::json!("hi")));
        // Core fields untouched.
        assert!(matches!(obj.get("slug"), Some(Value::String(s)) if s == "target"));
        assert!(
            matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Published.as_str())
        );
    }

    #[tokio::test]
    async fn set_meta_rejects_non_content_types() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));
        // User carries a meta field, but the tight surface refuses it.
        let err = store
            .set_meta("User", 1, "x", serde_json::json!(1))
            .unwrap_err();
        assert!(
            err.to_string().contains("not writable"),
            "expected a not-writable error, got: {err}"
        );
    }
}
