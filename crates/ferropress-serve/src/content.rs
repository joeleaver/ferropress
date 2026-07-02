//! Content resolution + on-demand SSR (Ferropress v1 serving path).
//!
//! Given a request *path*, this resolves the published entity behind it, renders
//! its block tree to HTML via [`ferropress_render`], and wraps that body in page
//! chrome via [`ferropress_theme`]. It is the read-side counterpart to the (still
//! deferred) [`ServeEngine`](crate::ServeEngine) prerender loop: v1 serves every
//! page on demand; the prerender-cache + change-driven regen is a later
//! increment (see the TODO on `ServeEngine::regen_loop`).
//!
//! ## Permalinks (v1)
//!
//! Flat, published-only: a path `"/<slug>"` resolves to a **published** `Post`
//! with that slug, falling back to a **published** `Page`. There is no nested
//! hierarchy, date prefix, or custom permalink structure yet — that is a
//! deliberate v1 simplification, documented here and in
//! [`slug_from_path`]. Everything else (404 / 500) flows from the absence of a
//! match or a backend/render fault.
//!
//! ## No block -> HTML logic here
//!
//! This module never inspects `BlockKind` or emits markup for a block: the one
//! and only block dispatch lives in `ferropress-render` (the one-shared-renderer
//! invariant). Here we only orchestrate store lookup -> `render` -> theme.

use std::sync::{Arc, LazyLock};

use ferropress_core::error::CoreError;
use ferropress_core::ports::BlobStore;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use ferropress_core::{BlockTree, Compare, FilterSpec, Object, PAGE_TYPE, POST_TYPE, Status};
use ferropress_render::{CustomBlockRenderer, Html, RenderMode, render_with};
use ferropress_theme::{PageContext, SandboxLimits, ThemeEngine, ThemeError};

use crate::cache_key;

/// The name of the built-in chrome template the content service renders into.
/// Named `.html` so the MiniJinja host auto-escapes interpolations like
/// `{{ title }}`; the already-rendered block body is injected via the `safe`
/// filter (`{{ content | safe }}`).
pub const PAGE_TEMPLATE: &str = "page.html";

/// Source of the single built-in page-chrome template. Kept here so the
/// composition root and the integration test share ONE source of truth (through
/// [`default_theme`]); a real theme system will later load author templates.
///
/// Besides the page title + pre-rendered content, the chrome emits the public-site
/// **island** mount points (`#fp-search`, `#fp-comments`) and the ESM `<script>`
/// that boots the wasm bundle served at `/_fp/islands`. The islands hydrate into
/// those placeholders client-side; a page that never loads the bundle (or has the
/// islands disabled) just shows empty placeholders. The comments island derives
/// its slug from `window.location.pathname` (permalinks v1 are flat `/<slug>`).
pub const PAGE_TEMPLATE_SRC: &str = r#"<!doctype html>
<html>
<head><title>{{ title }}</title></head>
<body>
<div id="fp-search"></div>
{{ content | safe }}
<div id="fp-comments"></div>
<script type="module">
import init from '/_fp/islands/ferropress_islands.js';
init({ module_or_path: '/_fp/islands/ferropress_islands_bg.wasm' });
</script>
</body>
</html>
"#;

/// Build the v1 [`ThemeEngine`] with [`PAGE_TEMPLATE`] registered. Both the
/// composition root (`ferropress-server`) and the end-to-end test call this, so
/// the page chrome they exercise is byte-for-byte identical.
pub fn default_theme() -> Result<ThemeEngine, ThemeError> {
    let mut theme = ThemeEngine::new(SandboxLimits::default());
    theme.add_template(PAGE_TEMPLATE.to_owned(), PAGE_TEMPLATE_SRC.to_owned())?;
    Ok(theme)
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

/// Derive the lookup slug from a request path (permalinks v1: flat `/<slug>`).
///
/// Strips a single leading `'/'` and any trailing `'/'`; the remainder is the
/// slug. `"/"` (the site root) yields an empty slug — no published entity has an
/// empty slug, so the root currently resolves to [`Resolved::NotFound`] until a
/// home page is wired. Nested paths (`"/a/b"`) are passed through verbatim as the
/// slug, so they simply will not match a flat slug today; nested permalinks are
/// a later increment.
pub fn slug_from_path(path: &str) -> &str {
    path.trim_start_matches('/').trim_end_matches('/')
}

/// Resolve a request path to a rendered HTML document (v1 SSR-on-demand).
///
/// Resolution order (published-only): `Post` by slug, then `Page` by slug. On a
/// hit, the stored `block_tree` JSON string is parsed, rendered in
/// [`RenderMode::Publish`], and wrapped in the `PAGE_TEMPLATE` chrome.
///
/// This is the *uncached* render. The cache-first hot path is [`serve_path`],
/// which consults the prerender [`BlobStore`] before falling through to here.
pub async fn resolve_path(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    path: &str,
) -> Resolved {
    match render_path(store, theme, custom, path).await {
        Ok(Some(html)) => Resolved::Found(html),
        Ok(None) => Resolved::NotFound,
        Err(e) => Resolved::Error(e),
    }
}

/// Cache-first resolution: the static-first hot path the HTTP fallback calls.
///
/// 1. Try the prerender cache (`blobs.get(cache_key(path))`). On a hit, return
///    the stored UTF-8 HTML verbatim — **no store lookup, no re-render**. This is
///    what makes serving "static-first": once a page is prerendered (on first
///    request here, or by the regen loop), it is served straight from blob bytes.
/// 2. On a miss, fall through to the uncached [`render_path`]. If it produces
///    HTML, write it *through* to the cache (`blobs.put`) so the next request
///    hits, then return `Found`. `NotFound` / `Error` pass through unchanged (we
///    never cache a 404 or a fault).
///
/// The cache is **best-effort**: a blob read or write failure never fails the
/// request. A read error is treated as a miss (we just render), and a
/// write-through error is logged and swallowed (we still return the freshly
/// rendered HTML). Cache I/O faults degrade us to v1 SSR-on-demand, never to a
/// 500. The change-driven regen loop
/// ([`ServeEngine::regen_loop`](crate::ServeEngine::regen_loop)) keeps populated
/// entries fresh.
pub async fn serve_path(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    path: &str,
) -> Resolved {
    let key = cache_key(path);

    // 1. Cache read. A hit short-circuits; a `NotFound` is an ordinary miss; any
    //    other error means the cache is degraded — log and treat it as a miss so
    //    the request still succeeds via a fresh render.
    match blobs.get(&key).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(html) => return Resolved::Found(html),
            Err(e) => {
                // A non-UTF-8 cache entry should never happen (we only ever put
                // rendered HTML), but if it does, don't serve garbage — log and
                // re-render below.
                tracing::warn!(%path, error = %e, "prerender cache held non-UTF-8 bytes; re-rendering");
            }
        },
        Err(CoreError::NotFound { .. }) => {
            // Ordinary cache miss — fall through to render-on-demand.
        }
        Err(e) => {
            tracing::warn!(%path, error = %e, "prerender cache read failed; falling back to render");
        }
    }

    // 2. Miss: render on demand, then populate the cache (write-through).
    match render_path(store, theme, custom, path).await {
        Ok(Some(html)) => {
            if let Err(e) = blobs.put(&key, html.clone().into_bytes()).await {
                // Populate-on-miss is best-effort: a write failure must not fail
                // the request — log it and serve the rendered HTML anyway.
                tracing::warn!(%path, error = %e, "prerender cache write-through failed; serving uncached render");
            }
            Resolved::Found(html)
        }
        Ok(None) => Resolved::NotFound,
        Err(e) => Resolved::Error(e),
    }
}

/// Uncached resolution returning `Result<Option<html>>` so the `?` operator can
/// carry `CoreError`s and `Ok(None)` cleanly distinguishes "not found" from
/// "rendered". [`resolve_path`] folds this into [`Resolved`]; [`serve_path`] and
/// the regen loop call it on a cache miss / regeneration.
pub(crate) async fn render_path(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    path: &str,
) -> Result<Option<String>, CoreError> {
    let slug = slug_from_path(path);

    // Permalinks v1: the published Post, then the published Page, behind this
    // slug. The shared resolver is the single definition of "published at this
    // slug" (also used by the island comment API).
    let object = match resolve_published_entity(store, slug).await? {
        Some((_type_name, obj)) => obj,
        None => return Ok(None),
    };

    let html = render_object(theme, custom, &object)?;
    Ok(Some(html))
}

/// Resolve a request slug to the PUBLISHED entity behind it, returning its store
/// type-name ([`POST_TYPE`] or [`PAGE_TYPE`]) and the materialized object.
///
/// This is the single definition of the v1 permalink rule: a published `Post` by
/// slug takes precedence, then a published `Page`. `Ok(None)` means no published
/// entity matched (callers map that to a 404). An empty slug never matches (no
/// flat entity has an empty slug; the site root is not yet a home page).
///
/// Shared by [`render_path`] (page rendering) and the island comment API, so a
/// comment can only ever attach to — and be listed for — content that is actually
/// publicly served, under ONE definition of "published at this slug". Returning
/// the type-name lets the comment API pick the correct `Post.comments` /
/// `Page.comments` relation to traverse.
pub async fn resolve_published_entity(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
) -> Result<Option<(&'static str, Object)>, CoreError> {
    if slug.is_empty() {
        return Ok(None);
    }
    if let Some(obj) = find_published(store, POST_TYPE, slug).await? {
        return Ok(Some((POST_TYPE, obj)));
    }
    if let Some(obj) = find_published(store, PAGE_TYPE, slug).await? {
        return Ok(Some((PAGE_TYPE, obj)));
    }
    Ok(None)
}

/// Look up a single PUBLISHED object of `type_name` by exact slug.
///
/// Runs the indexed single-predicate `filter` (`slug == slug`, limit 1) the
/// engine fast-paths, then gates on `status == "published"` in Rust. The status
/// gate is a second Rust check rather than a second predicate because the engine
/// filter is single-predicate (compound predicates are caller-composed); for a
/// `limit 1` slug hit a one-row post-filter is cheaper than a second scan.
async fn find_published(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    slug: &str,
) -> Result<Option<Object>, CoreError> {
    let hits = store
        .filter(FilterSpec {
            type_name: TypeName::from(type_name),
            field: "slug".to_owned(),
            op: Compare::Eq,
            value: Value::String(slug.to_owned()),
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

/// Turn a resolved object into a full HTML document: parse its `block_tree` JSON
/// string, render the body, and wrap it in the page-chrome template.
///
/// No `BlockKind` is inspected here — block markup is produced solely by
/// `ferropress_render::render_with` (the one-shared-renderer invariant), with
/// `custom` resolving plugin (`BlockKind::Custom`) blocks. The `title` is read off
/// the object's `title` field (empty if absent).
///
/// `pub(crate)` so the regen loop's `render_page` can render an object it has
/// already `get`-fetched (by id, off a change) without going back through the
/// slug-based [`render_path`] lookup.
pub(crate) fn render_object(
    theme: &ThemeEngine,
    custom: &dyn CustomBlockRenderer,
    obj: &Object,
) -> Result<String, CoreError> {
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
    // media `src`s (see [`rewrite_media_srcs`]) — the serve layer's job, not the
    // pure renderer's. This runs pre-cache in the one shared render path.
    let body = Html(rewrite_media_srcs(
        render_with(&tree, RenderMode::Publish, custom).as_str(),
    ));

    let title = match obj.get("title") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let ctx = PageContext {
        title,
        // TODO: hydrate SEO from the stored `seo` JSON String once the chrome
        // template consumes canonical/robots/og tags.
        seo: None,
        content: body,
    };

    theme
        .render_page(PAGE_TEMPLATE, &ctx)
        // ThemeError does not convert to CoreError; carry its message so the HTTP
        // layer can log it and return a generic 500.
        .map_err(|e| CoreError::Store(format!("theme render failed: {e}")))
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
