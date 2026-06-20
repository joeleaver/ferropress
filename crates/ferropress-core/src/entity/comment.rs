//! `Comment` — threaded, moderated. Modeled with BOTH a `block_tree` and a
//! plain `body` so the representation decision can be made without reshaping the
//! schema: a deployment uses one or the other. Guest commenters store name/email
//! inline; registered commenters link `author`. IP/agent are kept for moderation
//! but treated as PII (retention policy TBD). Dropped: `comment_karma`,
//! pingback/trackback `comment_type`.
//!
//! A comment attaches to EITHER a `Post` OR a `Page` — it carries both typed
//! relations (mirroring `Revision`'s dual `parent_post`/`parent_page`); exactly
//! one is set, and that mutual exclusion is enforced at the application layer
//! (the SDL keeps both relations independent with `@on_delete(cascade)`).

use time::OffsetDateTime;

use crate::block::BlockTree;
use crate::status::CommentStatus;
use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub id: Option<ObjectId>,
    pub status: CommentStatus,
    /// Rich body (when the deployment uses block comments). Empty otherwise.
    pub block_tree: Option<BlockTree>,
    /// Plain body (when the deployment uses plain comments). Empty otherwise.
    pub body: String,
    /// Derived plaintext for moderation/search.
    pub plaintext: String,
    // Guest-author fields (used when `author` is None):
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    pub author_url: Option<String>,
    /// PII — moderation only.
    pub author_ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: OffsetDateTime,
    pub meta: serde_json::Value,

    /// The commented object — EITHER a post OR a page (exactly one is set;
    /// mutual exclusion is enforced app-level).
    pub post: Option<ObjectId>, // -> Post
    pub page: Option<ObjectId>,   // -> Page
    pub author: Option<ObjectId>, // -> User (when registered)
    pub parent: Option<ObjectId>, // -> Comment (threading)
}
