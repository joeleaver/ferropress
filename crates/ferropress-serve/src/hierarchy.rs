//! Page hierarchy: the materialized-path model + its one-time (idempotent) backfill.
//!
//! A `Page` is served at a **nested** permalink built from its ancestor slugs plus
//! its own slug (`/about/team/history`). Resolving that at request time by walking
//! the parent chain would be O(depth) store reads on the hot path AND — decisively —
//! the change feed carries no before-image and `link`/`unlink` emit no change, so an
//! ancestor rename gives the regen loop no way to (in)validate a descendant's cached
//! URL. So each page stores its full public path in a `path: String @indexed` scalar:
//! resolution is one indexed lookup, and the path rides the change feed like any
//! scalar, so a descendant's cache entry regenerates from an ordinary change.
//!
//! `path` is a **derived cache** of the parent-slug chain (the parent EDGES are the
//! source of truth). The admin handler keeps it current on every create/save + cascades
//! it to descendants on a rename/re-parent. [`backfill_page_paths`] recomputes it from
//! the authoritative parent chain for every page: it seeds `path` for pages that predate
//! the field, and (run at every boot) repairs any drift a partially-applied cascade left
//! behind. It is idempotent — it writes only the pages whose stored `path` disagrees with
//! the computed one.

use std::collections::HashMap;

use ferropress_core::PAGE_TYPE;
use ferropress_core::error::CoreError;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};
use std::sync::Arc;

/// The hard ceiling on page-hierarchy depth. It bounds every ancestor/descendant walk
/// so a corrupt parent **cycle** in stored data can never hang a walk (the walk stops
/// and the affected pages are reported, never spun on). 100 is far deeper than any real
/// page tree; it exists purely as a cycle/runaway guard.
pub const MAX_PAGE_DEPTH: usize = 100;

/// Join a parent's materialized path (`None` for a top-level page) with a page's own
/// slug into the full public path — no leading/trailing slash. A top-level page's path
/// is just its slug; `/about` + `team` → `about/team`. This is the ONE definition of how
/// a page path is composed; the backfill, the admin create/save, and the cascade all use
/// it so a page's cached URL is derived identically everywhere.
pub fn join_page_path(parent_path: Option<&str>, slug: &str) -> String {
    match parent_path {
        Some(p) if !p.is_empty() => format!("{p}/{slug}"),
        _ => slug.to_owned(),
    }
}

/// The outcome of a [`backfill_page_paths`] pass, for boot logging + tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Total pages scanned.
    pub scanned: usize,
    /// Pages whose `path` scalar was (re)written because it disagreed with the computed
    /// path (0 on a steady-state boot — the pass is idempotent).
    pub updated: usize,
    /// Pages whose parent chain hit a cycle or the depth cap; their `path` was left
    /// untouched and they are reported for an operator to reconcile.
    pub cyclic: usize,
    /// Distinct materialized paths that MORE THAN ONE page computed to — a collision the
    /// going-forward uniqueness gate prevents for new writes, but legacy data (Page.slug
    /// was `@indexed`, never `@unique`, with no prior create gate) can carry. Reported so
    /// an operator can rename the losers (only one is reachable at the shared path).
    pub collisions: Vec<String>,
}

/// Recompute every page's materialized `path` from the authoritative parent chain and
/// write back the ones that drifted (or were never set). Idempotent: a steady-state boot
/// updates nothing. Safe to run on every boot as a self-healing repair pass.
///
/// Batch-shaped: ONE `scan` + ONE batched `get_links_many` for the parent edges, then all
/// path computation happens in memory (page trees are small). Each page's path is the
/// root→self slug chain joined by `/`; a dangling parent id (target missing) is treated as
/// a root, and a cycle/over-deep chain leaves that page's path untouched and counts it in
/// `cyclic`.
pub async fn backfill_page_paths(store: &Arc<dyn RhypeStore>) -> Result<BackfillReport, CoreError> {
    let ty = TypeName::from(PAGE_TYPE);
    let pages: Vec<Object> = store.scan(&ty).await?;
    let ids: Vec<ObjectId> = pages.iter().map(|o| o.id).collect();

    // One batched read of the `parent` to-one edge for every page (id-only fast path).
    let parent_links = store.get_links_many(&ty, &ids, "parent").await?;

    let mut slug_of: HashMap<ObjectId, String> = HashMap::with_capacity(pages.len());
    let mut stored_path_of: HashMap<ObjectId, String> = HashMap::with_capacity(pages.len());
    for o in &pages {
        slug_of.insert(o.id, str_field(o, "slug"));
        stored_path_of.insert(o.id, str_field(o, "path"));
    }
    let mut parent_of: HashMap<ObjectId, Option<ObjectId>> = HashMap::with_capacity(pages.len());
    for (id, parents) in ids.iter().zip(parent_links.iter()) {
        // A `parent` target that is not itself a scanned Page (dangling) is ignored →
        // the page is treated as top-level, exactly as the resolver would see it.
        let p = parents
            .first()
            .copied()
            .filter(|pid| slug_of.contains_key(pid));
        parent_of.insert(*id, p);
    }

    let mut report = BackfillReport {
        scanned: pages.len(),
        ..BackfillReport::default()
    };

    // Compute the intended path for each page (None = cycle/over-deep → skip).
    let mut computed: HashMap<ObjectId, String> = HashMap::with_capacity(pages.len());
    for &id in &ids {
        match compute_path(id, &slug_of, &parent_of) {
            Some(path) => {
                computed.insert(id, path);
            }
            None => report.cyclic += 1,
        }
    }

    // Detect paths shared by more than one page (legacy duplicate slugs → duplicate paths).
    let mut seen: HashMap<&str, u32> = HashMap::new();
    for path in computed.values() {
        *seen.entry(path.as_str()).or_insert(0) += 1;
    }
    report.collisions = seen
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(p, _)| p.to_owned())
        .collect();
    report.collisions.sort();

    // Write back only the pages whose stored `path` disagrees with the computed one.
    for &id in &ids {
        let Some(want) = computed.get(&id) else {
            continue;
        };
        if stored_path_of.get(&id).map(String::as_str) != Some(want.as_str()) {
            let mut patch: FieldMap = FieldMap::new();
            patch.insert("path".to_owned(), Value::String(want.clone()));
            store.update(&ty, id, patch).await?;
            report.updated += 1;
        }
    }

    Ok(report)
}

/// Walk `id`'s ancestor chain root-first and join the slugs into a materialized path, or
/// `None` if the chain hits a cycle or exceeds [`MAX_PAGE_DEPTH`]. Pure (operates on the
/// preloaded slug + parent maps), so the whole backfill stays in-memory after two reads.
fn compute_path(
    id: ObjectId,
    slug_of: &HashMap<ObjectId, String>,
    parent_of: &HashMap<ObjectId, Option<ObjectId>>,
) -> Option<String> {
    // Collect slugs self→root, guarding against a cycle (a repeated id) and runaway depth.
    let mut chain: Vec<&str> = Vec::new();
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        if !seen.insert(c) || seen.len() > MAX_PAGE_DEPTH {
            return None; // cycle or over-deep
        }
        chain.push(slug_of.get(&c)?.as_str());
        cur = parent_of.get(&c).copied().flatten();
    }
    chain.reverse();
    Some(chain.join("/"))
}

/// A single field read as an owned `String` (empty when absent or not a string).
fn str_field(obj: &Object, field: &str) -> String {
    match obj.get(field) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}
