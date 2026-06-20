//! # ferropress-http
//!
//! The owned, in-process HTTP server (axum). Ferropress NEVER delegates public
//! delivery to a host's static hosting — serving is owned, which is the hard
//! portability rule. This crate:
//!   * serves rendered HTML pages (v1: SSR-on-demand via [`ferropress_serve`];
//!     the prerendered-from-[`BlobStore`] hot path is a later increment),
//!   * exposes the small rhypedb-backed **island API**: semantic search via
//!     [`RhypeStore::vector_search`] and live comments (deferred — see below).
//!
//! INVARIANT (#6): routing / static serving / the island API are OWNED in
//! process — there is no port trait for HTTP. Only the data side ([`RhypeStore`],
//! [`BlobStore`]) is injected, plus the render/theme collaborators carried on
//! [`AppState`].

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use ferropress_core::error::CoreError;
use ferropress_core::ports::BlobStore;
use ferropress_core::store::RhypeStore;
use ferropress_serve::Resolved;
use ferropress_theme::ThemeEngine;

/// Shared HTTP application state, cloned into every axum handler. Holds the
/// injected data ports plus the render-side collaborators the SSR fallback needs
/// (the theme host). The router itself is owned.
///
/// `ThemeEngine` is not `Clone` and registering its templates per-request is
/// wasteful, so it is built once at boot and shared as an `Arc`.
#[derive(Clone)]
pub struct AppState {
    /// The typed object store (page resolution + island API).
    pub store: Arc<dyn RhypeStore>,
    /// Prerendered HTML + media originals. (Hot path / media serving: TODO.)
    pub blobs: Arc<dyn BlobStore>,
    /// The sandboxed MiniJinja chrome host, with the built-in page template
    /// already registered. Shared read-only across handlers.
    pub theme: Arc<ThemeEngine>,
}

impl AppState {
    /// Assemble the shared state from the injected ports + theme host.
    pub fn new(
        store: Arc<dyn RhypeStore>,
        blobs: Arc<dyn BlobStore>,
        theme: Arc<ThemeEngine>,
    ) -> Self {
        Self {
            store,
            blobs,
            theme,
        }
    }
}

/// The HTTP server. Constructed from [`AppState`], then [`HttpServer::serve`]d.
pub struct HttpServer {
    state: AppState,
}

impl HttpServer {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Bind `addr` and serve until shutdown.
    ///
    /// Both failure points cross into [`ferropress_core::error::Result`], which
    /// has no `From<std::io::Error>` / `From<axum>` impl, so each is mapped into a
    /// [`CoreError`] explicitly: a bind failure is a misconfigured port
    /// ([`CoreError::Unavailable`]); a mid-serve failure is a backend fault
    /// ([`CoreError::Store`]).
    pub async fn serve(self, addr: SocketAddr) -> ferropress_core::error::Result<()> {
        let app = router(self.state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| CoreError::Unavailable(format!("binding {addr}: {e}")))?;

        tracing::info!(%addr, "ferropress-http listening");

        axum::serve(listener, app)
            .await
            .map_err(|e| CoreError::Store(format!("http serve loop failed: {e}")))
    }
}

/// Convenience free function mirroring the composition-root call shape
/// (`http::serve(state, addr)`).
pub async fn serve(state: AppState, addr: SocketAddr) -> ferropress_core::error::Result<()> {
    HttpServer::new(state).serve(addr).await
}

/// Build the axum [`Router`]: a health probe, the (deferred) island API, and a
/// fallback that resolves + SSR-renders the requested page.
///
/// Exposed (not private) so an integration test can drive the EXACT same handler
/// graph the server serves, without binding a socket (via `tower::ServiceExt`
/// `oneshot`).
///
/// Static media is intentionally served from the [`BlobStore`] via a handler
/// (not `tower-http` `ServeDir`) so the port stays the single source of bytes —
/// that handler is a later increment.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // Island API (deferred): real routes, 501 bodies until the rhypedb-backed
        // search + comments handlers land. TODO: implement against
        // `RhypeStore::vector_search` (search) and the Comment entity (comments).
        .route("/api/search", get(island_not_implemented))
        .route(
            "/api/comments",
            get(island_not_implemented).post(island_not_implemented),
        )
        // Static-first hot path is the fallback: today it always SSR-renders on
        // demand. TODO: consult the prerender BlobStore cache first and only fall
        // through to SSR on a miss.
        .fallback(serve_page)
        .with_state(state)
}

/// Liveness probe. Always 200 once the process is up and routing.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Island API placeholder. v1 returns `501 Not Implemented` with a plain body;
/// the client never sees internals because there are none yet.
// TODO: replace with real semantic-search / comments handlers (the rhypedb
// island API) — `RhypeStore::vector_search` and the Comment entity.
async fn island_not_implemented() -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "island API not implemented")
}

/// The page fallback: resolve the request path to a published entity and render
/// it on demand (v1 SSR). Maps the resolution outcome to a status code, logging —
/// but never leaking — the cause of a 500.
async fn serve_page(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_owned();

    match ferropress_serve::resolve_path(&state.store, &state.theme, &path).await {
        Resolved::Found(html) => (StatusCode::OK, Html(html)).into_response(),
        Resolved::NotFound => (StatusCode::NOT_FOUND, "Not Found").into_response(),
        Resolved::Error(err) => {
            // Log the real cause; return a generic body so internals never leak.
            tracing::error!(%path, error = %err, "page render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

#[cfg(test)]
mod tests;
