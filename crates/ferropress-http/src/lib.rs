//! # ferropress-http
//!
//! The owned, in-process HTTP server (axum). Ferropress NEVER delegates public
//! delivery to a host's static hosting — serving is owned, which is the hard
//! portability rule. This crate:
//!   * serves rendered HTML pages (v1: SSR-on-demand via [`ferropress_serve`];
//!     the prerendered-from-[`BlobStore`] hot path is a later increment),
//!   * exposes the small rhypedb-backed **island API** (see [`island`]): semantic
//!     search via [`RhypeStore::vector_search`] and live comments over the
//!     `Comment` entity.
//!
//! INVARIANT (#6): routing / static serving / the island API are OWNED in
//! process — there is no port trait for HTTP. Only the data side ([`RhypeStore`],
//! [`BlobStore`]) is injected, plus the render/theme collaborators carried on
//! [`AppState`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tower_http::services::ServeDir;

use ferropress_core::error::CoreError;
use ferropress_core::ports::BlobStore;
use ferropress_core::store::RhypeStore;
use ferropress_render::{CustomBlockRenderer, NoCustomBlocks};
use ferropress_serve::Resolved;
use ferropress_theme::ThemeEngine;

pub mod island;

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
    /// Directory holding the built wasm island bundle (the `wasm-bindgen` output
    /// of `ferropress-islands`). When set, it is served at `/_fp/islands`; `None`
    /// (e.g. in tests) simply omits that route.
    pub islands_dir: Option<PathBuf>,
    /// Resolves `BlockKind::Custom` (plugin) blocks during page render. Defaults to
    /// [`NoCustomBlocks`] (custom blocks render as placeholders); the composition
    /// root injects the plugin host via [`with_custom_renderer`](Self::with_custom_renderer).
    pub custom: Arc<dyn CustomBlockRenderer>,
}

impl AppState {
    /// Assemble the shared state from the injected ports + theme host. Island asset
    /// serving is off until [`with_islands_dir`](Self::with_islands_dir); custom
    /// blocks render as placeholders until [`with_custom_renderer`](Self::with_custom_renderer).
    pub fn new(
        store: Arc<dyn RhypeStore>,
        blobs: Arc<dyn BlobStore>,
        theme: Arc<ThemeEngine>,
    ) -> Self {
        Self {
            store,
            blobs,
            theme,
            islands_dir: None,
            custom: Arc::new(NoCustomBlocks),
        }
    }

    /// Serve the wasm island bundle from `dir` (the `dist/` output of
    /// `cargo xtask build-islands`) at `/_fp/islands`.
    pub fn with_islands_dir(mut self, dir: PathBuf) -> Self {
        self.islands_dir = Some(dir);
        self
    }

    /// Resolve plugin (`BlockKind::Custom`) blocks during render via `custom`
    /// (the `ferropress-plugin-host`).
    pub fn with_custom_renderer(mut self, custom: Arc<dyn CustomBlockRenderer>) -> Self {
        self.custom = custom;
        self
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
/// Static *media* (user content) is intentionally served from the [`BlobStore`]
/// via a handler (not `ServeDir`) so the port stays the single source of content
/// bytes — that handler is a later increment. The island bundle below is
/// different: it is a *build artifact* (the `wasm-bindgen` output), not content,
/// so it is served straight from the build dir via [`ServeDir`].
pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        // Island API: the rhypedb-backed JSON endpoints the public-site islands
        // call. Semantic search over `Post.search`; live comments (list approved +
        // accept a pending comment) over the `Comment` entity. See [`island`].
        .route("/api/search", get(island::search::search))
        .route(
            "/api/comments",
            get(island::comments::list).post(island::comments::create),
        )
        // Static-first hot path is the fallback: it consults the prerender
        // BlobStore cache first (via `ferropress_serve::serve_path`) and only
        // falls through to an on-demand SSR render — populating the cache — on a
        // miss.
        .fallback(serve_page);

    // Serve the built wasm island bundle (JS + `_bg.wasm`) at `/_fp/islands` when
    // a bundle dir is configured. `ServeDir`'s mime_guess returns the right
    // `text/javascript` + `application/wasm` content types for ESM + wasm loading.
    if let Some(dir) = &state.islands_dir {
        app = app.nest_service("/_fp/islands", ServeDir::new(dir));
    }

    app.with_state(state)
}

/// Liveness probe. Always 200 once the process is up and routing.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// The page fallback: serve the request path **cache-first**.
///
/// Delegates to [`ferropress_serve::serve_path`], which tries the prerender
/// [`BlobStore`] cache and only renders-on-demand (then populates the cache) on a
/// miss — so the steady state is a static blob read, not a fresh render. The
/// `Resolved` outcome maps to a status code exactly as before; the cache is
/// best-effort inside `serve_path`, so a blob fault degrades to SSR rather than a
/// 500. The real cause of a 500 is logged but never leaked.
async fn serve_page(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_owned();

    match ferropress_serve::serve_path(
        &state.store,
        &state.blobs,
        &state.theme,
        state.custom.as_ref(),
        &path,
    )
    .await
    {
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
