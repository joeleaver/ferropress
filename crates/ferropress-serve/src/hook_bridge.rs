//! The change-feed → **ACTION hook** bridge.
//!
//! Phase 2 wired the *synchronous, pre-persist* `comment.create` **filter** (a
//! plugin can change a comment before it is written). This is the *asynchronous,
//! post-persist* counterpart: it consumes [`RhypeStore::subscribe`] — the SAME
//! committed-change feed the [`ServeEngine`](crate::ServeEngine) regen loop reads
//! — and, for every change, dispatches a WordPress-style **action** so plugins can
//! REACT to a write that already happened (a new comment posted, a post
//! published, …). Actions observe; their return is ignored.
//!
//! The bridge takes its OWN subscription (the engine hub fans out to every
//! subscriber), so it is fully decoupled from regeneration: a slow or failing
//! action hook never affects cache regen, and vice-versa.
//!
//! ## Event vocabulary
//!
//! Each change becomes the action `"<type>.<verb>"` where `<type>` is the lower-
//! cased entity type and `<verb>` is the past tense of the change kind:
//! `post.created`, `page.updated`, `comment.deleted`, … The past-tense verb
//! deliberately distinguishes these post-persist actions from a pre-persist
//! *filter* like `comment.create`. The payload is the change identity plus its
//! changed fields: `{ version, type, kind, object_id, fields? }`. `fields` is the
//! engine's JSON projection of the changed scalar fields, forwarded verbatim, so it
//! is present whenever the change carried scalars — on create/update AND on delete
//! (which carries the deleted object's pre-delete scalars).

use std::sync::Arc;

use ferropress_core::hook::{HookDispatcher, HookEvent, HookKind};
use ferropress_core::query::{Change, ChangeKind, SubscribeFilter};
use ferropress_core::store::RhypeStore;

/// Bridges the committed-change feed into action-hook dispatches. One per server,
/// spawned by the composition root alongside the regen loop.
pub struct HookBridge {
    store: Arc<dyn RhypeStore>,
    hooks: Arc<dyn HookDispatcher>,
}

impl HookBridge {
    /// Build the bridge over the store (to subscribe) and the hook dispatcher (the
    /// plugin host). Both are the same `Arc`s the rest of the server shares.
    pub fn new(store: Arc<dyn RhypeStore>, hooks: Arc<dyn HookDispatcher>) -> Self {
        Self { store, hooks }
    }

    /// Run forever: subscribe to ALL changes and dispatch one action per change.
    /// The loop ends only when the change feed does (the store / process is going
    /// away). A per-change failure is logged inside [`dispatch_change`] and never
    /// tears the loop down.
    pub async fn run(&self) -> ferropress_core::error::Result<()> {
        // `tokio-stream`'s `StreamExt::next` drives the `BoxStream` (the concrete
        // `SubscriptionStream` is `Unpin`), mirroring the regen loop.
        use tokio_stream::StreamExt;

        let mut stream = self.store.subscribe(SubscribeFilter::default()).await?;
        tracing::info!("hook bridge subscribed to the change feed");

        while let Some(change) = stream.next().await {
            self.dispatch_change(&change).await;
        }

        tracing::info!("hook bridge change feed ended");
        Ok(())
    }

    /// Dispatch the action for ONE change. Gated on a hook actually being
    /// registered for the action name, so a no-action-plugin deployment pays only a
    /// map lookup per change (no payload build, no thread hop). The extism call is
    /// synchronous + CPU-bound, so it runs on `spawn_blocking`. The dispatch result
    /// is ignored (actions observe) beyond logging a failure — one bad plugin must
    /// never break the feed.
    pub(crate) async fn dispatch_change(&self, change: &Change) {
        let name = action_name(change);
        if !self.hooks.has_hooks(&name) {
            return;
        }
        let event = HookEvent {
            name,
            kind: HookKind::Action,
            payload: change_payload(change),
        };
        let hooks = Arc::clone(&self.hooks);
        match tokio::task::spawn_blocking(move || hooks.dispatch(event)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "action hook dispatch failed"),
            Err(e) => tracing::error!(error = %e, "action hook dispatch task panicked"),
        }
    }
}

/// The action hook name for a change: `"<lowercased type>.<past-tense verb>"`
/// (e.g. `Post`/`Create` → `"post.created"`).
pub(crate) fn action_name(change: &Change) -> String {
    let verb = match change.kind {
        ChangeKind::Create => "created",
        ChangeKind::Update => "updated",
        ChangeKind::Delete => "deleted",
    };
    format!("{}.{verb}", change.type_name.as_str().to_ascii_lowercase())
}

/// The action payload: the change's identity plus its changed scalar `fields`.
/// `kind` is the present-tense verb (`create`/`update`/`delete`); `fields` is the
/// engine's JSON projection of the changed scalars, forwarded verbatim (so a plugin
/// sees ordinary JSON — `"n": 1`, `Bytes` base64, `DateTime` RFC3339), and is
/// present whenever the change carried fields (create/update and, now, delete).
pub(crate) fn change_payload(change: &Change) -> serde_json::Value {
    let kind = match change.kind {
        ChangeKind::Create => "create",
        ChangeKind::Update => "update",
        ChangeKind::Delete => "delete",
    };

    let mut obj = serde_json::Map::new();
    obj.insert("version".to_owned(), change.version.into());
    obj.insert(
        "type".to_owned(),
        serde_json::Value::from(change.type_name.as_str()),
    );
    obj.insert("kind".to_owned(), serde_json::Value::from(kind));
    obj.insert("object_id".to_owned(), change.object_id.0.into());
    // `change.fields` is already the engine's JSON projection — forward it verbatim
    // (the plugin receives the changed scalar fields as ordinary JSON).
    if let Some(fields) = &change.fields {
        obj.insert("fields".to_owned(), fields.clone());
    }
    serde_json::Value::Object(obj)
}
