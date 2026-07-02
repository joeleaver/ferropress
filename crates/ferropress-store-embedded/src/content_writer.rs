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
//! bridge must not re-dispatch a plugin's own write. Every write this backend
//! makes is therefore stamped with the [`PLUGIN_ORIGIN`] write origin (via the
//! engine's `*_with_origin` verbs), and the bridge subscribes with
//! `exclude_origin = PLUGIN_ORIGIN`, so a plugin's own writes are filtered off the
//! hub before they reach an action hook (the regen loop keeps its unfiltered
//! subscription and still prerenders them). With that guard the backend IS wired
//! into the production composition root; deny-by-default still holds structurally
//! (an un-backed `write_store` plugin fails to instantiate).

use ferropress_core::block::BlockTree;
use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::plugin_caps::{ContentWriter, PLUGIN_ORIGIN};
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

        // Stamp the plugin write origin so this create can't loop back through the
        // action-hook bridge (it excludes PLUGIN_ORIGIN); the regen loop, which does
        // not exclude it, still prerenders the new stub.
        let obj = self
            .db()
            .create_with_origin(
                PAGE_TYPE,
                convert::to_db_fields(fields),
                Some(PLUGIN_ORIGIN),
            )
            .map_err(AdapterError::from)?;
        Ok(obj.id)
    }

    fn set_meta(
        &self,
        type_name: &str,
        id: u64,
        namespace: &str,
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
        if namespace.is_empty() || key.is_empty() {
            return Err(CoreError::Store(
                "set_meta: namespace and key must not be empty".to_owned(),
            ));
        }

        // Serialize the read-modify-write: two concurrent set_meta calls to the same
        // object each read the whole `meta`, edit their sub-object, and write it all
        // back — without this a second writer's namespace could be lost. Recover a
        // poisoned lock (a panicked prior write must not wedge all future writes).
        let _guard = self
            .meta_write_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Read-modify-write the `meta` JSON object ONLY. Every other field is left
        // untouched, so a plugin can't reach a core/indexed field through here. The
        // value is nested under `meta[namespace][key]` — one level UNDER the caller's
        // namespace, never string-joined — so a plugin can only ever mutate its own
        // sub-object and cross-plugin/core keys are structurally unforgeable.
        let obj =
            convert::from_db_object(self.db().get(type_name, id).map_err(AdapterError::from)?);
        let mut meta = match obj.get("meta") {
            Some(Value::Json(serde_json::Value::Object(m))) => m.clone(),
            _ => serde_json::Map::new(),
        };
        let mut ns = match meta.get(namespace) {
            Some(serde_json::Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        };
        ns.insert(key.to_owned(), value);
        meta.insert(namespace.to_owned(), serde_json::Value::Object(ns));

        let mut patch: FieldMap = FieldMap::new();
        patch.insert(
            "meta".to_owned(),
            Value::Json(serde_json::Value::Object(meta)),
        );
        // Stamp the plugin write origin (see create_page_stub) so this meta write
        // is filtered out of the action-hook bridge and can't re-trigger the very
        // action that wrote it.
        self.db()
            .update_with_origin(
                type_name,
                id,
                convert::to_db_fields(patch),
                Some(PLUGIN_ORIGIN),
            )
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

        // Set a meta key under the plugin's namespace.
        store
            .set_meta(
                POST_TYPE,
                id.0,
                "plug-a",
                "backlinks",
                serde_json::json!(["/a", "/b"]),
            )
            .expect("set_meta");

        // Merge a SECOND key — the first must survive (read-modify-write, not replace).
        store
            .set_meta(POST_TYPE, id.0, "plug-a", "note", serde_json::json!("hi"))
            .expect("set_meta 2");

        // A DIFFERENT plugin writing the SAME key must NOT collide (separate sub-object).
        store
            .set_meta(
                POST_TYPE,
                id.0,
                "plug-b",
                "backlinks",
                serde_json::json!(["/z"]),
            )
            .expect("set_meta 3");

        let obj = RhypeStore::get(store.as_ref(), &TypeName::from(POST_TYPE), id)
            .await
            .expect("get");
        let meta = match obj.get("meta") {
            Some(Value::Json(serde_json::Value::Object(m))) => m.clone(),
            other => panic!("meta must be a JSON object, got {other:?}"),
        };
        // plug-a's sub-object holds BOTH of its keys.
        assert_eq!(
            meta.get("plug-a"),
            Some(&serde_json::json!({ "backlinks": ["/a", "/b"], "note": "hi" }))
        );
        // plug-b's same-named key lives in its OWN sub-object — no clobber.
        assert_eq!(
            meta.get("plug-b"),
            Some(&serde_json::json!({ "backlinks": ["/z"] }))
        );
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
            .set_meta("User", 1, "ns", "x", serde_json::json!(1))
            .unwrap_err();
        assert!(
            err.to_string().contains("not writable"),
            "expected a not-writable error, got: {err}"
        );
    }

    /// The FEED-LOOP GUARD at the adapter layer: a `ContentWriter` (plugin) write
    /// is stamped with [`PLUGIN_ORIGIN`] on the change feed, so a subscriber that
    /// sets `exclude_origin = PLUGIN_ORIGIN` (the action-hook bridge) never receives
    /// it, while an unfiltered subscriber (the regen loop) still does. Proven
    /// deterministically — no "wait and see nothing" — by writing a second, EXTERNAL
    /// (untagged) update right after the plugin write: the excluding stream's FIRST
    /// delivered event is that external write, which can only be true if the plugin
    /// write was dropped from it. The unfiltered stream sees BOTH, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plugin_writes_are_tagged_and_excludable_from_the_feed() {
        use ferropress_core::plugin_caps::PLUGIN_ORIGIN;
        use ferropress_core::query::{ChangeKind, SubscribeFilter};
        use std::time::Duration;
        use tokio_stream::StreamExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));

        // Seed a published Post to write meta onto (created BEFORE subscribing, so
        // the streams below start empty and carry only the two writes under test).
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

        // Two live subscriptions, registered BEFORE either write (no subscribe race):
        //   * `regen`  — unfiltered, like the ServeEngine regen loop: must see both.
        //   * `bridge` — excludes PLUGIN_ORIGIN, like the HookBridge: must skip the
        //                plugin write and see only the external one.
        let mut regen = RhypeStore::subscribe(store.as_ref(), SubscribeFilter::default())
            .await
            .expect("regen subscribe");
        let mut bridge = RhypeStore::subscribe(
            store.as_ref(),
            SubscribeFilter {
                exclude_origin: Some(PLUGIN_ORIGIN),
                ..SubscribeFilter::default()
            },
        )
        .await
        .expect("bridge subscribe");

        // (1) A plugin write via the ContentWriter surface — stamped PLUGIN_ORIGIN.
        store
            .set_meta(POST_TYPE, id.0, "plug", "k", serde_json::json!(1))
            .expect("plugin set_meta");
        // (2) An EXTERNAL (first-party) write via the async data port — untagged.
        let mut patch: FieldMap = FieldMap::new();
        patch.insert("title".to_owned(), Value::String("external".to_owned()));
        RhypeStore::update(store.as_ref(), &TypeName::from(POST_TYPE), id, patch)
            .await
            .expect("external update");

        // Fail fast rather than hang the suite if delivery stalls.
        async fn next_change(
            s: &mut futures_core::stream::BoxStream<'static, ferropress_core::query::Change>,
        ) -> ferropress_core::query::Change {
            tokio::time::timeout(Duration::from_secs(5), s.next())
                .await
                .expect("a change must arrive within 5s")
                .expect("the stream must not end")
        }

        // The unfiltered (regen) stream sees the plugin write FIRST (tagged), then
        // the external write (untagged) — both, in commit order.
        let r1 = next_change(&mut regen).await;
        assert_eq!(r1.kind, ChangeKind::Update);
        assert_eq!(r1.object_id, id);
        assert_eq!(
            r1.origin,
            Some(PLUGIN_ORIGIN),
            "the regen loop must see the plugin write, tagged with PLUGIN_ORIGIN"
        );
        let r2 = next_change(&mut regen).await;
        assert_eq!(r2.origin, None, "and then the untagged external write");

        // The excluding (bridge) stream DROPS the plugin write: its first delivered
        // event is the external (untagged) write. The plugin write never re-triggers
        // an action — this is the loop-breaker.
        let b1 = next_change(&mut bridge).await;
        assert_eq!(b1.object_id, id);
        assert_eq!(
            b1.origin, None,
            "the bridge must skip the plugin write and receive only the external one"
        );
    }
}
