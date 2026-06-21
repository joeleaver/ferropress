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
use ferropress_core::{Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, POST_TYPE, Status};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;

use crate::{AppState, router};

const PARAGRAPH_TEXT: &str = "Hello from the Ferropress end-to-end test.";
const PUBLISHED_SLUG: &str = "hello-world";
const DRAFT_SLUG: &str = "still-a-draft";

/// The block-tree JSON String for a post whose body is one paragraph. Built via
/// the domain types + `to_json_string`, so it matches exactly what
/// `BlockTree::from_json_str` expects on read.
fn paragraph_block_tree_json() -> String {
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
    tree.to_json_string()
        .expect("block tree serializes to JSON")
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
        Value::String(paragraph_block_tree_json()),
    );

    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a post must succeed");
}

/// Boot a real embedded store + the SAME theme the composition root uses, into an
/// `AppState`. Returns the store handle (for seeding) and the state (for serving).
fn boot_state(dir: &Path) -> (Arc<dyn RhypeStore>, AppState) {
    let store: Arc<dyn RhypeStore> =
        Arc::new(EmbeddedStore::open(dir.join("db")).expect("open embedded store"));
    let blobs = Arc::new(LocalFsBlobStore::new(dir.join("blobs")));
    // The EXACT chrome the server boots — not a test-local template.
    let theme = Arc::new(ferropress_serve::default_theme().expect("default theme builds"));
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
        Value::String(tree.to_json_string().expect("serialize block tree")),
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
    let mut host = ferropress_plugin_host::PluginHost::new();
    host.load_plugin("callout", &wasm, Default::default(), Default::default())
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
        Value::String(paragraph_block_tree_json()),
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
