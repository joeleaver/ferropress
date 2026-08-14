//! Cross-cutting content operations shared by the Post and Page admin handlers.
//!
//! These enforce the invariants a nested-permalink model needs to stay correct across BOTH
//! entities, so posts and pages can't disagree:
//!   * [`validate_slug`] — a slug must be a single flat path segment (the resolver splits a
//!     request path on `/`).
//!   * [`occupants_of`] — the cross-entity permalink namespace: a value must not be held as a
//!     Post `slug` AND a Page `path` at once (else one entity is unreachable and they share a
//!     cache blob).
//!   * [`record_move`] — when a permalink MOVES (a rename/re-parent), evict the stale cache
//!     blob at the old path and record a 301 old→new (with chain-collapse + shadow-guard) so
//!     the old URL forwards instead of 404ing. This is the write half of the redirect
//!     subsystem whose serve half lives in `ferropress-serve::redirects`.
//!   * [`retire_redirects_at`] — the shadow-guard on create: a live page taking a path drops
//!     any redirect FROM that path so a stale 301 can't mask it.

use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value};
use ferropress_core::{PAGE_TYPE, POST_TYPE, REDIRECT_TYPE};
use ferropress_serve::cache_key;

use super::AdminError;
use crate::AppState;

/// The default redirect status a move records — a permanent move.
const REDIRECT_STATUS: u32 = 301;

/// Path bases the PUBLIC URL grammar reserves, which top-level content must not
/// occupy: `page` is the home-pagination base (`/page/N` — a Page rooted at `page`
/// would collide with it in both routing and, worse, the listing cache namespace)
/// and `feed` is reserved beside the `/feed.xml`/`/feed.atom` routes for future
/// feed surfaces. Terms reserve the same tokens at EVERY level (see
/// `terms::RESERVED_TERM_SLUGS`) because the archive grammar strips a trailing
/// `/page/{N}` anywhere; content paths only ever collide at the ROOT.
const RESERVED_TOP_LEVEL_SLUGS: &[&str] = &["page", "feed"];

/// Reject a public path whose ROOT segment is a reserved URL base. `path_key` is a
/// trimmed path key (a Post slug, or a Page's full materialized path) — only its
/// first segment is checked, so a nested `about/page` stays legal.
pub(super) fn ensure_unreserved_root(path_key: &str) -> Result<(), AdminError> {
    let first = path_key.split('/').next().unwrap_or(path_key);
    if RESERVED_TOP_LEVEL_SLUGS.contains(&first) {
        return Err(AdminError::BadRequest(format!(
            "the slug {first:?} is reserved (pagination / feeds)"
        )));
    }
    Ok(())
}

/// Validate + normalize a slug so it is exactly ONE flat path segment. The nested-permalink
/// resolver splits a request path on `/` and keys a page on its full materialized `path`, so a
/// slug carrying a `/` would forge hierarchy and collide; whitespace/control chars and the
/// `.`/`..` traversal tokens are rejected too. Returns the trimmed slug on success.
pub(super) fn validate_slug(slug: &str) -> Result<String, AdminError> {
    let s = slug.trim();
    if s.is_empty() {
        return Err(AdminError::BadRequest("slug must not be empty".to_owned()));
    }
    if s == "." || s == ".." {
        return Err(AdminError::BadRequest(format!(
            "slug {s:?} is not a valid path segment"
        )));
    }
    if s.contains('/') || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AdminError::BadRequest(format!(
            "slug {s:?} must be a single segment with no slash or whitespace"
        )));
    }
    Ok(s.to_owned())
}

/// Every `(type, id)` currently occupying `value` in the public permalink namespace — a Post by
/// `slug` OR a Page by `path`. All statuses are considered (a draft that later publishes would
/// collide), matching the established Post `slug_taken` discipline. The caller excludes its own
/// id(s) and treats any remaining occupant as a 409 collision.
pub(super) async fn occupants_of(
    state: &AppState,
    value: &str,
) -> Result<Vec<(&'static str, ObjectId)>, AdminError> {
    let mut hits = Vec::new();
    for (type_name, field) in [(POST_TYPE, "slug"), (PAGE_TYPE, "path")] {
        let rows = state
            .store
            .filter(FilterSpec {
                type_name: TypeName::from(type_name),
                field: field.to_owned(),
                op: Compare::Eq,
                value: Value::String(value.to_owned()),
                // A handful is plenty to detect a collision + skip the caller's own id(s).
                limit: Some(4),
            })
            .await?;
        for o in rows {
            hits.push((type_name, o.id));
        }
    }
    Ok(hits)
}

/// Reject `value` (a Post slug or a Page path) when it is already held in the permalink
/// namespace by anything OTHER than an id in `exclude`. `exclude` covers the caller's own row
/// AND — for a page re-parent/rename cascade — every member of the moving subtree, so a subtree
/// can move without colliding with itself.
pub(super) async fn is_taken(
    state: &AppState,
    value: &str,
    exclude: &[ObjectId],
) -> Result<bool, AdminError> {
    Ok(occupants_of(state, value)
        .await?
        .into_iter()
        .any(|(_, id)| !exclude.contains(&id)))
}

/// Bookkeeping when a permalink MOVES from `old_key` to `new_key` (trimmed path keys, e.g.
/// `about/team`). Order is load-bearing: the caller must have ALREADY written the new `path`
/// scalar (so `resolve(old)` yields nothing) before calling this, then:
///   1. evict the stale cache blob at the old path (best-effort — a fault just means a stale
///      blob lingers until the next reshaping change, and the 301 below forwards regardless);
///   2. shadow-guard — drop any redirect FROM the new path (it is a live page now);
///   3. chain-collapse — any redirect pointing TO the old path now points TO the new one
///      (`X→old` becomes `X→new`), so a chain never forms;
///   4. upsert the redirect `old→new` (its create/update rides the feed → the live table).
///
/// A no-op when the path did not actually change.
pub(super) async fn record_move(
    state: &AppState,
    old_key: &str,
    new_key: &str,
) -> Result<(), AdminError> {
    if old_key == new_key {
        return Ok(());
    }
    let old_abs = format!("/{old_key}");
    let new_abs = format!("/{new_key}");

    // 1. Evict the stale cache blob at the old path (best-effort).
    if let Err(e) = state.blobs.delete(&cache_key(&old_abs)).await {
        tracing::warn!(path = %old_abs, error = %e, "evicting the moved page's old cache blob failed");
    }

    // 2. Shadow-guard: the new path is a live page → it must not be a redirect SOURCE.
    delete_redirects_from(state, &new_abs).await?;

    // 3. Chain-collapse: repoint any `X→old` to `X→new` (and drop a would-be self-redirect).
    for (id, from) in redirects_pointing_to(state, &old_abs).await? {
        if from == new_abs {
            // `new→old` would become `new→new`; new is live, so just retire it.
            state
                .store
                .delete(&TypeName::from(REDIRECT_TYPE), id)
                .await?;
        } else {
            let mut patch: FieldMap = FieldMap::new();
            patch.insert("to_path".to_owned(), Value::String(new_abs.clone()));
            state
                .store
                .update(&TypeName::from(REDIRECT_TYPE), id, patch)
                .await?;
        }
    }

    // 4. Upsert `old→new` (from_path is @unique).
    upsert_redirect(state, &old_abs, &new_abs).await?;
    Ok(())
}

/// Drop any redirect whose `from_path` equals `path_abs` — the shadow-guard so a live page at
/// `path_abs` is never masked by a stale 301. Used by [`record_move`] (the new path) and by a
/// create at a previously-redirected path ([`retire_redirects_at`]).
pub(super) async fn retire_redirects_at(
    state: &AppState,
    path_key: &str,
) -> Result<(), AdminError> {
    delete_redirects_from(state, &format!("/{path_key}")).await
}

/// Delete every `Redirect` whose `from_path` == `from_abs` (`from_path` is `@unique`, so at
/// most one — but we tolerate legacy duplicates).
async fn delete_redirects_from(state: &AppState, from_abs: &str) -> Result<(), AdminError> {
    let rows = state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(REDIRECT_TYPE),
            field: "from_path".to_owned(),
            op: Compare::Eq,
            value: Value::String(from_abs.to_owned()),
            limit: Some(8),
        })
        .await?;
    for o in rows {
        state
            .store
            .delete(&TypeName::from(REDIRECT_TYPE), o.id)
            .await?;
    }
    Ok(())
}

/// `(id, from_path)` of every redirect whose `to_path` == `to_abs`. `to_path` is unindexed, so
/// this scans the (low-volume) redirect table.
async fn redirects_pointing_to(
    state: &AppState,
    to_abs: &str,
) -> Result<Vec<(ObjectId, String)>, AdminError> {
    let mut out = Vec::new();
    for o in state.store.scan(&TypeName::from(REDIRECT_TYPE)).await? {
        let to = match o.get("to_path") {
            Some(Value::String(s)) => s.as_str(),
            _ => continue,
        };
        if to == to_abs {
            let from = match o.get("from_path") {
                Some(Value::String(s)) => s.clone(),
                _ => continue,
            };
            out.push((o.id, from));
        }
    }
    Ok(out)
}

/// Create or update the redirect `from_abs → to_abs` (301). A self-redirect is never written.
async fn upsert_redirect(state: &AppState, from_abs: &str, to_abs: &str) -> Result<(), AdminError> {
    if from_abs == to_abs {
        return Ok(());
    }
    // Shadow-guard, term-archive flavor: a live archive OWNS `from_abs` → never record a
    // redirect FROM it (the archive must win over a 301, matching the live-page shadow-guard
    // `retire_redirects_at` already enforces). Checked here — the one place a redirect is
    // actually written — rather than at every `record_move` call site.
    if state.taxonomies.term_path_owns(from_abs) {
        tracing::debug!(
            from = %from_abs,
            "skipping a redirect record — a live term archive owns this path"
        );
        return Ok(());
    }
    let existing = state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(REDIRECT_TYPE),
            field: "from_path".to_owned(),
            op: Compare::Eq,
            value: Value::String(from_abs.to_owned()),
            limit: Some(1),
        })
        .await?;
    if let Some(o) = existing.into_iter().next() {
        let mut patch: FieldMap = FieldMap::new();
        patch.insert("to_path".to_owned(), Value::String(to_abs.to_owned()));
        patch.insert("status_code".to_owned(), Value::U32(REDIRECT_STATUS));
        state
            .store
            .update(&TypeName::from(REDIRECT_TYPE), o.id, patch)
            .await?;
    } else {
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("from_path".to_owned(), Value::String(from_abs.to_owned()));
        fields.insert("to_path".to_owned(), Value::String(to_abs.to_owned()));
        fields.insert("status_code".to_owned(), Value::U32(REDIRECT_STATUS));
        state
            .store
            .create(&TypeName::from(REDIRECT_TYPE), fields)
            .await?;
    }
    Ok(())
}
