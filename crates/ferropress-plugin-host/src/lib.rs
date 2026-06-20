//! # ferropress-plugin-host
//!
//! Embedded **wasmtime** plugin runtime. Ferropress OWNS the resource limits and
//! the capability surface; a plugin is a sandboxed WebAssembly module with an
//! Extism-shaped bytes-in / bytes-out ABI.
//!
//! Three load-bearing mechanisms (all wasmtime-native, all configured by the
//! host — never by the guest):
//!
//! 1. **Resource limits.** [`HostLimits`] are enforced via
//!    `Engine` `epoch_interruption` (a wall-clock deadline that interrupts a
//!    runaway guest), `StoreLimits` (linear-memory / table / instance ceilings
//!    via `Store::limiter`), and optional `fuel` (deterministic step budget).
//! 2. **Capabilities are STRUCTURAL** ([`Capabilities`]). A guest can only call
//!    host functions the [`Linker`] actually defines for it. Deny-by-default: an
//!    empty capability set wires up NO host imports, so the plugin is pure
//!    compute. There is no ambient authority, no `WASI` filesystem/network unless
//!    a capability explicitly adds it.
//! 3. **Hook bus** ([`HookBus`]). A WordPress-style action/filter dispatcher.
//!    Actions observe an event; filters may transform its JSON payload. The store
//!    change feed (`RhypeStore::subscribe`) is bridged into hook events so
//!    plugins can react to content changes.
//!
//! STATUS: stub scaffold — real types + signatures, `todo!()` bodies. The whole
//! crate compiles on host so the composition root can name these types now.

#![allow(dead_code)]

use std::collections::HashMap;

use ferropress_core::error::Result as CoreResult;
use ferropress_core::hook::{HookEvent, HookKind};

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

/// Per-plugin resource ceilings the host enforces. None of these are negotiable
/// by the guest.
#[derive(Debug, Clone)]
pub struct HostLimits {
    /// Number of epoch ticks a single guest call may run before it is
    /// interrupted. The host increments the engine epoch on a timer; this is the
    /// wall-clock deadline guard against an infinite loop.
    pub epoch_deadline_ticks: u64,
    /// Maximum linear memory (bytes) the guest may grow to (enforced via
    /// `StoreLimits`).
    pub max_memory_bytes: usize,
    /// Maximum number of WebAssembly tables / table elements (enforced via
    /// `StoreLimits`).
    pub max_table_elements: usize,
    /// Optional deterministic fuel budget per call. `None` = rely on epoch
    /// interruption only (fuel adds overhead; use it when determinism matters).
    pub fuel: Option<u64>,
}

impl Default for HostLimits {
    fn default() -> Self {
        // Conservative defaults; the server config can tighten/loosen these.
        Self {
            epoch_deadline_ticks: 1,
            max_memory_bytes: 64 * 1024 * 1024, // 64 MiB
            max_table_elements: 10_000,
            fuel: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Capabilities (structural, deny-by-default)
// ---------------------------------------------------------------------------

/// The structural capability set granted to a plugin. Each `true` flag tells the
/// host to wire the corresponding group of host functions into that plugin's
/// [`Linker`]. The default (`Default::default()` -> all `false`) is a pure
/// compute plugin with no host access whatsoever.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// May call host functions that READ from the `RhypeStore` port.
    pub read_store: bool,
    /// May call host functions that WRITE to the `RhypeStore` port.
    pub write_store: bool,
    /// May call the host's outbound HTTP fetch shim.
    pub http_fetch: bool,
    /// May read/write a scoped key/value area in `Setting` (namespaced to the
    /// plugin id).
    pub plugin_settings: bool,
}

// ---------------------------------------------------------------------------
// Hook bus
// ---------------------------------------------------------------------------

/// A registration tying a plugin to a named hook. `priority` orders execution
/// (lower runs first, mirroring WordPress).
#[derive(Debug, Clone)]
pub struct HookRegistration {
    pub plugin_id: String,
    pub hook_name: String,
    pub kind: HookKind,
    pub priority: i32,
    /// The exported guest function name to invoke for this hook.
    pub export: String,
}

/// WordPress-style action/filter dispatcher. Holds the registration table and,
/// at dispatch time, invokes each matching plugin export under its
/// [`HostLimits`] + [`Capabilities`].
#[derive(Default)]
pub struct HookBus {
    /// hook_name -> ordered registrations.
    registrations: HashMap<String, Vec<HookRegistration>>,
}

impl HookBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin export against a hook. (Re-sorts that hook's list by
    /// priority.)
    pub fn register(&mut self, _reg: HookRegistration) {
        // TODO: push into registrations[hook_name], then sort_by_key(priority).
        todo!("insert registration and keep the per-hook list priority-sorted")
    }

    /// Remove all registrations for a plugin (on unload).
    pub fn unregister_plugin(&mut self, _plugin_id: &str) {
        // TODO: retain registrations whose plugin_id != plugin_id across all hooks.
        todo!("drop every registration belonging to this plugin id")
    }
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// The plugin host. Owns the shared wasmtime `Engine` (with epoch interruption
/// enabled), the loaded plugin instances, and the [`HookBus`]. One per server.
pub struct PluginHost {
    // engine: wasmtime::Engine,                  // epoch_interruption = true
    // plugins: HashMap<String, LoadedPlugin>,    // module + per-plugin Capabilities
    // bus: HookBus,
    // epoch_ticker: tokio::task::JoinHandle<()>, // increments engine.increment_epoch()
    bus: HookBus,
}

/// Per-plugin runtime state held by the host. A `wasmtime::Store` carries the
/// `StoreLimits` + the per-call epoch deadline; the `Linker` carries exactly the
/// host functions this plugin's [`Capabilities`] allow.
struct LoadedPlugin {
    // module: wasmtime::Module,
    // linker: wasmtime::Linker<HostCtx>,
    // capabilities: Capabilities,
    // limits: HostLimits,
    capabilities: Capabilities,
    limits: HostLimits,
}

/// Host-side store data threaded through every guest call. Carries the
/// `StoreLimits` (so `Store::limiter` can borrow it) plus handles to the host
/// services a capability may expose.
struct HostCtx {
    // limits: wasmtime::StoreLimits,
    // capabilities: Capabilities,
    // (store/http/setting handles wired in per capability)
}

impl PluginHost {
    /// Build a host: an `Engine` configured with `epoch_interruption`, plus the
    /// background ticker that advances the epoch on a timer.
    pub fn new() -> Self {
        // TODO:
        //   let mut cfg = wasmtime::Config::new();
        //   cfg.epoch_interruption(true);
        //   // cfg.consume_fuel(true) when any plugin uses a fuel budget.
        //   let engine = wasmtime::Engine::new(&cfg)?;
        //   spawn a tokio task that loops: sleep(tick); engine.increment_epoch();
        todo!("build wasmtime::Engine with epoch_interruption + start the epoch ticker")
    }

    /// Load a plugin module's bytes under `plugin_id`, with the given capability
    /// set and limits. Builds a deny-by-default [`Linker`] and adds ONLY the host
    /// functions the capabilities allow.
    pub fn load_plugin(
        &mut self,
        _plugin_id: &str,
        _wasm_bytes: &[u8],
        _capabilities: Capabilities,
        _limits: HostLimits,
    ) -> CoreResult<()> {
        // TODO:
        //   let module = Module::new(&engine, wasm_bytes)?;
        //   let mut linker = Linker::new(&engine);
        //   if capabilities.read_store  { wire read_store host fns }
        //   if capabilities.write_store { wire write_store host fns }
        //   if capabilities.http_fetch  { wire http host fns }
        //   if capabilities.plugin_settings { wire setting host fns }
        //   store the LoadedPlugin.
        todo!("compile module + build a deny-by-default Linker per Capabilities")
    }

    /// Invoke one plugin export (the Extism-shaped bytes-in/bytes-out ABI) under
    /// the plugin's limits. Sets the per-call epoch deadline + StoreLimits on a
    /// fresh `Store`, writes `input` into guest memory, calls the export, and
    /// reads back the output bytes.
    pub async fn call(
        &self,
        _plugin_id: &str,
        _export: &str,
        _input: &[u8],
    ) -> CoreResult<Vec<u8>> {
        // TODO:
        //   let mut store = Store::new(&engine, HostCtx{ limits: StoreLimitsBuilder...});
        //   store.limiter(|ctx| &mut ctx.limits);
        //   store.set_epoch_deadline(limits.epoch_deadline_ticks);
        //   if let Some(f) = limits.fuel { store.set_fuel(f)?; }
        //   instantiate via the plugin's Linker, run the export on a blocking-safe
        //   path, map a Trap (epoch/fuel/oom) -> CoreError::Unavailable.
        todo!("instantiate under limits, run the bytes-in/bytes-out export")
    }

    /// Dispatch a hook event to every subscribed plugin. Actions observe the
    /// payload (return value ignored); filters may transform it — the transformed
    /// payload feeds the next filter and is finally returned.
    pub async fn dispatch(&self, _event: HookEvent) -> CoreResult<HookEvent> {
        let _ = &self.bus;
        // TODO: look up self.bus.registrations[event.name]; for each, serialize the
        // payload, self.call(plugin, export, bytes), and for Filter hooks replace
        // event.payload with the (deserialized) result. Return the final event.
        todo!("route the hook through its registered plugins under limits + caps")
    }

    /// Borrow the hook bus to register/unregister hooks.
    pub fn bus_mut(&mut self) -> &mut HookBus {
        &mut self.bus
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}
