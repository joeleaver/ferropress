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
}

/// `{ id }` from a successful create (the rest of the response is ignored).
#[derive(Deserialize)]
struct CreateResponse {
    id: u64,
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<PostDetail>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// `PUT /admin/api/posts/{id}` — persist an edit. On failure the server's
/// `{ error }` message (e.g. a 409 slug clash, a 400 bad status transition) is
/// surfaced verbatim; a 401 routes back to login.
pub async fn save_post(id: u64, body: &SaveRequest) -> Result<(), ApiError> {
    let built = Request::put(&format!("/admin/api/posts/{id}"))
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    Ok(())
}

/// `POST /admin/api/posts` — create a new post; returns its new id. On failure the
/// server's `{ error }` message (a 409 slug clash, a 400 bad slug/status) is
/// surfaced verbatim; a 401 routes back to login.
pub async fn create_post(body: &CreateRequest) -> Result<u64, ApiError> {
    let built = Request::post("/admin/api/posts")
        .credentials(RequestCredentials::SameOrigin)
        .json(body);
    let req = built.map_err(|e| ApiError::Message(e.to_string()))?;
    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<CreateResponse>()
        .await
        .map(|r| r.id)
        .map_err(|e| ApiError::Message(e.to_string()))
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
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
        .map_err(|e| ApiError::Message(e.to_string()))?;
    if !resp.ok() {
        return Err(classify(resp).await);
    }
    resp.json::<SettingsDto>()
        .await
        .map_err(|e| ApiError::Message(e.to_string()))
}

/// Read a failed response's `{ error }` body, falling back to the status code.
async fn error_message(resp: Response) -> String {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(b) if !b.error.is_empty() => b.error,
        _ => format!("request failed ({status})"),
    }
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
