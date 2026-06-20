//! Query / mutation support types for the `RhypeStore` port: filter specs,
//! vector queries, link edges, and the change-feed event. All in core's own
//! vocabulary; the embedded adapter maps these onto `filter_scan*`,
//! `Vectorizer::search_*`, `link`/`get_links`, and the `ChangeEvent` stream.

use crate::value::{FieldMap, ObjectId, TypeName, Value};

/// A single-field comparison, mirroring rhypedb's storage `CompareOp`
/// (Eq/Ne/Lt/Le/Gt/Ge). The adapter forwards it to the matching `filter_scan*`
/// fast path (integer / bool / float / str / bytes pushdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A typed-scan request: "objects of `type_name` where `field` `op` `value`,
/// up to `limit`". Deliberately single-predicate to match the engine's indexed
/// `filter_scan*` surface; compound predicates are composed by the caller (or
/// land in a later query-language pass via `rhypedb-query`).
#[derive(Debug, Clone)]
pub struct FilterSpec {
    pub type_name: TypeName,
    pub field: String,
    pub op: Compare,
    pub value: Value,
    pub limit: Option<usize>,
}

/// A semantic-search request against a `@vectorize`d field. `restrict` optionally
/// constrains the candidate set to a pre-filtered id list (the engine takes an
/// exact brute-force path for small restrict sets).
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub type_name: TypeName,
    /// The Vector field to search (e.g. `"search"`).
    pub vector_field: String,
    pub query_text: String,
    pub k: usize,
    pub ef: usize,
    pub rerank: bool,
    pub restrict: Option<Vec<ObjectId>>,
}

/// A scored hit from a vector search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredId {
    pub id: ObjectId,
    pub score: f32,
}

/// Identifies one end of a relation traversal: "the `field` relation of
/// `(type_name, id)`". Used by `link`/`unlink`/`get_links`.
#[derive(Debug, Clone)]
pub struct Edge {
    pub type_name: TypeName,
    pub id: ObjectId,
    pub field: String,
}

/// Filter for the change subscription, mirroring rhypedb's `SubscriptionFilter`.
#[derive(Debug, Clone, Default)]
pub struct SubscribeFilter {
    pub type_name: Option<TypeName>,
    pub object_id: Option<ObjectId>,
    /// Empty = all kinds.
    pub kinds: Vec<ChangeKind>,
}

/// The kind of a change event (mirrors rhypedb `ChangeKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Create,
    Update,
    Delete,
}

/// A committed change, mirroring rhypedb's `ChangeEvent`. The regen loop in
/// `ferropress-serve` consumes a stream of these to invalidate exactly the
/// affected prerendered pages. `fields` carries only scalar fields (the engine
/// does not publish relation/vector fields on the change feed).
#[derive(Debug, Clone)]
pub struct Change {
    pub version: u64,
    pub kind: ChangeKind,
    pub type_name: TypeName,
    pub object_id: ObjectId,
    pub fields: Option<FieldMap>,
}
