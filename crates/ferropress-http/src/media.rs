//! Public media serving — `GET /media/{id}` streams a `Media` original's bytes.
//!
//! This is the read counterpart of the admin upload endpoint ([`admin::media`]) and
//! of the serve-layer `data-media-id` → `src` rewrite: the rewrite turns an image
//! block into `<img src="/media/{id}">`, and THIS handler resolves that id back to
//! bytes.
//!
//! Two deliberate placements (both flagged as reserved in `lib.rs`'s router doc):
//!   * **Un-authenticated** — public pages embed images, so there is no `AuthedUser`
//!     extractor here (unlike every admin route). Media is world-readable by id.
//!   * **In the main `router()`, not `admin::api_routes()`** — it must serve on a
//!     public-only deployment (`AppState.admin == None`) too.
//!
//! Bytes are read through the [`BlobStore`](ferropress_core::ports::BlobStore) port —
//! the single source of content bytes — never `ServeDir`.
//!
//! ## Access model (conscious MVP decision — a documented limitation, not an oversight)
//!
//! A media original is **public and addressable by its (sequential) object id**, with
//! NO per-post visibility gate: the `Media` entity carries no status, and this handler
//! never consults the block tree of any post that embeds it. This is the same
//! capability-URL model mainstream CMSes use (WordPress serves `/wp-content/uploads/…`
//! with no per-post access control), and it is what keeps the pipeline clean: the
//! id-in-URL is what lets the serve-layer rewrite and the editor bridge stay DB-free
//! (no id→URL manifest, no store lookup per rendered image).
//!
//! Two accepted consequences: (1) an image uploaded into a still-unpublished DRAFT is
//! fetchable by anyone who knows/guesses its URL the instant upload returns; and
//! (2) because ids are small and sequential, `/media/1,2,3,…` is **enumerable**.
//! Hardening (unguessable blob-derived / signed URLs, or serving only media reachable
//! from PUBLISHED content) is a deliberate follow-up: it unravels the DB-free design
//! above, so it is a separate slice rather than something bolted on here. Do not embed
//! a genuine secret in an image and rely on the draft being private.

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use ferropress_core::MEDIA_TYPE;
use ferropress_core::error::CoreError;
use ferropress_core::is_media_token;
use ferropress_core::ports::BlobKey;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::value::{TypeName, Value};

use crate::AppState;

/// `GET /media/{token}` — the media original's bytes with its stored `Content-Type`.
///
/// `token` is the `Media.uuid` (unguessable, so URLs aren't enumerable). A malformed
/// token, a missing `Media` row, a row without a `blob_key`, or missing blob bytes all
/// map to 404 (a token that resolves to nothing is "not found", not an error). A
/// genuine backend fault is logged and returned as a generic 500. Each media reference
/// is immutable (a new upload is a new uuid), so bytes carry a long `immutable` cache.
pub async fn serve(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    match load(&state, &token).await {
        Ok(Some((bytes, mime))) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".to_owned(),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "Not Found").into_response(),
        Err(e) => {
            // Log the real cause; return a generic body so internals never leak.
            tracing::error!(media_token = %token, error = %e, "media serve failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

/// Resolve a media reference token (a `Media.uuid`) to `(bytes, mime_type)`, or `None`
/// if it names nothing.
async fn load(state: &AppState, token: &str) -> Result<Option<(Vec<u8>, String)>, CoreError> {
    // A token failing the charset check can't be any real uuid → 404 without a query.
    if !is_media_token(token) {
        return Ok(None);
    }

    // `Media.uuid` is `@unique @indexed`; the single-predicate filter is the engine's
    // fast path. (There is no by-uuid `get`; identity `get` is by object id.)
    let hits = state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(MEDIA_TYPE),
            field: "uuid".to_owned(),
            op: Compare::Eq,
            value: Value::String(token.to_owned()),
            limit: Some(1),
        })
        .await?;
    let obj = match hits.into_iter().next() {
        Some(obj) => obj,
        None => return Ok(None),
    };

    let blob_key = match obj.get("blob_key") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        // A media row with no usable blob key is unserveable → 404, not a 500.
        _ => return Ok(None),
    };
    let mime = match obj.get("mime_type") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => "application/octet-stream".to_owned(),
    };

    match state.blobs.get(&BlobKey(blob_key)).await {
        Ok(bytes) => Ok(Some((bytes, mime))),
        // The row references bytes that aren't in the blob store (evicted / never
        // written) → nothing to serve.
        Err(CoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}
