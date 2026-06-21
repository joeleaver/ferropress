//! `impl ContentReader for EmbeddedStore` — the synchronous `content:read`
//! capability backend the plugin host exposes (as the `fp_lookup_slug` host
//! function) to plugins granted `read_store`.
//!
//! SYNCHRONOUS by design: the plugin host calls this from inside a synchronous
//! WASM host function, so unlike the async [`RhypeStore`] methods (which offload
//! each engine verb to `spawn_blocking`) this drives the engine's `filter_scan_str`
//! slug index DIRECTLY on the calling thread. It then applies the SAME
//! published-only gate and Post-then-Page order as the public read path's
//! `resolve_published_entity`, so a plugin sees exactly the entities the site
//! actually serves.

use ferropress_core::error::Result as CoreResult;
use ferropress_core::plugin_caps::{ContentReader, PublishedRef};
use ferropress_core::query::Compare;
use ferropress_core::value::Value;
use ferropress_core::{PAGE_TYPE, POST_TYPE, Status};

use crate::{AdapterError, EmbeddedStore, convert};

impl ContentReader for EmbeddedStore {
    fn lookup_published_slug(&self, slug: &str) -> CoreResult<Option<PublishedRef>> {
        if slug.is_empty() {
            return Ok(None);
        }
        // Post first, then Page — the same permalink order the public read path uses.
        for type_name in [POST_TYPE, PAGE_TYPE] {
            let op = convert::to_compare_op(Compare::Eq);
            let hit = self
                .db()
                .filter_scan_str(type_name, "slug", op, slug, Some(1))
                .map_err(AdapterError::from)?
                .into_iter()
                .next()
                .map(convert::from_db_object);

            if let Some(obj) = hit {
                // Published-only: an unpublished entity does not "exist" to a plugin
                // (it isn't publicly served), so a wiki link to it stays a red link.
                let published = matches!(
                    obj.get("status"),
                    Some(Value::String(s)) if s == Status::Published.as_str()
                );
                if published {
                    let title = match obj.get("title") {
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    return Ok(Some(PublishedRef {
                        id: obj.id.0,
                        type_name: type_name.to_owned(),
                        title,
                        slug: slug.to_owned(),
                    }));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferropress_core::plugin_caps::ContentReader;
    use ferropress_core::store::RhypeStore;
    use ferropress_core::value::{FieldMap, TypeName, Value};
    use ferropress_core::{PAGE_TYPE, POST_TYPE, Status};

    use crate::EmbeddedStore;

    async fn seed(store: &EmbeddedStore, type_name: &str, slug: &str, title: &str, status: Status) {
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
        fields.insert("title".to_owned(), Value::String(title.to_owned()));
        fields.insert(
            "status".to_owned(),
            Value::String(status.as_str().to_owned()),
        );
        if type_name == POST_TYPE {
            fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
        }
        // `RhypeStore::create` is the async write path; the sync read is what we test.
        RhypeStore::create(store, &TypeName::from(type_name), fields)
            .await
            .expect("seed");
    }

    #[tokio::test]
    async fn lookup_resolves_published_post_and_page_but_not_drafts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));

        seed(&store, POST_TYPE, "hello", "Hello", Status::Published).await;
        seed(&store, PAGE_TYPE, "about", "About", Status::Published).await;
        seed(&store, POST_TYPE, "secret", "Secret", Status::Draft).await;

        // A published Post resolves with its identity + title.
        let post = store
            .lookup_published_slug("hello")
            .expect("ok")
            .expect("found");
        assert_eq!(post.type_name, POST_TYPE);
        assert_eq!(post.title, "Hello");
        assert_eq!(post.slug, "hello");

        // A published Page resolves too (Post-miss falls through to Page).
        let page = store
            .lookup_published_slug("about")
            .expect("ok")
            .expect("found");
        assert_eq!(page.type_name, PAGE_TYPE);

        // A draft does NOT exist to a plugin (published-only gate).
        assert!(store.lookup_published_slug("secret").expect("ok").is_none());
        // An unknown slug is None.
        assert!(store.lookup_published_slug("nope").expect("ok").is_none());
        // An empty slug is None (no lookup).
        assert!(store.lookup_published_slug("").expect("ok").is_none());
    }
}
