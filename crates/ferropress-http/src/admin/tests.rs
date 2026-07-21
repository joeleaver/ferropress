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
    Block, BlockKind, BlockTree, Edge, InlineRun, MEDIA_TYPE, PAGE_TYPE, POST_TYPE, REDIRECT_TYPE,
    SETTING_TYPE, Status, USER_TYPE,
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
    let theme = ferropress_serve::default_theme_handle().expect("theme");
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
    f.insert(
        "mime_type".to_owned(),
        Value::String("image/png".to_owned()),
    );
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

/// Link a media row's `uploaded_by` to a user (mirrors the upload handler's edge), so
/// a test can exercise the media library's authorship scoping.
async fn link_uploader(store: &Arc<dyn RhypeStore>, media_id: ObjectId, user_id: ObjectId) {
    let edge = Edge {
        type_name: TypeName::from(MEDIA_TYPE),
        id: media_id,
        field: "uploaded_by".to_owned(),
    };
    store
        .link(&edge, user_id, FieldMap::new())
        .await
        .expect("link uploaded_by");
}

/// GET /admin/api/media with a cookie; return (status, body json).
async fn do_media_list(state: &AppState, cookie: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/media")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
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
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "save with featured should succeed"
    );
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
    assert_eq!(
        detail["featured_media"]["id"], b.0,
        "A must be replaced by B"
    );

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
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "bogus featured id must 400 on create"
    );
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
        edit_body(
            "Draft dispatch",
            "draft-dispatch",
            "pending",
            "ready for review",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "contributor submits for review");

    // But publishing own content is forbidden (lacks PublishOwnContent).
    let (status, _) = do_save(
        &state,
        &cookie,
        id,
        edit_body(
            "Draft dispatch",
            "draft-dispatch",
            "published",
            "trying to go live",
        ),
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

// ---------------------------------------------------------------------------
// Site settings (GET/PUT /admin/api/settings — ManageSettings)
// ---------------------------------------------------------------------------

/// GET /admin/api/settings → (status, body).
async fn do_get_settings(state: &AppState, cookie: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/admin/api/settings")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

/// PUT /admin/api/settings with `{ "values": … }` → (status, body).
async fn do_put_settings(
    state: &AppState,
    cookie: &str,
    values: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/api/settings")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "values": values }).to_string(),
        ))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

#[tokio::test]
async fn settings_get_returns_schema_and_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (status, body) = do_get_settings(&state, &cookie).await;
    assert_eq!(status, StatusCode::OK, "settings body: {body}");
    // The declarative schema is shipped (sections with fields)...
    assert!(body["schema"]["sections"].is_array());
    assert!(
        body["schema"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "identity"),
        "schema must carry the identity section: {body}"
    );
    // ...and every key has a value seeded from its default (no rows stored yet).
    assert_eq!(body["values"]["reading.posts_per_page"], 10);
    assert_eq!(body["values"]["reading.search_engine_visible"], true);
    assert_eq!(body["values"]["site.timezone"], "UTC");
    assert_eq!(body["values"]["site.title"], "");
}

#[tokio::test]
async fn settings_put_persists_and_get_reflects() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (status, body) = do_put_settings(
        &state,
        &cookie,
        serde_json::json!({
            "site.title": "The Foundry",
            "site.url": "https://foundry.example",
            "reading.posts_per_page": 25,
            "reading.search_engine_visible": false,
            "site.date_format": "Y-m-d",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "put body: {body}");
    // The PUT response already reflects the new values (client re-syncs from it).
    assert_eq!(body["values"]["site.title"], "The Foundry");
    assert_eq!(body["values"]["reading.posts_per_page"], 25);
    assert_eq!(body["values"]["reading.search_engine_visible"], false);

    // A fresh GET reads them back from the store.
    let (_, got) = do_get_settings(&state, &cookie).await;
    assert_eq!(got["values"]["site.title"], "The Foundry");
    assert_eq!(got["values"]["site.url"], "https://foundry.example");
    assert_eq!(got["values"]["site.date_format"], "Y-m-d");

    // Directly in the store: exactly ONE Setting row per key, value JSON-encoded as
    // a String, autoload set (no duplicate rows accumulated across the two writes).
    let rows = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String("site.title".to_owned()),
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one Setting row per key, not accumulating");
    assert!(
        matches!(rows[0].get("value"), Some(Value::String(s)) if s == "\"The Foundry\""),
        "value is stored as a JSON-encoded String: {:?}",
        rows[0].get("value")
    );
    assert!(matches!(rows[0].get("autoload"), Some(Value::Bool(true))));

    // A second PUT of the same key updates in place (still one row).
    let _ = do_put_settings(
        &state,
        &cookie,
        serde_json::json!({ "site.title": "Reforged" }),
    )
    .await;
    let rows = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String("site.title".to_owned()),
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "an update must not create a second row");
}

#[tokio::test]
async fn settings_put_rejects_invalid_values() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A javascript: URL is rejected by the shared href policy → 400, nothing stored.
    let (status, _) = do_put_settings(
        &state,
        &cookie,
        serde_json::json!({ "site.url": "javascript:alert(1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unsafe URL must 400");

    // An out-of-vocabulary select value is rejected.
    let (status, _) = do_put_settings(
        &state,
        &cookie,
        serde_json::json!({ "site.timezone": "Mars/Olympus" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad option must 400");

    // Nothing was persisted for the rejected keys (a fresh GET still shows defaults).
    let (_, got) = do_get_settings(&state, &cookie).await;
    assert_eq!(got["values"]["site.url"], "");
    assert_eq!(got["values"]["site.timezone"], "UTC");
}

#[tokio::test]
async fn settings_put_ignores_unknown_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // A key the schema doesn't declare is silently dropped, not persisted.
    let (status, _) = do_put_settings(
        &state,
        &cookie,
        serde_json::json!({ "site.title": "ok", "evil.injected": "payload" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String("evil.injected".to_owned()),
            limit: Some(1),
        })
        .await
        .unwrap();
    assert!(rows.is_empty(), "an unknown key must never be persisted");
}

#[tokio::test]
async fn settings_require_manage_settings_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    // An editor is high-privilege for content but LACKS ManageSettings (admin-only).
    seed_user(&store, "ed", "hunter2hunter2", "editor").await;
    let (_, cookie, _) = do_login(&state, "ed", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (get_status, _) = do_get_settings(&state, &cookie).await;
    assert_eq!(
        get_status,
        StatusCode::FORBIDDEN,
        "editor cannot read settings"
    );
    let (put_status, _) =
        do_put_settings(&state, &cookie, serde_json::json!({ "site.title": "nope" })).await;
    assert_eq!(
        put_status,
        StatusCode::FORBIDDEN,
        "editor cannot write settings"
    );

    // No session at all → 401.
    let req = Request::builder()
        .uri("/admin/api/settings")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router(state).oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
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
        edit_body(
            "Bob's work",
            "bobs-work",
            "published",
            "editor published it",
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an editor may publish others' content"
    );

    // ...but the author link stays with bob (backfill only fills a NULL author).
    assert_eq!(
        author_of(&store, post).await,
        Some(bob),
        "editing an attributed post must not reassign its author"
    );
}

// ---------------------------------------------------------------------------
// Draft preview (GET /admin/preview/{id}) — WordPress-style new-tab preview
// ---------------------------------------------------------------------------

/// Read a response body as UTF-8 text (the preview route returns HTML, not JSON).
async fn to_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("collect body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

/// Build a `GET /admin/preview/{id}` request, with or without a session cookie.
fn preview_req(cookie: Option<&str>, id: u64) -> Request<Body> {
    let mut b = Request::builder().uri(format!("/admin/preview/{id}"));
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

/// The preview renders an UNPUBLISHED draft through the real public theme — the
/// publish gate that hides it from the public site is bypassed — carrying the
/// preview banner, a forced `noindex`, and no-store / X-Robots-Tag headers.
#[tokio::test]
async fn preview_renders_a_draft_through_the_real_theme() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;
    let post = seed_post(&store, "hidden-draft", Status::Draft).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    // The public path hides a draft (404) ...
    let public = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/hidden-draft")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        public.status(),
        StatusCode::NOT_FOUND,
        "a draft must not be publicly served"
    );

    // ... but the authenticated preview renders it.
    let resp = router(state.clone())
        .oneshot(preview_req(Some(&cookie), post.0))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the author can preview a draft"
    );
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "a preview must not be stored by a shared cache"
    );
    assert_eq!(
        resp.headers().get("x-robots-tag").unwrap(),
        "noindex, nofollow",
        "a preview must never be indexed"
    );

    let html = to_text(resp).await;
    assert!(
        html.contains("original body"),
        "the draft body must be rendered through the theme: {html}"
    );
    assert!(
        html.contains("preview-bar"),
        "the chrome must show the preview banner"
    );
    assert!(
        html.contains(r#"name="robots" content="noindex"#),
        "the preview chrome must emit a noindex meta"
    );
    assert!(
        !html.contains(r#"id="fp-comments""#),
        "the comments island must be suppressed in a draft preview"
    );
}

/// The preview route is session-guarded: no cookie → 401 (never a leaked page).
#[tokio::test]
async fn preview_requires_a_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let post = seed_post(&store, "hidden-draft", Status::Draft).await;

    let resp = router(state)
        .oneshot(preview_req(None, post.0))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A lower role previewing another author's draft gets **404** (not 403) — the same
/// existence-oracle guard as `get_one`, so it can't confirm the draft exists.
#[tokio::test]
async fn preview_of_another_authors_draft_is_404_for_a_contributor() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let jane = seed_user(&store, "jane", "hunter2hunter2", "contributor").await;
    let bob = seed_user(&store, "bob", "hunter2hunter2", "contributor").await;
    let _ = jane;
    let theirs = seed_post(&store, "bobs-draft", Status::Draft).await;
    set_author(&store, theirs, bob).await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let resp = router(state)
        .oneshot(preview_req(Some(&cookie), theirs.0))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a contributor must not preview another author's draft"
    );
}

/// Previewing a non-existent post id is a clean 404.
#[tokio::test]
async fn preview_of_a_missing_post_is_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    seed_user(&store, "jane", "hunter2hunter2", "administrator").await;

    let (_, cookie, _) = do_login(&state, "jane", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let resp = router(state)
        .oneshot(preview_req(Some(&cookie), 999_999))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Plugin config (GET /admin/api/plugins, GET/PUT /admin/api/plugins/{id}/settings
// — ManagePlugins). Uses a fake PluginCatalog so the tests need no built wasm.
// ---------------------------------------------------------------------------

/// A `PluginCatalog` double: one configurable plugin (`demo`) and one that ships no
/// settings (`plain`).
struct FakeCatalog;

impl ferropress_render_form::PluginCatalog for FakeCatalog {
    fn plugins(&self) -> Vec<ferropress_render_form::PluginDescriptor> {
        // Intentionally NOT name-sorted, to prove the handler sorts.
        vec![
            ferropress_render_form::PluginDescriptor {
                id: "plain".to_owned(),
                name: "Plain".to_owned(),
                has_settings: false,
            },
            ferropress_render_form::PluginDescriptor {
                id: "demo".to_owned(),
                name: "Demo".to_owned(),
                has_settings: true,
            },
        ]
    }

    fn settings_schema(&self, id: &str) -> Option<ferropress_render_form::FormSchema> {
        (id == "demo").then(demo_schema)
    }
}

/// A compact demo schema exercising a Select (vocabulary gate) and a bounded Number
/// (clamp), mirroring what a real plugin ships.
fn demo_schema() -> ferropress_render_form::FormSchema {
    use ferropress_render_form::{Choice, Field, FormSchema, FormSection, WidgetKind};
    FormSchema {
        sections: vec![FormSection {
            id: "appearance".to_owned(),
            title: "Appearance".to_owned(),
            help: None,
            fields: vec![
                Field {
                    key: "variant".to_owned(),
                    label: "Variant".to_owned(),
                    help: None,
                    default: serde_json::json!("info"),
                    widget: WidgetKind::Select {
                        options: vec![
                            Choice {
                                value: "info".to_owned(),
                                label: "Info".to_owned(),
                            },
                            Choice {
                                value: "warn".to_owned(),
                                label: "Warn".to_owned(),
                            },
                        ],
                    },
                    visible_when: None,
                },
                Field {
                    key: "count".to_owned(),
                    label: "Count".to_owned(),
                    help: None,
                    default: serde_json::json!(3),
                    widget: WidgetKind::Number {
                        min: Some(1.0),
                        max: Some(10.0),
                        step: Some(1.0),
                        unit: None,
                    },
                    visible_when: None,
                },
            ],
        }],
    }
}

fn boot_with_plugins(dir: &Path) -> (Arc<dyn RhypeStore>, AppState) {
    let (store, state) = boot(dir);
    (store, state.with_plugin_catalog(Arc::new(FakeCatalog)))
}

async fn admin_cookie(state: &AppState, store: &Arc<dyn RhypeStore>) -> String {
    seed_user(store, "jane", "hunter2hunter2", "administrator").await;
    let (_, cookie, _) = do_login(state, "jane", "hunter2hunter2").await;
    session_pair(&cookie.unwrap())
}

async fn get_uri(state: &AppState, uri: &str, cookie: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

async fn do_put_plugin_settings(
    state: &AppState,
    id: &str,
    cookie: &str,
    values: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/admin/api/plugins/{id}/settings"))
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "values": values }).to_string(),
        ))
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

#[tokio::test]
async fn plugins_list_returns_sorted_and_gated() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot_with_plugins(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    let (status, body) = get_uri(&state, "/admin/api/plugins", &cookie).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let arr = body.as_array().expect("array");
    // Sorted by name: Demo before Plain.
    assert_eq!(arr[0]["id"], "demo");
    assert_eq!(arr[0]["name"], "Demo");
    assert_eq!(arr[0]["has_settings"], true);
    assert_eq!(arr[1]["id"], "plain");
    assert_eq!(arr[1]["has_settings"], false);

    // No session → 401.
    let (status, _) = get_uri(&state, "/admin/api/plugins", "session=bogus").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn plugin_settings_get_schema_defaults_and_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot_with_plugins(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    let (status, body) = get_uri(&state, "/admin/api/plugins/demo/settings", &cookie).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["schema"]["sections"].is_array());
    // Values are the schema defaults (no rows stored yet).
    assert_eq!(body["values"]["variant"], "info");
    assert_eq!(body["values"]["count"], 3);

    // An unknown plugin id → 404 (before any store access).
    let (status, _) = get_uri(&state, "/admin/api/plugins/nope/settings", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A plugin that ships no settings form → 404.
    let (status, _) = get_uri(&state, "/admin/api/plugins/plain/settings", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_settings_put_persists_namespaced_reflects_and_coerces() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot_with_plugins(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // A valid select value + an out-of-range number (clamped to max 10).
    let (status, body) = do_put_plugin_settings(
        &state,
        "demo",
        &cookie,
        serde_json::json!({ "variant": "warn", "count": 99 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["values"]["variant"], "warn");
    assert_eq!(body["values"]["count"], 10, "number clamped to max");

    // Persisted under the host-owned namespace `plugin.demo.*` — NOT the bare key,
    // and NOT any `site.*` key.
    let namespaced = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String(ferropress_core::entity::plugin_setting_key(
                "demo", "variant",
            )),
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(namespaced.len(), 1, "one namespaced row");
    assert!(
        matches!(namespaced[0].get("value"), Some(Value::String(s)) if s == "\"warn\""),
        "value JSON-encoded under the plugin namespace: {:?}",
        namespaced[0].get("value")
    );
    // No BARE `variant` row leaked into the global namespace.
    let bare = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String("variant".to_owned()),
            limit: Some(10),
        })
        .await
        .unwrap();
    assert!(
        bare.is_empty(),
        "a bare (un-namespaced) key must never be written"
    );

    // A fresh GET reflects the stored, namespaced values.
    let (_, got) = get_uri(&state, "/admin/api/plugins/demo/settings", &cookie).await;
    assert_eq!(got["values"]["variant"], "warn");
    assert_eq!(got["values"]["count"], 10);
}

#[tokio::test]
async fn plugin_settings_put_rejects_bad_and_ignores_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot_with_plugins(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // Out-of-vocabulary select value → 400, nothing stored.
    let (status, _) = do_put_plugin_settings(
        &state,
        "demo",
        &cookie,
        serde_json::json!({ "variant": "bogus" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown key is dropped (schema is the whitelist); the valid key still applies.
    let (status, body) = do_put_plugin_settings(
        &state,
        "demo",
        &cookie,
        serde_json::json!({ "variant": "warn", "evil.rce": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["values"]["variant"], "warn");
    // Nothing persisted for the unknown key under the plugin namespace.
    let evil = store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(SETTING_TYPE),
            field: "key".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String(ferropress_core::entity::plugin_setting_key(
                "demo", "evil.rce",
            )),
            limit: Some(10),
        })
        .await
        .unwrap();
    assert!(evil.is_empty(), "unknown keys must never be persisted");

    // Writing to an unknown plugin id → 404 (no write path for an un-cataloged id).
    let (status, _) = do_put_plugin_settings(
        &state,
        "nope",
        &cookie,
        serde_json::json!({ "variant": "warn" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_config_requires_manage_plugins_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot_with_plugins(tmp.path());
    // An editor is high-privilege for content but LACKS ManagePlugins (admin-only).
    seed_user(&store, "ed", "hunter2hunter2", "editor").await;
    let (_, cookie, _) = do_login(&state, "ed", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());

    let (status, _) = get_uri(&state, "/admin/api/plugins", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "list gated on ManagePlugins");

    let (status, _) = get_uri(&state, "/admin/api/plugins/demo/settings", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "get gated on ManagePlugins");

    let (status, _) = do_put_plugin_settings(
        &state,
        "demo",
        &cookie,
        serde_json::json!({ "variant": "warn" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "put gated on ManagePlugins");
}

/// The media library is authorship-scoped like the post list: an Author sees only the
/// media they uploaded, while an Editor (`EditOthersContent`) sees the whole library.
#[tokio::test]
async fn media_list_is_scoped_to_the_uploader_for_authors() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());

    let author_a = seed_user(&store, "aya", "passwordpassword", "author").await;
    let author_b = seed_user(&store, "ben", "passwordpassword", "author").await;
    seed_user(&store, "edi", "passwordpassword", "editor").await;

    let media_a = seed_media(&store, "aaaaaaaa").await;
    let media_b = seed_media(&store, "bbbbbbbb").await;
    link_uploader(&store, media_a, author_a).await;
    link_uploader(&store, media_b, author_b).await;

    // Author A sees ONLY their own upload.
    let (_, cookie_a, _) = do_login(&state, "aya", "passwordpassword").await;
    let (status, body) = do_media_list(&state, &session_pair(&cookie_a.unwrap())).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![media_a.0],
        "an author must see only their own uploaded media"
    );

    // The editor sees the WHOLE library.
    let (_, cookie_e, _) = do_login(&state, "edi", "passwordpassword").await;
    let (status, body) = do_media_list(&state, &session_pair(&cookie_e.unwrap())).await;
    assert_eq!(status, StatusCode::OK);
    let mut ids: Vec<u64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    ids.sort_unstable();
    let mut want = vec![media_a.0, media_b.0];
    want.sort_unstable();
    assert_eq!(ids, want, "an editor must see the whole media library");
}

// ---------------------------------------------------------------------------
// Pages: hierarchy, cascade + redirects, cross-entity uniqueness, preview
// ---------------------------------------------------------------------------

/// Drive one admin request (optional JSON body) with a cookie; return (status, body json).
async fn req(
    state: &AppState,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    let body = match body {
        Some(j) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from(j.to_string())
        }
        None => Body::empty(),
    };
    let resp = router(state.clone())
        .oneshot(b.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, to_json(resp).await)
}

/// Create a page via the API; return its id (panics on non-200 with the body).
async fn create_page(
    state: &AppState,
    cookie: &str,
    slug: &str,
    parent: Option<u64>,
    status: &str,
) -> u64 {
    let (code, body) = req(
        state,
        "POST",
        "/admin/api/pages",
        cookie,
        Some(serde_json::json!({
            "title": format!("Page {slug}"),
            "slug": slug,
            "status": status,
            "block_tree": one_paragraph("body"),
            "parent": parent,
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "create page {slug}: {body}");
    body["id"].as_u64().expect("create returns an id")
}

/// The stored `path` scalar of a page.
async fn page_path(store: &Arc<dyn RhypeStore>, id: u64) -> String {
    match store
        .get(&TypeName::from(PAGE_TYPE), ObjectId(id))
        .await
        .expect("get page")
        .get("path")
    {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// All `(from_path, to_path)` redirect rows in the store.
async fn all_redirects(store: &Arc<dyn RhypeStore>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = store
        .scan(&TypeName::from(REDIRECT_TYPE))
        .await
        .expect("scan redirects")
        .iter()
        .filter_map(|o| {
            let from = match o.get("from_path") {
                Some(Value::String(s)) => s.clone(),
                _ => return None,
            };
            let to = match o.get("to_path") {
                Some(Value::String(s)) => s.clone(),
                _ => return None,
            };
            Some((from, to))
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn page_create_get_save_roundtrip_and_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // A top-level page: path == slug.
    let about = create_page(&state, &cookie, "about", None, "published").await;
    assert_eq!(page_path(&store, about).await, "about");

    // A child: path == parent/child.
    let team = create_page(&state, &cookie, "team", Some(about), "published").await;
    assert_eq!(page_path(&store, team).await, "about/team");

    // GET the child back with its hierarchy meta.
    let (code, detail) = req(
        &state,
        "GET",
        &format!("/admin/api/pages/{team}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(detail["path"], "about/team");
    assert_eq!(detail["parent"], about);

    // Save an edit that doesn't move the page (title only) — path unchanged, no redirect.
    let (code, _) = req(
        &state,
        "PUT",
        &format!("/admin/api/pages/{team}"),
        &cookie,
        Some(serde_json::json!({
            "title": "The Team, Renamed",
            "slug": "team",
            "status": "published",
            "block_tree": one_paragraph("body"),
            "parent": about,
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(page_path(&store, team).await, "about/team");
    assert!(
        all_redirects(&store).await.is_empty(),
        "a non-move save writes no redirect"
    );
}

#[tokio::test]
async fn renaming_a_parent_cascades_descendant_paths_and_records_redirects() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // about -> team -> history (three deep), all published.
    let about = create_page(&state, &cookie, "about", None, "published").await;
    let team = create_page(&state, &cookie, "team", Some(about), "published").await;
    let history = create_page(&state, &cookie, "history", Some(team), "published").await;
    assert_eq!(page_path(&store, history).await, "about/team/history");

    // Rename the ROOT about -> company. Descendants must re-path.
    let (code, body) = req(
        &state,
        "PUT",
        &format!("/admin/api/pages/{about}"),
        &cookie,
        Some(serde_json::json!({
            "title": "Company",
            "slug": "company",
            "status": "published",
            "block_tree": one_paragraph("body"),
            "parent": null,
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "rename: {body}");
    assert_eq!(body["path"], "company");
    assert_eq!(page_path(&store, team).await, "company/team");
    assert_eq!(page_path(&store, history).await, "company/team/history");

    // A 301 is recorded for the root AND each descendant's old→new path.
    let redirects = all_redirects(&store).await;
    assert!(
        redirects.contains(&("/about".to_owned(), "/company".to_owned())),
        "{redirects:?}"
    );
    assert!(
        redirects.contains(&("/about/team".to_owned(), "/company/team".to_owned())),
        "{redirects:?}"
    );
    assert!(
        redirects.contains(&(
            "/about/team/history".to_owned(),
            "/company/team/history".to_owned()
        )),
        "{redirects:?}"
    );
}

#[tokio::test]
async fn self_parent_and_cycle_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    let a = create_page(&state, &cookie, "a", None, "draft").await;
    let b = create_page(&state, &cookie, "b", Some(a), "draft").await;

    // Self-parent → 400.
    let (code, _) = req(
        &state,
        "PUT",
        &format!("/admin/api/pages/{a}"),
        &cookie,
        Some(serde_json::json!({
            "title": "A", "slug": "a", "status": "draft",
            "block_tree": one_paragraph("x"), "parent": a,
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "a page can't be its own parent"
    );

    // Re-parent a under its own descendant b → cycle → 400.
    let (code, _) = req(
        &state,
        "PUT",
        &format!("/admin/api/pages/{a}"),
        &cookie,
        Some(serde_json::json!({
            "title": "A", "slug": "a", "status": "draft",
            "block_tree": one_paragraph("x"), "parent": b,
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "re-parenting into a cycle is rejected"
    );
}

#[tokio::test]
async fn slug_charset_and_template_are_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // A slug with a '/' forges hierarchy → 400.
    let (code, _) = req(
        &state,
        "POST",
        "/admin/api/pages",
        &cookie,
        Some(serde_json::json!({
            "title": "X", "slug": "a/b", "status": "draft", "block_tree": one_paragraph("x"),
        })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "a slug with '/' is rejected");

    // An unknown template → 400.
    let (code, _) = req(
        &state,
        "POST",
        "/admin/api/pages",
        &cookie,
        Some(serde_json::json!({
            "title": "X", "slug": "good", "status": "draft",
            "block_tree": one_paragraph("x"), "template": "no-such-template",
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "an unknown template is rejected"
    );

    // The registered full-width template is accepted + stored.
    let (code, body) = req(
        &state,
        "POST",
        "/admin/api/pages",
        &cookie,
        Some(serde_json::json!({
            "title": "Wide", "slug": "wide", "status": "draft",
            "block_tree": one_paragraph("x"), "template": "page-wide",
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::OK,
        "a registered template is accepted: {body}"
    );
    let id = body["id"].as_u64().unwrap();
    let (_, detail) = req(
        &state,
        "GET",
        &format!("/admin/api/pages/{id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(detail["template"], "page-wide");
}

#[tokio::test]
async fn cross_entity_slug_and_path_collisions_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;

    // A post already owns the slug "team".
    seed_post(&store, "team", Status::Published).await;
    // Creating a top-level PAGE at path "team" collides → 409.
    let (code, _) = req(
        &state,
        "POST",
        "/admin/api/pages",
        &cookie,
        Some(serde_json::json!({
            "title": "Team", "slug": "team", "status": "draft", "block_tree": one_paragraph("x"),
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "a page path can't collide with a post slug"
    );

    // A page owns path "docs". Creating a POST with slug "docs" collides → 409 (symmetric).
    create_page(&state, &cookie, "docs", None, "published").await;
    let (code, _) = req(
        &state,
        "POST",
        "/admin/api/posts",
        &cookie,
        Some(serde_json::json!({
            "title": "Docs", "slug": "docs", "block_tree": one_paragraph("x"),
        })),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "a post slug can't collide with a page path"
    );
}

#[tokio::test]
async fn page_access_is_404_for_a_non_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let admin = admin_cookie(&state, &store).await;
    // The admin creates a page.
    let page = create_page(&state, &admin, "secret", None, "draft").await;

    // A contributor (edits only their OWN content) must get 404 — not 403 — on someone else's
    // page, so the id endpoint isn't an existence oracle.
    seed_user(&store, "carol", "hunter2hunter2", "contributor").await;
    let (_, cookie, _) = do_login(&state, "carol", "hunter2hunter2").await;
    let cookie = session_pair(&cookie.unwrap());
    let (code, _) = req(
        &state,
        "GET",
        &format!("/admin/api/pages/{page}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "a non-owner sees 404, not 403");
}

#[tokio::test]
async fn page_preview_renders_a_draft_uncached_and_noindex() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, state) = boot(tmp.path());
    let cookie = admin_cookie(&state, &store).await;
    let page = create_page(&state, &cookie, "draft-page", None, "draft").await;

    let req_p = Request::builder()
        .method("GET")
        .uri(format!("/admin/preview/page/{page}"))
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req_p).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "a draft page previews");
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(
        resp.headers().get("X-Robots-Tag").unwrap(),
        "noindex, nofollow"
    );
    let html = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains("<!doctype html>"),
        "the real theme chrome renders"
    );
    assert!(html.contains("Preview"), "the preview banner shows");
}
