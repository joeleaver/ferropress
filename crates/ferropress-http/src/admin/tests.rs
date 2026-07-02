//! Admin API integration tests — drive the EXACT [`router`](crate::router) the
//! server serves, against a REAL embedded store, via `tower`'s `oneshot` (no
//! socket). They cover the full authenticated round-trip (login → cookie → me →
//! list → get → save → re-read), the auth gate (no/invalid session → 401), the
//! capability gate (a low-role session → 403), and save validation.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt; // for `oneshot`

use ferropress_auth::{SigningKey, hash_password};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value, now_millis};
use ferropress_core::{Block, BlockKind, BlockTree, InlineRun, POST_TYPE, Status, USER_TYPE};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;

use crate::admin::AdminConfig;
use crate::{AppState, router};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const SECRET: &str = "test-admin-signing-secret";
const TTL_MS: i64 = 60 * 60 * 1000; // 1h

fn boot(dir: &Path) -> (Arc<dyn RhypeStore>, AppState) {
    let store: Arc<dyn RhypeStore> =
        Arc::new(EmbeddedStore::open(dir.join("db")).expect("open store"));
    let blobs = Arc::new(LocalFsBlobStore::new(dir.join("blobs")));
    let theme = Arc::new(ferropress_serve::default_theme().expect("theme"));
    let admin = AdminConfig {
        signing_key: Arc::new(SigningKey::derive_from_secret(SECRET)),
        bundle_dir: None, // API tested without a built wasm bundle
        cookie_secure: false,
        session_ttl_ms: TTL_MS,
    };
    let state = AppState::new(Arc::clone(&store), blobs, theme).with_admin(admin);
    (store, state)
}

fn one_paragraph(text: &str) -> serde_json::Value {
    BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000TEST".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: text.to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }])
    .to_json_value()
    .expect("tree json")
}

/// Seed a user with a known password + role. Returns the new id.
async fn seed_user(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    password: &str,
    role: &str,
) -> ObjectId {
    let mut f: FieldMap = HashMap::new();
    f.insert("slug".to_owned(), Value::String(slug.to_owned()));
    f.insert(
        "email".to_owned(),
        Value::String(format!("{slug}@example.test")),
    );
    f.insert("uuid".to_owned(), Value::String(format!("uuid-{slug}")));
    f.insert(
        "display_name".to_owned(),
        Value::String(format!("User {slug}")),
    );
    f.insert("role".to_owned(), Value::String(role.to_owned()));
    f.insert(
        "password_hash".to_owned(),
        Value::String(hash_password(password).expect("hash")),
    );
    f.insert("created_at".to_owned(), Value::DateTime(now_millis()));
    store
        .create(&TypeName::from(USER_TYPE), f)
        .await
        .expect("seed user")
}

/// Seed a post. Returns the new id.
async fn seed_post(store: &Arc<dyn RhypeStore>, slug: &str, status: Status) -> ObjectId {
    let mut f: FieldMap = HashMap::new();
    f.insert("slug".to_owned(), Value::String(slug.to_owned()));
    f.insert("title".to_owned(), Value::String(format!("Title {slug}")));
    f.insert("post_type".to_owned(), Value::String("post".to_owned()));
    f.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    f.insert(
        "block_tree".to_owned(),
        Value::Json(one_paragraph("original body")),
    );
    f.insert("created_at".to_owned(), Value::DateTime(now_millis()));
    store
        .create(&TypeName::from(POST_TYPE), f)
        .await
        .expect("seed post")
}

/// POST /admin/api/login and return (status, set-cookie value, body json).
async fn do_login(
    state: &AppState,
    username: &str,
    password: &str,
) -> (StatusCode, Option<String>, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "username": username, "password": password }).to_string(),
        ))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    let body = to_json(resp).await;
    (status, cookie, body)
}

/// The `fp_session=…` name=value pair (first segment) from a Set-Cookie header, to
/// send back as a `Cookie` request header.
fn session_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

async fn to_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_then_me_roundtrips_with_the_cookie() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;

    let (status, cookie, body) = do_login(&state, "jane", "hunter2hunter2").await;
    assert_eq!(status, StatusCode::OK, "login body: {body}");
    let cookie = cookie.expect("login sets a cookie");
    assert!(cookie.contains("fp_session="), "cookie: {cookie}");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    assert!(
        !cookie.contains("Secure"),
        "cookie_secure=false omits Secure"
    );
    assert_eq!(body["user"]["username"], "jane");
    assert_eq!(body["user"]["role"], "administrator");

    // GET /admin/api/me with the cookie returns the same user.
    let req = Request::builder()
        .uri("/admin/api/me")
        .header(header::COOKIE, session_pair(&cookie))
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let me = to_json(resp).await;
    assert_eq!(me["user"]["username"], "jane");
}

#[tokio::test]
async fn login_is_uniform_401_for_bad_password_and_unknown_user() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;

    let (bad_pw, _, _) = do_login(&state, "jane", "wrong").await;
    assert_eq!(bad_pw, StatusCode::UNAUTHORIZED);

    let (unknown, cookie, _) = do_login(&state, "nobody", "whatever").await;
    assert_eq!(unknown, StatusCode::UNAUTHORIZED);
    assert!(
        cookie.is_none(),
        "a failed login must not set a session cookie"
    );
}

#[tokio::test]
async fn guarded_routes_reject_without_a_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_post(&store, "hello", Status::Published).await;

    for (method, uri) in [
        ("GET", "/admin/api/posts"),
        ("GET", "/admin/api/me"),
        ("GET", "/admin/api/posts/1"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    }

    // A forged/garbage cookie is also rejected.
    let req = Request::builder()
        .uri("/admin/api/posts")
        .header(header::COOKIE, "fp_session=not.a.valid.token")
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn low_role_session_is_forbidden_from_posts() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    // A subscriber authenticates fine but lacks EditOthersContent.
    seed_user(&store, "sam", "passwordpassword", "subscriber").await;

    let (status, cookie, _) = do_login(&state, "sam", "passwordpassword").await;
    assert_eq!(status, StatusCode::OK);
    let cookie = session_pair(&cookie.unwrap());

    // me works (valid session)...
    let req = Request::builder()
        .uri("/admin/api/me")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    // ...but listing posts is forbidden.
    let req = Request::builder()
        .uri("/admin/api/posts")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn list_get_save_roundtrip_updates_body_and_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let post_id = seed_post(&store, "hello-world", Status::Published).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // List includes the seeded post.
    let req = Request::builder()
        .uri("/admin/api/posts")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let posts = to_json(resp).await;
    let arr = posts.as_array().expect("array");
    assert!(
        arr.iter()
            .any(|p| p["id"] == post_id.0 && p["slug"] == "hello-world"),
        "list must contain the seeded post: {posts}"
    );

    // Get the post's body.
    let req = Request::builder()
        .uri(format!("/admin/api/posts/{}", post_id.0))
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = to_json(resp).await;
    assert_eq!(detail["status"], "published");
    assert!(detail["block_tree"]["blocks"].is_array());

    // Save an edited body.
    let new_tree = one_paragraph("a freshly edited body about ferrous metallurgy");
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/admin/api/posts/{}", post_id.0))
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({
                "title": "Hello, edited",
                "slug": "hello-world",
                "status": "published",
                "block_tree": new_tree,
            })
            .to_string(),
        ))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "save should succeed");
    let saved = to_json(resp).await;
    assert!(saved["updated_at"].as_i64().unwrap() > 0);

    // Re-read directly from the store: body, title, plaintext, updated_at all updated.
    let obj = store
        .get(&TypeName::from(POST_TYPE), post_id)
        .await
        .expect("get");
    assert!(matches!(obj.get("title"), Some(Value::String(s)) if s == "Hello, edited"));
    let stored_tree = match obj.get("block_tree") {
        Some(Value::Json(j)) => j.clone(),
        other => panic!("block_tree must be Json, got {other:?}"),
    };
    assert_eq!(stored_tree, new_tree, "the edited body must be persisted");
    // plaintext is re-derived from the new body (feeds @vectorize search).
    assert!(
        matches!(obj.get("plaintext"), Some(Value::String(s)) if s.contains("ferrous metallurgy")),
        "plaintext must be re-derived on save: {:?}",
        obj.get("plaintext")
    );
    assert!(matches!(obj.get("updated_at"), Some(Value::DateTime(_))));
}

#[tokio::test]
async fn save_rejects_bad_input() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let post_id = seed_post(&store, "hello", Status::Published).await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let put = |body: serde_json::Value| {
        let cookie = cookie.clone();
        let state = state.clone();
        async move {
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/admin/api/posts/{}", post_id.0))
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            router(state).oneshot(req).await.unwrap().status()
        }
    };
    let good_tree = one_paragraph("x");

    // Empty slug.
    assert_eq!(
        put(
            serde_json::json!({"title":"t","slug":"  ","status":"published","block_tree":good_tree})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    // Unknown status.
    assert_eq!(
        put(serde_json::json!({"title":"t","slug":"hello","status":"nonsense","block_tree":good_tree}))
            .await,
        StatusCode::BAD_REQUEST
    );
    // Malformed block tree.
    assert_eq!(
        put(serde_json::json!({"title":"t","slug":"hello","status":"published","block_tree":{"nope":1}}))
            .await,
        StatusCode::BAD_REQUEST
    );
    // Illegal transition: published -> scheduled is not allowed.
    assert_eq!(
        put(serde_json::json!({"title":"t","slug":"hello","status":"scheduled","block_tree":good_tree}))
            .await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn save_rejects_a_slug_taken_by_another_post() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let a = seed_post(&store, "iron-oxide", Status::Published).await;
    let b = seed_post(&store, "hello", Status::Published).await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());
    let tree = one_paragraph("body");

    // Saving post B with A's slug is a 409 conflict (would make one unreachable).
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/admin/api/posts/{}", b.0))
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"title":"t","slug":"iron-oxide","status":"published","block_tree":tree})
                .to_string(),
        ))
        .unwrap();
    assert_eq!(
        router(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::CONFLICT
    );

    // But saving post A with its OWN slug is fine (self is not a clash).
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/admin/api/posts/{}", a.0))
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"title":"t","slug":"iron-oxide","status":"published","block_tree":tree})
                .to_string(),
        ))
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn logout_clears_the_cookie() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/logout")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cleared = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        cleared.contains("fp_session=;") && cleared.contains("Max-Age=0"),
        "logout must clear the cookie: {cleared}"
    );
}
