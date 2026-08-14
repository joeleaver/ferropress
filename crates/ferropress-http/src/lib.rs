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
use ferropress_serve::{
    AuthorsHandle, ContentIndexHandle, MenuHandle, RedirectHandle, Resolved, SettingsHandle,
    TaxonomyHandle, ThemeHandle,
};

pub mod admin;
pub mod feed;
pub mod island;
pub mod media;

pub use admin::AdminConfig;

/// Shared HTTP application state, cloned into every axum handler. Holds the
/// injected data ports plus the render-side collaborators the SSR fallback needs
/// (the theme host). The router itself is owned.
///
/// `ThemeEngine` is not `Clone` and registering its templates per-request is
/// wasteful, so it is built once at boot and held in a live [`ThemeHandle`] — an
/// `Arc`-swapped engine the read path clones per render and the regen loop rebuilds
/// on an `appearance.theme` change, so a theme switch takes effect with no restart.
#[derive(Clone)]
pub struct AppState {
    /// The typed object store (page resolution + island API).
    pub store: Arc<dyn RhypeStore>,
    /// Prerendered HTML + media originals. (Hot path / media serving: TODO.)
    pub blobs: Arc<dyn BlobStore>,
    /// The live public theme the read path frames pages with. The SAME handle is given to
    /// the `ServeEngine` regen loop, which rebuilds + swaps it on an `appearance.theme`
    /// change — so switching theme is reflected on the next request without a restart and
    /// without page regeneration (envelopes hold theme-agnostic body HTML; the theme chrome
    /// is composed live). Cloned per render via [`ThemeHandle::current`].
    pub theme: ThemeHandle,
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
    /// The live nav-menu set the public read path frames every page's navigation from. The
    /// SAME handle is given to the `ServeEngine` regen loop, which full-reloads it on a
    /// `Menu`/`MenuItem`/`MenuLocation` change — so a menu edit re-frames the nav with no page
    /// regeneration (menus are live chrome). Defaults to an empty set (every location falls
    /// back to its theme default) until seeded via [`with_menus`](Self::with_menus).
    pub menus: MenuHandle,
    /// The live content index the read path resolves nav targets (Post/Page id → href+title)
    /// from. The SAME handle is given to the `ServeEngine` regen loop, which keeps it current
    /// on a `Post`/`Page` change — so a page rename/publish is reflected in every menu
    /// targeting it with no menu edit. Defaults to an empty index (targets resolve to nothing)
    /// until seeded via [`with_content_index`](Self::with_content_index).
    pub content_index: ContentIndexHandle,
    /// The live taxonomy set the read path resolves term archives, post chips, and nav
    /// `Term` targets from. The SAME handle is given to the `ServeEngine` regen loop, which
    /// full-reloads it on a `Taxonomy`/`Term` change — so a re-parent, rename, or new term is
    /// reflected on the public site with no page regeneration (the archive page itself is
    /// evicted separately; this handle only backs live resolution). Defaults to an empty set
    /// (no term resolves) until the composition root seeds it via
    /// [`with_taxonomies`](Self::with_taxonomies).
    pub taxonomies: TaxonomyHandle,
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
    /// Serializes nav-menu-tree-mutating admin writes (the whole-tree items reconcile + a
    /// location assignment) so the in-memory forest validation + the N per-item writes are
    /// atomic w.r.t. a concurrent submission of the same menu. Deliberately SEPARATE from
    /// [`hierarchy_lock`](Self::hierarchy_lock) — a menu reorder and a page re-parent are
    /// unrelated and must not contend. Menu edits are rare, so this is near-uncontended.
    pub menu_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes taxonomy/term-mutating admin writes (term CRUD's per-sibling slug
    /// uniqueness pre-check + create-then-link, and the post save's inline tag creation)
    /// so two concurrent creates can't both pass the app-level uniqueness check —
    /// `Term.slug` is `@indexed`, NOT `@unique` (the same slug may exist in different
    /// taxonomies / under different parents), so the engine enforces nothing and the
    /// pre-check + write must be atomic.
    ///
    /// It ALSO serializes the bidirectional term-archive-vs-page-path collision guard:
    /// the term side checks page paths and the page side checks term archive paths, so
    /// BOTH sides' check + row commit must run under this one lock or each side's check
    /// can pass before the other's commit. Hence the ONE sanctioned lock nesting:
    /// the page handlers acquire [`hierarchy_lock`](Self::hierarchy_lock) THEN
    /// `taxonomy_lock` — always in that order, never the reverse (term handlers take
    /// only `taxonomy_lock`, so no cycle is possible). [`menu_lock`](Self::menu_lock)
    /// is never combined with either. Term/page edits are rare, so this is
    /// near-uncontended.
    pub taxonomy_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    /// Assemble the shared state from the injected ports + theme host. Island asset
    /// serving is off until [`with_islands_dir`](Self::with_islands_dir); custom
    /// blocks render as placeholders until [`with_custom_renderer`](Self::with_custom_renderer).
    pub fn new(store: Arc<dyn RhypeStore>, blobs: Arc<dyn BlobStore>, theme: ThemeHandle) -> Self {
        Self {
            store,
            blobs,
            theme,
            settings: SettingsHandle::default(),
            authors: AuthorsHandle::default(),
            redirects: RedirectHandle::default(),
            menus: MenuHandle::default(),
            content_index: ContentIndexHandle::default(),
            taxonomies: TaxonomyHandle::default(),
            islands_dir: None,
            custom: Arc::new(NoCustomBlocks),
            hooks: Arc::new(NoHooks),
            plugins: Arc::new(NoPlugins),
            admin: None,
            hierarchy_lock: Arc::new(tokio::sync::Mutex::new(())),
            menu_lock: Arc::new(tokio::sync::Mutex::new(())),
            taxonomy_lock: Arc::new(tokio::sync::Mutex::new(())),
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

    /// Share the live [`MenuHandle`] the public read path frames navigation from. The
    /// composition root creates ONE handle (seeded from the store) and gives the same handle to
    /// both this state and the `ServeEngine` regen loop, so a menu edit reloaded by the loop is
    /// immediately visible here.
    pub fn with_menus(mut self, menus: MenuHandle) -> Self {
        self.menus = menus;
        self
    }

    /// Share the live [`ContentIndexHandle`] the public read path resolves nav targets from.
    /// The composition root creates ONE handle (seeded from the store) and gives the same handle
    /// to both this state and the `ServeEngine` regen loop, so a page rename/publish reflected by
    /// the loop is immediately visible here.
    pub fn with_content_index(mut self, content_index: ContentIndexHandle) -> Self {
        self.content_index = content_index;
        self
    }

    /// Share the live [`TaxonomyHandle`] the public read path resolves term archives, chips,
    /// and nav targets from. The composition root creates ONE handle (seeded from the store)
    /// and gives the same handle to both this state and the `ServeEngine` regen loop, so a
    /// term/taxonomy change reloaded by the loop is immediately visible here.
    pub fn with_taxonomies(mut self, taxonomies: TaxonomyHandle) -> Self {
        self.taxonomies = taxonomies;
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
        // Public syndication feeds (`GET /feed.xml` RSS, `GET /feed.atom` Atom) — explicit
        // routes BEFORE the fallback so a post/page slug can never shadow them; un-authenticated
        // and served on every deployment. See [`feed`].
        .route("/feed.xml", get(feed::feed_rss))
        .route("/feed.atom", get(feed::feed_atom))
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

    // A structural `/page/1` suffix is redundant — page 1's canonical URL is the bare base
    // (no suffix; `ferropress_serve`'s own routing never even treats "n == 1" as a pagination
    // suffix — see `strip_page_suffix` — so `/page/1` would otherwise just 404 or fall through
    // to nothing). 301 it to the bare base BEFORE the redirect-table lookup below (this is
    // canonical-URL normalization, not a recorded redirect: nothing is written to the
    // `Redirect` table for it). Claim-only-on-resolve extends here too: fire ONLY when the
    // stripped base actually resolves as the front page or a live archive — the reserved
    // `page` top-level slug guarantees no legitimate PERMALINK content can occupy this exact
    // shape, but a base that resolves to NEITHER must still fall through to the ordinary 404
    // flow rather than 301 to a dead page.
    if let Some(base) = strip_bare_page_one_suffix(&path)
        && (base.is_empty() || state.taxonomies.term_path_owns(base))
    {
        let location = if base.is_empty() {
            "/".to_owned()
        } else {
            format!("/{base}")
        };
        // Pure canonical-URL normalization of the SAME resource (never a recorded redirect),
        // so unlike the Redirect-table 301s below — which forward to an author-authored
        // DIFFERENT destination, where dropping the query is defensible — the query string
        // must be carried across, exactly as WordPress's own canonical redirect does. Query
        // params (UTM tags, etc.) on `/page/1?utm=...` must survive the hop to `/?utm=...`.
        let location = match req.uri().query() {
            Some(q) if !q.is_empty() => format!("{location}?{q}"),
            _ => location,
        };
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response();
    }

    // A moved URL 301s to its new home BEFORE the cache is consulted, so a renamed/re-parented
    // page's old path forwards instead of serving a stale blob or 404ing. The shadow-guard (the
    // admin handler deletes a redirect when a live page later takes that path) keeps a redirect
    // from masking a real page; the table itself never redirects the site root. A live TERM
    // ARCHIVE is the same kind of shadow-guard win: it always wins over a stale 301 recorded
    // before the archive existed (or before a term reused a once-redirected path), so the
    // redirect table is consulted only when no archive currently claims this path — `serve_path`
    // below resolves the archive branch itself on the fall-through. Ownership is checked on the
    // STRIPPED pagination base (see `path_is_front_or_archive_owned`), not just the raw path —
    // otherwise only an archive's bare page-1 path counted as "owned" and a stale 301 recorded
    // at `{archive}/page/{n}` would shadow the archive's own later pages.
    if !path_is_front_or_archive_owned(&state.taxonomies, &path)
        && let Some(target) = state.redirects.lookup(&path)
    {
        let status = StatusCode::from_u16(target.status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
        return (status, [(axum::http::header::LOCATION, target.to)]).into_response();
    }

    match ferropress_serve::serve_path(
        &state.store,
        &state.blobs,
        &state.theme.current(),
        state.custom.as_ref(),
        &state.settings.current(),
        &state.authors.current(),
        &state.menus.current(),
        &state.content_index.current(),
        &state.taxonomies.current(),
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

/// Strip a trailing structural `/page/1` suffix — the exact literal `"page/1"` as the
/// path's last two segments — returning the base beneath it (`""` for the bare
/// `/page/1`). `None` for any other shape, INCLUDING a different page number: `/page/2`
/// must never redirect (n != 1 is a real page), and `/page/1extra` or a non-numeric
/// segment isn't this suffix at all. This is the ONE dedicated "n == 1" case;
/// `ferropress_serve`'s own `strip_page_suffix` deliberately never treats n == 1 as a
/// pagination suffix (this 301 rule is what handles it, one layer up).
fn strip_bare_page_one_suffix(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed == "page/1" {
        Some("")
    } else {
        trimmed.strip_suffix("/page/1")
    }
}

/// Whether `path` is currently claimed by the front page or a live term archive —
/// INCLUDING a paginated `/page/{n}` suffix. Strips the SAME structural suffix
/// [`ferropress_serve::strip_page_suffix`] does before checking ownership of the STRIPPED
/// base, so `/category/fiction/page/2` is recognized as archive-owned exactly like its own
/// page-1 path `/category/fiction` is — closing the asymmetry where only an archive's BARE
/// path counted as "owned" for shadow-guard purposes, letting a stale 301 recorded at
/// `{archive}/page/{n}` mask the archive's own later pages. Falls back to checking the raw
/// path when there is no suffix (the ordinary, unpaginated case) — a non-canonical page-number
/// spelling (rejected by `strip_page_suffix`) is simply treated as an ordinary path here too.
///
/// Shared by both shadow-guard call sites: [`serve_page`]'s redirect-table lookup (serve-time
/// — a live archive/front page always wins over a stale 301) and
/// `admin::content_ops::upsert_redirect` (write-time — never RECORD a redirect FROM a path a
/// live archive/front page currently owns).
pub(crate) fn path_is_front_or_archive_owned(taxonomies: &TaxonomyHandle, path: &str) -> bool {
    let term_path = ferropress_serve::slug_from_path(path);
    // The bare front page itself — never reachable through `strip_page_suffix` (that only ever
    // matches a `/page/{n}` SUFFIX), and never a real `Redirect::from_path` either (the empty
    // string is not a valid Page path), but always front-owned by definition.
    if term_path.is_empty() {
        return true;
    }
    match ferropress_serve::strip_page_suffix(term_path) {
        Some((base, _n)) => base.is_empty() || taxonomies.term_path_owns(base),
        None => taxonomies.term_path_owns(path),
    }
}

#[cfg(test)]
mod tests;
