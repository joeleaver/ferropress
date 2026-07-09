//! The plugin **config-surface registry** — a port letting the admin enumerate
//! loaded plugins and fetch a plugin's settings [`FormSchema`] without depending on
//! the extism-backed plugin host.
//!
//! This mirrors `ferropress_render::CustomBlockRenderer` (the block-render port) and
//! `ferropress_core::hook::HookDispatcher` (the hook port): the plugin host is the
//! only crate that touches extism, so the HTTP layer talks to it exclusively through
//! trait objects declared in a rinch-free workspace crate. The port lives HERE (not
//! in `ferropress-core`) because its payload is a [`FormSchema`], which is defined in
//! this crate — `ferropress-core` cannot depend on `ferropress-render-form` (that
//! crate depends on core). `ferropress-http` already depends on this crate for the
//! settings schema, so it consumes the port from here.

use serde::{Deserialize, Serialize};

use crate::FormSchema;

/// A loaded plugin as surfaced to the admin's plugin list: its stable id, a display
/// name, and whether it ships a settings form (so the UI shows a "Configure" affordance
/// only when there is something to configure).
///
/// `Serialize` for the admin API (server → client); `Deserialize` so the wasm admin
/// parses the same type off the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    /// Stable plugin id (matches the `plugin.toml` `id` and `BlockKind::Custom.plugin`).
    pub id: String,
    /// Human-facing name (the manifest `name`, falling back to `id`).
    pub name: String,
    /// Whether the plugin declares a `[settings]` form schema.
    pub has_settings: bool,
}

/// The read-only registry of loaded plugins the admin's plugin-config surface needs.
/// A PORT so `ferropress-http` can list plugins and fetch a plugin's settings schema
/// without depending on the plugin host (which pulls in extism). The plugin host
/// implements it over its loaded manifests; tests use a lightweight double.
pub trait PluginCatalog: Send + Sync {
    /// Every loaded plugin, for the admin's plugin list. Order is unspecified; the
    /// admin sorts for display.
    fn plugins(&self) -> Vec<PluginDescriptor>;

    /// The declared settings [`FormSchema`] (BARE field keys) for a plugin id, or
    /// `None` if the id is unknown or the plugin ships no settings form. The admin
    /// route uses this both to render the form and to whitelist a submission — an
    /// unknown id therefore 404s before any write.
    fn settings_schema(&self, id: &str) -> Option<FormSchema>;
}

/// The default catalog: no plugins are configurable. Used by `AppState` until the
/// composition root injects the real plugin host (mirrors `NoCustomBlocks` /
/// `NoHooks`), so a plugin-free or test deployment answers the plugin routes with an
/// empty list / 404 rather than needing the host wired.
pub struct NoPlugins;

impl PluginCatalog for NoPlugins {
    fn plugins(&self) -> Vec<PluginDescriptor> {
        Vec::new()
    }

    fn settings_schema(&self, _id: &str) -> Option<FormSchema> {
        None
    }
}
