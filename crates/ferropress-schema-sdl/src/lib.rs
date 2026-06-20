//! The single source of truth for the Ferropress on-disk schema.
//!
//! The whole content model is expressed as ONE rhypedb SDL string and parsed
//! via `rhypedb_schema::parser::parse_schema` (the only supported constructor —
//! there is no builder/macro). `parsed_schema()` both parses and validates.
//!
//! VERIFIED rhypedb constraints baked into this SDL (checked against source at
//! the pinned rev `e735be47`):
//!   * Working scalar types are String/u32/u64/i32/i64/f32/f64/Bool/Bytes.
//!     `DateTime` and `Json` are PARSE-ONLY / write-dead (no runtime Value
//!     variant) — so every timestamp and every JSON blob is a `String`. We do
//!     NOT use `DateTime`/`Json` anywhere.
//!   * `@indexed` is the scalar secondary-index directive (NOT `@index`).
//!     `@index(hnsw, ...)` is the VECTOR-index directive and is rejected on a
//!     scalar. Each entity's `search` Vector field carries `@index(hnsw, ...)`;
//!     every scalar secondary index uses `@indexed`.
//!   * `@vectorize(source: "<f>", model: "<m>")` requires a `Vector<N>` field and
//!     a `String` source field. Each content entity has exactly one `plaintext`
//!     String feeding one `search: Vector<384>`.
//!   * `[T]` is ALWAYS a to-many relation (never a scalar list). Edge fields go
//!     in `{ ... }` and must be scalar.
//!   * `@inverse(Type.field)` back-relations are virtual (read-only); the named
//!     target `Type.field` must exist.
//!   * Field names may not contain `__` (engine sidecar reservation).
//!   * `@on_delete(remove|cascade|deny)` sets referential delete policy.
//!
//! Single-site: there are NO tenant/tenancy columns. Custom post types (CPTs)
//! ride the `Post.post_type` discriminator (an `@indexed` scalar), not per-CPT
//! SDL types. Auth is stateless signed tokens — there is no `Session` entity —
//! but `User` carries `password_reset_token` (@indexed) + `password_reset_expires`
//! for the reset flow. A `Comment` attaches to EITHER a `Post` OR a `Page`: it
//! holds BOTH `post` and `page` relations (mirroring `Revision`'s dual
//! `parent_post`/`parent_page`); the mutual exclusion is enforced app-level.
//!
//! Embedding model/dim: `all-MiniLM-L6-v2` @ 384 dims. This is a DEPLOY-TIME
//! decision baked into every `@index(hnsw)` at first open and effectively
//! immutable (changing it is a re-index migration): change it before the first
//! production open.

use rhypedb_schema::{Schema, SchemaResult};

/// The embedding model used by every `@vectorize` field.
pub const EMBEDDING_MODEL: &str = "all-MiniLM-L6-v2";
/// The embedding dimension; must match `EMBEDDING_MODEL`.
pub const EMBEDDING_DIM: u32 = 384;

/// The complete Ferropress content schema, as rhypedb SDL.
pub const SCHEMA_SDL: &str = r#"
type User {
    uuid: String @unique
    slug: String @unique
    display_name: String
    email: String @unique
    role: String @indexed
    password_hash: String
    password_reset_token: String @indexed
    password_reset_expires: String
    url: String
    bio: String
    plaintext: String
    activation_key: String
    created_at: String @indexed
    meta: String

    search: Vector<384>
        @vectorize(source: "plaintext", model: "all-MiniLM-L6-v2")
        @index(hnsw, metric: cosine, quantization: turboquant_3bit)

    avatar_media: Media @on_delete(remove)
}

type Media {
    uuid: String @unique
    slug: String @indexed
    filename: String
    mime_type: String @indexed
    byte_size: u64
    width: u32
    height: u32
    alt_text: String
    caption: String
    description: String
    blob_key: String @unique
    plaintext: String
    focal_x: f32
    focal_y: f32
    created_at: String @indexed
    meta: String

    search: Vector<384>
        @vectorize(source: "plaintext", model: "all-MiniLM-L6-v2")
        @index(hnsw, metric: cosine, quantization: turboquant_3bit)

    uploaded_by: User @on_delete(remove)
}

type Taxonomy {
    key: String @unique
    label: String
    hierarchical: Bool
    multiple: Bool
    meta: String
}

type Term {
    slug: String @indexed
    name: String
    description: String
    plaintext: String
    count: u32
    meta: String

    search: Vector<384>
        @vectorize(source: "plaintext", model: "all-MiniLM-L6-v2")
        @index(hnsw, metric: cosine, quantization: turboquant_3bit)

    taxonomy: Taxonomy @on_delete(cascade)
    parent: Term @on_delete(remove)
}

type Post {
    uuid: String @unique
    slug: String @indexed
    title: String
    status: String @indexed
    post_type: String @indexed
    block_tree: String
    plaintext: String
    excerpt: String
    seo: String
    meta: String
    created_at: String @indexed
    updated_at: String
    published_at: String @indexed
    deleted_at: String @indexed
    content_hash: String @indexed

    search: Vector<384>
        @vectorize(source: "plaintext", model: "all-MiniLM-L6-v2")
        @index(hnsw, metric: cosine, quantization: turboquant_3bit)

    author: User @on_delete(deny)
    featured_media: Media @on_delete(remove)
    terms: [Term] @on_delete(remove)
    comments: [Comment] @inverse(Comment.post)
    revisions: [Revision] @inverse(Revision.parent_post)
}

type Page {
    uuid: String @unique
    slug: String @indexed
    title: String
    status: String @indexed
    block_tree: String
    plaintext: String
    excerpt: String
    seo: String
    meta: String
    menu_order: i32 @indexed
    template: String
    created_at: String @indexed
    updated_at: String
    published_at: String @indexed
    deleted_at: String @indexed
    content_hash: String @indexed

    search: Vector<384>
        @vectorize(source: "plaintext", model: "all-MiniLM-L6-v2")
        @index(hnsw, metric: cosine, quantization: turboquant_3bit)

    author: User @on_delete(deny)
    parent: Page @on_delete(remove)
    featured_media: Media @on_delete(remove)
    comments: [Comment] @inverse(Comment.page)
    revisions: [Revision] @inverse(Revision.parent_page)
}

type Comment {
    status: String @indexed
    block_tree: String
    body: String
    plaintext: String
    author_name: String
    author_email: String
    author_url: String
    author_ip: String
    user_agent: String
    created_at: String @indexed
    meta: String

    post: Post @on_delete(cascade)
    page: Page @on_delete(cascade)
    author: User @on_delete(remove)
    parent: Comment @on_delete(cascade)
}

type Menu {
    slug: String @unique
    name: String
    location: String @indexed
    meta: String
}

type MenuItem {
    label: String
    item_order: i32 @indexed
    target: String
    meta: String

    menu: Menu @on_delete(cascade)
    parent: MenuItem @on_delete(cascade)
}

type Setting {
    key: String @unique
    value: String
    autoload: Bool @indexed
}

type Revision {
    kind: String @indexed
    title: String
    block_tree: String
    plaintext: String
    created_at: String @indexed

    parent_post: Post @on_delete(cascade)
    parent_page: Page @on_delete(cascade)
    author: User @on_delete(remove)
}

type Redirect {
    from_path: String @unique
    to_path: String
    status_code: u32
}
"#;

/// Parse + validate the canonical schema. Wraps
/// `rhypedb_schema::parser::parse_schema` (not re-exported at the schema crate
/// root, so we reach it by module path).
pub fn parsed_schema() -> SchemaResult<Schema> {
    rhypedb_schema::parser::parse_schema(SCHEMA_SDL)
}

/// The canonical SDL as a `&'static str`. Thin accessor so call sites can read
/// the schema source without naming the const directly (and so the name is
/// stable if the const ever moves behind a build step).
pub fn schema_sdl() -> &'static str {
    SCHEMA_SDL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical schema must parse + validate. This is the cheapest guard
    /// against an SDL typo (wrong directive, `__` in a field name, unknown
    /// relation target, non-String @vectorize source).
    #[test]
    fn canonical_schema_parses_and_validates() {
        let schema = parsed_schema().expect("Ferropress SDL must parse + validate");
        // Spot-check a few load-bearing facts.
        assert!(schema.get_type("Post").is_some());
        assert!(schema.get_type("Page").is_some());
        assert!(schema.get_type("Comment").is_some());
        assert!(schema.get_type("User").is_some());
        // 12 content types, single-site (no tenancy types).
        assert_eq!(schema.types.len(), 12);
    }

    /// Guard the GLOBAL DECISION embedding choice so a stray edit can't silently
    /// change the model/dim baked into every HNSW index.
    #[test]
    fn embedding_model_and_dim_are_pinned() {
        assert_eq!(EMBEDDING_MODEL, "all-MiniLM-L6-v2");
        assert_eq!(EMBEDDING_DIM, 384);
    }
}
