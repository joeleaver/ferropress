//! Live nav menus for the public read path — structure held in memory, hrefs composed live.
//!
//! A [`Menu`](ferropress_core::Menu) is a named, nestable tree of
//! [`MenuItem`](ferropress_core::MenuItem)s; a [`MenuLocation`](ferropress_core::MenuLocation)
//! binds a theme *location* (`"primary"`, `"footer"`, …) to a menu (one location → at most
//! one menu; one menu may fill many locations). Menus are **chrome**: they frame every page
//! but bake into none, so — like [`SiteSettings`](crate::settings), the
//! [`AuthorDirectory`](crate::authors), and the [`RedirectMap`](crate::redirects) — they are
//! resolved LIVE, not cached into page envelopes. A menu edit therefore evicts **no** page.
//!
//! Two live handles cooperate (the design's resolution mechanism (b)):
//!   * this module's [`MenuHandle`] holds the menu **structure** — the item forest per
//!     location, each item carrying its label, typed [`LinkTarget`] (target *ids*, not hrefs),
//!     `new_tab`, and children. It is FULL-reloaded ([`load_menus`]) on a `Menu` / `MenuItem` /
//!     `MenuLocation` change only (menus are low-volume, so a rescan is cheap and immune to
//!     incremental-staleness — the [`RedirectMap`](crate::redirects) discipline).
//!   * the [`ContentIndex`](crate::content_index) resolves a `Post`/`Page` id → its live
//!     `(href, title)`, maintained incrementally off `Post`/`Page` changes.
//!
//! [`MenuSet::compose`] joins the two — pure, in-memory, ZERO store I/O — into the typed
//! [`MenuItemCtx`] tree the theme renders. Keeping it store-free is what lets the cache-hit
//! read path stay store-free: composing chrome must never re-open the store.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use ferropress_core::error::Result;
use ferropress_core::query::Edge;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{Object, ObjectId, TypeName, Value};
use ferropress_core::{LinkTarget, MENU_ITEM_TYPE, MENU_LOCATION_TYPE, MENU_TYPE, sanitize_href};
use parking_lot::RwLock;
use serde::Serialize;

use crate::content_index::ContentIndex;
use crate::redirects::normalize_path;
use crate::taxonomies::TaxonomySet;

/// A defensive cap on how deep [`load_menus`] will descend when rebuilding a menu tree
/// from stored parent links. Guards against externally-corrupted data forming a loop; set
/// above [`MAX_NAV_DEPTH`] (nothing renders past the compose cap anyway) yet still bounded.
const MAX_LOAD_DEPTH: usize = 32;

/// The maximum nav nesting LEVELS [`MenuSet::compose`] will render (a top-level item is
/// level 1). A HARD cap, enforced at compose so the resolved tree can NEVER exceed it —
/// deeper descendants are dropped (only reachable via corrupt/legacy data, since the admin
/// write path caps saved depth at `MAX_NAV_DEPTH - 1` ancestors = this many levels).
///
/// It exists because the theme frames the nav through a RECURSIVE MiniJinja macro bounded by
/// the theme sandbox's `recursion_limit`; a tree deeper than that limit can render raises a
/// recursion error, and because the nav is live chrome on every page (no cache masks it) that
/// would 500 the whole public site. This cap sits comfortably below that limit (real nav menus
/// are 2-3 levels), so an admin-accepted menu always renders and corrupt data degrades to a
/// truncated menu, never a crash. The admin's `MAX_MENU_DEPTH` is defined in terms of this
/// constant, so the two bounds can never drift.
pub const MAX_NAV_DEPTH: usize = 6;

/// One node in a menu's structure tree: the item's label, its typed target (ids, not a
/// resolved href), whether it opens a new tab, and its ordered children. This is the
/// *stored* shape (what [`MenuHandle`] holds); [`MenuSet::compose`] turns it into the
/// resolved [`MenuItemCtx`] the theme renders.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuNode {
    pub label: String,
    pub target: LinkTarget,
    pub new_tab: bool,
    pub children: Vec<MenuNode>,
}

/// The full set of live menus grouped by theme location: `location → its item forest`.
/// Cheap to clone (reloaded wholesale on a menu change), and read purely in memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuSet {
    by_location: BTreeMap<String, Vec<MenuNode>>,
}

impl MenuSet {
    /// Build a set directly from `location → forest` pairs — for tests + the loader.
    pub fn from_locations(entries: impl IntoIterator<Item = (String, Vec<MenuNode>)>) -> Self {
        Self {
            by_location: entries.into_iter().collect(),
        }
    }

    /// The item forest bound to `location`, if any (empty menus and unbound locations are absent).
    pub fn location(&self, location: &str) -> Option<&[MenuNode]> {
        self.by_location.get(location).map(Vec::as_slice)
    }

    /// Whether any location has a bound menu.
    pub fn is_empty(&self) -> bool {
        self.by_location.is_empty()
    }

    /// Compose every bound location's forest into the resolved [`MenuItemCtx`] trees the
    /// theme renders — the ONE join of menu structure with the live [`ContentIndex`]. Pure
    /// and store-free (the cache-hit read path depends on it staying so).
    ///
    /// Per item (see [`compose_node`]): a `Post`/`Page`/`Term`/`Custom` target resolves to an
    /// href (published-gated for content, live-archive for a term, scheme-allow-listed for a
    /// custom URL). An unresolvable LEAF is dropped; an unresolvable PARENT survives as a
    /// label-only entry keeping its subtree. `current_path` (the normalized request path, or
    /// `None` for a preview) marks the active item via EXACT normalized-path equality.
    ///
    /// A location whose forest composes to nothing is OMITTED from the map, so the theme's
    /// `{% if menus.<location> %}` cleanly falls back to its default chrome.
    pub fn compose(
        &self,
        index: &ContentIndex,
        taxonomies: &TaxonomySet,
        current_path: Option<&str>,
    ) -> BTreeMap<String, Vec<MenuItemCtx>> {
        let mut out = BTreeMap::new();
        for (location, forest) in &self.by_location {
            let items: Vec<MenuItemCtx> = forest
                .iter()
                .filter_map(|node| compose_node(node, index, taxonomies, current_path, 1))
                .collect();
            if !items.is_empty() {
                out.insert(location.clone(), items);
            }
        }
        out
    }
}

/// A fully-resolved menu item, ready for the theme. `href` is `None` for a label-only
/// entry (an unresolvable parent kept only to carry its subtree); the theme renders such
/// an entry as a `<span>`, never `href="#"` or `href=""`. Fields are minimal + typed —
/// NEVER the raw item `meta` — and `label`/`href` are plain strings the template emits
/// WITHOUT `|safe` (autoescape + the [`sanitize_href`] allow-list are the two XSS layers).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MenuItemCtx {
    pub label: String,
    pub href: Option<String>,
    /// When true, the theme hardcodes `target="_blank" rel="noopener noreferrer"` — the
    /// item never supplies its own `rel`/`target`.
    pub new_tab: bool,
    /// `true` iff this item's href is the current page (EXACT normalized-path equality).
    pub aria_current: bool,
    pub children: Vec<MenuItemCtx>,
}

/// A target resolved to its live href + (for content) its live title.
struct ResolvedTarget {
    href: String,
    /// The target's current title — used as the label when the item's own label is blank.
    /// `None` for a `Custom` URL (whose blank-label fallback is the URL itself).
    live_title: Option<String>,
}

/// Resolve one [`LinkTarget`] to its live href, or `None` when it currently resolves to
/// nothing (an unpublished/absent Post/Page, a `Term` deleted since the item was saved, or a
/// `Custom` URL that fails the scheme allow-list — the defensive render-time half of the
/// two-layer XSS guard).
fn resolve_target(
    target: &LinkTarget,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
) -> Option<ResolvedTarget> {
    match target {
        LinkTarget::Post { id } => index.post(*id).map(|e| ResolvedTarget {
            href: e.href.clone(),
            live_title: Some(e.title.clone()),
        }),
        LinkTarget::Page { id } => index.page(*id).map(|e| ResolvedTarget {
            href: e.href.clone(),
            live_title: Some(e.title.clone()),
        }),
        // The term's own canonical archive href (the full ancestor chain) + its live name —
        // same id/name split as Post/Page, resolved from the live TaxonomySet rather than the
        // ContentIndex. A deleted term (or corrupt data) resolves to `None` exactly like a
        // deleted Post/Page: a leaf drops, a parent-with-children survives as a label-only span.
        LinkTarget::Term { id } => {
            let name = taxonomies.term(*id)?.name.clone();
            let href = taxonomies.archive_href(*id)?;
            Some(ResolvedTarget {
                href,
                live_title: Some(name),
            })
        }
        LinkTarget::Custom { url } => sanitize_href(url).map(|href| ResolvedTarget {
            href,
            live_title: None,
        }),
    }
}

/// Compose one stored [`MenuNode`] (and its subtree) into a [`MenuItemCtx`], or `None` when
/// it should be dropped. Children are composed first, so the parent's keep/drop decision can
/// see whether any survived:
///   * target resolves → a normal link; a blank label falls back to the target's live title
///     (content) or its URL (custom);
///   * target does not resolve, but a child survived → a label-only entry keeping the subtree;
///   * target does not resolve and no child survived → dropped (never an empty/`#` link).
fn compose_node(
    node: &MenuNode,
    index: &ContentIndex,
    taxonomies: &TaxonomySet,
    current_path: Option<&str>,
    depth: usize,
) -> Option<MenuItemCtx> {
    // Hard render-depth cap: never emit a tree deeper than the theme's recursive nav macro can
    // render (see [`MAX_NAV_DEPTH`]). At the cap, descendants are dropped rather than risk a
    // site-wide 500. Only reachable via corrupt/legacy data — the admin write path caps saved
    // depth to this bound — so a truncation warns loudly.
    let children: Vec<MenuItemCtx> = if depth >= MAX_NAV_DEPTH {
        if !node.children.is_empty() {
            tracing::warn!(
                depth,
                "nav menu nested past the render cap; truncating deeper items",
            );
        }
        Vec::new()
    } else {
        node.children
            .iter()
            .filter_map(|child| compose_node(child, index, taxonomies, current_path, depth + 1))
            .collect()
    };

    match resolve_target(&node.target, index, taxonomies) {
        Some(resolved) => {
            let label = if node.label.trim().is_empty() {
                resolved
                    .live_title
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| resolved.href.clone())
            } else {
                node.label.clone()
            };
            let aria_current = current_path
                .map(|cp| href_is_current(&resolved.href, cp))
                .unwrap_or(false);
            Some(MenuItemCtx {
                label,
                href: Some(resolved.href),
                new_tab: node.new_tab,
                aria_current,
                children,
            })
        }
        None => {
            if children.is_empty() {
                None
            } else {
                // Unresolvable parent: keep it as a label-only span so its resolvable
                // descendants aren't orphaned. A new tab is meaningless without an href.
                Some(MenuItemCtx {
                    label: node.label.clone(),
                    href: None,
                    new_tab: false,
                    aria_current: false,
                    children,
                })
            }
        }
    }
}

/// Whether `href` addresses the current page: EXACT equality of the two normalized paths
/// (never a prefix, so `/` isn't "current" on every page). The href's `?query`/`#fragment`
/// is stripped first; an external URL simply never equals the site-relative request path.
fn href_is_current(href: &str, current_path: &str) -> bool {
    let path_only = href.split(['#', '?']).next().unwrap_or(href);
    normalize_path(path_only) == normalize_path(current_path)
}

/// Read the whole menu set from the store: scan [`MenuLocation`](ferropress_core::MenuLocation)
/// for each `location → menu` binding, then rebuild each referenced menu's item forest. Used
/// for the boot seed AND the per-change reload — menus are low-volume, so a full rebuild on a
/// `Menu`/`MenuItem`/`MenuLocation` change is cheap and robust (the redirect-table discipline).
///
/// `MenuLocation.location` is `@unique`, so a location maps to one menu; a duplicate (only
/// possible from externally-corrupted data) resolves deterministically to the lowest menu id.
pub async fn load_menus(store: &Arc<dyn RhypeStore>) -> Result<MenuSet> {
    // Collect (location, menu_id) bindings.
    let mut assignments: Vec<(String, ObjectId)> = Vec::new();
    for row in store.scan(&TypeName::from(MENU_LOCATION_TYPE)).await? {
        let location = match row.get("location") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        let Some(menu_id) = single_link(store, MENU_LOCATION_TYPE, row.id, "menu").await? else {
            continue; // a location bound to no menu contributes nothing
        };
        assignments.push((location, menu_id));
    }
    // Deterministic dedupe: lowest menu id wins a (should-be-impossible) duplicate location.
    assignments.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));

    let mut forests: HashMap<u64, Vec<MenuNode>> = HashMap::new();
    let mut by_location: BTreeMap<String, Vec<MenuNode>> = BTreeMap::new();
    for (location, menu_id) in assignments {
        if by_location.contains_key(&location) {
            continue; // keep the first (lowest-menu-id) binding
        }
        let forest = match forests.get(&menu_id.0) {
            Some(forest) => forest.clone(),
            None => {
                let forest = load_menu_forest(store, menu_id).await?;
                forests.insert(menu_id.0, forest.clone());
                forest
            }
        };
        by_location.insert(location, forest);
    }
    Ok(MenuSet { by_location })
}

/// One flat menu item read from the store, before the tree is assembled.
struct ItemRow {
    id: u64,
    parent: Option<u64>,
    order: i32,
    label: String,
    target: LinkTarget,
    new_tab: bool,
}

/// Rebuild a single menu's item forest from the store: fetch its items (one batched
/// `get_many` + one batched parent `get_links_many`, no N+1), parse each target (a row
/// with an unparseable target is skipped + logged, never aborting the load), and assemble
/// the tree ordered by `(item_order, id)` per sibling group.
async fn load_menu_forest(store: &Arc<dyn RhypeStore>, menu_id: ObjectId) -> Result<Vec<MenuNode>> {
    let items_edge = Edge {
        type_name: TypeName::from(MENU_TYPE),
        id: menu_id,
        field: "items".to_owned(),
    };
    let item_ids: Vec<ObjectId> = store
        .get_links(&items_edge)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }

    let objs = store
        .get_many(&TypeName::from(MENU_ITEM_TYPE), &item_ids)
        .await?;
    let by_id: HashMap<u64, Object> = objs.into_iter().map(|o| (o.id.0, o)).collect();
    let parents = store
        .get_links_many(&TypeName::from(MENU_ITEM_TYPE), &item_ids, "parent")
        .await?;

    let mut rows: Vec<ItemRow> = Vec::with_capacity(item_ids.len());
    for (iid, parent_ids) in item_ids.iter().zip(parents) {
        let Some(obj) = by_id.get(&iid.0) else {
            continue; // a live inverse edge should always resolve; a gap is transient
        };
        let Some(target) = parse_target(obj) else {
            // Our writes are always valid tagged JSON, so an unparseable target is external
            // / forward-compat corruption. Skip + log (the render path can't surface it);
            // the admin loader FAILS loud on the same row so it is never silently re-saved.
            tracing::warn!(
                item = iid.0,
                "skipping a menu item with an unparseable target",
            );
            continue;
        };
        rows.push(ItemRow {
            id: iid.0,
            parent: parent_ids.into_iter().next().map(|p| p.0),
            order: i32_field(obj, "item_order"),
            label: str_field(obj, "label"),
            target,
            new_tab: new_tab_of(obj),
        });
    }

    Ok(assemble_tree(rows))
}

/// Assemble flat [`ItemRow`]s into a nested [`MenuNode`] forest, ordered by `(order, id)`
/// within each sibling group. A row whose `parent` is not itself a row in this menu is
/// promoted to a root (never dropped); a visited-set + depth cap make a corrupt parent
/// cycle terminate safely (unreachable cyclic nodes are dropped rather than looping).
fn assemble_tree(rows: Vec<ItemRow>) -> Vec<MenuNode> {
    let valid: HashSet<u64> = rows.iter().map(|r| r.id).collect();
    let mut children_of: HashMap<Option<u64>, Vec<ItemRow>> = HashMap::new();
    for row in rows {
        let key = match row.parent {
            Some(p) if valid.contains(&p) => Some(p),
            _ => None, // orphan / top-level → a root
        };
        children_of.entry(key).or_default().push(row);
    }
    for group in children_of.values_mut() {
        group.sort_by(|a, b| a.order.cmp(&b.order).then(a.id.cmp(&b.id)));
    }
    let mut visited: HashSet<u64> = HashSet::new();
    build_children(None, &children_of, &mut visited, 0)
}

fn build_children(
    parent: Option<u64>,
    children_of: &HashMap<Option<u64>, Vec<ItemRow>>,
    visited: &mut HashSet<u64>,
    depth: usize,
) -> Vec<MenuNode> {
    if depth > MAX_LOAD_DEPTH {
        return Vec::new();
    }
    let Some(group) = children_of.get(&parent) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(group.len());
    for row in group {
        if !visited.insert(row.id) {
            continue; // already placed → a cycle; skip to terminate
        }
        let children = build_children(Some(row.id), children_of, visited, depth + 1);
        out.push(MenuNode {
            label: row.label.clone(),
            target: row.target.clone(),
            new_tab: row.new_tab,
            children,
        });
    }
    out
}

/// Parse a menu item's `target` column back into a [`LinkTarget`] (`None` on a corrupt value).
fn parse_target(obj: &Object) -> Option<LinkTarget> {
    match obj.get("target") {
        Some(Value::String(raw)) => serde_json::from_str::<LinkTarget>(raw).ok(),
        _ => None,
    }
}

/// A menu item's `meta.new_tab` flag (false when absent).
fn new_tab_of(obj: &Object) -> bool {
    match obj.get("meta") {
        Some(Value::Json(j)) => j
            .get("new_tab")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

/// A `String` field read (empty when absent / not a string).
fn str_field(obj: &Object, field: &str) -> String {
    match obj.get(field) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// An `i32` field read (0 when absent / wrong type).
fn i32_field(obj: &Object, field: &str) -> i32 {
    match obj.get(field) {
        Some(Value::I32(n)) => *n,
        _ => 0,
    }
}

/// The single target of a to-one relation `field` on `(type_name, id)`, if linked.
async fn single_link(
    store: &Arc<dyn RhypeStore>,
    type_name: &str,
    id: ObjectId,
    field: &str,
) -> Result<Option<ObjectId>> {
    let edge = Edge {
        type_name: TypeName::from(type_name),
        id,
        field: field.to_owned(),
    };
    Ok(store
        .get_links(&edge)
        .await?
        .into_iter()
        .next()
        .map(|(target, _)| target))
}

/// A cheaply-cloneable handle to the current live [`MenuSet`], shared between the read path
/// (which frames every page's nav) and the regen loop (which reloads it on a menu change).
/// Same read/write discipline as [`RedirectHandle`](crate::redirects::RedirectHandle): reads
/// clone the inner `Arc` under a short read lock; a reload swaps the `Arc` under a short write
/// lock, so a render never sees a half-updated set.
#[derive(Clone)]
pub struct MenuHandle(Arc<RwLock<Arc<MenuSet>>>);

impl MenuHandle {
    /// Seed the handle with an initial set (built at startup from the store).
    pub fn new(initial: MenuSet) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current set snapshot (cheap `Arc` clone). Hold it for one render.
    pub fn current(&self) -> Arc<MenuSet> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current set (a full reload).
    pub fn set(&self, next: MenuSet) {
        *self.0.write() = Arc::new(next);
    }
}

impl Default for MenuHandle {
    /// An empty set — the state before the store is read (and what tests get without wiring
    /// menus; every location falls back to its theme default).
    fn default() -> Self {
        Self::new(MenuSet::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_index::ContentIndexHandle;
    use crate::taxonomies::{TaxonomyInfo, TermEntry};
    use ferropress_core::query::{Change, ChangeKind};
    use ferropress_core::value::ObjectId;
    use ferropress_core::{PAGE_TYPE, POST_TYPE};
    use serde_json::json;

    /// A published-content index seeded with a post at `/hello` and a page at `/about`.
    fn sample_index() -> Arc<ContentIndex> {
        let handle = ContentIndexHandle::default();
        for (ty, id, slug_field, key, title) in [
            (POST_TYPE, 1u64, "slug", "hello", "Hello Post"),
            (PAGE_TYPE, 1u64, "path", "about", "About Us"),
        ] {
            handle.apply_content_change(&Change {
                version: 1,
                kind: ChangeKind::Create,
                type_name: TypeName::from(ty),
                object_id: ObjectId(id),
                fields: Some(json!({ "status": "published", slug_field: key, "title": title })),
                origin: None,
            });
        }
        handle.current()
    }

    /// A taxonomy set with a root "Fiction" (id 5, `category`) and its child "Space Opera"
    /// (id 6) — enough to exercise a term's live name, its own root href, AND a child's href
    /// as the FULL ancestor chain (not just its own slug).
    fn sample_taxonomies() -> TaxonomySet {
        TaxonomySet::build(
            [TaxonomyInfo {
                id: 1,
                key: "category".to_owned(),
                label: "Category".to_owned(),
                hierarchical: true,
            }],
            [
                (
                    5,
                    TermEntry {
                        taxonomy_key: "category".to_owned(),
                        slug: "fiction".to_owned(),
                        name: "Fiction".to_owned(),
                        description: String::new(),
                        parent: None,
                    },
                ),
                (
                    6,
                    TermEntry {
                        taxonomy_key: "category".to_owned(),
                        slug: "space-opera".to_owned(),
                        name: "Space Opera".to_owned(),
                        description: String::new(),
                        parent: Some(5),
                    },
                ),
            ],
        )
    }

    fn leaf(label: &str, target: LinkTarget) -> MenuNode {
        MenuNode {
            label: label.to_owned(),
            target,
            new_tab: false,
            children: vec![],
        }
    }

    #[test]
    fn resolves_content_and_custom_targets() {
        let index = sample_index();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![
                leaf("Blog", LinkTarget::Post { id: 1 }),
                leaf("About", LinkTarget::Page { id: 1 }),
                leaf(
                    "Home",
                    LinkTarget::Custom {
                        url: "/".to_owned(),
                    },
                ),
            ],
        )]);
        let nav = set.compose(&index, &TaxonomySet::default(), Some("/hello"));
        let primary = &nav["primary"];
        assert_eq!(primary[0].href.as_deref(), Some("/hello"));
        assert_eq!(primary[1].href.as_deref(), Some("/about"));
        assert_eq!(primary[2].href.as_deref(), Some("/"));
        // aria_current is EXACT: the blog post is current, not the home link.
        assert!(primary[0].aria_current);
        assert!(
            !primary[2].aria_current,
            "home is not current on a post page"
        );
    }

    #[test]
    fn an_unresolvable_leaf_is_dropped_but_a_parent_survives_as_a_span() {
        let index = sample_index();
        // Term id 9 is absent from this (otherwise populated) taxonomy set — a deleted term
        // whose menu item wasn't cleaned up, exactly like a deleted Post/Page.
        let taxonomies = sample_taxonomies();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![
                // Unresolvable leaf (an unknown term id, or a deleted page) → dropped.
                leaf("Ghost", LinkTarget::Term { id: 9 }),
                // Unresolvable parent WITH a resolvable child → kept as a label-only span.
                MenuNode {
                    label: "Reads".to_owned(),
                    target: LinkTarget::Term { id: 9 },
                    new_tab: false,
                    children: vec![leaf("About", LinkTarget::Page { id: 1 })],
                },
            ],
        )]);
        let nav = set.compose(&index, &taxonomies, None);
        let primary = &nav["primary"];
        assert_eq!(primary.len(), 1, "the unresolvable leaf was dropped");
        assert_eq!(primary[0].label, "Reads");
        assert_eq!(primary[0].href, None, "the unresolvable parent is a span");
        assert_eq!(primary[0].children[0].href.as_deref(), Some("/about"));
    }

    #[test]
    fn a_blank_label_falls_back_to_the_live_title_then_the_url() {
        let index = sample_index();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![
                leaf("", LinkTarget::Post { id: 1 }), // blank → the post's live title
                leaf(
                    "",
                    LinkTarget::Custom {
                        url: "/free-reads".to_owned(),
                    },
                ), // blank custom → its URL
            ],
        )]);
        let nav = set.compose(&index, &TaxonomySet::default(), None);
        assert_eq!(nav["primary"][0].label, "Hello Post");
        assert_eq!(nav["primary"][1].label, "/free-reads");
    }

    #[test]
    fn a_javascript_custom_url_never_resolves() {
        let index = sample_index();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![leaf(
                "Evil",
                LinkTarget::Custom {
                    url: "javascript:alert(1)".to_owned(),
                },
            )],
        )]);
        // A lone unsafe leaf resolves to nothing → dropped → location omitted entirely.
        assert!(
            set.compose(&index, &TaxonomySet::default(), None)
                .is_empty()
        );
    }

    #[test]
    fn compose_caps_render_depth_to_prevent_a_recursion_500() {
        let index = sample_index();
        // A single resolvable chain FAR deeper than the admin permits (corrupt/legacy data). The
        // theme frames nav through a recursion-limited macro, so compose must hard-cap the
        // rendered nesting at MAX_NAV_DEPTH rather than emit a tree that 500s every page.
        let over = MAX_NAV_DEPTH + 4;
        let mut node = leaf(
            "deepest",
            LinkTarget::Custom {
                url: "/".to_owned(),
            },
        );
        for i in (0..over - 1).rev() {
            node = MenuNode {
                label: format!("n{i}"),
                target: LinkTarget::Custom {
                    url: "/".to_owned(),
                },
                new_tab: false,
                children: vec![node],
            };
        }
        let set = MenuSet::from_locations([("primary".to_owned(), vec![node])]);
        let nav = set.compose(&index, &TaxonomySet::default(), None);

        fn rendered_depth(items: &[MenuItemCtx]) -> usize {
            items
                .iter()
                .map(|i| 1 + rendered_depth(&i.children))
                .max()
                .unwrap_or(0)
        }
        assert_eq!(
            rendered_depth(&nav["primary"]),
            MAX_NAV_DEPTH,
            "compose must cap rendered nesting at MAX_NAV_DEPTH",
        );
    }

    #[test]
    fn an_empty_location_is_omitted_so_the_theme_falls_back() {
        let index = sample_index();
        // A location bound to an all-unresolvable forest composes to nothing. Term id 9 is
        // absent from this taxonomy set.
        let set = MenuSet::from_locations([(
            "footer".to_owned(),
            vec![leaf("Ghost", LinkTarget::Term { id: 9 })],
        )]);
        assert!(
            !set.compose(&index, &sample_taxonomies(), None)
                .contains_key("footer")
        );
    }

    /// The Term arm's live resolution: a blank label falls back to the term's live NAME (the
    /// same live_title fallback Post/Page already use), and a child term's href is the FULL
    /// ancestor chain, not just its own slug — proving `resolve_target` calls
    /// `TaxonomySet::archive_href`, not some shortcut that only handles roots.
    #[test]
    fn a_term_link_resolves_to_its_live_name_and_ancestor_chain_href() {
        let index = sample_index();
        let taxonomies = sample_taxonomies();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![leaf("", LinkTarget::Term { id: 6 })],
        )]);
        let nav = set.compose(&index, &taxonomies, None);
        let primary = &nav["primary"];
        assert_eq!(
            primary.len(),
            1,
            "a resolvable term link must not be dropped"
        );
        assert_eq!(
            primary[0].label, "Space Opera",
            "a blank label falls back to the term's live name"
        );
        assert_eq!(
            primary[0].href.as_deref(),
            Some("/category/fiction/space-opera"),
            "a child term's href must be the full ancestor chain, not just its own slug"
        );
    }

    /// An explicit menu-item label overrides the term's live name, identically to how it
    /// already overrides a Post/Page's live title.
    #[test]
    fn a_term_link_keeps_an_explicit_label_override() {
        let index = sample_index();
        let taxonomies = sample_taxonomies();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![leaf("Space Operas", LinkTarget::Term { id: 6 })],
        )]);
        let nav = set.compose(&index, &taxonomies, None);
        assert_eq!(
            nav["primary"][0].label, "Space Operas",
            "an explicit item label must override the term's live name"
        );
    }

    /// `aria_current` on a term link is EXACT normalized-path equality, not a prefix match —
    /// the site-wide discipline [`href_is_current`] enforces for every target kind.
    #[test]
    fn a_term_links_aria_current_is_an_exact_path_match() {
        let index = sample_index();
        let taxonomies = sample_taxonomies();
        let set = MenuSet::from_locations([(
            "primary".to_owned(),
            vec![leaf("Fiction", LinkTarget::Term { id: 5 })],
        )]);

        let on_archive = set.compose(&index, &taxonomies, Some("/category/fiction"));
        assert!(
            on_archive["primary"][0].aria_current,
            "the term's own archive path must mark it current"
        );

        let on_child_archive =
            set.compose(&index, &taxonomies, Some("/category/fiction/space-opera"));
        assert!(
            !on_child_archive["primary"][0].aria_current,
            "EXACT-match: a descendant archive path must NOT mark the parent term current"
        );
    }
}
