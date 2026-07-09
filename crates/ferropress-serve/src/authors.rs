//! Live author directory for the public read path — cross-entity byline resolution.
//!
//! A post's byline (`User.display_name`) is a **cross-entity** value: it shows on
//! every one of that author's pages, but it belongs to a different object (the
//! `User`) than the page content (the `Post`). Baking the resolved name into the
//! prerender envelope would make the byline go stale the instant the author renames
//! — the exact global-coupling the settings track avoided for site chrome.
//!
//! So the name is resolved **live**, precisely as [`SiteSettings`](crate::settings)
//! are: the page envelope stores only the author's object *id* (a content-stable
//! property of the post — it changes only when the post is re-linked, which is a
//! Post edit and regenerates the envelope anyway); the *name* is looked up at
//! request time from an in-memory [`AuthorDirectory`] that the regen loop refreshes
//! off the change feed. A `User` rename is reflected on the next request with **no
//! page regeneration** — and without the serving-model **guardrail 2** violation the
//! alternative would be (regenerating every post a prolific author ever wrote on a
//! single rename).
//!
//! [`load_author_directory`] seeds the directory once at boot (one `User` scan);
//! thereafter [`AuthorsHandle::apply_user_change`] keeps it current from the feed —
//! O(1) per `User` change, reading the new name straight off the change's full
//! scalar snapshot with no extra store round-trip.

use std::collections::HashMap;
use std::sync::Arc;

use ferropress_core::USER_TYPE;
use ferropress_core::error::Result;
use ferropress_core::query::{Change, ChangeKind};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use parking_lot::RwLock;

/// An in-memory map from a `User` object id to its byline display name.
///
/// Only non-blank names are stored: a user whose `display_name` is empty/whitespace
/// simply has no entry, which the read path renders as *no byline* (matching the
/// prior per-request resolution). Cheap to clone (it is clone-on-write'd on each
/// `User` change), and cheap to read (a `HashMap` lookup, no store round-trip).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorDirectory {
    by_id: HashMap<u64, String>,
}

impl AuthorDirectory {
    /// Build a directory from `(user_id, display_name)` pairs. Blank names are
    /// dropped (an empty display name means no byline), so callers need not
    /// pre-filter.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u64, String)>) -> Self {
        let mut dir = AuthorDirectory::default();
        for (id, name) in pairs {
            dir.upsert(id, name);
        }
        dir
    }

    /// The byline display name for a `User` id, if one is known (and non-blank).
    pub fn name(&self, id: u64) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// Number of known authors (non-blank names). Cheap; used by tests + tracing.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the directory has no known authors.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Insert / replace `id`'s name, dropping the entry when `name` is blank (so a
    /// rename-to-blank correctly clears the byline rather than leaving the old one).
    fn upsert(&mut self, id: u64, name: String) {
        if name.trim().is_empty() {
            self.by_id.remove(&id);
        } else {
            self.by_id.insert(id, name);
        }
    }

    /// Forget `id` (a deleted user).
    fn remove(&mut self, id: u64) {
        self.by_id.remove(&id);
    }
}

/// Read the whole author directory from the store: one `User` scan, keeping each
/// user's non-blank `display_name`. Used for the startup seed; thereafter the feed
/// keeps the live handle current incrementally (see [`AuthorsHandle::apply_user_change`]).
///
/// This deliberately loads EVERY user, not only authors: a post's `author` may point
/// at any `User` (there is no role constraint on authorship, and `Post.author` has no
/// `@inverse` back-edge to cheaply enumerate authors), so filtering by role here could
/// drop a legitimate byline. With CLI-only user creation the set is small; once open
/// self-registration lands, the scale path is to resolve names lazily per author id
/// (bounded cache) rather than hold every registered user in memory. Bounded and
/// correct for v1.
pub async fn load_author_directory(store: &Arc<dyn RhypeStore>) -> Result<AuthorDirectory> {
    let mut dir = AuthorDirectory::default();
    for user in store.scan(&TypeName::from(USER_TYPE)).await? {
        if let Some(Value::String(name)) = user.get("display_name") {
            dir.upsert(user.id.0, name.clone());
        }
    }
    Ok(dir)
}

/// The `display_name` carried on a `User` change's JSON `fields`, if present and
/// non-blank. The engine publishes the object's FULL scalar snapshot on every
/// create/update (the patch is merged over the stored fields before the event is
/// emitted), so a rename that touches only `display_name` — and equally an update
/// that does NOT touch it — both carry the current name here, with no extra read.
fn display_name_from_change(change: &Change) -> Option<String> {
    change
        .fields
        .as_ref()
        .and_then(|f| f.get("display_name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// A cheaply-cloneable handle to the current live [`AuthorDirectory`], shared
/// between the read path (which resolves bylines) and the regen loop (which
/// refreshes it off the change feed). The read/write discipline mirrors
/// [`SettingsHandle`](crate::settings::SettingsHandle): reads clone the inner `Arc`
/// under a short read lock; a refresh swaps the `Arc` under a short write lock, so a
/// render never sees a half-updated directory.
#[derive(Clone)]
pub struct AuthorsHandle(Arc<RwLock<Arc<AuthorDirectory>>>);

impl AuthorsHandle {
    /// Seed the handle with an initial directory (built at startup from the store).
    pub fn new(initial: AuthorDirectory) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current directory snapshot. Cloning the `Arc` is cheap; hold it for the
    /// duration of one render.
    pub fn current(&self) -> Arc<AuthorDirectory> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current directory (a full reseed).
    pub fn set(&self, next: AuthorDirectory) {
        *self.0.write() = Arc::new(next);
    }

    /// Apply ONE `User` change from the feed, keeping the directory current with no
    /// store round-trip and no page regeneration:
    ///   * **Create / Update** → upsert the id → current `display_name` (read off the
    ///     change's full scalar snapshot); a blank/absent name removes the entry.
    ///   * **Delete** → forget the id.
    ///
    /// Clone-on-write: the desired state is compared against the current directory
    /// first, so the many `User` updates that do not change the byline (or delete of
    /// an unknown id) are a no-op — no map clone, no `Arc` swap. Only a real change
    /// clones the (small) map and swaps.
    pub fn apply_user_change(&self, change: &Change) {
        let id = change.object_id.0;
        let desired: Option<String> = match change.kind {
            ChangeKind::Create | ChangeKind::Update => display_name_from_change(change),
            ChangeKind::Delete => None,
        };

        let current = self.current();
        // No-op when the directory already reflects the desired byline — avoids a
        // needless clone+swap on every unrelated `User` write.
        if current.name(id).map(str::to_owned) == desired {
            return;
        }

        let mut next = (*current).clone();
        match desired {
            Some(name) => next.upsert(id, name),
            None => next.remove(id),
        }
        self.set(next);
    }
}

impl Default for AuthorsHandle {
    /// An empty directory — the state before the store is read (and what tests get
    /// without wiring authors; unresolved ids simply render as no byline).
    fn default() -> Self {
        Self::new(AuthorDirectory::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferropress_core::value::ObjectId;
    use serde_json::json;

    fn user_change(kind: ChangeKind, id: u64, display_name: Option<&str>) -> Change {
        Change {
            version: 1,
            kind,
            type_name: TypeName::from(USER_TYPE),
            object_id: ObjectId(id),
            fields: display_name.map(|n| json!({ "display_name": n })),
            origin: None,
        }
    }

    #[test]
    fn blank_names_are_never_stored() {
        let dir = AuthorDirectory::from_pairs([
            (1, "Ada Lovelace".to_owned()),
            (2, "   ".to_owned()),
            (3, String::new()),
        ]);
        assert_eq!(dir.name(1), Some("Ada Lovelace"));
        assert_eq!(dir.name(2), None);
        assert_eq!(dir.name(3), None);
        assert_eq!(dir.len(), 1);
    }

    #[test]
    fn apply_user_change_upserts_on_update_and_forgets_on_delete() {
        let handle = AuthorsHandle::new(AuthorDirectory::from_pairs([(7, "Grace".to_owned())]));

        // A rename update carries the new name on the change's scalar snapshot.
        handle.apply_user_change(&user_change(ChangeKind::Update, 7, Some("Grace Hopper")));
        assert_eq!(
            handle.current().name(7).map(str::to_owned),
            Some("Grace Hopper".to_owned())
        );

        // A create introduces a new author.
        handle.apply_user_change(&user_change(
            ChangeKind::Create,
            8,
            Some("Katherine Johnson"),
        ));
        assert_eq!(handle.current().name(8), Some("Katherine Johnson"));

        // A delete forgets the id (fields absent on delete is fine).
        handle.apply_user_change(&user_change(ChangeKind::Delete, 7, None));
        assert_eq!(handle.current().name(7), None);
        // The untouched author is still present.
        assert_eq!(handle.current().name(8), Some("Katherine Johnson"));
    }

    #[test]
    fn update_to_blank_name_clears_the_byline() {
        let handle = AuthorsHandle::new(AuthorDirectory::from_pairs([(1, "Ada".to_owned())]));
        handle.apply_user_change(&user_change(ChangeKind::Update, 1, Some("  ")));
        assert_eq!(handle.current().name(1), None);
    }

    #[test]
    fn unrelated_update_is_a_no_op_snapshot() {
        let handle = AuthorsHandle::new(AuthorDirectory::from_pairs([(1, "Ada".to_owned())]));
        let before = handle.current();
        // An update carrying the SAME display_name must not swap the Arc.
        handle.apply_user_change(&user_change(ChangeKind::Update, 1, Some("Ada")));
        assert!(
            Arc::ptr_eq(&before, &handle.current()),
            "an update that doesn't change the byline must not swap the snapshot",
        );
    }
}
