//! Taxonomy + Term admin surface — the WP "Categories / Tags" management backend
//! (taxonomy slice, gap #3), plus the shared term-creation core the post editor's
//! inline tag creation rides (see [`posts`](super::posts)).
//!
//! ## Auth split (WP-faithful)
//! * READ endpoints ([`list_taxonomies`], [`list`]) gate on `EditOwnContent`
//!   (Contributor+): anyone who can edit content needs the vocabulary to pick from.
//! * Term WRITE endpoints ([`create`], [`update`], [`delete`]) gate on `ManageTerms`
//!   (Editor+, WP's `manage_categories`).
//! * The ONE sub-Editor creation route is the post save's inline tag creation
//!   ([`create_term_core`] via `posts::save`/`create`): tags-only (a NON-hierarchical
//!   taxonomy), gated by the post-edit gate, never by `ManageTerms`.
//! * Taxonomy rows themselves have NO create/delete endpoint in v1 — they are
//!   provisioned by the `ferropress-schema` migrate tool (deleting one cascades to
//!   every term in it and strips them from every post; that footgun stays unexposed).
//!
//! ## Integrity rules
//! * **Per-sibling slug uniqueness** (WP model): a term's slug is unique among the
//!   terms sharing its (taxonomy, parent). `Term.slug` is `@indexed`, NOT `@unique` —
//!   the engine enforces nothing — so every mutation runs an in-memory check over the
//!   taxonomy's term set UNDER [`AppState::taxonomy_lock`] (pre-check + write must be
//!   atomic or two concurrent creates both pass).
//! * **Hierarchy**: `parent` only in a `hierarchical` taxonomy, same-taxonomy only,
//!   cycle-free, and the whole chain (plus the moved subtree) stays ≤
//!   [`MAX_TERM_DEPTH`]. Guards run on an in-memory snapshot of the taxonomy's terms
//!   (the menus whole-forest-validation discipline), never piecemeal store walks.
//! * **Archive-path collision guard**: a term's computed public archive path
//!   (`{taxonomy_key}/{ancestor slugs…}/{slug}`) must not equal an existing Post slug
//!   or Page path (any status — a draft that later publishes would collide), or the
//!   term archive would silently shadow live content. Checked for every path that
//!   MOVES: on create, on a rename/re-parent (the term + every descendant), and on a
//!   delete (the re-homed children + their subtrees). An UNCHANGED path is never
//!   re-checked — a later-arising occupant must not brick name-only edits. The guard
//!   is BIDIRECTIONAL: the page handlers run the reverse check
//!   ([`all_term_archive_paths`]) under this same lock, so neither creation order
//!   can land a page and a term archive on one path.
//! * **Reserved slugs**: `page` (the pagination path token — `/…/page/2` must never
//!   be parseable as a term) and `feed` (future per-archive feeds) are forbidden term
//!   slugs at every level.
//! * **Eventless-link touch**: rhypedb `link`/`unlink` emit NO ChangeEvent, and a
//!   term's `taxonomy`/`parent` are links. A Term CREATE event would reach the (Inc-2)
//!   live `TaxonomyHandle` BEFORE its taxonomy link commits — leaving the handle
//!   permanently stale — so every mutation ends with [`touch_term`] (a `meta._rev`
//!   bump → one settling `Term` Update AFTER all link work), the `touch_menu` idiom.
//!
//! Deriveds: `Term.plaintext` (the `@vectorize` source) is `name + description`,
//! refreshed on every write. The stored `Term.count` column is intentionally DEAD —
//! never written, never read; counts are DERIVED live (published-filtered) so they
//! can't drift (the change feed can't reliably maintain a stored count: membership
//! edits are eventless links).

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use ferropress_core::query::{Compare, Edge, FilterSpec};
use ferropress_core::role::Capability;
use ferropress_core::status::Status;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};
use ferropress_core::{POST_TYPE, TAXONOMY_TYPE, TERM_TYPE};

use super::{AdminError, AdminJson, AuthedUser, content_ops, str_field};
use crate::AppState;

/// Maximum term-hierarchy depth (root = depth 0). Bounds the archive URL's segment
/// count, the ancestor-chain walks, and the (Inc-2) TaxonomyHandle's descendant
/// traversal. Real category trees are 2-3 levels; 10 is generous headroom.
pub(super) const MAX_TERM_DEPTH: usize = 10;

/// Slugs a term may never use, at ANY level. `page` because the archive URL grammar
/// strips a trailing `/page/{N}` as the pagination token — a term slugged `page`
/// would make `/category/page/2` ambiguous between "term `page`, page 2" and
/// "the bare base, page 2". `feed` is reserved for future per-archive feeds.
const RESERVED_TERM_SLUGS: &[&str] = &["page", "feed"];

// ---- DTOs -------------------------------------------------------------------

/// One taxonomy, for the admin's vocabulary pickers + the term-management screen.
#[derive(Serialize)]
pub struct TaxonomyDto {
    pub id: u64,
    /// Stable key (`"category"`, `"tag"`) — the archive URL base + the API handle.
    pub key: String,
    pub label: String,
    pub hierarchical: bool,
    pub multiple: bool,
}

/// One term row in the management list: flat-with-depth (the admin renders the
/// hierarchy by indenting `depth`, the menus/pages idiom), siblings name-sorted.
#[derive(Serialize)]
pub struct TermDto {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub description: String,
    /// The parent term's id (`None` = a root term).
    pub parent: Option<u64>,
    /// Depth in the tree (root = 0) — the indent the list renders.
    pub depth: usize,
    /// DIRECT assignments: how many PUBLISHED posts hold exactly this term (WP's
    /// admin count column semantics — the archive's rolled-up total is a different,
    /// serve-side number). Derived live, never the dead stored `count` column.
    pub count: usize,
}

/// The created/updated term echoed back to the client.
#[derive(Serialize)]
pub struct TermRef {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub parent: Option<u64>,
}

/// `GET /admin/api/terms` query: which taxonomy's terms to list.
#[derive(Deserialize)]
pub struct ListTermsQuery {
    /// The taxonomy KEY (`"category"`, `"tag"`).
    pub taxonomy: String,
}

/// `POST /admin/api/terms` body.
#[derive(Deserialize)]
pub struct CreateTermRequest {
    /// The taxonomy KEY the term belongs to.
    pub taxonomy: String,
    pub name: String,
    /// Explicit slug; when absent/empty one is derived from the name.
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Parent term id (hierarchical taxonomies only).
    #[serde(default)]
    pub parent: Option<u64>,
}

/// `PUT /admin/api/terms/{id}` body — the FULL desired state (a WP term-edit form):
/// `slug: None` keeps the current slug; `parent: None` means "a root term" (so
/// clearing a parent is expressible); `description` is the full desired text.
#[derive(Deserialize)]
pub struct UpdateTermRequest {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parent: Option<u64>,
}

// ---- handlers ---------------------------------------------------------------

/// `GET /admin/api/taxonomies` — every taxonomy, key-sorted. Readable by anyone who
/// can edit content (the editor's assignment panel needs the vocabulary).
pub async fn list_taxonomies(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<TaxonomyDto>>, AdminError> {
    who.require(Capability::EditOwnContent)?;

    let mut out: Vec<TaxonomyDto> = state
        .store
        .scan(&TypeName::from(TAXONOMY_TYPE))
        .await?
        .iter()
        .map(|o| TaxonomyDto {
            id: o.id.0,
            key: str_field(o, "key").unwrap_or_default(),
            label: str_field(o, "label").unwrap_or_default(),
            hierarchical: bool_field(o, "hierarchical"),
            multiple: bool_field(o, "multiple"),
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key).then(a.id.cmp(&b.id)));
    Ok(Json(out))
}

/// `GET /admin/api/terms?taxonomy={key}` — the taxonomy's terms as a flat-with-depth
/// tree (DFS, siblings case-folded-name-asc — Term has no ordinal column, so the
/// order is pinned here and mirrored by the serve-side TaxonomyHandle), each with its
/// live DIRECT published-post count. Readable by anyone who can edit content.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
    Query(q): Query<ListTermsQuery>,
) -> Result<Json<Vec<TermDto>>, AdminError> {
    who.require(Capability::EditOwnContent)?;

    let taxonomy = taxonomy_by_key(&state, &q.taxonomy).await?;
    let rows = load_term_rows(&state, taxonomy.id).await?;

    // Live DIRECT counts: one batched inverse traversal (Term.objects), then one
    // batched post read to apply the publish gate — a raw link count would count
    // drafts/trashed (over-counting AND leaking a draft's membership).
    let ids: Vec<ObjectId> = rows.keys().map(|id| ObjectId(*id)).collect();
    let links_per = state
        .store
        .get_links_many(&TypeName::from(TERM_TYPE), &ids, "objects")
        .await?;
    let mut post_ids: Vec<ObjectId> = links_per.iter().flatten().copied().collect();
    post_ids.sort_unstable();
    post_ids.dedup();
    let published: HashSet<ObjectId> = if post_ids.is_empty() {
        HashSet::new()
    } else {
        state
            .store
            .get_many(&TypeName::from(POST_TYPE), &post_ids)
            .await?
            .iter()
            .filter(|o| is_published(o))
            .map(|o| o.id)
            .collect()
    };
    let counts: HashMap<u64, usize> = ids
        .iter()
        .zip(&links_per)
        .map(|(id, links)| (id.0, links.iter().filter(|p| published.contains(p)).count()))
        .collect();

    let mut out = Vec::with_capacity(rows.len());
    for (id, depth) in flatten_tree(&rows) {
        let row = &rows[&id];
        out.push(TermDto {
            id,
            slug: row.slug.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            parent: row.parent,
            depth,
            count: counts.get(&id).copied().unwrap_or(0),
        });
    }
    Ok(Json(out))
}

/// `POST /admin/api/terms` — create a term (`ManageTerms`, Editor+).
pub async fn create(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<CreateTermRequest>,
) -> Result<Json<TermRef>, AdminError> {
    who.require(Capability::ManageTerms)?;

    let taxonomy = taxonomy_by_key(&state, &body.taxonomy).await?;

    // Serialize the uniqueness/guard pre-checks + the create so two concurrent
    // creates can't both pass (Term.slug is not engine-unique).
    let _guard = state.taxonomy_lock.lock().await;
    let rows = load_term_rows(&state, taxonomy.id).await?;
    let id = create_term_core(
        &state,
        &taxonomy,
        &rows,
        &body.name,
        body.slug.as_deref(),
        &body.description,
        body.parent,
    )
    .await?;

    Ok(Json(TermRef {
        id: id.0,
        slug: resolve_term_slug(body.slug.as_deref(), &body.name)?,
        name: body.name.trim().to_owned(),
        parent: body.parent,
    }))
}

/// `PUT /admin/api/terms/{id}` — update a term to the full desired state
/// (`ManageTerms`). A slug change or re-parent MOVES the term's archive path — and
/// every descendant's — so the collision guard re-checks the whole subtree.
pub async fn update(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<UpdateTermRequest>,
) -> Result<Json<TermRef>, AdminError> {
    who.require(Capability::ManageTerms)?;

    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return Err(AdminError::BadRequest("a term needs a name".to_owned()));
    }

    let _guard = state.taxonomy_lock.lock().await;
    let current = state
        .store
        .get(&TypeName::from(TERM_TYPE), ObjectId(id))
        .await?;
    let taxonomy_id = single_link(&state, TERM_TYPE, ObjectId(id), "taxonomy")
        .await?
        .ok_or_else(|| {
            // A term with no taxonomy link is corrupt (create links it, cascade
            // deletes it) — refuse to compound the damage through this path.
            AdminError::Internal(ferropress_core::CoreError::Store(format!(
                "term {id} has no taxonomy link"
            )))
        })?;
    let taxonomy = state
        .store
        .get(&TypeName::from(TAXONOMY_TYPE), taxonomy_id)
        .await?;
    let taxonomy_key = str_field(&taxonomy, "key").unwrap_or_default();
    let hierarchical = bool_field(&taxonomy, "hierarchical");

    let slug = match body
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => validate_term_slug(s)?,
        None => str_field(&current, "slug").unwrap_or_default(),
    };

    let rows = load_term_rows(&state, taxonomy_id).await?;
    let old_parent = rows.get(&id).and_then(|r| r.parent);
    let old_slug = str_field(&current, "slug").unwrap_or_default();
    // Whether this write moves the term in the path namespace at all. A name/
    // description-only edit keeps (slug, parent) — and therefore every archive
    // path — byte-identical, so the collision guards below are SKIPPED for it:
    // re-checking an UNCHANGED path would let an externally-arising occupant (or
    // pre-existing corrupt data) permanently brick ordinary edits of this term.
    let path_moved = slug != old_slug || body.parent != old_parent;

    // Parent guards (same rules as create, plus the self/descendant cycle check and
    // the moved-subtree depth budget).
    if let Some(parent) = body.parent {
        if !hierarchical {
            return Err(AdminError::BadRequest(format!(
                "taxonomy {taxonomy_key:?} is flat; its terms cannot have a parent"
            )));
        }
        if parent == id {
            return Err(AdminError::BadRequest(
                "a term cannot be its own parent".to_owned(),
            ));
        }
        if !rows.contains_key(&parent) {
            return Err(AdminError::BadRequest(format!(
                "parent term {parent} does not exist in taxonomy {taxonomy_key:?}"
            )));
        }
        if is_ancestor_or_self(&rows, id, parent) {
            return Err(AdminError::BadRequest(
                "cannot re-parent a term under its own descendant".to_owned(),
            ));
        }
        let new_depth = depth_of(&rows, parent) + 1;
        if new_depth + subtree_height(&rows, id) > MAX_TERM_DEPTH {
            return Err(AdminError::BadRequest(format!(
                "term hierarchy exceeds the maximum depth of {MAX_TERM_DEPTH}"
            )));
        }
    }

    if path_moved {
        // Per-sibling uniqueness under the DESIRED parent, excluding self.
        if sibling_slug_taken(&rows, body.parent, &slug, Some(id)) {
            return Err(AdminError::Conflict(format!(
                "a sibling term with the slug {slug:?} already exists"
            )));
        }

        // The term's archive path — and every descendant's — after this write.
        // Simulate the post-write tree and collision-check each moved path against
        // the permalink namespace (a Page at `category/news` must not be shadowed).
        let mut next = rows.clone();
        if let Some(row) = next.get_mut(&id) {
            row.slug = slug.clone();
            row.parent = body.parent;
        }
        let mut moved: Vec<u64> = vec![id];
        moved.extend(descendants_of(&next, id));
        for term in moved {
            let path = archive_path(&taxonomy_key, &next, term);
            if content_ops::is_taken(&state, &path, &[]).await? {
                return Err(AdminError::Conflict(format!(
                    "the archive path {path:?} is already used by a post or page"
                )));
            }
        }
    }

    // Relation work FIRST, the scalar update LAST — the posts.rs save discipline:
    // the parent re-link is eventless (rhypedb link/unlink emit nothing), so the
    // scalar Update below doubles as the settling event, fired only AFTER the link
    // work committed. A re-link fault here leaves the term entirely UNTOUCHED
    // (old parent, old slug) instead of the reverse order's unvalidated
    // (old-parent, new-slug) stranding.
    if body.parent != old_parent {
        super::reconcile_to_one(
            &state.store,
            &term_edge(ObjectId(id), "parent"),
            body.parent.map(ObjectId),
        )
        .await?;
    }

    let mut patch: FieldMap = FieldMap::new();
    patch.insert("slug".to_owned(), Value::String(slug.clone()));
    patch.insert("name".to_owned(), Value::String(name.clone()));
    patch.insert(
        "description".to_owned(),
        Value::String(body.description.clone()),
    );
    patch.insert(
        "plaintext".to_owned(),
        Value::String(term_plaintext(&name, &body.description)),
    );
    // The scalar write IS the settling event (one real Term Update after the link
    // work — no separate touch needed). If it faults after a successful re-link,
    // revert the link so the term is never stranded at the unvalidated
    // (new-parent, old-slug) combination; both link ops are eventless, so a
    // successful revert is a net zero the live handle never needs to see.
    if let Err(e) = state
        .store
        .update(&TypeName::from(TERM_TYPE), ObjectId(id), patch)
        .await
    {
        if body.parent != old_parent
            && let Err(revert) = super::reconcile_to_one(
                &state.store,
                &term_edge(ObjectId(id), "parent"),
                old_parent.map(ObjectId),
            )
            .await
        {
            tracing::error!(
                error = ?revert,
                term_id = id,
                "failed to revert the parent re-link after a scalar-update fault; \
                 the term is stranded at (new parent, old slug) until a retry"
            );
        }
        return Err(e.into());
    }

    Ok(Json(TermRef {
        id,
        slug,
        name,
        parent: body.parent,
    }))
}

/// `DELETE /admin/api/terms/{id}` — delete a term (`ManageTerms`). Child handling
/// is WP-faithful (`wp_delete_term`): the children are RE-PARENTED to the deleted
/// term's OWN parent first; for a root term that means becoming roots, which the
/// SDL default (`parent @on_delete(remove)` unlinks, never cascades — proven in
/// the store tests) already produces without any re-link. Every post's membership
/// link is dropped (`Post.terms @on_delete(remove)`). 404 for a term that never
/// existed.
///
/// Re-homing changes every child's (parent, slug) pair AND shortens its (and its
/// whole subtree's) archive path — the same moves `update` guards — so delete runs
/// the same checks over the simulated post-delete tree and 409s (naming the
/// offending child) rather than silently violating per-sibling uniqueness or
/// landing a promoted archive path on a live post/page. Delete must not be the one
/// mutation that can break the invariants every other mutation enforces.
pub async fn delete(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<StatusCode, AdminError> {
    who.require(Capability::ManageTerms)?;

    let _guard = state.taxonomy_lock.lock().await;
    state
        .store
        .get(&TypeName::from(TERM_TYPE), ObjectId(id))
        .await?;

    // A taxonomy-less term is corrupt data (create links one, cascade deletes it);
    // deleting it can only help — skip the guards and just drop it.
    let (rows, taxonomy_key) =
        match single_link(&state, TERM_TYPE, ObjectId(id), "taxonomy").await? {
            Some(tid) => {
                let taxonomy = state.store.get(&TypeName::from(TAXONOMY_TYPE), tid).await?;
                (
                    load_term_rows(&state, tid).await?,
                    str_field(&taxonomy, "key").unwrap_or_default(),
                )
            }
            None => (HashMap::new(), String::new()),
        };

    let new_parent = rows.get(&id).and_then(|r| r.parent);
    let children: Vec<u64> = rows
        .iter()
        .filter(|(_, row)| row.parent == Some(id))
        .map(|(cid, _)| *cid)
        .collect();

    if !children.is_empty() {
        // Simulate the post-delete tree: the term gone, its children re-homed.
        let mut next = rows.clone();
        next.remove(&id);
        for cid in &children {
            if let Some(row) = next.get_mut(cid) {
                row.parent = new_parent;
            }
        }
        // (a) Each re-homed child's slug must stay unique among its NEW siblings.
        // (The children were already unique among THEMSELVES as siblings of the
        // deleted term; only clashes with the new parent's existing children can
        // arise.) Depth can only shrink, so no depth re-check is needed.
        for cid in &children {
            let child_slug = next[cid].slug.clone();
            if sibling_slug_taken(&next, new_parent, &child_slug, Some(*cid)) {
                return Err(AdminError::Conflict(format!(
                    "deleting this term would move its child {child_slug:?} next to \
                     an existing sibling with the same slug — re-slug or re-parent \
                     the child first"
                )));
            }
        }
        // (b) Every re-homed subtree's archive path shortens by one segment — none
        // may land on a live post/page (the same shadow guard create/update run).
        let mut moved: Vec<u64> = children.clone();
        for cid in &children {
            moved.extend(descendants_of(&next, *cid));
        }
        for term in moved {
            let path = archive_path(&taxonomy_key, &next, term);
            if content_ops::is_taken(&state, &path, &[]).await? {
                return Err(AdminError::Conflict(format!(
                    "deleting this term would move a child's archive path to \
                     {path:?}, which is already used by a post or page — re-slug \
                     or re-parent the child first"
                )));
            }
        }
        // Re-home the children BEFORE the delete (only needed when the term has a
        // parent — a root term's children reach the same end state via the SDL
        // `@on_delete(remove)`). The re-links are eventless, but the Term Delete
        // event below fires AFTER them and settles the (Inc-2) handle reload.
        if new_parent.is_some() {
            for cid in &children {
                if let Err(e) = super::reconcile_to_one(
                    &state.store,
                    &term_edge(ObjectId(*cid), "parent"),
                    new_parent.map(ObjectId),
                )
                .await
                {
                    settle_children(&state, &children).await;
                    return Err(e);
                }
            }
        }
    }

    if let Err(e) = state
        .store
        .delete(&TypeName::from(TERM_TYPE), ObjectId(id))
        .await
    {
        // The delete would have been the settling event for the eventless re-links
        // above — without it they'd stay invisible to the (Inc-2) live handle.
        settle_children(&state, &children).await;
        return Err(e.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Best-effort settle of re-homed children after a delete-path fault: the parent
/// re-links are eventless, and the Term Delete that would have settled them never
/// fired — touch each child so the (Inc-2) live handle still reloads onto the
/// committed state. Failures are logged, never mask the original error.
async fn settle_children(state: &AppState, children: &[u64]) {
    for cid in children {
        if let Err(e) = touch_term(state, ObjectId(*cid)).await {
            tracing::error!(
                error = ?e,
                term_id = cid,
                "failed to settle a re-homed child term after a delete fault"
            );
        }
    }
}

// ---- the shared creation core (admin create + the post save's inline tags) ---

/// Create a term after running EVERY integrity guard. The single creation path —
/// the admin [`create`] handler AND the post save's inline tag creation both funnel
/// here, so the guards can never diverge. The caller MUST hold
/// [`AppState::taxonomy_lock`] and pass a fresh [`load_term_rows`] snapshot for the
/// target taxonomy (the uniqueness check reads it).
///
/// Steps: resolve + validate the slug (shape, reserved set), parent guards
/// (hierarchical-only, exists, depth), per-sibling uniqueness, archive-path
/// collision, then create → link `taxonomy` (orphan-rollback) → link `parent`
/// (orphan-rollback) → [`touch_term`] (the settling event AFTER the eventless links).
pub(super) async fn create_term_core(
    state: &AppState,
    taxonomy: &Object,
    rows: &HashMap<u64, TermRow>,
    name: &str,
    slug: Option<&str>,
    description: &str,
    parent: Option<u64>,
) -> Result<ObjectId, AdminError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AdminError::BadRequest("a term needs a name".to_owned()));
    }
    let taxonomy_key = str_field(taxonomy, "key").unwrap_or_default();
    let slug = resolve_term_slug(slug, name)?;

    if let Some(parent) = parent {
        if !bool_field(taxonomy, "hierarchical") {
            return Err(AdminError::BadRequest(format!(
                "taxonomy {taxonomy_key:?} is flat; its terms cannot have a parent"
            )));
        }
        if !rows.contains_key(&parent) {
            return Err(AdminError::BadRequest(format!(
                "parent term {parent} does not exist in taxonomy {taxonomy_key:?}"
            )));
        }
        // The new term is a leaf: its depth is parent's + 1.
        if depth_of(rows, parent) + 1 > MAX_TERM_DEPTH {
            return Err(AdminError::BadRequest(format!(
                "term hierarchy exceeds the maximum depth of {MAX_TERM_DEPTH}"
            )));
        }
    }

    if sibling_slug_taken(rows, parent, &slug, None) {
        return Err(AdminError::Conflict(format!(
            "a sibling term with the slug {slug:?} already exists"
        )));
    }

    // The new term's public archive path must not shadow existing content.
    let path = match parent {
        Some(p) => format!("{}/{slug}", archive_path(&taxonomy_key, rows, p)),
        None => format!("{taxonomy_key}/{slug}"),
    };
    if content_ops::is_taken(state, &path, &[]).await? {
        return Err(AdminError::Conflict(format!(
            "the archive path {path:?} is already used by a post or page"
        )));
    }

    let mut fields: FieldMap = FieldMap::new();
    fields.insert("slug".to_owned(), Value::String(slug));
    fields.insert("name".to_owned(), Value::String(name.to_owned()));
    fields.insert(
        "description".to_owned(),
        Value::String(description.to_owned()),
    );
    fields.insert(
        "plaintext".to_owned(),
        Value::String(term_plaintext(name, description)),
    );
    fields.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let id = state
        .store
        .create(&TypeName::from(TERM_TYPE), fields)
        .await?;

    // Link the owning taxonomy; a term without one is corrupt, so roll the orphan
    // back on failure (the posts.rs author-link discipline).
    if let Err(e) = state
        .store
        .link(&term_edge(id, "taxonomy"), taxonomy.id, FieldMap::new())
        .await
    {
        rollback_term(state, id).await;
        return Err(e.into());
    }
    if let Some(parent) = parent
        && let Err(e) = state
            .store
            .link(&term_edge(id, "parent"), ObjectId(parent), FieldMap::new())
            .await
    {
        rollback_term(state, id).await;
        return Err(e.into());
    }

    // Settle: the Create event races the eventless taxonomy/parent links above; one
    // trailing scalar Update guarantees the (Inc-2) handle reload sees them.
    touch_term(state, id).await?;

    Ok(id)
}

/// Best-effort delete of a just-created term whose link work failed (never leave a
/// taxonomy-less orphan).
async fn rollback_term(state: &AppState, id: ObjectId) {
    if let Err(e) = state.store.delete(&TypeName::from(TERM_TYPE), id).await {
        tracing::error!(
            error = %e,
            term_id = id.0,
            "failed to roll back orphan term after link failure"
        );
    }
}

/// Bump `Term.meta._rev` and persist it — one deliberate `Term` Update emitted AFTER
/// all (eventless) link work, so the serve-side reload always fires on a settled
/// state. The `touch_menu` idiom; must run under `taxonomy_lock`, last.
async fn touch_term(state: &AppState, id: ObjectId) -> Result<(), AdminError> {
    let obj = state.store.get(&TypeName::from(TERM_TYPE), id).await?;
    let mut meta = match obj.get("meta") {
        Some(Value::Json(j)) if j.is_object() => j.clone(),
        _ => serde_json::json!({}),
    };
    let rev = meta
        .get("_rev")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        + 1;
    meta.as_object_mut()
        .expect("meta is an object by construction above")
        .insert("_rev".to_owned(), serde_json::Value::from(rev));
    let mut patch = FieldMap::new();
    patch.insert("meta".to_owned(), Value::Json(meta));
    state
        .store
        .update(&TypeName::from(TERM_TYPE), id, patch)
        .await?;
    Ok(())
}

// ---- taxonomy/term lookups + the in-memory guard snapshot -------------------

/// Resolve a taxonomy by its `@unique` key → 404 when unknown.
pub(super) async fn taxonomy_by_key(state: &AppState, key: &str) -> Result<Object, AdminError> {
    state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(TAXONOMY_TYPE),
            field: "key".to_owned(),
            op: Compare::Eq,
            value: Value::String(key.trim().to_owned()),
            limit: Some(1),
        })
        .await?
        .into_iter()
        .next()
        .ok_or(AdminError::NotFound)
}

/// One term row in the in-memory guard snapshot.
#[derive(Debug, Clone)]
pub(super) struct TermRow {
    pub slug: String,
    pub name: String,
    pub description: String,
    /// Parent term id — only when the parent is a live member of the SAME taxonomy;
    /// a dangling/cross-taxonomy parent link is defensively treated as a root.
    pub parent: Option<u64>,
}

/// Load every term of `taxonomy_id` into memory: one Term scan + two batched link
/// reads (taxonomy membership, parent). ALL integrity guards (sibling uniqueness,
/// cycles, depth, archive paths) run over this snapshot — the menus
/// whole-forest-validation discipline; terms are low-volume, so the scan is cheap
/// and the guards can't diverge from each other mid-flight.
pub(super) async fn load_term_rows(
    state: &AppState,
    taxonomy_id: ObjectId,
) -> Result<HashMap<u64, TermRow>, AdminError> {
    let all = state.store.scan(&TypeName::from(TERM_TYPE)).await?;
    let ids: Vec<ObjectId> = all.iter().map(|o| o.id).collect();
    let taxonomies = state
        .store
        .get_links_many(&TypeName::from(TERM_TYPE), &ids, "taxonomy")
        .await?;

    let members: Vec<&Object> = all
        .iter()
        .zip(&taxonomies)
        .filter(|(_, tax)| tax.first() == Some(&taxonomy_id))
        .map(|(o, _)| o)
        .collect();
    let member_ids: Vec<ObjectId> = members.iter().map(|o| o.id).collect();
    let parents = state
        .store
        .get_links_many(&TypeName::from(TERM_TYPE), &member_ids, "parent")
        .await?;

    let member_set: HashSet<u64> = member_ids.iter().map(|id| id.0).collect();
    let mut rows = HashMap::with_capacity(members.len());
    for (obj, parent_links) in members.iter().zip(&parents) {
        let parent = parent_links
            .first()
            .map(|p| p.0)
            .filter(|p| member_set.contains(p));
        rows.insert(
            obj.id.0,
            TermRow {
                slug: str_field(obj, "slug").unwrap_or_default(),
                name: str_field(obj, "name").unwrap_or_default(),
                description: str_field(obj, "description").unwrap_or_default(),
                parent,
            },
        );
    }
    Ok(rows)
}

/// Every term's computed archive path, across ALL taxonomies — the REVERSE
/// direction of the archive-path collision guard. Term create/update/delete refuse
/// a term path landing on a live post/page ([`content_ops::is_taken`]); the page
/// handlers call THIS (under `taxonomy_lock`, nested inside `hierarchy_lock` — the
/// one sanctioned nesting, see [`AppState::taxonomy_lock`]) to refuse a page path
/// landing on a live term archive. One direction alone leaves the collision state
/// reachable by simply creating the page second. Posts need no such check: a post
/// slug is a single segment, an archive path always `{key}/…` (≥ 2 segments).
pub(super) async fn all_term_archive_paths(
    state: &AppState,
) -> Result<HashSet<String>, AdminError> {
    let mut out = HashSet::new();
    for tax in state.store.scan(&TypeName::from(TAXONOMY_TYPE)).await? {
        let key = str_field(&tax, "key").unwrap_or_default();
        if key.is_empty() {
            continue; // corrupt row — no key means no archive namespace
        }
        let rows = load_term_rows(state, tax.id).await?;
        for id in rows.keys() {
            out.insert(archive_path(&key, &rows, *id));
        }
    }
    Ok(out)
}

/// Whether another sibling (same `parent`) already holds `slug`, excluding `exclude`
/// (the caller's own id on an update).
fn sibling_slug_taken(
    rows: &HashMap<u64, TermRow>,
    parent: Option<u64>,
    slug: &str,
    exclude: Option<u64>,
) -> bool {
    rows.iter()
        .any(|(id, row)| Some(*id) != exclude && row.parent == parent && row.slug == slug)
}

/// `candidate` is `term` itself or one of its ancestors — the cycle predicate for a
/// re-parent ("may I put `term` under `candidate`?" must be NO when candidate sits
/// inside term's own subtree, i.e. term is an ancestor-or-self of candidate).
fn is_ancestor_or_self(rows: &HashMap<u64, TermRow>, term: u64, candidate: u64) -> bool {
    let mut cur = Some(candidate);
    let mut seen = HashSet::new();
    while let Some(id) = cur {
        if id == term {
            return true;
        }
        if !seen.insert(id) {
            return false; // pre-existing cycle in stored data → stop walking
        }
        cur = rows.get(&id).and_then(|r| r.parent);
    }
    false
}

/// A term's depth (root = 0), cycle-guarded (a corrupt stored cycle yields the walk
/// length rather than hanging).
fn depth_of(rows: &HashMap<u64, TermRow>, id: u64) -> usize {
    let mut depth = 0;
    let mut cur = rows.get(&id).and_then(|r| r.parent);
    let mut seen = HashSet::from([id]);
    while let Some(p) = cur {
        if !seen.insert(p) {
            break;
        }
        depth += 1;
        cur = rows.get(&p).and_then(|r| r.parent);
    }
    depth
}

/// Height of the subtree rooted at `id` (a leaf = 0): how much depth budget the
/// subtree consumes below its root when the root moves.
fn subtree_height(rows: &HashMap<u64, TermRow>, id: u64) -> usize {
    descendants_of(rows, id)
        .into_iter()
        .map(|d| depth_of(rows, d))
        .max()
        .map(|max| max.saturating_sub(depth_of(rows, id)))
        .unwrap_or(0)
}

/// Every descendant of `id` (children, grandchildren, …), cycle-guarded.
fn descendants_of(rows: &HashMap<u64, TermRow>, id: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut frontier = vec![id];
    let mut seen = HashSet::from([id]);
    while let Some(cur) = frontier.pop() {
        for (child, row) in rows {
            if row.parent == Some(cur) && seen.insert(*child) {
                out.push(*child);
                frontier.push(*child);
            }
        }
    }
    out
}

/// The term's public archive PATH KEY (no leading slash): the taxonomy key followed
/// by the root→term slug chain, e.g. `category/fiction/space-opera`. Cycle-guarded.
fn archive_path(taxonomy_key: &str, rows: &HashMap<u64, TermRow>, id: u64) -> String {
    let mut segments = Vec::new();
    let mut cur = Some(id);
    let mut seen = HashSet::new();
    while let Some(t) = cur {
        if !seen.insert(t) {
            break;
        }
        match rows.get(&t) {
            Some(row) => {
                segments.push(row.slug.clone());
                cur = row.parent;
            }
            None => break,
        }
    }
    segments.push(taxonomy_key.to_owned());
    segments.reverse();
    segments.join("/")
}

/// Flatten the snapshot into DFS (id, depth) order: roots first, siblings sorted by
/// case-folded name (id tiebreak — the pinned name-asc ordering; Term has no ordinal
/// column). Orphaned/cyclic rows that no root-walk reaches are appended at depth 0
/// rather than dropped (the menus assemble_tree/M2 discipline: a listing must never
/// hide a live row).
fn flatten_tree(rows: &HashMap<u64, TermRow>) -> Vec<(u64, usize)> {
    let mut children: HashMap<Option<u64>, Vec<u64>> = HashMap::new();
    for (id, row) in rows {
        children.entry(row.parent).or_default().push(*id);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| {
            let (ra, rb) = (&rows[a], &rows[b]);
            ra.name
                .to_lowercase()
                .cmp(&rb.name.to_lowercase())
                .then(a.cmp(b))
        });
    }

    let mut out = Vec::with_capacity(rows.len());
    let mut visited = HashSet::new();
    let mut stack: Vec<(u64, usize)> = children
        .get(&None)
        .map(|roots| roots.iter().rev().map(|id| (*id, 0)).collect())
        .unwrap_or_default();
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        out.push((id, depth));
        if let Some(kids) = children.get(&Some(id)) {
            for kid in kids.iter().rev() {
                stack.push((*kid, depth + 1));
            }
        }
    }
    // Unreachable rows (orphaned by a dangling parent that ISN'T in this taxonomy,
    // or a stored cycle): surface them as roots so nothing silently disappears.
    let mut stray: Vec<u64> = rows
        .keys()
        .filter(|id| !visited.contains(id))
        .copied()
        .collect();
    stray.sort_by(|a, b| {
        rows[a]
            .name
            .to_lowercase()
            .cmp(&rows[b].name.to_lowercase())
            .then(a.cmp(b))
    });
    out.extend(stray.into_iter().map(|id| (id, 0)));
    out
}

// ---- small shared helpers ---------------------------------------------------

/// Resolve the effective slug for a term: an explicit one is validated as-is; absent
/// → derived from the name via [`super::slugify`]. Both paths land in
/// [`validate_term_slug`] (shape + the reserved set). `pub(super)` for the post
/// save's inline tag creation (its reuse-by-name lookup needs the same slug the
/// create would mint, or reuse and create could disagree).
pub(super) fn resolve_term_slug(slug: Option<&str>, name: &str) -> Result<String, AdminError> {
    match slug.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => validate_term_slug(s),
        None => {
            let derived = super::slugify(name).ok_or_else(|| {
                AdminError::BadRequest(format!("cannot derive a slug from the name {name:?}"))
            })?;
            validate_term_slug(&derived)
        }
    }
}

/// Validate a term slug: the shared single-segment shape rules PLUS the reserved
/// set (`page`, `feed` — see [`RESERVED_TERM_SLUGS`]). Case-NORMALIZES first
/// (lowercase — WP's `sanitize_title` discipline): the inline tag-create path's
/// reuse-by-slug compares byte-exactly against slugify-derived (lowercase) slugs,
/// so a mixed-case explicit slug would mint an unreachable near-duplicate the
/// reuse machinery can never match — and `Page`/`Feed` would dodge the reserved
/// set.
fn validate_term_slug(slug: &str) -> Result<String, AdminError> {
    let s = content_ops::validate_slug(&slug.to_lowercase())?;
    if RESERVED_TERM_SLUGS.contains(&s.as_str()) {
        return Err(AdminError::BadRequest(format!(
            "the slug {s:?} is reserved (archive pagination / feeds)"
        )));
    }
    Ok(s)
}

/// The `@vectorize` source text for a term: name + description (see the core
/// `Term.plaintext` doc).
fn term_plaintext(name: &str, description: &str) -> String {
    if description.trim().is_empty() {
        name.trim().to_owned()
    } else {
        format!("{} {}", name.trim(), description.trim())
    }
}

/// A `Term.{field}` relation edge.
fn term_edge(id: ObjectId, field: &str) -> Edge {
    Edge {
        type_name: TypeName::from(TERM_TYPE),
        id,
        field: field.to_owned(),
    }
}

/// The single target of a to-one link, or `None`. `pub(super)` for the post save's
/// term-plan validation (resolving each assigned term's taxonomy).
pub(super) async fn single_link(
    state: &AppState,
    type_name: &str,
    id: ObjectId,
    field: &str,
) -> Result<Option<ObjectId>, AdminError> {
    Ok(state
        .store
        .get_links(&Edge {
            type_name: TypeName::from(type_name),
            id,
            field: field.to_owned(),
        })
        .await?
        .into_iter()
        .next()
        .map(|(target, _)| target))
}

/// Read a `Bool` field, defaulting to `false`.
fn bool_field(obj: &Object, field: &str) -> bool {
    matches!(obj.get(field), Some(Value::Bool(true)))
}

/// Whether an object's `status` is exactly `"published"` (the publish gate the
/// live counts apply — mirrors the serve read path's).
fn is_published(obj: &Object) -> bool {
    matches!(obj.get("status"), Some(Value::String(s)) if s == Status::Published.as_str())
}
