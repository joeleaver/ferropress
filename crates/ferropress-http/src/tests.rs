//! End-to-end integration test for the v1 SSR-on-demand serving path.
//!
//! Proves the WHOLE pipeline the running server uses — `store -> render -> theme
//! -> http` — with a real embedded store, by driving the EXACT [`router`](crate::router)
//! the server serves (no socket: `tower`'s `oneshot` feeds a synthetic request
//! straight into the handler graph) and the SAME page chrome the composition root
//! boots ([`ferropress_serve::default_theme`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // for `oneshot`

use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use ferropress_core::{Block, BlockKind, BlockTree, InlineRun, POST_TYPE, Status};

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
