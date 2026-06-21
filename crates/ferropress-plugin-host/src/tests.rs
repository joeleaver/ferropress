//! Integration test: load the built `callout` reference plugin and render a block
//! through it. This is the extism-integration + ABI-conformance proof — a real
//! `extism-pdk` guest running on our [`PluginHost`]. It is gated on the wasm
//! having been built (`cargo xtask build-plugins`); if absent it skips with a
//! message, mirroring the ONNX-gated tests elsewhere.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::hook::{HookEvent, HookKind};
use ferropress_core::plugin_caps::{ContentReader, PublishedRef};
use ferropress_render::CustomBlockRenderer;

use crate::{Capabilities, HookRegistration, HostLimits, PluginHost};

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

/// `load_dir` registers the audit-log plugin's ACTION hook (`comment.created`)
/// from its `plugin.toml` — the action-side counterpart to the comment-mod filter.
#[test]
fn load_dir_registers_audit_log_action() {
    let dist = repo_root().join("plugins/dist");
    if !dist.join("audit-log").exists() {
        eprintln!(
            "skipping load_dir_registers_audit_log_action: audit-log not built — run `cargo xtask build-plugins`"
        );
        return;
    }
    let mut host = PluginHost::new();
    host.load_dir(&dist).expect("load plugins dir");
    assert!(host.has_plugin("audit-log"));
    assert!(
        host.has_hooks("comment.created"),
        "audit-log registered its comment.created action hook"
    );
}

/// An ACTION hook must IGNORE the guest's returned payload (only filters
/// transform). Proven with real wasm by registering comment-mod's
/// spam-classifying export under an ACTION hook: as a filter the same export sets
/// `status:"spam"`, but dispatched as an action the event payload comes back
/// UNCHANGED.
#[test]
fn action_dispatch_ignores_guest_return() {
    let dist = repo_root().join("plugins/dist");
    if !dist.join("comment-mod").exists() {
        eprintln!(
            "skipping action_dispatch_ignores_guest_return: comment-mod not built — run `cargo xtask build-plugins`"
        );
        return;
    }
    let mut host = PluginHost::new();
    host.load_dir(&dist).expect("load plugins dir");
    host.bus_mut().register(HookRegistration {
        plugin_id: "comment-mod".to_owned(),
        hook_name: "comment.created".to_owned(),
        kind: HookKind::Action,
        priority: 0,
        export: "comment_create".to_owned(),
    });

    let out = host
        .dispatch(HookEvent {
            name: "comment.created".to_owned(),
            kind: HookKind::Action,
            payload: serde_json::json!({
                "author_name": "x",
                "body": "buy viagra now",
                "status": "pending",
            }),
        })
        .expect("dispatch action");
    assert_eq!(
        out.payload["status"], "pending",
        "an action must not mutate the event payload: {}",
        out.payload
    );
}

/// A [`ContentReader`] double: only the given slugs "exist" (as published Posts).
struct StubReader {
    existing: HashSet<String>,
}

impl ContentReader for StubReader {
    fn lookup_published_slug(&self, slug: &str) -> CoreResult<Option<PublishedRef>> {
        if self.existing.contains(slug) {
            Ok(Some(PublishedRef {
                id: 1,
                type_name: "Post".to_owned(),
                title: format!("Title of {slug}"),
                slug: slug.to_owned(),
            }))
        } else {
            Ok(None)
        }
    }
}

/// Read the built wiki plugin wasm, or `None` if not built yet.
fn wiki_wasm() -> Option<Vec<u8>> {
    std::fs::read(repo_root().join("plugins/dist/wiki/ferropress_plugin_wiki.wasm")).ok()
}

/// The wiki plugin (granted `content:read`) resolves `[[links]]` through the
/// `fp_lookup_slug` host function: an existing target renders a normal link with
/// the page title as a tooltip; a missing target renders a "red link". This is the
/// end-to-end proof that a capability host function works with a real wasm guest.
#[test]
fn wiki_plugin_resolves_links_via_capability() {
    let Some(wasm) = wiki_wasm() else {
        eprintln!(
            "skipping wiki_plugin_resolves_links_via_capability: wiki wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    let reader = Arc::new(StubReader {
        existing: ["hello-world".to_owned()].into_iter().collect(),
    });
    let mut host = PluginHost::new().with_content_reader(reader);
    host.load_plugin(
        "wiki",
        &wasm,
        Capabilities {
            read_store: true,
            ..Default::default()
        },
        HostLimits::default(),
    )
    .expect("load wiki plugin");

    let html = host
        .render(
            "wiki",
            "wiki",
            &serde_json::json!({ "text": "See [[Hello World]] and [[No Such Page]]." }),
        )
        .expect("wiki renders Some(html)")
        .into_string();

    // The existing target: a normal link, with the real page title as the tooltip.
    assert!(
        html.contains(
            "<a href=\"/hello-world\" class=\"wiki-link\" title=\"Title of hello-world\">Hello World</a>"
        ),
        "existing wiki link resolved via the capability: {html}"
    );
    // The missing target: a red link.
    assert!(
        html.contains(
            "<a href=\"/no-such-page\" class=\"wiki-link wiki-link-new\" title=\"Page does not exist\">No Such Page</a>"
        ),
        "missing wiki link rendered as a red link: {html}"
    );
}

/// Deny-by-default is STRUCTURAL: a plugin that declares `read_store` but is loaded
/// with NO `ContentReader` wired has no `fp_lookup_slug` import, so it fails to
/// instantiate rather than silently running without the capability.
#[test]
fn wiki_plugin_without_capability_backend_fails_to_load() {
    let Some(wasm) = wiki_wasm() else {
        eprintln!(
            "skipping wiki_plugin_without_capability_backend_fails_to_load: wiki wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    // No `with_content_reader`, so the host function is never wired.
    let mut host = PluginHost::new();
    let err = host
        .load_plugin(
            "wiki",
            &wasm,
            Capabilities {
                read_store: true,
                ..Default::default()
            },
            HostLimits::default(),
        )
        .expect_err("a read_store plugin must fail to load when no ContentReader backs it");

    // Pin the CAUSE so this proves the *structural* deny, not some unrelated load
    // error: the failure is the unresolved `fp_lookup_slug` host import. The sibling
    // test loads the SAME wasm bytes successfully once a ContentReader is wired, so
    // together they prove the capability — not the wasm — gates loading.
    assert!(
        matches!(err, CoreError::Unavailable(_)),
        "expected an instantiation failure, got: {err}"
    );
    assert!(
        err.to_string().contains("fp_lookup_slug"),
        "the failure must be the unresolved fp_lookup_slug host import: {err}"
    );
}
