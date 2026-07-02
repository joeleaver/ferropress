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
//! Capabilities ship incrementally: [`ContentReader`] (read published content)
//! and [`ContentWriter`] (a tight, typed write surface) exist; a plugin-settings
//! backend is a later increment.

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

/// The `content:write` capability backend: a DELIBERATELY TIGHT, typed write
/// surface. Backs the `fp_create_page_stub` / `fp_set_meta` host functions the
/// plugin host exposes to plugins granted `write_store`.
///
/// Security posture: this is NOT "create any type with any fields." A plugin can
/// only (a) create a *stub Page* (a draft placeholder — never published, so it
/// can't be used to publish arbitrary content), and (b) set ONE key inside its OWN
/// namespaced sub-object of an object's `meta` JSON (never a core/indexed field
/// like `slug`/`status`, so a plugin can't corrupt the content model or escalate).
/// This keeps the blast radius of a write grant small and reviewable.
///
/// Synchronous (see the module docs): implemented by the embedded store adapter
/// directly over the engine; injected at the composition root.
///
/// FEED-LOOP NOTE: a write here commits and therefore emits a `ChangeEvent`,
/// which the action-hook bridge would otherwise re-dispatch — enabling a
/// write→change→action→write loop. Breaking that loop requires correlating a
/// write with its own `ChangeEvent`, which needs a token the engine does not yet
/// surface at write time (see rhypedb#13). Until that lands, the composition root
/// does NOT wire a `ContentWriter` in production (deny-by-default: an ungranted /
/// un-backed `write_store` plugin fails to instantiate), so this surface is
/// exercised only in isolation.
pub trait ContentWriter: Send + Sync {
    /// Create a **draft** stub `Page` at `slug` with `title` and an empty body,
    /// returning its new object id. Used e.g. to auto-create a placeholder for a
    /// red `[[wiki link]]` target so an author can fill it in later. A draft is
    /// deliberate: the stub is not publicly served until a human publishes it.
    /// If a page already occupies `slug`, the implementation may return the
    /// existing id rather than create a duplicate.
    fn create_page_stub(&self, slug: &str, title: &str) -> Result<u64>;

    /// Set a single `key` inside object `(type_name, id)`'s `meta` JSON, scoped to
    /// the `namespace` sub-object — i.e. `meta[namespace][key] = value`
    /// (read-modify-write on the `meta` field only; every other field is untouched).
    /// The host passes the CALLING PLUGIN's id as `namespace`, and the value is
    /// nested one level UNDER it (not string-joined), so a plugin can only ever
    /// mutate its own sub-object: cross-plugin (or core) meta is structurally
    /// unforgeable regardless of what characters appear in the id or key. Used e.g.
    /// by a backlink indexer to record "pages that link here" on a target page.
    fn set_meta(
        &self,
        type_name: &str,
        id: u64,
        namespace: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<()>;
}
