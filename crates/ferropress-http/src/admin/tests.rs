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
use ferropress_core::{
    Block, BlockKind, BlockTree, Edge, InlineRun, MEDIA_TYPE, POST_TYPE, Status, USER_TYPE,
};

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

/// A valid 1×1 PNG (the smallest real image `imagesize` can sniff a type + size
/// from). Decoded from base64 so the fixture stays readable.
fn tiny_png() -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+P+/HgAFhAJ/wlseKgAAAABJRU5ErkJggg==",
        )
        .expect("valid base64 PNG")
}

/// A minimal PNG *header* declaring `width`×`height`. `imagesize` reads dimensions
/// from the IHDR at fixed offsets and needs no IDAT/IEND, so this is enough to test
/// the dimension guard with a tiny file that CLAIMS a huge canvas.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG signature
    v.extend_from_slice(&[0, 0, 0, 0x0D]); // IHDR chunk length = 13
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth, color type, compression, filter, interlace
    v
}

/// Build a `multipart/form-data` body with a `file` part (bytes + filename + a
/// declared content-type — which the server IGNORES in favor of sniffing) and an
/// `alt` text part. Returns the `Content-Type` header value + the raw body bytes.
fn multipart_image(
    filename: &str,
    declared_ct: &str,
    bytes: &[u8],
    alt: &str,
) -> (String, Vec<u8>) {
    let boundary = "FerropressTestBoundary8f3a";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {declared_ct}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"alt\"\r\n\r\n");
    body.extend_from_slice(alt.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
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

/// Seed a `Media` row (mirrors the scalar fields the upload handler writes, minus
/// the `meta` Json field this test doesn't read) so a test can feature it. Returns
/// the created row's `ObjectId` (`uuid` is an input, echoed in the served URL).
async fn seed_media(store: &Arc<dyn RhypeStore>, uuid: &str) -> ObjectId {
    let mut f: FieldMap = HashMap::new();
    f.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
    f.insert("slug".to_owned(), Value::String(uuid.to_owned()));
    f.insert("filename".to_owned(), Value::String(format!("{uuid}.png")));
    f.insert("mime_type".to_owned(), Value::String("image/png".to_owned()));
    f.insert("byte_size".to_owned(), Value::U64(68));
    f.insert("width".to_owned(), Value::U32(1));
    f.insert("height".to_owned(), Value::U32(1));
    f.insert("alt_text".to_owned(), Value::String(String::new()));
    f.insert("caption".to_owned(), Value::String(String::new()));
    f.insert("description".to_owned(), Value::String(String::new()));
    f.insert(
        "blob_key".to_owned(),
        Value::String(format!("media/{uuid}.png")),
    );
    f.insert("plaintext".to_owned(), Value::String(String::new()));
    f.insert("focal_x".to_owned(), Value::F32(0.5));
    f.insert("focal_y".to_owned(), Value::F32(0.5));
    f.insert("created_at".to_owned(), Value::DateTime(now_millis()));
    store
        .create(&TypeName::from(MEDIA_TYPE), f)
        .await
        .expect("seed media")
}

/// POST /admin/api/posts (create) with the given cookie + JSON body; return
/// (status, body json).
async fn do_create(
    state: &AppState,
    cookie: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/posts")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
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
async fn featured_media_set_read_and_cleared() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let post_id = seed_post(&store, "featured", Status::Published).await;
    let uuid = "0191aaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let media_id = seed_media(&store, uuid).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let save_body = |featured: serde_json::Value| {
        serde_json::json!({
            "title": "Featured",
            "slug": "featured",
            "status": "published",
            "block_tree": one_paragraph("body"),
            "featured_media": featured,
        })
        .to_string()
    };
    let put = |body: String| {
        Request::builder()
            .method("PUT")
            .uri(format!("/admin/api/posts/{}", post_id.0))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let get = || {
        Request::builder()
            .uri(format!("/admin/api/posts/{}", post_id.0))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap()
    };

    // Save WITH a featured image → get_one returns {id, url} keyed by the uuid.
    let resp = router(state.clone())
        .oneshot(put(save_body(serde_json::json!(media_id.0))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "save with featured should succeed");
    let detail = to_json(router(state.clone()).oneshot(get()).await.unwrap()).await;
    assert_eq!(detail["featured_media"]["id"], media_id.0);
    assert_eq!(detail["featured_media"]["url"], format!("/media/{uuid}"));

    // The list row carries the same thumbnail URL.
    let list = to_json(
        router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/posts")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert!(
        list.as_array().unwrap().iter().any(|p| p["id"] == post_id.0
            && p["featured_media"]["url"] == format!("/media/{uuid}")),
        "list row must carry featured_media: {list}"
    );

    // Save with null clears the relation.
    let resp = router(state.clone())
        .oneshot(put(save_body(serde_json::Value::Null)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = to_json(router(state.clone()).oneshot(get()).await.unwrap()).await;
    assert!(
        detail["featured_media"].is_null(),
        "featured should be cleared: {detail}"
    );

    // A nonexistent featured id is rejected up front (no half-save).
    let resp = router(state.clone())
        .oneshot(put(save_body(serde_json::json!(9_999_999))))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a bogus featured_media id must 400"
    );
}

#[tokio::test]
async fn create_with_featured_then_replace_keeps_it_to_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let a = seed_media(&store, "0191aaaa-0000-7000-8000-000000000001").await;
    let b = seed_media(&store, "0191bbbb-0000-7000-8000-000000000002").await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // Create a post WITH a featured image (the create path + its link).
    let (status, body) = do_create(
        &state,
        &cookie,
        serde_json::json!({
            "title": "New", "slug": "new-featured",
            "block_tree": one_paragraph("body"), "featured_media": a.0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let post_id = body["id"].as_u64().expect("new id");

    let get = |cookie: &str| {
        Request::builder()
            .uri(format!("/admin/api/posts/{post_id}"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let detail = to_json(router(state.clone()).oneshot(get(&cookie)).await.unwrap()).await;
    assert_eq!(detail["featured_media"]["id"], a.0, "create must attach A");

    // Replace A -> B on save.
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/api/posts/{post_id}"))
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "title": "New", "slug": "new-featured", "status": "draft",
                        "block_tree": one_paragraph("body"), "featured_media": b.0,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = to_json(router(state.clone()).oneshot(get(&cookie)).await.unwrap()).await;
    assert_eq!(detail["featured_media"]["id"], b.0, "A must be replaced by B");

    // The to-one invariant: exactly ONE featured link remains (no A+B accumulation).
    let links = store
        .get_links(&Edge {
            type_name: TypeName::from(POST_TYPE),
            id: ObjectId(post_id),
            field: "featured_media".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(links.len(), 1, "exactly one featured link");
    assert_eq!(links[0].0, b, "the single link is B");

    // Create with a nonexistent featured id is rejected (create's up-front guard).
    let (status, _) = do_create(
        &state,
        &cookie,
        serde_json::json!({
            "title": "Bad", "slug": "bad-featured",
            "block_tree": one_paragraph("x"), "featured_media": 9_999_999,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bogus featured id must 400 on create");
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
async fn create_stamps_a_full_post_and_attributes_the_author() {
    use ferropress_core::query::Edge;

    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let jane = seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // Create a blank draft (no status → defaults to Draft).
    let (status, body) = do_create(
        &state,
        &cookie,
        serde_json::json!({
            "title": "A new dispatch",
            "slug": "a-new-dispatch",
            "block_tree": one_paragraph("first words on the sheet"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let new_id = body["id"].as_u64().expect("create returns an id");
    assert!(body["created_at"].as_i64().unwrap() > 0);

    // The post is readable, is a Draft, and carries the derived + stamped fields.
    let obj = store
        .get(&TypeName::from(POST_TYPE), ObjectId(new_id))
        .await
        .expect("get created");
    assert!(matches!(obj.get("title"), Some(Value::String(s)) if s == "A new dispatch"));
    assert!(matches!(obj.get("slug"), Some(Value::String(s)) if s == "a-new-dispatch"));
    assert!(matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Draft.as_str()));
    assert!(matches!(obj.get("post_type"), Some(Value::String(s)) if s == "post"));
    assert!(
        matches!(obj.get("uuid"), Some(Value::String(s)) if !s.is_empty()),
        "a fresh uuid must be stamped: {:?}",
        obj.get("uuid")
    );
    assert!(
        matches!(obj.get("plaintext"), Some(Value::String(s)) if s.contains("first words")),
        "plaintext must be derived on create: {:?}",
        obj.get("plaintext")
    );
    assert!(matches!(obj.get("created_at"), Some(Value::DateTime(_))));
    assert!(matches!(obj.get("updated_at"), Some(Value::DateTime(_))));

    // The post is attributed to its creator via the `author` relation.
    let links = store
        .get_links(&Edge {
            type_name: TypeName::from(POST_TYPE),
            id: ObjectId(new_id),
            field: "author".to_owned(),
        })
        .await
        .expect("get author links");
    assert!(
        links.iter().any(|(id, _)| *id == jane),
        "the new post must be linked to its author {jane:?}: {links:?}"
    );

    // It shows up in the galley list.
    let req = Request::builder()
        .uri("/admin/api/posts")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let listed = to_json(router(state).oneshot(req).await.unwrap()).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == new_id && p["slug"] == "a-new-dispatch"),
        "list must contain the created post: {listed}"
    );
}

#[tokio::test]
async fn create_accepts_an_explicit_publishable_status() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (status, body) = do_create(
        &state,
        &cookie,
        serde_json::json!({
            "title": "Straight to press",
            "slug": "straight-to-press",
            "status": "published",
            "block_tree": one_paragraph("body"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create body: {body}");
    let obj = store
        .get(
            &TypeName::from(POST_TYPE),
            ObjectId(body["id"].as_u64().unwrap()),
        )
        .await
        .expect("get");
    assert!(matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Published.as_str()));
}

#[tokio::test]
async fn create_rejects_bad_input() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    seed_post(&store, "taken-slug", Status::Published).await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());
    let good_tree = one_paragraph("x");

    // Empty slug.
    assert_eq!(
        do_create(
            &state,
            &cookie,
            serde_json::json!({"title":"t","slug":"  ","block_tree":good_tree})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // A slug another post already holds is a conflict.
    assert_eq!(
        do_create(
            &state,
            &cookie,
            serde_json::json!({"title":"t","slug":"taken-slug","block_tree":good_tree})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Unknown status.
    assert_eq!(
        do_create(
            &state,
            &cookie,
            serde_json::json!({"title":"t","slug":"ok","status":"nonsense","block_tree":good_tree})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // A status that isn't a legal birth state (private/trashed are not reachable
    // directly from a fresh draft).
    for bad in ["private", "trashed"] {
        assert_eq!(
            do_create(
                &state,
                &cookie,
                serde_json::json!({"title":"t","slug":"ok","status":bad,"block_tree":good_tree})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "status {bad:?} must be rejected at create"
        );
    }
    // Malformed block tree.
    assert_eq!(
        do_create(
            &state,
            &cookie,
            serde_json::json!({"title":"t","slug":"ok","block_tree":{"nope":1}})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn create_requires_a_session_and_the_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "sam", "passwordpassword", "subscriber").await;
    let tree = one_paragraph("x");

    // No session → 401.
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/posts")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"title":"t","slug":"nope","block_tree":tree}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        router(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // Authenticated but under-privileged → 403.
    let (_, cookie, _) = do_login(&state, "sam", "passwordpassword").await;
    let cookie = session_pair(&cookie.unwrap());
    assert_eq!(
        do_create(
            &state,
            &cookie,
            serde_json::json!({"title":"t","slug":"nope","block_tree":one_paragraph("x")})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn media_upload_then_serve_roundtrips() {
    use ferropress_core::query::Edge;

    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let jane = seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let png = tiny_png();
    let (ct, body) = multipart_image("Red Pixel.png", "image/png", &png, "a red pixel");
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "upload should succeed");
    let up = to_json(resp).await;
    let id = up["id"].as_u64().expect("upload returns an id");
    let url = up["url"].as_str().expect("upload returns a url").to_owned();
    assert!(url.starts_with("/media/"), "url is the served path: {url}");
    assert_eq!(up["mime_type"], "image/png");
    assert_eq!(up["width"], 1);
    assert_eq!(up["height"], 1);

    // The Media row carries the sniffed metadata (not the client's claims).
    let obj = store
        .get(&TypeName::from(MEDIA_TYPE), ObjectId(id))
        .await
        .expect("get media");
    assert!(matches!(obj.get("mime_type"), Some(Value::String(s)) if s == "image/png"));
    assert!(matches!(obj.get("byte_size"), Some(Value::U64(n)) if *n == png.len() as u64));
    assert!(matches!(obj.get("width"), Some(Value::U32(1))));
    assert!(matches!(obj.get("alt_text"), Some(Value::String(s)) if s == "a red pixel"));
    assert!(
        matches!(obj.get("slug"), Some(Value::String(s)) if s == "red-pixel"),
        "slug is derived from the filename stem: {:?}",
        obj.get("slug")
    );
    // The public URL is keyed by the unguessable uuid, NOT the sequential object id.
    let uuid = match obj.get("uuid") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("media uuid must be a string, got {other:?}"),
    };
    assert_eq!(url, format!("/media/{uuid}"), "url is keyed by the uuid");
    assert_ne!(
        url,
        format!("/media/{id}"),
        "url must NOT be the sequential id"
    );

    // Attributed to its uploader via the `uploaded_by` relation.
    let links = store
        .get_links(&Edge {
            type_name: TypeName::from(MEDIA_TYPE),
            id: ObjectId(id),
            field: "uploaded_by".to_owned(),
        })
        .await
        .expect("get uploader links");
    assert!(
        links.iter().any(|(uid, _)| *uid == jane),
        "media must be attributed to its uploader {jane:?}: {links:?}"
    );

    // GET /media/{uuid} is PUBLIC (no cookie) and returns the exact bytes + content-type.
    let req = Request::builder().uri(&url).body(Body::empty()).unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "image/png"
    );
    let served = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        served.as_ref(),
        png.as_slice(),
        "served bytes must equal the uploaded bytes"
    );
}

#[tokio::test]
async fn media_upload_rejects_a_non_image() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // Bytes that are NOT an image, even though the part CLAIMS image/png — the server
    // sniffs the bytes and rejects.
    let (ct, body) = multipart_image(
        "evil.png",
        "image/png",
        b"<script>definitely not an image</script>",
        "",
    );
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "content-type is not trusted; a non-image is rejected"
    );
}

#[tokio::test]
async fn media_upload_requires_a_session_and_the_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    // A subscriber authenticates fine but lacks UploadMedia (Author+).
    seed_user(&store, "sam", "passwordpassword", "subscriber").await;
    let png = tiny_png();

    // No session → 401.
    let (ct, body) = multipart_image("p.png", "image/png", &png, "");
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(
        router(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // Authenticated but under-privileged → 403.
    let (_, cookie, _) = do_login(&state, "sam", "passwordpassword").await;
    let cookie = session_pair(&cookie.unwrap());
    let (ct, body) = multipart_image("p.png", "image/png", &png, "");
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn media_upload_rejects_oversized_alt_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A valid image, but an alt string far past the cap → rejected (would otherwise
    // bloat the row + the @vectorize source).
    let png = tiny_png();
    let alt = "x".repeat(3000);
    let (ct, body) = multipart_image("p.png", "image/png", &png, &alt);
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn media_upload_rejects_a_dimension_bomb() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A tiny file that DECLARES a 30000-wide canvas — rejected before it can be stored
    // and served to public viewers (whose browsers would decode it to a huge bitmap).
    let bomb = png_header(30_000, 1);
    let (ct, body) = multipart_image("bomb.png", "image/png", &bomb, "");
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/media")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn serving_a_missing_media_id_is_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, state) = boot(tmp.path());
    let req = Request::builder()
        .uri("/media/999999")
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

// ---------------------------------------------------------------------------
// Per-author ownership scoping (EditOwnContent / Publish* gating / backfill)
// ---------------------------------------------------------------------------

/// Link `user_id` as the `author` of `post_id`. Posts made by `seed_post` are born
/// null-author (like a CLI-seeded row); give one an author to exercise ownership.
async fn set_author(store: &Arc<dyn RhypeStore>, post_id: ObjectId, user_id: ObjectId) {
    store
        .link(
            &Edge {
                type_name: TypeName::from(POST_TYPE),
                id: post_id,
                field: "author".to_owned(),
            },
            user_id,
            FieldMap::new(),
        )
        .await
        .expect("link author");
}

/// The single `author` link of `post_id`, or `None` if unattributed.
async fn author_of(store: &Arc<dyn RhypeStore>, post_id: ObjectId) -> Option<ObjectId> {
    store
        .get_links(&Edge {
            type_name: TypeName::from(POST_TYPE),
            id: post_id,
            field: "author".to_owned(),
        })
        .await
        .expect("get author links")
        .into_iter()
        .next()
        .map(|(id, _)| id)
}

/// GET /admin/api/posts/{id} → (status, body).
async fn do_get(state: &AppState, cookie: &str, id: u64) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(format!("/admin/api/posts/{id}"))
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

/// PUT /admin/api/posts/{id} (save) → (status, body).
async fn do_save(
    state: &AppState,
    cookie: &str,
    id: u64,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/admin/api/posts/{id}"))
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

/// GET /admin/api/posts → (status, body).
async fn do_list(state: &AppState, cookie: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/admin/api/posts")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

/// A save/create JSON body with the given title/slug/status and a one-paragraph body.
fn edit_body(title: &str, slug: &str, status: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "slug": slug,
        "status": status,
        "block_tree": one_paragraph(text),
    })
}

/// Whether the listing contains a row for `id`.
fn list_has(list: &serde_json::Value, id: u64) -> bool {
    list.as_array()
        .expect("list is an array")
        .iter()
        .any(|p| p["id"].as_u64() == Some(id))
}

#[tokio::test]
async fn list_is_scoped_to_own_posts_for_a_lower_role() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let jane = seed_user(&store, "jane", "hunter2hunter2", "contributor").await;
    let bob = seed_user(&store, "bob", "hunter2hunter2", "contributor").await;

    let mine = seed_post(&store, "mine", Status::Draft).await;
    set_author(&store, mine, jane).await;
    let theirs = seed_post(&store, "theirs", Status::Draft).await;
    set_author(&store, theirs, bob).await;
    let orphan = seed_post(&store, "orphan", Status::Draft).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (status, list) = do_list(&state, &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list_has(&list, mine.0), "own post must be listed: {list}");
    assert!(
        !list_has(&list, theirs.0),
        "another author's post must NOT be listed: {list}"
    );
    assert!(
        !list_has(&list, orphan.0),
        "a null-author post must NOT be listed to a lower role: {list}"
    );
}

#[tokio::test]
async fn contributor_can_edit_and_submit_own_draft_but_not_publish() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "contributor").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A contributor may create a draft (attributed to them).
    let (status, body) = do_create(
        &state,
        &cookie,
        edit_body("Draft dispatch", "draft-dispatch", "draft", "rough notes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "contributor create draft: {body}");
    let id = body["id"].as_u64().unwrap();

    // Edit the body, still a draft → OK.
    let (status, _) = do_save(
        &state,
        &cookie,
        id,
        edit_body("Draft dispatch", "draft-dispatch", "draft", "revised notes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "contributor edits own draft");

    // Submit for review (draft → pending) → OK (no publish cap needed).
    let (status, _) = do_save(
        &state,
        &cookie,
        id,
        edit_body("Draft dispatch", "draft-dispatch", "pending", "ready for review"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "contributor submits for review");

    // But publishing own content is forbidden (lacks PublishOwnContent).
    let (status, _) = do_save(
        &state,
        &cookie,
        id,
        edit_body("Draft dispatch", "draft-dispatch", "published", "trying to go live"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a contributor must not publish their own post"
    );

    // And creating a post directly in a published state is likewise forbidden.
    let (status, _) = do_create(
        &state,
        &cookie,
        edit_body("Instant", "instant", "published", "body"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a contributor must not create a published post"
    );
}

#[tokio::test]
async fn a_lower_role_cannot_read_or_edit_anothers_post() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "contributor").await;
    let bob = seed_user(&store, "bob", "hunter2hunter2", "author").await;

    let theirs = seed_post(&store, "bobs-post", Status::Draft).await;
    set_author(&store, theirs, bob).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A denial on the id endpoints is 404, not 403, so it can't be told apart from a
    // missing id — the id space of others' (unpublished) posts stays hidden.
    let (status, _) = do_get(&state, &cookie, theirs.0).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cannot open another's post");

    let (status, _) = do_save(
        &state,
        &cookie,
        theirs.0,
        edit_body("Hijack", "bobs-post", "draft", "not mine"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cannot save another's post");

    // Existence oracle is closed: a genuinely nonexistent id returns the SAME 404, so
    // the two cases are indistinguishable to a lower role.
    let (missing, _) = do_get(&state, &cookie, 999_999).await;
    assert_eq!(missing, StatusCode::NOT_FOUND, "a missing id is also 404");

    // The post is untouched: still authored by bob, original slug.
    assert_eq!(author_of(&store, theirs).await, Some(bob));
}

#[tokio::test]
async fn an_author_may_publish_their_own_content() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "amy", "hunter2hunter2", "author").await;
    let (_, cookie, _) = do_login(&state, "amy", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // Create a draft, then publish it → both OK (Author has PublishOwnContent).
    let (status, body) = do_create(
        &state,
        &cookie,
        edit_body("Amy's piece", "amys-piece", "draft", "draft body"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "author create draft: {body}");
    let id = body["id"].as_u64().unwrap();

    let (status, _) = do_save(
        &state,
        &cookie,
        id,
        edit_body("Amy's piece", "amys-piece", "published", "final body"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "author publishes own draft");

    let (_, detail) = do_get(&state, &cookie, id).await;
    assert_eq!(detail["status"], "published");

    // Creating straight into a published state is also allowed for an author.
    let (status, _) = do_create(
        &state,
        &cookie,
        edit_body("Straight to press", "straight", "published", "body"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "author creates a published post");
}

#[tokio::test]
async fn editor_backfills_a_null_author_on_open_and_on_save() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let ed = seed_user(&store, "ed", "hunter2hunter2", "editor").await;
    seed_user(&store, "jane", "hunter2hunter2", "contributor").await;

    let opened = seed_post(&store, "legacy-opened", Status::Draft).await;
    let saved = seed_post(&store, "legacy-saved", Status::Draft).await;
    assert_eq!(author_of(&store, opened).await, None, "starts null-author");

    // A contributor can neither see nor open a null-author post (a denial reads as 404,
    // hiding the orphan's existence).
    let (_, jcookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let jcookie = session_pair(&jcookie.unwrap());
    let (status, _) = do_get(&state, &jcookie, opened.0).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a contributor cannot open an orphaned post"
    );
    assert_eq!(
        author_of(&store, opened).await,
        None,
        "a denied open must NOT stamp an author"
    );

    // An editor opening it claims authorship (backfill-on-touch).
    let (_, ecookie, _) = do_login(&state, "ed", "hunter2hunter2").await;
    let ecookie = session_pair(&ecookie.unwrap());
    let (status, _) = do_get(&state, &ecookie, opened.0).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        author_of(&store, opened).await,
        Some(ed),
        "opening a null-author post backfills the opener as author"
    );

    // Saving a (different) orphan the editor never opened also backfills.
    let (status, _) = do_save(
        &state,
        &ecookie,
        saved.0,
        edit_body("Legacy saved", "legacy-saved", "draft", "edited by editor"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        author_of(&store, saved).await,
        Some(ed),
        "saving a null-author post backfills the editor as author"
    );
}

#[tokio::test]
async fn editor_edits_others_post_without_reassigning_the_author() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "ed", "hunter2hunter2", "editor").await;
    let bob = seed_user(&store, "bob", "hunter2hunter2", "contributor").await;

    let post = seed_post(&store, "bobs-work", Status::Draft).await;
    set_author(&store, post, bob).await;

    let (_, cookie, _) = do_login(&state, "ed", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // The editor may open + publish another author's post (EditOthers + PublishOthers)...
    let (status, _) = do_get(&state, &cookie, post.0).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = do_save(
        &state,
        &cookie,
        post.0,
        edit_body("Bob's work", "bobs-work", "published", "editor published it"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an editor may publish others' content");

    // ...but the author link stays with bob (backfill only fills a NULL author).
    assert_eq!(
        author_of(&store, post).await,
        Some(bob),
        "editing an attributed post must not reassign its author"
    );
}
