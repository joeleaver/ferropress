//! Content resolution + on-demand SSR (Ferropress v1 serving path).
//!
//! Given a request *path*, this resolves the published entity behind it, renders
//! its block tree to HTML via [`ferropress_render`], and frames that body in page
//! chrome via [`ferropress_theme`], consuming **live** site settings.
//!
//! ## The cache holds an *envelope*, chrome is composed live
//!
//! The expensive, per-object work — block tree -> HTML plus the media `src`
//! rewrite — is cached as a [`CachedPage`] envelope (body + the object's own
//! metadata). The **chrome** (masthead title/tagline, robots meta, the dateline's
//! formatting) is composed at *request time* from the current [`SiteSettings`], so
//! a settings change is reflected on the next request with **no page
//! regeneration** — the serving model's "don't couple a global setting to every
//! cached page" guardrail. [`serve_path`] deserializes the envelope and composes;
//! the regen loop caches the envelope.
//!
//! ## Permalinks
//!
//! Published-only. A single-segment path `"/<slug>"` resolves a **published** `Post`
//! by slug first, then a top-level **published** `Page` by its materialized `path`. A
//! **nested** path `"/parent/child"` resolves a published `Page` by its full `path`
//! (posts are always flat). The site root `"/"` (empty key) is the cached front page —
//! a static page or the post galley (see [`serve_front`] / [`build_front`]).
//!
//! ## No block -> HTML logic here
//!
//! This module never inspects `BlockKind` or emits markup for a block: the one
//! and only block dispatch lives in `ferropress-render` (the one-shared-renderer
//! invariant). Here we only orchestrate store lookup -> `render` -> theme.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock};

use ferropress_core::error::CoreError;
use ferropress_core::ports::BlobStore;
use ferropress_core::query::Edge;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{
    BlockTree, Compare, FilterSpec, MEDIA_TYPE, Object, PAGE_TYPE, POST_TYPE, Seo, Status,
    TERM_TYPE, is_media_token, media_url,
};
use ferropress_render::{CustomBlockRenderer, RenderMode, render_with};
use ferropress_render_form::SiteSettings;
use ferropress_theme::{ThemeEngine, ThemeError};
use serde::{Deserialize, Serialize};

use crate::authors::AuthorDirectory;
use crate::content_index::ContentIndex;
use crate::datefmt;
use crate::menus::{MenuItemCtx, MenuSet};
use crate::taxonomies::TaxonomySet;
use crate::templates::{HOME_TEMPLATE, template_name_for};
use crate::{archive_cache_key, cache_key};

/// Build a [`ThemeEngine`] for the **default** theme (the built-in letterpress "Composing
/// Room" — `appearance.theme`'s default) from a builtin-only registry. The integration tests
/// and any pre-settings boot use this; the composition root instead builds the registry from
/// the themes dir ([`ThemeRegistry::load_dir`](crate::ThemeRegistry::load_dir)) and the theme
/// named by the live `appearance.theme` setting. Kept as the zero-arg constructor so every test
/// that just wants "a theme" gets the built-in one.
pub fn default_theme() -> Result<ThemeEngine, ThemeError> {
    crate::themes::ThemeRegistry::builtin().build(ferropress_render_form::DEFAULT_THEME)
}

/// A [`ThemeHandle`](crate::ThemeHandle) over a builtin-only registry — the test / pre-settings
/// seam paralleling [`default_theme`]. The composition root instead builds the registry from the
/// themes dir ([`ThemeRegistry::load_dir`](crate::ThemeRegistry::load_dir)) and seeds the handle
/// with the theme named by the live `appearance.theme` setting. There is deliberately no
/// `impl Default for ThemeHandle`: building a theme is fallible, so the constructor stays honest
/// (a `Result`) rather than papering over a parse failure with a panic in `Default`.
pub fn default_theme_handle() -> Result<crate::themes::ThemeHandle, ThemeError> {
    crate::themes::ThemeHandle::new(
        crate::themes::ThemeRegistry::builtin(),
        ferropress_render_form::DEFAULT_THEME,
    )
}

/// Outcome of resolving a request path to a fully rendered HTML document.
///
/// The HTTP layer maps these to status codes (`Found` -> 200, `NotFound` -> 404,
/// `Error` -> 500). `Error` carries the underlying [`CoreError`] *for logging
/// only* — the HTTP layer logs it and returns a generic 500 body, never leaking
/// internals to the client.
#[derive(Debug)]
pub enum Resolved {
    /// A published entity was found and rendered; the `String` is the final
    /// HTML document (body + chrome).
    Found(String),
    /// No published Post or Page matched the path's slug.
    NotFound,
    /// A backend / parse / render fault occurred. The message is for the server
    /// log; it must not be returned to the client.
    Error(CoreError),
}

/// The cached, per-object render of a page: the expensive block-tree -> HTML body
/// (with media `src`s already rewritten) plus the object's own metadata. This is
/// what the prerender cache stores (serialized as JSON). The chrome is NOT baked
/// in — it is composed live from the current [`SiteSettings`] on each request, so
/// the envelope only ever needs regenerating when the *content* changes.
///
/// `#[serde(deny_unknown_fields)]` makes the envelope **fail-closed on any shape
/// drift**: an envelope written by a different (older or newer) format — e.g. a
/// pre-`author_id` envelope that still carries the old resolved `author` name key —
/// fails to deserialize rather than deserializing to a lossy/wrong value. The read
/// path ([`serve_path`]) already treats a deserialize failure as a miss and
/// re-renders live, so a format change **self-heals on first access** instead of
/// silently serving a stale/blank page. (The persistent blob cache has no schema
/// version in its key and no prefix-delete, so this fail-closed-then-re-render is
/// how a cache migration happens.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedPage {
    /// The entry's own title (the `<h1>` and part of the `<title>`).
    pub title: String,
    /// A short summary for the meta description / listing excerpt.
    pub excerpt: String,
    /// Publish instant (epoch millis, UTC) for the live-formatted dateline.
    pub published_at: Option<i64>,
    /// The author's `User` object id (posts only). The BYLINE NAME is **not** baked
    /// here — it is resolved LIVE from the [`AuthorDirectory`](crate::authors) at
    /// compose time, so an author rename is reflected on every one of their posts
    /// with no page regeneration (the cross-entity byline-staleness fix). The *id*
    /// itself is content-stable: it changes only when the post is re-linked to a
    /// different author, which is a Post edit and regenerates this envelope.
    ///
    /// A pre-existing envelope that stored the resolved `author` NAME (and no
    /// `author_id`) is a shape mismatch under `deny_unknown_fields`, so it fails to
    /// deserialize and is re-rendered live on first access — the byline self-heals
    /// rather than silently vanishing.
    pub author_id: Option<u64>,
    /// Featured image URL (`/media/{uuid}`) for the hero, if set.
    pub featured_image: Option<String>,
    /// Whether this is a `Post` (shows a byline) vs a `Page` (does not).
    pub is_post: bool,
    /// The `Post.terms` M:N membership as BAKED ids only (posts only; a `Page` has no
    /// `terms` relation so this is always empty for one). The chip NAME + archive href are
    /// **not** baked here — they are resolved LIVE from the current
    /// [`TaxonomySet`](crate::taxonomies::TaxonomySet) at compose time (same discipline as
    /// [`author_id`](Self::author_id)'s byline name), so a term rename/re-parent is
    /// reflected on the next request with no page regeneration. Direct assignments only —
    /// never rolled up to ancestors. `#[serde(default)]` so a pre-taxonomy legacy envelope
    /// (missing this key entirely) deserializes to an empty chip list rather than failing
    /// (the `template` field's precedent).
    #[serde(default)]
    pub term_ids: Vec<u64>,
    /// The chosen theme template VALUE for a page (e.g. `"page-wide"`), or `None` for the
    /// default single template. Content-stable (a template edit is a Page save that
    /// regenerates this envelope), so it is cached rather than resolved live. Posts never set
    /// it. Composed at request time via
    /// [`template_name_for`](crate::templates::template_name_for), which falls back to the
    /// default on an unknown/absent value — so a legacy envelope with no `template` key
    /// deserializes to `None` (serde defaults a missing `Option`) and renders the default with
    /// NO re-render.
    #[serde(default)]
    pub template: Option<String>,
    /// Stored SEO metadata (canonical/description), if present.
    pub seo: Option<Seo>,
    /// The rendered, media-rewritten block body (emitted `| safe`).
    pub body: String,
}

/// The cached front page: either a configured static [`Page`](CachedFront::Static) or the
/// latest-posts [`galley`](CachedFront::Galley). Stored (serialized as JSON) at
/// [`cache_key`](crate::cache_key)`("/")`. Like [`CachedPage`], it holds only
/// **content-stable** data — the static case reuses the per-object envelope; the galley
/// stores raw `published_at` millis + author ids. The chrome, datelines, and byline names
/// are composed **live** in [`compose_front`], so a settings edit or an author rename is
/// reflected with no `/` regeneration.
///
/// The regen loop never *builds* this — it only **evicts** `/` when a change can reshape it
/// (see [`ServeEngine::apply_change`](crate::ServeEngine)); the read path ([`serve_front`])
/// is the sole populator, rebuilding on the next request. An externally-tagged enum rejects
/// an unknown variant tag on deserialize, and the inner structs are `deny_unknown_fields`,
/// so any format drift fails to deserialize and self-heals on first access (the same
/// discipline as [`CachedPage`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum CachedFront {
    /// The configured static front page (a published `Page`): the SAME per-object envelope
    /// its own permalink caches, composed with `is_home = true`. Boxed so the (much larger)
    /// static variant does not bloat every galley envelope; the `Box` is transparent to serde
    /// (the on-disk JSON is identical to an unboxed `CachedPage`).
    Static(Box<CachedPage>),
    /// The latest-posts galley (newest first, capped at `posts_per_page`).
    Galley(Vec<CachedHomePost>),
}

/// One content-stable galley row. The dateline is formatted and the byline name is resolved
/// LIVE at compose time (from the settings + the author directory), so only the raw
/// `published_at` millis + the author id are cached — an author rename or a date-format
/// change needs no `/` regeneration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedHomePost {
    /// The post title (galley card headline + link text).
    pub title: String,
    /// The post permalink (`"/<slug>"`).
    pub url: String,
    /// A short summary for the galley card.
    pub excerpt: String,
    /// Publish instant (epoch millis, UTC) for the live-formatted dateline.
    pub published_at: Option<i64>,
    /// The author's `User` id; the byline NAME is resolved live from the author directory.
    pub author_id: Option<u64>,
    /// The post's `terms` M:N membership as baked ids only — same discipline as
    /// [`CachedPage::term_ids`], resolved to live chips at compose time. `#[serde(default)]`
    /// for legacy-envelope missing-key tolerance (the `Option` fields above predate this
    /// field and are NOT independently default-tolerant; only newly-added fields need it).
    #[serde(default)]
    pub term_ids: Vec<u64>,
}

/// The cached, content-stable render of a term archive listing (`/{taxonomy_key}/{chain}`):
/// the rolled-up post rows (own term + every descendant, deduped, newest first) for ONE page.
/// Like [`CachedFront::Galley`], the heading — the term's NAME and description — is NOT baked
/// here: it is resolved LIVE from the current [`TaxonomySet`] at compose time, so a term rename
/// needs no archive regeneration (a rename doesn't even reach here — the taxonomy slice's
/// pinned strategy blunt-EVICTS every archive on any `Taxonomy`/`Term` change instead of
/// precisely invalidating, so this envelope's `rows` are always rebuilt fresh on the next
/// request after one anyway; the live-heading discipline is what keeps a *hit* correct too, in
/// the window before that evict lands).
///
/// `page` exists from D1 on (always `1` here) so D2's `/page/{n}` pagination adds no new
/// envelope shape — no blob migration when it lands, just a wider range of valid `page` values
/// and `total`-derived page count. `deny_unknown_fields` for the same fail-closed-then-re-render
/// self-heal every other envelope uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedTermArchive {
    /// The term this archive lists (its own posts ∪ every descendant's, deduped). Resolved
    /// live at compose time for the heading name/description/href — never baked, so a rename
    /// or re-parent needs no regeneration (though the blunt evict above rebuilds it anyway).
    pub term_id: u64,
    /// This page's rows, reusing [`CachedHomePost`] so a row's dateline/byline/chips resolve
    /// identically to the front-page galley.
    pub rows: Vec<CachedHomePost>,
    /// The FULL deduped, published-only post count across the whole rollup (before slicing to
    /// one page) — the archive heading's count and D2's page-count arithmetic.
    pub total: usize,
    /// The 1-based page number this envelope holds. Always `1` in D1.
    pub page: usize,
}

/// Derive the lookup **path key** from a request path.
///
/// Strips a single leading `'/'` and any trailing `'/'`; the remainder is the key.
/// `"/"` (the site root) yields an empty key, which the read paths route to the
/// front-page galley. A single segment (`"about"`) keys a Post slug or a top-level
/// Page path; a nested key (`"about/team"`) keys a nested Page's materialized `path`
/// (see [`resolve_published_entity`]).
pub fn slug_from_path(path: &str) -> &str {
    path.trim_start_matches('/').trim_end_matches('/')
}

/// Resolve a request path to a rendered HTML document (v1 SSR-on-demand, uncached).
///
/// The site root renders the front-page galley; any other path resolves the
/// published `Post` then `Page` behind its slug, builds the [`CachedPage`]
/// envelope, and composes chrome around it. The cache-first hot path is
/// [`serve_path`]; this is the uncached form used by tests + `resolve` callers.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_path(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    path: &str,
) -> Resolved {
    let term_path = slug_from_path(path);
    if term_path.is_empty() {
        return front_page(
            store, theme, custom, settings, authors, menus, index, taxonomies,
        )
        .await;
    }

    // Claim-only-on-resolve, mirroring `serve_path`'s archive branch (kept in parity here too,
    // even though `resolve_path` has no production caller today — the uncached form must
    // resolve the SAME set of paths the cache-first hot path does, or the two would silently
    // diverge on what "this path exists" means). A bare taxonomy key (`/category`, no chain)
    // does not resolve, so it falls through untouched to the ordinary permalink flow below.
    if let Some(term_id) = taxonomies.resolve_archive_path(term_path) {
        return match build_archive(store, taxonomies, settings, term_id).await {
            Ok(archive) => {
                match compose_archive(theme, settings, authors, menus, index, taxonomies, &archive)
                {
                    Ok(html) => Resolved::Found(html),
                    Err(e) => Resolved::Error(e),
                }
            }
            Err(e) => Resolved::Error(e),
        };
    }

    match build_page(store, custom, path).await {
        Ok(Some(page)) => {
            match compose_single(
                theme,
                settings,
                authors,
                menus,
                index,
                taxonomies,
                &page,
                false,
                None,
                Some(path),
            ) {
                Ok(html) => Resolved::Found(html),
                Err(e) => Resolved::Error(e),
            }
        }
        Ok(None) => Resolved::NotFound,
        Err(e) => Resolved::Error(e),
    }
}

/// Cache-first resolution: the static-first hot path the HTTP fallback calls.
///
/// The site root is rendered live (the galley depends on the whole post set +
/// `posts_per_page`, so it is not blob-cached in v1). For a permalink:
///
/// 1. Try the prerender cache (`blobs.get(cache_key(path))`). On a hit, deserialize
///    the [`CachedPage`] envelope and compose chrome around it with the current
///    settings — **no store lookup, no block re-render**. A corrupt/legacy entry
///    that fails to deserialize is treated as a miss.
/// 2. On a miss, build the envelope (block tree -> HTML + metadata), write it
///    *through* to the cache, and compose. `NotFound` / `Error` are never cached.
///
/// The cache is **best-effort**: a blob read or write failure never fails the
/// request — it degrades to render-on-demand, never a 500. The change-driven regen
/// loop ([`ServeEngine::regen_loop`](crate::ServeEngine::regen_loop)) keeps
/// populated envelopes fresh.
#[allow(clippy::too_many_arguments)]
pub async fn serve_path(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    path: &str,
) -> Resolved {
    let term_path = slug_from_path(path);
    if term_path.is_empty() {
        // The front page is blob-cached too (a `CachedFront` envelope at `/`), composed
        // live from the current settings + author directory. `serve_front` is cache-first
        // (build + write-through on a miss); the change-driven regen loop EVICTS `/` when a
        // content/settings change can reshape it, and this read path is the sole populator.
        return serve_front(
            store, blobs, theme, custom, settings, authors, menus, index, taxonomies,
        )
        .await;
    }

    // Claim-only-on-resolve: intercept BEFORE the permalink cache key/read below, so a live
    // term archive always wins at its own path. `resolve_archive_path` requires a FULL chain
    // under a live taxonomy key — a bare `/category` (the key alone) does not resolve, so it
    // falls through untouched to the ordinary permalink flow (and, from there, to the normal
    // 404 if nothing else claims it either). `None` here is never write-through-cached — there
    // is nothing to cache for a path nobody claims.
    if let Some(term_id) = taxonomies.resolve_archive_path(term_path) {
        return serve_archive(
            store, blobs, theme, settings, authors, menus, index, taxonomies, term_id, term_path,
        )
        .await;
    }

    let key = cache_key(path);

    // 1. Cache read. A hit -> compose chrome around the stored envelope. A
    //    `NotFound` is an ordinary miss; any other error (or a non-envelope entry)
    //    degrades to a fresh render.
    match blobs.get(&key).await {
        Ok(bytes) => match serde_json::from_slice::<CachedPage>(&bytes) {
            Ok(page) => {
                return match compose_single(
                    theme,
                    settings,
                    authors,
                    menus,
                    index,
                    taxonomies,
                    &page,
                    false,
                    None,
                    Some(path),
                ) {
                    Ok(html) => Resolved::Found(html),
                    Err(e) => Resolved::Error(e),
                };
            }
            Err(e) => {
                // The entry doesn't match the current `CachedPage` shape: either a
                // corrupt/non-envelope blob, or — after a format change — a legacy
                // envelope rejected by `deny_unknown_fields` (e.g. one that still
                // carries the old resolved `author` name). Either way, don't serve
                // garbage or a lossy render: fall through to re-render live and
                // write-through the current-format envelope (the cache self-heals on
                // first access). One log line per stale entry, once.
                tracing::warn!(%path, error = %e, "prerender cache entry not in the current envelope format; re-rendering");
            }
        },
        Err(CoreError::NotFound { .. }) => {
            // Ordinary cache miss — fall through to render-on-demand.
        }
        Err(e) => {
            tracing::warn!(%path, error = %e, "prerender cache read failed; falling back to render");
        }
    }

    // 2. Miss: build the envelope, populate the cache (write-through), compose.
    match build_page(store, custom, path).await {
        Ok(Some(page)) => {
            match serde_json::to_vec(&page) {
                Ok(bytes) => {
                    if let Err(e) = blobs.put(&key, bytes).await {
                        // Populate-on-miss is best-effort: a write failure must not
                        // fail the request — log it and serve the render anyway.
                        tracing::warn!(%path, error = %e, "prerender cache write-through failed; serving uncached render");
                    }
                }
                Err(e) => {
                    tracing::warn!(%path, error = %e, "could not serialize page envelope for the cache; serving uncached render");
                }
            }
            match compose_single(
                theme,
                settings,
                authors,
                menus,
                index,
                taxonomies,
                &page,
                false,
                None,
                Some(path),
            ) {
                Ok(html) => Resolved::Found(html),
                Err(e) => Resolved::Error(e),
            }
        }
        Ok(None) => Resolved::NotFound,
        Err(e) => Resolved::Error(e),
    }
}

/// Cache-first resolution of the site root (`/`): the static-first hot path for the front
/// page, mirroring [`serve_path`] for permalinks.
///
/// 1. Try the prerender cache (`blobs.get(cache_key("/"))`). On a hit, deserialize the
///    [`CachedFront`] envelope and compose it live (chrome + datelines + bylines from the
///    current settings + author directory) — **no store scan, no block re-render**. A
///    corrupt/legacy entry that fails to deserialize is treated as a miss (self-heal).
/// 2. On a miss, [`build_front`] the envelope (a static Page or the galley scan), write it
///    *through* to the cache, and compose.
///
/// The cache is **best-effort** (a blob fault degrades to a live render, never a 500). The
/// regen loop keeps `/` fresh by **evicting** it on a reshaping change; this read path is the
/// sole *populator*, so there is no eager-regen writer to race a PUT against. A narrow
/// residual window remains — a write-through that builds from state S then lands just after a
/// concurrent evict can re-cache pre-change content — the same best-effort read-vs-invalidate
/// class the permalink path ([`serve_path`]) already carries; it self-clears on the next
/// reshaping change (and a post/page/setting change is a broad trigger set).
#[allow(clippy::too_many_arguments)]
async fn serve_front(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
) -> Resolved {
    let key = cache_key("/");

    match blobs.get(&key).await {
        Ok(bytes) => match serde_json::from_slice::<CachedFront>(&bytes) {
            Ok(front) => {
                return match compose_front(
                    theme, settings, authors, menus, index, taxonomies, &front,
                ) {
                    Ok(html) => Resolved::Found(html),
                    Err(e) => Resolved::Error(e),
                };
            }
            Err(e) => {
                // Not the current `CachedFront` shape (corrupt, or a legacy/format-drifted
                // entry rejected by the enum tag / `deny_unknown_fields`): re-render live and
                // write-through the current format — the cache self-heals on first access.
                tracing::warn!(error = %e, "home prerender cache entry not in the current format; re-rendering");
            }
        },
        Err(CoreError::NotFound { .. }) => {
            // Ordinary cache miss — fall through to render-on-demand.
        }
        Err(e) => {
            tracing::warn!(error = %e, "home prerender cache read failed; falling back to render");
        }
    }

    match build_front(store, custom, settings).await {
        Ok(front) => {
            match serde_json::to_vec(&front) {
                Ok(bytes) => {
                    if let Err(e) = blobs.put(&key, bytes).await {
                        tracing::warn!(error = %e, "home prerender cache write-through failed; serving uncached render");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not serialize the home envelope for the cache; serving uncached render");
                }
            }
            match compose_front(theme, settings, authors, menus, index, taxonomies, &front) {
                Ok(html) => Resolved::Found(html),
                Err(e) => Resolved::Error(e),
            }
        }
        Err(e) => Resolved::Error(e),
    }
}

/// Cache-first resolution of one term archive (`/{taxonomy_key}/{chain}`), mirroring
/// [`serve_front`] for the site root: try the archive-namespaced cache
/// ([`archive_cache_key`]) and compose live on a hit; on a miss, [`build_archive`] (the
/// rollup), write-through, and compose. `term_path` is the ALREADY-RESOLVED, trimmed chain
/// (the caller's `taxonomies.resolve_archive_path` succeeded on it) — the exact string both
/// the cache key and the archive's own canonical href are built from, so they can never drift.
///
/// An archive whose rollup is empty is a VALID page (the empty-galley shape), never a 404 —
/// only a path nobody's `resolve_archive_path` claims 404s, and that never reaches this
/// function at all (the caller's job). The cache is best-effort, exactly like [`serve_front`].
#[allow(clippy::too_many_arguments)]
async fn serve_archive(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
    theme: &ThemeEngine,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    term_id: u64,
    term_path: &str,
) -> Resolved {
    let key = archive_cache_key(term_path);

    match blobs.get(&key).await {
        Ok(bytes) => match serde_json::from_slice::<CachedTermArchive>(&bytes) {
            Ok(archive) => {
                return match compose_archive(
                    theme, settings, authors, menus, index, taxonomies, &archive,
                ) {
                    Ok(html) => Resolved::Found(html),
                    Err(e) => Resolved::Error(e),
                };
            }
            Err(e) => {
                // Not the current `CachedTermArchive` shape: re-render live and write-through
                // the current format — the cache self-heals on first access, same discipline
                // as every other envelope here.
                tracing::warn!(%term_path, error = %e, "term-archive prerender cache entry not in the current format; re-rendering");
            }
        },
        Err(CoreError::NotFound { .. }) => {
            // Ordinary cache miss — fall through to render-on-demand.
        }
        Err(e) => {
            tracing::warn!(%term_path, error = %e, "term-archive prerender cache read failed; falling back to render");
        }
    }

    match build_archive(store, taxonomies, settings, term_id).await {
        Ok(archive) => {
            match serde_json::to_vec(&archive) {
                Ok(bytes) => {
                    if let Err(e) = blobs.put(&key, bytes).await {
                        tracing::warn!(%term_path, error = %e, "term-archive prerender cache write-through failed; serving uncached render");
                    }
                }
                Err(e) => {
                    tracing::warn!(%term_path, error = %e, "could not serialize the term-archive envelope for the cache; serving uncached render");
                }
            }
            match compose_archive(theme, settings, authors, menus, index, taxonomies, &archive) {
                Ok(html) => Resolved::Found(html),
                Err(e) => Resolved::Error(e),
            }
        }
        Err(e) => Resolved::Error(e),
    }
}

/// Build the [`CachedPage`] envelope for a permalink, or `None` if no PUBLISHED
/// entity backs its slug. Resolves the entity, renders + media-rewrites its body,
/// and gathers its metadata (author, featured image, SEO). `pub(crate)` so the
/// regen loop caches the envelope it produces.
pub(crate) async fn build_page(
    store: &Arc<dyn RhypeStore>,
    custom: &dyn CustomBlockRenderer,
    path: &str,
) -> Result<Option<CachedPage>, CoreError> {
    let slug = slug_from_path(path);
    let (type_name, object) = match resolve_published_entity(store, slug).await? {
        Some(pair) => pair,
        None => return Ok(None),
    };
    Ok(Some(
        cached_page_from_object(store, custom, RenderMode::Publish, type_name, &object).await?,
    ))
}

/// Build the [`CachedTermArchive`] envelope for `term_id`'s rolled-up listing (page 1 — D1 has
/// no pagination yet, but the envelope already carries `page` so D2 adds no new shape).
/// `pub(crate)` so the regen loop could cache it directly in a future increment; today only
/// the read path ([`serve_archive`], and `resolve_path`'s archive branch) builds it, on a miss.
pub(crate) async fn build_archive(
    store: &Arc<dyn RhypeStore>,
    taxonomies: &TaxonomySet,
    settings: &SiteSettings,
    term_id: u64,
) -> Result<CachedTermArchive, CoreError> {
    let limit = settings.posts_per_page as usize;
    let (published, author_links, term_links, total) =
        rollup_published_posts(store, taxonomies, term_id, limit).await?;

    let rows = published
        .iter()
        .zip(author_links.iter())
        .zip(term_links.iter())
        .map(|((obj, author_ids), row_term_ids)| to_cached_home_post(obj, author_ids, row_term_ids))
        .collect();

    Ok(CachedTermArchive {
        term_id,
        rows,
        total,
        page: 1,
    })
}

/// Render a specific (possibly **unpublished**) object through the real public theme,
/// **uncached**, for the authenticated new-tab draft preview.
///
/// This is the WordPress-style "preview draft in the real theme" path. It deliberately
/// reuses the exact serve pipeline — [`cached_page_from_object`] (the one shared block
/// renderer + media rewrite + metadata) framed by [`compose_single`] against the live
/// [`SiteSettings`] — so a preview is byte-for-byte what publishing would produce,
/// EXCEPT for the chrome preview banner and a forced `noindex`. It differs from
/// [`serve_path`]/[`resolve_path`] in two ways only:
///   * **No publish gate.** The caller (the admin preview route) has already loaded
///     and authorized the object, so a draft/pending/scheduled entity renders instead
///     of 404ing. The permalink read paths must never do this — only this authed path.
///   * **No cache.** Nothing is read from or written to the prerender [`BlobStore`]
///     cache, so previewing a draft can never populate the public cache with
///     unpublished content, and a preview always reflects the object as it is *now*.
///
/// `mode` is [`RenderMode::Preview`]; `type_name` is the object's store type
/// ([`POST_TYPE`]/[`PAGE_TYPE`]). The banner carries the object's status label.
#[allow(clippy::too_many_arguments)]
pub async fn render_preview(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    type_name: &'static str,
    obj: &Object,
) -> Resolved {
    let label = status_label(obj);
    match cached_page_from_object(store, custom, RenderMode::Preview, type_name, obj).await {
        // A preview shows the real theme's nav (composed from the live menus + index), but no
        // item is ever "current": a draft has no public URL, so `current_path` is `None`.
        Ok(page) => match compose_single(
            theme,
            settings,
            authors,
            menus,
            index,
            taxonomies,
            &page,
            false,
            Some(&label),
            None,
        ) {
            Ok(html) => Resolved::Found(html),
            Err(e) => Resolved::Error(e),
        },
        Err(e) => Resolved::Error(e),
    }
}

/// A human-friendly banner label for a preview from the object's raw `status`
/// (`"draft"` → `"Draft"`). Empty/absent status falls back to `"Draft"` (a post always
/// carries one; this is belt-and-suspenders).
fn status_label(obj: &Object) -> String {
    let raw = str_field(obj, "status");
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Draft".to_owned(),
    }
}

/// Render `object` into a [`CachedPage`] envelope: the block body (media-rewritten)
/// plus the metadata the chrome needs. The block dispatch is solely
/// `ferropress_render::render_with` (the one-shared-renderer invariant); `custom`
/// resolves plugin blocks.
///
/// `mode` is threaded straight to the renderer. The public serve/regen paths pass
/// [`RenderMode::Publish`]; the authenticated draft preview passes
/// [`RenderMode::Preview`]. Block output is deliberately **mode-independent** — the
/// preview's whole value is showing exactly what will publish (the
/// what-you-see-is-what-you-publish invariant), so the "this is a preview" signal
/// lives in the chrome, never in the block HTML.
pub(crate) async fn cached_page_from_object(
    store: &Arc<dyn RhypeStore>,
    custom: &dyn CustomBlockRenderer,
    mode: RenderMode,
    type_name: &'static str,
    obj: &Object,
) -> Result<CachedPage, CoreError> {
    // `block_tree` is persisted as a native `Value::Json` (rhypedb Json scalar).
    let tree = match obj.get("block_tree") {
        Some(Value::Json(j)) => BlockTree::from_json_value(j.clone())?,
        other => {
            return Err(CoreError::TypeMismatch {
                type_name: obj.type_name.as_str().to_owned(),
                field: "block_tree".to_owned(),
                detail: format!("expected JSON, got {other:?}"),
            });
        }
    };

    // Render the block body, then rewrite `data-media-id` placeholders into real
    // media `src`s — the serve layer's job, not the pure renderer's. This runs
    // pre-cache so cached envelopes always carry final URLs.
    let body = render_body(&tree, mode, custom);

    let is_post = type_name == POST_TYPE;

    Ok(CachedPage {
        title: str_field(obj, "title"),
        excerpt: str_field(obj, "excerpt"),
        published_at: obj.get("published_at").and_then(Value::as_datetime),
        // A byline only makes sense on posts; pages have no author line (WP parity).
        // Cache only the author's *id* — the display name is resolved live at compose
        // time from the author directory, so a rename needs no page regeneration.
        author_id: if is_post {
            single_link(store, type_name, obj.id, "author")
                .await
                .map(|oid| oid.0)
        } else {
            None
        },
        featured_image: featured_image_url(store, type_name, obj.id).await,
        is_post,
        // `Post.terms` is a Post-only relation — a Page has no `terms` field at all, so it
        // always bakes an empty chip list (WP parity, mirroring the `author_id` gate above).
        term_ids: if is_post {
            multi_link(store, type_name, obj.id, "terms").await
        } else {
            Vec::new()
        },
        // The page's chosen template value (posts carry no `template` field → None). An empty
        // string is treated as the default (None), matching the "" default in `page_templates`.
        template: match obj.get("template") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        },
        seo: obj
            .get("seo")
            .and_then(Value::as_json)
            .and_then(|j| serde_json::from_value::<Seo>(j.clone()).ok()),
        body,
    })
}

/// Compose the final single-page HTML: frame the cached envelope in chrome, with
/// the dateline formatted live per `settings.date_format` + `settings.timezone`.
///
/// `is_home` is `true` only when this page is standing in as the static front page
/// (the masthead marks the Front-page nav link current); an ordinary permalink or
/// preview passes `false`.
///
/// `preview_status` distinguishes the two callers: `None` is the ordinary public
/// render; `Some(label)` is the authenticated draft preview, which surfaces the
/// preview banner (the template reads `preview_status`) and forces `noindex`
/// regardless of the site's search-engine-visibility setting — a private,
/// unpublished view must never be indexable.
#[allow(clippy::too_many_arguments)]
fn compose_single(
    theme: &ThemeEngine,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    page: &CachedPage,
    is_home: bool,
    preview_status: Option<&str>,
    current_path: Option<&str>,
) -> Result<String, CoreError> {
    let dateline = page
        .published_at
        .map(|ms| datefmt::format_datetime(ms, &settings.date_format, &settings.timezone));

    // Resolve the byline name LIVE from the author directory (posts only): the
    // envelope carries only the author id, so a `User` rename shows here on the next
    // request with no page regeneration. An unknown id (e.g. a legacy envelope with
    // no id, or an author since deleted) simply renders as no byline.
    let author = if page.is_post {
        page.author_id.and_then(|id| authors.name(id))
    } else {
        None
    };

    let mut site = SiteCtx::from(settings);
    if preview_status.is_some() {
        // A preview is an authenticated view of unpublished (or not-yet-live) content:
        // keep it out of every index no matter what the site setting says.
        site.noindex = true;
    }

    let ctx = SingleCtx {
        page_title: page_title(&page.title, settings),
        page_description: meta_description(page),
        canonical: page.seo.as_ref().and_then(|s| s.canonical_url.as_deref()),
        site,
        is_home,
        preview_status,
        nav: menus.compose(index, taxonomies, current_path),
        title: &page.title,
        dateline,
        kicker: None,
        author,
        author_initials: author.map(initials).unwrap_or_default(),
        featured_image: page.featured_image.as_deref(),
        terms: resolve_chips(taxonomies, &page.term_ids),
        body: &page.body,
    };

    // Pick the page's chosen template (default single for a post, an unset page, or an
    // unknown/dropped value — `template_name_for` is the membership-checked fallback).
    theme
        .render(template_name_for(page.template.as_deref()), &ctx)
        // ThemeError does not convert to CoreError; carry its message so the HTTP
        // layer can log it and return a generic 500.
        .map_err(|e| CoreError::Store(format!("theme render failed: {e}")))
}

/// Resolve the site root **uncached** (the SSR-on-demand form used by [`resolve_path`] +
/// tests): [`build_front`] the envelope, then [`compose_front`] it live. The cache-first
/// hot path is [`serve_front`]; both share `build_front`/`compose_front`, so the cached and
/// uncached front pages are byte-for-byte identical.
#[allow(clippy::too_many_arguments)]
async fn front_page(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
) -> Resolved {
    match build_front(store, custom, settings).await {
        Ok(front) => {
            match compose_front(theme, settings, authors, menus, index, taxonomies, &front) {
                Ok(html) => Resolved::Found(html),
                Err(e) => Resolved::Error(e),
            }
        }
        Err(e) => Resolved::Error(e),
    }
}

/// Build the [`CachedFront`] envelope for the site root, or resolve which shape it takes.
///
/// A configured static front page ([`settings.front_page_id`](SiteSettings)) wins when it
/// exists AND is published, producing a [`CachedFront::Static`] carrying the SAME per-object
/// envelope its own permalink caches (composed later with `is_home = true`). A missing /
/// unpublished / trashed target FALLS BACK to the [`CachedFront::Galley`] rather than 404ing
/// or leaking an unpublished page — the front page is never a dead end. The by-id lookup
/// bypasses the slug publish gate (the id came from a trusted setting, not a public slug),
/// so this re-checks [`is_published`] itself.
///
/// `pub(crate)` so the read path ([`serve_front`], [`front_page`]) builds it on a miss. The
/// regen loop never calls this — it only *evicts* `/`.
pub(crate) async fn build_front(
    store: &Arc<dyn RhypeStore>,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
) -> Result<CachedFront, CoreError> {
    if let Some(page_id) = settings.front_page_id {
        match store
            .get(&TypeName::from(PAGE_TYPE), ObjectId(page_id))
            .await
        {
            Ok(obj) if is_published(&obj) => {
                let page =
                    cached_page_from_object(store, custom, RenderMode::Publish, PAGE_TYPE, &obj)
                        .await?;
                return Ok(CachedFront::Static(Box::new(page)));
            }
            // Unpublished target -> galley fallback (must never surface publicly).
            Ok(_) => {}
            // Dangling id -> galley fallback.
            Err(CoreError::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(CachedFront::Galley(build_galley(store, settings).await?))
}

/// Compose the final front-page HTML from a cached [`CachedFront`], live. The static case
/// is framed exactly like its own permalink (only the `is_home` nav flag differs); the
/// galley formats each row's dateline (from the live settings) and resolves its byline name
/// (from the live [`AuthorDirectory`]) — so a settings edit or an author rename is reflected
/// with no `/` regeneration, exactly as on the single-page path.
fn compose_front(
    theme: &ThemeEngine,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    front: &CachedFront,
) -> Result<String, CoreError> {
    // The front page's own path is the site root, so nav items pointing at `/` are current.
    let current_path = Some("/");
    match front {
        CachedFront::Static(page) => compose_single(
            theme,
            settings,
            authors,
            menus,
            index,
            taxonomies,
            page,
            true,
            None,
            current_path,
        ),
        CachedFront::Galley(rows) => {
            let posts: Vec<PostSummary> = rows
                .iter()
                .map(|p| PostSummary {
                    title: p.title.clone(),
                    url: p.url.clone(),
                    excerpt: p.excerpt.clone(),
                    dateline: p.published_at.map(|ms| {
                        datefmt::format_datetime(ms, &settings.date_format, &settings.timezone)
                    }),
                    author: p
                        .author_id
                        .and_then(|id| authors.name(id))
                        .map(str::to_owned),
                    terms: resolve_chips(taxonomies, &p.term_ids),
                })
                .collect();

            let ctx = HomeCtx {
                page_title: settings.title_or_default().to_owned(),
                page_description: if settings.tagline.is_empty() {
                    None
                } else {
                    Some(settings.tagline.clone())
                },
                canonical: None,
                site: SiteCtx::from(settings),
                is_home: true,
                preview_status: None,
                nav: menus.compose(index, taxonomies, current_path),
                archive: None,
                posts,
            };

            theme
                .render(HOME_TEMPLATE, &ctx)
                .map_err(|e| CoreError::Store(format!("theme render failed: {e}")))
        }
    }
}

/// Compose a term archive's final HTML from its cached [`CachedTermArchive`] envelope, live —
/// REUSES [`HOME_TEMPLATE`] (same shape as the front-page galley: an eyebrow + an `<ol>` of
/// rows), with `archive: Some(ArchiveCtx { .. })` swapping the theme's eyebrow for the term's
/// own heading. The heading's name/description, the canonical href, and the nav's
/// `current_path` are ALL resolved live from `taxonomies` — the envelope bakes only `term_id`
/// (identity-independence: a rename/re-parent needs no regeneration).
///
/// A term id the current [`TaxonomySet`] can no longer resolve (only reachable in the narrow
/// window between a rename/delete and the taxonomy slice's blunt archive evict landing — see
/// [`ServeEngine::apply_change`](crate::ServeEngine)) degrades to an EMPTY heading rather than
/// erroring: the row list still renders, the page just self-heals fully once the evict catches
/// up on the very next request.
fn compose_archive(
    theme: &ThemeEngine,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    menus: &MenuSet,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    archive: &CachedTermArchive,
) -> Result<String, CoreError> {
    let entry = taxonomies.term(archive.term_id);
    let name = entry.map(|e| e.name.clone()).unwrap_or_default();
    let description = entry.and_then(|e| {
        if e.description.trim().is_empty() {
            None
        } else {
            Some(e.description.clone())
        }
    });
    let href = taxonomies.archive_href(archive.term_id);
    let current_path = href.as_deref();
    let canonical = href
        .as_deref()
        .map(|h| format!("{}{h}", settings.url.trim_end_matches('/')));

    let posts: Vec<PostSummary> = archive
        .rows
        .iter()
        .map(|p| PostSummary {
            title: p.title.clone(),
            url: p.url.clone(),
            excerpt: p.excerpt.clone(),
            dateline: p
                .published_at
                .map(|ms| datefmt::format_datetime(ms, &settings.date_format, &settings.timezone)),
            author: p
                .author_id
                .and_then(|id| authors.name(id))
                .map(str::to_owned),
            terms: resolve_chips(taxonomies, &p.term_ids),
        })
        .collect();

    let ctx = HomeCtx {
        page_title: page_title(&name, settings),
        page_description: description.clone(),
        canonical,
        site: SiteCtx::from(settings),
        is_home: false,
        preview_status: None,
        nav: menus.compose(index, taxonomies, current_path),
        archive: Some(ArchiveCtx {
            name,
            description,
            total: archive.total,
        }),
        posts,
    };

    theme
        .render(HOME_TEMPLATE, &ctx)
        .map_err(|e| CoreError::Store(format!("theme render failed: {e}")))
}

/// The most-recent published posts for the galley, newest first, capped at
/// `settings.posts_per_page`, as content-stable [`CachedHomePost`] rows (raw `published_at`
/// millis + the author *id* + the `terms` membership ids — the dateline is formatted and the
/// byline name + term chips resolved live in [`compose_front`], so a settings edit, an author
/// rename, or a term rename needs no `/` regeneration).
///
/// Scans posts and filters/sorts in Rust: v1 has no compound "status == published ORDER BY
/// published_at DESC LIMIT n" query primitive, and the post table is small. Posts with no
/// `published_at` sort last (keyed as `i64::MIN`). Each row's author id + term ids are each
/// resolved with ONE batched link read (no N+1).
async fn build_galley(
    store: &Arc<dyn RhypeStore>,
    settings: &SiteSettings,
) -> Result<Vec<CachedHomePost>, CoreError> {
    let (published, author_links, term_links) =
        recent_published_posts(store, settings.posts_per_page as usize).await?;

    Ok(published
        .iter()
        .zip(author_links.iter())
        .zip(term_links.iter())
        .map(|((obj, author_ids), term_ids)| to_cached_home_post(obj, author_ids, term_ids))
        .collect())
}

/// Project one published post + its batched author/term links into a [`CachedHomePost`] row —
/// the ONE row-shaping rule [`build_galley`] and [`build_archive`] share, so a home row and an
/// archive row can never independently drift on which scalar fields they carry.
fn to_cached_home_post(
    obj: &Object,
    author_ids: &[ObjectId],
    term_ids: &[ObjectId],
) -> CachedHomePost {
    CachedHomePost {
        title: str_field(obj, "title"),
        url: format!("/{}", str_field(obj, "slug")),
        excerpt: str_field(obj, "excerpt"),
        published_at: obj.get("published_at").and_then(Value::as_datetime),
        author_id: author_ids.first().map(|id| id.0),
        term_ids: term_ids.iter().map(|id| id.0).collect(),
    }
}

/// The most-recent PUBLISHED posts (newest first, capped at `limit`) with each row's author
/// target ids AND `terms` membership ids each in ONE batched link read — the
/// scan/filter/sort/truncate + no-N+1 author query shared by BOTH the front-page galley
/// ([`build_galley`]) and the syndication feed ([`crate::feed::build_feed`]), so the two
/// listings can never drift on which posts they include or on the batched-link discipline.
/// The feed does not (yet) surface term chips, so it simply ignores the third element.
///
/// Scans posts and filters/sorts in Rust: v1 has no compound "status == published ORDER BY
/// published_at DESC LIMIT n" primitive, and the post table is small. Posts with no
/// `published_at` sort last (keyed `i64::MIN`). Returns the truncated objects and, positionally
/// aligned by index, each object's `author` target ids and `terms` target ids (empty vec when
/// unlinked).
pub(crate) async fn recent_published_posts(
    store: &Arc<dyn RhypeStore>,
    limit: usize,
) -> Result<(Vec<Object>, Vec<Vec<ObjectId>>, Vec<Vec<ObjectId>>), CoreError> {
    let mut published: Vec<Object> = store
        .scan(&TypeName::from(POST_TYPE))
        .await?
        .into_iter()
        .filter(is_published)
        .collect();

    sort_newest_first(&mut published);
    published.truncate(limit);

    let (author_links, term_links) = batch_author_and_term_links(store, &published).await?;
    Ok((published, author_links, term_links))
}

/// THE shared listing order: newest-first by publish instant (missing sorts last), with an id
/// DESCENDING tiebreak for determinism when two posts share a publish instant. Used by BOTH
/// [`recent_published_posts`] (the front-page galley + feed) and [`rollup_published_posts`]
/// (the term-archive listing), so the home page and every archive can never disagree on
/// tie-breaking — a single ordering, not two independently-maintained ones.
fn sort_newest_first(objects: &mut [Object]) {
    objects.sort_by(|a, b| {
        let key = |o: &Object| {
            (
                o.get("published_at")
                    .and_then(Value::as_datetime)
                    .unwrap_or(i64::MIN),
                o.id,
            )
        };
        key(b).cmp(&key(a))
    });
}

/// Resolve `posts`' author id + term ids in ONE batched link read each (no N+1) — the batching
/// discipline [`recent_published_posts`] and [`rollup_published_posts`] share. Neither the
/// byline name nor the chip name/href is stored — both are looked up live at compose time (the
/// author directory / the SAME TaxonomySet the single-page path uses, so every listing and the
/// permalink can never disagree).
async fn batch_author_and_term_links(
    store: &Arc<dyn RhypeStore>,
    posts: &[Object],
) -> Result<(Vec<Vec<ObjectId>>, Vec<Vec<ObjectId>>), CoreError> {
    let ids: Vec<ObjectId> = posts.iter().map(|o| o.id).collect();
    let author_links = store
        .get_links_many(&TypeName::from(POST_TYPE), &ids, "author")
        .await?;
    let term_links = store
        .get_links_many(&TypeName::from(POST_TYPE), &ids, "terms")
        .await?;
    Ok((author_links, term_links))
}

/// The rolled-up, published-only posts for a term ARCHIVE (`/{taxonomy_key}/{chain}`): the
/// term's own DIRECT `Post.terms` membership `∪` every DESCENDANT term's (the WP "a parent
/// category also lists its children's posts" convention), deduped, then [`sort_newest_first`]
/// — the SAME order [`recent_published_posts`] uses, so the home page and every archive can
/// never drift on tie-breaking. Returns `(rows, author_links, term_links, total)`: `total` is
/// the FULL deduped count *before* slicing to one page — the archive heading's count and (from
/// D2) the page-count arithmetic. `limit` slices from the start (page 1); D2 adds an offset.
///
/// Post membership is read off `Term.objects`, the `Post.terms` INVERSE relation ([`TERM_TYPE`]
/// docs) — one batched [`RhypeStore::get_links_many`] over `term_id ∪ descendants` resolves
/// every member post in a single round trip, no N+1 per term.
async fn rollup_published_posts(
    store: &Arc<dyn RhypeStore>,
    taxonomies: &TaxonomySet,
    term_id: u64,
    limit: usize,
) -> Result<(Vec<Object>, Vec<Vec<ObjectId>>, Vec<Vec<ObjectId>>, usize), CoreError> {
    let mut rollup_term_ids = taxonomies.descendants(term_id);
    rollup_term_ids.push(term_id);
    let rollup_term_object_ids: Vec<ObjectId> =
        rollup_term_ids.iter().map(|&id| ObjectId(id)).collect();

    let member_links = store
        .get_links_many(
            &TypeName::from(TERM_TYPE),
            &rollup_term_object_ids,
            "objects",
        )
        .await?;

    // Union + DEDUPE post ids across every term in the rollup — a post directly in BOTH a
    // parent and a child term (or in two sibling descendants) must appear exactly once.
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut post_ids: Vec<ObjectId> = Vec::new();
    for links in &member_links {
        for &id in links {
            if seen.insert(id) {
                post_ids.push(id);
            }
        }
    }

    let mut published: Vec<Object> = store
        .get_many(&TypeName::from(POST_TYPE), &post_ids)
        .await?
        .into_iter()
        .filter(is_published)
        .collect();
    sort_newest_first(&mut published);
    let total = published.len();
    published.truncate(limit);

    let (author_links, term_links) = batch_author_and_term_links(store, &published).await?;
    Ok((published, author_links, term_links, total))
}

/// Resolve a request path key to the PUBLISHED entity behind it, returning its store
/// type-name ([`POST_TYPE`] or [`PAGE_TYPE`]) and the materialized object.
///
/// The `path_key` is the request path with its leading/trailing `/` trimmed (see
/// [`slug_from_path`]) — a single segment (`"about"`) or a nested page path
/// (`"about/team"`). The resolution rule:
///
///   * A **nested** path (contains `/`) can only be a `Page`: posts are always flat
///     (served at a single-segment `/<slug>`), and a page's full public path is stored
///     in its `path` scalar (materialized from its ancestor-slug chain).
///   * A **single-segment** path resolves a published `Post` by `slug` FIRST (the v1
///     precedence), then a top-level published `Page` by `path` (whose `path` equals
///     its own slug).
///
/// `Ok(None)` means no published entity matched (callers map that to a 404). An empty
/// key never matches (the site root is the front page, not a permalink).
///
/// Shared by [`build_page`] (page rendering) and the island comment API, so a comment
/// can only ever attach to — and be listed for — content that is actually publicly
/// served, under ONE definition of "published at this path".
pub async fn resolve_published_entity(
    store: &Arc<dyn RhypeStore>,
    path_key: &str,
) -> Result<Option<(&'static str, Object)>, CoreError> {
    if path_key.is_empty() {
        return Ok(None);
    }
    // Posts are flat: only a single-segment path can name a Post, and it wins over a
    // top-level page at the same key (the established precedence).
    if !path_key.contains('/')
        && let Some(obj) = find_published(store, POST_TYPE, "slug", path_key).await?
    {
        return Ok(Some((POST_TYPE, obj)));
    }
    // A page (top-level or nested) is addressed by its materialized `path`.
    if let Some(obj) = find_published(store, PAGE_TYPE, "path", path_key).await? {
        return Ok(Some((PAGE_TYPE, obj)));
    }
    Ok(None)
}

/// Look up a single PUBLISHED object of `type_name` by an exact indexed `field` value
/// (`"slug"` for a Post, `"path"` for a Page).
///
/// Runs the indexed single-predicate `filter` (`field == value`, limit 1) the engine
/// fast-paths, then gates on `status == "published"` in Rust. The status gate is a
/// second Rust check rather than a second predicate because the engine filter is
/// single-predicate (compound predicates are caller-composed); for a `limit 1` hit a
/// one-row post-filter is cheaper than a second scan.
async fn find_published(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    field: &str,
    value: &str,
) -> Result<Option<Object>, CoreError> {
    let hits = store
        .filter(FilterSpec {
            type_name: TypeName::from(type_name),
            field: field.to_owned(),
            op: Compare::Eq,
            value: Value::String(value.to_owned()),
            limit: Some(1),
        })
        .await?;

    match hits.into_iter().next() {
        Some(obj) if is_published(&obj) => Ok(Some(obj)),
        _ => Ok(None),
    }
}

/// Published iff the `status` field is the string `"published"`
/// (== [`Status::Published`]`.as_str()`; statuses are stored as plain strings).
///
/// `pub(crate)` so the regen loop can apply the SAME publish gate the read path
/// uses when it re-`get`s a changed object (a draft/unpublished entity must be
/// evicted from the cache, not regenerated).
pub(crate) fn is_published(obj: &Object) -> bool {
    matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Published.as_str())
}

/// A single field read as a `String` (empty when absent or not a string).
///
/// `pub(crate)` so the feed builder ([`crate::feed`]) reads a post's scalar
/// title/slug/uuid/excerpt through the SAME accessor the permalink envelope uses.
pub(crate) fn str_field(obj: &Object, field: &str) -> String {
    match obj.get(field) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Resolve an object's `featured_media` relation to a public `/media/{uuid}` URL.
async fn featured_image_url(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    id: ObjectId,
) -> Option<String> {
    let media_id = single_link(store, type_name, id, "featured_media").await?;
    let media = store
        .get(&TypeName::from(MEDIA_TYPE), media_id)
        .await
        .ok()?;
    match media.get("uuid") {
        Some(Value::String(uuid)) if is_media_token(uuid) => Some(media_url(uuid)),
        _ => None,
    }
}

/// The first target of a to-one relation `field` on `(type_name, id)`, if linked.
async fn single_link(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    id: ObjectId,
    field: &str,
) -> Option<ObjectId> {
    let edge = Edge {
        type_name: TypeName::from(type_name),
        id,
        field: field.to_owned(),
    };
    store
        .get_links(&edge)
        .await
        .ok()?
        .into_iter()
        .next()
        .map(|(target, _)| target)
}

/// Every target id of a to-many relation `field` on `(type_name, id)` (empty when unlinked
/// OR on a read fault — a term-chip miss must degrade the page, never fail it, matching
/// [`featured_image_url`]'s best-effort discipline).
async fn multi_link(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    id: ObjectId,
    field: &str,
) -> Vec<u64> {
    let edge = Edge {
        type_name: TypeName::from(type_name),
        id,
        field: field.to_owned(),
    };
    store
        .get_links(&edge)
        .await
        .map(|links| links.into_iter().map(|(target, _)| target.0).collect())
        .unwrap_or_default()
}

/// The document `<title>`: `"Entry — Site"`, or just the site title when the entry
/// has none.
fn page_title(title: &str, settings: &SiteSettings) -> String {
    let site = settings.title_or_default();
    if title.trim().is_empty() {
        site.to_owned()
    } else {
        format!("{title} — {site}")
    }
}

/// The `<meta name="description">`: the stored SEO description, else the excerpt,
/// else nothing.
fn meta_description(page: &CachedPage) -> Option<&str> {
    page.seo
        .as_ref()
        .and_then(|s| s.meta_description.as_deref())
        .or_else(|| {
            if page.excerpt.trim().is_empty() {
                None
            } else {
                Some(&page.excerpt)
            }
        })
}

/// Up to two uppercased initials from a display name (the byline avatar).
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// The live site chrome the templates read (`site.*`). Derived from the current
/// [`SiteSettings`] on every render.
#[derive(Serialize)]
struct SiteCtx<'a> {
    title: &'a str,
    tagline: &'a str,
    url: &'a str,
    /// The site logo's `/media/{uuid}` URL, if one is set — the masthead shows it in
    /// place of the text title. Resolved live from the settings snapshot.
    logo: Option<&'a str>,
    /// `true` when search-engine indexing is off → the chrome emits a `noindex`
    /// robots meta.
    noindex: bool,
}

impl<'a> From<&'a SiteSettings> for SiteCtx<'a> {
    fn from(s: &'a SiteSettings) -> Self {
        SiteCtx {
            title: s.title_or_default(),
            tagline: &s.tagline,
            url: &s.url,
            logo: s.logo_url.as_deref(),
            noindex: !s.search_engine_visible,
        }
    }
}

/// The context for the single-page template.
#[derive(Serialize)]
struct SingleCtx<'a> {
    page_title: String,
    page_description: Option<&'a str>,
    canonical: Option<&'a str>,
    site: SiteCtx<'a>,
    is_home: bool,
    /// `Some(status label)` on the authenticated draft preview → the chrome shows the
    /// preview banner and the comments island is suppressed; `None` on public renders.
    preview_status: Option<&'a str>,
    /// The composed nav menus, keyed by theme location (`"primary"`, `"footer"`, …). A
    /// sibling field, NOT folded into [`SiteCtx`] (which stays a pure `SiteSettings`
    /// projection). The theme loops the location(s) it declares, falling back to its default
    /// chrome for any location absent from this map. See [`MenuSet::compose`].
    nav: BTreeMap<String, Vec<MenuItemCtx>>,
    title: &'a str,
    dateline: Option<String>,
    kicker: Option<&'a str>,
    author: Option<&'a str>,
    author_initials: String,
    featured_image: Option<&'a str>,
    /// This entry's term chips, resolved live (see [`resolve_chips`]) — direct assignments
    /// only, name-sorted for a deterministic display order.
    terms: Vec<TermChipCtx>,
    body: &'a str,
}

/// The context for the front-page galley template — ALSO reused, unchanged shape, for a term
/// archive listing (`compose_archive`): the same eyebrow + `<ol>` of rows, with `archive: Some`
/// swapping the eyebrow for the archive heading (name/description/count) instead of the
/// site-wide "Latest …" line. `is_home` is `true` only for the actual site root; an archive is
/// `false` (it is never the front page).
#[derive(Serialize)]
struct HomeCtx<'a> {
    page_title: String,
    /// Owned (not `&'a str`, unlike [`SingleCtx::page_description`]): an archive's description
    /// is a `String` resolved live from the [`TaxonomySet`] snapshot inside `compose_archive`'s
    /// own stack frame, not borrowed from a longer-lived caller argument.
    page_description: Option<String>,
    /// The archive's own canonical URL (`settings.url` + its archive href), `None` for the
    /// plain galley (which has no canonical target of its own today). Built the same way
    /// [`SingleCtx::canonical`] is — a `<link rel="canonical">` the base chrome emits when set.
    canonical: Option<String>,
    site: SiteCtx<'a>,
    is_home: bool,
    /// Always `None` — neither the front page nor an archive is ever previewed; declared so
    /// the shared base chrome's `preview_status` reference resolves without relying on
    /// lenient-undefined.
    preview_status: Option<&'a str>,
    /// The composed nav menus, keyed by theme location — the front-page/archive twin of
    /// [`SingleCtx::nav`], so the shared base chrome's `menus.*` loops resolve identically on
    /// every listing shape.
    nav: BTreeMap<String, Vec<MenuItemCtx>>,
    /// `Some` only when this is a term-archive listing — the theme conditionalizes its eyebrow
    /// on this field (`{% if archive %}` the heading, `{% else %}` the site-wide "Latest …").
    archive: Option<ArchiveCtx>,
    posts: Vec<PostSummary>,
}

/// The term-archive heading: name + description + the FULL rolled-up post count (before
/// pagination slicing). Resolved LIVE from the current [`TaxonomySet`] at compose time — the
/// envelope bakes only `term_id` (see [`CachedTermArchive`]), so a rename/re-parent needs no
/// regeneration.
#[derive(Serialize)]
struct ArchiveCtx {
    name: String,
    description: Option<String>,
    total: usize,
}

/// One row in the front-page galley OR a term-archive listing (`compose_front` / `compose_archive`).
#[derive(Serialize)]
struct PostSummary {
    title: String,
    url: String,
    excerpt: String,
    dateline: Option<String>,
    author: Option<String>,
    /// This row's term chips — the galley twin of [`SingleCtx::terms`].
    terms: Vec<TermChipCtx>,
}

/// One resolved term chip: display name + canonical archive href. Both fields are resolved
/// LIVE from the current [`TaxonomySet`] at compose time (see [`resolve_chips`]) — the
/// envelope bakes only the term *id*, so a term rename or re-parent is reflected on the next
/// request with no page regeneration, the same discipline as the byline name.
#[derive(Serialize)]
struct TermChipCtx {
    name: String,
    href: String,
}

/// Resolve baked `term_ids` to their live chips — direct assignments only, never rolled up
/// to ancestors. An id the current [`TaxonomySet`] can't resolve (a deleted term whose cache
/// eviction hasn't landed yet, or corrupt data) is silently dropped: a chip list degrades, it
/// never breaks the page. Sorted case-folded name ascending (id tiebreak) for a DETERMINISTIC
/// display order regardless of the store's (unordered) link order — the same discipline
/// [`TaxonomySet`]'s own per-taxonomy listing order uses.
fn resolve_chips(taxonomies: &TaxonomySet, term_ids: &[u64]) -> Vec<TermChipCtx> {
    let mut resolved: Vec<(u64, TermChipCtx)> = term_ids
        .iter()
        .filter_map(|&id| {
            let name = taxonomies.term(id)?.name.clone();
            let href = taxonomies.archive_href(id)?;
            Some((id, TermChipCtx { name, href }))
        })
        .collect();
    resolved.sort_by(|(a_id, a), (b_id, b)| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a_id.cmp(b_id))
    });
    resolved.into_iter().map(|(_, chip)| chip).collect()
}

/// Render a block tree to its final, media-rewritten HTML body.
///
/// The SINGLE code path BOTH the permalink envelope ([`cached_page_from_object`]) and the
/// syndication feed ([`crate::feed::build_feed`]) funnel a body through, so the
/// one-shared-renderer invariant holds for every surface that bakes body HTML: block dispatch
/// is solely [`render_with`] and the media rewrite is applied identically. `mode` threads to
/// the renderer (public serve/regen pass [`RenderMode::Publish`]; the feed always publishes).
pub(crate) fn render_body(
    tree: &BlockTree,
    mode: RenderMode,
    custom: &dyn CustomBlockRenderer,
) -> String {
    rewrite_media_srcs(render_with(tree, mode, custom).as_str())
}

/// Rewrite the renderer's `src`-less media placeholder into a real media URL.
///
/// `ferropress-render` is pure (no DB), so `BlockKind::Image` emits
/// `<img data-media-id="N" …>` with no `src`. Here in the serve layer — which DOES
/// front the store — we add `src="/media/N"` ([`ferropress_core::media_url`]) so the
/// browser fetches the bytes from the media route. Deliberately kept OUT of the
/// renderer to preserve the one-shared-renderer invariant AND to keep the editor
/// preview (which reuses `render`) DB-free.
///
/// This is a **pure string transform**: the id is already in the HTML, so it needs no
/// store lookup and stays off the page-render data path. It runs exactly once per
/// (re)render, and — because [`render_object`] is the single point BOTH the on-demand
/// write-through ([`serve_path`]) and the regen loop
/// ([`ServeEngine::render_page`](crate::ServeEngine::render_page)) funnel through — it
/// runs BEFORE the HTML is cached, so cached pages always carry final URLs. The
/// leading fast-path check makes the common (image-less) page free.
///
/// Robustness: the needle anchors on the renderer's exact opener `<img data-media-id="`
/// (the token is the FIRST attribute). This literal cannot appear inside an attribute
/// VALUE — any `"` there is escaped to `&quot;` — so the match only ever lands on a
/// real image tag. The value is only rewritten if it passes
/// [`ferropress_core::is_media_token`] (a uuid-shaped token); a hand-crafted non-token
/// value (which the renderer already attribute-escaped) is copied through with NO `src`,
/// so it can neither be fetched nor inject markup.
fn rewrite_media_srcs(html: &str) -> String {
    let needle = MEDIA_IMG_NEEDLE.as_str();
    if !html.contains(needle) {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len() + 32);
    let mut rest = html;
    while let Some(pos) = rest.find(needle) {
        let after = pos + needle.len();
        let tail = &rest[after..];
        // The token runs to the closing quote. A valid token is uuid-shaped, so it is
        // unaffected by attribute-escaping (it equals its raw form here).
        match tail.find('"') {
            Some(q) if ferropress_core::is_media_token(&tail[..q]) => {
                // The token already sits in the HTML, so reuse it verbatim:
                // `MEDIA_URL_PREFIX + token` == `media_url(token)` with no allocation.
                let token = &tail[..q];
                out.push_str(&rest[..pos]);
                out.push_str("<img src=\"");
                out.push_str(ferropress_core::MEDIA_URL_PREFIX);
                out.push_str(token);
                out.push_str("\" ");
                out.push_str(ferropress_core::MEDIA_ID_ATTR);
                out.push_str("=\"");
                out.push_str(token);
                out.push('"');
                rest = &tail[q + 1..];
            }
            _ => {
                // Not a valid media token — copy through and advance past the needle so
                // the loop always makes progress (no `src` added; escaping already
                // neutralized any crafted value).
                out.push_str(&rest[..after]);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The renderer's exact `<img data-media-id="` opener, derived ONCE from the shared
/// [`ferropress_core::MEDIA_ID_ATTR`] const (still single-source). A `LazyLock` so the
/// invariant needle isn't re-allocated on every page render, image-free ones included.
static MEDIA_IMG_NEEDLE: LazyLock<String> =
    LazyLock::new(|| format!("<img {}=\"", ferropress_core::MEDIA_ID_ATTR));

#[cfg(test)]
mod media_rewrite_tests {
    use super::rewrite_media_srcs;
    use ferropress_core::{Block, BlockKind, BlockTree};
    use ferropress_render::{NoCustomBlocks, RenderMode, render_with};

    fn image(media: &str, alt: &str) -> Block {
        Block {
            uid: media.to_owned(),
            kind: BlockKind::Image {
                media: media.to_owned(),
                alt: alt.to_owned(),
            },
            children: vec![],
        }
    }

    #[test]
    fn adds_src_to_the_renderers_image_output() {
        // Drive the REAL renderer so the rewrite is tested against its actual markup,
        // not a hand-written string that could drift from the emitter.
        let token = "018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90";
        let tree = BlockTree::from_blocks(vec![image(token, "a proof")]);
        let html = render_with(&tree, RenderMode::Publish, &NoCustomBlocks).into_string();
        assert!(!html.contains("src="), "sanity: renderer emits no src");

        let out = rewrite_media_srcs(&html);
        assert!(
            out.contains(&format!("src=\"/media/{token}\"")),
            "got: {out}"
        );
        assert!(
            out.contains(&format!("data-media-id=\"{token}\"")),
            "token attr is preserved: {out}"
        );
        assert!(out.contains("alt=\"a proof\""));
    }

    #[test]
    fn rewrites_multiple_images() {
        let tree = BlockTree::from_blocks(vec![image("aa11", ""), image("bb22", "")]);
        let html = render_with(&tree, RenderMode::Publish, &NoCustomBlocks).into_string();
        let out = rewrite_media_srcs(&html);
        assert!(out.contains("src=\"/media/aa11\""), "got: {out}");
        assert!(out.contains("src=\"/media/bb22\""), "got: {out}");
    }

    #[test]
    fn passes_image_free_html_through_untouched() {
        let html = "<p>no images here — data-media-id is just text</p>";
        assert_eq!(rewrite_media_srcs(html), html);
    }

    #[test]
    fn a_crafted_non_token_media_value_is_escaped_and_never_gets_a_src() {
        // A hand-crafted block_tree with a markup payload where the uuid should be: the
        // renderer must attribute-escape it (no raw tag reaches the page) AND the rewrite
        // must refuse to give it a src (it isn't a valid media token).
        let tree = BlockTree::from_blocks(vec![image("\"><script>alert(1)</script>", "x")]);
        let html = render_with(&tree, RenderMode::Publish, &NoCustomBlocks).into_string();
        let out = rewrite_media_srcs(&html);
        assert!(
            !out.contains("<script>"),
            "payload must be escaped, not live: {out}"
        );
        assert!(
            !out.contains("src="),
            "a non-token value must not get a src: {out}"
        );
    }
}
