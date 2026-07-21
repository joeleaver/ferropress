//! Live content index for the public read path — nav-menu target resolution.
//!
//! A nav [`MenuItem`](ferropress_core::MenuItem) points at a typed
//! [`LinkTarget`](ferropress_core::LinkTarget): a `Post`/`Page` by *id*, a taxonomy
//! `Term` (not yet), or a `Custom` URL. To render a menu the compose path must turn a
//! `Post`/`Page` id into its public **href** (and, for an empty menu label, its current
//! **title**). Resolving that from the store on every request would put a lookup back on
//! the cache-hit hot path the prerender cache exists to keep store-free.
//!
//! So — exactly like the [`AuthorDirectory`](crate::authors) resolves a byline name
//! live — the id→(href, title) mapping is held in an in-memory [`ContentIndex`] the
//! read path clones per render and the regen loop keeps current off the change feed.
//! A page/post rename, publish, or unpublish is reflected in every menu that targets it
//! on the next request with **no page regeneration** and **no menu edit** (WP-faithful:
//! a menu item stores an id, so its label/href track the target's live title/URL).
//!
//! Two disciplines, both borrowed from [`AuthorsHandle`](crate::authors::AuthorsHandle):
//!   * [`load_content_index`] seeds it once at boot (one `Post` + one `Page` scan,
//!     keeping only PUBLISHED rows — an unpublished target must resolve to nothing);
//!   * [`ContentIndexHandle::apply_content_change`] keeps it current per `Post`/`Page`
//!     change — O(1), reading the new `slug`/`path`/`title`/`status` straight off the
//!     change's full scalar snapshot with no extra store round-trip.
//!
//! `Post` and `Page` ids are kept in SEPARATE maps: a [`LinkTarget::Post`] and a
//! [`LinkTarget::Page`] are distinct even if they share a numeric id, so resolution is
//! unambiguous regardless of how the store allocates ids.

use std::collections::HashMap;
use std::sync::Arc;

use ferropress_core::error::Result;
use ferropress_core::query::{Change, ChangeKind};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use ferropress_core::{PAGE_TYPE, POST_TYPE, Status};
use parking_lot::RwLock;

/// A published content object's resolved public identity: the href a menu item linking
/// to it should point at, and its current title (used when the menu item's own label is
/// blank — WP shows the target's title in that case).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentEntry {
    /// The public URL: `"/<slug>"` for a post, `"/<path>"` for a page.
    pub href: String,
    /// The current title (the empty-label fallback).
    pub title: String,
}

/// An in-memory index of PUBLISHED content, id → its public [`ContentEntry`], kept in
/// two maps so a `Post` id and a `Page` id never collide. Only published rows are
/// present: a draft/trashed target simply has no entry, which the compose path renders
/// as an unresolvable target (dropped or label-only), matching the publish gate the
/// permalink read path applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentIndex {
    posts: HashMap<u64, ContentEntry>,
    pages: HashMap<u64, ContentEntry>,
}

impl ContentIndex {
    /// The published entry for a `Post` id, if one is known.
    pub fn post(&self, id: u64) -> Option<&ContentEntry> {
        self.posts.get(&id)
    }

    /// The published entry for a `Page` id, if one is known.
    pub fn page(&self, id: u64) -> Option<&ContentEntry> {
        self.pages.get(&id)
    }

    /// Number of indexed (published) content objects — tests + tracing.
    pub fn len(&self) -> usize {
        self.posts.len() + self.pages.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.posts.is_empty() && self.pages.is_empty()
    }

    /// Upsert `id`'s entry for the given content type.
    fn upsert(&mut self, type_name: &str, id: u64, entry: ContentEntry) {
        self.map_mut(type_name).insert(id, entry);
    }

    /// Forget `id` for the given content type (unpublished / deleted).
    fn remove(&mut self, type_name: &str, id: u64) {
        self.map_mut(type_name).remove(&id);
    }

    /// The current entry for `(type_name, id)` — used for the clone-on-write no-op check.
    fn get(&self, type_name: &str, id: u64) -> Option<&ContentEntry> {
        match type_name {
            POST_TYPE => self.posts.get(&id),
            _ => self.pages.get(&id),
        }
    }

    fn map_mut(&mut self, type_name: &str) -> &mut HashMap<u64, ContentEntry> {
        if type_name == POST_TYPE {
            &mut self.posts
        } else {
            &mut self.pages
        }
    }
}

/// The public href a content object resolves to, from its type + scalar fields: a
/// post keys on `slug` (`/<slug>`), a page on its materialized `path` (`/<path>`).
/// `None` when the keying field is absent/empty (no permalink to point a menu at).
/// The `fields` are read the same way whether they come off a change snapshot or a
/// scanned object (both expose `get`), so the boot seed and the incremental update
/// derive byte-identical hrefs.
fn href_from_fields(type_name: &str, get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let field = if type_name == PAGE_TYPE {
        "path"
    } else {
        "slug"
    };
    get(field)
        .filter(|s| !s.is_empty())
        .map(|key| format!("/{key}"))
}

/// Seed the whole index from the store: one `Post` scan + one `Page` scan, keeping each
/// PUBLISHED row's href + title. Used for the boot seed; thereafter the feed keeps the
/// live handle current incrementally (see [`ContentIndexHandle::apply_content_change`]).
pub async fn load_content_index(store: &Arc<dyn RhypeStore>) -> Result<ContentIndex> {
    let mut index = ContentIndex::default();
    for type_name in [POST_TYPE, PAGE_TYPE] {
        for obj in store.scan(&TypeName::from(type_name)).await? {
            if !is_published_status(obj.get("status")) {
                continue;
            }
            let get = |field: &str| match obj.get(field) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            if let Some(href) = href_from_fields(type_name, get) {
                index.upsert(
                    type_name,
                    obj.id.0,
                    ContentEntry {
                        href,
                        title: match obj.get("title") {
                            Some(Value::String(s)) => s.clone(),
                            _ => String::new(),
                        },
                    },
                );
            }
        }
    }
    Ok(index)
}

/// Whether a `status` field value is exactly `"published"` (statuses are stored as
/// plain strings; mirrors [`content::is_published`](crate::content)).
fn is_published_status(status: Option<&Value>) -> bool {
    matches!(status, Some(Value::String(s)) if s == Status::Published.as_str())
}

/// The desired [`ContentEntry`] a `Post`/`Page` change implies, or `None` when the
/// object should have no index entry (unpublished, deleted, or missing a keying field).
/// Reads `status` / `slug` / `path` / `title` straight off the change's full scalar
/// snapshot (rhypedb publishes the merged fields on every create/update, and the
/// pre-delete fields on a delete), so this needs no store round-trip.
fn desired_entry(change: &Change) -> Option<ContentEntry> {
    // A delete removes the entry regardless of the (pre-delete) fields it carries.
    if matches!(change.kind, ChangeKind::Delete) {
        return None;
    }
    let fields = change.fields.as_ref()?;
    let str_field = |field: &str| fields.get(field).and_then(|v| v.as_str());
    // Unpublished → no entry (a draft/trashed target must not resolve).
    if str_field("status") != Some(Status::Published.as_str()) {
        return None;
    }
    let type_name = change.type_name.as_str();
    let href = href_from_fields(type_name, |field| str_field(field).map(str::to_owned))?;
    Some(ContentEntry {
        href,
        title: str_field("title").unwrap_or_default().to_owned(),
    })
}

/// A cheaply-cloneable handle to the current live [`ContentIndex`], shared between the
/// read path (which resolves nav targets) and the regen loop (which refreshes it off
/// the change feed). The read/write discipline mirrors
/// [`AuthorsHandle`](crate::authors::AuthorsHandle): reads clone the inner `Arc` under a
/// short read lock; a refresh swaps the `Arc` under a short write lock, so a render never
/// sees a half-updated index.
#[derive(Clone)]
pub struct ContentIndexHandle(Arc<RwLock<Arc<ContentIndex>>>);

impl ContentIndexHandle {
    /// Seed the handle with an initial index (built at startup from the store).
    pub fn new(initial: ContentIndex) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current index snapshot. Cloning the `Arc` is cheap; hold it for one render.
    pub fn current(&self) -> Arc<ContentIndex> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current index (a full reseed).
    pub fn set(&self, next: ContentIndex) {
        *self.0.write() = Arc::new(next);
    }

    /// Apply ONE `Post`/`Page` change from the feed, keeping the index current with no
    /// store round-trip and no page regeneration:
    ///   * **Create / Update** of a PUBLISHED object → upsert its href + title (read off
    ///     the change's scalar snapshot); an unpublished object → remove any entry.
    ///   * **Delete** → forget the id.
    ///
    /// Clone-on-write: the desired state is compared against the current entry first, so
    /// the many content updates that don't change href/title (or a change for an object
    /// that stays absent) are a no-op — no map clone, no `Arc` swap. Only a real change
    /// clones the (small) map and swaps.
    pub fn apply_content_change(&self, change: &Change) {
        let type_name = change.type_name.as_str();
        if type_name != POST_TYPE && type_name != PAGE_TYPE {
            return;
        }
        let id = change.object_id.0;
        let desired = desired_entry(change);

        let current = self.current();
        // No-op when the index already reflects the desired entry — avoids a needless
        // clone+swap on every unrelated content write.
        if current.get(type_name, id) == desired.as_ref() {
            return;
        }

        let mut next = (*current).clone();
        match desired {
            Some(entry) => next.upsert(type_name, id, entry),
            None => next.remove(type_name, id),
        }
        self.set(next);
    }
}

impl Default for ContentIndexHandle {
    /// An empty index — the state before the store is read (and what tests get without
    /// wiring content; every target resolves to nothing).
    fn default() -> Self {
        Self::new(ContentIndex::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferropress_core::value::ObjectId;
    use serde_json::json;

    fn change(
        kind: ChangeKind,
        type_name: &str,
        id: u64,
        fields: Option<serde_json::Value>,
    ) -> Change {
        Change {
            version: 1,
            kind,
            type_name: TypeName::from(type_name),
            object_id: ObjectId(id),
            fields,
            origin: None,
        }
    }

    #[test]
    fn post_and_page_ids_do_not_collide() {
        let mut index = ContentIndex::default();
        index.upsert(
            POST_TYPE,
            1,
            ContentEntry {
                href: "/a-post".to_owned(),
                title: "A Post".to_owned(),
            },
        );
        index.upsert(
            PAGE_TYPE,
            1,
            ContentEntry {
                href: "/a-page".to_owned(),
                title: "A Page".to_owned(),
            },
        );
        assert_eq!(index.post(1).unwrap().href, "/a-post");
        assert_eq!(index.page(1).unwrap().href, "/a-page");
    }

    #[test]
    fn published_post_upserts_and_keys_on_slug() {
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Create,
            POST_TYPE,
            7,
            Some(json!({"status": "published", "slug": "hello", "title": "Hello"})),
        ));
        let e = handle.current().post(7).cloned().expect("post indexed");
        assert_eq!(e.href, "/hello");
        assert_eq!(e.title, "Hello");
    }

    #[test]
    fn published_page_keys_on_the_materialized_path() {
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Update,
            PAGE_TYPE,
            3,
            Some(json!({"status": "published", "path": "about/team", "slug": "team", "title": "The Team"})),
        ));
        let e = handle.current().page(3).cloned().expect("page indexed");
        assert_eq!(
            e.href, "/about/team",
            "a page keys on its nested path, not its slug"
        );
    }

    #[test]
    fn unpublishing_removes_the_entry() {
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Create,
            POST_TYPE,
            7,
            Some(json!({"status": "published", "slug": "hello", "title": "Hello"})),
        ));
        assert!(handle.current().post(7).is_some());
        // Update to draft → the target must stop resolving.
        handle.apply_content_change(&change(
            ChangeKind::Update,
            POST_TYPE,
            7,
            Some(json!({"status": "draft", "slug": "hello", "title": "Hello"})),
        ));
        assert!(handle.current().post(7).is_none());
    }

    #[test]
    fn delete_forgets_the_id() {
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Create,
            PAGE_TYPE,
            5,
            Some(json!({"status": "published", "path": "faq", "title": "FAQ"})),
        ));
        handle.apply_content_change(&change(ChangeKind::Delete, PAGE_TYPE, 5, None));
        assert!(handle.current().page(5).is_none());
    }

    #[test]
    fn a_rename_reflects_without_a_menu_edit() {
        // The whole point: only the id is stored on the menu item; a target rename
        // updates the index (href + title) so the menu tracks it with no menu write.
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Create,
            POST_TYPE,
            7,
            Some(json!({"status": "published", "slug": "old", "title": "Old"})),
        ));
        handle.apply_content_change(&change(
            ChangeKind::Update,
            POST_TYPE,
            7,
            Some(json!({"status": "published", "slug": "new", "title": "New Title"})),
        ));
        let e = handle.current().post(7).cloned().unwrap();
        assert_eq!(e.href, "/new");
        assert_eq!(e.title, "New Title");
    }

    #[test]
    fn an_unrelated_update_is_a_no_op_snapshot() {
        let handle = ContentIndexHandle::default();
        handle.apply_content_change(&change(
            ChangeKind::Create,
            POST_TYPE,
            1,
            Some(json!({"status": "published", "slug": "x", "title": "X"})),
        ));
        let before = handle.current();
        // Same href + title → no swap.
        handle.apply_content_change(&change(
            ChangeKind::Update,
            POST_TYPE,
            1,
            Some(json!({"status": "published", "slug": "x", "title": "X"})),
        ));
        assert!(
            Arc::ptr_eq(&before, &handle.current()),
            "an update that changes neither href nor title must not swap the snapshot",
        );
    }

    #[test]
    fn a_non_content_change_is_ignored() {
        let handle = ContentIndexHandle::default();
        let before = handle.current();
        handle.apply_content_change(&change(
            ChangeKind::Update,
            "User",
            1,
            Some(json!({"display_name": "Ada"})),
        ));
        assert!(Arc::ptr_eq(&before, &handle.current()));
    }
}
