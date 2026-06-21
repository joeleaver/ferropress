//! Plugin **capability backends** — the host-side surfaces a sandboxed plugin can
//! reach, one trait per capability. Deny-by-default is STRUCTURAL: the plugin host
//! wires the WASM host function for a capability ONLY when a plugin's manifest
//! grants it AND the composition root injected the backing implementation. An
//! ungranted (or un-backed) capability simply has no import in the guest.
//!
//! These are **synchronous** on purpose: the host calls them from inside a
//! synchronous WASM host function, and the embedded engine is synchronous
//! underneath (the async [`RhypeStore`](crate::store::RhypeStore) port only wraps
//! it in `spawn_blocking`), so no async bridge is needed. The adapter implements
//! the sync read directly over the engine.
//!
//! Capabilities ship incrementally: [`ContentReader`] (read published content) is
//! first; content-write and plugin-settings backends are later increments.

use crate::error::Result;

/// A minimal, public summary of a published entity handed to a plugin. Carries
/// identity + display fields only — never body, never moderation/PII fields.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PublishedRef {
    /// The entity's object id.
    pub id: u64,
    /// The entity's type name (`"Post"` / `"Page"`).
    #[serde(rename = "type")]
    pub type_name: String,
    /// The entity's title (may be empty).
    pub title: String,
    /// The public slug it resolves at.
    pub slug: String,
}

/// The `content:read` capability backend: a read-only view of PUBLISHED content.
/// Backs the `fp_lookup_slug` host function the plugin host exposes to plugins
/// granted `read_store`.
///
/// Synchronous (see the module docs). Implemented by the embedded store adapter
/// over the engine; injected at the composition root.
pub trait ContentReader: Send + Sync {
    /// Resolve a PUBLISHED entity by its public slug — Post first, then Page,
    /// mirroring the public read path's permalink rule — or `None` if no published
    /// entity answers that slug. Used e.g. by a wiki plugin to decide whether a
    /// `[[link]]` target exists.
    fn lookup_published_slug(&self, slug: &str) -> Result<Option<PublishedRef>>;
}
