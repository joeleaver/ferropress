//! Domain entities — the typed WordPress content model.
//!
//! Each entity here is a plain Rust struct (the in-memory domain form). Its
//! mapping to a rhypedb object type (field names, scalar types, relations,
//! `@vectorize`) is defined ONCE in `ferropress-schema-sdl` and materialized by
//! `ferropress-store-embedded`. The `*_TYPE` constants below are the single
//! source of truth for the type-name strings used at the store boundary, so a
//! typo can't silently address the wrong type.
//!
//! Cross-cutting fields present on every content entity (Post/Page/Media/Term/
//! Comment/Revision):
//!   - `block_tree: BlockTree` — canonical body (JSON String at rest).
//!   - `plaintext: String` — server-derived, the SINGLE `@vectorize` source
//!     feeding that entity's `search` Vector field.
//!   - `slug`, `status`, `created_at`/`updated_at`/`published_at`.
//!   - `meta: serde_json::Value` — Tier-1 plugin meta (namespaced JSON String
//!     at rest).
//!   - `seo: Seo` — first-class SEO (JSON String at rest).

pub mod comment;
pub mod media;
pub mod menu;
pub mod page;
pub mod post;
pub mod redirect;
pub mod revision;
pub mod setting;
pub mod taxonomy;
pub mod user;

pub use comment::Comment;
pub use media::Media;
pub use menu::{LinkTarget, Menu, MenuItem};
pub use page::Page;
pub use post::Post;
pub use redirect::Redirect;
pub use revision::{Revision, RevisionKind};
pub use setting::Setting;
pub use taxonomy::{Taxonomy, Term};
pub use user::User;

/// Canonical store type-name constants. These MUST match the SDL type names in
/// `ferropress-schema-sdl`.
pub const POST_TYPE: &str = "Post";
pub const PAGE_TYPE: &str = "Page";
pub const MEDIA_TYPE: &str = "Media";
pub const TAXONOMY_TYPE: &str = "Taxonomy";
pub const TERM_TYPE: &str = "Term";
pub const USER_TYPE: &str = "User";
pub const COMMENT_TYPE: &str = "Comment";
pub const MENU_TYPE: &str = "Menu";
pub const MENU_ITEM_TYPE: &str = "MenuItem";
pub const SETTING_TYPE: &str = "Setting";
pub const REVISION_TYPE: &str = "Revision";
pub const REDIRECT_TYPE: &str = "Redirect";
