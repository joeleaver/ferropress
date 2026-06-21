//! Integration test: load the built `callout` reference plugin and render a block
//! through it. This is the extism-integration + ABI-conformance proof — a real
//! `extism-pdk` guest running on our [`PluginHost`]. It is gated on the wasm
//! having been built (`cargo xtask build-plugins`); if absent it skips with a
//! message, mirroring the ONNX-gated tests elsewhere.

use std::path::PathBuf;

use ferropress_render::CustomBlockRenderer;

use crate::{Capabilities, HostLimits, PluginHost};

/// Repo root (the plugin-host crate is `crates/ferropress-plugin-host`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels under the repo root")
        .to_path_buf()
}

#[test]
fn callout_plugin_renders_block() {
    let wasm_path = repo_root().join("plugins/dist/callout/ferropress_plugin_callout.wasm");
    let Ok(wasm) = std::fs::read(&wasm_path) else {
        eprintln!(
            "skipping callout_plugin_renders_block: {} not built — run `cargo xtask build-plugins`",
            wasm_path.display()
        );
        return;
    };

    let mut host = PluginHost::new();
    host.load_plugin(
        "callout",
        &wasm,
        Capabilities::default(),
        HostLimits::default(),
    )
    .expect("load callout plugin");

    // Render via the same seam the renderer uses. The plugin sanitizes the variant
    // (-> `warning`) and HTML-escapes the body text.
    let html = host
        .render(
            "callout",
            "callout",
            &serde_json::json!({ "variant": "Warning!", "text": "Be careful <b>x</b>" }),
        )
        .expect("callout renders Some(html)")
        .into_string();

    assert!(
        html.contains("fp-callout-warning"),
        "variant sanitized + applied: {html}"
    );
    assert!(
        html.contains("Be careful &lt;b&gt;x&lt;/b&gt;"),
        "body text HTML-escaped: {html}"
    );
    assert!(
        !html.contains("<b>x</b>"),
        "raw markup must not survive: {html}"
    );

    // An unknown plugin id resolves to None (renderer falls back to placeholder).
    assert!(
        host.render("nope", "callout", &serde_json::Value::Null)
            .is_none()
    );
}

#[test]
fn load_dir_loads_callout() {
    let dist = repo_root().join("plugins/dist");
    if !dist.join("callout").exists() {
        eprintln!(
            "skipping load_dir_loads_callout: plugins not built — run `cargo xtask build-plugins`"
        );
        return;
    }
    let mut host = PluginHost::new();
    host.load_dir(&dist).expect("load plugins dir");
    assert!(
        host.has_plugin("callout"),
        "callout loaded from plugins/dist"
    );
}
