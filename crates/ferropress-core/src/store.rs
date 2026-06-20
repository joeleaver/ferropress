//! The `RhypeStore` PORT — the central abstraction over the typed object store.
//!
//! This trait is *async* even though rhypedb's engine verbs are all synchronous:
//! the engine does blocking LSM I/O, so the embedded adapter wraps each call in
//! `tokio::task::spawn_blocking` and bridges the synchronous change-subscription
//! `mpsc::Receiver` into an async `Stream`. Making the port async is what lets
//! the HTTP/serve layers stay non-blocking and lets a future networked adapter
//! drop in without changing call sites.
//!
//! INVARIANT: every type in this signature is a `ferropress-core` type. No
//! rhypedb type appears here — that is the whole point of the port. The verbs
//! mirror, one-to-one where it matters, the engine surface we verified in
//! `rhypedb-engine::database`: create/create_batch/get/get_many/update/delete,
//! link/unlink/get_links, filter_scan* (→ `filter`), the vectorizer search
//! (→ `vector_search`), and the subscription hub (→ `subscribe`).

use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::error::Result;
use crate::query::{Change, Edge, FilterSpec, ScoredId, SubscribeFilter, VectorQuery};
use crate::value::{FieldMap, Object, ObjectId, TypeName};

/// A typed object store with relationships, semantic search, and a change feed.
///
/// Implementors: `ferropress-store-embedded::EmbeddedStore` (the only one today,
/// over the embedded rhypedb engine). The trait is object-safe so the server can
/// hold an `Arc<dyn RhypeStore>` and inject it everywhere.
#[async_trait]
pub trait RhypeStore: Send + Sync + 'static {
    /// Create one object; returns its new id. `fields` must satisfy the schema
    /// for `type_`.
    async fn create(&self, type_: &TypeName, fields: FieldMap) -> Result<ObjectId>;

    /// Create many objects in one engine batch.
    async fn create_batch(&self, type_: &TypeName, rows: Vec<FieldMap>) -> Result<Vec<ObjectId>>;

    /// Read one object by id. Errors `NotFound` if absent.
    async fn get(&self, type_: &TypeName, id: ObjectId) -> Result<Object>;

    /// Read many objects; missing ids are skipped (mirrors `get_many`).
    async fn get_many(&self, type_: &TypeName, ids: &[ObjectId]) -> Result<Vec<Object>>;

    /// Full type scan (use sparingly; prefer `filter`).
    async fn scan(&self, type_: &TypeName) -> Result<Vec<Object>>;

    /// Patch a subset of an object's fields.
    async fn update(&self, type_: &TypeName, id: ObjectId, patch: FieldMap) -> Result<()>;

    /// Delete an object; the engine enforces declared `@on_delete` policies.
    async fn delete(&self, type_: &TypeName, id: ObjectId) -> Result<()>;

    /// Add a relation edge from `from` to `to`, with optional edge fields
    /// (validated against the relation's declared edge scalar types).
    async fn link(&self, from: &Edge, to: ObjectId, edge_fields: FieldMap) -> Result<()>;

    /// Remove a relation edge.
    async fn unlink(&self, from: &Edge, to: ObjectId) -> Result<()>;

    /// Traverse a relation. Returns the linked ids paired with their edge fields
    /// (transparently uses the reverse-edge index for `@inverse` fields).
    async fn get_links(&self, from: &Edge) -> Result<Vec<(ObjectId, FieldMap)>>;

    /// Batched relation traversal: for ONE relation `field` of `type_`, resolve
    /// the linked target ids for EACH source id in `ids`, returning one
    /// `Vec<ObjectId>` per input id in the SAME order. This is the id-only fast
    /// path that collapses what would otherwise be N separate [`get_links`] calls
    /// into a single store round-trip (e.g. resolving the `parent` of every
    /// comment on a page at once). Edge fields are intentionally dropped — callers
    /// that need them use [`get_links`].
    ///
    /// The default implementation simply loops [`get_links`] (correct, but N
    /// round-trips); the embedded adapter overrides it with the engine's native
    /// batched traversal so production never pays the N+1.
    async fn get_links_many(
        &self,
        type_: &TypeName,
        ids: &[ObjectId],
        field: &str,
    ) -> Result<Vec<Vec<ObjectId>>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let edge = Edge {
                type_name: type_.clone(),
                id,
                field: field.to_owned(),
            };
            let links = self.get_links(&edge).await?;
            out.push(links.into_iter().map(|(tid, _edge_fields)| tid).collect());
        }
        Ok(out)
    }

    /// Indexed single-predicate scan (maps to the engine's `filter_scan*` fast
    /// path). Returns matching objects.
    async fn filter(&self, spec: FilterSpec) -> Result<Vec<Object>>;

    /// Semantic / vector search over a `@vectorize`d field.
    async fn vector_search(&self, query: VectorQuery) -> Result<Vec<ScoredId>>;

    /// Subscribe to the change feed. Returns a `'static` boxed async stream of
    /// `Change`s; the adapter pumps the engine's synchronous `mpsc::Receiver`
    /// onto this stream from a dedicated forwarder thread.
    async fn subscribe(&self, filter: SubscribeFilter) -> Result<BoxStream<'static, Change>>;
}
