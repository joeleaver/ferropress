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
use ferropress_core::hook::{HookDispatcher, NoHooks};
use ferropress_core::ports::BlobStore;
use ferropress_core::store::RhypeStore;
use ferropress_render::{CustomBlockRenderer, NoCustomBlocks};
use ferropress_render_form::{NoPlugins, PluginCatalog};
use ferropress_serve::{AuthorsHandle, RedirectHandle, Resolved, SettingsHandle};
use ferropress_theme::ThemeEngine;

pub mod admin;
pub mod island;
pub mod media;

pub use admin::AdminConfig;

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
    /// The live site-settings snapshot the public read path composes chrome from.
    /// The SAME handle is given to the `ServeEngine` regen loop, which refreshes it
    /// on a `Setting` change — so a settings edit is reflected on the public site
    /// with no page regeneration. Defaults to the schema defaults until the
    /// composition root seeds it via [`with_settings`](Self::with_settings).
    pub settings: SettingsHandle,
    /// The live author directory the public read path resolves post bylines from.
    /// The SAME handle is given to the `ServeEngine` regen loop, which refreshes it
    /// on a `User` change — so an author rename is reflected in every post's byline
    /// without page regeneration. Defaults to an empty directory (unresolved ids
    /// render as no byline) until the composition root seeds it via
    /// [`with_authors`](Self::with_authors).
    pub authors: AuthorsHandle,
    /// The live redirect table the page fallback 301s a moved URL from. The SAME handle is
    /// given to the `ServeEngine` regen loop, which reloads it on a `Redirect` change — so a
    /// rename's 301 is honored without page regeneration. Defaults to an empty table (no path
    /// redirects) until the composition root seeds it via [`with_redirects`](Self::with_redirects).
    pub redirects: RedirectHandle,
    /// Directory holding the built wasm island bundle (the `wasm-bindgen` output
    /// of `ferropress-islands`). When set, it is served at `/_fp/islands`; `None`
    /// (e.g. in tests) simply omits that route.
    pub islands_dir: Option<PathBuf>,
    /// Resolves `BlockKind::Custom` (plugin) blocks during page render. Defaults to
    /// [`NoCustomBlocks`] (custom blocks render as placeholders); the composition
    /// root injects the plugin host via [`with_custom_renderer`](Self::with_custom_renderer).
    pub custom: Arc<dyn CustomBlockRenderer>,
    /// Runs WP-style hooks (e.g. the `comment.create` moderation filter). Defaults
    /// to [`NoHooks`] (every event passes through unchanged); the composition root
    /// injects the plugin host via [`with_hook_dispatcher`](Self::with_hook_dispatcher).
    pub hooks: Arc<dyn HookDispatcher>,
    /// Lists loaded plugins + their config schemas for the admin's plugin-config
    /// surface. Defaults to [`NoPlugins`] (no plugin is configurable — the plugin
    /// routes answer an empty list / 404); the composition root injects the plugin
    /// host via [`with_plugin_catalog`](Self::with_plugin_catalog).
    pub plugins: Arc<dyn PluginCatalog>,
    /// Admin API + SPA configuration (signing key, bundle dir, cookie policy). When
    /// `None`, NO `/admin*` route is mounted — a public-only deployment. Injected by
    /// the composition root via [`with_admin`](Self::with_admin).
    pub admin: Option<AdminConfig>,
    /// Serializes page-hierarchy-mutating admin writes (create/save that touch `parent`/`slug`
    /// → the materialized `path`). Held across the cycle-check + path-uniqueness pre-flight and
    /// the writes so two concurrent re-parents can't race into a cycle or a duplicate path. A
    /// process-wide lock shared by every cloned handler; page edits are infrequent, so the
    /// contention is negligible (posts are unaffected — they have no hierarchy).
    pub hierarchy_lock: Arc<tokio::sync::Mutex<()>>,
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
            settings: SettingsHandle::default(),
            authors: AuthorsHandle::default(),
            redirects: RedirectHandle::default(),
            islands_dir: None,
            custom: Arc::new(NoCustomBlocks),
            hooks: Arc::new(NoHooks),
            plugins: Arc::new(NoPlugins),
            admin: None,
            hierarchy_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Share the live [`SettingsHandle`] the public read path composes chrome from.
    /// The composition root creates ONE handle (seeded from the store) and gives
    /// the same handle to both this state and the `ServeEngine` regen loop, so a
    /// settings edit refreshed by the loop is immediately visible here.
    pub fn with_settings(mut self, settings: SettingsHandle) -> Self {
        self.settings = settings;
        self
    }

    /// Share the live [`AuthorsHandle`] the public read path resolves post bylines
    /// from. The composition root creates ONE handle (seeded from the store) and
    /// gives the same handle to both this state and the `ServeEngine` regen loop, so
    /// an author rename refreshed by the loop is immediately visible here.
    pub fn with_authors(mut self, authors: AuthorsHandle) -> Self {
        self.authors = authors;
        self
    }

    /// Share the live [`RedirectHandle`] the page fallback 301s a moved URL from. The
    /// composition root creates ONE handle (seeded from the store) and gives the same handle to
    /// both this state and the `ServeEngine` regen loop, so a rename's 301 reloaded by the loop
    /// is immediately visible here.
    pub fn with_redirects(mut self, redirects: RedirectHandle) -> Self {
        self.redirects = redirects;
        self
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

    /// Run hooks (e.g. the `comment.create` moderation filter) through `hooks`
    /// (the `ferropress-plugin-host`).
    pub fn with_hook_dispatcher(mut self, hooks: Arc<dyn HookDispatcher>) -> Self {
        self.hooks = hooks;
        self
    }

    /// List plugins + fetch their config schemas through `plugins` (the
    /// `ferropress-plugin-host`), for the admin's plugin-config routes.
    pub fn with_plugin_catalog(mut self, plugins: Arc<dyn PluginCatalog>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Enable the admin API + SPA with `admin` (signing key + bundle dir + cookie
    /// policy). Without this, `/admin*` is not routed at all.
    pub fn with_admin(mut self, admin: AdminConfig) -> Self {
        self.admin = Some(admin);
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
/// Static *media* (user content) is served from the [`BlobStore`] via the
/// [`media::serve`] handler (not `ServeDir`) so the port stays the single source of
/// content bytes. The island bundle below is different: it is a *build artifact* (the
/// `wasm-bindgen` output), not content, so it is served straight from the build dir
/// via [`ServeDir`].
pub fn router(state: AppState) -> Router {
    // The media route is built from the shared URL prefix so it can never drift from
    // `ferropress_core::media_url` (what the serve rewrite + editor emit). `{token}` is
    // the `Media.uuid`.
    let media_route = format!("{}{{token}}", ferropress_core::MEDIA_URL_PREFIX);
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
        // Public media originals (`GET /media/{id}`) — un-authenticated, served on
        // every deployment (not gated on `admin`). See [`media`].
        .route(&media_route, get(media::serve))
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

    // Admin API + SPA, only when configured. The API (login/session-guarded posts)
    // is always available once an `AdminConfig` is present; the SPA shell + wasm
    // bundle are additionally gated on a built bundle dir, so the API is testable
    // without a wasm build.
    if let Some(admin_cfg) = &state.admin {
        app = app.merge(admin::api_routes());
        if let Some(dir) = &admin_cfg.bundle_dir {
            app = app
                .route("/admin", get(admin::shell))
                .nest_service("/_fp/admin", ServeDir::new(dir));
        }
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

    // A moved URL 301s to its new home BEFORE the cache is consulted, so a renamed/re-parented
    // page's old path forwards instead of serving a stale blob or 404ing. The shadow-guard (the
    // admin handler deletes a redirect when a live page later takes that path) keeps a redirect
    // from masking a real page; the table itself never redirects the site root.
    if let Some(target) = state.redirects.lookup(&path) {
        let status = StatusCode::from_u16(target.status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
        return (status, [(axum::http::header::LOCATION, target.to)]).into_response();
    }

    match ferropress_serve::serve_path(
        &state.store,
        &state.blobs,
        &state.theme,
        state.custom.as_ref(),
        &state.settings.current(),
        &state.authors.current(),
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
