//! `Taxonomy` + `Term` — the WP taxonomy system, collapsed from 3 tables to 2.
//!
//! WP splits terms (lexical) from term_taxonomy (which taxonomy + parent/count)
//! so one slug can live in multiple taxonomies — rarely useful. We collapse:
//! a `Term` belongs to exactly one `Taxonomy` (a relation), carries its own
//! `parent` self-relation for hierarchy, and a derived `count`. The object<->term
//! M:N join is the native rhypedb relation `Post.terms <-> Term`, traversable
//! from BOTH sides: the SDL declares a virtual `Term.objects @inverse(Post.terms)`
//! back-relation (not a struct field here — inverses are read-only, queried via
//! `get_links`, like `Post.comments`), which is what the term-archive listing and
//! the live term counts enumerate "posts in term T" through.
//!
//! Slug uniqueness is PER-SIBLING (the WP model, app-enforced — `Term.slug` is
//! `@indexed`, not `@unique`): a slug is unique among terms sharing a (taxonomy,
//! parent), so a nested archive URL's ancestor chain is what disambiguates it.

use crate::value::ObjectId;

/// A taxonomy registry entry (category / tag / custom).
#[derive(Debug, Clone, PartialEq)]
pub struct Taxonomy {
    pub id: Option<ObjectId>,
    /// Stable key, e.g. `"category"`, `"tag"`. Unique.
    pub key: String,
    pub label: String,
    /// Categories are hierarchical; tags are flat.
    pub hierarchical: bool,
    /// Whether an object may hold multiple terms of this taxonomy.
    pub multiple: bool,
    pub meta: serde_json::Value,
}

/// A term within exactly one taxonomy.
#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub id: Option<ObjectId>,
    pub slug: String,
    pub name: String,
    pub description: String,
    /// name + description; single `@vectorize` source for `search` (semantic
    /// term browse).
    pub plaintext: String,
    /// INTENTIONALLY DEAD in v1: nothing writes or reads this column. Counts are
    /// DERIVED live (published-filtered) from the `Term.objects` inverse at each
    /// point of display — a stored count cannot be reliably maintained because
    /// membership edits are eventless `link`/`unlink` calls. Kept only because
    /// dropping an SDL column is a gated schema SHRINK.
    pub count: u32,
    pub meta: serde_json::Value,

    pub taxonomy: Option<ObjectId>, // -> Taxonomy (one)
    pub parent: Option<ObjectId>,   // -> Term (hierarchy, when hierarchical)
}
