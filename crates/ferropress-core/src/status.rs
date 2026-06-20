//! Typed content lifecycle + comment moderation state machines.
//!
//! WordPress models these as magic strings (`post_status`, `comment_approved`
//! ∈ {1,0,spam,trash}). We model them as explicit enums with a guarded
//! transition function — the single biggest "ADAPT" from the WP content-model
//! audit. Dropped WP statuses: `auto-draft` (we create a row only on first save)
//! and `inherit` (revisions/attachments are their own typed entities here).
//! `future` is kept as `Scheduled`; `trash` generalizes to a soft-delete usable
//! across entities (paired with a `deleted_at` timestamp on the entity).

/// Lifecycle of a content object (Post / Page / and CPTs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Work in progress, not publicly visible.
    Draft,
    /// Submitted for editorial review (contributor -> editor gate).
    Pending,
    /// Publicly visible.
    Published,
    /// Published but visible only to authenticated, authorized users.
    Private,
    /// Will auto-publish at a scheduled instant (WP `future`). For static-first
    /// this drives a scheduled rebuild via the `Scheduler` port, not a poll.
    Scheduled,
    /// Soft-deleted (WP `trash`); eligible for purge.
    Trashed,
}

impl Status {
    /// Whether `self -> next` is a permitted transition. Enforced on write; an
    /// illegal transition becomes `CoreError::IllegalTransition`.
    pub fn can_transition_to(self, next: Status) -> bool {
        use Status::*;
        match (self, next) {
            (Draft, Pending | Published | Scheduled | Trashed) => true,
            (Pending, Draft | Published | Scheduled | Trashed) => true,
            (Scheduled, Published | Draft | Trashed) => true,
            (Published, Private | Draft | Trashed) => true,
            (Private, Published | Draft | Trashed) => true,
            (Trashed, Draft) => true, // restore
            (a, b) if a == b => true, // idempotent no-op
            _ => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Draft => "draft",
            Status::Pending => "pending",
            Status::Published => "published",
            Status::Private => "private",
            Status::Scheduled => "scheduled",
            Status::Trashed => "trashed",
        }
    }
}

/// Comment moderation state (WP `comment_approved`, retyped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentStatus {
    Pending,
    Approved,
    Spam,
    Trash,
}

impl CommentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CommentStatus::Pending => "pending",
            CommentStatus::Approved => "approved",
            CommentStatus::Spam => "spam",
            CommentStatus::Trash => "trash",
        }
    }
}
