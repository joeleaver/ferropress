//! # ferropress-plugin-host
//!
//! Ferropress's embedded WASM plugin runtime, built on the **`extism`** host SDK.
//! Ferropress owns the resource limits and the capability surface; a plugin is a
//! sandboxed WebAssembly module with the Extism bytes-in / bytes-out ABI (so the
//! 11-language Extism PDK authors them).
//!
//! Three load-bearing mechanisms, all configured by the host — never the guest:
//!
//! 1. **Resource limits** ([`HostLimits`]) — a wall-clock `timeout` and a
//!    linear-memory page ceiling, applied to every plugin's extism [`Manifest`].
//! 2. **Capabilities are deny-by-default** ([`Capabilities`]). A plugin gets NO
//!    host access unless granted: no WASI, no network (`allowed_hosts` empty), and
//!    only the host functions a granted capability wires in. An empty capability
//!    set is a pure-compute plugin.
//! 3. **Hook bus** ([`HookBus`]) — a WordPress-style action/filter dispatcher.
//!    Actions observe an event; filters transform its JSON payload.
//!
//! The host also implements [`ferropress_render::CustomBlockRenderer`], so the one
//! shared render crate can resolve `BlockKind::Custom` blocks by calling a
//! plugin's `render_block` export — without taking any wasmtime/extism dependency
//! itself.
//!
//! `extism` is the runtime, but [`PluginHost`] is the seam: the public API here
//! (load / call / dispatch + [`Capabilities`]/[`HostLimits`]) is runtime-agnostic,
//! so the implementation could move to raw wasmtime later without touching callers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use extism::{Manifest, Plugin, PluginBuilder, UserData, ValType, Wasm, host_fn};
use serde::Deserialize;

use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::hook::{HookDispatcher, HookEvent, HookKind};
use ferropress_core::plugin_caps::{ContentReader, ContentWriter};
use ferropress_render::{CustomBlockRenderer, Html};

/// The guest export a plugin must provide to render a custom block: it receives
/// JSON `{ "name": <block name>, "data": <block payload> }` and returns HTML bytes.
const BLOCK_RENDER_EXPORT: &str = "render_block";

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

/// Per-plugin resource ceilings the host enforces (mapped onto extism's
/// [`Manifest`]). Not negotiable by the guest.
#[derive(Debug, Clone)]
pub struct HostLimits {
    /// Wall-clock budget for a single guest call; extism interrupts the plugin if
    /// it meets or exceeds this (`Manifest::with_timeout`).
    pub timeout: Duration,
    /// Maximum WebAssembly linear-memory pages (64 KiB each) the guest may grow to
    /// (`Manifest::with_memory_max`). `None` leaves it at extism's default.
    pub max_memory_pages: Option<u32>,
}

impl Default for HostLimits {
    fn default() -> Self {
        // Conservative defaults; a deployment can tighten/loosen these.
        Self {
            timeout: Duration::from_millis(1000),
            max_memory_pages: Some(256), // 256 * 64 KiB = 16 MiB
        }
    }
}

// ---------------------------------------------------------------------------
// Capabilities (deny-by-default)
// ---------------------------------------------------------------------------

/// The capability set granted to a plugin. Default = all denied (a pure-compute
/// plugin: no WASI, no network, no host functions).
///
/// v1's bundled plugins are capability-zero, so the store/settings host-function
/// wiring is intentionally not built yet — when a plugin needs `read_store` etc.,
/// the granted host functions are added to [`PluginHost::load_plugin`]'s builder
/// here (deny-by-default falls out of only-wiring-what-is-granted). `http_fetch`
/// maps to extism's `allowed_hosts`.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// May call host functions that READ from the store.
    pub read_store: bool,
    /// May call host functions that WRITE to the store.
    pub write_store: bool,
    /// May make outbound HTTP requests — only to [`Self::allowed_hosts`].
    pub http_fetch: bool,
    /// May read/write a plugin-scoped settings area.
    pub plugin_settings: bool,
    /// Hosts the plugin may reach when `http_fetch` is granted (extism
    /// `allowed_hosts`). Empty = none (deny-by-default).
    pub allowed_hosts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Hook bus
// ---------------------------------------------------------------------------

/// A registration tying a plugin export to a named hook. `priority` orders
/// execution (lower runs first, mirroring WordPress).
#[derive(Debug, Clone)]
pub struct HookRegistration {
    pub plugin_id: String,
    pub hook_name: String,
    pub kind: HookKind,
    pub priority: i32,
    /// The exported guest function to invoke for this hook.
    pub export: String,
}

/// WordPress-style action/filter dispatcher: the registration table, kept
/// priority-sorted per hook.
#[derive(Default)]
pub struct HookBus {
    registrations: HashMap<String, Vec<HookRegistration>>,
}

impl HookBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin export against a hook (keeps the per-hook list
    /// priority-sorted, lowest first).
    pub fn register(&mut self, reg: HookRegistration) {
        let list = self.registrations.entry(reg.hook_name.clone()).or_default();
        list.push(reg);
        list.sort_by_key(|r| r.priority);
    }

    /// Remove every registration belonging to a plugin (on unload).
    pub fn unregister_plugin(&mut self, plugin_id: &str) {
        for list in self.registrations.values_mut() {
            list.retain(|r| r.plugin_id != plugin_id);
        }
    }

    /// The (priority-ordered) registrations for a hook name.
    fn for_hook(&self, name: &str) -> &[HookRegistration] {
        self.registrations
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether any registration exists for a hook name. (`unregister_plugin` can
    /// leave an empty vec behind, so this checks non-emptiness, not key presence.)
    fn has_registrations(&self, name: &str) -> bool {
        self.registrations.get(name).is_some_and(|v| !v.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Declarative plugin manifest (`plugin.toml`)
// ---------------------------------------------------------------------------

/// A plugin's `plugin.toml`, read from each subdirectory of the plugins dir.
#[derive(Debug, Deserialize)]
struct PluginManifest {
    /// Stable plugin id (matches `BlockKind::Custom.plugin`).
    id: String,
    /// Wasm filename, relative to the plugin's directory.
    wasm: String,
    #[serde(default)]
    capabilities: CapabilitiesManifest,
    #[serde(default)]
    hooks: Vec<HookManifest>,
    /// Block names this plugin renders — discovery metadata for the editor; the
    /// renderer routes by plugin id + the `render_block` export, so this is not
    /// load-bearing here.
    #[serde(default)]
    #[allow(dead_code)]
    blocks: Vec<String>,
}

/// `[capabilities]` table (all default-deny).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CapabilitiesManifest {
    read_store: bool,
    write_store: bool,
    http_fetch: bool,
    plugin_settings: bool,
    allowed_hosts: Vec<String>,
}

impl From<CapabilitiesManifest> for Capabilities {
    fn from(m: CapabilitiesManifest) -> Self {
        Capabilities {
            read_store: m.read_store,
            write_store: m.write_store,
            http_fetch: m.http_fetch,
            plugin_settings: m.plugin_settings,
            allowed_hosts: m.allowed_hosts,
        }
    }
}

/// One `[[hooks]]` entry.
#[derive(Debug, Deserialize)]
struct HookManifest {
    event: String,
    /// `"action"` or `"filter"`.
    kind: String,
    export: String,
    #[serde(default)]
    priority: i32,
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// The plugin host. Owns the loaded plugins and the [`HookBus`]. One per server,
/// shared as `Arc<PluginHost>`.
///
/// Each [`Plugin`] is single-threaded, so it is held behind a `Mutex` — calls to
/// one plugin serialize, which is exactly extism's requirement, while different
/// plugins run independently.
pub struct PluginHost {
    plugins: HashMap<String, Mutex<Plugin>>,
    bus: HookBus,
    /// The `content:read` capability backend. `None` until injected via
    /// [`with_content_reader`](Self::with_content_reader); when absent, a plugin's
    /// `read_store` grant has no effect (the `fp_lookup_slug` host function is not
    /// wired, so deny-by-default holds).
    content: Option<Arc<dyn ContentReader>>,
    /// The `content:write` capability backend. `None` until injected via
    /// [`with_content_writer`](Self::with_content_writer); when absent, a plugin's
    /// `write_store` grant has no effect (the `fp_create_page_stub` / `fp_set_meta`
    /// host functions are not wired, so deny-by-default holds). Production leaves
    /// this unset until the change-feed loop guard lands (rhypedb#13).
    writer: Option<Arc<dyn ContentWriter>>,
}

impl PluginHost {
    /// An empty host (no plugins, no capability backends). Plugins are added via
    /// [`load_plugin`] / [`load_dir`]; capability backends via the `with_*` setters.
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            bus: HookBus::new(),
            content: None,
            writer: None,
        }
    }

    /// Inject the `content:read` capability backend (the embedded store's
    /// synchronous [`ContentReader`]). Plugins granted `read_store` then get the
    /// `fp_lookup_slug` host function backed by it. MUST be set before
    /// [`load_dir`](Self::load_dir) / [`load_plugin`](Self::load_plugin), since the
    /// host function is wired at plugin-build time.
    pub fn with_content_reader(mut self, content: Arc<dyn ContentReader>) -> Self {
        self.content = Some(content);
        self
    }

    /// Inject the `content:write` capability backend (the embedded store's
    /// synchronous [`ContentWriter`]). Plugins granted `write_store` then get the
    /// `fp_create_page_stub` / `fp_set_meta` host functions backed by it. MUST be
    /// set before [`load_dir`](Self::load_dir) / [`load_plugin`](Self::load_plugin).
    ///
    /// ⚠️ A write from a plugin commits and emits a `ChangeEvent`, which the
    /// action-hook bridge would re-dispatch — a `write→change→action→write` loop.
    /// Do NOT wire this in a deployment that also runs the action-hook bridge until
    /// the feed-loop guard exists (needs a write-origin token on the feed —
    /// rhypedb#13). It is safe to wire in isolation (e.g. tests) where no bridge runs.
    pub fn with_content_writer(mut self, writer: Arc<dyn ContentWriter>) -> Self {
        self.writer = Some(writer);
        self
    }

    /// Scan a plugins directory: each subdirectory with a `plugin.toml` is loaded
    /// (its wasm under the declared capabilities, its hooks registered). A missing
    /// directory is not an error (a deployment may run with no plugins). A single
    /// malformed plugin is logged and skipped so it can't stop the others.
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) -> CoreResult<()> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            tracing::info!(dir = %dir.display(), "no plugins dir; running without plugins");
            return Ok(());
        }
        let entries = std::fs::read_dir(dir).map_err(|e| {
            CoreError::Unavailable(format!("reading plugins dir {}: {e}", dir.display()))
        })?;
        for entry in entries {
            let entry = entry
                .map_err(|e| CoreError::Unavailable(format!("reading plugins dir entry: {e}")))?;
            let sub = entry.path();
            if !sub.is_dir() {
                continue;
            }
            let manifest_path = sub.join("plugin.toml");
            if !manifest_path.exists() {
                continue;
            }
            if let Err(e) = self.load_manifest(&manifest_path) {
                // One bad plugin must not abort the rest.
                tracing::error!(path = %manifest_path.display(), error = %e, "skipping plugin");
            }
        }
        Ok(())
    }

    /// Load one plugin from its `plugin.toml` path.
    fn load_manifest(&mut self, manifest_path: &Path) -> CoreResult<()> {
        let dir = manifest_path
            .parent()
            .ok_or_else(|| CoreError::Validation("plugin.toml has no parent dir".to_owned()))?;
        let text = std::fs::read_to_string(manifest_path).map_err(|e| {
            CoreError::Unavailable(format!("reading {}: {e}", manifest_path.display()))
        })?;
        let manifest: PluginManifest = toml::from_str(&text).map_err(|e| {
            CoreError::Validation(format!("parsing {}: {e}", manifest_path.display()))
        })?;

        let wasm_path = dir.join(&manifest.wasm);
        let wasm = std::fs::read(&wasm_path)
            .map_err(|e| CoreError::Unavailable(format!("reading {}: {e}", wasm_path.display())))?;

        self.load_plugin(
            &manifest.id,
            &wasm,
            manifest.capabilities.into(),
            HostLimits::default(),
        )?;

        for hook in manifest.hooks {
            let kind = parse_hook_kind(&hook.kind)?;
            self.bus.register(HookRegistration {
                plugin_id: manifest.id.clone(),
                hook_name: hook.event,
                kind,
                priority: hook.priority,
                export: hook.export,
            });
        }
        tracing::info!(id = %manifest.id, "loaded plugin");
        Ok(())
    }

    /// Load a plugin module under `id` with the given capabilities + limits.
    /// Builds an extism [`Manifest`] (timeout + memory ceiling + `allowed_hosts`)
    /// and a deny-by-default [`PluginBuilder`] (`with_wasi(false)`, host functions
    /// added only for granted capabilities).
    pub fn load_plugin(
        &mut self,
        id: &str,
        wasm_bytes: &[u8],
        capabilities: Capabilities,
        limits: HostLimits,
    ) -> CoreResult<()> {
        let mut manifest =
            Manifest::new([Wasm::data(wasm_bytes.to_vec())]).with_timeout(limits.timeout);
        if let Some(pages) = limits.max_memory_pages {
            manifest = manifest.with_memory_max(pages);
        }
        if capabilities.http_fetch {
            for host in &capabilities.allowed_hosts {
                manifest = manifest.with_allowed_host(host);
            }
        }

        // Deny-by-default: WASI off; host functions are added ONLY for granted
        // capabilities. An ungranted capability has NO import in the guest, so a
        // plugin can never call a host function it did not declare.
        let mut builder = PluginBuilder::new(manifest).with_wasi(false);

        // `content:read` (`read_store`): expose `fp_lookup_slug`, backed by the
        // injected `ContentReader`. If a plugin declares `read_store` but no backend
        // was wired, the host function is absent and the plugin fails to
        // instantiate — surfaced clearly rather than silently granting nothing.
        if capabilities.read_store {
            match &self.content {
                Some(content) => {
                    builder = builder.with_function(
                        "fp_lookup_slug",
                        [ValType::I64],
                        [ValType::I64],
                        UserData::new(Arc::clone(content)),
                        fp_lookup_slug,
                    );
                }
                None => {
                    tracing::warn!(
                        plugin = id,
                        "plugin requests `read_store` but no ContentReader is wired; \
                         `fp_lookup_slug` will be unresolved and the plugin will fail to load"
                    );
                }
            }
        }

        // `content:write` (`write_store`): expose `fp_create_page_stub` + `fp_set_meta`,
        // backed by the injected `ContentWriter`. Same deny-by-default posture as
        // read_store: if the backend is unwired, the host functions are absent and a
        // `write_store` plugin fails to instantiate (rather than silently granting
        // nothing). Production leaves the writer unwired until the feed-loop guard
        // lands (rhypedb#13), so this branch is a no-op there by design.
        if capabilities.write_store {
            match &self.writer {
                Some(writer) => {
                    builder = builder.with_function(
                        "fp_create_page_stub",
                        [ValType::I64],
                        [ValType::I64],
                        UserData::new(WriteBackend {
                            writer: Arc::clone(writer),
                            plugin_id: id.to_owned(),
                        }),
                        fp_create_page_stub,
                    );
                    builder = builder.with_function(
                        "fp_set_meta",
                        [ValType::I64],
                        [ValType::I64],
                        UserData::new(WriteBackend {
                            writer: Arc::clone(writer),
                            plugin_id: id.to_owned(),
                        }),
                        fp_set_meta,
                    );
                }
                None => {
                    tracing::warn!(
                        plugin = id,
                        "plugin requests `write_store` but no ContentWriter is wired; \
                         write host functions will be unresolved and the plugin will fail to load"
                    );
                }
            }
        }

        let plugin = builder
            .build()
            .map_err(|e| CoreError::Unavailable(format!("building plugin {id}: {e}")))?;

        self.plugins.insert(id.to_owned(), Mutex::new(plugin));
        Ok(())
    }

    /// Invoke one plugin export (Extism bytes-in/bytes-out) under its limits.
    /// Synchronous (an extism call is CPU-bound); async callers should
    /// `spawn_blocking`.
    pub fn call(&self, plugin_id: &str, export: &str, input: &[u8]) -> CoreResult<Vec<u8>> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| CoreError::Unavailable(format!("plugin {plugin_id} is not loaded")))?;
        let mut guard = plugin
            .lock()
            .map_err(|_| CoreError::Store(format!("plugin {plugin_id} mutex poisoned")))?;
        guard
            .call::<&[u8], Vec<u8>>(export, input)
            .map_err(|e| CoreError::Store(format!("plugin {plugin_id}.{export} failed: {e}")))
    }

    /// Whether a plugin id is loaded.
    pub fn has_plugin(&self, plugin_id: &str) -> bool {
        self.plugins.contains_key(plugin_id)
    }

    /// Dispatch a hook event to every registered plugin, in priority order.
    /// **Filter** hooks replace the payload with the guest's returned JSON (fed to
    /// the next filter and finally returned); **Action** hooks ignore the return.
    /// A failing hook is logged and skipped — one bad plugin never breaks the
    /// request. Synchronous; async callers should `spawn_blocking`.
    pub fn dispatch(&self, mut event: HookEvent) -> CoreResult<HookEvent> {
        let regs = self.bus.for_hook(&event.name);
        if regs.is_empty() {
            return Ok(event);
        }
        let input = serde_json::to_vec(&event.payload)
            .map_err(|e| CoreError::Store(format!("serializing hook payload: {e}")))?;
        // The payload may be transformed by filters; track the current bytes.
        let mut current = input;
        for reg in regs {
            match self.call(&reg.plugin_id, &reg.export, &current) {
                Ok(out) => {
                    if reg.kind == HookKind::Filter {
                        match serde_json::from_slice::<serde_json::Value>(&out) {
                            Ok(value) => {
                                event.payload = value;
                                current = out;
                            }
                            Err(e) => tracing::error!(
                                hook = %event.name,
                                plugin = %reg.plugin_id,
                                error = %e,
                                "filter returned invalid JSON; ignoring its output",
                            ),
                        }
                    }
                }
                Err(e) => tracing::error!(
                    hook = %event.name,
                    plugin = %reg.plugin_id,
                    error = %e,
                    "hook plugin failed; skipping",
                ),
            }
        }
        Ok(event)
    }

    /// Whether any plugin registered a hook under `name` (so a caller can skip
    /// [`dispatch`](Self::dispatch) — and the thread hop async callers need for
    /// it — when nothing listens).
    pub fn has_hooks(&self, name: &str) -> bool {
        self.bus.has_registrations(name)
    }

    /// Borrow the hook bus (register/unregister outside of manifest loading).
    pub fn bus_mut(&mut self) -> &mut HookBus {
        &mut self.bus
    }
}

/// The plugin host is the [`HookDispatcher`] port (declared in `ferropress-core`
/// so handlers can dispatch through `Arc<dyn HookDispatcher>` without depending on
/// extism). Both methods forward to the inherent ones above: in method-call
/// syntax an inherent method shadows a same-named trait method, so `self.dispatch`
/// / `self.has_hooks` resolve to `PluginHost`'s own — this delegates, it does not
/// recurse.
impl HookDispatcher for PluginHost {
    fn dispatch(&self, event: HookEvent) -> CoreResult<HookEvent> {
        self.dispatch(event)
    }

    fn has_hooks(&self, name: &str) -> bool {
        self.has_hooks(name)
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `BlockKind::Custom` blocks by calling the owning plugin's
/// `render_block` export. An absent plugin or a failing call yields `None`, so the
/// renderer falls back to its placeholder rather than erroring the whole page.
impl CustomBlockRenderer for PluginHost {
    fn render(&self, plugin: &str, name: &str, data: &serde_json::Value) -> Option<Html> {
        if !self.has_plugin(plugin) {
            return None;
        }
        let input = serde_json::to_vec(&serde_json::json!({ "name": name, "data": data }))
            .map_err(|e| tracing::error!(plugin, name, error = %e, "serializing block input"))
            .ok()?;
        match self.call(plugin, BLOCK_RENDER_EXPORT, &input) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(html) => Some(Html(html)),
                Err(e) => {
                    tracing::error!(plugin, name, error = %e, "custom block output was not UTF-8");
                    None
                }
            },
            Err(e) => {
                tracing::error!(plugin, name, error = %e, "custom block render failed");
                None
            }
        }
    }
}

// PluginHost is shared as `Arc<PluginHost>` across async tasks (it implements
// CustomBlockRenderer and is dispatched to from request handlers), so it must stay
// Send + Sync. `Mutex<Plugin>` provides that as long as extism's `Plugin: Send` —
// this assertion fails to compile if that ever stops holding (then switch to
// extism's `Pool`).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PluginHost>();
};

// The `fp_lookup_slug` host function (the `content:read` capability). The guest
// passes a slug; it returns the JSON of a [`PublishedRef`](ferropress_core::PublishedRef)
// (`{id, type, title, slug}`) when a PUBLISHED entity answers that slug, else the
// literal `null`. Backed by the [`ContentReader`] captured in `UserData`. A lookup
// error / poisoned lock degrades to `null` (the target simply reads as
// non-existent) — a capability call never aborts the guest. Wired into a plugin's
// linker only when it is granted `read_store`.
host_fn!(fp_lookup_slug(user_data: Arc<dyn ContentReader>; slug: String) -> String {
    let cell = user_data.get()?;
    let guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return Ok("null".to_owned()),
    };
    let json = match guard.lookup_published_slug(&slug) {
        Ok(Some(found)) => serde_json::to_string(&found).unwrap_or_else(|_| "null".to_owned()),
        Ok(None) => "null".to_owned(),
        Err(e) => {
            tracing::error!(error = %e, "fp_lookup_slug failed");
            "null".to_owned()
        }
    };
    Ok(json)
});

// The `content:write` capability's UserData: the injected [`ContentWriter`] plus
// the CALLING plugin's id, which the host passes as the `meta` NAMESPACE so every
// write nests under `meta[plugin_id]` — a plugin can't clobber another plugin's
// (or core) meta, and forging is structurally impossible (no string-joined key).
struct WriteBackend {
    writer: Arc<dyn ContentWriter>,
    plugin_id: String,
}

// `fp_create_page_stub` (the `content:write` capability). The guest passes a JSON
// request `{"slug","title"}`; it returns `{"id": <u64>}` for the new (or existing,
// on a slug collision) DRAFT page, or the literal `null` on any failure (a
// capability call never aborts the guest). Wired only when a plugin is granted
// `write_store` AND a `ContentWriter` backend is present.
host_fn!(fp_create_page_stub(user_data: WriteBackend; req: String) -> String {
    let cell = user_data.get()?;
    let guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return Ok("null".to_owned()),
    };
    let v: serde_json::Value = serde_json::from_str(&req).unwrap_or(serde_json::Value::Null);
    let slug = v.get("slug").and_then(|x| x.as_str()).unwrap_or("");
    let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
    let json = match guard.writer.create_page_stub(slug, title) {
        Ok(id) => serde_json::json!({ "id": id }).to_string(),
        Err(e) => {
            tracing::error!(error = %e, "fp_create_page_stub failed");
            "null".to_owned()
        }
    };
    Ok(json)
});

// `fp_set_meta` (the `content:write` capability). The guest passes a JSON request
// `{"type","id","key","value"}`; the host sets that key inside the object's `meta`
// JSON UNDER the calling plugin's namespace (`meta[plugin_id][key] = value`), so a
// plugin can only ever touch its own sub-object. Returns the JSON literal `true` on
// success, `false` on any failure/denial (malformed request, non-Post/Page type,
// engine error). Wired only under `write_store` with a backend present.
host_fn!(fp_set_meta(user_data: WriteBackend; req: String) -> String {
    let cell = user_data.get()?;
    let guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return Ok("false".to_owned()),
    };
    let v: serde_json::Value = serde_json::from_str(&req).unwrap_or(serde_json::Value::Null);
    let ty = v.get("type").and_then(|x| x.as_str());
    let id = v.get("id").and_then(|x| x.as_u64());
    let key = v.get("key").and_then(|x| x.as_str());
    let value = v.get("value").cloned();
    let (ty, id, key, value) = match (ty, id, key, value) {
        (Some(t), Some(i), Some(k), Some(val)) => (t, i, k, val),
        _ => return Ok("false".to_owned()),
    };
    // Pass the calling plugin's id as the namespace; set_meta nests the value under
    // meta[plugin_id][key], so a plugin can only ever write its own sub-object
    // (forging another plugin's / core keys is structurally impossible).
    let json = match guard.writer.set_meta(ty, id, &guard.plugin_id, key, value) {
        Ok(()) => "true".to_owned(),
        Err(e) => {
            tracing::error!(error = %e, "fp_set_meta failed");
            "false".to_owned()
        }
    };
    Ok(json)
});

/// Map a manifest hook-kind string to [`HookKind`].
fn parse_hook_kind(s: &str) -> CoreResult<HookKind> {
    match s {
        "action" => Ok(HookKind::Action),
        "filter" => Ok(HookKind::Filter),
        other => Err(CoreError::Validation(format!(
            "unknown hook kind {other:?} (expected \"action\" or \"filter\")"
        ))),
    }
}

#[cfg(test)]
mod tests;
