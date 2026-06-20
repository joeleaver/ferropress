//! Hook-bus event vocabulary (WP actions/filters), consumed by
//! `ferropress-plugin-host`. The store change-feed is bridged into these so
//! plugins can react to content changes. Skeleton — the dispatch lives in the
//! plugin host.

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
