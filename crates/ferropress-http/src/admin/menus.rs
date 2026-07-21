//! Nav-menu admin CRUD — the WordPress *Appearance → Menus* surface, backend half.
//!
//! A [`Menu`](ferropress_core::Menu) is a named container for an ordered, nestable
//! tree of [`MenuItem`](ferropress_core::MenuItem)s. Each item points at a typed
//! [`LinkTarget`] (a Post / Page / taxonomy Term / a custom URL) serialized as JSON
//! into the item's `target` `String` column. Which theme *location* a menu appears
//! in is a SEPARATE assignment ([`MenuLocation`](ferropress_core::MenuLocation),
//! wired in a later increment) — a menu carries no location itself.
//!
//! Every endpoint is gated on [`Capability::ManageMenus`] (Editor+). The item tree
//! is edited by a single **whole-tree PUT** (`/menus/{id}/items`): the client sends
//! the FULL desired forest and the server reconciles it (delete-absent, upsert-
//! present, set parent + contiguous `item_order` in one topological pass) under the
//! [`menu_lock`](crate::AppState::menu_lock). The desired-state shape makes the PUT
//! idempotent and safely re-submittable after a partial failure — rhypedb has no
//! multi-object transaction, so the N per-item writes are individually committed.
//!
//! Two invariants are enforced BEFORE any write: the submitted forest is validated
//! in memory (unique client ids, every parent present, no cycle, depth ≤
//! [`MAX_MENU_DEPTH`], count ≤ [`MAX_MENU_ITEMS`]); and every `Custom` URL passes
//! [`sanitize_href`]'s scheme allow-list (the write-time half of the two-layer XSS
//! guard — render-time autoescape is the other).

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use ferropress_core::LinkTarget;
use ferropress_core::error::CoreError;
use ferropress_core::query::{Compare, Edge, FilterSpec};
use ferropress_core::role::Capability;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};
use ferropress_core::{MENU_ITEM_TYPE, MENU_TYPE};

use super::{AdminError, AdminJson, AuthedUser, i32_field, str_field};
use crate::AppState;

/// The deepest a menu may nest. Well above any real menu; a submission past it is
/// rejected rather than rendered as an unbounded dropdown.
const MAX_MENU_DEPTH: usize = 10;

/// A hard cap on items per menu — a runaway/abusive submission guard.
const MAX_MENU_ITEMS: usize = 500;

// ---- DTOs -------------------------------------------------------------------

/// One row in the menu list (id + name + slug + how many items it holds).
#[derive(Serialize)]
pub struct MenuSummary {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub item_count: usize,
}

/// The lightweight menu identity returned by create/update.
#[derive(Serialize)]
pub struct MenuRef {
    pub id: u64,
    pub slug: String,
    pub name: String,
}

/// A menu + its full item forest (flat, in `(item_order, id)` order — the client
/// rebuilds the nesting by grouping on `parent_client_id`).
#[derive(Serialize)]
pub struct MenuDetail {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub items: Vec<MenuItemNode>,
}

/// One menu item on the wire — used BOTH in the GET response and the whole-tree
/// PUT. `id` is `Some` for an existing item (absent = a brand-new one to create);
/// `client_id` is the payload-stable handle a child references via
/// `parent_client_id` (the server maps `client_id -> ObjectId` as it creates new
/// items, so a new child can resolve its new parent in one pass). `target` is the
/// serde-tagged `LinkTarget` (`{"kind":"page","id":5}` etc.).
#[derive(Serialize, Deserialize)]
pub struct MenuItemNode {
    #[serde(default)]
    pub id: Option<u64>,
    pub client_id: String,
    #[serde(default)]
    pub parent_client_id: Option<String>,
    pub label: String,
    pub target: LinkTarget,
    #[serde(default)]
    pub new_tab: bool,
}

/// `POST /menus` body.
#[derive(Deserialize)]
pub struct CreateMenuRequest {
    pub name: String,
    /// An explicit slug; when absent/empty a slug is derived from the name.
    #[serde(default)]
    pub slug: Option<String>,
}

/// `PUT /menus/{id}` body.
#[derive(Deserialize)]
pub struct UpdateMenuRequest {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
}

/// `PUT /menus/{id}/items` body — the FULL desired item forest.
#[derive(Deserialize)]
pub struct SaveItemsRequest {
    pub items: Vec<MenuItemNode>,
}

/// `PUT /menus/{id}/items` response.
#[derive(Serialize)]
pub struct SaveItemsResponse {
    pub id: u64,
    pub item_count: usize,
}

// ---- handlers ---------------------------------------------------------------

/// `GET /admin/api/menus` — every menu with its item count, name-sorted.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<MenuSummary>>, AdminError> {
    who.require(Capability::ManageMenus)?;

    let menus = state.store.scan(&TypeName::from(MENU_TYPE)).await?;
    // One batched traversal for every menu's item count (not a get_links per menu).
    let ids: Vec<ObjectId> = menus.iter().map(|m| m.id).collect();
    let items_per = state
        .store
        .get_links_many(&TypeName::from(MENU_TYPE), &ids, "items")
        .await?;
    let mut out: Vec<MenuSummary> = menus
        .iter()
        .zip(items_per)
        .map(|(m, item_ids)| MenuSummary {
            id: m.id.0,
            slug: str_field(m, "slug").unwrap_or_default(),
            name: str_field(m, "name").unwrap_or_default(),
            item_count: item_ids.len(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(Json(out))
}

/// `GET /admin/api/menus/{id}` — one menu + its item forest, ready to edit.
pub async fn get_one(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<Json<MenuDetail>, AdminError> {
    who.require(Capability::ManageMenus)?;

    let menu = state
        .store
        .get(&TypeName::from(MENU_TYPE), ObjectId(id))
        .await?;

    // Batch the item objects + their parent links (one get_many + one
    // get_links_many, not an N+1 per item).
    let item_ids = menu_item_ids(&state, ObjectId(id)).await?;
    let objs = state
        .store
        .get_many(&TypeName::from(MENU_ITEM_TYPE), &item_ids)
        .await?;
    let by_id: HashMap<u64, Object> = objs.into_iter().map(|o| (o.id.0, o)).collect();
    let parents = state
        .store
        .get_links_many(&TypeName::from(MENU_ITEM_TYPE), &item_ids, "parent")
        .await?;

    // Sort by (item_order, id) so siblings-under-a-parent come out in display order
    // once the client groups on parent_client_id.
    let mut rows: Vec<(i32, u64, MenuItemNode)> = Vec::with_capacity(item_ids.len());
    for (iid, parent_ids) in item_ids.iter().zip(parents) {
        let Some(obj) = by_id.get(&iid.0) else {
            // get_many drops a missing id; an id from the live `items` inverse edge
            // should always resolve, so a gap is a transient store inconsistency.
            continue;
        };
        let Some(target) = target_from_field(obj) else {
            // Our writes are always valid JSON, so an unparseable target is external
            // / forward-compat corruption. FAIL LOUDLY — a silent skip here would let
            // the next whole-tree save (which omits what it never saw) delete the
            // item for good.
            return Err(internal(&format!(
                "menu item {} has an unparseable target; refusing to load the menu",
                iid.0
            )));
        };
        rows.push((
            i32_field(obj, "item_order"),
            iid.0,
            MenuItemNode {
                id: Some(iid.0),
                client_id: iid.0.to_string(),
                parent_client_id: parent_ids.into_iter().next().map(|p| p.0.to_string()),
                label: str_field(obj, "label").unwrap_or_default(),
                target,
                new_tab: item_new_tab(obj),
            },
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    Ok(Json(MenuDetail {
        id,
        slug: str_field(&menu, "slug").unwrap_or_default(),
        name: str_field(&menu, "name").unwrap_or_default(),
        items: rows.into_iter().map(|(_, _, n)| n).collect(),
    }))
}

/// `POST /admin/api/menus` — create a menu (empty; items are added via the item PUT).
pub async fn create(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<CreateMenuRequest>,
) -> Result<Json<MenuRef>, AdminError> {
    who.require(Capability::ManageMenus)?;

    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::BadRequest("a menu needs a name".to_owned()));
    }
    let slug = resolve_slug(body.slug.as_deref(), &name)?;

    // Serialize the uniqueness pre-check + create so two creates can't both pass it.
    let _guard = state.menu_lock.lock().await;
    if menu_slug_taken(&state, &slug, None).await? {
        return Err(AdminError::Conflict(format!(
            "a menu with the slug {slug:?} already exists"
        )));
    }

    let mut fields: FieldMap = FieldMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.clone()));
    fields.insert("name".to_owned(), Value::String(name.clone()));
    fields.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let id = state
        .store
        .create(&TypeName::from(MENU_TYPE), fields)
        .await?;

    Ok(Json(MenuRef {
        id: id.0,
        slug,
        name,
    }))
}

/// `PUT /admin/api/menus/{id}` — rename a menu (and optionally re-slug it).
pub async fn update(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<UpdateMenuRequest>,
) -> Result<Json<MenuRef>, AdminError> {
    who.require(Capability::ManageMenus)?;

    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::BadRequest("a menu needs a name".to_owned()));
    }

    let _guard = state.menu_lock.lock().await;
    let current = state
        .store
        .get(&TypeName::from(MENU_TYPE), ObjectId(id))
        .await?;
    let slug = match body
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => super::content_ops::validate_slug(s)?,
        None => str_field(&current, "slug").unwrap_or_default(),
    };
    if menu_slug_taken(&state, &slug, Some(ObjectId(id))).await? {
        return Err(AdminError::Conflict(format!(
            "a menu with the slug {slug:?} already exists"
        )));
    }

    let mut patch: FieldMap = FieldMap::new();
    patch.insert("slug".to_owned(), Value::String(slug.clone()));
    patch.insert("name".to_owned(), Value::String(name.clone()));
    state
        .store
        .update(&TypeName::from(MENU_TYPE), ObjectId(id), patch)
        .await?;

    Ok(Json(MenuRef { id, slug, name }))
}

/// `DELETE /admin/api/menus/{id}` — delete a menu; the SDL cascade removes its items
/// (and their descendants) and any [`MenuLocation`](ferropress_core::MenuLocation)
/// assignment pointing at it.
pub async fn delete(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<StatusCode, AdminError> {
    who.require(Capability::ManageMenus)?;
    // Under menu_lock so the SDL cascade (menu → its items) can't race a concurrent
    // whole-tree save into orphaned MenuItems — save_items holds the same lock and
    // re-checks the menu's existence under it.
    let _guard = state.menu_lock.lock().await;
    // 404 (not 204) for a menu that never existed.
    state
        .store
        .get(&TypeName::from(MENU_TYPE), ObjectId(id))
        .await?;
    state
        .store
        .delete(&TypeName::from(MENU_TYPE), ObjectId(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /admin/api/menus/{id}/items` — reconcile the menu's item tree to the FULL
/// desired forest in the body. See the module docs for the reconcile contract.
pub async fn save_items(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<SaveItemsRequest>,
) -> Result<Json<SaveItemsResponse>, AdminError> {
    who.require(Capability::ManageMenus)?;

    // Validate the whole forest in memory BEFORE any lock/write (all-or-nothing intent).
    validate_forest(&body.items)?;

    let _guard = state.menu_lock.lock().await;

    // Existence check UNDER the lock (404 otherwise), serialized against delete() so
    // this can't link items into a menu being concurrently removed.
    state
        .store
        .get(&TypeName::from(MENU_TYPE), ObjectId(id))
        .await?;

    // The menu's current items — the deletion baseline + the ownership allow-list.
    let existing = menu_item_ids(&state, ObjectId(id)).await?;
    let existing_set: HashSet<u64> = existing.iter().map(|o| o.0).collect();

    // Every node that claims an id must claim one of THIS menu's items (no adopting
    // another menu's item, no resurrecting a stale id), and no id twice.
    let mut claimed: HashSet<u64> = HashSet::new();
    for n in &body.items {
        if let Some(nid) = n.id {
            if !existing_set.contains(&nid) {
                return Err(AdminError::BadRequest(format!(
                    "menu item {nid} does not belong to this menu"
                )));
            }
            if !claimed.insert(nid) {
                return Err(AdminError::BadRequest(format!(
                    "menu item {nid} is listed twice"
                )));
            }
        }
    }

    let ordered = topo_order(&body.items)?;
    let ordinals = sibling_ordinals(&body.items);

    // Reconcile order is UPSERT-then-DELETE, deliberately. rhypedb has no multi-object
    // transaction, so a fault partway through can't be rolled back; upserting first
    // means a partial failure leaves a SUPERSET of the desired items (extra rows a
    // re-submit of this idempotent desired-state PUT reconciles away) rather than a
    // subset (lost items). Deleting removed items only AFTER every kept child has been
    // repointed also means a removed parent's cascade can never take a surviving child.
    // Upsert every desired node parents-first, mapping client_id -> ObjectId so a new
    // child resolves its (possibly new) parent. Kept ids accumulate for the delete pass.
    let mut resolved: HashMap<&str, ObjectId> = HashMap::new();
    let mut kept: HashSet<u64> = HashSet::new();
    for n in &ordered {
        let parent_oid: Option<ObjectId> = match n.parent_client_id.as_deref() {
            Some(pc) => Some(*resolved.get(pc).ok_or_else(|| {
                internal("topological order violated: a child preceded its parent")
            })?),
            None => None,
        };
        let order = ordinals.get(n.client_id.as_str()).copied().unwrap_or(0);
        let target_val = target_to_value(&n.target)?;
        let meta_val = Value::Json(serde_json::json!({ "new_tab": n.new_tab }));

        let oid = match n.id {
            Some(nid) => {
                let mut patch: FieldMap = FieldMap::new();
                patch.insert("label".to_owned(), Value::String(n.label.clone()));
                patch.insert("item_order".to_owned(), Value::I32(order));
                patch.insert("target".to_owned(), target_val);
                patch.insert("meta".to_owned(), meta_val);
                state
                    .store
                    .update(&TypeName::from(MENU_ITEM_TYPE), ObjectId(nid), patch)
                    .await?;
                super::reconcile_to_one(&state.store, &item_parent_edge(ObjectId(nid)), parent_oid)
                    .await?;
                kept.insert(nid);
                ObjectId(nid)
            }
            None => {
                let mut fields: FieldMap = FieldMap::new();
                fields.insert("label".to_owned(), Value::String(n.label.clone()));
                fields.insert("item_order".to_owned(), Value::I32(order));
                fields.insert("target".to_owned(), target_val);
                fields.insert("meta".to_owned(), meta_val);
                let new_id = state
                    .store
                    .create(&TypeName::from(MENU_ITEM_TYPE), fields)
                    .await?;
                // Link the menu; on failure delete the orphan we just made.
                if let Err(e) = state
                    .store
                    .link(&menu_edge(new_id), ObjectId(id), FieldMap::new())
                    .await
                {
                    let _ = state
                        .store
                        .delete(&TypeName::from(MENU_ITEM_TYPE), new_id)
                        .await;
                    return Err(e.into());
                }
                if let Some(p) = parent_oid
                    && let Err(e) = state
                        .store
                        .link(&item_parent_edge(new_id), p, FieldMap::new())
                        .await
                {
                    let _ = state
                        .store
                        .delete(&TypeName::from(MENU_ITEM_TYPE), new_id)
                        .await;
                    return Err(e.into());
                }
                new_id
            }
        };
        resolved.insert(n.client_id.as_str(), oid);
    }

    // Delete every existing item the desired forest dropped. A cascade (deleting a
    // removed parent) may have already taken some, so a NotFound is tolerated.
    for old in existing {
        if !kept.contains(&old.0) {
            match state
                .store
                .delete(&TypeName::from(MENU_ITEM_TYPE), old)
                .await
            {
                Ok(()) => {}
                Err(CoreError::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    Ok(Json(SaveItemsResponse {
        id,
        item_count: resolved.len(),
    }))
}

// ---- forest validation (pure, pre-write) ------------------------------------

/// Validate the submitted forest in memory: unique non-empty client ids, every
/// `parent_client_id` present in the payload, no cycle, depth ≤ [`MAX_MENU_DEPTH`],
/// count ≤ [`MAX_MENU_ITEMS`], and every `Custom` URL allow-listed. A pure check so
/// the whole PUT aborts before touching the store.
fn validate_forest(items: &[MenuItemNode]) -> Result<(), AdminError> {
    if items.len() > MAX_MENU_ITEMS {
        return Err(AdminError::BadRequest(format!(
            "a menu may hold at most {MAX_MENU_ITEMS} items"
        )));
    }

    let mut index: HashMap<&str, &MenuItemNode> = HashMap::with_capacity(items.len());
    for n in items {
        if n.client_id.trim().is_empty() {
            return Err(AdminError::BadRequest(
                "a menu item is missing its client_id".to_owned(),
            ));
        }
        if index.insert(n.client_id.as_str(), n).is_some() {
            return Err(AdminError::BadRequest(format!(
                "duplicate menu-item client_id {:?}",
                n.client_id
            )));
        }
    }

    // Walk each node's ancestor chain: every parent must exist, no cycle, bounded depth.
    for n in items {
        let mut depth = 0usize;
        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(n.client_id.as_str());
        let mut cur = n.parent_client_id.as_deref();
        while let Some(pc) = cur {
            if !seen.insert(pc) {
                return Err(AdminError::BadRequest(
                    "menu items form a parent cycle".to_owned(),
                ));
            }
            let parent = index.get(pc).ok_or_else(|| {
                AdminError::BadRequest(format!(
                    "menu item {:?} references a missing parent {:?}",
                    n.client_id, pc
                ))
            })?;
            depth += 1;
            if depth > MAX_MENU_DEPTH {
                return Err(AdminError::BadRequest(format!(
                    "menu nesting exceeds the maximum depth of {MAX_MENU_DEPTH}"
                )));
            }
            cur = parent.parent_client_id.as_deref();
        }
    }

    // Custom URLs must pass the scheme allow-list (reject before any write).
    for n in items {
        if let LinkTarget::Custom { url } = &n.target {
            sanitize_href(url)?;
        }
    }

    Ok(())
}

/// Order the flat forest parents-before-children (a stable topological sort that
/// preserves payload order within each ready wave). The forest is pre-validated
/// (acyclic, all parents present), so this always terminates; the no-progress guard
/// is defensive only.
fn topo_order(items: &[MenuItemNode]) -> Result<Vec<&MenuItemNode>, AdminError> {
    let mut out: Vec<&MenuItemNode> = Vec::with_capacity(items.len());
    let mut placed: HashSet<&str> = HashSet::new();
    let mut remaining: Vec<&MenuItemNode> = items.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        remaining.retain(|n| {
            let ready = match n.parent_client_id.as_deref() {
                None => true,
                Some(pc) => placed.contains(pc),
            };
            if ready {
                out.push(*n);
                placed.insert(n.client_id.as_str());
            }
            !ready
        });
        if remaining.len() == before {
            return Err(internal(
                "menu forest could not be ordered (unexpected cycle)",
            ));
        }
    }
    Ok(out)
}

/// Assign each node its ordinal within its `parent_client_id` sibling group, in
/// payload order (contiguous `0..n` per group).
fn sibling_ordinals(items: &[MenuItemNode]) -> HashMap<String, i32> {
    let mut counters: HashMap<Option<&str>, i32> = HashMap::new();
    let mut out: HashMap<String, i32> = HashMap::with_capacity(items.len());
    for n in items {
        let counter = counters.entry(n.parent_client_id.as_deref()).or_insert(0);
        out.insert(n.client_id.clone(), *counter);
        *counter += 1;
    }
    out
}

// ---- link + field helpers ---------------------------------------------------

fn menu_edge(item_id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(MENU_ITEM_TYPE),
        id: item_id,
        field: "menu".to_owned(),
    }
}

fn item_parent_edge(item_id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(MENU_ITEM_TYPE),
        id: item_id,
        field: "parent".to_owned(),
    }
}

/// The ids of every item belonging to `menu_id` (via the `items` inverse edge).
async fn menu_item_ids(state: &AppState, menu_id: ObjectId) -> Result<Vec<ObjectId>, AdminError> {
    let edge = Edge {
        type_name: TypeName::from(MENU_TYPE),
        id: menu_id,
        field: "items".to_owned(),
    };
    Ok(state
        .store
        .get_links(&edge)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// Whether any OTHER menu already holds `slug` (`exclude` is the caller's own id on
/// an update). `Menu.slug` is `@unique`, but the app pre-checks it — the house idiom
/// (see `content_ops::is_taken`) — so a collision is a clean 409, not an engine fault.
async fn menu_slug_taken(
    state: &AppState,
    slug: &str,
    exclude: Option<ObjectId>,
) -> Result<bool, AdminError> {
    let rows = state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(MENU_TYPE),
            field: "slug".to_owned(),
            op: Compare::Eq,
            value: Value::String(slug.to_owned()),
            limit: Some(4),
        })
        .await?;
    Ok(rows.into_iter().any(|o| Some(o.id) != exclude))
}

/// Read an item's `meta.new_tab` flag (false when absent).
fn item_new_tab(obj: &Object) -> bool {
    match obj.get("meta") {
        Some(Value::Json(j)) => j
            .get("new_tab")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

/// Serialize a [`LinkTarget`] into the `target` `String` column (tagged JSON). A
/// `Custom` URL is normalized to its sanitized/trimmed form FIRST, so the STORED
/// value is the clean one AND this is a second, independent scheme-guard even if a
/// caller reached here without [`validate_forest`].
fn target_to_value(target: &LinkTarget) -> Result<Value, AdminError> {
    let normalized = match target {
        LinkTarget::Custom { url } => LinkTarget::Custom {
            url: sanitize_href(url)?,
        },
        other => other.clone(),
    };
    serde_json::to_string(&normalized)
        .map(Value::String)
        .map_err(|e| internal(&format!("serializing menu target: {e}")))
}

/// Parse an item object's `target` column back into a [`LinkTarget`] (`None` on a
/// corrupt/absent value).
fn target_from_field(obj: &Object) -> Option<LinkTarget> {
    let raw = str_field(obj, "target")?;
    serde_json::from_str::<LinkTarget>(&raw).ok()
}

/// Resolve the slug for a new menu: an explicit slug (validated) or one derived from
/// the name. Errors if neither yields a usable single-segment slug.
fn resolve_slug(explicit: Option<&str>, name: &str) -> Result<String, AdminError> {
    match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => super::content_ops::validate_slug(s),
        None => super::slugify(name).ok_or_else(|| {
            AdminError::BadRequest(
                "could not derive a slug from the name; please provide a slug".to_owned(),
            )
        }),
    }
}

/// Validate a `Custom` link URL against a scheme allow-list, returning it trimmed.
/// Permits a site-relative path (`/…`), a bare `#fragment`/`?query`, a relative
/// reference (no scheme), and the `http`/`https`/`mailto`/`tel` schemes. Rejects a
/// protocol-relative `//host`, any control/whitespace char (blocks `java\tscript:`
/// obfuscation), and every other scheme (`javascript:`, `data:`, `vbscript:`,
/// `file:`, `blob:`, …). The write-time half of the two-layer XSS guard.
pub(crate) fn sanitize_href(raw: &str) -> Result<String, AdminError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(AdminError::BadRequest(
            "a custom link needs a URL".to_owned(),
        ));
    }
    let reject =
        || AdminError::BadRequest(format!("the URL {url:?} is not an allowed link target"));

    // No raw control/whitespace: an embedded tab/newline can smuggle a scheme past
    // the check below yet still be honored by a browser once stripped.
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(reject());
    }
    // Protocol-relative `//host` (and backslash variants some browsers treat alike)
    // inherit the page scheme — reject.
    if url.starts_with("//") || url.starts_with("/\\") || url.starts_with('\\') {
        return Err(reject());
    }
    match url_scheme(url) {
        // No scheme → a relative / site-relative / fragment reference: safe.
        None => Ok(url.to_owned()),
        Some(scheme) => match scheme.as_str() {
            "http" | "https" | "mailto" | "tel" => Ok(url.to_owned()),
            _ => Err(reject()),
        },
    }
}

/// The URL scheme (lowercased) iff `url` begins with a valid RFC-3986 scheme
/// (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`), else `None` (a relative ref).
fn url_scheme(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            return Some(url[..i].to_ascii_lowercase());
        }
        if !(b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.') {
            return None; // a non-scheme char before any ':' → no scheme
        }
    }
    None
}

/// Shorthand for a 500 from an internal invariant violation (never author-facing).
fn internal(msg: &str) -> AdminError {
    AdminError::Internal(CoreError::Store(msg.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(client_id: &str, parent: Option<&str>) -> MenuItemNode {
        MenuItemNode {
            id: None,
            client_id: client_id.to_owned(),
            parent_client_id: parent.map(str::to_owned),
            label: client_id.to_owned(),
            target: LinkTarget::Custom {
                url: "/".to_owned(),
            },
            new_tab: false,
        }
    }

    #[test]
    fn sanitize_href_allows_safe_targets() {
        for ok in [
            "/about",
            "/",
            "#top",
            "?q=1",
            "about/team",
            "http://example.com",
            "https://example.com/x?y=1#z",
            "HTTPS://EXAMPLE.COM",
            "mailto:a@b.com",
            "tel:+15551234",
        ] {
            assert!(sanitize_href(ok).is_ok(), "expected {ok:?} to be allowed");
        }
    }

    #[test]
    fn sanitize_href_rejects_dangerous_targets() {
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>",
            "vbscript:msgbox",
            "file:///etc/passwd",
            "blob:https://x",
            "//evil.example.com",
            "/\\evil.example.com",
            "\\\\evil",
            "  ",
            "",
        ] {
            assert!(
                sanitize_href(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_forest_accepts_a_bounded_tree() {
        let items = vec![node("a", None), node("b", Some("a")), node("c", Some("b"))];
        assert!(validate_forest(&items).is_ok());
    }

    #[test]
    fn validate_forest_rejects_dupes_cycles_and_missing_parents() {
        // duplicate client_id
        assert!(validate_forest(&[node("a", None), node("a", None)]).is_err());
        // missing parent
        assert!(validate_forest(&[node("a", Some("ghost"))]).is_err());
        // self-parent (a cycle of length 1)
        assert!(validate_forest(&[node("a", Some("a"))]).is_err());
        // two-node cycle
        assert!(validate_forest(&[node("a", Some("b")), node("b", Some("a"))]).is_err());
    }

    #[test]
    fn validate_forest_enforces_depth() {
        // A chain deeper than MAX_MENU_DEPTH is rejected.
        let mut items = vec![node("n0", None)];
        for i in 1..=MAX_MENU_DEPTH + 1 {
            items.push(node(&format!("n{i}"), Some(&format!("n{}", i - 1))));
        }
        assert!(validate_forest(&items).is_err());
    }

    #[test]
    fn validate_forest_rejects_a_bad_custom_url() {
        let mut bad = node("a", None);
        bad.target = LinkTarget::Custom {
            url: "javascript:alert(1)".to_owned(),
        };
        assert!(validate_forest(&[bad]).is_err());
    }

    #[test]
    fn topo_order_places_parents_first() {
        // Deliberately out of order in the payload.
        let items = vec![node("c", Some("b")), node("a", None), node("b", Some("a"))];
        let Ok(ordered) = topo_order(&items) else {
            panic!("an acyclic forest must order");
        };
        let seq: Vec<&str> = ordered.iter().map(|n| n.client_id.as_str()).collect();
        assert!(seq.iter().position(|&x| x == "a") < seq.iter().position(|&x| x == "b"));
        assert!(seq.iter().position(|&x| x == "b") < seq.iter().position(|&x| x == "c"));
    }

    #[test]
    fn sibling_ordinals_are_contiguous_per_group() {
        let items = vec![
            node("a", None),
            node("b", None),
            node("a1", Some("a")),
            node("a2", Some("a")),
        ];
        let ord = sibling_ordinals(&items);
        assert_eq!(ord["a"], 0);
        assert_eq!(ord["b"], 1);
        assert_eq!(ord["a1"], 0);
        assert_eq!(ord["a2"], 1);
    }

    #[test]
    fn target_json_round_trips_through_the_column() {
        for t in [
            LinkTarget::Page { id: 7 },
            LinkTarget::Post { id: 9 },
            LinkTarget::Term { id: 3 },
            LinkTarget::Custom {
                url: "/free-reads".to_owned(),
            },
        ] {
            let Ok(Value::String(s)) = target_to_value(&t) else {
                panic!("target must serialize to a String");
            };
            let mut fields = FieldMap::new();
            fields.insert("target".to_owned(), Value::String(s));
            let obj = Object {
                type_name: TypeName::from(MENU_ITEM_TYPE),
                id: ObjectId(1),
                fields,
            };
            assert_eq!(target_from_field(&obj), Some(t));
        }
    }
}
