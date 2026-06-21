//! Hook-bus event vocabulary (WP actions/filters), consumed by
//! `ferropress-plugin-host`. The store change-feed is bridged into these so
//! plugins can react to content changes.
//!
//! The dispatch *engine* lives in the plugin host (it owns the WASM runtime), but
//! the [`HookDispatcher`] **port** is declared here so request handlers can run a
//! hook through `Arc<dyn HookDispatcher>` without depending on the plugin host /
//! extism — exactly mirroring how `CustomBlockRenderer` keeps the WASM runtime out
//! of the render crate. A deployment with no plugins uses [`NoHooks`].

use crate::error::Result;

/// Whether a hook observes (action) or transforms (filter) its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Action,
    Filter,
}

/// A named hook point with a JSON payload.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub name: String,
    pub kind: HookKind,
    pub payload: serde_json::Value,
}

/// Runs an event through the registered hooks. Implemented by the plugin host;
/// consumed by handlers (e.g. the comment-create path) through an
/// `Arc<dyn HookDispatcher>` carried on the app state.
///
/// `Send + Sync` so it can be shared across the async serve path and moved into a
/// `spawn_blocking` (the underlying WASM call is synchronous + CPU-bound).
pub trait HookDispatcher: Send + Sync {
    /// Run `event` through every hook registered for its `name`, in priority
    /// order, returning the (possibly filter-transformed) event. A misbehaving
    /// plugin is logged and skipped inside the implementation — dispatch is never
    /// fatal to the caller; the only `Err` is a failure to serialize the payload.
    fn dispatch(&self, event: HookEvent) -> Result<HookEvent>;

    /// Whether any hook is registered for `name`. Lets a caller skip building a
    /// payload and hopping to a blocking thread when nothing listens, so the
    /// common no-plugin path keeps zero overhead.
    fn has_hooks(&self, name: &str) -> bool;
}

/// A [`HookDispatcher`] with no hooks: every event passes through unchanged. Used
/// when no plugin host is wired (tests, a deployment running without plugins).
pub struct NoHooks;

impl HookDispatcher for NoHooks {
    fn dispatch(&self, event: HookEvent) -> Result<HookEvent> {
        Ok(event)
    }

    fn has_hooks(&self, _name: &str) -> bool {
        false
    }
}
