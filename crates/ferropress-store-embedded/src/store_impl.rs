//! `impl RhypeStore for EmbeddedStore`.
//!
//! The engine verbs are synchronous and do blocking LSM I/O, so each async port
//! method offloads to `tokio::task::spawn_blocking` over a cloned `Arc<Database>`
//! (cheap; the Arc is the intended sharing handle). Each closure captures only
//! owned, `Send` values — a cloned `Arc`, owned `String` type-names, owned
//! `FieldMap`s, an owned `Vec<u64>` of ids — never `&self` or a borrow of the
//! caller's args, so the blocking work outlives this stack frame safely.
//!
//! `subscribe` is special: the engine's hub is `std::sync::mpsc` (sync), so we
//! spawn a dedicated OS thread that pumps the blocking `Receiver` into a Tokio
//! channel and hand back a `BoxStream` over that channel — the standard
//! sync->async bridge for this hub. Dropping the returned stream runs an
//! unsubscribe guard that removes the subscription from the hub; that drops the
//! hub's `Sender`, so the forwarder thread's blocking `rx.recv()` returns `Err`
//! and the thread exits deterministically — no leaked thread or hub entry, no
//! waiting on a future `publish`.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use futures_core::stream::BoxStream;
use rhypedb_engine::database::Database;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::query::{Change, Edge, FilterSpec, ScoredId, SubscribeFilter, VectorQuery};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};

use crate::{AdapterError, EmbeddedStore, convert};

/// Flatten the two failure layers of a `spawn_blocking` over an engine verb:
/// the outer `JoinError` (task panicked / was cancelled) and the inner
/// `EngineError`. Both collapse into the appropriate `CoreError` — engine errors
/// through the adapter's precise mapping, a join failure into `CoreError::Store`
/// (an internal runtime fault, not a domain error).
fn join<T>(
    joined: Result<rhypedb_engine::error::EngineResult<T>, tokio::task::JoinError>,
) -> CoreResult<T> {
    match joined {
        Ok(engine_result) => engine_result.map_err(|e| AdapterError::from(e).into()),
        Err(join_err) => Err(CoreError::Store(format!(
            "blocking task failed: {join_err}"
        ))),
    }
}

#[async_trait]
impl RhypeStore for EmbeddedStore {
    async fn create(&self, type_: &TypeName, fields: FieldMap) -> CoreResult<ObjectId> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let db_fields = convert::to_db_fields(fields);
        let obj =
            join(tokio::task::spawn_blocking(move || db.create(&type_name, db_fields)).await)?;
        Ok(ObjectId(obj.id))
    }

    async fn create_batch(
        &self,
        type_: &TypeName,
        rows: Vec<FieldMap>,
    ) -> CoreResult<Vec<ObjectId>> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let db_rows: Vec<_> = rows.into_iter().map(convert::to_db_fields).collect();
        let objs =
            join(tokio::task::spawn_blocking(move || db.create_batch(&type_name, db_rows)).await)?;
        Ok(objs.into_iter().map(|o| ObjectId(o.id)).collect())
    }

    async fn get(&self, type_: &TypeName, id: ObjectId) -> CoreResult<Object> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let ObjectId(oid) = id;
        let obj = join(tokio::task::spawn_blocking(move || db.get(&type_name, oid)).await)?;
        Ok(convert::from_db_object(obj))
    }

    async fn get_many(&self, type_: &TypeName, ids: &[ObjectId]) -> CoreResult<Vec<Object>> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        // `get_many` borrows `&[u64]`, so materialize an owned Vec and let the
        // blocking closure lend it the slice (missing ids are silently skipped
        // by the engine; the result is sorted + deduped).
        let ids: Vec<u64> = ids.iter().map(|&ObjectId(n)| n).collect();
        let objs = join(tokio::task::spawn_blocking(move || db.get_many(&type_name, &ids)).await)?;
        Ok(objs.into_iter().map(convert::from_db_object).collect())
    }

    async fn scan(&self, type_: &TypeName) -> CoreResult<Vec<Object>> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let objs = join(tokio::task::spawn_blocking(move || db.scan_type(&type_name)).await)?;
        Ok(objs.into_iter().map(convert::from_db_object).collect())
    }

    async fn update(&self, type_: &TypeName, id: ObjectId, patch: FieldMap) -> CoreResult<()> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let ObjectId(oid) = id;
        let db_patch = convert::to_db_fields(patch);
        // `update` returns the merged Object; the port contract is `()`, so the
        // merged object is discarded (callers re-`get` if they need it).
        join(tokio::task::spawn_blocking(move || db.update(&type_name, oid, db_patch)).await)?;
        Ok(())
    }

    async fn delete(&self, type_: &TypeName, id: ObjectId) -> CoreResult<()> {
        let db = Arc::clone(self.db());
        let type_name = type_.as_str().to_owned();
        let ObjectId(oid) = id;
        join(tokio::task::spawn_blocking(move || db.delete(&type_name, oid)).await)
    }

    async fn link(&self, from: &Edge, to: ObjectId, edge_fields: FieldMap) -> CoreResult<()> {
        let db = Arc::clone(self.db());
        let source_type = from.type_name.as_str().to_owned();
        let ObjectId(source_id) = from.id;
        let field_name = from.field.clone();
        let ObjectId(target_id) = to;
        let db_edge = convert::to_db_fields(edge_fields);
        join(
            tokio::task::spawn_blocking(move || {
                db.link(
                    &source_type,
                    source_id,
                    &field_name,
                    target_id,
                    Some(db_edge),
                )
            })
            .await,
        )
    }

    async fn unlink(&self, from: &Edge, to: ObjectId) -> CoreResult<()> {
        let db = Arc::clone(self.db());
        let source_type = from.type_name.as_str().to_owned();
        let ObjectId(source_id) = from.id;
        let field_name = from.field.clone();
        let ObjectId(target_id) = to;
        join(
            tokio::task::spawn_blocking(move || {
                db.unlink(&source_type, source_id, &field_name, target_id)
            })
            .await,
        )
    }

    async fn get_links(&self, from: &Edge) -> CoreResult<Vec<(ObjectId, FieldMap)>> {
        let db = Arc::clone(self.db());
        let source_type = from.type_name.as_str().to_owned();
        let ObjectId(source_id) = from.id;
        let field_name = from.field.clone();
        let pairs = join(
            tokio::task::spawn_blocking(move || db.get_links(&source_type, source_id, &field_name))
                .await,
        )?;
        Ok(pairs
            .into_iter()
            .map(|(target_id, edge_fields)| {
                (ObjectId(target_id), convert::from_db_fields(edge_fields))
            })
            .collect())
    }

    async fn filter(&self, spec: FilterSpec) -> CoreResult<Vec<Object>> {
        let db = Arc::clone(self.db());
        let type_name = spec.type_name.as_str().to_owned();
        let field = spec.field;
        let op = convert::to_compare_op(spec.op);
        let limit = spec.limit;

        // Dispatch on the predicate value's variant onto the matching typed
        // `filter_scan*` fast path. Borrowed-target paths (str/bytes) move an
        // owned value into the closure and lend it. Integers widen to the i64
        // query literal the engine's integer fast path expects; f32 widens to
        // the f64 layout the float path uses. A `Null` predicate has no
        // `filter_scan` form, so it is rejected as a conversion error.
        let objs = match spec.value {
            Value::String(s) => join(
                tokio::task::spawn_blocking(move || {
                    db.filter_scan_str(&type_name, &field, op, &s, limit)
                })
                .await,
            )?,
            Value::Bool(b) => join(
                tokio::task::spawn_blocking(move || {
                    db.filter_scan_bool(&type_name, &field, op, b, limit)
                })
                .await,
            )?,
            Value::F32(n) => {
                let target = n as f64;
                join(
                    tokio::task::spawn_blocking(move || {
                        db.filter_scan_float(&type_name, &field, op, target, limit)
                    })
                    .await,
                )?
            }
            Value::F64(n) => join(
                tokio::task::spawn_blocking(move || {
                    db.filter_scan_float(&type_name, &field, op, n, limit)
                })
                .await,
            )?,
            Value::U32(n) => {
                let target = n as i64;
                join(
                    tokio::task::spawn_blocking(move || {
                        db.filter_scan(&type_name, &field, op, target, limit)
                    })
                    .await,
                )?
            }
            Value::U64(n) => {
                // The engine's integer filter path takes an `i64` target; a `u64`
                // above `i64::MAX` cannot be represented, so reject it rather than
                // wrap to a negative and return silently-wrong matches.
                let target = i64::try_from(n).map_err(|_| {
                    AdapterError::Conversion(format!(
                        "filter predicate u64 value {n} exceeds i64::MAX"
                    ))
                })?;
                join(
                    tokio::task::spawn_blocking(move || {
                        db.filter_scan(&type_name, &field, op, target, limit)
                    })
                    .await,
                )?
            }
            Value::I32(n) => {
                let target = n as i64;
                join(
                    tokio::task::spawn_blocking(move || {
                        db.filter_scan(&type_name, &field, op, target, limit)
                    })
                    .await,
                )?
            }
            Value::I64(n) => join(
                tokio::task::spawn_blocking(move || {
                    db.filter_scan(&type_name, &field, op, n, limit)
                })
                .await,
            )?,
            Value::Bytes(b) => join(
                tokio::task::spawn_blocking(move || {
                    db.filter_scan_bytes(&type_name, &field, op, &b, limit)
                })
                .await,
            )?,
            Value::Null => {
                return Err(AdapterError::Conversion(
                    "filter predicate value cannot be Null: no filter_scan path exists for a \
                     null literal"
                        .to_owned(),
                )
                .into());
            }
        };

        Ok(objs.into_iter().map(convert::from_db_object).collect())
    }

    async fn vector_search(&self, query: VectorQuery) -> CoreResult<Vec<ScoredId>> {
        let vectorizer = Arc::clone(self.vectorizer());
        let type_name = query.type_name.as_str().to_owned();
        let vector_field = query.vector_field;
        let query_text = query.query_text;
        let k = query.k;
        let ef = query.ef;
        let rerank = query.rerank;
        // Owned `Option<HashSet<u64>>`; `search_text` wants `Option<&HashSet<u64>>`,
        // so the closure lends it with `.as_ref()`.
        let restrict = convert::to_restrict_set(query.restrict);

        let hits = join(
            tokio::task::spawn_blocking(move || {
                vectorizer.search_text(
                    &type_name,
                    &vector_field,
                    &query_text,
                    k,
                    ef,
                    rerank,
                    restrict.as_ref(),
                )
            })
            .await,
        )?;

        Ok(hits
            .into_iter()
            .map(|(id, score)| ScoredId {
                id: ObjectId(id),
                score,
            })
            .collect())
    }

    async fn subscribe(&self, filter: SubscribeFilter) -> CoreResult<BoxStream<'static, Change>> {
        // Register with the engine's synchronous hub. `sub_id` is the explicit
        // unsubscribe handle, used by the drop guard below for deterministic
        // teardown.
        let db_filter = convert::to_subscription_filter(filter);
        let (sub_id, rx) = self.db().subscriptions().subscribe(db_filter);

        // Bridge the blocking `std::sync::mpsc::Receiver<ChangeEvent>` onto an
        // async `tokio::sync::mpsc` channel via a dedicated OS thread (a blocking
        // `recv` must not run on a Tokio worker). The channel is unbounded: the
        // change feed has no backpressure today (a bounded channel + drop/lag
        // policy is a deliberate later choice).
        let (tx, async_rx) = tokio::sync::mpsc::unbounded_channel::<Change>();
        std::thread::Builder::new()
            .name("ferropress-sub-forward".to_owned())
            .spawn(move || {
                // Exits deterministically when EITHER the consumer drops the
                // stream (the guard unsubscribes -> the hub drops our `Sender` ->
                // `rx.recv()` returns `Err`) OR a forward `send` fails.
                while let Ok(ev) = rx.recv() {
                    if tx.send(convert::from_change_event(ev)).is_err() {
                        break;
                    }
                }
            })
            // A thread-spawn failure (OS exhaustion) is a recoverable store error,
            // never a panic on a Tokio worker.
            .map_err(|e| {
                CoreError::Store(format!(
                    "failed to spawn subscription forwarder thread: {e}"
                ))
            })?;

        // Teardown is tied to the stream's lifetime: dropping the stream runs
        // `SubscriptionStream::drop`, which unsubscribes from the hub.
        Ok(Box::pin(SubscriptionStream {
            inner: UnboundedReceiverStream::new(async_rx),
            db: Arc::clone(self.db()),
            sub_id,
        }))
    }
}

/// The change-feed stream handed to callers. Wraps the async receiver and, on
/// drop, unsubscribes from the engine hub — which removes the hub's
/// `Subscription` (dropping its `Sender`), making the forwarder thread's blocking
/// `rx.recv()` return `Err` so the thread terminates. No leaked thread or hub
/// entry, and no dependence on a future `publish`.
struct SubscriptionStream {
    inner: UnboundedReceiverStream<Change>,
    db: Arc<Database>,
    sub_id: u64,
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        self.db.subscriptions().unsubscribe(self.sub_id);
    }
}

impl Stream for SubscriptionStream {
    type Item = Change;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `inner` is `Unpin` (so is the whole struct), so we can project safely.
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}
