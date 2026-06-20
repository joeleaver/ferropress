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

use std::sync::Arc;

use ferropress_core::error::CoreError;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use ferropress_core::{BlockTree, Compare, FilterSpec, Object, PAGE_TYPE, POST_TYPE, Status};
use ferropress_render::{RenderMode, render};
use ferropress_theme::{PageContext, SandboxLimits, ThemeEngine, ThemeError};

/// The name of the built-in chrome template the content service renders into.
/// Named `.html` so the MiniJinja host auto-escapes interpolations like
/// `{{ title }}`; the already-rendered block body is injected via the `safe`
/// filter (`{{ content | safe }}`).
pub const PAGE_TEMPLATE: &str = "page.html";

/// Source of the single built-in page-chrome template. Kept here so the
/// composition root and the integration test share ONE source of truth (through
/// [`default_theme`]); a real theme system will later load author templates.
pub const PAGE_TEMPLATE_SRC: &str = "<!doctype html>\n<html>\n<head><title>{{ title }}</title></head>\n<body>{{ content | safe }}</body>\n</html>\n";

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
/// TODO (prerender cache): before this on-demand render, consult the prerender
/// `BlobStore` cache for the path and serve the stored HTML on a hit; on a miss,
/// render here and (optionally) populate the cache. The change-driven regen loop
/// ([`ServeEngine::regen_loop`](crate::ServeEngine::regen_loop)) then keeps that
/// cache fresh. v1 renders every request on demand and does not cache.
pub async fn resolve_path(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    path: &str,
) -> Resolved {
    match resolve_inner(store, theme, path).await {
        Ok(Some(html)) => Resolved::Found(html),
        Ok(None) => Resolved::NotFound,
        Err(e) => Resolved::Error(e),
    }
}

/// Inner resolution returning `Result<Option<html>>` so the `?` operator can
/// carry `CoreError`s and `Ok(None)` cleanly distinguishes "not found" from
/// "rendered". [`resolve_path`] folds this into [`Resolved`].
async fn resolve_inner(
    store: &Arc<dyn RhypeStore>,
    theme: &ThemeEngine,
    path: &str,
) -> Result<Option<String>, CoreError> {
    let slug = slug_from_path(path);
    if slug.is_empty() {
        // No flat entity has an empty slug (site root is not yet a home page).
        return Ok(None);
    }

    // Permalinks v1: published Post first, then published Page.
    let object = match find_published(store, POST_TYPE, slug).await? {
        Some(obj) => obj,
        None => match find_published(store, PAGE_TYPE, slug).await? {
            Some(obj) => obj,
            None => return Ok(None),
        },
    };

    let html = render_object(theme, &object)?;
    Ok(Some(html))
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
fn is_published(obj: &Object) -> bool {
    matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Published.as_str())
}

/// Turn a resolved object into a full HTML document: parse its `block_tree` JSON
/// string, render the body, and wrap it in the page-chrome template.
///
/// No `BlockKind` is inspected here — block markup is produced solely by
/// `ferropress_render::render` (the one-shared-renderer invariant). The `title`
/// is read off the object's `title` field (empty if absent).
fn render_object(theme: &ThemeEngine, obj: &Object) -> Result<String, CoreError> {
    // `block_tree` is persisted as a JSON *String* (not Bytes / Json scalar).
    let tree = match obj.get("block_tree") {
        Some(Value::String(s)) => BlockTree::from_json_str(s)?,
        other => {
            return Err(CoreError::TypeMismatch {
                type_name: obj.type_name.as_str().to_owned(),
                field: "block_tree".to_owned(),
                detail: format!("expected JSON String, got {other:?}"),
            });
        }
    };

    let body = render(&tree, RenderMode::Publish);

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
