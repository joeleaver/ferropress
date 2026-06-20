//! `Page` — hierarchical content. Same body/search/meta/SEO pattern as `Post`,
//! minus taxonomy, plus `parent` (self-relation) + `menu_order` + `template`.
//! WP overloads `post_parent` for three meanings (page parent / attachment owner
//! / revision parent); we split those into distinct typed relations across the
//! Page/Media/Revision entities — `parent` here means page hierarchy only.

use time::OffsetDateTime;

use crate::block::BlockTree;
use crate::seo::Seo;
use crate::status::Status;
use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub id: Option<ObjectId>,
    pub uuid: String,
    pub slug: String,
    pub title: String,
    pub status: Status,
    pub block_tree: BlockTree,
    pub plaintext: String,
    pub excerpt: String,
    pub seo: Seo,
    pub meta: serde_json::Value,
    pub menu_order: i32,
    /// Theme template override (e.g. `"full-width"`).
    pub template: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub published_at: Option<OffsetDateTime>,
    pub deleted_at: Option<OffsetDateTime>,
    pub content_hash: String,

    pub author: Option<ObjectId>,         // -> User
    pub parent: Option<ObjectId>,         // -> Page (hierarchy)
    pub featured_media: Option<ObjectId>, // -> Media
}
