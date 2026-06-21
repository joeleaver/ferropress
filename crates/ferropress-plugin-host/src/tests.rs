//! Integration test: load the built `callout` reference plugin and render a block
//! through it. This is the extism-integration + ABI-conformance proof — a real
//! `extism-pdk` guest running on our [`PluginHost`]. It is gated on the wasm
//! having been built (`cargo xtask build-plugins`); if absent it skips with a
//! message, mirroring the ONNX-gated tests elsewhere.

use std::path::PathBuf;

use ferropress_core::hook::{HookEvent, HookKind};
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

/// Build a `comment.create` filter event with the given body/author (the shape the
/// island comment-create handler sends).
fn comment_event(author_name: &str, body: &str) -> HookEvent {
    HookEvent {
        name: "comment.create".to_owned(),
        kind: HookKind::Filter,
        payload: serde_json::json!({
            "slug": "hello",
            "author_name": author_name,
            "body": body,
            "status": "pending",
        }),
    }
}

/// The comment-mod plugin, loaded from `plugins/dist` (which also registers its
/// `comment.create` hook from `plugin.toml`), flags a spammy comment as `spam` and
/// leaves a clean one `pending`. This is the dispatch + filter + hook-registration
/// ABI proof — a real `extism-pdk` guest run through `PluginHost::dispatch`.
#[test]
fn comment_mod_plugin_flags_spam() {
    let dist = repo_root().join("plugins/dist");
    if !dist.join("comment-mod").exists() {
        eprintln!(
            "skipping comment_mod_plugin_flags_spam: comment-mod not built — run `cargo xtask build-plugins`"
        );
        return;
    }
    let mut host = PluginHost::new();
    host.load_dir(&dist).expect("load plugins dir");
    assert!(
        host.has_hooks("comment.create"),
        "comment-mod registered its comment.create hook"
    );

    // A keyword-matching comment is reclassified spam …
    let spam = host
        .dispatch(comment_event("Spammer", "Cheap VIAGRA, click here now!"))
        .expect("dispatch spam event");
    assert_eq!(
        spam.payload["status"], "spam",
        "spammy comment flagged: {}",
        spam.payload
    );

    // … a link-flooded one too (more than two URLs) …
    let links = host
        .dispatch(comment_event(
            "Linker",
            "see http://a.test http://b.test https://c.test",
        ))
        .expect("dispatch link-flood event");
    assert_eq!(links.payload["status"], "spam", "link flood flagged");

    // … while a genuine comment passes through untouched.
    let clean = host
        .dispatch(comment_event(
            "Jo",
            "Really enjoyed this — thanks for writing it.",
        ))
        .expect("dispatch clean event");
    assert_eq!(
        clean.payload["status"], "pending",
        "clean comment stays pending: {}",
        clean.payload
    );
    // The filter is faithful: it returns the rest of the payload unchanged.
    assert_eq!(clean.payload["author_name"], "Jo");
    assert_eq!(clean.payload["slug"], "hello");
}
