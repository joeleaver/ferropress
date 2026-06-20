//! `Post` — the primary content object. Also backs custom post types (CPTs):
//! rather than minting new SDL types per CPT (deferred, Tier-2), a CPT is a Post
//! with a reserved `post_type` discriminator string. This mirrors WP's overload
//! intentionally but *typed* — `post_type` is an indexed scalar we filter on.

use time::OffsetDateTime;

use crate::block::BlockTree;
use crate::seo::Seo;
use crate::status::Status;
use crate::value::ObjectId;

/// A post (or CPT row).
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    /// Store id (None before first persist).
    pub id: Option<ObjectId>,
    /// Immutable public UUID (WP `guid`, done right — never the permalink).
    pub uuid: String,
    pub slug: String,
    pub title: String,
    pub status: Status,
    /// CPT discriminator. Default `"post"`. Indexed for `filter`.
    pub post_type: String,
    /// Canonical body.
    pub block_tree: BlockTree,
    /// Server-derived plaintext; the single `@vectorize` source for `search`.
    pub plaintext: String,
    pub excerpt: String,
    pub seo: Seo,
    /// Tier-1 plugin meta (namespaced JSON).
    pub meta: serde_json::Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub published_at: Option<OffsetDateTime>,
    /// Soft-delete marker (generalized WP `trash`).
    pub deleted_at: Option<OffsetDateTime>,
    /// Content hash for static-rebuild cache invalidation.
    pub content_hash: String,

    // --- relations (resolved via the store's link API; ids held here) --------
    // revisions / comments are queried by inverse relation, not stored inline.
    pub author: Option<ObjectId>,         // -> User
    pub featured_media: Option<ObjectId>, // -> Media
    pub terms: Vec<ObjectId>,             // -> Term (many)
}
