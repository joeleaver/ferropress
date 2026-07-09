//! # ferropress-serve
//!
//! Static-first hybrid orchestration. Pages are pre-rendered to HTML and stored
//! through the [`BlobStore`] port; a regeneration loop consumes
//! [`RhypeStore::subscribe`] and regenerates ONLY the pages affected by each
//! change — never a full rebuild. Generation is deferred / on-demand:
//! render-on-first-request -> cache -> regenerate-on-change. The same change
//! subscription doubles as the multi-instance cache-invalidation broadcast.
//!
//! This crate owns the *policy* (what a change invalidates, what to re-render);
//! it delegates the actual block-tree -> HTML to `ferropress-render` and the page
//! chrome to `ferropress-theme`, and it never talks to a concrete store/blob
//! backend — only the [`RhypeStore`] / [`BlobStore`] ports.
//!
//! Two read paths live here:
//!   * [`content`] — the path resolver. [`resolve_path`] is the uncached
//!     SSR-on-demand render; [`serve_path`] is the **cache-first** hot path the
//!     HTTP fallback calls (try the prerender cache, render + populate on a miss).
//!   * [`ServeEngine`] — the change-driven regeneration loop: it consumes the
//!     change feed and write-throughs each affected page's HTML to the cache
//!     (or evicts it when an entity becomes unpublished).

use std::sync::Arc;

use ferropress_core::ports::{BlobKey, BlobStore};
use ferropress_core::query::{Change, ChangeKind, SubscribeFilter};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::Value;
use ferropress_core::{PAGE_TYPE, POST_TYPE, USER_TYPE};
use ferropress_render::CustomBlockRenderer;

pub mod authors;
pub mod content;
pub mod datefmt;
pub mod hook_bridge;
pub mod settings;
pub mod templates;

pub use authors::{AuthorDirectory, AuthorsHandle, load_author_directory};
pub use content::{
    Resolved, default_theme, render_preview, resolve_path, resolve_published_entity, serve_path,
    slug_from_path,
};
pub use hook_bridge::HookBridge;
pub use settings::{SettingsHandle, load_site_settings, load_values, overlay_settings};

/// Identifies one prerendered output page. The serve cache is keyed by the path
/// (URL path -> `BlobKey`); a content change maps to the set of `OutputPage`s it
/// invalidates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputPage {
    /// Site-relative URL path, e.g. `"/blog/hello-world"`.
    pub path: String,
}

/// The `BlobKey` namespace prefix for prerendered pages. Keeps the rendered-HTML
/// cache in its own subtree of the blob root, away from media originals.
const CACHE_PREFIX: &str = "prerender";

/// Map a request path to its deterministic, traversal-safe prerender-cache key.
///
/// Two disjoint namespaces under `prerender/` keep LISTING pages apart from per-slug
/// PERMALINK pages so they can never collide:
///   * the site root `"/"` (empty slug) is a listing page → `prerender/listing/index.html`;
///   * any other path `"/<slug>"` is a permalink → `prerender/permalink/<slug>.html`.
///
/// The split is load-bearing: without it, a permalink whose slug is literally `index` (or a
/// nested slug) would map to `prerender/index.html`, the very key the front page would use —
/// silently defeating BOTH caches (each is deserialized as the other's type, rejected, and
/// re-rendered on every hit). Because permalinks live one namespace *below* the listing
/// pages, no slug can ever produce a listing key. Future listing pages (post archive, RSS/
/// Atom feed) join the `listing/` subtree; slugs can never reach it.
///
/// Determinism + collision safety within `permalink/`: distinct slugs map to distinct keys
/// because the slug is preserved verbatim inside the namespace. Traversal safety is layered:
///   * we strip the leading `/` ourselves, because the localfs adapter REJECTS a key with a
///     leading slash (it must be a relative path under the blob root);
///   * the [`BlobStore`] adapter is the real guard — it independently rejects `..`, NUL,
///     backslash, and absolute keys (see `ferropress-blob-localfs`), so a `../`-bearing slug
///     cannot escape `permalink/` into `listing/` or the blob root.
///
/// `.html` suffix so the on-disk cache is self-describing and never collides with a media key
/// of the same stem.
pub fn cache_key(path: &str) -> BlobKey {
    // Drop a single leading '/'; the prefix join re-introduces the separator.
    // Trailing '/' is also trimmed so "/a/" and "/a" share one cache entry,
    // matching `slug_from_path`'s trim (they resolve to the same page).
    let rel = path.trim_start_matches('/').trim_end_matches('/');
    if rel.is_empty() {
        // Site root / front page — a LISTING page, in its own reserved subtree that no
        // permalink slug can produce (see the collision note above).
        return BlobKey(format!("{CACHE_PREFIX}/listing/index.html"));
    }
    // A permalink page, one namespace below the listing pages.
    BlobKey(format!("{CACHE_PREFIX}/permalink/{rel}.html"))
}

/// The prerender-cache key for an [`OutputPage`]. Thin wrapper over [`cache_key`]
/// keyed on the page's path, so the regen loop and the read path derive the SAME
/// key for a given page.
fn page_blob_key(page: &OutputPage) -> BlobKey {
    cache_key(&page.path)
}

/// The slug carried on a change's JSON `fields` (the engine publishes the changed
/// scalar fields, incl. on delete), if present and non-empty. Lets the regen loop
/// derive a page path straight from the change — no extra store read, and the only
/// way to know a *deleted* object's slug.
fn slug_from_change(change: &Change) -> Option<String> {
    change
        .fields
        .as_ref()
        .and_then(|f| f.get("slug"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Drives prerender + incremental regeneration over the ports.
///
/// Holds the collaborators the *envelope* build needs: the store (to read the
/// changed entity + its metadata), the blob cache (to write-through / evict), and
/// the custom-block renderer (to resolve plugin blocks) — so a regenerated
/// envelope is byte-for-byte what a fresh [`serve_path`] render would cache. The
/// chrome is NOT produced here: it is composed live at request time, so the regen
/// loop needs no theme host (and a settings change never triggers regeneration).
pub struct ServeEngine {
    store: Arc<dyn RhypeStore>,
    blobs: Arc<dyn BlobStore>,
    custom: Arc<dyn CustomBlockRenderer>,
    /// The live site-settings snapshot the read path composes chrome from. When
    /// present, a `Setting` change on the feed refreshes it (see
    /// [`apply_change`](Self::apply_change)); `None` (tests, a public-only boot
    /// without settings wired) simply skips the refresh. This is the SAME handle
    /// the HTTP read path holds, so a refresh here is visible there.
    settings: Option<SettingsHandle>,
    /// The live author directory the read path resolves post bylines from. When
    /// present, a `User` change on the feed refreshes it (see
    /// [`apply_change`](Self::apply_change)) — so an author rename is reflected on
    /// the public site with NO page regeneration (the byline name is composed live,
    /// like chrome). `None` (tests, a boot without authors wired) skips the refresh;
    /// unresolved ids simply render as no byline. Same handle the read path holds.
    authors: Option<AuthorsHandle>,
}

impl ServeEngine {
    /// Build the engine over the injected ports + the custom-block renderer. The
    /// concrete adapters (plugin host) are chosen in `ferropress-server` (the
    /// composition root), never here.
    pub fn new(
        store: Arc<dyn RhypeStore>,
        blobs: Arc<dyn BlobStore>,
        custom: Arc<dyn CustomBlockRenderer>,
    ) -> Self {
        Self {
            store,
            blobs,
            custom,
            settings: None,
            authors: None,
        }
    }

    /// Wire the live [`SettingsHandle`] so the regen loop refreshes it whenever a
    /// `Setting` changes on the feed. Pass the SAME handle the HTTP read path
    /// holds (the composition root creates one and shares it), so a settings edit
    /// is reflected on the public site without any page-cache regeneration.
    pub fn with_settings(mut self, settings: SettingsHandle) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Wire the live [`AuthorsHandle`] so the regen loop refreshes it whenever a
    /// `User` changes on the feed. Pass the SAME handle the HTTP read path holds, so
    /// an author rename is reflected in every post's byline on the next request
    /// without regenerating a single cached page (the byline name is composed live).
    pub fn with_authors(mut self, authors: AuthorsHandle) -> Self {
        self.authors = Some(authors);
        self
    }

    /// Run the regeneration loop forever: subscribe to ALL changes and, for each
    /// committed change, write-through (regenerate) or evict exactly the affected
    /// prerendered pages. Never a full rebuild.
    ///
    /// Per change (the slug comes off the change feed's `fields` — the engine
    /// publishes the changed scalar fields, so no extra read is needed; a re-`get`
    /// is only a fallback when the feed somehow carried no slug):
    ///   * **Create / Update** of a `Post`/`Page`: derive its `/<slug>` path and
    ///     [`build_page`](Self::build_page) its envelope. `Some(_)` -> `put`
    ///     (regenerate); `None` (the entity is no longer published) -> `delete`
    ///     (evict). This makes an unpublish/trash a cache eviction, not a stale
    ///     page.
    ///   * **Delete**: the object is gone, but the change carries its (pre-delete)
    ///     scalar `fields`, so we read the slug from there and evict the cached
    ///     page. This closes the previously-unfixable "deleted page stays cached"
    ///     gap (which needed either this engine feature or a reverse index).
    ///
    /// The loop runs forever. A per-change error is logged and the loop continues
    /// — one bad change must never tear down regeneration for the whole site.
    pub async fn regen_loop(&self) -> ferropress_core::error::Result<()> {
        // `tokio-stream`'s `StreamExt::next` drives the `BoxStream`; the concrete
        // `SubscriptionStream` is `Unpin` (Box-pinned), so `next().await` on a
        // plain `&mut` binding works without an explicit `pin!`.
        use tokio_stream::StreamExt;

        let mut stream = self.store.subscribe(SubscribeFilter::default()).await?;
        tracing::info!("serve regen loop subscribed to the change feed");

        while let Some(change) = stream.next().await {
            if let Err(e) = self.apply_change(&change).await {
                // Best-effort regen: log and keep consuming. The next request for
                // an un-regenerated page falls back to render-on-demand via
                // `serve_path`, so a missed regen degrades to SSR, never to stale.
                tracing::error!(
                    version = change.version,
                    type_name = %change.type_name.as_str(),
                    object_id = change.object_id.0,
                    error = %e,
                    "regen step failed; continuing",
                );
            }
        }

        // The stream ends only when the store (hence the whole process) is going
        // away; returning Ok lets the spawned task exit quietly.
        tracing::info!("serve regen loop change feed ended");
        Ok(())
    }

    /// Apply ONE change to the cache: regenerate or evict its affected pages.
    /// Split out of [`regen_loop`](Self::regen_loop) so a per-change failure is a
    /// recoverable `Err` the loop logs, not a loop-killing `?` at the top level.
    async fn apply_change(&self, change: &Change) -> ferropress_core::error::Result<()> {
        // A `Setting` change refreshes the live snapshot the read path composes
        // chrome from — NOT a broad page-cache eviction. Global chrome (title,
        // tagline, robots) and date formatting are applied live at request time,
        // so a settings edit is reflected without regenerating cached pages (the
        // serving model's no-global-coupling guardrail). The ONE exception is the
        // home page, whose cached CONTENT a few `reading.*` keys reshape (see the
        // key-gated `evict_front` below); every other key still busts nothing.
        if change.type_name.as_str() == ferropress_core::SETTING_TYPE {
            if let Some(handle) = &self.settings {
                match settings::load_site_settings(&self.store).await {
                    Ok(next) => {
                        handle.set(next);
                        tracing::debug!("refreshed live site settings from change feed");
                    }
                    Err(e) => tracing::warn!(error = %e, "failed to refresh site settings"),
                }
            }
            // The front page is the ONE cached page whose CONTENT a setting can reshape:
            // `reading.posts_per_page` / `show_on_front` / `page_on_front` change which and
            // how many entries `/` lists (and whether it is a static page or the galley).
            // Evict `/` for those keys so the next request rebuilds from the (just-refreshed)
            // settings — ordered AFTER the refresh above. This is keyed off the CHANGE, not
            // the engine's snapshot, so it stays correct whether or not THIS engine holds a
            // settings handle (the read path carries its own). Every OTHER setting (title,
            // tagline, robots, timezone, date_format, logo, …) composes live in the chrome and
            // must NOT bust the cache: the serving model's "a settings change regenerates no
            // pages" guardrail. `evict_front` is best-effort, so a cache fault never fails the
            // change apply.
            if setting_reshapes_front(change) {
                self.evict_front().await;
            }
            return Ok(());
        }

        // A `User` change refreshes the live author directory the read path resolves
        // bylines from — NOT a page-cache eviction. A post's byline name is composed
        // live at request time (only the author's *id* is cached in the envelope), so
        // an author rename is reflected on every one of their posts without
        // regenerating any page — the cross-entity analogue of the settings guardrail
        // above (and it avoids the guardrail-2 trap of regenerating a prolific
        // author's entire back catalogue on a single rename).
        if change.type_name.as_str() == USER_TYPE {
            if let Some(authors) = &self.authors {
                authors.apply_user_change(change);
                tracing::debug!(
                    user_id = change.object_id.0,
                    "refreshed author directory from change feed",
                );
            }
            return Ok(());
        }

        // Only content types map to a page in permalinks v1.
        let ty = change.type_name.as_str();
        if ty != POST_TYPE && ty != PAGE_TYPE {
            return Ok(());
        }

        // The front page is a LISTING page this content change may also touch — a post
        // joins/leaves the galley, or the configured static front page itself changed.
        // Evict `/` (settings-gated + I/O-free) BEFORE the slug-dependent permalink handling
        // below, so even a slug-less delete still invalidates the home page. Eviction (not
        // eager rebuild) is the guardrail-2 strategy for this fan-in-N shared page: an
        // idempotent, coalescing delete whose next-request rebuild is the sole populator.
        self.invalidate_front_for_content(change).await;

        match change.kind {
            ChangeKind::Create | ChangeKind::Update => {
                // Prefer the slug off the change feed (no extra read). Fall back to
                // re-`get`ting the object only if the event carried no slug.
                let slug = match slug_from_change(change) {
                    Some(slug) => slug,
                    None => {
                        let obj = self.store.get(&change.type_name, change.object_id).await?;
                        match obj.get("slug") {
                            Some(Value::String(s)) if !s.is_empty() => s.clone(),
                            // No usable slug -> no permalink to (in)validate; skip.
                            _ => return Ok(()),
                        }
                    }
                };
                let page = OutputPage {
                    path: format!("/{slug}"),
                };

                // `build_page` applies the publish gate: `Some` envelope for a
                // published entity, `None` once it is draft/trashed/etc.
                match self.build_page(&page).await? {
                    Some(cached) => {
                        // Write-through: the regenerated envelope replaces any cached
                        // copy so the next request composes it straight from blobs.
                        let bytes = serde_json::to_vec(&cached).map_err(|e| {
                            ferropress_core::error::CoreError::Store(format!(
                                "serializing page envelope for the cache: {e}"
                            ))
                        })?;
                        self.blobs.put(&page_blob_key(&page), bytes).await?;
                        tracing::debug!(path = %page.path, "regenerated prerender cache entry");
                    }
                    None => {
                        // The entity became unpublished -> evict the cached page so
                        // a stale render can't keep being served. `delete` is
                        // idempotent (no-op if never cached).
                        self.blobs.delete(&page_blob_key(&page)).await?;
                        tracing::debug!(path = %page.path, "evicted prerender cache entry (unpublished)");
                    }
                }
                Ok(())
            }
            ChangeKind::Delete => {
                // The engine publishes the deleted object's (pre-delete) scalar
                // fields on the change, so we can map the deletion back to its page
                // and evict the cached HTML — the fix for the former persistent-stale
                // case (a deleted page, or a slug-collision sibling, served stale
                // from cache with no event able to dislodge it). `delete` is
                // idempotent (no-op if never cached).
                match slug_from_change(change) {
                    Some(slug) => {
                        let page = OutputPage {
                            path: format!("/{slug}"),
                        };
                        self.blobs.delete(&page_blob_key(&page)).await?;
                        tracing::debug!(path = %page.path, "evicted prerender cache entry (deleted)");
                    }
                    None => {
                        // No slug on the event (e.g. a type without a slug field) ->
                        // nothing to key the cache on. Safe to skip: such an entity
                        // has no permalink to go stale.
                        tracing::debug!(
                            type_name = %change.type_name.as_str(),
                            object_id = change.object_id.0,
                            "delete change carried no slug; nothing to evict",
                        );
                    }
                }
                Ok(())
            }
        }
    }

    /// Evict the home page (`/`) prerender cache entry when a POST/PAGE change can reshape
    /// what it shows. I/O-free (reads only the in-memory settings snapshot, when present).
    ///
    /// Home *eviction* is deliberately NOT gated on the engine holding a [`SettingsHandle`]:
    /// the read path caches `/` unconditionally (it carries its own settings), so eviction
    /// must stay live too, or a `/` cached by a read could never be invalidated. Only the
    /// PAGE precision below consults the snapshot; without one we evict conservatively.
    ///
    /// - A **post** change always evicts `/`: a post can appear in the galley, INCLUDING the
    ///   fallback galley shown when a configured static front page is missing/unpublished.
    ///   Distinguishing a live static front from that fallback would need a store read, so we
    ///   always evict on a post change — correct in every configuration, and cheap under
    ///   eviction (an idempotent delete + one lazy rebuild, never an eager scan). The only
    ///   "waste" is a cheap static-page rebuild on a static-front site's post edits.
    /// - A **page** change evicts `/` only when the page IS the configured static front page
    ///   (pages never appear in the galley). With a settings snapshot we gate precisely on
    ///   `front_page_id`; without one we cannot tell, so we evict conservatively. Keyed on the
    ///   change's object id, so it also covers unpublishing/deleting the front page (→ the
    ///   next request falls back to the galley).
    async fn invalidate_front_for_content(&self, change: &Change) {
        let affects_front = match change.type_name.as_str() {
            POST_TYPE => true,
            PAGE_TYPE => match &self.settings {
                Some(handle) => handle.current().front_page_id == Some(change.object_id.0),
                // No snapshot to check the configured front page against → evict conservatively.
                None => true,
            },
            _ => false,
        };
        if affects_front {
            self.evict_front().await;
        }
    }

    /// Evict the home page's prerender cache entry (`/`). **Best-effort**: a blob delete
    /// failure is logged and swallowed — the read path rebuilds `/` on the next request
    /// regardless, and the cache is best-effort throughout, so a cache fault must never fail
    /// the change apply. Idempotent (a no-op if `/` was never cached).
    async fn evict_front(&self) {
        if let Err(e) = self.blobs.delete(&cache_key("/")).await {
            tracing::warn!(error = %e, "evicting the home prerender cache failed");
        }
    }

    /// Build the cache envelope for a single output page, or `None` if no
    /// PUBLISHED entity backs it.
    ///
    /// Reuses the exact read-path build ([`content::build_page`]): block tree ->
    /// HTML via `ferropress-render` + the media rewrite + the object metadata, with
    /// `custom` resolving plugin blocks. Routing through the same code keeps a
    /// regenerated envelope byte-for-byte identical to an on-demand [`serve_path`]
    /// render of the same path. `Ok(None)` (unpublished/absent) is the signal
    /// `regen_loop` turns into a cache eviction. Chrome is composed later, live,
    /// from the current settings — so the envelope is settings-independent.
    pub(crate) async fn build_page(
        &self,
        page: &OutputPage,
    ) -> ferropress_core::error::Result<Option<content::CachedPage>> {
        content::build_page(&self.store, self.custom.as_ref(), &page.path).await
    }
}

/// Whether a `Setting` change altered a key that reshapes the cached front page's CONTENT
/// (which and how many entries `/` lists, and whether it is a static page or the galley).
/// Only these three `reading.*` keys do; every other setting composes live in the chrome
/// and must not bust the home cache.
///
/// The changed row's `key` is read off the change's scalar `fields` (rhypedb publishes the
/// full merged scalar snapshot, so an update to a Setting row carries its `key`). If the key
/// is somehow absent, we treat the change as reshaping and evict conservatively — a delete is
/// cheap and idempotent, and a stale front page is worse than a needless rebuild.
///
/// These key strings mirror the `reading.*` schema keys `SiteSettings::from_values` reads in
/// `ferropress-render-form`; keep them in sync.
fn setting_reshapes_front(change: &Change) -> bool {
    match change
        .fields
        .as_ref()
        .and_then(|f| f.get("key"))
        .and_then(|v| v.as_str())
    {
        Some(key) => matches!(
            key,
            "reading.posts_per_page" | "reading.show_on_front" | "reading.page_on_front"
        ),
        None => true,
    }
}

#[cfg(test)]
mod tests;
