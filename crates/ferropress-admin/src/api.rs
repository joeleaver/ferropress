//! The admin API wire types + `gloo-net` fetch helpers.
//!
//! The DTOs MIRROR the JSON the admin API emits/accepts in
//! `ferropress-http/src/admin/` (the same source-of-truth-by-mirroring the islands
//! use). serde ignores unknown fields, so the client reads only what it renders.
//!
//! Every request is same-origin (`/admin/api/*`, served under the same origin as the
//! SPA at `/admin`) and carries `credentials: same-origin` so the HttpOnly
//! `fp_session` cookie rides along — the wasm can never read that cookie, the
//! browser attaches it. The guarded data endpoints return an [`ApiError`] that
//! distinguishes a 401 (expired/revoked session → re-login) from a message error;
//! where the server sends a `{ "error": … }` body (400 bad status/slug, 409 slug
//! clash) that message is surfaced verbatim so the editor can show it.

use serde::{Deserialize, Serialize};
use web_sys::{FormData, RequestCredentials};

use gloo_net::http::{Request, Response};

/// The signed-in user (safe public shape; never the password hash).
#[derive(Clone, PartialEq, Deserialize)]
pub struct UserDto {
    pub id: u64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub role: String,
}

/// `{ user }` — the login / `me` response envelope.
#[derive(Deserialize)]
struct SessionResponse {
    user: UserDto,
}

/// A post's featured image: the Media `id` (echoed back on save to set the relation)
/// and its public `url` (for the thumbnail).
#[derive(Clone, PartialEq, Deserialize)]
pub struct FeaturedMedia {
    pub id: u64,
    #[serde(default)]
    pub url: String,
}

/// One term (category/tag) assigned to a post, resolved for the editor: the
/// assignment panel's current-selection seed AND the chip/checklist label — mirrors
/// `ferropress-http::admin::posts::TermRefDto`. `taxonomy` is the owning taxonomy's
/// KEY, so a pooled `PostDetail.terms` (all taxonomies together, matching how the
/// server stores membership) can be split back out per-panel client-side.
#[derive(Clone, PartialEq, Deserialize)]
pub struct TermRefDto {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub taxonomy: String,
}

/// An inline new-tag request riding a post save/create (WP's "add new tag" box):
/// create-or-reuse a root term in a FLAT taxonomy and assign it. Mirrors
/// `ferropress-http::admin::posts::NewTermRequest`. Tags-only server-side (a
/// hierarchical taxonomy 400s) — see the ADMIN+AUTH pinned design.
#[derive(Serialize)]
pub struct NewTermRequest {
    /// The FLAT taxonomy's key (e.g. `"tag"`).
    pub taxonomy: String,
    pub name: String,
}

/// One row of `GET /admin/api/posts`.
#[derive(Clone, PartialEq, Deserialize)]
pub struct PostSummary {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub status: String,
    /// Last-touched instant, epoch millis (or `None`).
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// The featured image, for the galley-row thumbnail.
    #[serde(default)]
    pub featured_media: Option<FeaturedMedia>,
}

/// `GET /admin/api/posts/{id}` — a post with its body, for the editor to load.
#[derive(Deserialize)]
pub struct PostDetail {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub status: String,
    /// The canonical Ferropress `BlockTree` JSON (the bridge converts it editor-side).
    #[serde(default)]
    pub block_tree: serde_json::Value,
    /// The featured image, shown in the editor's featured-image control.
    #[serde(default)]
    pub featured_media: Option<FeaturedMedia>,
    /// The post's currently assigned terms, pooled across every taxonomy (mirrors
    /// how the server stores membership) — the assignment panels' initial seed.
    #[serde(default)]
    pub terms: Vec<TermRefDto>,
}

#[derive(Serialize)]
struct LoginBody<'a> {
    username: &'a str,
    password: &'a str,
}

/// `PUT /admin/api/posts/{id}` body.
#[derive(Serialize)]
pub struct SaveRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    /// The featured image's Media id, or `None` to clear it.
    pub featured_media: Option<u64>,
    /// Desired term memberships (term ids), SF1 "dirty-send" — build this with
    /// [`dirty_terms`]. `None` = LEAVE UNCHANGED (an absent/unchanged field must
    /// never clear a post's categories/tags); `Some(v)` set-reconciles the
    /// membership to exactly `v` (plus any `new_terms`); `Some([])` is the
    /// deliberate "clear everything" signal. Mirrors
    /// `ferropress-http::admin::posts::SaveRequest::terms` exactly.
    pub terms: Option<Vec<u64>>,
    /// Inline new tags to create-or-reuse and assign, ADDITIVE on top of `terms` (or
    /// the current membership when `terms` is `None`). Flat taxonomies only.
    pub new_terms: Option<Vec<NewTermRequest>>,
}

/// `POST /admin/api/posts` body — create a new post. Same shape as [`SaveRequest`]
/// (the server derives the rest: `uuid`, `plaintext`, timestamps, `author`).
#[derive(Serialize)]
pub struct CreateRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    /// An optional featured image (Media id) for the new post.
    pub featured_media: Option<u64>,
    /// Initial term memberships (term ids). Same contract as [`SaveRequest::terms`];
    /// on create, `None` simply means "no terms".
    pub terms: Option<Vec<u64>>,
    /// Inline new tags to create-or-reuse and assign (see [`NewTermRequest`]).
    pub new_terms: Option<Vec<NewTermRequest>>,
}

/// `{ id }` from a successful page create (the rest of the response is ignored). Kept
/// distinct from [`PostCreateResponse`] — a page has no `terms`, so widening this
/// shared shape with a defaulted `terms` field would let a page create silently
/// deserialize into a post-shaped path.
#[derive(Deserialize)]
struct CreateResponse {
    id: u64,
}

/// `PUT /admin/api/posts/{id}` response (SF4): the AUTHORITATIVE terms after the
/// save's reconcile — an inline-created tag comes back with its real id, so the
/// editor re-seeds its assignment panel from this rather than trusting what it sent.
/// Mirrors `ferropress-http::admin::posts::SaveResponse`; `updated_at` is on the wire
/// too but unread here (the module doc's "the client reads only what it renders").
#[derive(Deserialize)]
pub struct PostSaveResponse {
    pub id: u64,
    #[serde(default)]
    pub terms: Vec<TermRefDto>,
}

/// `POST /admin/api/posts` response (SF4): the newly assigned id + the AUTHORITATIVE
/// terms after the create's reconcile. See [`CreateResponse`] for why this is NOT the
/// shared page-create shape.
#[derive(Deserialize)]
pub struct PostCreateResponse {
    pub id: u64,
    #[serde(default)]
    pub terms: Vec<TermRefDto>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
}

/// Outcome of the boot `me` check / a `login`, distinguishing "not signed in" (401)
/// from a real error so the SPA can route to the login view rather than an error.
pub enum Auth {
    /// Signed in as this user.
    User(UserDto),
    /// 401 — no / invalid session (show the login view).
    Anonymous,
    /// A network or unexpected server error (message for the console/UI).
    Error(String),
}

/// `GET /admin/api/me` — who the current session belongs to. Used on boot to choose
/// the login vs list view.
pub async fn me() -> Auth {
    match Request::get("/admin/api/me")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
    {
        Ok(resp) if resp.ok() => match resp.json::<SessionResponse>().await {
            Ok(s) => Auth::User(s.user),
            Err(e) => Auth::Error(e.to_string()),
        },
        Ok(resp) if resp.status() == 401 => Auth::Anonymous,
        Ok(resp) => Auth::Error(error_message(resp).await),
        Err(e) => Auth::Error(e.to_string()),
    }
}

/// `POST /admin/api/login` — verify credentials; on success the server sets the
/// session cookie. A 401 is bad credentials (a uniform, enumeration-safe failure).
pub async fn login(username: &str, password: &str) -> Auth {
    let body = LoginBody { username, password };
    let built = Request::post("/admin/api/login")
        .credentials(RequestCredentials::SameOrigin)
        .json(&body);
    let req = match built {
        Ok(req) => req,
        Err(e) => return Auth::Error(e.to_string()),
    };
    match req.send().await {
        Ok(resp) if resp.ok() => match resp.json::<SessionResponse>().await {
            Ok(s) => Auth::User(s.user),
            Err(e) => Auth::Error(e.to_string()),
        },
        Ok(resp) if resp.status() == 401 => Auth::Anonymous,
        Ok(resp) => Auth::Error(error_message(resp).await),
        Err(e) => Auth::Error(e.to_string()),
    }
}

/// `POST /admin/api/logout` — clear the session cookie.
pub async fn logout() -> Result<(), String> {
    let resp = Request::post("/admin/api/logout")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(error_message(resp).await);
    }
    Ok(())
}

/// A failed guarded-endpoint request. Distinguishes an expired/revoked session (401)
/// so the SPA routes back to login instead of reporting a false outage — the same
/// distinction boot [`me`] makes, applied to the data endpoints too.
pub enum ApiError {
    /// 401 — the session is no longer valid; the caller should re-authenticate.
    Unauthorized,
    /// Any other failure, with a message (the server's `{ error }` body when present).
    Message(String),
}

/// SF3 remnant: a `.send()` failure means the REQUEST never reached the server
/// (DNS, connection refused, offline, CORS) — never a status the server chose,
/// which always goes through [`classify`] instead. The raw JS error
/// (`e.to_string()`, e.g. `"TypeError: Failed to fetch"`) is not something a
/// user can act on, so every send-failure site uses this fixed, human message
/// instead of surfacing it verbatim. Deliberately NOT applied to a `.json()`
/// parse failure (a malformed response body) or a request-BODY construction
/// failure (`.json(&body)`/`.body(form)`) — both indicate an actual bug, not a
/// connectivity problem, and stay as `{e}` for that signal.
const NETWORK_ERROR: &str =
    "Couldn't reach the server \u{2014} check your connection and try again.";

/// Classify a non-2xx response: a 401 is a session problem, anything else carries
/// its message.
async fn classify(resp: Response) -> ApiError {
    if resp.status() == 401 {
        ApiError::Unauthorized
    } else {
        ApiError::Message(error_message(resp).await)
    }
}

/// `GET /admin/api/posts` — every non-trashed post, most-recently-touched first.
pub async fn list_posts() -> Result<Vec<PostSummary>, ApiError> {
    let resp = Request::get("/admin/api/posts")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<PostSummary>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/posts/{id}` — one post with its body.
pub async fn get_post(id: u64) -> Result<PostDetail, ApiError> {
    let resp = Request::get(&format!("/admin/api/posts/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<PostDetail>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/posts/{id}` — persist an edit; returns the AUTHORITATIVE post
/// (SF4) the editor's assignment panel re-seeds from. On failure the server's
/// `{ error }` message (e.g. a 409 slug clash, a 400 bad status transition) is
/// surfaced verbatim; a 401 routes back to login.
pub async fn save_post(id: u64, body: &SaveRequest) -> Result<PostSaveResponse, ApiError> {
    let built = Request::put(&format!("/admin/api/posts/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<PostSaveResponse>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `POST /admin/api/posts` — create a new post; returns its new id + the
/// AUTHORITATIVE terms (SF4). On failure the server's `{ error }` message (a 409
/// slug clash, a 400 bad slug/status) is surfaced verbatim; a 401 routes back to
/// login.
pub async fn create_post(body: &CreateRequest) -> Result<PostCreateResponse, ApiError> {
    let built = Request::post("/admin/api/posts")
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<PostCreateResponse>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

// ── taxonomies + terms ──────────────────────────────────────────────────────────
//
// Mirrors `ferropress-http::admin::terms`. Reads (`list_taxonomies`, `list_terms`)
// are any editing role; writes (`create_term`/`update_term`/`delete_term`) are
// Editor+ (`ManageTerms`) — the post editor's own inline TAG creation does not go
// through these (it rides `SaveRequest::new_terms`/`CreateRequest::new_terms`
// instead, gated by the post-edit permission, never `ManageTerms`).

/// One taxonomy, for the assignment panels + the term-management screen. Mirrors
/// `ferropress-http::admin::terms::TaxonomyDto`. (`Default` only to satisfy the
/// rinch `#[component]` macro — `TaxonomyPanel` takes it as a prop.)
#[derive(Clone, PartialEq, Default, Deserialize, Debug)]
pub struct TaxonomyDto {
    pub id: u64,
    /// Stable key (`"category"`, `"tag"`) — the archive URL base + the API handle.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub hierarchical: bool,
    #[serde(default)]
    pub multiple: bool,
}

/// One term row, flat-with-depth (the client renders the hierarchy by indenting
/// `depth`, the pages/menus idiom). Mirrors `ferropress-http::admin::terms::TermDto`.
#[derive(Clone, PartialEq, Deserialize)]
pub struct TermDto {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The parent term's id (`None` = a root term).
    #[serde(default)]
    pub parent: Option<u64>,
    /// Depth in the tree (root = 0) — the indent the checklist/list renders.
    #[serde(default)]
    pub depth: usize,
    /// DIRECT published-post assignments (see the server doc comment for why this
    /// is never "posts affected by deleting this term").
    #[serde(default)]
    pub count: usize,
    /// `meta._rev` (SF12) — a TermEditor form should snapshot this on load and echo
    /// it back as [`update_term`]'s `expected_rev` on save.
    #[serde(default)]
    pub rev: i64,
}

/// `GET /admin/api/terms` response: the taxonomy's terms plus a truncation flag
/// (mirrors [`LinkCandidates::posts_truncated`]).
#[derive(Clone, Deserialize)]
pub struct ListTermsResponse {
    #[serde(default)]
    pub terms: Vec<TermDto>,
    /// Only ever `true` for a `limit`-bounded call (`list_terms`'s own doc
    /// comment — an unbounded call always returns the whole vocabulary,
    /// never partial). The ONE bounded caller today, the tag token-input's
    /// suggestion combobox (`load_suggestions`), does not yet surface this —
    /// a search matching more than its 8-item cap silently shows only the
    /// first 8 with no "keep typing to narrow" affordance (unlike the
    /// link-candidate picker's `posts_truncated`, which IS surfaced). A real,
    /// tracked UX gap, left for Inc-3's adversarial review (the S2 tag-input
    /// combobox owns the keyboard-nav state this would need to thread
    /// through, and reworking that is out of S3's scope) rather than papered
    /// over by dropping the field the server correctly sends.
    #[serde(default)]
    #[allow(dead_code)]
    pub truncated: bool,
}

/// The created/updated term echoed back to the client. Mirrors
/// `ferropress-http::admin::terms::TermRef`.
#[derive(Clone, PartialEq, Deserialize)]
pub struct TermRef {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parent: Option<u64>,
    /// `meta._rev` AFTER this write (SF12) — refresh the client's local snapshot
    /// with this rather than re-fetching, so an immediate second edit still carries
    /// a live precondition.
    #[serde(default)]
    pub rev: i64,
}

#[derive(Serialize)]
struct CreateTermBody<'a> {
    taxonomy: &'a str,
    name: &'a str,
    slug: Option<&'a str>,
    description: &'a str,
    parent: Option<u64>,
}

#[derive(Serialize)]
struct UpdateTermBody<'a> {
    name: &'a str,
    slug: Option<&'a str>,
    description: &'a str,
    parent: Option<u64>,
    /// SF12 optimistic-concurrency precondition — see [`TermDto::rev`].
    expected_rev: Option<i64>,
}

/// `GET /admin/api/taxonomies` — every taxonomy, key-sorted. Any editing role.
pub async fn list_taxonomies() -> Result<Vec<TaxonomyDto>, ApiError> {
    let resp = Request::get("/admin/api/taxonomies")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<TaxonomyDto>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/terms?taxonomy={key}` — a taxonomy's terms, flat-with-depth.
/// `counts: false` funds the post editor's cheap `?counts=0` mode (the management
/// screen, which shows the count column, passes `true`); `q`/`limit` fund bounded
/// suggestions — `q: None, limit: None` returns the whole vocabulary, unbounded.
pub async fn list_terms(
    taxonomy: &str,
    counts: bool,
    q: Option<&str>,
    limit: Option<usize>,
) -> Result<ListTermsResponse, ApiError> {
    let mut url = format!("/admin/api/terms?taxonomy={}", encode_query(taxonomy));
    if !counts {
        url.push_str("&counts=0");
    }
    if let Some(needle) = q.map(str::trim).filter(|s| !s.is_empty()) {
        url.push_str(&format!("&q={}", encode_query(needle)));
    }
    if let Some(limit) = limit {
        url.push_str(&format!("&limit={limit}"));
    }
    let resp = Request::get(&url)
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<ListTermsResponse>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `POST /admin/api/terms` — create a term (`ManageTerms`). `slug: None` derives one
/// from `name`. On failure the server's `{ error }` message (400 empty name/bad
/// parent/cycle/depth, 409 sibling-slug or archive-path clash) is surfaced verbatim;
/// a 401 routes back to login.
pub async fn create_term(
    taxonomy: &str,
    name: &str,
    slug: Option<&str>,
    description: &str,
    parent: Option<u64>,
) -> Result<TermRef, ApiError> {
    let built = Request::post("/admin/api/terms")
        .credentials(RequestCredentials::SameOrigin)
        .json(&CreateTermBody {
            taxonomy,
            name,
            slug,
            description,
            parent,
        });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<TermRef>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/terms/{id}` — update a term to the full desired state
/// (`ManageTerms`). `expected_rev` is the SF12 lost-update guard: `Some` and stale →
/// the server 409s the whole write rather than silently reverting a rename/re-parent
/// that happened elsewhere; `None` skips the check. On failure the server's message
/// (400/409, including the 409 the stale-rev guard produces) is surfaced verbatim; a
/// 401 routes back to login.
pub async fn update_term(
    id: u64,
    name: &str,
    slug: Option<&str>,
    description: &str,
    parent: Option<u64>,
    expected_rev: Option<i64>,
) -> Result<TermRef, ApiError> {
    let built = Request::put(&format!("/admin/api/terms/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(&UpdateTermBody {
            name,
            slug,
            description,
            parent,
            expected_rev,
        });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<TermRef>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `DELETE /admin/api/terms/{id}` — delete a term (`ManageTerms`); children are
/// re-homed to its parent. The success response is `204 No Content` — this must NOT
/// attempt to parse a JSON body (SF13). On failure the server's `{ error }` message
/// (409 naming the re-home slug collision) is surfaced verbatim; a 401 routes back
/// to login.
pub async fn delete_term(id: u64) -> Result<(), ApiError> {
    let resp = Request::delete(&format!("/admin/api/terms/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    Ok(())
}

// ── pages ────────────────────────────────────────────────────────────────────────
//
// Pages are the hierarchical content type: each is served at a NESTED permalink built
// from its ancestor slugs + its own slug (the materialized `path`). The editor DTOs
// mirror `ferropress-http/src/admin/pages.rs` and add the hierarchy fields posts lack
// (`path`, `parent`, `menu_order`, `depth`, `template`). Distinct types from the post
// DTOs so the two surfaces can't silently drift.

/// One row of `GET /admin/api/pages` — a page in the tree. `path` is the full
/// materialized permalink; `depth` (ancestor count) drives the list indent; `parent`
/// and `menu_order` are the hierarchy/order keys.
#[derive(Clone, PartialEq, Deserialize)]
pub struct PageSummary {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub menu_order: i32,
    #[serde(default)]
    pub parent: Option<u64>,
    /// Ancestor count (0 = top-level), for the tree indent.
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub featured_media: Option<FeaturedMedia>,
}

/// `GET /admin/api/pages/{id}` — a page with its body + hierarchy meta, for the editor.
#[derive(Deserialize)]
pub struct PageDetail {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub block_tree: serde_json::Value,
    #[serde(default)]
    pub menu_order: i32,
    #[serde(default)]
    pub parent: Option<u64>,
    /// The chosen theme template value (e.g. `"page-wide"`), or `None` for the default.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub featured_media: Option<FeaturedMedia>,
}

/// `PUT /admin/api/pages/{id}` body. Same hierarchy fields as [`CreatePageRequest`];
/// the server recomputes the `path` + cascades to descendants on a slug/parent change.
#[derive(Serialize)]
pub struct SavePageRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    /// The featured image's Media id, or `None` to clear it.
    pub featured_media: Option<u64>,
    /// The parent page id, or `None` for a top-level page.
    pub parent: Option<u64>,
    /// Sibling ordering key.
    pub menu_order: i32,
    /// The theme template value, or `None`/`""` for the default.
    pub template: Option<String>,
}

/// `POST /admin/api/pages` body — create a new page.
#[derive(Serialize)]
pub struct CreatePageRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    pub featured_media: Option<u64>,
    pub parent: Option<u64>,
    pub menu_order: i32,
    pub template: Option<String>,
}

/// One theme page-template option from `GET /admin/api/templates`, for the editor's
/// Template `<select>`. The empty `value` is the default (single) template.
#[derive(Clone, PartialEq, Deserialize)]
pub struct TemplateOption {
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub label: String,
}

/// `GET /admin/api/pages` — every non-trashed page this user may edit, in tree order
/// (parent before children, then `menu_order`). A 401 routes back to login.
pub async fn list_pages() -> Result<Vec<PageSummary>, ApiError> {
    let resp = Request::get("/admin/api/pages")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<PageSummary>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/pages/{id}` — one page with its body + hierarchy meta.
pub async fn get_page(id: u64) -> Result<PageDetail, ApiError> {
    let resp = Request::get(&format!("/admin/api/pages/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<PageDetail>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/pages/{id}` — persist a page edit. On failure the server's
/// `{ error }` message (409 path clash, 400 bad slug/status/template/parent, cycle) is
/// surfaced verbatim; a 401 routes back to login.
pub async fn save_page(id: u64, body: &SavePageRequest) -> Result<(), ApiError> {
    let built = Request::put(&format!("/admin/api/pages/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    Ok(())
}

/// `POST /admin/api/pages` — create a new page; returns its new id. On failure the
/// server's `{ error }` message is surfaced verbatim; a 401 routes back to login.
pub async fn create_page(body: &CreatePageRequest) -> Result<u64, ApiError> {
    let built = Request::post("/admin/api/pages")
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<CreateResponse>()
        .await
        .map(|r| r.id)
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/templates` — the theme's page templates for the editor's Template
/// picker. Any editing role may read them (theme metadata, not content).
pub async fn list_templates() -> Result<Vec<TemplateOption>, ApiError> {
    let resp = Request::get("/admin/api/templates")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<TemplateOption>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `POST /admin/api/media` response. `url` (`/media/{uuid}`) is what the editor
/// inserts as the image `src`; `id` is the Media's handle, echoed back as
/// `featured_media` to set the relation. (The server also returns dimensions +
/// mime; serde ignores the fields we don't use.)
#[derive(Deserialize)]
pub struct UploadResponse {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
}

/// `POST /admin/api/media` — upload an image as multipart (`file` + `alt`). The
/// browser sets the `multipart/form-data` boundary from the `FormData` body, so we
/// must NOT set a content-type header. Returns the new media's id + served URL; a 401
/// routes back to login.
pub async fn upload_media(form: FormData) -> Result<UploadResponse, ApiError> {
    let req = Request::post("/admin/api/media")
        .credentials(RequestCredentials::SameOrigin)
        .body(form)
        .map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<UploadResponse>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// One row of `GET /admin/api/media` — the media library, for the picker grid. `url`
/// (`/media/{uuid}`) is the thumbnail src; `id` is the Media handle a picker writes
/// back into a `MediaPicker` value. (`Default` exists only to satisfy the rinch
/// `#[component]` macro — `MediaCell` takes a `MediaSummary` prop.)
#[derive(Clone, PartialEq, Default, Deserialize)]
pub struct MediaSummary {
    pub id: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub alt: String,
    #[serde(default)]
    pub uploaded_at: Option<i64>,
}

/// `GET /admin/api/media` — the media library, newest first (Author+; a 401 routes
/// back to login, a 403 surfaces as a message).
pub async fn list_media() -> Result<Vec<MediaSummary>, ApiError> {
    let resp = Request::get("/admin/api/media")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<MediaSummary>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET`/`PUT /admin/api/settings` response: the declarative schema (rendered by
/// `ferropress-form-view`) + the current `key -> value` map. `FormSchema` is the
/// shared rinch-free type from `ferropress-render-form`.
#[derive(Deserialize)]
pub struct SettingsDto {
    #[serde(default)]
    pub schema: ferropress_render_form::FormSchema,
    #[serde(default)]
    pub values: serde_json::Map<String, serde_json::Value>,
    /// Resolved references the id-valued widgets need to render: `EntityRef` dropdown
    /// options + `MediaPicker` thumbnail URLs. Empty for a schema that uses neither.
    #[serde(default)]
    pub refs: ferropress_render_form::SettingRefs,
}

/// `PUT /admin/api/settings` body — a sparse map of edits (the server whitelists to
/// the schema's keys).
#[derive(Serialize)]
struct PutSettingsBody {
    values: serde_json::Map<String, serde_json::Value>,
}

/// `GET /admin/api/settings` — the settings schema + current values (Administrator
/// only; a 401 routes back to login, a 403 surfaces as a message).
pub async fn get_settings() -> Result<SettingsDto, ApiError> {
    let resp = Request::get("/admin/api/settings")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<SettingsDto>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/settings` — persist edited values; returns the fresh schema +
/// values. On a validation failure the server's `{ error }` message is surfaced
/// verbatim; a 401 routes back to login.
pub async fn put_settings(
    values: serde_json::Map<String, serde_json::Value>,
) -> Result<SettingsDto, ApiError> {
    let built = Request::put("/admin/api/settings")
        .credentials(RequestCredentials::SameOrigin)
        .json(&PutSettingsBody { values });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<SettingsDto>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

// ── plugin config ────────────────────────────────────────────────────────────────

/// A configurable plugin, as listed by `GET /admin/api/plugins`. Reuses the shared
/// rinch-free `PluginDescriptor` from `ferropress-render-form` (server → client),
/// so the list shape has a single source of truth.
pub use ferropress_render_form::PluginDescriptor;

/// `GET /admin/api/plugins` — the installed plugins (Administrator only; a 401 routes
/// back to login, a 403 surfaces as a message).
pub async fn list_plugins() -> Result<Vec<PluginDescriptor>, ApiError> {
    let resp = Request::get("/admin/api/plugins")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<PluginDescriptor>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/plugins/{id}/settings` — a plugin's config schema + current values
/// (the SAME `SettingsDto` shape as site settings, rendered by the same form).
pub async fn get_plugin_settings(id: &str) -> Result<SettingsDto, ApiError> {
    let resp = Request::get(&format!("/admin/api/plugins/{id}/settings"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<SettingsDto>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/plugins/{id}/settings` — persist edited plugin config; returns the
/// fresh schema + values. A validation failure surfaces the server message; a 401
/// routes back to login.
pub async fn put_plugin_settings(
    id: &str,
    values: serde_json::Map<String, serde_json::Value>,
) -> Result<SettingsDto, ApiError> {
    let built = Request::put(&format!("/admin/api/plugins/{id}/settings"))
        .credentials(RequestCredentials::SameOrigin)
        .json(&PutSettingsBody { values });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<SettingsDto>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

// ── nav menus ────────────────────────────────────────────────────────────────────
//
// The menu editor's DTOs mirror `ferropress-http/src/admin/menus.rs`. The item
// `target` REUSES `ferropress_core::LinkTarget` directly (the admin crate already deps
// core) rather than a hand-mirrored enum, so the tagged JSON can't drift and a `Term`
// (or any future variant) round-trips through GET→edit→PUT unchanged. The whole-tree PUT
// returns the reconciled [`MenuDetail`] (authoritative) — the editor re-seeds from it.

pub use ferropress_core::LinkTarget;

/// One row of `GET /admin/api/menus`.
#[derive(Clone, PartialEq, Deserialize)]
pub struct MenuSummary {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub item_count: usize,
}

/// The lightweight menu identity from create/update (and embedded in a location row).
#[derive(Clone, PartialEq, Deserialize)]
pub struct MenuRef {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
}

/// A menu item's server-resolved DISPLAY: the target's title (always) + its public href
/// (`Some` only when it currently resolves — `None` = won't render in the live nav). A
/// `Custom` item carries none (its URL is the target). Output-only from the server.
#[derive(Clone, PartialEq, Deserialize)]
pub struct ResolvedTarget {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub href: Option<String>,
    /// S4 fix-forward: a `Term` target's owning taxonomy's display label
    /// ("Categories"/"Tags") — `None` for Post/Page/Custom, or an
    /// unresolvable (deleted) term. Lets the row's kind stamp show the real
    /// taxonomy name instead of the literal word "Term" (SF14e).
    #[serde(default)]
    pub taxonomy_label: Option<String>,
}

/// One menu item on the wire — the GET response shape AND the whole-tree PUT body.
/// `id` present = an existing item; `client_id` is the payload-stable handle a child
/// references via `parent_client_id`. `resolved` is server→client only (skipped on a PUT).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MenuItemNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_client_id: Option<String>,
    pub label: String,
    pub target: LinkTarget,
    #[serde(default)]
    pub new_tab: bool,
    /// Server-computed display sidecar — read from a GET/save response, never sent in a
    /// PUT body (`skip_serializing`), so `ResolvedTarget` needs no `Serialize`.
    #[serde(default, skip_serializing)]
    pub resolved: Option<ResolvedTarget>,
}

/// `GET /admin/api/menus/{id}` (and the `PUT .../items` save response) — a menu + its
/// item forest (flat, `(item_order, id)`-ordered; the client rebuilds nesting from
/// `parent_client_id`).
#[derive(Deserialize)]
pub struct MenuDetail {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    /// WordPress's "Automatically add new top-level pages to this menu" flag.
    #[serde(default)]
    pub auto_add_pages: bool,
    #[serde(default)]
    pub items: Vec<MenuItemNode>,
}

/// One row of `GET /admin/api/menus/locations`: a theme location + the menu bound to it.
/// `declared: false` marks a STRANDED assignment (a binding from a theme that declared a
/// location the current one does not) — shown so it can still be cleared. (`Default` only
/// to satisfy the rinch `#[component]` macro — `MenuLocationView` takes it as a prop.)
#[derive(Clone, PartialEq, Default, Deserialize)]
pub struct MenuLocationRow {
    pub location: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub menu: Option<MenuRef>,
    #[serde(default)]
    pub declared: bool,
}

/// One pickable target from `GET /admin/api/menus/link-candidates` — a PUBLISHED Post,
/// Page, or Term, with the href it will resolve to. (`Default` only to satisfy the
/// rinch `#[component]` macro — `CandidateRow` takes it as a prop.) `taxonomy` /
/// `depth` are populated ONLY for `kind == "term"` (S4: they split a term candidate
/// list into per-taxonomy picker tabs and drive its indent) — `None` for Pages/Posts.
#[derive(Clone, PartialEq, Default, Deserialize)]
pub struct LinkCandidate {
    #[serde(default)]
    pub kind: String,
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub href: String,
    /// The owning taxonomy's KEY, term candidates only.
    #[serde(default)]
    pub taxonomy: Option<String>,
    /// Ancestor depth (root = 0), term candidates only.
    #[serde(default)]
    pub depth: Option<u32>,
}

/// `GET /admin/api/menus/link-candidates` response: Pages in full, Posts as a bounded,
/// searchable slice (`posts_truncated` when more exist), Terms in full (taxonomy-
/// key-sorted, then hierarchy/depth order WITHIN each taxonomy — never re-sort this
/// list, `depth` only means what it says in that order).
#[derive(Default, Clone, Deserialize)]
pub struct LinkCandidates {
    #[serde(default)]
    pub pages: Vec<LinkCandidate>,
    #[serde(default)]
    pub posts: Vec<LinkCandidate>,
    #[serde(default)]
    pub posts_truncated: bool,
    /// S1 wire groundwork for Inc-3 S4 (the menu-item picker growing
    /// Categories/Tags tabs, per the design ruling's owner call #2) — the
    /// picker modal itself does not read this yet, so it is dead until S4
    /// lands.
    #[serde(default)]
    #[allow(dead_code)]
    pub terms: Vec<LinkCandidate>,
}

#[derive(Serialize)]
struct MenuNameBody<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<&'a str>,
    /// Present only on an update that toggles it (create always sends `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_add_pages: Option<bool>,
}

#[derive(Serialize)]
struct SaveItemsBody<'a> {
    items: &'a [MenuItemNode],
}

#[derive(Serialize)]
struct AssignLocationBody {
    menu_id: Option<u64>,
}

/// `GET /admin/api/menus` — every menu with its item count (Editor+; a 401 routes to login).
pub async fn list_menus() -> Result<Vec<MenuSummary>, ApiError> {
    let resp = Request::get("/admin/api/menus")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<MenuSummary>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/menus/{id}` — one menu + its item forest, ready to edit.
pub async fn get_menu(id: u64) -> Result<MenuDetail, ApiError> {
    let resp = Request::get(&format!("/admin/api/menus/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<MenuDetail>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `POST /admin/api/menus` — create a menu; returns its identity.
pub async fn create_menu(name: &str, slug: Option<&str>) -> Result<MenuRef, ApiError> {
    let built = Request::post("/admin/api/menus")
        .credentials(RequestCredentials::SameOrigin)
        .json(&MenuNameBody {
            name,
            slug,
            auto_add_pages: None,
        });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<MenuRef>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/menus/{id}` — rename / re-slug a menu, and (when `auto_add_pages` is
/// `Some`) set its "automatically add new top-level pages" flag.
pub async fn update_menu(
    id: u64,
    name: &str,
    slug: Option<&str>,
    auto_add_pages: Option<bool>,
) -> Result<MenuRef, ApiError> {
    let built = Request::put(&format!("/admin/api/menus/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(&MenuNameBody {
            name,
            slug,
            auto_add_pages,
        });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<MenuRef>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `DELETE /admin/api/menus/{id}` — delete a menu (cascades its items + location bindings).
pub async fn delete_menu(id: u64) -> Result<(), ApiError> {
    let resp = Request::delete(&format!("/admin/api/menus/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    Ok(())
}

/// `PUT /admin/api/menus/{id}/items` — reconcile the whole item forest; returns the
/// reconciled [`MenuDetail`] (authoritative: every new item carries its real id).
pub async fn save_menu_items(id: u64, items: &[MenuItemNode]) -> Result<MenuDetail, ApiError> {
    let built = Request::put(&format!("/admin/api/menus/{id}/items"))
        .credentials(RequestCredentials::SameOrigin)
        .json(&SaveItemsBody { items });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<MenuDetail>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `GET /admin/api/menus/locations` — theme locations + their current menu bindings.
pub async fn list_menu_locations() -> Result<Vec<MenuLocationRow>, ApiError> {
    let resp = Request::get("/admin/api/menus/locations")
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<Vec<MenuLocationRow>>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/menus/locations/{location}` — bind a menu to a location, or clear it
/// (`menu_id` = None).
pub async fn assign_location(location: &str, menu_id: Option<u64>) -> Result<(), ApiError> {
    let built = Request::put(&format!("/admin/api/menus/locations/{location}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(&AssignLocationBody { menu_id });
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    Ok(())
}

/// `GET /admin/api/menus/link-candidates` — the published Post/Page targets for the "add
/// item" picker (optionally search-filtered by `q`).
pub async fn list_link_candidates(q: Option<&str>) -> Result<LinkCandidates, ApiError> {
    let url = match q.map(str::trim).filter(|s| !s.is_empty()) {
        Some(q) => format!("/admin/api/menus/link-candidates?q={}", encode_query(q)),
        None => "/admin/api/menus/link-candidates".to_owned(),
    };
    let resp = Request::get(&url)
        .credentials(RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|_| ApiError::Message(NETWORK_ERROR.to_owned()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<LinkCandidates>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// Minimal percent-encoding for a query-string value (the search needle): encodes the
/// characters that would break the `?q=` parameter. Enough for a title search; the
/// server trims + lowercases anyway.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Read a failed response's `{ error }` body, falling back to the status code.
async fn error_message(resp: Response) -> String {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(b) if !b.error.is_empty() => b.error,
        _ => format!("request failed ({status})"),
    }
}

// ── request-shaping helpers ─────────────────────────────────────────────────────

/// The `SaveRequest`/`CreateRequest::terms` value to SEND (SF1 "dirty-send"): `None`
/// when `current` is UNCHANGED from `snapshot` (order-independent — a taxonomy panel
/// re-tick in a different order is not a change), `Some(current)` otherwise. An
/// untouched taxonomy panel must therefore never clear or touch a post's category/tag
/// membership on a body-only save, and every such save stays off the server's
/// process-global `taxonomy_lock` (`ferropress-http`'s `posts.rs`). `Some(vec![])` is
/// returned — deliberately, not folded into `None` — exactly when `current` is empty
/// but `snapshot` was not: that is the explicit "clear everything" signal, distinct
/// from "the panel was never touched". `new_terms` is unaffected by this helper: it
/// stays additive and race-safe even when `terms` itself is sent as `None`.
pub fn dirty_terms(current: &[u64], snapshot: &[u64]) -> Option<Vec<u64>> {
    let mut c = current.to_vec();
    let mut s = snapshot.to_vec();
    c.sort_unstable();
    s.sort_unstable();
    if c == s { None } else { Some(current.to_vec()) }
}

// ── display helpers ──────────────────────────────────────────────────────────────

/// The author-facing statuses, in ladder order: `(api value, label)`. Excludes
/// `trashed` (the API already hides trashed posts from the list).
pub const STATUSES: &[(&str, &str)] = &[
    ("draft", "Draft"),
    ("pending", "Pending"),
    ("published", "Published"),
    ("private", "Private"),
    ("scheduled", "Scheduled"),
];

/// The display label for an API status value (falls back to the raw value).
pub fn status_label(api: &str) -> String {
    STATUSES
        .iter()
        .find(|(v, _)| *v == api)
        .map(|(_, l)| (*l).to_owned())
        .unwrap_or_else(|| api.to_owned())
}

/// The inked-stamp CSS modifier for a status: published is solid green, draft is a
/// dashed ochre, everything else is steel (per the design mockup).
pub fn status_stamp_class(api: &str) -> &'static str {
    match api {
        "published" => "stamp stamp--published",
        "draft" => "stamp stamp--draft",
        _ => "stamp stamp--private",
    }
}

/// A coarse relative time from an epoch-millis instant (no date lib in the bundle):
/// "just now" / "Nm ago" / "Nh ago" / "yesterday" / "Nd ago". Empty when absent.
pub fn fmt_relative(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return String::new();
    };
    let now = js_sys::Date::now() as i64;
    let sec = (now - ms).max(0) / 1000;
    if sec < 60 {
        "just now".to_owned()
    } else if sec < 3600 {
        format!("{}m ago", sec / 60)
    } else if sec < 86_400 {
        format!("{}h ago", sec / 3600)
    } else {
        let days = sec / 86_400;
        if days == 1 {
            "yesterday".to_owned()
        } else {
            format!("{days}d ago")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dirty_terms;

    #[test]
    fn dirty_terms_unchanged_sends_none_regardless_of_order() {
        assert_eq!(dirty_terms(&[1, 2, 3], &[3, 2, 1]), None);
        assert_eq!(dirty_terms(&[], &[]), None);
        assert_eq!(dirty_terms(&[5], &[5]), None);
    }

    #[test]
    fn dirty_terms_changed_sends_the_current_selection_verbatim() {
        // Sent unsorted/as-is — the server doesn't care about order, and the
        // client's own selection order (e.g. checklist DFS order) is preserved.
        assert_eq!(dirty_terms(&[2, 1], &[1]), Some(vec![2, 1]));
    }

    #[test]
    fn dirty_terms_clearing_everything_is_some_empty_not_none() {
        // The deliberate "clear" signal must survive dirty-send — collapsing it to
        // `None` would silently leave the old membership in place.
        assert_eq!(dirty_terms(&[], &[1, 2]), Some(vec![]));
    }

    #[test]
    fn dirty_terms_newly_assigning_from_empty() {
        assert_eq!(dirty_terms(&[1, 2], &[]), Some(vec![1, 2]));
    }
}
