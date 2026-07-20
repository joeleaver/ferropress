//! Public syndication feeds — `GET /feed.xml` (RSS 2.0) and `GET /feed.atom` (Atom 1.0).
//!
//! Both are explicit routes registered BEFORE the page fallback (see [`crate::router`]), so they
//! are never subject to the permalink resolver and can't be shadowed by a post/page slug. Like
//! [`crate::media`], they are **un-authenticated** and mounted on every deployment (not gated on
//! `AppState.admin`) — feeds are public.
//!
//! The heavy lifting (cache-first envelope + live compose) lives in
//! [`ferropress_serve::serve_feed`]; these handlers only (1) resolve the request's origin (the
//! absolute-base fallback used when `site.url` is unset), (2) set the correct XML `Content-Type`,
//! and (3) implement conditional GET (`ETag` + `304 Not Modified`) so aggregators that poll
//! frequently can revalidate cheaply. The ETag is a deterministic hash of the composed bytes, so
//! it is stable across process restarts for unchanged content.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use ferropress_serve::FeedFormat;

use crate::AppState;

/// `GET /feed.xml` — the RSS 2.0 feed.
pub async fn feed_rss(State(state): State<AppState>, headers: HeaderMap) -> Response {
    render(state, &headers, FeedFormat::Rss).await
}

/// `GET /feed.atom` — the Atom 1.0 feed.
pub async fn feed_atom(State(state): State<AppState>, headers: HeaderMap) -> Response {
    render(state, &headers, FeedFormat::Atom).await
}

/// Compose the requested feed and return it with the right content type + conditional-GET
/// headers. A backend/render fault is logged and returned as a generic 500 (never leaked).
async fn render(state: AppState, headers: &HeaderMap, format: FeedFormat) -> Response {
    let origin = request_origin(headers);
    let xml = match ferropress_serve::serve_feed(
        &state.store,
        &state.blobs,
        state.custom.as_ref(),
        &state.settings.current(),
        &state.authors.current(),
        format,
        origin.as_deref(),
    )
    .await
    {
        Ok(xml) => xml,
        Err(e) => {
            tracing::error!(feed = format.path(), error = %e, "feed render failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };

    // Conditional GET: a strong ETag over the composed bytes. Aggregators poll often; a matching
    // `If-None-Match` gets a bodyless 304. `no-cache` keeps clients revalidating (so a just-
    // published post appears immediately) while the ETag makes that revalidation cheap.
    let etag = format!("\"{:016x}\"", fnv1a(xml.as_bytes()));
    if if_none_match_matches(headers, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, format.content_type().to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        xml,
    )
        .into_response()
}

/// The request's `{scheme}://{host}` origin, used as the absolute-base fallback when `site.url`
/// is unset. Prefers the `X-Forwarded-*` proxy headers (Ferropress usually runs behind a
/// TLS-terminating proxy), falling back to the `Host` header and an `http` scheme. `None` when no
/// host is determinable (e.g. an HTTP/1.0 request without `Host`), which degrades the feed to
/// relative URLs.
fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = first_host(headers)?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    Some(format!("{scheme}://{host}"))
}

/// The first NON-EMPTY host across `X-Forwarded-Host` (preferred) then `Host`. Selecting by
/// non-empty *value* (not by header presence) means a present-but-empty `X-Forwarded-Host:` — or
/// a leading-empty comma list `, real.host` — still falls through to the real `Host` header
/// rather than yielding `None` and degrading the feed to relative URLs.
fn first_host(headers: &HeaderMap) -> Option<&str> {
    ["x-forwarded-host", "host"]
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find(|s| !s.is_empty())
}

/// Whether the request's `If-None-Match` names our current `etag` (so we can 304). Handles a
/// comma-separated list and the `*` wildcard; a weak-validator prefix (`W/`) still matches on the
/// opaque tag since our tags are strong and identical byte-for-byte.
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/") == etag
    })
}

/// FNV-1a 64-bit hash — a small, dependency-free, DETERMINISTIC hash for the ETag (unlike
/// `DefaultHasher`, whose seed is randomized per process, which would break conditional GET
/// across restarts).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn first_host_falls_through_to_host_when_xforwarded_is_empty() {
        // A present-but-EMPTY X-Forwarded-Host must not shadow a valid Host header.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-host", HeaderValue::from_static(""));
        h.insert(header::HOST, HeaderValue::from_static("real.host"));
        assert_eq!(first_host(&h), Some("real.host"));
    }

    #[test]
    fn first_host_prefers_first_nonempty_forwarded_hop() {
        // A leading-empty comma list still yields the real client-facing hop.
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-host",
            HeaderValue::from_static(", proxied.host"),
        );
        h.insert(header::HOST, HeaderValue::from_static("real.host"));
        assert_eq!(first_host(&h), Some("proxied.host"));
    }

    #[test]
    fn first_host_is_none_without_any_host_header() {
        assert_eq!(first_host(&HeaderMap::new()), None);
    }
}
