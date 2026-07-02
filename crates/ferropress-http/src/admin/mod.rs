//! The **admin API** + SPA shell — the authenticated, rhypedb-backed surface the
//! rinch admin app (login → post list → editor) calls.
//!
//! Mirrors the [`island`](crate::island) module's ownership + error conventions
//! (invariant #6: HTTP is owned in-process; handlers reach data only through the
//! injected [`RhypeStore`](ferropress_core::store::RhypeStore) on
//! [`AppState`](crate::AppState)). It differs in one way: every endpoint except
//! `login`/`logout` requires a valid **session** — a stateless HMAC-signed token
//! ([`ferropress_auth`]) carried in an HttpOnly cookie. The [`AuthedUser`]
//! extractor verifies that cookie and yields the caller's id + role; handlers then
//! gate on a [`Capability`].
//!
//! The API speaks Ferropress's OWN `BlockTree` JSON — it has NO rinch dependency.
//! The `BlockTree ⇄ rinch DocNode` conversion lives client-side in the wasm SPA.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;

use ferropress_auth::{SigningKey, token};
use ferropress_core::error::CoreError;
use ferropress_core::role::{Capability, Role};
use ferropress_core::value::{Object, ObjectId, Value, now_millis};

use crate::AppState;

pub mod auth;
pub mod media;
pub mod posts;

#[cfg(test)]
mod tests;

/// The name of the session cookie the admin API sets on login and reads on every
/// guarded request.
pub(crate) const SESSION_COOKIE: &str = "fp_session";

/// Admin-side configuration carried on [`AppState`]. Its presence is what *enables*
/// the admin routes; when `None`, none of `/admin*` is mounted (a public-only
/// deployment). Injected by the composition root.
#[derive(Clone)]
pub struct AdminConfig {
    /// HMAC key that signs/verifies session tokens (from the `SecretStore`).
    pub signing_key: Arc<SigningKey>,
    /// The built wasm admin bundle dir (`xtask build-admin` output). When set, the
    /// SPA shell (`GET /admin`) + bundle (`/_fp/admin`) are served; the API works
    /// regardless (so it is testable without a built bundle).
    pub bundle_dir: Option<std::path::PathBuf>,
    /// Whether the session cookie carries `Secure` (HTTPS-only). Default true;
    /// local plain-HTTP dev opts out so the browser will store the cookie.
    pub cookie_secure: bool,
    /// Session lifetime in milliseconds (cookie Max-Age + token `exp`).
    pub session_ttl_ms: i64,
}

impl AdminConfig {
    /// Build a session cookie header value carrying `token`, or (with an empty
    /// value + Max-Age 0) clearing it. Always HttpOnly + SameSite=Strict, scoped to
    /// `/admin`; `Secure` per [`cookie_secure`](Self::cookie_secure).
    fn cookie(&self, token: &str, clear: bool) -> String {
        let max_age = if clear { 0 } else { self.session_ttl_ms / 1000 };
        let mut c = format!(
            "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={max_age}"
        );
        if self.cookie_secure {
            c.push_str("; Secure");
        }
        c
    }
}

/// The authenticated caller, extracted from the session cookie. Its presence in a
/// handler signature is the auth gate: no valid session → the request is rejected
/// with 401 before the handler body runs.
#[derive(Debug, Clone)]
pub struct AuthedUser {
    pub id: ObjectId,
    pub role: Role,
}

impl AuthedUser {
    /// Authorize an action: `Ok` iff this user's role grants `cap`, else 403.
    pub(crate) fn require(&self, cap: Capability) -> Result<(), AdminError> {
        if self.role.has(cap) {
            Ok(())
        } else {
            Err(AdminError::Forbidden)
        }
    }
}

impl FromRequestParts<AppState> for AuthedUser {
    type Rejection = AdminError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Admin disabled → no session can be valid.
        let admin = state.admin.as_ref().ok_or(AdminError::Unauthorized)?;

        let cookie_header = parts
            .headers
            .get(COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let raw = cookie_value(cookie_header, SESSION_COOKIE).ok_or(AdminError::Unauthorized)?;

        let claims = token::verify(raw, &admin.signing_key, now_millis())
            .map_err(|_| AdminError::Unauthorized)?;

        Ok(AuthedUser {
            id: ObjectId(claims.sub),
            role: claims.role,
        })
    }
}

/// Find `name`'s value in a `Cookie` header (`a=1; b=2`). Returns the raw value
/// slice, or `None` if absent. Tolerant of surrounding whitespace.
fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// The admin API's uniform error type (mirrors [`island::ApiError`](crate::island)
/// with auth statuses added). Internal causes are logged, never serialized.
pub enum AdminError {
    /// 400 — a client input problem; the message is author-controlled + safe.
    BadRequest(String),
    /// 401 — no / invalid session.
    Unauthorized,
    /// 403 — authenticated but the role lacks the capability.
    Forbidden,
    /// 404 — no such resource.
    NotFound,
    /// 409 — the request conflicts with existing state (e.g. a slug already taken).
    Conflict(String),
    /// 500 — a backend fault; the cause is logged, the client gets a generic body.
    Internal(CoreError),
}

impl From<CoreError> for AdminError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::NotFound { .. } => AdminError::NotFound,
            other => AdminError::Internal(other),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AdminError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AdminError::Unauthorized => (StatusCode::UNAUTHORIZED, "not signed in".to_owned()),
            AdminError::Forbidden => (
                StatusCode::FORBIDDEN,
                "you don't have access to that".to_owned(),
            ),
            AdminError::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            AdminError::Conflict(m) => (StatusCode::CONFLICT, m),
            AdminError::Internal(err) => {
                tracing::error!(error = %err, "admin API internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

/// JSON body extractor mapping axum's [`JsonRejection`] into the uniform
/// [`AdminError`] contract (the admin counterpart of `island::ApiJson`).
pub struct AdminJson<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for AdminJson<T>
where
    Json<T>: axum::extract::FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = AdminError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AdminJson(value)),
            Err(rejection) => Err(AdminError::BadRequest(format!(
                "invalid request body: {rejection}"
            ))),
        }
    }
}

/// The admin API sub-router (no static serving; that is mounted separately, gated
/// on a bundle dir). Merged into the main router before `with_state`.
pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/admin/api/login", post(auth::login))
        .route("/admin/api/logout", post(auth::logout))
        .route("/admin/api/me", get(auth::me))
        .route("/admin/api/posts", get(posts::list).post(posts::create))
        .route(
            "/admin/api/posts/{id}",
            get(posts::get_one).put(posts::save),
        )
        // Media upload (multipart). axum's default 2 MiB body limit would reject a
        // real image, so this route carries its own limit sized to the handler's
        // per-file cap plus multipart-framing headroom.
        .route(
            "/admin/api/media",
            post(media::upload).layer(DefaultBodyLimit::max(media::MAX_UPLOAD_BYTES + (1 << 20))),
        )
}

/// The SPA shell (`GET /admin`): a minimal HTML page that boots the wasm admin
/// bundle. The bundle mounts the rinch app (which then handles login + editing).
pub async fn shell() -> Html<&'static str> {
    Html(SHELL_HTML)
}

const SHELL_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Ferropress · admin</title>
<style>
  html, body { margin: 0; padding: 0; background: #EEEFEA; }
  #fp-admin-boot {
    font: 500 0.8rem/1.4 ui-monospace, monospace; letter-spacing: .12em;
    text-transform: uppercase; color: #6B6F76;
    display: grid; place-items: center; min-height: 100vh;
  }
</style>
</head>
<body>
<div id="fp-admin-boot">Loading the composing room…</div>
<script type="module">
  import init from '/_fp/admin/ferropress_admin.js';
  init({ module_or_path: '/_fp/admin/ferropress_admin_bg.wasm' })
    .then(() => { const b = document.getElementById('fp-admin-boot'); if (b) b.remove(); });
</script>
</body>
</html>
"#;

// ---- shared Object field helpers (admin-local; mirror island's) -------------

/// Read a `String` field off an object, or `None` if absent / not a string.
pub(crate) fn str_field(obj: &Object, field: &str) -> Option<String> {
    match obj.get(field) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Read a `DateTime` (epoch-millis) field, or `None` if absent / not a DateTime.
pub(crate) fn datetime_field(obj: &Object, field: &str) -> Option<i64> {
    match obj.get(field) {
        Some(Value::DateTime(ms)) => Some(*ms),
        _ => None,
    }
}

/// Read a `Json` field (e.g. `block_tree`), or `None` if absent / not Json.
pub(crate) fn json_field(obj: &Object, field: &str) -> Option<serde_json::Value> {
    match obj.get(field) {
        Some(Value::Json(j)) => Some(j.clone()),
        _ => None,
    }
}
