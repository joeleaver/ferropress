//! Page read/list/save/create for the admin editor — the hierarchical counterpart to
//! [`posts`](super::posts). A Page is served at a NESTED permalink built from its ancestor
//! slugs + its own slug, so this handler owns the materialized-`path` bookkeeping the flat Post
//! handler does not need:
//!   * it validates + maintains a page's `parent` (a self-relation), rejecting a self-parent or
//!     a cycle;
//!   * on every create/save it computes the page's full `path` from the parent chain, and on a
//!     rename/re-parent it CASCADES the recomputed path to every descendant and records the
//!     old→new move (cache eviction + a 301 redirect) via [`content_ops`](super::content_ops);
//!   * all hierarchy-mutating writes are serialized under the shared `hierarchy_lock` so two
//!     concurrent re-parents can't race into a cycle or a duplicate path.
//!
//! Authorization, the publish gate, per-author scoping (404-not-403 masking), featured-image
//! reconciliation, and the status ladder are IDENTICAL to posts — the entity-agnostic pieces
//! are reused from [`posts`](super::posts); only the Page-shaped fields and the hierarchy logic
//! are new here. Pages carry no `post_type` and no taxonomy.

use std::collections::{HashSet, VecDeque};

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use uuid::Uuid;

use ferropress_core::block::BlockTree;
use ferropress_core::query::Edge;
use ferropress_core::role::Capability;
use ferropress_core::status::Status;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value, now_millis};
use ferropress_core::{CoreError, MEDIA_TYPE, PAGE_TYPE, media_url};
use ferropress_serve::hierarchy::MAX_PAGE_DEPTH;
use ferropress_serve::join_page_path;
use ferropress_serve::templates::page_templates;

use super::posts::{
    FeaturedMediaDto, effective_time, ensure_media_exists, initial_status, parse_status,
};
use super::{
    AdminError, AdminJson, AuthedUser, content_ops, i32_field, json_field, str_field,
    terms as term_ops,
};
use crate::AppState;

/// A runaway guard on the descendant cascade: the max number of subtree nodes a single
/// re-parent/rename will recompute. Far above any real page tree; a corrupt tree that somehow
/// exceeds it stops rather than spinning.
const MAX_SUBTREE_NODES: usize = 10_000;

/// One row in the editor's page list. Carries `parent` + `path` + `depth` so the client can
/// render the tree (indent by depth), and `menu_order` for sibling ordering.
#[derive(Serialize)]
pub struct PageSummary {
    pub id: u64,
    pub title: String,
    pub slug: String,
    pub path: String,
    pub status: String,
    pub updated_at: Option<i64>,
    pub menu_order: i32,
    pub parent: Option<u64>,
    /// Ancestor count (0 = top-level), derived from `path` for the tree indent.
    pub depth: u32,
    pub featured_media: Option<FeaturedMediaDto>,
}

/// One theme page-template option for the editor's Template `<select>`.
#[derive(Serialize)]
pub struct TemplateOption {
    pub value: String,
    pub label: String,
}

/// `GET /admin/api/templates` — the theme's page templates (value + label) for the editor's
/// Template `<select>`. Any editing role may read them (they are theme metadata, not content).
pub async fn templates(who: AuthedUser) -> Result<Json<Vec<TemplateOption>>, AdminError> {
    who.require(Capability::EditOwnContent)?;
    Ok(Json(
        page_templates()
            .iter()
            .map(|(value, label)| TemplateOption {
                value: (*value).to_owned(),
                label: (*label).to_owned(),
            })
            .collect(),
    ))
}

/// The full page the editor loads (body + hierarchy meta included).
#[derive(Serialize)]
pub struct PageDetail {
    pub id: u64,
    pub title: String,
    pub slug: String,
    pub path: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    pub updated_at: Option<i64>,
    pub menu_order: i32,
    pub parent: Option<u64>,
    pub template: Option<String>,
    pub featured_media: Option<FeaturedMediaDto>,
}

/// `GET /admin/api/pages` — the non-trashed pages this user may edit, in tree order. Editor+
/// sees every page; a Contributor/Author sees only the pages they authored (same scoping as
/// posts).
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<PageSummary>>, AdminError> {
    who.require(Capability::EditOwnContent)?;

    let objs: Vec<Object> = state
        .store
        .scan(&TypeName::from(PAGE_TYPE))
        .await?
        .into_iter()
        .filter(|o| str_field(o, "status").as_deref() != Some(Status::Trashed.as_str()))
        .collect();

    // Author-scope (Editor+ sees all; a lower role only their own) via ONE batched link read.
    let objs: Vec<Object> = if who.can_edit_others() {
        objs
    } else {
        let ids: Vec<ObjectId> = objs.iter().map(|o| o.id).collect();
        let authors = state
            .store
            .get_links_many(&TypeName::from(PAGE_TYPE), &ids, "author")
            .await?;
        objs.into_iter()
            .zip(authors)
            .filter(|(_, authors)| authors.contains(&who.id))
            .map(|(obj, _)| obj)
            .collect()
    };

    // Resolve parent + featured per row (small admin set, not a hot path).
    let mut pages = Vec::with_capacity(objs.len());
    for obj in &objs {
        let featured = resolve_featured(&state, obj.id).await?;
        let parent = parent_of(&state, obj.id).await?.map(|p| p.0);
        pages.push(summary(obj, parent, featured));
    }

    // Tree order: sort by the materialized `path` — a parent's path is a prefix of its children,
    // so children sort directly under their parent; `menu_order` then `title` break ties.
    pages.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.menu_order.cmp(&b.menu_order))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(Json(pages))
}

/// `GET /admin/api/pages/{id}` — one page with its body + hierarchy meta, for the editor.
pub async fn get_one(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<Json<PageDetail>, AdminError> {
    let obj = state
        .store
        .get(&TypeName::from(PAGE_TYPE), ObjectId(id))
        .await?;
    let author = author_of(&state, ObjectId(id)).await?;
    who.require_post_access(author)?;
    backfill_author(&state, ObjectId(id), &who, author).await;

    Ok(Json(PageDetail {
        id,
        title: str_field(&obj, "title").unwrap_or_default(),
        slug: str_field(&obj, "slug").unwrap_or_default(),
        path: str_field(&obj, "path").unwrap_or_default(),
        status: str_field(&obj, "status").unwrap_or_default(),
        block_tree: json_field(&obj, "block_tree").unwrap_or_else(|| {
            BlockTree::from_blocks(Vec::new())
                .to_json_value()
                .unwrap_or(serde_json::Value::Null)
        }),
        updated_at: effective_time(&obj),
        menu_order: i32_field(&obj, "menu_order"),
        parent: parent_of(&state, ObjectId(id)).await?.map(|p| p.0),
        template: template_value(&obj),
        featured_media: resolve_featured(&state, ObjectId(id)).await?,
    }))
}

/// The editor's save payload for a page.
#[derive(Deserialize)]
pub struct SaveRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    pub block_tree: serde_json::Value,
    #[serde(default)]
    pub featured_media: Option<u64>,
    /// The parent page id (hierarchy), or `null`/absent for a top-level page.
    #[serde(default)]
    pub parent: Option<u64>,
    /// The sibling ordering key (default 0).
    #[serde(default)]
    pub menu_order: i32,
    /// The chosen theme template value (e.g. `"page-wide"`), or `null`/`""` for the default.
    #[serde(default)]
    pub template: Option<String>,
}

#[derive(Serialize)]
pub struct SaveResponse {
    pub id: u64,
    pub updated_at: i64,
    /// The recomputed public path (may have moved if the slug/parent changed).
    pub path: String,
}

/// `PUT /admin/api/pages/{id}` — persist a page edit, maintaining its hierarchy. On a slug or
/// parent change the page's `path` moves; every descendant's path is recomputed and the old→new
/// move recorded (cache eviction + 301). Serialized under the shared hierarchy lock.
pub async fn save(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<SaveRequest>,
) -> Result<Json<SaveResponse>, AdminError> {
    let slug = content_ops::validate_slug(&body.slug)?;
    let new_status = parse_status(&body.status)
        .ok_or_else(|| AdminError::BadRequest(format!("unknown status {:?}", body.status)))?;
    let tree = BlockTree::from_json_value(body.block_tree.clone())
        .map_err(|e| AdminError::BadRequest(format!("invalid block tree: {e}")))?;
    let template = validate_template(body.template.as_deref())?;

    let current = state
        .store
        .get(&TypeName::from(PAGE_TYPE), ObjectId(id))
        .await?;
    let current_status =
        parse_status(&str_field(&current, "status").unwrap_or_default()).unwrap_or(Status::Draft);
    let author = author_of(&state, ObjectId(id)).await?;
    who.require_post_access(author)?;
    if current_status.is_publish_state() || new_status.is_publish_state() {
        who.require_publish(author)?;
    }
    if let Some(mid) = body.featured_media {
        ensure_media_exists(&state, mid).await?;
    }
    if !current_status.can_transition_to(new_status) {
        return Err(AdminError::BadRequest(format!(
            "cannot change status from {} to {}",
            current_status.as_str(),
            new_status.as_str()
        )));
    }

    // Capture the pre-write parent + a title clone (title moves into the patch) for the auto-add
    // hook: a page that BECOMES a published top-level page joins every auto-add menu.
    let old_parent = parent_of(&state, ObjectId(id)).await?;
    let title = body.title.clone();

    // Serialize hierarchy mutations from here: the cycle check + path-uniqueness pre-flight and
    // the writes must be atomic w.r.t. another concurrent re-parent.
    let hierarchy_guard = state.hierarchy_lock.lock().await;
    // Nested INSIDE the hierarchy lock — the ONE sanctioned nesting (see
    // `AppState::taxonomy_lock`): the term-archive collision check below and the
    // path writes must be atomic w.r.t. concurrent TERM writes, whose own
    // archive-path guard runs under this same lock. Without it, each side's check
    // can pass before the other side's row commits and a page and a term archive
    // land on one path (which the Inc-2 archive resolver would silently shadow).
    let taxonomy_guard = state.taxonomy_lock.lock().await;

    // Validate the parent (exists, is a Page, not self, no cycle) and compute the new path.
    let parent_path = resolve_parent_path(&state, Some(ObjectId(id)), body.parent).await?;
    let new_path = join_page_path(parent_path.as_deref(), &slug);
    let old_path = str_field(&current, "path").unwrap_or_default();
    // The ROOT segment must not be a reserved URL base (`page`/`feed` — pagination/
    // feed grammar). Checked on the whole computed path, so a re-parent to top level
    // can't smuggle a reserved slug in; descendants inherit an already-checked root.
    // Enforced only when the root segment actually CHANGES: a page grandfathered at
    // a reserved root (created before the reservation existed) — and every
    // descendant under it, whose recomputed path keeps that root — must stay
    // editable in place; only moving INTO the reserved namespace is refused.
    if new_path.split('/').next() != old_path.split('/').next() {
        content_ops::ensure_unreserved_root(&new_path)?;
    }

    // If the path moved, recompute every descendant's new path from the (new) parent chain.
    let moved = new_path != old_path;
    let descendants = if moved {
        recompute_descendants(&state, ObjectId(id), &new_path).await?
    } else {
        Vec::new()
    };

    // Pre-flight cross-entity uniqueness for the page + EVERY descendant path, excluding the
    // moving subtree's own ids, BEFORE any write — a single collision aborts the whole move.
    let mut subtree: Vec<ObjectId> = vec![ObjectId(id)];
    subtree.extend(descendants.iter().map(|(cid, _, _)| *cid));
    if content_ops::is_taken(&state, &new_path, &subtree).await? {
        return Err(AdminError::Conflict(format!(
            "the path {new_path:?} is already in use"
        )));
    }
    for (_, _, dnew) in &descendants {
        if content_ops::is_taken(&state, dnew, &subtree).await? {
            return Err(AdminError::Conflict(format!(
                "the path {dnew:?} is already in use"
            )));
        }
    }

    // The REVERSE archive-path guard: term create/update/delete refuse a term path
    // landing on a live page — a page path landing on a live term archive must be
    // refused just the same, or the collision the 409s exist to prevent stays
    // reachable by moving the page second. Only MOVED paths are checked (an
    // unchanged path cannot newly collide, and re-checking it would brick edits of
    // a page in a legacy-corrupt collision state).
    if moved {
        let term_paths = term_ops::all_term_archive_paths(&state).await?;
        for path in std::iter::once(&new_path).chain(descendants.iter().map(|(_, _, d)| d)) {
            if term_paths.contains(path) {
                return Err(AdminError::Conflict(format!(
                    "the path {path:?} is already used as a term archive"
                )));
            }
        }
    }

    // Write the page's own fields, INCLUDING the new `path` scalar, FIRST — so once we evict the
    // old cache blob below, a concurrent read of the old path resolves to nothing (can't
    // repopulate it).
    let now = now_millis();
    let mut patch: FieldMap = FieldMap::new();
    patch.insert("title".to_owned(), Value::String(body.title));
    patch.insert("slug".to_owned(), Value::String(slug));
    patch.insert("path".to_owned(), Value::String(new_path.clone()));
    patch.insert(
        "status".to_owned(),
        Value::String(new_status.as_str().to_owned()),
    );
    patch.insert("block_tree".to_owned(), Value::Json(body.block_tree));
    patch.insert("plaintext".to_owned(), Value::String(tree.plaintext()));
    patch.insert("menu_order".to_owned(), Value::I32(body.menu_order));
    patch.insert("template".to_owned(), Value::String(template));
    patch.insert("updated_at".to_owned(), Value::DateTime(now));
    state
        .store
        .update(&TypeName::from(PAGE_TYPE), ObjectId(id), patch)
        .await?;

    // Reconcile the to-one `parent` relation to the request.
    set_parent(&state, ObjectId(id), body.parent).await?;

    // Update each descendant's `path` scalar ONLY (not `updated_at` — a rename must not reorder
    // the admin last-touched list for pages the author didn't edit). Each emits a change, so the
    // regen loop rebuilds every descendant at its new key.
    for (cid, _old, dnew) in &descendants {
        let mut p: FieldMap = FieldMap::new();
        p.insert("path".to_owned(), Value::String(dnew.clone()));
        state
            .store
            .update(&TypeName::from(PAGE_TYPE), *cid, p)
            .await?;
    }

    set_featured(&state, ObjectId(id), body.featured_media).await?;

    // Path scalars are now the NEW values → do the move bookkeeping (evict old blobs + 301s).
    content_ops::record_move(&state, &old_path, &new_path).await?;
    for (_, dold, dnew) in &descendants {
        content_ops::record_move(&state, dold, dnew).await?;
    }

    backfill_author(&state, ObjectId(id), &who, author).await;

    // Release both locks BEFORE the auto-add hook (it takes `menu_lock` itself). Fire when
    // this save leaves the page as a published TOP-LEVEL page AND it just crossed into that state —
    // either by becoming published (WordPress's transition trigger) or by re-parenting to top level.
    drop(taxonomy_guard);
    drop(hierarchy_guard);
    let now_published_top = new_status.is_publish_state() && body.parent.is_none();
    let became_published = !current_status.is_publish_state() && new_status.is_publish_state();
    let became_top_level = old_parent.is_some() && body.parent.is_none();
    if now_published_top && (became_published || became_top_level) {
        super::menus::auto_add_top_level_page(&state, id, &title).await;
    }

    Ok(Json(SaveResponse {
        id,
        updated_at: now,
        path: new_path,
    }))
}

/// The editor's "New page" payload.
#[derive(Deserialize)]
pub struct CreateRequest {
    pub title: String,
    pub slug: String,
    #[serde(default)]
    pub status: Option<String>,
    pub block_tree: serde_json::Value,
    #[serde(default)]
    pub featured_media: Option<u64>,
    #[serde(default)]
    pub parent: Option<u64>,
    #[serde(default)]
    pub menu_order: i32,
    #[serde(default)]
    pub template: Option<String>,
}

#[derive(Serialize)]
pub struct CreateResponse {
    pub id: u64,
    pub created_at: i64,
    pub path: String,
}

/// `POST /admin/api/pages` — create a new page (the editor's "New page" flow). Validates the
/// slug, status, template, and parent (existence + type); computes the page's `path` from the
/// parent chain; enforces cross-entity uniqueness; attributes the page to its creator. Serialized
/// under the hierarchy lock.
pub async fn create(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<CreateRequest>,
) -> Result<Json<CreateResponse>, AdminError> {
    who.require(Capability::EditOwnContent)?;

    let slug = content_ops::validate_slug(&body.slug)?;
    let status = initial_status(body.status.as_deref())?;
    if status.is_publish_state() {
        who.require_publish(Some(who.id))?;
    }
    let tree = BlockTree::from_json_value(body.block_tree.clone())
        .map_err(|e| AdminError::BadRequest(format!("invalid block tree: {e}")))?;
    let template = validate_template(body.template.as_deref())?;
    if let Some(mid) = body.featured_media {
        ensure_media_exists(&state, mid).await?;
    }

    // A top-level page published on creation joins every auto-add menu (below, once the hierarchy
    // lock is released). Capture the facts + a title clone before `body.title` is moved into fields.
    let published = status.is_publish_state();
    let is_top_level = body.parent.is_none();
    let title = body.title.clone();

    let hierarchy_guard = state.hierarchy_lock.lock().await;
    // Nested INSIDE the hierarchy lock — the one sanctioned nesting (see
    // `AppState::taxonomy_lock` and the identical block in `save`).
    let taxonomy_guard = state.taxonomy_lock.lock().await;

    // Validate the parent (no self on create; a fresh page has no id yet) and compute the path.
    let parent_path = resolve_parent_path(&state, None, body.parent).await?;
    let path = join_page_path(parent_path.as_deref(), &slug);
    // A ROOT page must not occupy a reserved URL base (`page`/`feed`).
    content_ops::ensure_unreserved_root(&path)?;
    if content_ops::is_taken(&state, &path, &[]).await? {
        return Err(AdminError::Conflict(format!(
            "the path {path:?} is already in use"
        )));
    }
    // The reverse archive-path guard (see `save`): a new page must not land on a
    // live term's archive path — the same collision the term side 409s in the
    // other creation order.
    if term_ops::all_term_archive_paths(&state)
        .await?
        .contains(&path)
    {
        return Err(AdminError::Conflict(format!(
            "the path {path:?} is already used as a term archive"
        )));
    }

    let now = now_millis();
    let mut fields: FieldMap = FieldMap::new();
    fields.insert("uuid".to_owned(), Value::String(Uuid::now_v7().to_string()));
    fields.insert("slug".to_owned(), Value::String(slug));
    fields.insert("path".to_owned(), Value::String(path.clone()));
    fields.insert("title".to_owned(), Value::String(body.title));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("block_tree".to_owned(), Value::Json(body.block_tree));
    fields.insert("plaintext".to_owned(), Value::String(tree.plaintext()));
    fields.insert("menu_order".to_owned(), Value::I32(body.menu_order));
    fields.insert("template".to_owned(), Value::String(template));
    fields.insert("created_at".to_owned(), Value::DateTime(now));
    fields.insert("updated_at".to_owned(), Value::DateTime(now));

    let id = state
        .store
        .create(&TypeName::from(PAGE_TYPE), fields)
        .await?;

    // Attribute to the creator (rollback the orphan on a link failure).
    if let Err(e) = state
        .store
        .link(&author_edge(id), who.id, FieldMap::new())
        .await
    {
        rollback(&state, id).await;
        return Err(e.into());
    }
    // Link the parent (rollback on failure).
    if let Some(pid) = body.parent
        && let Err(e) = state
            .store
            .link(&parent_edge(id), ObjectId(pid), FieldMap::new())
            .await
    {
        rollback(&state, id).await;
        return Err(e.into());
    }
    // Attach the featured image if chosen (rollback on failure).
    if let Some(mid) = body.featured_media
        && let Err(e) = state
            .store
            .link(&featured_edge(id), ObjectId(mid), FieldMap::new())
            .await
    {
        rollback(&state, id).await;
        return Err(e.into());
    }

    // Shadow-guard: a live page now occupies this path → drop any stale 301 FROM it.
    content_ops::retire_redirects_at(&state, &path).await?;

    // Release both locks BEFORE the auto-add hook — it takes `menu_lock` itself, never
    // nested under either. A newly-published top-level page joins every auto-add menu.
    drop(taxonomy_guard);
    drop(hierarchy_guard);
    if published && is_top_level {
        super::menus::auto_add_top_level_page(&state, id.0, &title).await;
    }

    Ok(Json(CreateResponse {
        id: id.0,
        created_at: now,
        path,
    }))
}

// ---- Page-only hierarchy helpers -------------------------------------------

/// Validate a requested `parent` and return the parent's materialized `path` (`None` for a
/// top-level page). Rejects a non-existent parent, a self-parent, and a re-parent that would
/// create a cycle (the page being saved must not be an ancestor of the proposed parent).
async fn resolve_parent_path(
    state: &AppState,
    self_id: Option<ObjectId>,
    parent_id: Option<u64>,
) -> Result<Option<String>, AdminError> {
    let Some(pid) = parent_id.map(ObjectId) else {
        return Ok(None);
    };
    if Some(pid) == self_id {
        return Err(AdminError::BadRequest(
            "a page cannot be its own parent".to_owned(),
        ));
    }
    // Existence + type: `get(Page, pid)` is NotFound when the id is not a Page.
    let parent = match state.store.get(&TypeName::from(PAGE_TYPE), pid).await {
        Ok(o) => o,
        Err(CoreError::NotFound { .. }) => {
            return Err(AdminError::BadRequest(format!(
                "parent page {} does not exist",
                pid.0
            )));
        }
        Err(e) => return Err(e.into()),
    };
    // Cycle: the page being saved must not already be an ancestor of the proposed parent.
    if let Some(sid) = self_id {
        let chain = ancestor_ids(state, pid).await?;
        if chain.contains(&sid) {
            return Err(AdminError::BadRequest(
                "re-parenting there would create a cycle".to_owned(),
            ));
        }
    }
    Ok(Some(str_field(&parent, "path").unwrap_or_default()))
}

/// The ancestor id chain of `start` (inclusive), following `parent` edges upward. Bounded by a
/// visited-set + [`MAX_PAGE_DEPTH`] so a pre-existing corrupt cycle can never spin the walk.
async fn ancestor_ids(state: &AppState, start: ObjectId) -> Result<Vec<ObjectId>, AdminError> {
    let mut chain = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut cur = Some(start);
    while let Some(c) = cur {
        if !seen.insert(c) || seen.len() > MAX_PAGE_DEPTH {
            break;
        }
        chain.push(c);
        cur = parent_of(state, c).await?;
    }
    Ok(chain)
}

/// Recompute every descendant's new path from the root's NEW path, breadth-first: a child's new
/// path is `parent_new_path + "/" + child_slug`. Returns `(id, old_path, new_path)` per
/// descendant (root excluded). Bounded (visited-set + [`MAX_SUBTREE_NODES`]).
async fn recompute_descendants(
    state: &AppState,
    root: ObjectId,
    root_new_path: &str,
) -> Result<Vec<(ObjectId, String, String)>, AdminError> {
    let mut out = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::from([root]);
    let mut queue: VecDeque<(ObjectId, String)> =
        VecDeque::from([(root, root_new_path.to_owned())]);
    while let Some((pid, p_new_path)) = queue.pop_front() {
        if out.len() > MAX_SUBTREE_NODES {
            break;
        }
        for child in children_of(state, pid).await? {
            if !seen.insert(child) {
                continue; // cycle guard
            }
            let obj = state.store.get(&TypeName::from(PAGE_TYPE), child).await?;
            let child_slug = str_field(&obj, "slug").unwrap_or_default();
            let old = str_field(&obj, "path").unwrap_or_default();
            let new = join_page_path(Some(&p_new_path), &child_slug);
            out.push((child, old, new.clone()));
            queue.push_back((child, new));
        }
    }
    Ok(out)
}

/// The direct children of `page_id` (via the `children` inverse edge).
async fn children_of(state: &AppState, page_id: ObjectId) -> Result<Vec<ObjectId>, AdminError> {
    let edge = Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id: page_id,
        field: "children".to_owned(),
    };
    Ok(state
        .store
        .get_links(&edge)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// Reconcile the to-one `parent` relation to `parent_id` (`None` clears it → top-level), via the
/// shared fail-safe [`reconcile_to_one`](super::reconcile_to_one) helper.
async fn set_parent(
    state: &AppState,
    page_id: ObjectId,
    parent_id: Option<u64>,
) -> Result<(), AdminError> {
    super::reconcile_to_one(&state.store, &parent_edge(page_id), parent_id.map(ObjectId)).await
}

/// The single `parent` link target of `page_id`, if any.
async fn parent_of(state: &AppState, page_id: ObjectId) -> Result<Option<ObjectId>, AdminError> {
    Ok(state
        .store
        .get_links(&parent_edge(page_id))
        .await?
        .into_iter()
        .next()
        .map(|(id, _)| id))
}

// ---- Page-bound edge + featured/author helpers (mirror posts, Page-typed) --

fn author_edge(id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id,
        field: "author".to_owned(),
    }
}

fn parent_edge(id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id,
        field: "parent".to_owned(),
    }
}

fn featured_edge(id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id,
        field: "featured_media".to_owned(),
    }
}

/// Read a page's author id (the single `author` link), or `None` when unattributed.
pub(super) async fn author_of(
    state: &AppState,
    page_id: ObjectId,
) -> Result<Option<ObjectId>, AdminError> {
    Ok(state
        .store
        .get_links(&author_edge(page_id))
        .await?
        .into_iter()
        .next()
        .map(|(id, _)| id))
}

/// Backfill-on-touch: attribute a legacy null-author page to `who` (an Editor+ toucher only).
/// Mirrors `posts::backfill_author` — best-effort, re-reads before writing to avoid stacking a
/// second author on the to-one relation.
async fn backfill_author(
    state: &AppState,
    page_id: ObjectId,
    who: &AuthedUser,
    current_author: Option<ObjectId>,
) {
    if current_author.is_some() || !who.can_edit_others() {
        return;
    }
    let edge = author_edge(page_id);
    match state.store.get_links(&edge).await {
        Ok(links) if !links.is_empty() => return,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, page_id = page_id.0, "page backfill precheck failed (skipping)");
            return;
        }
    }
    if let Err(e) = state.store.link(&edge, who.id, FieldMap::new()).await {
        tracing::warn!(error = %e, page_id = page_id.0, "failed to backfill null page author");
    }
}

/// Reconcile the to-one `featured_media` relation for a page (`None` detaches it), via the shared
/// fail-safe [`reconcile_to_one`](super::reconcile_to_one) helper.
async fn set_featured(
    state: &AppState,
    page_id: ObjectId,
    media_id: Option<u64>,
) -> Result<(), AdminError> {
    super::reconcile_to_one(
        &state.store,
        &featured_edge(page_id),
        media_id.map(ObjectId),
    )
    .await
}

/// Resolve a page's featured image to `{id, url}`, or `None`.
async fn resolve_featured(
    state: &AppState,
    page_id: ObjectId,
) -> Result<Option<FeaturedMediaDto>, AdminError> {
    let Some((media_id, _)) = state
        .store
        .get_links(&featured_edge(page_id))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    match state.store.get(&TypeName::from(MEDIA_TYPE), media_id).await {
        Ok(media) => Ok(Some(FeaturedMediaDto {
            id: media_id.0,
            url: media_url(&str_field(&media, "uuid").unwrap_or_default()),
        })),
        Err(CoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ---- small field + validation helpers --------------------------------------

/// Build a page list-row summary. `depth` is derived from the materialized `path` (the number of
/// `/` separators = ancestor count).
fn summary(obj: &Object, parent: Option<u64>, featured: Option<FeaturedMediaDto>) -> PageSummary {
    let path = str_field(obj, "path").unwrap_or_default();
    let depth = path.matches('/').count() as u32;
    PageSummary {
        id: obj.id.0,
        title: str_field(obj, "title").unwrap_or_default(),
        slug: str_field(obj, "slug").unwrap_or_default(),
        path,
        status: str_field(obj, "status").unwrap_or_default(),
        updated_at: effective_time(obj),
        menu_order: i32_field(obj, "menu_order"),
        parent,
        depth,
        featured_media: featured,
    }
}

/// A page's `template` value as `Option` — an empty stored string is the default (None).
fn template_value(obj: &Object) -> Option<String> {
    match str_field(obj, "template") {
        Some(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// Validate a submitted template value against the theme's registered page templates. The empty
/// value (default) is stored as `""`; any other value must be a known template, else 400.
fn validate_template(value: Option<&str>) -> Result<String, AdminError> {
    let v = value.unwrap_or("").trim();
    if v.is_empty() {
        return Ok(String::new());
    }
    if page_templates().iter().any(|(val, _)| *val == v) {
        Ok(v.to_owned())
    } else {
        Err(AdminError::BadRequest(format!("unknown template {v:?}")))
    }
}

/// Delete a just-created page after a follow-up link failed, to avoid leaving an orphan.
async fn rollback(state: &AppState, id: ObjectId) {
    if let Err(e) = state.store.delete(&TypeName::from(PAGE_TYPE), id).await {
        tracing::error!(error = %e, page_id = id.0, "failed to roll back a page after a link failure");
    }
}
