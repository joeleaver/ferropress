//! Island API integration tests.
//!
//! * **Comments** run against a REAL embedded store (no ONNX needed) driving the
//!   exact [`router`](crate::router) the server serves, via `tower`'s `oneshot`.
//!   They cover the moderation gate (approved-only listing, POST creates pending),
//!   threading (`parent_id`), and input validation.
//! * **Search** runs against a `RhypeStore` DOUBLE returning canned
//!   `vector_search` + `get_many` results, so the handler's ranking / publish-gate
//!   / DTO-mapping logic is exercised deterministically WITHOUT the runtime ONNX
//!   embedding model. The real end-to-end search path is ONNX-gated (see
//!   `island::search` module docs).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt; // for `oneshot`

use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value, rfc3339_to_millis};
use ferropress_core::{
    Block, BlockKind, BlockTree, COMMENT_TYPE, CommentStatus, InlineRun, PAGE_TYPE, POST_TYPE,
    Status,
};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;

use crate::{AppState, router};

// ---------------------------------------------------------------------------
// Shared harness
// ---------------------------------------------------------------------------

/// Boot a real embedded store + the SAME theme the composition root uses.
fn boot_state(dir: &Path) -> (Arc<dyn RhypeStore>, AppState) {
    let store: Arc<dyn RhypeStore> =
        Arc::new(EmbeddedStore::open(dir.join("db")).expect("open embedded store"));
    let blobs = Arc::new(LocalFsBlobStore::new(dir.join("blobs")));
    let theme = Arc::new(ferropress_serve::default_theme().expect("default theme builds"));
    let state = AppState::new(Arc::clone(&store), blobs, theme);
    (store, state)
}

/// A one-paragraph block-tree JSON value (the serve path needs a parseable
/// `block_tree`, though the comment API itself does not read it).
fn paragraph_block_tree_json() -> serde_json::Value {
    BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000TEST".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: "Body.".to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }])
    .to_json_value()
    .expect("block tree serializes")
}

/// Seed a Post with the given slug + status. Returns its id.
async fn seed_post(store: &Arc<dyn RhypeStore>, slug: &str, status: Status) -> ObjectId {
    let mut fields: FieldMap = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("A Post".to_owned()));
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

/// Seed a Page with the given slug + status. Returns its id.
async fn seed_page(store: &Arc<dyn RhypeStore>, slug: &str, status: Status) -> ObjectId {
    let mut fields: FieldMap = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("A Page".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    store
        .create(&TypeName::from(PAGE_TYPE), fields)
        .await
        .expect("seed page")
}

/// Seed a Comment attached to `post_id` (inline `post` relation).
async fn seed_comment(
    store: &Arc<dyn RhypeStore>,
    post_id: ObjectId,
    status: CommentStatus,
    author_name: &str,
    body: &str,
    created_at: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    seed_comment_on(
        store,
        "post",
        post_id,
        status,
        author_name,
        body,
        created_at,
        parent,
    )
    .await
}

/// Seed a Comment attached to an entity via the given relation field (`"post"` or
/// `"page"`), with explicit status / author / body / created_at and an optional
/// `parent`.
#[allow(clippy::too_many_arguments)]
async fn seed_comment_on(
    store: &Arc<dyn RhypeStore>,
    relation: &str,
    entity_id: ObjectId,
    status: CommentStatus,
    author_name: &str,
    body: &str,
    created_at: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    let mut fields: FieldMap = HashMap::new();
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("body".to_owned(), Value::String(body.to_owned()));
    fields.insert("plaintext".to_owned(), Value::String(body.to_owned()));
    fields.insert(
        "author_name".to_owned(),
        Value::String(author_name.to_owned()),
    );
    fields.insert(
        "created_at".to_owned(),
        Value::DateTime(rfc3339_to_millis(created_at).expect("test seed created_at is RFC3339")),
    );
    fields.insert(relation.to_owned(), Value::U64(entity_id.0));
    if let Some(ObjectId(pid)) = parent {
        fields.insert("parent".to_owned(), Value::U64(pid));
    }
    store
        .create(&TypeName::from(COMMENT_TYPE), fields)
        .await
        .expect("seed comment")
}

/// Approve a comment (moderation action) via the store.
async fn approve(store: &Arc<dyn RhypeStore>, id: ObjectId) {
    let mut patch: FieldMap = HashMap::new();
    patch.insert(
        "status".to_owned(),
        Value::String(CommentStatus::Approved.as_str().to_owned()),
    );
    store
        .update(&TypeName::from(COMMENT_TYPE), id, patch)
        .await
        .expect("approve comment");
}

/// Drive one GET through the real router; return (status, parsed JSON body).
async fn get_json(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("request builds");
    drive(state, request).await
}

/// Drive one POST with a JSON body through the real router.
async fn post_json(
    state: &AppState,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds");
    drive(state, request).await
}

/// Drive one POST with a JSON body AND a custom User-Agent header.
async fn post_json_ua(
    state: &AppState,
    path: &str,
    body: serde_json::Value,
    user_agent: &str,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::USER_AGENT, user_agent)
        .body(Body::from(body.to_string()))
        .expect("request builds");
    drive(state, request).await
}

/// Drive one POST with a RAW body and an optional explicit Content-Type, for the
/// extractor-contract tests (malformed JSON, missing Content-Type).
async fn post_raw(
    state: &AppState,
    path: &str,
    content_type: Option<&str>,
    raw_body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let request = builder
        .body(Body::from(raw_body.to_owned()))
        .expect("request builds");
    drive(state, request).await
}

async fn drive(state: &AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let json: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response is JSON")
    };
    (status, json)
}

// ---------------------------------------------------------------------------
// Comments — listing + moderation gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_only_approved_comments_chronologically() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "hello", Status::Published).await;

    // Two approved (out of timestamp order) + one pending + one spam.
    seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Bob",
        "second",
        "2026-01-02T00:00:00Z",
        None,
    )
    .await;
    seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Alice",
        "first",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;
    seed_comment(
        &store,
        post,
        CommentStatus::Pending,
        "Mallory",
        "pending",
        "2026-01-03T00:00:00Z",
        None,
    )
    .await;
    seed_comment(
        &store,
        post,
        CommentStatus::Spam,
        "Spammer",
        "spam",
        "2026-01-04T00:00:00Z",
        None,
    )
    .await;

    let (status, body) = get_json(&state, "/api/comments?slug=hello").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    // Only the two approved comments, oldest first.
    assert_eq!(arr.len(), 2, "only approved comments are listed: {body}");
    assert_eq!(arr[0]["body"], "first");
    assert_eq!(arr[0]["author_name"], "Alice");
    assert_eq!(arr[1]["body"], "second");
    // PII is never exposed.
    assert!(arr[0].get("author_email").is_none());
    // No parent => `parent_id` is omitted entirely.
    assert!(arr[0].get("parent_id").is_none());
}

#[tokio::test]
async fn list_exposes_parent_id_for_threaded_replies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "hello", Status::Published).await;

    let parent = seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Top",
        "top-level",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;
    seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Reply",
        "a reply",
        "2026-01-02T00:00:00Z",
        Some(parent),
    )
    .await;

    let (status, body) = get_json(&state, "/api/comments?slug=hello").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    // The reply (second, chronologically) carries the parent's id.
    let reply = arr.iter().find(|c| c["body"] == "a reply").expect("reply");
    assert_eq!(
        reply["parent_id"].as_u64(),
        Some(parent.0),
        "reply must expose its parent id: {body}"
    );
    let top = arr.iter().find(|c| c["body"] == "top-level").expect("top");
    assert!(top.get("parent_id").is_none(), "top-level has no parent");
}

#[tokio::test]
async fn list_empty_when_no_approved_comments() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "hello", Status::Published).await;
    seed_comment(
        &store,
        post,
        CommentStatus::Pending,
        "P",
        "pending",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;

    let (status, body) = get_json(&state, "/api/comments?slug=hello").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn list_unknown_or_unpublished_slug_is_404() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "draft", Status::Draft).await;

    let (status, _) = get_json(&state, "/api/comments?slug=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown slug => 404");

    let (status, _) = get_json(&state, "/api/comments?slug=draft").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "draft slug => 404");
}

#[tokio::test]
async fn list_empty_slug_is_400() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_store, state) = boot_state(tmp.path());
    let (status, body) = get_json(&state, "/api/comments?slug=").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "JSON error body: {body}");
}

// ---------------------------------------------------------------------------
// Comments — creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_creates_pending_comment_hidden_until_approved() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    // POST a valid comment.
    let (status, body) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({
            "slug": "hello",
            "author_name": "Visitor",
            "author_email": "visitor@example.com",
            "body": "Great post!",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create => 201: {body}");
    assert_eq!(body["status"], "pending", "new comments await moderation");
    let new_id = body["id"].as_u64().expect("id");

    // It is NOT visible yet (pending).
    let (_, listed) = get_json(&state, "/api/comments?slug=hello").await;
    assert_eq!(
        listed.as_array().expect("array").len(),
        0,
        "pending comment must be hidden"
    );

    // Approve it, then it appears.
    approve(&store, ObjectId(new_id)).await;
    let (_, listed) = get_json(&state, "/api/comments?slug=hello").await;
    let arr = listed.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["body"], "Great post!");
    assert_eq!(arr[0]["author_name"], "Visitor");
}

#[tokio::test]
async fn post_threaded_reply_links_to_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "hello", Status::Published).await;
    let parent = seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Top",
        "top",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;

    let (status, body) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({
            "slug": "hello",
            "author_name": "Replier",
            "body": "I agree",
            "parent_id": parent.0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "reply create => 201: {body}");
    approve(&store, ObjectId(body["id"].as_u64().expect("id"))).await;

    let (_, listed) = get_json(&state, "/api/comments?slug=hello").await;
    let arr = listed.as_array().expect("array");
    let reply = arr.iter().find(|c| c["body"] == "I agree").expect("reply");
    assert_eq!(reply["parent_id"].as_u64(), Some(parent.0));
}

#[tokio::test]
async fn post_rejects_parent_from_another_entity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post_a = seed_post(&store, "post-a", Status::Published).await;
    seed_post(&store, "post-b", Status::Published).await;
    // A comment that belongs to post-a.
    let foreign = seed_comment(
        &store,
        post_a,
        CommentStatus::Approved,
        "X",
        "on a",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;

    // Replying on post-b with post-a's comment as parent must be rejected.
    let (status, body) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({
            "slug": "post-b",
            "author_name": "Y",
            "body": "wrong parent",
            "parent_id": foreign.0,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "cross-entity parent: {body}"
    );
}

#[tokio::test]
async fn post_validation_rejects_bad_input() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    // Missing body.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "A", "body": "   " }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "blank body");

    // Missing author_name.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "", "body": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "blank author_name");

    // Bad email.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "A", "body": "hi", "author_email": "not-an-email" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad email");

    // Unknown slug.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "nope", "author_name": "A", "body": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown slug => 404");
}

#[tokio::test]
async fn post_enforces_length_caps_and_url_scheme() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    // author_name over the 100-char cap.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "a".repeat(101), "body": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "over-long author_name");

    // body over the 10_000-char cap.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "A", "body": "x".repeat(10_001) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "over-long body");

    // A `javascript:` author_url is rejected (stored-XSS guard).
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "A", "body": "hi", "author_url": "javascript:alert(1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "javascript: url");

    // A control character in author_name is rejected.
    let (status, _) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "Bad\r\nName", "body": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "control chars in name");

    // A valid https author_url IS accepted and echoed back (after approval).
    let (status, body) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "hello", "author_name": "A", "body": "hi", "author_url": "https://example.com/me" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "valid https url accepted");
    approve(&store, ObjectId(body["id"].as_u64().expect("id"))).await;
    let (_, listed) = get_json(&state, "/api/comments?slug=hello").await;
    let arr = listed.as_array().expect("array");
    assert_eq!(arr[0]["author_url"], "https://example.com/me");
}

#[tokio::test]
async fn list_never_exposes_pii_even_when_present() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    // Round-trip a comment that DOES carry email + user_agent (UA via header), so
    // the PII gate is load-bearing rather than passing because no PII was stored.
    let (status, body) = post_json_ua(
        &state,
        "/api/comments",
        serde_json::json!({
            "slug": "hello",
            "author_name": "Visitor",
            "author_email": "visitor@example.com",
            "author_url": "https://example.com",
            "body": "hi",
        }),
        "Mozilla/5.0 (test-agent)",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    approve(&store, ObjectId(body["id"].as_u64().expect("id"))).await;

    let (_, listed) = get_json(&state, "/api/comments?slug=hello").await;
    let comment = &listed.as_array().expect("array")[0];
    // The public DTO must omit every moderation-only PII field …
    assert!(comment.get("author_email").is_none(), "no email: {comment}");
    assert!(comment.get("author_ip").is_none(), "no ip: {comment}");
    assert!(
        comment.get("user_agent").is_none(),
        "no user_agent: {comment}"
    );
    // … while the public author_url IS surfaced.
    assert_eq!(comment["author_url"], "https://example.com");
}

#[tokio::test]
async fn list_drops_parent_id_when_parent_is_not_approved() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "hello", Status::Published).await;

    // A PENDING parent with an APPROVED reply: the reply must render as top-level,
    // not dangling at a parent that is absent from the (approved-only) listing.
    let pending_parent = seed_comment(
        &store,
        post,
        CommentStatus::Pending,
        "Hidden",
        "held parent",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;
    seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "Replier",
        "visible reply",
        "2026-01-02T00:00:00Z",
        Some(pending_parent),
    )
    .await;

    let (status, body) = get_json(&state, "/api/comments?slug=hello").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1, "only the approved reply is listed: {body}");
    assert_eq!(arr[0]["body"], "visible reply");
    assert!(
        arr[0].get("parent_id").is_none(),
        "parent_id to a non-approved parent must be dropped: {body}"
    );
}

// ---------------------------------------------------------------------------
// Comments — the Page entity branch (mirrors the Post suite)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_and_create_comments_on_a_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let page = seed_page(&store, "about", Status::Published).await;

    // A pre-seeded approved comment attached to the Page (via the `page` relation).
    seed_comment_on(
        &store,
        "page",
        page,
        CommentStatus::Approved,
        "Reader",
        "on the page",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;

    // List resolves the Page's @inverse(Comment.page) thread.
    let (status, body) = get_json(&state, "/api/comments?slug=about").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1, "page comment listed: {body}");
    assert_eq!(arr[0]["body"], "on the page");

    // Create attaches to the Page (the `page` relation branch of create()).
    let (status, created) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({ "slug": "about", "author_name": "V", "body": "new page comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "page create => 201: {created}");
    approve(&store, ObjectId(created["id"].as_u64().expect("id"))).await;
    let (_, listed) = get_json(&state, "/api/comments?slug=about").await;
    assert_eq!(
        listed.as_array().expect("array").len(),
        2,
        "both page comments listed after approval"
    );
}

#[tokio::test]
async fn post_rejects_cross_entity_parent_with_page_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    let post = seed_post(&store, "a-post", Status::Published).await;
    seed_page(&store, "a-page", Status::Published).await;
    // A comment that belongs to the POST.
    let post_comment = seed_comment(
        &store,
        post,
        CommentStatus::Approved,
        "X",
        "on the post",
        "2026-01-01T00:00:00Z",
        None,
    )
    .await;

    // Replying ON THE PAGE with the post's comment as parent must be rejected.
    let (status, body) = post_json(
        &state,
        "/api/comments",
        serde_json::json!({
            "slug": "a-page",
            "author_name": "Y",
            "body": "wrong parent",
            "parent_id": post_comment.0,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "post comment as a page reply's parent: {body}"
    );
}

// ---------------------------------------------------------------------------
// Extractor contract — malformed/absent input stays inside the JSON envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn absent_query_param_is_uniform_json_400() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_store, state) = boot_state(tmp.path());

    // No `slug` param at all (vs the empty `slug=` case) still returns JSON.
    let (status, body) = get_json(&state, "/api/comments").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "JSON error envelope: {body}");

    // Same for search's `q`.
    let (status, body) = get_json(&state, "/api/search").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "JSON error envelope: {body}");
}

#[tokio::test]
async fn malformed_or_missing_content_type_post_is_uniform_json_400() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, state) = boot_state(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    // Syntactically malformed JSON body.
    let (status, body) = post_raw(
        &state,
        "/api/comments",
        Some("application/json"),
        "{ not valid json",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "malformed JSON => 400");
    assert!(body["error"].is_string(), "JSON error envelope: {body}");

    // Valid JSON but no Content-Type header (the Json extractor would reject 415).
    let (status, body) = post_raw(
        &state,
        "/api/comments",
        None,
        &serde_json::json!({ "slug": "hello", "author_name": "A", "body": "hi" }).to_string(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing content-type => 400"
    );
    assert!(body["error"].is_string(), "JSON error envelope: {body}");

    // Valid JSON, correct Content-Type, but missing the required `author_name` key.
    let (status, body) = post_raw(
        &state,
        "/api/comments",
        Some("application/json"),
        &serde_json::json!({ "slug": "hello", "body": "hi" }).to_string(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing required key => 400"
    );
    assert!(body["error"].is_string(), "JSON error envelope: {body}");
}

// ---------------------------------------------------------------------------
// Search — handler logic via a store double (no ONNX)
// ---------------------------------------------------------------------------

mod search_double {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use futures_core::stream::BoxStream;

    use ferropress_core::error::Result as CoreResult;
    use ferropress_core::query::{
        Change, Edge, FilterSpec, ScoredId, SubscribeFilter, VectorQuery,
    };
    use ferropress_core::value::Object;

    /// A `RhypeStore` double for the search handler: it returns canned
    /// `vector_search` scores and `get_many` posts, and records the last
    /// `VectorQuery` so tests can assert parameter handling (e.g. the `k` clamp).
    /// Every other verb is unused by the search handler and panics if reached.
    pub(super) struct MockSearchStore {
        pub scored: Vec<ScoredId>,
        pub posts: Vec<Object>,
        pub last_query: Mutex<Option<VectorQuery>>,
    }

    #[async_trait]
    impl RhypeStore for MockSearchStore {
        async fn vector_search(&self, query: VectorQuery) -> CoreResult<Vec<ScoredId>> {
            *self.last_query.lock().unwrap() = Some(query);
            Ok(self.scored.clone())
        }

        async fn get_many(&self, _type_: &TypeName, ids: &[ObjectId]) -> CoreResult<Vec<Object>> {
            // Mirror the engine: return objects whose id is requested, sorted by
            // id (NOT score order) — the handler must restore ranking itself.
            let want: std::collections::HashSet<u64> = ids.iter().map(|i| i.0).collect();
            let mut out: Vec<Object> = self
                .posts
                .iter()
                .filter(|o| want.contains(&o.id.0))
                .cloned()
                .collect();
            out.sort_by_key(|o| o.id.0);
            Ok(out)
        }

        async fn create(&self, _t: &TypeName, _f: FieldMap) -> CoreResult<ObjectId> {
            unimplemented!("not used by the search handler")
        }
        async fn create_batch(
            &self,
            _t: &TypeName,
            _r: Vec<FieldMap>,
        ) -> CoreResult<Vec<ObjectId>> {
            unimplemented!()
        }
        async fn get(&self, _t: &TypeName, _id: ObjectId) -> CoreResult<Object> {
            unimplemented!()
        }
        async fn scan(&self, _t: &TypeName) -> CoreResult<Vec<Object>> {
            unimplemented!()
        }
        async fn update(&self, _t: &TypeName, _id: ObjectId, _p: FieldMap) -> CoreResult<()> {
            unimplemented!()
        }
        async fn delete(&self, _t: &TypeName, _id: ObjectId) -> CoreResult<()> {
            unimplemented!()
        }
        async fn link(&self, _f: &Edge, _to: ObjectId, _ef: FieldMap) -> CoreResult<()> {
            unimplemented!()
        }
        async fn unlink(&self, _f: &Edge, _to: ObjectId) -> CoreResult<()> {
            unimplemented!()
        }
        async fn get_links(&self, _f: &Edge) -> CoreResult<Vec<(ObjectId, FieldMap)>> {
            unimplemented!()
        }
        async fn filter(&self, _s: FilterSpec) -> CoreResult<Vec<Object>> {
            unimplemented!()
        }
        async fn subscribe(&self, _f: SubscribeFilter) -> CoreResult<BoxStream<'static, Change>> {
            unimplemented!()
        }
    }

    fn post(id: u64, slug: &str, title: &str, status: Status) -> Object {
        let mut fields: FieldMap = HashMap::new();
        fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
        fields.insert("title".to_owned(), Value::String(title.to_owned()));
        fields.insert(
            "status".to_owned(),
            Value::String(status.as_str().to_owned()),
        );
        fields.insert("excerpt".to_owned(), Value::String(format!("about {slug}")));
        Object {
            type_name: TypeName::from(POST_TYPE),
            id: ObjectId(id),
            fields,
        }
    }

    fn state_with(store: MockSearchStore) -> AppState {
        let store: Arc<dyn RhypeStore> = Arc::new(store);
        // The search handler never touches blobs/theme, but AppState owns them.
        let blobs = Arc::new(LocalFsBlobStore::new(std::path::PathBuf::from(
            "/nonexistent-blobs-unused",
        )));
        let theme = Arc::new(ferropress_serve::default_theme().expect("theme"));
        AppState::new(store, blobs, theme)
    }

    #[tokio::test]
    async fn ranks_by_score_and_filters_unpublished() {
        // Scores in descending order over ids 30, 10, 20. Post 20 is a draft.
        let store = MockSearchStore {
            scored: vec![
                ScoredId {
                    id: ObjectId(30),
                    score: 0.9,
                },
                ScoredId {
                    id: ObjectId(10),
                    score: 0.8,
                },
                ScoredId {
                    id: ObjectId(20),
                    score: 0.7,
                },
            ],
            posts: vec![
                post(10, "ten", "Ten", Status::Published),
                post(20, "twenty", "Twenty", Status::Draft),
                post(30, "thirty", "Thirty", Status::Published),
            ],
            last_query: Mutex::new(None),
        };
        let state = state_with(store);

        let (status, body) = get_json(&state, "/api/search?q=hello").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let arr = body.as_array().expect("array");
        // Draft (20) filtered; published kept in SCORE order (30 then 10), not id order.
        assert_eq!(arr.len(), 2, "draft filtered out: {body}");
        assert_eq!(arr[0]["slug"], "thirty");
        assert_eq!(arr[0]["id"].as_u64(), Some(30));
        assert_eq!(arr[1]["slug"], "ten");
        assert_eq!(arr[0]["excerpt"], "about thirty");
        assert!(arr[0]["score"].is_number());
    }

    #[tokio::test]
    async fn empty_query_is_400_without_touching_store() {
        let store = MockSearchStore {
            scored: vec![],
            posts: vec![],
            last_query: Mutex::new(None),
        };
        let state = state_with(store);
        let (status, body) = get_json(&state, "/api/search?q=%20%20").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn over_long_query_is_400_without_touching_store() {
        let store = MockSearchStore {
            scored: vec![],
            posts: vec![],
            last_query: Mutex::new(None),
        };
        let store = Arc::new(store);
        let state = {
            let blobs = Arc::new(LocalFsBlobStore::new(std::path::PathBuf::from(
                "/nonexistent-blobs-unused",
            )));
            let theme = Arc::new(ferropress_serve::default_theme().expect("theme"));
            AppState::new(Arc::clone(&store) as Arc<dyn RhypeStore>, blobs, theme)
        };
        // q over MAX_QUERY_LEN (1_000) — rejected before embedding/searching.
        let path = format!("/api/search?q={}", "a".repeat(1_001));
        let (status, body) = get_json(&state, &path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
        assert!(
            store.last_query.lock().unwrap().is_none(),
            "store must not be queried for an over-long q"
        );
    }

    #[tokio::test]
    async fn k_is_clamped_to_max() {
        let store = MockSearchStore {
            scored: vec![],
            posts: vec![],
            last_query: Mutex::new(None),
        };
        let store = Arc::new(store);
        let state = {
            let blobs = Arc::new(LocalFsBlobStore::new(std::path::PathBuf::from(
                "/nonexistent-blobs-unused",
            )));
            let theme = Arc::new(ferropress_serve::default_theme().expect("theme"));
            AppState::new(Arc::clone(&store) as Arc<dyn RhypeStore>, blobs, theme)
        };

        let (status, _) = get_json(&state, "/api/search?q=hi&k=9999").await;
        assert_eq!(status, StatusCode::OK);
        let recorded = store.last_query.lock().unwrap().clone().expect("query ran");
        assert_eq!(recorded.k, 50, "k must be clamped to MAX_K");
    }

    #[tokio::test]
    async fn malformed_query_value_is_uniform_json_400() {
        // `k` is a usize; a non-integer value makes axum's Query extractor reject.
        // The custom `ApiQuery` wrapper must map that rejection into the uniform
        // JSON envelope (this is the path a bare `Query` would return plain text
        // for), and it must short-circuit before any store call.
        let store = MockSearchStore {
            scored: vec![],
            posts: vec![],
            last_query: Mutex::new(None),
        };
        let store = Arc::new(store);
        let state = {
            let blobs = Arc::new(LocalFsBlobStore::new(std::path::PathBuf::from(
                "/nonexistent-blobs-unused",
            )));
            let theme = Arc::new(ferropress_serve::default_theme().expect("theme"));
            AppState::new(Arc::clone(&store) as Arc<dyn RhypeStore>, blobs, theme)
        };

        let (status, body) = get_json(&state, "/api/search?q=hi&k=abc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string(), "JSON error envelope: {body}");
        assert!(
            store.last_query.lock().unwrap().is_none(),
            "a malformed query must not reach the store"
        );
    }
}
