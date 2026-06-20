//! `Revision` — versioned content snapshots as a dedicated entity (NOT
//! overloaded post rows with `inherit` status, as WP does). Stores a full
//! block-tree snapshot at minimum; a later optimization may store JSON-patch
//! diffs. Autosave is a distinct revision kind (single overwriting row in
//! practice — the store layer enforces the "one autosave per parent" rule).
//! Only current versions are searchable, so revisions carry NO `@vectorize`.

use time::OffsetDateTime;

use crate::block::BlockTree;
use crate::value::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionKind {
    /// Periodic autosave (overwrites the prior autosave for the same parent).
    Autosave,
    /// Explicit user-saved revision.
    Manual,
    /// Snapshot taken at publish.
    Publish,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    pub id: Option<ObjectId>,
    pub kind: RevisionKind,
    pub title: String,
    /// Full snapshot of the body at this revision.
    pub block_tree: BlockTree,
    /// Snapshot plaintext (for diff/restore only — not vectorized).
    pub plaintext: String,
    pub created_at: OffsetDateTime,

    /// Exactly one of these is set (the parent being versioned). Splitting WP's
    /// overloaded `post_parent` into typed relations.
    pub parent_post: Option<ObjectId>, // -> Post
    pub parent_page: Option<ObjectId>, // -> Page
    pub author: Option<ObjectId>,      // -> User
}
