//! `impl RhypeStore for EmbeddedStore`.
//!
//! The engine verbs are synchronous and do blocking LSM I/O, so each async port
//! method offloads to `tokio::task::spawn_blocking` over a cloned `Arc<Database>`
//! (cheap; the Arc is the intended sharing handle). `subscribe` is special: the
//! engine's hub is `std::sync::mpsc` (sync), so we spawn a dedicated OS thread
//! that pumps the blocking `Receiver` into a Tokio channel, and hand back a
//! `BoxStream` over that channel — the standard sync->async bridge for this hub.
//!
//! Bodies are `todo!()` for this scaffold; every signature is real and the
//! commented sketch in each method records the exact wiring (the verb, the
//! `convert::*` calls, the error map) so filling them in is mechanical.

use std::sync::Arc;

use async_trait::async_trait;
use futures_core::stream::BoxStream;

use ferropress_core::error::Result as CoreResult;
use ferropress_core::query::{Change, Edge, FilterSpec, ScoredId, SubscribeFilter, VectorQuery};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName};

use crate::EmbeddedStore;

#[async_trait]
impl RhypeStore for EmbeddedStore {
    async fn create(&self, _type_: &TypeName, _fields: FieldMap) -> CoreResult<ObjectId> {
        // let db = Arc::clone(self.db());
        // let db_fields = convert::to_db_fields(fields);
        // let type_name = type_.as_str().to_owned();
        // let obj = tokio::task::spawn_blocking(move || db.create(&type_name, db_fields))
        //     .await
        //     .expect("blocking task panicked")
        //     .map_err(AdapterError::from)?;
        // Ok(ObjectId(obj.id))
        // TODO: spawn_blocking over Database::create + convert::to_db_fields.
        todo!("spawn_blocking over Database::create + convert::to_db_fields")
    }

    async fn create_batch(
        &self,
        _type_: &TypeName,
        _rows: Vec<FieldMap>,
    ) -> CoreResult<Vec<ObjectId>> {
        // TODO: spawn_blocking over Database::create_batch (one engine batch).
        todo!("spawn_blocking over Database::create_batch")
    }

    async fn get(&self, _type_: &TypeName, _id: ObjectId) -> CoreResult<Object> {
        // TODO: spawn_blocking over Database::get + convert::from_db_object
        // (which ensures fields are deserialized before reading them).
        todo!("spawn_blocking over Database::get + convert::from_db_object")
    }

    async fn get_many(&self, _type_: &TypeName, _ids: &[ObjectId]) -> CoreResult<Vec<Object>> {
        // TODO: spawn_blocking over Database::get_many (silently skips missing).
        todo!("spawn_blocking over Database::get_many (skips missing)")
    }

    async fn scan(&self, _type_: &TypeName) -> CoreResult<Vec<Object>> {
        // TODO: spawn_blocking over Database::scan_type (full type scan).
        todo!("spawn_blocking over Database::scan_type")
    }

    async fn update(&self, _type_: &TypeName, _id: ObjectId, _patch: FieldMap) -> CoreResult<()> {
        // TODO: spawn_blocking over Database::update (partial field patch).
        todo!("spawn_blocking over Database::update")
    }

    async fn delete(&self, _type_: &TypeName, _id: ObjectId) -> CoreResult<()> {
        // TODO: spawn_blocking over Database::delete (enforces @on_delete).
        todo!("spawn_blocking over Database::delete (enforces @on_delete)")
    }

    async fn link(&self, _from: &Edge, _to: ObjectId, _edge_fields: FieldMap) -> CoreResult<()> {
        // TODO: spawn_blocking over Database::link with Some(edge_fields).
        todo!("spawn_blocking over Database::link with Some(edge_fields)")
    }

    async fn unlink(&self, _from: &Edge, _to: ObjectId) -> CoreResult<()> {
        // TODO: spawn_blocking over Database::unlink.
        todo!("spawn_blocking over Database::unlink")
    }

    async fn get_links(&self, _from: &Edge) -> CoreResult<Vec<(ObjectId, FieldMap)>> {
        // TODO: spawn_blocking over Database::get_links; map (u64, FieldMap)
        // pairs into (ObjectId, core FieldMap) via convert::from_db_fields.
        todo!("spawn_blocking over Database::get_links; map (u64, FieldMap) pairs")
    }

    async fn filter(&self, _spec: FilterSpec) -> CoreResult<Vec<Object>> {
        // Dispatch on the Value variant to the right filter_scan* overload:
        //   String -> filter_scan_str, Bool -> filter_scan_bool,
        //   F32/F64 -> filter_scan_float, integers -> filter_scan, Bytes ->
        //   filter_scan_bytes. op via convert::to_compare_op.
        // TODO: dispatch FilterSpec onto the typed filter_scan* fast paths.
        todo!("dispatch FilterSpec onto the typed filter_scan* fast paths")
    }

    async fn vector_search(&self, _query: VectorQuery) -> CoreResult<Vec<ScoredId>> {
        // let v = Arc::clone(self.vectorizer());
        // let restrict = convert::to_restrict_set(query.restrict);
        // spawn_blocking: v.search_text(&type, &field, &text, k, ef, rerank,
        //     restrict.as_ref())  // search_text wants Option<&HashSet<u64>>
        // map Vec<(u64, f32)> -> Vec<ScoredId { id: ObjectId, score }>.
        // TODO: spawn_blocking over Vectorizer::search_text.
        let _ = self.vectorizer(); // held here; consumed once search_text lands
        todo!("spawn_blocking over Vectorizer::search_text")
    }

    async fn subscribe(&self, _filter: SubscribeFilter) -> CoreResult<BoxStream<'static, Change>> {
        // let (sub_id, rx) = self.db().subscriptions().subscribe(db_filter);
        // let (tx, async_rx) = tokio::sync::mpsc::unbounded_channel();
        // std::thread::spawn(move || while let Ok(ev) = rx.recv() {
        //     if tx.send(convert::from_change_event(ev)).is_err() { break; } });
        // Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(async_rx)))
        // NB: `subscribe` returns (u64, std::sync::mpsc::Receiver<ChangeEvent>);
        // store sub_id somewhere droppable for explicit unsubscribe, else the hub
        // auto-prunes when the receiver thread exits on send failure.
        let _ = Arc::clone(self.db());
        // TODO: subscribe + forwarder-thread bridge -> BoxStream<Change>.
        todo!("subscribe + forwarder-thread bridge -> BoxStream<Change>")
    }
}
