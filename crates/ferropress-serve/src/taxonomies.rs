//! Live taxonomy set for the public read path — term identity, hierarchy, and
//! archive-URL routing held in memory.
//!
//! A [`Term`](ferropress_core::Term) is public-facing three ways: its archive page
//! (`/{taxonomy_key}/{ancestor slugs…}/{slug}`), the term CHIPS on a post (name +
//! archive href, resolved live from baked ids — the byline discipline), and a nav
//! [`LinkTarget::Term`](ferropress_core::LinkTarget). All three need id → (name,
//! href) and path → id resolution on the cache-hit hot path, which must stay
//! store-free — so, exactly like [`MenuSet`](crate::menus), the whole (low-volume)
//! taxonomy forest is held in an in-memory [`TaxonomySet`] the read path clones per
//! render and the regen loop FULL-reloads ([`load_taxonomies`]) on any `Taxonomy` /
//! `Term` change (rescan-on-change, immune to incremental staleness — re-parents
//! and slug renames move whole subtrees).
//!
//! Routing is PER-SIBLING (the WP model, owner sign-off #3): a slug is unique only
//! among the terms sharing its (taxonomy, parent), so a path can only be resolved
//! by walking it segment-by-segment from the roots ([`TaxonomySet::resolve_chain`])
//! — a last-segment lookup cannot disambiguate. Each term still has exactly ONE
//! canonical URL: its own ancestor chain ([`TaxonomySet::archive_href`]), the
//! single source every consumer (archive canonical, chips, nav, the admin picker)
//! derives from so `aria_current`'s exact-match can never drift.
//!
//! The loader is DEFENSIVE (the menus discipline): a term with a missing/unknown
//! taxonomy or an empty slug is skipped + logged; a dangling or cross-taxonomy
//! parent link degrades to a root; every walk is cycle-guarded — corrupt data
//! degrades a listing, never loops or crashes the read path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use ferropress_core::error::Result;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{TAXONOMY_TYPE, TERM_TYPE};
use parking_lot::RwLock;

/// A defensive cap on ancestor-chain walks (root → term). The admin write path
/// enforces its own `MAX_TERM_DEPTH` (10); this sits above it so admin-accepted
/// data always resolves, while externally-corrupted chains terminate bounded.
const MAX_CHAIN_WALK: usize = 32;

/// One taxonomy's identity, as the read path needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomyInfo {
    pub id: u64,
    /// The stable key — the archive URL base (`"category"`, `"tag"`).
    pub key: String,
    pub label: String,
    pub hierarchical: bool,
}

/// One term's live identity: everything the read path resolves from a baked id
/// (chip name + href), an archive hit (heading name/description), or a nav target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermEntry {
    /// The owning taxonomy's key — the first archive-path segment.
    pub taxonomy_key: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    /// The parent term's id (`None` = a root term). Only ever a live member of
    /// the SAME taxonomy — the loader degrades anything else to a root.
    pub parent: Option<u64>,
}

/// The full live taxonomy forest: taxonomies by key, terms by id, plus the
/// derived indices resolution needs (per-sibling routing, children, name order).
/// Rebuilt wholesale by [`load_taxonomies`]; read purely in memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaxonomySet {
    taxonomies: BTreeMap<String, TaxonomyInfo>,
    terms: HashMap<u64, TermEntry>,
    /// Per-sibling routing: a ROOT term by `(taxonomy_key, slug)` …
    root_index: HashMap<(String, String), u64>,
    /// … and a CHILD term by `(parent_id, slug)` (the parent already pins the
    /// taxonomy, per the loader's same-taxonomy guarantee).
    child_index: HashMap<(u64, String), u64>,
    /// Direct children per term id — the descendants walk's adjacency.
    children: HashMap<u64, Vec<u64>>,
    /// Every taxonomy's term ids in the PINNED listing order (case-folded name
    /// asc, id tiebreak — `Term` has no ordinal column, so the order lives here
    /// and in the admin list, nowhere else).
    ordered: HashMap<String, Vec<u64>>,
}

impl TaxonomySet {
    /// Build a set from parts, deriving every index — the single constructor the
    /// loader AND tests use, so index derivation can never diverge from the data.
    ///
    /// `Term.slug` is `@indexed`, not `@unique` — per-sibling uniqueness is enforced only by
    /// an app-level pre-check under a process-local lock, so two rows sharing a
    /// `(taxonomy, parent, slug)` are representable (a should-be-impossible race between two
    /// instances, or externally-corrupted data). Routing MUST still resolve deterministically
    /// — the root/child routing indices always keep the LOWEST term id on a collision,
    /// regardless of the source `terms` iteration order, the same precedent
    /// [`crate::menus::load_menus`] documents for a duplicate menu-location binding. (F6 /
    /// review finding C8: before this, last-write-wins over an unordered `HashMap` meant the
    /// winner could silently flip on every unrelated rebuild.)
    pub fn build(
        taxonomies: impl IntoIterator<Item = TaxonomyInfo>,
        terms: impl IntoIterator<Item = (u64, TermEntry)>,
    ) -> Self {
        let taxonomies: BTreeMap<String, TaxonomyInfo> =
            taxonomies.into_iter().map(|t| (t.key.clone(), t)).collect();
        let terms: HashMap<u64, TermEntry> = terms.into_iter().collect();

        let mut root_index: HashMap<(String, String), u64> = HashMap::new();
        let mut child_index: HashMap<(u64, String), u64> = HashMap::new();
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut ordered: HashMap<String, Vec<u64>> = HashMap::new();
        for (id, entry) in &terms {
            match entry.parent {
                Some(parent) => {
                    // Deterministic dedupe: lowest term id wins a duplicate (parent, slug).
                    child_index
                        .entry((parent, entry.slug.clone()))
                        .and_modify(|winner| *winner = (*winner).min(*id))
                        .or_insert(*id);
                    children.entry(parent).or_default().push(*id);
                }
                None => {
                    // Same dedupe, root siblings: lowest term id wins a duplicate
                    // (taxonomy, slug).
                    root_index
                        .entry((entry.taxonomy_key.clone(), entry.slug.clone()))
                        .and_modify(|winner| *winner = (*winner).min(*id))
                        .or_insert(*id);
                }
            }
            ordered
                .entry(entry.taxonomy_key.clone())
                .or_default()
                .push(*id);
        }
        for list in children.values_mut() {
            list.sort_unstable();
        }
        for list in ordered.values_mut() {
            list.sort_by(|a, b| {
                let (ta, tb) = (&terms[a], &terms[b]);
                ta.name
                    .to_lowercase()
                    .cmp(&tb.name.to_lowercase())
                    .then(a.cmp(b))
            });
        }

        Self {
            taxonomies,
            terms,
            root_index,
            child_index,
            children,
            ordered,
        }
    }

    /// The taxonomy registered under `key`, if any.
    pub fn taxonomy(&self, key: &str) -> Option<&TaxonomyInfo> {
        self.taxonomies.get(key)
    }

    /// A term's live entry, if the id is a known term.
    pub fn term(&self, id: u64) -> Option<&TermEntry> {
        self.terms.get(&id)
    }

    /// Every term id of `taxonomy_key` in the pinned listing order (case-folded
    /// name asc, id tiebreak).
    pub fn terms_of(&self, taxonomy_key: &str) -> &[u64] {
        self.ordered
            .get(taxonomy_key)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether the set holds no taxonomies at all (the pre-seed default).
    pub fn is_empty(&self) -> bool {
        self.taxonomies.is_empty()
    }

    /// The term's canonical archive href — `/` + the taxonomy key + the
    /// root→term slug chain (`/category/fiction/space-opera`). THE single source
    /// of a term's URL: the archive's own canonical, the chips, the nav arm, and
    /// `resolve_chain` all agree by construction. `None` for an unknown id or a
    /// chain that walks out of bounds (corrupt data).
    pub fn archive_href(&self, id: u64) -> Option<String> {
        let entry = self.terms.get(&id)?;
        let mut segments = Vec::new();
        let mut cur = Some(id);
        let mut seen = HashSet::new();
        while let Some(t) = cur {
            if !seen.insert(t) || seen.len() > MAX_CHAIN_WALK {
                return None; // corrupt cycle / over-deep chain — no canonical URL
            }
            let row = self.terms.get(&t)?;
            segments.push(row.slug.as_str());
            cur = row.parent;
        }
        segments.push(entry.taxonomy_key.as_str());
        segments.reverse();
        Some(format!("/{}", segments.join("/")))
    }

    /// Resolve a slug chain UNDER `taxonomy_key` segment-by-segment from the
    /// roots — the ONLY resolution the per-sibling model admits. Empty chains
    /// never resolve (the bare `/{key}` base is not an archive). Each hop must
    /// exist or the whole chain fails — claim-only-on-resolve's precondition.
    pub fn resolve_chain(&self, taxonomy_key: &str, segments: &[&str]) -> Option<u64> {
        let (first, rest) = segments.split_first()?;
        let mut cur = *self
            .root_index
            .get(&(taxonomy_key.to_owned(), (*first).to_owned()))?;
        for seg in rest {
            cur = *self.child_index.get(&(cur, (*seg).to_owned()))?;
        }
        Some(cur)
    }

    /// Resolve a whole site-relative PATH (`category/fiction/space-opera`, no
    /// leading slash required) to the term whose canonical archive path it is:
    /// first segment = a taxonomy key, the rest = a full term chain. `None` when
    /// any part fails — the caller FALLS THROUGH to the permalink resolver
    /// (claim-only-on-resolve). Also the redirect shadow-guard's
    /// "does an archive own this path" predicate.
    pub fn resolve_archive_path(&self, path: &str) -> Option<u64> {
        let mut segments = path.trim_matches('/').split('/');
        let key = segments.next().filter(|s| !s.is_empty())?;
        if !self.taxonomies.contains_key(key) {
            return None;
        }
        let chain: Vec<&str> = segments.collect();
        if chain.iter().any(|s| s.is_empty()) {
            return None; // `a//b` is nobody's canonical path
        }
        self.resolve_chain(key, &chain)
    }

    /// Every descendant of `id` (children, grandchildren, …), cycle-guarded —
    /// the rollup query's expansion (`/category/parent` lists the parent ∪ all
    /// descendants, deduped). Order is unspecified; callers dedupe/sort the
    /// posts, not the terms.
    pub fn descendants(&self, id: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut frontier = vec![id];
        let mut seen = HashSet::from([id]);
        while let Some(cur) = frontier.pop() {
            if let Some(kids) = self.children.get(&cur) {
                for kid in kids {
                    if seen.insert(*kid) {
                        out.push(*kid);
                        frontier.push(*kid);
                    }
                }
            }
        }
        out
    }
}

/// Read the whole taxonomy forest from the store: one `Taxonomy` scan + one
/// `Term` scan + two batched link reads (owning taxonomy, parent) — never N+1.
/// Used for the boot seed AND the per-change reload (terms are low-volume; a
/// full rebuild is cheap and immune to the incremental-staleness a re-parent or
/// slug rename would inflict — the menus/redirects discipline).
///
/// Defensive row handling (a listing must degrade, never crash):
///   * a taxonomy with an empty key is skipped (no archive namespace);
///   * a term with a missing/unknown taxonomy link or an empty slug is skipped
///     + logged (it has no resolvable URL);
///   * a parent link that dangles or crosses taxonomies degrades to a root.
pub async fn load_taxonomies(store: &Arc<dyn RhypeStore>) -> Result<TaxonomySet> {
    let mut taxonomies: Vec<TaxonomyInfo> = Vec::new();
    let mut key_by_tax_id: HashMap<u64, String> = HashMap::new();
    for row in store.scan(&TypeName::from(TAXONOMY_TYPE)).await? {
        let key = match row.get("key") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                tracing::warn!(taxonomy = row.id.0, "skipping a taxonomy with no key");
                continue;
            }
        };
        key_by_tax_id.insert(row.id.0, key.clone());
        taxonomies.push(TaxonomyInfo {
            id: row.id.0,
            key,
            label: match row.get("label") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            },
            hierarchical: matches!(row.get("hierarchical"), Some(Value::Bool(true))),
        });
    }

    let term_rows = store.scan(&TypeName::from(TERM_TYPE)).await?;
    let ids: Vec<ObjectId> = term_rows.iter().map(|o| o.id).collect();
    let tax_links = store
        .get_links_many(&TypeName::from(TERM_TYPE), &ids, "taxonomy")
        .await?;
    let parent_links = store
        .get_links_many(&TypeName::from(TERM_TYPE), &ids, "parent")
        .await?;

    // First pass: rows with a live taxonomy + a non-empty slug, raw parent kept.
    let mut raw: HashMap<u64, (TermEntry, Option<u64>)> = HashMap::new();
    for ((obj, tax), parents) in term_rows.iter().zip(&tax_links).zip(&parent_links) {
        let Some(taxonomy_key) = tax
            .first()
            .and_then(|tid| key_by_tax_id.get(&tid.0))
            .cloned()
        else {
            tracing::warn!(term = obj.id.0, "skipping a term with no live taxonomy");
            continue;
        };
        let slug = match obj.get("slug") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                tracing::warn!(term = obj.id.0, "skipping a term with no slug");
                continue;
            }
        };
        let str_of = |field: &str| match obj.get(field) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        raw.insert(
            obj.id.0,
            (
                TermEntry {
                    taxonomy_key,
                    slug,
                    name: str_of("name"),
                    description: str_of("description"),
                    parent: None, // filled below once same-taxonomy liveness is known
                },
                parents.first().map(|p| p.0),
            ),
        );
    }

    // Second pass: keep a parent only when it is a kept term of the SAME taxonomy.
    let taxonomy_of: HashMap<u64, String> = raw
        .iter()
        .map(|(id, (e, _))| (*id, e.taxonomy_key.clone()))
        .collect();
    let terms = raw.into_iter().map(|(id, (mut entry, raw_parent))| {
        entry.parent =
            raw_parent.filter(|p| taxonomy_of.get(p) == Some(&entry.taxonomy_key) && *p != id);
        (id, entry)
    });

    Ok(TaxonomySet::build(taxonomies, terms))
}

/// A cheaply-cloneable handle to the current live [`TaxonomySet`], shared between
/// the read path (archives, chips, nav Term targets) and the regen loop (which
/// full-reloads it on a `Taxonomy`/`Term` change). Same read/write discipline as
/// [`MenuHandle`](crate::menus::MenuHandle): reads clone the inner `Arc` under a
/// short read lock; a reload swaps it under a short write lock, so a render never
/// sees a half-updated forest.
#[derive(Clone)]
pub struct TaxonomyHandle(Arc<RwLock<Arc<TaxonomySet>>>);

impl TaxonomyHandle {
    /// Seed the handle with an initial set (built at startup from the store).
    pub fn new(initial: TaxonomySet) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current set snapshot (cheap `Arc` clone). Hold it for one render.
    pub fn current(&self) -> Arc<TaxonomySet> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current set (a full reload).
    pub fn set(&self, next: TaxonomySet) {
        *self.0.write() = Arc::new(next);
    }

    /// Whether a live term archive currently owns `path` — the redirect shadow-guard's "does
    /// an archive claim this URL" predicate ([`TaxonomySet::resolve_archive_path`], wrapped so
    /// callers that only need the yes/no answer don't have to hold a snapshot themselves). A
    /// live archive always wins over a stale 301: the serve path skips a redirect-table hit
    /// here, and the admin write path refuses to RECORD a redirect from an owned path.
    pub fn term_path_owns(&self, path: &str) -> bool {
        self.current().resolve_archive_path(path).is_some()
    }
}

impl Default for TaxonomyHandle {
    /// An empty set — the state before the store is read (and what tests get
    /// without wiring taxonomies; no archive resolves, chips resolve to nothing).
    fn default() -> Self {
        Self::new(TaxonomySet::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(taxonomy_key: &str, slug: &str, name: &str, parent: Option<u64>) -> TermEntry {
        TermEntry {
            taxonomy_key: taxonomy_key.to_owned(),
            slug: slug.to_owned(),
            name: name.to_owned(),
            description: String::new(),
            parent,
        }
    }

    /// category: fiction(1) → space-opera(2) → far-future(4); news(3) at root;
    /// a second `space-opera` root (5, per-sibling legal); tag: rust(6).
    fn sample() -> TaxonomySet {
        TaxonomySet::build(
            [
                TaxonomyInfo {
                    id: 100,
                    key: "category".into(),
                    label: "Categories".into(),
                    hierarchical: true,
                },
                TaxonomyInfo {
                    id: 101,
                    key: "tag".into(),
                    label: "Tags".into(),
                    hierarchical: false,
                },
            ],
            [
                (1, entry("category", "fiction", "Fiction", None)),
                (2, entry("category", "space-opera", "Space Opera", Some(1))),
                (4, entry("category", "far-future", "Far Future", Some(2))),
                (3, entry("category", "news", "News", None)),
                (
                    5,
                    entry("category", "space-opera", "Space Opera (root)", None),
                ),
                (6, entry("tag", "rust", "Rust", None)),
            ],
        )
    }

    #[test]
    fn archive_href_is_the_full_ancestor_chain() {
        let set = sample();
        assert_eq!(set.archive_href(1).as_deref(), Some("/category/fiction"));
        assert_eq!(
            set.archive_href(4).as_deref(),
            Some("/category/fiction/space-opera/far-future")
        );
        assert_eq!(set.archive_href(6).as_deref(), Some("/tag/rust"));
        assert_eq!(set.archive_href(99), None);
    }

    #[test]
    fn per_sibling_duplicates_resolve_by_their_chains() {
        // Two `space-opera` terms — only the ancestor chain disambiguates.
        let set = sample();
        assert_eq!(
            set.resolve_archive_path("category/fiction/space-opera"),
            Some(2)
        );
        assert_eq!(set.resolve_archive_path("/category/space-opera"), Some(5));
        // And each one's canonical href round-trips to itself.
        for id in [2, 5] {
            let href = set.archive_href(id).unwrap();
            assert_eq!(set.resolve_archive_path(&href), Some(id), "{href}");
        }
    }

    /// F6 (review finding C8): TRUE siblings — same taxonomy, same parent (both `None` here,
    /// i.e. both roots), same slug — must resolve to a STABLE winner (lowest term id),
    /// regardless of the order `TaxonomySet::build` receives them in. This is the collision
    /// `per_sibling_duplicates_resolve_by_their_chains` deliberately does NOT exercise (its two
    /// `space-opera` terms have DIFFERENT parents, so their chains already disambiguate them —
    /// this test's two terms are truly indistinguishable by path).
    #[test]
    fn duplicate_sibling_slugs_resolve_deterministically_to_the_lowest_id() {
        let tax = [TaxonomyInfo {
            id: 100,
            key: "category".into(),
            label: "Categories".into(),
            hierarchical: true,
        }];
        // Two ROOT terms sharing (taxonomy="category", slug="duplicate") — ids 21 and 20 (20 is
        // lower). Built in BOTH id orders, over an underlying `HashMap` whose iteration order is
        // otherwise unspecified, so a pass here can't be a coincidence of insertion order.
        let terms_desc = [
            (21, entry("category", "duplicate", "Duplicate (21)", None)),
            (20, entry("category", "duplicate", "Duplicate (20)", None)),
        ];
        let terms_asc = [
            (20, entry("category", "duplicate", "Duplicate (20)", None)),
            (21, entry("category", "duplicate", "Duplicate (21)", None)),
        ];
        for terms in [terms_desc, terms_asc] {
            let set = TaxonomySet::build(tax.clone(), terms);
            assert_eq!(
                set.resolve_archive_path("category/duplicate"),
                Some(20),
                "the lowest id must win regardless of build order"
            );
        }

        // The CHILD-sibling twin: two terms sharing (parent, slug) under a common parent.
        let terms_children_desc = [
            (1, entry("category", "parent", "Parent", None)),
            (31, entry("category", "child", "Child (31)", Some(1))),
            (30, entry("category", "child", "Child (30)", Some(1))),
        ];
        let terms_children_asc = [
            (1, entry("category", "parent", "Parent", None)),
            (30, entry("category", "child", "Child (30)", Some(1))),
            (31, entry("category", "child", "Child (31)", Some(1))),
        ];
        for terms in [terms_children_desc, terms_children_asc] {
            let set = TaxonomySet::build(tax.clone(), terms);
            assert_eq!(
                set.resolve_archive_path("category/parent/child"),
                Some(30),
                "the lowest CHILD id must win regardless of build order"
            );
        }
    }

    #[test]
    fn claim_only_on_resolve_fails_partial_chains() {
        let set = sample();
        // The bare base is NOT an archive (a root page may live there).
        assert_eq!(set.resolve_archive_path("category"), None);
        // A chain with any unresolvable hop fails wholesale → permalink fallthrough.
        assert_eq!(set.resolve_archive_path("category/nope"), None);
        assert_eq!(set.resolve_archive_path("category/fiction/nope"), None);
        assert_eq!(set.resolve_archive_path("category/news/space-opera"), None);
        // An unknown taxonomy key never claims anything.
        assert_eq!(set.resolve_archive_path("blog/fiction"), None);
        // Degenerate shapes.
        assert_eq!(set.resolve_archive_path(""), None);
        assert_eq!(set.resolve_archive_path("category//news"), None);
    }

    #[test]
    fn descendants_roll_up_the_whole_subtree() {
        let set = sample();
        let mut d = set.descendants(1);
        d.sort_unstable();
        assert_eq!(d, vec![2, 4]);
        assert!(set.descendants(3).is_empty());
    }

    #[test]
    fn a_parent_cycle_terminates_and_yields_no_canonical_url() {
        // Externally-corrupted data: 1 ⇄ 2. Walks must terminate; the terms have
        // no canonical URL rather than an infinite one.
        let set = TaxonomySet::build(
            [TaxonomyInfo {
                id: 100,
                key: "category".into(),
                label: String::new(),
                hierarchical: true,
            }],
            [
                (1, entry("category", "a", "A", Some(2))),
                (2, entry("category", "b", "B", Some(1))),
            ],
        );
        assert_eq!(set.archive_href(1), None);
        assert_eq!(set.archive_href(2), None);
        // descendants() must terminate too.
        assert_eq!(set.descendants(1), vec![2]);
    }

    #[test]
    fn terms_of_is_name_ordered_with_id_tiebreak() {
        let set = sample();
        // Case-folded name asc: Far Future(4), Fiction(1), News(3),
        // Space Opera(2), Space Opera (root)(5).
        assert_eq!(set.terms_of("category"), &[4, 1, 3, 2, 5]);
        assert_eq!(set.terms_of("tag"), &[6]);
        assert!(set.terms_of("nope").is_empty());
    }
}
