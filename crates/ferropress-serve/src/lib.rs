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
use ferropress_core::{PAGE_TYPE, POST_TYPE};
use ferropress_theme::ThemeEngine;

pub mod content;

pub use content::{
    PAGE_TEMPLATE, PAGE_TEMPLATE_SRC, Resolved, default_theme, resolve_path,
    resolve_published_entity, serve_path, slug_from_path,
};

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
/// The scheme is `"prerender/<sanitized-path>.html"`. Determinism + collision
/// safety: distinct paths map to distinct keys because the path is preserved
/// verbatim inside the namespace (only the leading `/` is dropped — it is implied
/// by the prefix join). Traversal safety is layered:
///   * we strip the leading `/` ourselves, because the localfs adapter REJECTS a
///     key with a leading slash (it must be a relative path under the blob root);
///   * the site root `"/"` (empty slug) maps to the sentinel `prerender/index.html`
///     so it still names a file rather than the bare directory;
///   * the [`BlobStore`] adapter is the real guard — it independently rejects
///     `..`, NUL, backslash, and absolute keys (see `ferropress-blob-localfs`),
///     so even a hostile path cannot escape the root. This function only has to
///     produce a *valid relative* key; the port enforces safety.
///
/// `.html` suffix so the on-disk cache is self-describing and never collides with
/// a media key of the same stem.
pub fn cache_key(path: &str) -> BlobKey {
    // Drop a single leading '/'; the prefix join re-introduces the separator.
    // Trailing '/' is also trimmed so "/a/" and "/a" share one cache entry,
    // matching `slug_from_path`'s trim (they resolve to the same page).
    let rel = path.trim_start_matches('/').trim_end_matches('/');
    if rel.is_empty() {
        // Site root: a bare `prerender/` would name the directory, not a file.
        return BlobKey(format!("{CACHE_PREFIX}/index.html"));
    }
    BlobKey(format!("{CACHE_PREFIX}/{rel}.html"))
}

/// The prerender-cache key for an [`OutputPage`]. Thin wrapper over [`cache_key`]
/// keyed on the page's path, so the regen loop and the read path derive the SAME
/// key for a given page.
fn page_blob_key(page: &OutputPage) -> BlobKey {
    cache_key(&page.path)
}

/// Drives prerender + incremental regeneration over the ports.
///
/// Holds the same three collaborators the read path uses: the store (to read the
/// changed entity), the blob cache (to write-through / evict), and the theme (to
/// produce final chrome) — so a regenerated page is byte-for-byte what a fresh
/// [`serve_path`] render would have produced.
pub struct ServeEngine {
    store: Arc<dyn RhypeStore>,
    blobs: Arc<dyn BlobStore>,
    theme: Arc<ThemeEngine>,
}

impl ServeEngine {
    /// Build the engine over the injected ports + the shared theme host. The
    /// concrete adapters (and the theme) are chosen in `ferropress-server` (the
    /// composition root), never here.
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

    /// Run the regeneration loop forever: subscribe to ALL changes and, for each
    /// committed change, write-through (regenerate) or evict exactly the affected
    /// prerendered pages. Never a full rebuild.
    ///
    /// Per change:
    ///   * **Create / Update** of a `Post`/`Page`: re-`get` the object (the change
    ///     payload's `fields` is always `None` from the embedded adapter, so the
    ///     slug/status MUST be read back from the store), derive its `/<slug>`
    ///     path, and [`render_page`](Self::render_page) it. `Some(html)` -> `put`
    ///     (regenerate); `None` (the entity is no longer published) -> `delete`
    ///     (evict). This makes an unpublish/trash a cache eviction, not a stale
    ///     page.
    ///   * **Delete**: the object is gone, so it cannot be re-`get`'d to learn its
    ///     slug, and `Change.fields` is `None` — the path is unknown. See the TODO
    ///     in [`affected_pages`](Self::affected_pages): correct delete-eviction
    ///     needs a reverse `object_id -> path` index (a later increment). For now
    ///     a Delete is logged and skipped.
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
        match change.kind {
            ChangeKind::Create | ChangeKind::Update => {
                // Only content types map to a page in permalinks v1.
                let ty = change.type_name.as_str();
                if ty != POST_TYPE && ty != PAGE_TYPE {
                    return Ok(());
                }

                // The change payload carries no fields (embedded adapter sets
                // `fields: None`); re-`get` the object to read its current slug.
                let obj = self.store.get(&change.type_name, change.object_id).await?;

                let slug = match obj.get("slug") {
                    Some(Value::String(s)) if !s.is_empty() => s.clone(),
                    // No usable slug -> no permalink to (in)validate. Nothing we
                    // can key the cache on; skip.
                    _ => return Ok(()),
                };
                let page = OutputPage {
                    path: format!("/{slug}"),
                };

                // `render_page` applies the publish gate: `Some` for a published
                // entity, `None` once it is draft/trashed/etc.
                match self.render_page(&page).await? {
                    Some(html) => {
                        // Write-through: the regenerated HTML replaces any cached
                        // copy so the next request serves it straight from blobs.
                        self.blobs
                            .put(&page_blob_key(&page), html.into_bytes())
                            .await?;
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
                // TODO(reverse-index): a hard delete removes the object, so we can
                // neither `get` it for its slug nor read fields off the change
                // (`Change.fields` is always `None`). Without a reverse
                // `object_id -> path` index we cannot know WHICH cache key to
                // evict, so we log + skip. Wiring that index (maintained as pages
                // are written) is a later increment.
                //
                // CAVEAT: skipping is safe ONLY when the delete makes the slug
                // unresolvable — then the next request misses the (now absent)
                // entity and `serve_path` returns an uncached 404. If a DIFFERENT
                // published entity still answers that slug (e.g. a Page sharing a
                // deleted Post's slug), the cache keeps serving the deleted page's
                // HTML as a cache HIT, with no event to dislodge it, until the
                // reverse index lands. A `/<slug>` collision across a hard delete
                // is the one persistent-stale case in v1.
                tracing::warn!(
                    type_name = %change.type_name.as_str(),
                    object_id = change.object_id.0,
                    "delete change: cannot map object_id -> path without a reverse index; skipping eviction (see TODO)",
                );
                Ok(())
            }
        }
    }

    /// Map a committed change to the set of output pages it invalidates. Pure
    /// policy (no I/O): given a known slug, the page itself.
    ///
    /// v1 returns ONLY the entity's own permalink (`/<slug>`). The slug must be
    /// supplied by the caller because it is not on the `Change` (the embedded
    /// adapter publishes `fields: None`, so `regen_loop` re-`get`s the object and
    /// reads the slug before calling this).
    ///
    /// TODO(index/archive/feed): a real invalidation set also includes the pages
    /// that *list* this entity — the home page, the post archive, term/category
    /// archives, and the RSS/Atom feed. Those are a documented later increment;
    /// once index pages exist as `OutputPage`s, this returns the permalink PLUS
    /// each listing page the change touches.
    pub fn affected_pages(&self, slug: &str) -> Vec<OutputPage> {
        if slug.is_empty() {
            return Vec::new();
        }
        vec![OutputPage {
            path: format!("/{slug}"),
        }]
    }

    /// Render a single output page to its final HTML, or `None` if no PUBLISHED
    /// entity backs it.
    ///
    /// Reuses the exact read-path resolution ([`content::render_path`]): block
    /// tree -> HTML via `ferropress-render`, then page chrome via
    /// `ferropress-theme`. Routing through the same code keeps a regenerated page
    /// byte-for-byte identical to an on-demand [`serve_path`] render of the same
    /// path. `Ok(None)` (unpublished/absent) is the signal `regen_loop` turns into
    /// a cache eviction.
    pub async fn render_page(
        &self,
        page: &OutputPage,
    ) -> ferropress_core::error::Result<Option<String>> {
        content::render_path(&self.store, &self.theme, &page.path).await
    }
}

#[cfg(test)]
mod tests;
