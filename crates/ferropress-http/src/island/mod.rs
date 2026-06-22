//! The public-site **island API** — the small rhypedb-backed JSON endpoints the
//! interactive islands (semantic search, live comments) call from the otherwise
//! static prerendered pages.
//!
//! INVARIANT (#6): the island API is OWNED in process alongside routing and
//! static serving — there is no port trait for HTTP. Handlers reach the data side
//! through the injected [`RhypeStore`](ferropress_core::store::RhypeStore) on
//! [`AppState`](crate::AppState) only.
//!
//! Both endpoints speak JSON and share [`ApiError`] for a uniform, leak-free
//! error contract: client input problems return a 400 with an author-written
//! message; a missing resource is a 404; any backend fault is logged and returned
//! as an opaque 500 (internals never reach the client).

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use ferropress_core::error::CoreError;
use ferropress_core::value::{Object, Value};

pub mod comments;
pub mod search;

#[cfg(test)]
mod tests;

/// The JSON error body every island endpoint returns on failure: `{"error": …}`.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// The island API's uniform error type. Handlers return `Result<Json<T>, ApiError>`;
/// [`IntoResponse`] maps each variant to a status + a **client-safe** JSON body.
/// Internal causes are logged but never serialized to the client.
pub enum ApiError {
    /// 400 — a client input problem. The message is author-controlled (never a
    /// raw backend string), so it is safe to return verbatim.
    BadRequest(String),
    /// 404 — no published entity / resource behind the request.
    NotFound,
    /// 500 — an internal/backend fault. The [`CoreError`] is logged; the client
    /// gets a generic body.
    Internal(CoreError),
}

/// Map a store/backend [`CoreError`] into a client-facing status. A store
/// `NotFound` becomes a 404; everything else is an opaque 500 (logged on render).
/// This lets handlers use `?` on store calls and still get the right status.
impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::NotFound { .. } => ApiError::NotFound,
            other => ApiError::Internal(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            ApiError::Internal(err) => {
                // Log the real cause; return a generic body so internals never leak.
                tracing::error!(error = %err, "island API internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

/// JSON body extractor that funnels axum's [`JsonRejection`] into the island
/// API's uniform [`ApiError`] contract. The bare `Json<T>` extractor rejects
/// malformed / wrong-`Content-Type` / mis-typed bodies with axum's *plain-text*
/// default response (415 / 400 / 422), which would bypass the `{"error": …}`
/// envelope this module promises. `ApiJson` maps every such rejection to a
/// 400 JSON `BadRequest` so the contract holds for ALL client-input failures.
///
/// The rejection `Display` strings are axum's own static, input-describing
/// messages (never a backend internal), so they are safe to echo to the client.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ApiError::BadRequest(format!(
                "invalid request body: {rejection}"
            ))),
        }
    }
}

/// Query-string extractor that funnels axum's [`QueryRejection`] into the uniform
/// [`ApiError`] contract — the query counterpart of [`ApiJson`]. A malformed query
/// string (e.g. a non-integer where a number is expected) becomes a 400 JSON
/// error rather than axum's plain-text default. (A *missing* optional param is not
/// a rejection — the handlers model required params as `Option` and validate
/// presence themselves, so absent and blank both yield one uniform 400.)
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(ApiError::BadRequest(format!(
                "invalid query parameters: {rejection}"
            ))),
        }
    }
}

/// Read a `String` field off an object, or `None` if it is absent or not a
/// string. Shared by both island endpoints' DTO mapping.
pub(crate) fn string_field(obj: &Object, field: &str) -> Option<String> {
    match obj.get(field) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Read a `DateTime` field (epoch-millis) off an object, or `None` if it is
/// absent or not a `DateTime`. Timestamps are stored as `Value::DateTime`; the
/// API formats them to RFC3339 only at the DTO boundary.
pub(crate) fn datetime_field(obj: &Object, field: &str) -> Option<i64> {
    obj.get(field).and_then(Value::as_datetime)
}

/// Whether an object's `status` field equals `expected` (statuses are stored as
/// plain strings — e.g. `Status::Published.as_str()` /
/// `CommentStatus::Approved.as_str()`). A missing/non-string status is never a
/// match, so it fails closed (hidden from public output).
pub(crate) fn status_is(obj: &Object, expected: &str) -> bool {
    matches!(obj.get("status"), Some(Value::String(s)) if s == expected)
}
