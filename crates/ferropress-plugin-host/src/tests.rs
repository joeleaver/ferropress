//! Integration test: load the built `callout` reference plugin and render a block
//! through it. This is the extism-integration + ABI-conformance proof — a real
//! `extism-pdk` guest running on our [`PluginHost`]. It is gated on the wasm
//! having been built (`cargo xtask build-plugins`); if absent it skips with a
//! message, mirroring the ONNX-gated tests elsewhere.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::hook::{HookEvent, HookKind};
use ferropress_core::plugin_caps::{
    ContentReader, ContentWriter, PluginSettingsReader, PublishedRef,
};
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

    // Callout now imports `fp_get_setting` (the `plugin_settings` capability), so the
    // host function must be wired for it to instantiate. This block sets its own
    // variant, so the configured default is never consulted (a null reader suffices).
    let mut host = PluginHost::new().with_plugin_settings(Arc::new(NullSettings));
    host.load_plugin(
        "callout",
        &wasm,
        Capabilities {
            plugin_settings: true,
            ..Default::default()
        },
        HostLimits::default(),
        None,
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
    // Callout grants `plugin_settings`, so the settings backend must be wired for it
    // to instantiate (deny-by-default is structural; `load_dir` would otherwise skip it).
    let mut host = PluginHost::new().with_plugin_settings(Arc::new(NullSettings));
    host.load_dir(&dist).expect("load plugins dir");
    assert!(
        host.has_plugin("callout"),
        "callout loaded from plugins/dist"
    );
    // And its config schema is surfaced through the catalog port.
    use ferropress_render_form::PluginCatalog;
    let listed = host.plugins();
    let callout = listed
        .iter()
        .find(|p| p.id == "callout")
        .expect("callout in catalog");
    assert_eq!(callout.name, "Callout");
    assert!(callout.has_settings, "callout ships a settings form");
    assert!(host.settings_schema("callout").is_some());
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
        None,
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
            None,
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

/// A [`ContentWriter`] double that RECORDS every `set_meta` call (the host has
/// already namespaced the key under the calling plugin's id by the time it lands
/// here), so a test can assert exactly what a plugin wrote.
/// A recorded `set_meta` call: `(type_name, id, namespace, key, value)`.
type SetMetaCall = (String, u64, String, String, serde_json::Value);

struct StubWriter {
    set_meta_calls: Mutex<Vec<SetMetaCall>>,
}

impl ContentWriter for StubWriter {
    fn create_page_stub(&self, _slug: &str, _title: &str) -> CoreResult<u64> {
        Ok(999)
    }
    fn set_meta(
        &self,
        type_name: &str,
        id: u64,
        namespace: &str,
        key: &str,
        value: serde_json::Value,
    ) -> CoreResult<()> {
        self.set_meta_calls.lock().unwrap().push((
            type_name.to_owned(),
            id,
            namespace.to_owned(),
            key.to_owned(),
            value,
        ));
        Ok(())
    }
}

/// Read the built backlink-index plugin wasm, or `None` if not built yet.
fn backlink_wasm() -> Option<Vec<u8>> {
    std::fs::read(
        repo_root().join("plugins/dist/backlink-index/ferropress_plugin_backlink_index.wasm"),
    )
    .ok()
}

/// A post.updated payload whose body links to `[[Target Page]]` (published) and
/// `[[Missing]]` (not) — the shape the change-feed → action bridge delivers.
fn post_update_event(source_id: u64, source_slug: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "type": "Post",
        "kind": "update",
        "object_id": source_id,
        "fields": {
            "slug": source_slug,
            "block_tree": {
                "schema_version": 1,
                "blocks": [{
                    "uid": "b1",
                    "kind": {
                        "type": "custom",
                        "plugin": "wiki",
                        "name": "wiki",
                        "data": { "text": "See [[Target Page]] and [[Missing]]." }
                    },
                    "children": []
                }]
            }
        }
    }))
    .expect("serialize payload")
}

/// The backlink-index plugin (granted `content:read` + `content:write`) parses a
/// post's `wiki` [[targets]], resolves each via `fp_lookup_slug`, and records a
/// backlink on each RESOLVABLE target via `fp_set_meta`. This is the end-to-end
/// proof that the content:write host function works with a real wasm guest — the
/// host namespaces the meta key under the plugin id, and only published targets
/// get a backlink. (The action-via-bridge loop guard is separate — rhypedb#13.)
#[test]
fn backlink_index_writes_backlink_via_capabilities() {
    let Some(wasm) = backlink_wasm() else {
        eprintln!(
            "skipping backlink_index_writes_backlink_via_capabilities: backlink-index wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    // Only "target-page" is published; "missing" resolves to null.
    let reader = Arc::new(StubReader {
        existing: ["target-page".to_owned()].into_iter().collect(),
    });
    let writer = Arc::new(StubWriter {
        set_meta_calls: Mutex::new(Vec::new()),
    });
    let mut host = PluginHost::new()
        .with_content_reader(reader)
        .with_content_writer(writer.clone());
    host.load_plugin(
        "backlink-index",
        &wasm,
        Capabilities {
            read_store: true,
            write_store: true,
            ..Default::default()
        },
        HostLimits::default(),
        None,
    )
    .expect("load backlink-index plugin");

    host.call(
        "backlink-index",
        "on_change",
        &post_update_event(42, "source-post"),
    )
    .expect("dispatch on_change");

    let calls = writer.set_meta_calls.lock().unwrap();
    // EXACTLY one backlink — only the published target, never the red link.
    assert_eq!(
        calls.len(),
        1,
        "only the resolvable target gets a backlink: {calls:?}"
    );
    let (type_name, id, namespace, key, value) = &calls[0];
    // StubReader resolves any existing slug to a Post with id 1.
    assert_eq!(type_name, "Post");
    assert_eq!(*id, 1);
    // The host passed the calling plugin's id as the namespace (meta[namespace][key]),
    // NOT a string-joined key — so isolation can't be forged.
    assert_eq!(
        namespace, "backlink-index",
        "meta write must be namespaced under the calling plugin id: {namespace}"
    );
    assert_eq!(
        key, "from:42",
        "the plugin's own key is nested, not joined: {key}"
    );
    assert_eq!(value, &serde_json::json!("source-post"));
}

/// Deny-by-default is STRUCTURAL for `write_store` exactly as for `read_store`: a
/// plugin that declares `write_store` but is loaded with NO `ContentWriter` wired
/// has no `fp_set_meta` import, so it fails to instantiate. A `ContentReader` IS
/// wired here (so `fp_lookup_slug` resolves), pinning the failure to the missing
/// WRITE backend specifically.
#[test]
fn backlink_index_without_writer_backend_fails_to_load() {
    let Some(wasm) = backlink_wasm() else {
        eprintln!(
            "skipping backlink_index_without_writer_backend_fails_to_load: backlink-index wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    // Reader wired, writer NOT — so only the write host functions are unresolved.
    let reader = Arc::new(StubReader {
        existing: HashSet::new(),
    });
    let mut host = PluginHost::new().with_content_reader(reader);
    let err = host
        .load_plugin(
            "backlink-index",
            &wasm,
            Capabilities {
                read_store: true,
                write_store: true,
                ..Default::default()
            },
            HostLimits::default(),
            None,
        )
        .expect_err("a write_store plugin must fail to load when no ContentWriter backs it");

    assert!(
        matches!(err, CoreError::Unavailable(_)),
        "expected an instantiation failure, got: {err}"
    );
    assert!(
        err.to_string().contains("fp_set_meta"),
        "the failure must be the unresolved fp_set_meta host import: {err}"
    );
}

// ---------------------------------------------------------------------------
// plugin_settings capability + config-schema catalog
// ---------------------------------------------------------------------------

/// A [`PluginSettingsReader`] double that never has a stored value (so the host's
/// schema-default overlay is what the guest sees).
struct NullSettings;
impl PluginSettingsReader for NullSettings {
    fn get_setting(&self, _namespace: &str, _key: &str) -> CoreResult<Option<serde_json::Value>> {
        Ok(None)
    }
}

/// A [`PluginSettingsReader`] double returning one canned value for a single
/// (namespace, key) — proves a stored value overrides the schema default and that a
/// plugin only reads its OWN namespace.
struct StubSettings {
    namespace: String,
    key: String,
    value: serde_json::Value,
}
impl PluginSettingsReader for StubSettings {
    fn get_setting(&self, namespace: &str, key: &str) -> CoreResult<Option<serde_json::Value>> {
        if namespace == self.namespace && key == self.key {
            Ok(Some(self.value.clone()))
        } else {
            Ok(None)
        }
    }
}

/// The built callout plugin wasm, or `None` if it has not been built yet.
fn callout_wasm() -> Option<Vec<u8>> {
    let path = repo_root().join("plugins/dist/callout/ferropress_plugin_callout.wasm");
    std::fs::read(path).ok()
}

/// A plugin's inline `[settings]` TOML parses into a `FormSchema` via the
/// TOML → JSON → schema bridge (the mechanism that lets a plugin ship a config form
/// the admin renders with the very same `FormSchemaRenderer`).
#[test]
fn settings_toml_parses_into_a_form_schema() {
    let toml_src = r#"
        [[sections]]
        id = "appearance"
        title = "Appearance"

        [[sections.fields]]
        key = "default_variant"
        label = "Default variant"
        default = "note"

        [sections.fields.widget]
        type = "select"

        [[sections.fields.widget.options]]
        value = "note"
        label = "Note"

        [[sections.fields.widget.options]]
        value = "warning"
        label = "Warning"
    "#;
    let raw: toml::Value = toml::from_str(toml_src).expect("parse toml");
    let schema = crate::parse_settings_schema(raw).expect("valid schema");
    assert_eq!(
        schema.defaults()["default_variant"],
        serde_json::json!("note")
    );
    let field = schema.field("default_variant").expect("field present");
    assert!(matches!(
        field.widget,
        ferropress_render_form::WidgetKind::Select { .. }
    ));
}

/// An empty or key-duplicating settings form is rejected at parse (the field key is
/// the persisted `Setting` sub-key, so duplicates/blanks are a config error).
#[test]
fn settings_schema_rejects_empty_and_duplicate_keys() {
    // No sections at all → no fields → rejected.
    let empty: toml::Value = toml::from_str("").expect("parse");
    assert!(crate::parse_settings_schema(empty).is_err());

    // The SAME field key in two sections → rejected (the key is the persisted sub-key).
    let dup: toml::Value = toml::from_str(
        r#"
        [[sections]]
        id = "s"
        title = "S"
        [[sections.fields]]
        key = "k"
        label = "A"
        default = ""
        [sections.fields.widget]
        type = "text_area"
        [[sections]]
        id = "s2"
        title = "S2"
        [[sections.fields]]
        key = "k"
        label = "B"
        default = ""
        [sections.fields.widget]
        type = "text_area"
    "#,
    )
    .expect("parse dup");
    assert!(
        crate::parse_settings_schema(dup).is_err(),
        "a duplicate field key across sections must be rejected"
    );
}

/// The catalog lists a loaded plugin's config schema (via the `PluginCatalog` port),
/// and `fp_get_setting` returns the effective value: the schema DEFAULT when unset,
/// the STORED value when set. Proven end-to-end through the real callout guest.
#[test]
fn callout_reads_configured_default_variant() {
    let Some(wasm) = callout_wasm() else {
        eprintln!(
            "skipping callout_reads_configured_default_variant: callout wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    // Build the callout config schema the way the manifest declares it (default = note).
    let schema_toml: toml::Value = toml::from_str(
        r#"
        [[sections]]
        id = "appearance"
        title = "Appearance"
        [[sections.fields]]
        key = "default_variant"
        label = "Default variant"
        default = "note"
        [sections.fields.widget]
        type = "select"
        [[sections.fields.widget.options]]
        value = "note"
        label = "Note"
        [[sections.fields.widget.options]]
        value = "warning"
        label = "Warning"
    "#,
    )
    .expect("parse");
    let schema = crate::parse_settings_schema(schema_toml).expect("schema");

    // (1) No stored value → the guest sees the schema DEFAULT ("note"). A block with
    // an EMPTY variant triggers the configured-default read.
    let mut host = PluginHost::new().with_plugin_settings(Arc::new(NullSettings));
    host.load_plugin(
        "callout",
        &wasm,
        Capabilities {
            plugin_settings: true,
            ..Default::default()
        },
        HostLimits::default(),
        Some(&schema),
    )
    .expect("load callout");
    let html = host
        .render("callout", "callout", &serde_json::json!({ "text": "hi" }))
        .expect("renders")
        .into_string();
    assert!(
        html.contains("fp-callout-note"),
        "unset → schema default variant: {html}"
    );

    // (2) A STORED value overrides the default: the same empty-variant block now
    // renders the configured "warning".
    let mut host = PluginHost::new().with_plugin_settings(Arc::new(StubSettings {
        namespace: "callout".to_owned(),
        key: "default_variant".to_owned(),
        value: serde_json::json!("warning"),
    }));
    host.load_plugin(
        "callout",
        &wasm,
        Capabilities {
            plugin_settings: true,
            ..Default::default()
        },
        HostLimits::default(),
        Some(&schema),
    )
    .expect("load callout");
    let html = host
        .render("callout", "callout", &serde_json::json!({ "text": "hi" }))
        .expect("renders")
        .into_string();
    assert!(
        html.contains("fp-callout-warning"),
        "stored value overrides default: {html}"
    );
}

/// Deny-by-default is STRUCTURAL for `plugin_settings` too: callout declares the
/// grant but, loaded with NO `PluginSettingsReader`, has no `fp_get_setting` import
/// and fails to instantiate.
#[test]
fn callout_without_settings_backend_fails_to_load() {
    let Some(wasm) = callout_wasm() else {
        eprintln!(
            "skipping callout_without_settings_backend_fails_to_load: callout wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };
    let mut host = PluginHost::new(); // no with_plugin_settings
    let err = host
        .load_plugin(
            "callout",
            &wasm,
            Capabilities {
                plugin_settings: true,
                ..Default::default()
            },
            HostLimits::default(),
            None,
        )
        .expect_err("a plugin_settings plugin must fail to load when no backend is wired");
    assert!(
        matches!(err, CoreError::Unavailable(_)),
        "expected an instantiation failure, got: {err}"
    );
    assert!(
        err.to_string().contains("fp_get_setting"),
        "the failure must be the unresolved fp_get_setting host import: {err}"
    );
}

/// A plugin id containing a `.` is rejected at load: it would make the
/// `plugin.{id}.{bare}` `Setting`-key namespace ambiguous (a dotted-prefix id pair
/// could read across namespaces). Validation runs before the wasm is even parsed.
#[test]
fn load_plugin_rejects_id_with_dot() {
    let mut host = PluginHost::new();
    let err = host
        .load_plugin(
            "acme.pro",
            b"not-wasm",
            Capabilities::default(),
            HostLimits::default(),
            None,
        )
        .expect_err("a dotted plugin id must be rejected");
    assert!(matches!(err, CoreError::Validation(_)), "got: {err}");
    assert!(
        err.to_string().contains("invalid plugin id"),
        "the failure must name the invalid id: {err}"
    );
}
