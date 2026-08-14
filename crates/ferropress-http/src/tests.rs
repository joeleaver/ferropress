//! End-to-end integration test for the v1 SSR-on-demand serving path.
//!
//! Proves the WHOLE pipeline the running server uses — `store -> render -> theme
//! -> http` — with a real embedded store, by driving the EXACT [`router`](crate::router)
//! the server serves (no socket: `tower`'s `oneshot` feeds a synthetic request
//! straight into the handler graph) and the SAME page chrome the composition root
//! boots ([`ferropress_serve::default_theme`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt; // for `oneshot`

use ferropress_core::hook::{HookDispatcher, HookEvent};
use ferropress_core::query::Edge;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{
    Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, POST_TYPE, REDIRECT_TYPE, Status,
    TAXONOMY_TYPE, TERM_TYPE, USER_TYPE,
};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_serve::{AuthorDirectory, AuthorsHandle, load_author_directory};
use ferropress_store_embedded::EmbeddedStore;

use crate::{AppState, router};

const PARAGRAPH_TEXT: &str = "Hello from the Ferropress end-to-end test.";
const PUBLISHED_SLUG: &str = "hello-world";
const DRAFT_SLUG: &str = "still-a-draft";

/// The block-tree JSON for a post whose body is one paragraph. Built via the
/// domain types + `to_json_value`, so it matches exactly what
/// `BlockTree::from_json_value` expects on read.
fn paragraph_block_tree_json() -> serde_json::Value {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000TEST".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: PARAGRAPH_TEXT.to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }]);
    tree.to_json_value().expect("block tree serializes to JSON")
}

/// Insert one Post with the given slug + status + a single-paragraph body. Only
/// the fields the serve path reads are populated.
async fn seed_post(store: &Arc<dyn RhypeStore>, slug: &str, status: Status) {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Hello World".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );

    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a post must succeed");
}

/// Boot a real embedded store + the SAME theme the composition root uses, into an
/// `AppState`. Returns the store handle (for seeding) and the state (for serving).
/// A `PluginSettingsReader` that never has a stored value — used where a plugin
/// imports `fp_get_setting` (so the host function must be wired to instantiate) but
/// the test doesn't exercise configuration.
struct NullSettings;
impl ferropress_core::plugin_caps::PluginSettingsReader for NullSettings {
    fn get_setting(
        &self,
        _namespace: &str,
        _key: &str,
    ) -> ferropress_core::error::Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

fn boot_state(dir: &Path) -> (Arc<dyn RhypeStore>, AppState) {
    let store: Arc<dyn RhypeStore> =
        Arc::new(EmbeddedStore::open(dir.join("db")).expect("open embedded store"));
    let blobs = Arc::new(LocalFsBlobStore::new(dir.join("blobs")));
    // The EXACT chrome the server boots — not a test-local template.
    let theme = ferropress_serve::default_theme_handle().expect("default theme builds");
    let state = AppState::new(Arc::clone(&store), blobs, theme);
    (store, state)
}

/// Drive one GET through the real router and return (status, body string).
async fn get(state: &AppState, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request builds");

    // `router(..)` is the EXACT graph `HttpServer::serve` runs; `oneshot` feeds
    // the request straight in, no TcpListener.
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body collects");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

#[tokio::test]
async fn serves_published_post_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, PUBLISHED_SLUG, Status::Published).await;

    // A published post resolves and renders. Asserting the rendered paragraph
    // sits *inside* the chrome <body> proves store -> render -> theme -> http and
    // rules out the text leaking outside the document body.
    let (status, body) = get(&state, &format!("/{PUBLISHED_SLUG}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "published post must be 200; body=\n{body}"
    );

    let rendered = format!("<p>{PARAGRAPH_TEXT}</p>");
    let body_open = body.find("<body>").expect("chrome must have <body>");
    let body_close = body.find("</body>").expect("chrome must have </body>");
    let para_at = body
        .find(&rendered)
        .expect("the rendered <p> paragraph must be present");
    assert!(
        body_open < para_at && para_at < body_close,
        "rendered paragraph must sit inside <body>; body was:\n{body}"
    );
    assert!(
        body.contains("<!doctype html>"),
        "must be wrapped in chrome; body was:\n{body}"
    );

    // An unknown slug is a clean 404 with the generic body.
    let (status, body) = get(&state, "/no-such-slug").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown slug must be 404");
    assert_eq!(body, "Not Found");

    // The health probe is up.
    let (status, body) = get(&state, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

/// Seed one `Redirect` row (from_path/to_path, 301), as the admin handler records on a rename.
async fn seed_redirect(store: &Arc<dyn RhypeStore>, from: &str, to: &str) {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("from_path".to_owned(), Value::String(from.to_owned()));
    fields.insert("to_path".to_owned(), Value::String(to.to_owned()));
    fields.insert("status_code".to_owned(), Value::U32(301));
    store
        .create(&TypeName::from(REDIRECT_TYPE), fields)
        .await
        .expect("seeding a redirect must succeed");
}

/// Drive one GET and return `(status, Location header)` — for asserting a redirect.
async fn get_redirect(state: &AppState, path: &str) -> (StatusCode, Option<String>) {
    let request = Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request builds");
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible");
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, location)
}

#[tokio::test]
async fn a_moved_url_301s_to_its_new_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    // Record a redirect as the admin handler would after renaming /about -> /company.
    seed_redirect(&store, "/about", "/company").await;
    let redirects = ferropress_serve::RedirectHandle::new(
        ferropress_serve::load_redirects(&store)
            .await
            .expect("load redirects"),
    );
    let state = state.with_redirects(redirects);

    // The old path 301s to the new one, BEFORE the cache/resolver is consulted.
    let (status, location) = get_redirect(&state, "/about").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY, "moved URL must 301");
    assert_eq!(
        location.as_deref(),
        Some("/company"),
        "301 points at the new path"
    );

    // A trailing slash on the old path still forwards (normalized keying).
    let (status, location) = get_redirect(&state, "/about/").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location.as_deref(), Some("/company"));

    // A path with no redirect falls through to the normal serve path (404 here).
    let (status, _) = get(&state, "/no-such").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a non-redirected path serves normally"
    );
}

#[tokio::test]
async fn draft_post_is_not_served() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, DRAFT_SLUG, Status::Draft).await;

    // The slug EXISTS but the post is a draft. A 404 here can only come from the
    // `is_published` status gate — not a routing or lookup miss — which proves the
    // gate actually runs.
    let (status, body) = get(&state, &format!("/{DRAFT_SLUG}")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a draft must not be served; body=\n{body}"
    );
    assert_eq!(body, "Not Found");
}

/// The built callout plugin wasm, or `None` if it has not been built yet
/// (`cargo xtask build-plugins`).
fn callout_wasm() -> Option<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels under the repo root")
        .join("plugins/dist/callout/ferropress_plugin_callout.wasm");
    std::fs::read(path).ok()
}

/// Seed a published post whose body is a single custom `callout` block.
async fn seed_callout_post(store: &Arc<dyn RhypeStore>, slug: &str) {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000CALL".to_owned(),
        kind: BlockKind::Custom {
            plugin: "callout".to_owned(),
            name: "callout".to_owned(),
            data: serde_json::json!({ "variant": "warning", "text": "Heads up <b>!</b>" }),
        },
        children: Vec::new(),
    }]);
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Callout".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(tree.to_json_value().expect("serialize block tree")),
    );
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seed callout post");
}

/// Full stack: a custom block is rendered by a REAL plugin through the REAL router.
/// router -> serve_page -> serve_path -> render_object -> render_with ->
/// PluginHost (extism) -> callout wasm -> `<div class="fp-callout …">` in the page.
/// Gated on the wasm being built (skips otherwise, like the ONNX tests).
#[tokio::test]
async fn serves_custom_block_via_plugin() {
    let Some(wasm) = callout_wasm() else {
        eprintln!(
            "skipping serves_custom_block_via_plugin: callout wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());

    // A real plugin host, loaded with the built callout plugin, as the renderer.
    // Callout now imports `fp_get_setting` (the `plugin_settings` capability), so the
    // host function must be wired for it to instantiate; this block's own variant is
    // "warning", so the configured default is never consulted (a null reader suffices).
    let mut host =
        ferropress_plugin_host::PluginHost::new().with_plugin_settings(Arc::new(NullSettings));
    host.load_plugin(
        "callout",
        &wasm,
        ferropress_plugin_host::Capabilities {
            plugin_settings: true,
            ..Default::default()
        },
        Default::default(),
        None,
    )
    .expect("load callout plugin");
    let state = state.with_custom_renderer(Arc::new(host));

    seed_callout_post(&store, "with-callout").await;

    let (status, body) = get(&state, "/with-callout").await;
    assert_eq!(status, StatusCode::OK, "body=\n{body}");
    assert!(
        body.contains("<div class=\"fp-callout fp-callout-warning\">"),
        "the plugin's HTML must appear (not the placeholder); body=\n{body}"
    );
    assert!(
        body.contains("Heads up &lt;b&gt;!&lt;/b&gt;"),
        "the plugin escaped the block text; body=\n{body}"
    );
    assert!(
        !body.contains("data-plugin=\"callout\""),
        "the built-in placeholder must NOT be used when the plugin renders; body=\n{body}"
    );
}

/// `plugins/dist` (the `cargo xtask build-plugins` output), relative to this crate.
fn plugins_dist() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels under the repo root")
        .join("plugins/dist")
}

/// Whether the comment-mod plugin wasm has been built.
fn comment_mod_built() -> bool {
    plugins_dist()
        .join("comment-mod/ferropress_plugin_comment_mod.wasm")
        .exists()
}

/// Whether the wiki plugin wasm has been built.
fn wiki_built() -> bool {
    plugins_dist()
        .join("wiki/ferropress_plugin_wiki.wasm")
        .exists()
}

/// Seed a published post whose body is a single custom `wiki` block with `text`.
async fn seed_wiki_post(store: &Arc<dyn RhypeStore>, slug: &str, text: &str) {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000WIKI".to_owned(),
        kind: BlockKind::Custom {
            plugin: "wiki".to_owned(),
            name: "wiki".to_owned(),
            data: serde_json::json!({ "text": text }),
        },
        children: Vec::new(),
    }]);
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Wiki".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(tree.to_json_value().expect("serialize block tree")),
    );
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seed wiki post");
}

/// Full stack: a wiki custom block resolves `[[links]]` against LIVE content via
/// the `content:read` capability, through the REAL router. The plugin host is
/// wired with the real embedded store as its `ContentReader`; an existing target
/// page renders a normal link, a missing one a red link.
/// router -> serve -> render -> PluginHost -> wiki wasm -> fp_lookup_slug -> store.
#[tokio::test]
async fn serves_wiki_block_resolving_links_via_capability() {
    if !wiki_built() {
        eprintln!(
            "skipping serves_wiki_block_resolving_links_via_capability: wiki wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    // The CONCRETE store backs both the AppState's RhypeStore and the plugin host's
    // ContentReader capability (the composition root does the same coercion).
    let store_concrete =
        Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open embedded store"));
    let store: Arc<dyn RhypeStore> = store_concrete.clone();
    let blobs = Arc::new(LocalFsBlobStore::new(tmp.path().join("blobs")));
    let theme = ferropress_serve::default_theme_handle().expect("default theme builds");

    // A real plugin host loaded from plugins/dist (wiki's plugin.toml grants
    // read_store; callout's grants plugin_settings), with the embedded store backing
    // both capabilities — so the whole dir loads cleanly, as the composition root does.
    let mut host = ferropress_plugin_host::PluginHost::new()
        .with_content_reader(store_concrete.clone())
        .with_plugin_settings(store_concrete);
    host.load_dir(plugins_dist()).expect("load plugins dir");
    let state = AppState::new(store.clone(), blobs, theme).with_custom_renderer(Arc::new(host));

    // The link target exists (a published page); the wiki page links to it + a
    // missing page.
    seed_published_post(&store, "hello-world").await;
    seed_wiki_post(
        &store,
        "my-wiki",
        "See [[Hello World]] and [[No Such Page]].",
    )
    .await;

    let (status, body) = get(&state, "/my-wiki").await;
    assert_eq!(status, StatusCode::OK, "body=\n{body}");
    // The existing target resolved to a normal link (capability read live content).
    assert!(
        body.contains("<a href=\"/hello-world\" class=\"wiki-link\""),
        "existing [[link]] resolved to a normal wiki link; body=\n{body}"
    );
    // The missing target rendered as a red link.
    assert!(
        body.contains("<a href=\"/no-such-page\" class=\"wiki-link wiki-link-new\""),
        "missing [[link]] rendered as a red link; body=\n{body}"
    );
}

/// Seed a published Post and return its id (the comment path attaches to it).
async fn seed_published_post(store: &Arc<dyn RhypeStore>, slug: &str) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Moderated".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seed post")
}

/// POST a JSON comment through the real router; return (status, parsed JSON).
async fn post_comment(
    state: &AppState,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/comments")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds");
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let json = serde_json::from_slice(&bytes).expect("response is JSON");
    (status, json)
}

/// Read a stored comment's `status` by matching its `body`, going straight to the
/// store (the moderation outcome is invisible through the approved-only list API).
async fn comment_status_by_body(
    store: &Arc<dyn RhypeStore>,
    post: ObjectId,
    body: &str,
) -> Option<String> {
    let links = store
        .get_links(&Edge {
            type_name: TypeName::from(POST_TYPE),
            id: post,
            field: "comments".to_owned(),
        })
        .await
        .expect("get comment links");
    let ids: Vec<ObjectId> = links.into_iter().map(|(id, _)| id).collect();
    store
        .get_many(&TypeName::from(COMMENT_TYPE), &ids)
        .await
        .expect("get_many comments")
        .into_iter()
        .find(|obj| matches!(obj.get("body"), Some(Value::String(s)) if s == body))
        .and_then(|obj| match obj.get("status") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
}

/// Full stack: the `comment.create` FILTER hook runs a real plugin (comment-mod)
/// through the REAL router on the comment-create path. A spammy comment lands
/// `spam` (hidden), a clean one `pending` — and the public POST response leaks
/// neither (both report "awaiting moderation"). Gated on the wasm being built.
#[tokio::test]
async fn flags_spam_comment_via_plugin() {
    if !comment_mod_built() {
        eprintln!(
            "skipping flags_spam_comment_via_plugin: comment-mod wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());

    // A real plugin host loaded from plugins/dist — load_dir registers comment-mod's
    // `comment.create` hook from its plugin.toml — wired as the hook dispatcher.
    let mut host = ferropress_plugin_host::PluginHost::new();
    host.load_dir(plugins_dist()).expect("load plugins dir");
    let state = state.with_hook_dispatcher(Arc::new(host));

    let post = seed_published_post(&store, "moderated").await;

    // A spammy comment: still 201, and the response is the SAME neutral
    // "awaiting moderation" as a pending comment — the spam flag is never disclosed.
    let spam_body = "Cheap viagra, click here now!";
    let (status, resp) = post_comment(
        &state,
        serde_json::json!({ "slug": "moderated", "author_name": "Spammer", "body": spam_body }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spam create => 201: {resp}");
    assert_eq!(
        resp["status"], "pending",
        "the POST response must not reveal the spam flag: {resp}"
    );

    // A clean comment.
    let clean_body = "Thoughtful, on-topic remark — thanks for writing this.";
    let (status, resp) = post_comment(
        &state,
        serde_json::json!({ "slug": "moderated", "author_name": "Reader", "body": clean_body }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "clean create => 201: {resp}");
    assert_eq!(resp["status"], "pending");

    // The STORE reveals the filter's effect: spam vs pending.
    assert_eq!(
        comment_status_by_body(&store, post, spam_body)
            .await
            .as_deref(),
        Some("spam"),
        "the comment-mod plugin marked the spammy comment spam"
    );
    assert_eq!(
        comment_status_by_body(&store, post, clean_body)
            .await
            .as_deref(),
        Some("pending"),
        "a clean comment stays pending"
    );

    // Neither is publicly listed (spam never; pending until a moderator approves).
    let (status, listed) = get(&state, "/api/comments?slug=moderated").await;
    assert_eq!(status, StatusCode::OK);
    let arr: serde_json::Value = serde_json::from_str(&listed).expect("json array");
    assert_eq!(
        arr.as_array().expect("array").len(),
        0,
        "neither the spam nor the pending comment is publicly listed: {listed}"
    );
}

/// A stub [`HookDispatcher`] for the `comment.create` filter that replaces the
/// event payload with a fixed `reply` — lets a test drive the create handler's
/// status read-back with ANY plugin response (incl. malformed ones a real PDK
/// guest can't easily produce). No wasm needed, so this runs in the normal suite.
struct StubFilter {
    reply: serde_json::Value,
}

impl HookDispatcher for StubFilter {
    fn dispatch(&self, mut event: HookEvent) -> ferropress_core::error::Result<HookEvent> {
        event.payload = self.reply.clone();
        Ok(event)
    }

    fn has_hooks(&self, name: &str) -> bool {
        name == "comment.create"
    }
}

/// Boot state behind a [`StubFilter`] that returns `reply`, POST one comment with
/// body `body_text`, and return `(stored status, the 201 response JSON)`.
async fn post_with_stub_filter(
    reply: serde_json::Value,
    body_text: &str,
) -> (String, serde_json::Value) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let state = state.with_hook_dispatcher(Arc::new(StubFilter { reply }));
    let post = seed_published_post(&store, "stubbed").await;
    let (status, resp) = post_comment(
        &state,
        serde_json::json!({ "slug": "stubbed", "author_name": "A", "body": body_text }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create => 201: {resp}");
    let stored = comment_status_by_body(&store, post, body_text)
        .await
        .expect("comment stored");
    (stored, resp)
}

/// The create handler reads back the filter's `status` defensively: a malformed,
/// unknown, or absent status leaves the comment `pending` (FAIL-CLOSED — never
/// auto-publish on a bad plugin response), a `spam` verdict is stored but never
/// disclosed in the response, and an honest `approved` verdict is both stored and
/// reflected in the response. Locks the documented contract against a regression
/// that the toolchain cannot catch (e.g. defaulting a deserialize failure to
/// `approved`).
#[tokio::test]
async fn filter_status_readback_is_fail_closed_and_honest() {
    // Unknown status string -> fail-closed to pending.
    let (stored, resp) =
        post_with_stub_filter(serde_json::json!({ "status": "bogus" }), "b1").await;
    assert_eq!(stored, "pending", "unknown status falls back to pending");
    assert_eq!(resp["status"], "pending");

    // Absent status -> pending.
    let (stored, _) = post_with_stub_filter(serde_json::json!({ "author_name": "A" }), "b2").await;
    assert_eq!(stored, "pending", "absent status falls back to pending");

    // Non-object payload -> pending.
    let (stored, _) = post_with_stub_filter(serde_json::json!("not an object"), "b3").await;
    assert_eq!(
        stored, "pending",
        "non-object payload falls back to pending"
    );

    // Honest spam: stored spam, but the response masks it as pending (no leak).
    let (stored, resp) = post_with_stub_filter(serde_json::json!({ "status": "spam" }), "b4").await;
    assert_eq!(stored, "spam", "a spam verdict is stored");
    assert_eq!(
        resp["status"], "pending",
        "the response never discloses spam"
    );

    // Honest auto-approve: a trusted filter may approve; stored approved AND the
    // response reports it honestly.
    let (stored, resp) =
        post_with_stub_filter(serde_json::json!({ "status": "approved" }), "b5").await;
    assert_eq!(stored, "approved", "an approve verdict is honored");
    assert_eq!(
        resp["status"], "approved",
        "auto-approve is reflected honestly"
    );
}

// --- Cross-entity byline resolution through the real HTTP read path ----------

/// Seed a `User` with a unique uuid + display name; return its id.
async fn seed_user(store: &Arc<dyn RhypeStore>, uuid: &str, display_name: &str) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
    fields.insert(
        "display_name".to_owned(),
        Value::String(display_name.to_owned()),
    );
    store
        .create(&TypeName::from(USER_TYPE), fields)
        .await
        .expect("seeding a user must succeed")
}

/// Seed a published post whose `author` to-one relation is linked to `user_id`.
async fn seed_authored_post(store: &Arc<dyn RhypeStore>, slug: &str, user_id: ObjectId) {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Hello World".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    let post_id = store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a post must succeed");
    let edge = Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "author".to_owned(),
    };
    store
        .link(&edge, user_id, HashMap::new())
        .await
        .expect("linking the post author must succeed");
}

/// The full HTTP read path (`router -> serve_page -> serve_path -> compose`) resolves a
/// post's byline LIVE from the shared author directory: renaming the author — as the
/// regen loop does on a `User` change, swapping the SAME `AuthorsHandle` `AppState`
/// holds — updates the byline on the ALREADY-CACHED page with NO regeneration.
#[tokio::test]
async fn serve_page_resolves_byline_live_from_the_author_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());

    let user_id = seed_user(&store, "user-ada", "Ada Lovelace").await;
    seed_authored_post(&store, PUBLISHED_SLUG, user_id).await;

    // Wire the live author directory into the state exactly as the composition root does.
    let authors = AuthorsHandle::new(load_author_directory(&store).await.unwrap());
    let state = state.with_authors(authors.clone());

    // First GET (a cache MISS): the byline renders the author's current name.
    let (status, body) = get(&state, &format!("/{PUBLISHED_SLUG}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("By Ada Lovelace"),
        "the byline must render the author's name; was:\n{body}"
    );

    // Rename the author by swapping the shared handle (what the regen loop does on a
    // `User` change off the feed). No page cache is touched.
    authors.set(AuthorDirectory::from_pairs([(
        user_id.0,
        "Ada, Countess of Lovelace".to_owned(),
    )]));

    // Second GET: a cache HIT, yet the byline reflects the NEW name — composed live.
    let (status2, body2) = get(&state, &format!("/{PUBLISHED_SLUG}")).await;
    assert_eq!(status2, StatusCode::OK);
    assert!(
        body2.contains("By Ada, Countess of Lovelace"),
        "the renamed byline must show live on the cached page; was:\n{body2}"
    );
    assert!(
        !body2.contains("By Ada Lovelace"),
        "the stale byline must be gone; was:\n{body2}"
    );
}

// --- Syndication feeds (RSS + Atom) ------------------------------------------

/// Drive one GET and return `(status, content_type, etag, body)` — the feed tests need the
/// content-type + ETag headers `get` drops.
async fn get_feed(
    state: &AppState,
    path: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, Option<String>, Option<String>, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(etag) = if_none_match {
        builder = builder.header(header::IF_NONE_MATCH, etag);
    }
    let request = builder.body(Body::empty()).expect("request builds");
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible");
    let status = response.status();
    let header_str = |h| {
        response
            .headers()
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = header_str(header::CONTENT_TYPE);
    let etag = header_str(header::ETAG);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body collects");
    (
        status,
        content_type,
        etag,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

#[tokio::test]
async fn feed_xml_serves_rss_naming_published_posts_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, PUBLISHED_SLUG, Status::Published).await;
    seed_post(&store, DRAFT_SLUG, Status::Draft).await;

    let (status, content_type, etag, body) = get_feed(&state, "/feed.xml", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type.as_deref(),
        Some("application/rss+xml; charset=utf-8"),
    );
    assert!(etag.is_some(), "the feed sets an ETag for conditional GET");
    assert!(body.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    assert!(body.contains("<rss"), "an RSS document");
    assert!(
        body.contains(PUBLISHED_SLUG),
        "the published post is syndicated"
    );
    assert!(
        !body.contains(DRAFT_SLUG),
        "a draft must never appear in the public feed",
    );
}

#[tokio::test]
async fn feed_atom_serves_atom_with_its_content_type() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, PUBLISHED_SLUG, Status::Published).await;

    let (status, content_type, _etag, body) = get_feed(&state, "/feed.atom", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type.as_deref(),
        Some("application/atom+xml; charset=utf-8"),
    );
    assert!(body.contains("<feed"), "an Atom document");
    assert!(body.contains(PUBLISHED_SLUG));
}

#[tokio::test]
async fn feed_supports_conditional_get_with_etag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, PUBLISHED_SLUG, Status::Published).await;

    // First request yields an ETag + a body.
    let (status1, _ct, etag, body1) = get_feed(&state, "/feed.xml", None).await;
    assert_eq!(status1, StatusCode::OK);
    assert!(!body1.is_empty());
    let etag = etag.expect("first response carries an ETag");

    // A conditional request with the matching validator gets a bodyless 304.
    let (status2, _ct2, etag2, body2) = get_feed(&state, "/feed.xml", Some(&etag)).await;
    assert_eq!(status2, StatusCode::NOT_MODIFIED, "matching ETag → 304");
    assert!(body2.is_empty(), "a 304 carries no body");
    assert_eq!(etag2.as_ref(), Some(&etag), "304 echoes the ETag");
}

/// Seed a Taxonomy row + one Term linked to it (the serve crate's own `seed_term`, mirrored
/// here since this file has no taxonomy fixtures yet). Returns the term's id.
async fn seed_term(
    store: &Arc<dyn RhypeStore>,
    taxonomy_key: &str,
    slug: &str,
    name: &str,
) -> ObjectId {
    let mut t: HashMap<String, Value> = HashMap::new();
    t.insert("key".to_owned(), Value::String(taxonomy_key.to_owned()));
    t.insert("label".to_owned(), Value::String(taxonomy_key.to_owned()));
    t.insert("hierarchical".to_owned(), Value::Bool(true));
    t.insert("multiple".to_owned(), Value::Bool(true));
    t.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let tax_id = store
        .create(&TypeName::from(TAXONOMY_TYPE), t)
        .await
        .expect("seed taxonomy");

    let mut f: HashMap<String, Value> = HashMap::new();
    f.insert("slug".to_owned(), Value::String(slug.to_owned()));
    f.insert("name".to_owned(), Value::String(name.to_owned()));
    f.insert("description".to_owned(), Value::String(String::new()));
    f.insert("plaintext".to_owned(), Value::String(name.to_owned()));
    f.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let term_id = store
        .create(&TypeName::from(TERM_TYPE), f)
        .await
        .expect("seed term");
    store
        .link(
            &Edge {
                type_name: TypeName::from(TERM_TYPE),
                id: term_id,
                field: "taxonomy".to_owned(),
            },
            tax_id,
            HashMap::new(),
        )
        .await
        .expect("link term to taxonomy");
    term_id
}

/// The redirect shadow-guard's SERVE-time touch point: a live term archive OWNS a path, so a
/// stale redirect recorded FROM that same path (e.g. before the term existed, or before it was
/// re-slugged onto a once-redirected path) must never fire — the archive always wins.
#[tokio::test]
async fn an_archive_owned_path_wins_over_a_stale_redirect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());

    seed_term(&store, "category", "fiction", "Fiction").await;
    let taxonomies = ferropress_serve::TaxonomyHandle::new(
        ferropress_serve::load_taxonomies(&store)
            .await
            .expect("load taxonomies"),
    );
    let state = state.with_taxonomies(taxonomies);

    // A stale redirect recorded FROM the path the archive now owns.
    seed_redirect(&store, "/category/fiction", "/somewhere-else").await;
    let redirects = ferropress_serve::RedirectHandle::new(
        ferropress_serve::load_redirects(&store)
            .await
            .expect("load redirects"),
    );
    let state = state.with_redirects(redirects);

    let (status, location) = get_redirect(&state, "/category/fiction").await;
    assert_ne!(
        status,
        StatusCode::MOVED_PERMANENTLY,
        "a live archive must win over a stale redirect at the same path"
    );
    assert_eq!(location, None, "no redirect must fire");
    assert_eq!(status, StatusCode::OK, "the archive itself must serve");

    // A path the archive does NOT own still redirects normally (the guard is narrow).
    seed_redirect(&store, "/about", "/company").await;
    let redirects2 = ferropress_serve::RedirectHandle::new(
        ferropress_serve::load_redirects(&store)
            .await
            .expect("load redirects"),
    );
    let state = state.with_redirects(redirects2);
    let (status, location) = get_redirect(&state, "/about").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location.as_deref(), Some("/company"));
}

/// A bare structural `/page/1` suffix 301s to the home base (`/`) — page 1's canonical URL
/// has no suffix at all.
#[tokio::test]
async fn bare_page_1_suffix_redirects_to_the_home_base() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_store, state) = boot_state(tmp.path());
    let (status, location) = get_redirect(&state, "/page/1").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location.as_deref(), Some("/"));
}

/// The archive twin: `/{taxonomy_key}/{chain}/page/1` 301s to the archive's own bare base.
#[tokio::test]
async fn archive_page_1_suffix_redirects_to_the_archive_base() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_term(&store, "category", "fiction", "Fiction").await;
    let taxonomies = ferropress_serve::TaxonomyHandle::new(
        ferropress_serve::load_taxonomies(&store)
            .await
            .expect("load taxonomies"),
    );
    let state = state.with_taxonomies(taxonomies);

    let (status, location) = get_redirect(&state, "/category/fiction/page/1").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location.as_deref(), Some("/category/fiction"));
}

/// Claim-only-on-resolve extends to the `/page/1` 301 rule too: a path shaped like a
/// page-1 suffix whose stripped base resolves as NEITHER the front nor a live archive must
/// never redirect (it would 301 to a dead page) — it falls through to the ordinary 404 flow.
#[tokio::test]
async fn page_1_suffix_does_not_redirect_when_the_base_does_not_resolve() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_store, state) = boot_state(tmp.path());
    let (status, location) = get_redirect(&state, "/no-such-archive/page/1").await;
    assert_ne!(
        status,
        StatusCode::MOVED_PERMANENTLY,
        "an unresolvable base must never 301"
    );
    assert_eq!(location, None);
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "falls through to the ordinary 404 flow"
    );
}
