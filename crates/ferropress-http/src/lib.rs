//! # ferropress-http
//!
//! The owned, in-process HTTP server (axum). Ferropress NEVER delegates public
//! delivery to a host's static hosting — serving is owned, which is the hard
//! portability rule. This crate:
//!   * serves prerendered HTML from the [`BlobStore`] port (the static-first hot
//!     path),
//!   * runs SSR entrypoints for on-demand / uncached pages,
//!   * exposes the small rhypedb-backed **island API**: semantic search via
//!     [`RhypeStore::vector_search`] and live comments.
//!
//! INVARIANT (#6): routing / static serving / the island API are OWNED in
//! process — there is no port trait for HTTP. Only the data side ([`RhypeStore`],
//! [`BlobStore`]) is injected. STUB scaffold: real signatures, `todo!()` bodies.

use std::net::SocketAddr;
use std::sync::Arc;

use ferropress_core::ports::BlobStore;
use ferropress_core::store::RhypeStore;

/// Shared HTTP application state, cloned into every axum handler. Holds only the
/// injected data ports + the serve engine handle; the router itself is owned.
#[derive(Clone)]
pub struct AppState {
    /// The typed object store (island API: search, comments).
    pub store: Arc<dyn RhypeStore>,
    /// Prerendered HTML + media originals.
    pub blobs: Arc<dyn BlobStore>,
}

/// The HTTP server. Constructed from [`AppState`], then [`HttpServer::serve`]d.
pub struct HttpServer {
    state: AppState,
}

impl HttpServer {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Build the axum [`Router`]: island API routes + a fallback that serves the
    /// prerendered page (or renders + caches it on miss).
    fn router(&self) -> Router {
        let _ = &self.state;
        // TODO:
        //   Router::new()
        //     .route("/api/search", get(island_search))
        //     .route("/api/comments", post(island_comment))
        //     .route("/api/comments", get(island_list_comments))
        //     .fallback(serve_prerendered_or_render)   // static-first hot path
        //     .with_state(self.state.clone())
        // Static media is served from the BlobStore via a handler, not tower-http
        // ServeDir, so the BlobStore port stays the single source of bytes.
        todo!("assemble the axum Router (island API + prerendered fallback)")
    }

    /// Bind `addr` and serve until shutdown.
    pub async fn serve(self, _addr: SocketAddr) -> ferropress_core::error::Result<()> {
        self.router();
        // TODO:
        //   let listener = tokio::net::TcpListener::bind(addr).await?;
        //   axum::serve(listener, router).await.map_err(...);
        todo!("bind the TcpListener and run axum::serve")
    }
}

/// Convenience free function mirroring the composition-root call shape
/// (`http::serve(state, addr)`).
pub async fn serve(state: AppState, addr: SocketAddr) -> ferropress_core::error::Result<()> {
    HttpServer::new(state).serve(addr).await
}

/// Opaque alias kept local so the signature of [`HttpServer::router`] reads as
/// intended without forcing `axum` into the public API before the router exists.
/// Replaced by `axum::Router<AppState>` (or `axum::Router`) once routes land.
type Router = ();
