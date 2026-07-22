//! Post read/list/save for the editor, scoped per author. A session with the
//! `EditOthersContent` capability (Editor+) may act on ANY post; a Contributor/Author
//! with only `EditOwnContent` is limited to posts they authored (the list is filtered,
//! and get/save on someone else's post return 403). Moving a post INTO or OUT OF a
//! published state additionally requires a `Publish*` capability — this is what lets a
//! Contributor edit their own drafts yet not publish them. A CLI-seeded / legacy post
//! with no `author` link is *backfilled* to whoever (with `EditOthersContent`) first
//! opens or saves it, bringing it into the ownership model.
//!
//! The wire body is Ferropress's own `BlockTree` JSON (`block_tree`), never rinch's
//! `DocNode` — the editor↔BlockTree conversion is client-side in the wasm SPA.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use uuid::Uuid;

use ferropress_core::block::BlockTree;
use ferropress_core::query::Edge;
use ferropress_core::role::Capability;
use ferropress_core::status::Status;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value, now_millis};
use ferropress_core::{CoreError, MEDIA_TYPE, POST_TYPE, TAXONOMY_TYPE, TERM_TYPE, media_url};

use super::{
    AdminError, AdminJson, AuthedUser, content_ops, datetime_field, json_field, str_field,
    terms as term_ops,
};
use crate::AppState;

/// A post's featured image, resolved for the client: the Media's `id` (echoed back
/// on save to set the relation) and its public `url` (for the thumbnail). The `id`
/// is an authenticated-admin handle, not a public URL — the enumeration guard is on
/// the public `/media` route, keyed by the unguessable uuid inside `url`.
#[derive(Serialize)]
pub struct FeaturedMediaDto {
    pub id: u64,
    pub url: String,
}

/// One row in the editor's post list.
#[derive(Serialize)]
pub struct PostSummary {
    pub id: u64,
    pub title: String,
    pub slug: String,
    pub status: String,
    /// Effective last-touched instant (updated_at, else created_at), epoch millis.
    pub updated_at: Option<i64>,
    /// The post's featured image, if any (for the galley-row thumbnail).
    pub featured_media: Option<FeaturedMediaDto>,
}

/// The full post the editor loads (body included).
#[derive(Serialize)]
pub struct PostDetail {
    pub id: u64,
    pub title: String,
    pub slug: String,
    pub status: String,
    /// The canonical block tree as JSON — the SPA converts it to the rinch editor
    /// model client-side.
    pub block_tree: serde_json::Value,
    pub updated_at: Option<i64>,
    /// The post's featured image, if any (shown in the editor's featured control).
    pub featured_media: Option<FeaturedMediaDto>,
    /// The post's assigned terms (categories/tags), resolved for the editor's
    /// assignment panel. Echoing ids back on save sets the membership.
    pub terms: Vec<TermRefDto>,
}

/// One assigned term, resolved for the client (editor chips + the assignment
/// panel's current-selection seed). `taxonomy` is the owning taxonomy's KEY.
#[derive(Serialize)]
pub struct TermRefDto {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub taxonomy: String,
}

/// An inline new-tag request riding a post save/create (WP's "add new tag" box):
/// create — or REUSE by derived slug, WP-style — a root term in a FLAT taxonomy and
/// assign it to the post. Rides the post-edit gate, NOT `ManageTerms`; the server
/// restricts it to non-hierarchical taxonomies (WP: authors mint tags, never
/// categories — those go through the `ManageTerms`-gated terms endpoint).
#[derive(Deserialize)]
pub struct NewTermRequest {
    /// The FLAT taxonomy's key (e.g. `"tag"`).
    pub taxonomy: String,
    pub name: String,
}

/// `GET /admin/api/posts` — the non-trashed posts this user may edit, most-recently-
/// touched first. Editor+ (`EditOthersContent`) sees every post; a Contributor/Author
/// sees only the posts they authored.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<PostSummary>>, AdminError> {
    // The minimum bar for any editing surface (a Subscriber → 403). `EditOthersContent`
    // implies `EditOwnContent` (the ladder is cumulative), so this admits Contributor+
    // and we branch on scope next.
    who.require(Capability::EditOwnContent)?;

    let objs: Vec<Object> = state
        .store
        .scan(&TypeName::from(POST_TYPE))
        .await?
        .into_iter()
        .filter(|o| str_field(o, "status").as_deref() != Some(Status::Trashed.as_str()))
        .collect();

    // Scope to authorship. Editor+ sees everything; a lower role sees only their own.
    // `author` has no `@inverse` reverse edge, so ownership is resolved by ONE batched
    // link read over the scanned ids (the id-only `get_links_many` fast path — a single
    // store round-trip, not N+1). A null-author (legacy) post belongs to no one, so it
    // is invisible to the own-only view until an Editor backfills it.
    let objs: Vec<Object> = if who.can_edit_others() {
        objs
    } else {
        let ids: Vec<ObjectId> = objs.iter().map(|o| o.id).collect();
        let authors = state
            .store
            .get_links_many(&TypeName::from(POST_TYPE), &ids, "author")
            .await?;
        objs.into_iter()
            .zip(authors)
            .filter(|(_, authors)| authors.contains(&who.id))
            .map(|(obj, _)| obj)
            .collect()
    };

    // Resolve each row's featured image (a to-one relation, so it's a per-row link
    // read — fine for the admin galley, which is small and not a hot path).
    let mut posts = Vec::with_capacity(objs.len());
    for obj in &objs {
        let featured = resolve_featured(&state, obj.id).await?;
        posts.push(summary(obj, featured));
    }

    // Most-recently-touched first (no server-side sort primitive; sort in Rust,
    // exactly like the comments island). Untimed rows (`None`) sink to the bottom.
    posts.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
    Ok(Json(posts))
}

/// `GET /admin/api/posts/{id}` — one post with its body, for the editor to load.
pub async fn get_one(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<Json<PostDetail>, AdminError> {
    // Load first (a missing post is 404 regardless of who asks), then authorize against
    // its author: Editor+ may open any post, a lower role only their own.
    let obj = state
        .store
        .get(&TypeName::from(POST_TYPE), ObjectId(id))
        .await?;
    let author = author_of(&state, ObjectId(id)).await?;
    // 404 (not 403) when denied, so a lower role can't tell another author's post from
    // a missing id — matching the scoped list, which hides those posts.
    who.require_post_access(author)?;
    // Opening a legacy null-author post in the editor claims authorship for the opener
    // (backfill-on-touch). Best-effort: never fail the read on a link hiccup.
    backfill_author(&state, ObjectId(id), &who, author).await;

    Ok(Json(PostDetail {
        id,
        title: str_field(&obj, "title").unwrap_or_default(),
        slug: str_field(&obj, "slug").unwrap_or_default(),
        status: str_field(&obj, "status").unwrap_or_default(),
        block_tree: json_field(&obj, "block_tree").unwrap_or_else(|| {
            // A post with no body yet: hand back a valid empty tree so the editor
            // always loads a well-formed document.
            BlockTree::from_blocks(Vec::new())
                .to_json_value()
                .unwrap_or(serde_json::Value::Null)
        }),
        updated_at: effective_time(&obj),
        featured_media: resolve_featured(&state, ObjectId(id)).await?,
        terms: resolve_terms(&state, ObjectId(id)).await?,
    }))
}

/// The editor's save payload.
#[derive(Deserialize)]
pub struct SaveRequest {
    pub title: String,
    pub slug: String,
    pub status: String,
    /// The edited block tree as JSON (Ferropress's shape, converted from the rinch
    /// editor client-side).
    pub block_tree: serde_json::Value,
    /// The featured image's Media id, or `null`/absent to clear it. The save
    /// reconciles the to-one `featured_media` relation to match.
    #[serde(default)]
    pub featured_media: Option<u64>,
    /// Desired term memberships (term ids). `None`/absent = LEAVE UNCHANGED — an
    /// absent field must never clear, or a save from an older client (or any path
    /// that omits it) silently strips every category/tag off the post. `Some(v)` =
    /// set-reconcile the membership to exactly `v` (plus any `new_terms`);
    /// `Some([])` clears. This asymmetry with a plain `Vec` default is deliberate
    /// and load-bearing.
    #[serde(default)]
    pub terms: Option<Vec<u64>>,
    /// Inline new tags to create-or-reuse and assign, ADDITIVE on top of `terms`
    /// (or on top of the current membership when `terms` is absent). Flat
    /// taxonomies only; see [`NewTermRequest`].
    #[serde(default)]
    pub new_terms: Option<Vec<NewTermRequest>>,
}

#[derive(Serialize)]
pub struct SaveResponse {
    pub id: u64,
    pub updated_at: i64,
    /// The post's terms AFTER the save's reconcile — authoritative, so the editor
    /// re-seeds its assignment panel from this (inline-created tags gain real ids).
    pub terms: Vec<TermRefDto>,
}

/// `PUT /admin/api/posts/{id}` — persist an edit. Validates the slug, the status
/// (and its transition from the current one), and that the block tree is
/// well-formed; re-derives `plaintext` (for search) and stamps `updated_at`. The
/// regen loop then regenerates the affected public page off the change feed.
pub async fn save(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<SaveRequest>,
) -> Result<Json<SaveResponse>, AdminError> {
    // Validate the request shape (independent of the stored resource) first. A post slug must be
    // a single flat path segment (the resolver's flat-vs-nested split assumes it), and never a
    // reserved URL base (`page`/`feed` — the pagination + feed grammar).
    let slug = content_ops::validate_slug(&body.slug)?;
    content_ops::ensure_unreserved_root(&slug)?;
    let new_status = parse_status(&body.status)
        .ok_or_else(|| AdminError::BadRequest(format!("unknown status {:?}", body.status)))?;
    let tree = BlockTree::from_json_value(body.block_tree.clone())
        .map_err(|e| AdminError::BadRequest(format!("invalid block tree: {e}")))?;

    // Load the target and AUTHORIZE before touching any other resource — so an
    // unauthorized caller can't probe slug/media existence through the checks below.
    // A missing post is 404; its current status gates the transition.
    let current = state
        .store
        .get(&TypeName::from(POST_TYPE), ObjectId(id))
        .await?;
    let old_slug = str_field(&current, "slug").unwrap_or_default();
    let current_status =
        parse_status(&str_field(&current, "status").unwrap_or_default()).unwrap_or(Status::Draft);
    let author = author_of(&state, ObjectId(id)).await?;
    // 404 (not 403) when denied — an unauthorized caller can't distinguish an existing
    // post they may not edit from a missing id (and can't probe the slug/media checks
    // below). See [`AuthedUser::require_post_access`].
    who.require_post_access(author)?;
    // Moving a post INTO or OUT OF a published state is a publish act — gate it on a
    // Publish* capability. A Contributor may edit their own draft but not publish it,
    // and (since a published post's current status is a publish state) may not edit a
    // published post at all; an Author may publish their own, an Editor anyone's.
    if current_status.is_publish_state() || new_status.is_publish_state() {
        who.require_publish(author)?;
    }

    // Validate the featured image up front so a bad id can't half-save (scalars
    // written, relation not) — the relation is reconciled after the scalar update.
    if let Some(mid) = body.featured_media {
        ensure_media_exists(&state, mid).await?;
    }

    // Plan the term work up front too (reads only): every assigned id must be a real
    // Term with a taxonomy, inline tags must target a known flat taxonomy, and the
    // per-taxonomy `multiple` cap must hold — all BEFORE any mutation.
    let terms_plan = plan_terms(&state, Some(ObjectId(id)), &body.terms, &body.new_terms).await?;

    // Reject a slug already held in the permalink namespace by ANOTHER entity — a different
    // post OR a page at the same path (cross-entity: both share one cache key + resolver slot).
    if content_ops::is_taken(&state, &slug, &[ObjectId(id)]).await? {
        return Err(AdminError::Conflict(format!(
            "the slug {slug:?} is already in use"
        )));
    }

    if !current_status.can_transition_to(new_status) {
        return Err(AdminError::BadRequest(format!(
            "cannot change status from {} to {}",
            current_status.as_str(),
            new_status.as_str()
        )));
    }

    // Relation work FIRST, the scalar update LAST. rhypedb link/unlink emit NO
    // ChangeEvent, so the one Post Update below is the SETTLING event the regen
    // loop rebuilds the page envelope from — ordered after the links, it can never
    // observe (and bake) a pre-reconcile membership/featured state. This is the
    // menus `touch_menu` discipline with the save's own scalar write as the touch.
    // (Failure semantics improve too: a relation fault now fails the save with the
    // scalars UNWRITTEN, instead of yesterday's half-saved post.)
    set_featured(&state, ObjectId(id), body.featured_media).await?;

    // The planned term work (validated above): inline tags create-or-reuse under
    // `taxonomy_lock` (each created Term settles itself via its own touch), then
    // the membership set-reconcile.
    if let Some(action) = terms_plan {
        apply_terms(&state, ObjectId(id), action).await?;
    }

    let now = now_millis();
    let mut patch: FieldMap = HashMap::new();
    patch.insert("title".to_owned(), Value::String(body.title));
    patch.insert("slug".to_owned(), Value::String(slug.to_owned()));
    patch.insert(
        "status".to_owned(),
        Value::String(new_status.as_str().to_owned()),
    );
    patch.insert("block_tree".to_owned(), Value::Json(body.block_tree));
    // Refresh the @vectorize source so search tracks the edit.
    patch.insert("plaintext".to_owned(), Value::String(tree.plaintext()));
    patch.insert("updated_at".to_owned(), Value::DateTime(now));

    state
        .store
        .update(&TypeName::from(POST_TYPE), ObjectId(id), patch)
        .await?;

    // The slug (hence the post's permalink) may have moved. The slug scalar is now the new
    // value, so evict the stale cache blob at the old path and record a 301 old→new (a no-op
    // when the slug is unchanged) — closing the renamed-post-serves-stale gap symmetrically
    // with pages.
    content_ops::record_move(&state, &old_slug, &slug).await?;

    // A legacy null-author post that was just successfully edited is now attributed to
    // its editor (backfill-on-touch); best-effort so it never fails the save.
    backfill_author(&state, ObjectId(id), &who, author).await;

    Ok(Json(SaveResponse {
        id,
        updated_at: now,
        terms: resolve_terms(&state, ObjectId(id)).await?,
    }))
}

/// The editor's "New post" payload. `status` is optional (a new post is born a
/// `Draft`); `block_tree` may be an empty tree for a blank draft.
#[derive(Deserialize)]
pub struct CreateRequest {
    pub title: String,
    pub slug: String,
    #[serde(default)]
    pub status: Option<String>,
    /// The initial block tree as JSON (Ferropress's shape). Usually the empty tree.
    pub block_tree: serde_json::Value,
    /// An optional featured image (Media id) to attach to the new post.
    #[serde(default)]
    pub featured_media: Option<u64>,
    /// Initial term memberships (term ids). Same contract as
    /// [`SaveRequest::terms`]; on create, absent simply means "no terms".
    #[serde(default)]
    pub terms: Option<Vec<u64>>,
    /// Inline new tags to create-or-reuse and assign (see [`NewTermRequest`]).
    #[serde(default)]
    pub new_terms: Option<Vec<NewTermRequest>>,
}

#[derive(Serialize)]
pub struct CreateResponse {
    pub id: u64,
    pub created_at: i64,
    /// The new post's terms after the reconcile — authoritative (inline-created
    /// tags carry their real ids).
    pub terms: Vec<TermRefDto>,
}

/// `POST /admin/api/posts` — create a new post (the editor's "New post" flow).
/// Validates the slug (non-empty + globally unique) and the initial status, and
/// checks the block tree is well-formed; derives `plaintext` (for search) and
/// stamps `uuid`/`post_type`/`created_at`/`updated_at`. The post is attributed to
/// its creator via the `author` relation. Returns the new id so the editor can
/// switch from create to update on the next save; the regen loop then prerenders
/// it (a `Draft` stays out of the public index).
pub async fn create(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<CreateRequest>,
) -> Result<Json<CreateResponse>, AdminError> {
    // Anyone who can edit their own content may create (Contributor+); a Subscriber
    // is forbidden.
    who.require(Capability::EditOwnContent)?;

    let slug = content_ops::validate_slug(&body.slug)?;
    content_ops::ensure_unreserved_root(&slug)?;
    let status = initial_status(body.status.as_deref())?;
    // A new post born directly into a published state is a publish act — gate it. The
    // creator is the post's author, so authorize as the owner: a Contributor may create
    // a Draft/Pending but not a Published post; an Author (PublishOwnContent) may.
    if status.is_publish_state() {
        who.require_publish(Some(who.id))?;
    }
    let tree = BlockTree::from_json_value(body.block_tree.clone())
        .map_err(|e| AdminError::BadRequest(format!("invalid block tree: {e}")))?;

    // Validate a chosen featured image before creating anything.
    if let Some(mid) = body.featured_media {
        ensure_media_exists(&state, mid).await?;
    }

    // Plan the term work before creating anything (reads only — see `save`).
    let terms_plan = plan_terms(&state, None, &body.terms, &body.new_terms).await?;

    // No self to exclude on create: ANY post OR page already at this slug/path is a clash.
    if content_ops::is_taken(&state, &slug, &[]).await? {
        return Err(AdminError::Conflict(format!(
            "the slug {slug:?} is already in use"
        )));
    }

    let now = now_millis();
    let mut fields: FieldMap = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(Uuid::now_v7().to_string()));
    fields.insert("slug".to_owned(), Value::String(slug.clone()));
    fields.insert("title".to_owned(), Value::String(body.title));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert("block_tree".to_owned(), Value::Json(body.block_tree));
    // Derive the @vectorize source so a new post is immediately searchable.
    fields.insert("plaintext".to_owned(), Value::String(tree.plaintext()));
    fields.insert("created_at".to_owned(), Value::DateTime(now));
    fields.insert("updated_at".to_owned(), Value::DateTime(now));

    let id = state
        .store
        .create(&TypeName::from(POST_TYPE), fields)
        .await?;

    // Attribute the post to its creator. `author` is a to-one RELATION, set via
    // the link API (not a scalar field). If the link fails — e.g. the signed-in
    // user was deleted mid-session, so the link target no longer exists — roll the
    // just-created post back rather than leave an unattributed orphan, then report
    // the cause. (A later slice reads `author` for per-author `EditOwnContent`
    // scoping, so every created post must carry it.)
    let author = Edge {
        type_name: TypeName::from(POST_TYPE),
        id,
        field: "author".to_owned(),
    };
    if let Err(e) = state.store.link(&author, who.id, FieldMap::new()).await {
        if let Err(rollback) = state.store.delete(&TypeName::from(POST_TYPE), id).await {
            tracing::error!(
                error = %rollback,
                "failed to roll back orphan post after author-link failure"
            );
        }
        return Err(e.into());
    }

    // Attach the featured image if one was chosen (validated above). Like `author`,
    // a to-one relation set via the link API; roll the post back on failure so we
    // never leave an orphan.
    if let Some(mid) = body.featured_media
        && let Err(e) = state
            .store
            .link(&featured_edge(id), ObjectId(mid), FieldMap::new())
            .await
    {
        if let Err(rollback) = state.store.delete(&TypeName::from(POST_TYPE), id).await {
            tracing::error!(
                error = %rollback,
                "failed to roll back post after featured-media link failure"
            );
        }
        return Err(e.into());
    }

    // Apply the planned term work (validated above). Like the author/featured links,
    // a fault rolls the just-created post back rather than leaving a half-attributed
    // orphan (any inline tag already created survives — it is valid vocabulary).
    if let Some(action) = terms_plan
        && let Err(e) = apply_terms(&state, id, action).await
    {
        if let Err(rollback) = state.store.delete(&TypeName::from(POST_TYPE), id).await {
            tracing::error!(
                error = %rollback,
                "failed to roll back post after term-assignment failure"
            );
        }
        return Err(e);
    }

    // Settle: the Create event above raced the (eventless) author/featured/term
    // links, so a regen-loop rebuild triggered by it could read a pre-link state.
    // One trailing scalar Update AFTER all link work re-emits with everything in
    // place — the `touch_menu` discipline (harmless for a draft: the rebuild
    // publish-gates to an eviction either way).
    let mut settle: FieldMap = HashMap::new();
    settle.insert("updated_at".to_owned(), Value::DateTime(now));
    state
        .store
        .update(&TypeName::from(POST_TYPE), id, settle)
        .await?;

    // Shadow-guard: a live post now occupies this slug → drop any stale 301 FROM it (e.g. a
    // path freed by an earlier rename that this new post reuses).
    content_ops::retire_redirects_at(&state, &slug).await?;

    Ok(Json(CreateResponse {
        id: id.0,
        created_at: now,
        terms: resolve_terms(&state, id).await?,
    }))
}

/// Validate the initial status of a NEW post. A post is born a `Draft`; an
/// explicit status must be a legal departure from `Draft` (so `Published`,
/// `Pending`, `Scheduled` are allowed) and never `Trashed`. Absent/empty → `Draft`.
pub(super) fn initial_status(raw: Option<&str>) -> Result<Status, AdminError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Status::Draft),
        Some(s) => {
            let status = parse_status(s)
                .ok_or_else(|| AdminError::BadRequest(format!("unknown status {s:?}")))?;
            if status == Status::Draft {
                return Ok(status);
            }
            if status == Status::Trashed || !Status::Draft.can_transition_to(status) {
                return Err(AdminError::BadRequest(format!(
                    "a new post can't start as {}",
                    status.as_str()
                )));
            }
            Ok(status)
        }
    }
}

/// Build a list-row summary from a post object + its resolved featured image.
fn summary(obj: &Object, featured: Option<FeaturedMediaDto>) -> PostSummary {
    PostSummary {
        id: obj.id.0,
        title: str_field(obj, "title").unwrap_or_default(),
        slug: str_field(obj, "slug").unwrap_or_default(),
        status: str_field(obj, "status").unwrap_or_default(),
        updated_at: effective_time(obj),
        featured_media: featured,
    }
}

/// The `Post.featured_media` relation edge for `post_id` (a to-one link to a `Media`).
fn featured_edge(post_id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "featured_media".to_owned(),
    }
}

/// The `Post.author` relation edge for `post_id` (a to-one link to a `User`).
fn author_edge(post_id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "author".to_owned(),
    }
}

/// Read a post's author id (the single `author` to-one link), or `None` when the post
/// is unattributed — a CLI-seeded or pre-attribution legacy row. This is the ownership
/// signal every per-author gate reads (incl. the preview route, hence `pub(super)`).
pub(super) async fn author_of(
    state: &AppState,
    post_id: ObjectId,
) -> Result<Option<ObjectId>, AdminError> {
    Ok(state
        .store
        .get_links(&author_edge(post_id))
        .await?
        .into_iter()
        .next()
        .map(|(author_id, _)| author_id))
}

/// Backfill-on-touch: stamp `who` as the author of `post_id` when it currently has
/// none. Guarded two ways up front — it no-ops when the caller already saw an author
/// (`current_author.is_some()`) and when the toucher may only edit their OWN content
/// (`!who.can_edit_others()`), so a lower role can never claim an orphaned post (they
/// are already denied by `require_post_access` before reaching here; this is
/// belt-and-suspenders).
///
/// Before writing, it RE-READS the author link, for two reasons: the `current_author`
/// snapshot can be stale (in `save` a whole update runs between that read and here),
/// and the to-one `author` link is NOT cardinality-enforced by the store — `link`
/// appends a second edge rather than replacing one (the reason [`set_featured`] must
/// unlink the old target). Skipping when an edge already exists therefore avoids
/// stacking a second author on a post another concurrent touch just attributed, and
/// never overwrites a real author. The only residual is a truly simultaneous
/// double-backfill of the SAME orphan (the same narrow race `set_featured` accepts);
/// it self-limits because a post is backfilled at most once in its life.
///
/// Best-effort throughout: any store error is logged, never surfaced. The read or save
/// it rides on still succeeds, and the next touch retries.
async fn backfill_author(
    state: &AppState,
    post_id: ObjectId,
    who: &AuthedUser,
    current_author: Option<ObjectId>,
) {
    if current_author.is_some() || !who.can_edit_others() {
        return;
    }
    let edge = author_edge(post_id);
    match state.store.get_links(&edge).await {
        // Attributed since the caller's read (a real author, or a concurrent backfill):
        // do NOT add a second edge to the to-one relation.
        Ok(links) if !links.is_empty() => return,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, post_id = post_id.0, "backfill precheck failed (skipping)");
            return;
        }
    }
    if let Err(e) = state.store.link(&edge, who.id, FieldMap::new()).await {
        tracing::warn!(
            error = %e,
            post_id = post_id.0,
            "failed to backfill null author (will retry on next touch)"
        );
    }
}

/// Resolve a post's featured image to `{id, url}`, or `None` when it has none.
/// `featured_media` is a to-one relation, so we take the single link. `Media` is
/// `@on_delete(remove)` from this edge, so the link can't dangle — but a missing
/// target is treated as "no featured image" rather than erroring the whole read.
async fn resolve_featured(
    state: &AppState,
    post_id: ObjectId,
) -> Result<Option<FeaturedMediaDto>, AdminError> {
    let Some((media_id, _)) = state
        .store
        .get_links(&featured_edge(post_id))
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

/// Fail with a `BadRequest` if no `Media` has id `media_id` (so an author can't
/// feature a non-existent image, and a bad id is caught BEFORE any mutation).
pub(super) async fn ensure_media_exists(state: &AppState, media_id: u64) -> Result<(), AdminError> {
    match state
        .store
        .get(&TypeName::from(MEDIA_TYPE), ObjectId(media_id))
        .await
    {
        Ok(_) => Ok(()),
        Err(CoreError::NotFound { .. }) => Err(AdminError::BadRequest(format!(
            "featured_media {media_id} does not exist"
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Reconcile the to-one `featured_media` relation to `media_id`. The target's
/// existence is validated by the caller before this runs, so this only performs the
/// link churn. `media_id == None` clears the featured image.
///
/// This can't be atomic (the store has no transaction spanning two link ops), so it's
/// ordered to fail SAFE: it links the NEW target FIRST, then removes any OTHER (old)
/// links. A fault during the link leaves the existing featured image intact rather
/// than dropping it; the residual worst case (a link succeeds but the subsequent
/// unlink faults) is a transient extra link a retry cleans up. The early return skips
/// all of this — and its failure window — when the relation already matches.
async fn set_featured(
    state: &AppState,
    post_id: ObjectId,
    media_id: Option<u64>,
) -> Result<(), AdminError> {
    let edge = featured_edge(post_id);
    let existing = state.store.get_links(&edge).await?;
    // Already exactly the requested target (the common "keep" case): nothing to do.
    if existing.len() == 1 && Some(existing[0].0.0) == media_id {
        return Ok(());
    }
    if let Some(mid) = media_id {
        state
            .store
            .link(&edge, ObjectId(mid), FieldMap::new())
            .await?;
    }
    for (old, _) in existing {
        if Some(old.0) != media_id {
            state.store.unlink(&edge, old).await?;
        }
    }
    Ok(())
}

// ---- term assignment (the Post.terms M:N membership) ------------------------

/// The `Post.terms` relation edge for `post_id` (the to-many M:N join to `Term`).
fn terms_edge(post_id: ObjectId) -> Edge {
    Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "terms".to_owned(),
    }
}

/// The post's current term ids (the forward `Post.terms` links).
async fn current_terms(state: &AppState, post_id: ObjectId) -> Result<Vec<ObjectId>, AdminError> {
    Ok(state
        .store
        .get_links(&terms_edge(post_id))
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// The validated term work a save/create applies AFTER its scalar write. Produced by
/// [`plan_terms`] (all reads, no mutation — so a bad request fails BEFORE anything
/// is written), consumed by [`apply_terms`] under `taxonomy_lock`.
struct TermsAction {
    /// The explicit desired membership (`SaveRequest::terms`), deduped. `None` =
    /// additive: keep the current membership and add the new tags on top.
    explicit: Option<Vec<ObjectId>>,
    /// Inline tags to create-or-reuse: `(taxonomy object, display name, resolved
    /// slug)`. The authoritative reuse-vs-create decision is REMADE under the lock;
    /// this list is the validated intent.
    creates: Vec<(Object, String, String)>,
}

/// Distinguishes prospective members for the per-taxonomy `multiple` cap: a known
/// term id, or a yet-to-create tag identified by its resolved slug (a reused slug
/// collapses onto the existing id, so the count is exact, never inflated).
#[derive(PartialEq, Eq, Hash)]
enum MemberKey {
    Id(u64),
    Slug(String),
}

/// Validate a save/create's term inputs and build the [`TermsAction`] — reads only.
///
/// Enforced here, BEFORE any mutation (the critique's ordering rule):
///   * every explicit id exists AND is a `Term` (`get(&TERM_TYPE, id)` is the type
///     assert — the link API alone would accept an arbitrary ObjectId injected into
///     `Post.terms`) and carries a taxonomy link → else 400;
///   * every inline tag names a KNOWN, FLAT taxonomy (tags-only rule) and yields a
///     valid, unreserved slug → else 400;
///   * the per-taxonomy `multiple = false` cap holds over the FULL prospective
///     membership — explicit ids (or, additively, the CURRENT membership) plus the
///     inline tags — grouped per taxonomy, because `Post.terms` is one shared edge
///     carrying every taxonomy's assignments at once (a global cap would forbid
///     tagging a categorized post) → else 400.
///
/// `post_id` is `None` on create (the additive base is empty). Returns `None` when
/// the request carries no term work at all (membership left completely untouched).
async fn plan_terms(
    state: &AppState,
    post_id: Option<ObjectId>,
    terms: &Option<Vec<u64>>,
    new_terms: &Option<Vec<NewTermRequest>>,
) -> Result<Option<TermsAction>, AdminError> {
    let has_new = new_terms.as_ref().is_some_and(|v| !v.is_empty());
    if terms.is_none() && !has_new {
        return Ok(None);
    }

    // Explicit ids, deduped (a duplicate id is client sloppiness, not an error).
    let explicit: Option<Vec<ObjectId>> = terms.as_ref().map(|ids| {
        let mut seen = std::collections::HashSet::new();
        ids.iter()
            .filter(|id| seen.insert(**id))
            .map(|id| ObjectId(*id))
            .collect()
    });

    // The membership base the cap is checked over: the explicit set, or (additive)
    // the post's current membership.
    let base: Vec<ObjectId> = match &explicit {
        Some(ids) => ids.clone(),
        None => match post_id {
            Some(id) => current_terms(state, id).await?,
            None => Vec::new(),
        },
    };

    // Resolve each base term's taxonomy; a missing/foreign id or a taxonomy-less
    // (corrupt) term is a 400, distinct from the handler's own 404 semantics.
    let mut prospective: std::collections::HashSet<(u64, MemberKey)> =
        std::collections::HashSet::new();
    let mut tax_ids: Vec<ObjectId> = Vec::new();
    for id in &base {
        match state.store.get(&TypeName::from(TERM_TYPE), *id).await {
            Ok(_) => {}
            Err(CoreError::NotFound { .. }) => {
                return Err(AdminError::BadRequest(format!(
                    "term {} does not exist",
                    id.0
                )));
            }
            Err(e) => return Err(e.into()),
        }
        let tax = term_ops::single_link(state, TERM_TYPE, *id, "taxonomy")
            .await?
            .ok_or_else(|| AdminError::BadRequest(format!("term {} has no taxonomy", id.0)))?;
        prospective.insert((tax.0, MemberKey::Id(id.0)));
        tax_ids.push(tax);
    }

    // Validate the inline tags: known FLAT taxonomy + a valid slug; dedupe by
    // (taxonomy key, slug) so one request can't mint the same tag twice. A slug
    // that matches an EXISTING root term collapses onto its id (reuse) for the cap.
    let mut creates: Vec<(Object, String, String)> = Vec::new();
    let mut planned: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    if let Some(reqs) = new_terms {
        // Cache each taxonomy's object + term rows across the loop (one request
        // usually targets one taxonomy).
        let mut taxonomies: HashMap<String, Object> = HashMap::new();
        let mut rows_cache: HashMap<u64, HashMap<u64, term_ops::TermRow>> = HashMap::new();
        for req in reqs {
            let name = req.name.trim();
            if name.is_empty() {
                return Err(AdminError::BadRequest("a new term needs a name".to_owned()));
            }
            let key = req.taxonomy.trim().to_owned();
            if !taxonomies.contains_key(&key) {
                let tax = match term_ops::taxonomy_by_key(state, &key).await {
                    Ok(t) => t,
                    Err(AdminError::NotFound) => {
                        return Err(AdminError::BadRequest(format!("unknown taxonomy {key:?}")));
                    }
                    Err(e) => return Err(e),
                };
                taxonomies.insert(key.clone(), tax);
            }
            let tax = &taxonomies[&key];
            if matches!(tax.get("hierarchical"), Some(Value::Bool(true))) {
                return Err(AdminError::BadRequest(format!(
                    "inline term creation is tags-only; taxonomy {key:?} is \
                     hierarchical — use the terms endpoint"
                )));
            }
            let slug = term_ops::resolve_term_slug(None, name)?;
            if !planned.insert((key.clone(), slug.clone())) {
                continue; // same tag twice in one request → keep the first
            }
            if let std::collections::hash_map::Entry::Vacant(e) = rows_cache.entry(tax.id.0) {
                e.insert(term_ops::load_term_rows(state, tax.id).await?);
            }
            let reused = rows_cache[&tax.id.0]
                .iter()
                .find(|(_, row)| row.parent.is_none() && row.slug == slug)
                .map(|(id, _)| *id);
            match reused {
                // Reuse collapses onto the id — exact cap counting, and the entry
                // stays in `creates` so the under-lock pass makes the final call.
                Some(id) => prospective.insert((tax.id.0, MemberKey::Id(id))),
                None => prospective.insert((tax.id.0, MemberKey::Slug(slug.clone()))),
            };
            tax_ids.push(tax.id);
            creates.push((tax.clone(), name.to_owned(), slug));
        }
    }

    // The per-taxonomy `multiple` cap over the full prospective membership.
    tax_ids.sort_unstable();
    tax_ids.dedup();
    let tax_objs = if tax_ids.is_empty() {
        Vec::new() // e.g. an explicit clear (`terms: []`) — nothing to cap.
    } else {
        state
            .store
            .get_many(&TypeName::from(TAXONOMY_TYPE), &tax_ids)
            .await?
    };
    for tax in &tax_objs {
        if matches!(tax.get("multiple"), Some(Value::Bool(true))) {
            continue;
        }
        let members = prospective.iter().filter(|(t, _)| *t == tax.id.0).count();
        if members > 1 {
            return Err(AdminError::BadRequest(format!(
                "taxonomy {:?} accepts a single term per post; got {members}",
                str_field(tax, "key").unwrap_or_default()
            )));
        }
    }

    Ok(Some(TermsAction { explicit, creates }))
}

/// Execute a validated [`TermsAction`]: create-or-reuse the inline tags (under
/// `taxonomy_lock`, through the SAME [`term_ops::create_term_core`] guards the
/// admin endpoint uses — the two creation paths can never diverge), then
/// set-reconcile `Post.terms` to the final desired membership.
///
/// Runs AFTER the post's scalar write, so the write's `updated_at` bump has already
/// emitted the Post Update event that drives cache handling — the (eventless)
/// link/unlink work here needs no settling event of its own on the POST side; the
/// created TERMS get theirs from `create_term_core`'s touch. A mid-op fault leaves
/// a partially-applied membership; the reconcile is idempotent, so a client retry
/// converges (and any inline tag already created is simply reused).
async fn apply_terms(
    state: &AppState,
    post_id: ObjectId,
    action: TermsAction,
) -> Result<(), AdminError> {
    let _guard = state.taxonomy_lock.lock().await;

    // Create-or-reuse the inline tags. The reuse decision is REMADE here, under the
    // lock, against fresh rows — the plan's read was advisory (a concurrent create
    // between plan and apply must yield a reuse, not a duplicate-slug 409).
    let mut resolved_new: Vec<ObjectId> = Vec::new();
    let mut rows_cache: HashMap<u64, HashMap<u64, term_ops::TermRow>> = HashMap::new();
    for (tax, name, slug) in &action.creates {
        if let std::collections::hash_map::Entry::Vacant(e) = rows_cache.entry(tax.id.0) {
            e.insert(term_ops::load_term_rows(state, tax.id).await?);
        }
        let rows = rows_cache.get_mut(&tax.id.0).expect("cached above");
        let reused = rows
            .iter()
            .find(|(_, row)| row.parent.is_none() && row.slug == *slug)
            .map(|(id, _)| *id);
        match reused {
            Some(id) => resolved_new.push(ObjectId(id)),
            None => {
                let id = term_ops::create_term_core(state, tax, rows, name, Some(slug), "", None)
                    .await?;
                rows.insert(
                    id.0,
                    term_ops::TermRow {
                        slug: slug.clone(),
                        name: name.clone(),
                        description: String::new(),
                        parent: None,
                    },
                );
                resolved_new.push(id);
            }
        }
    }

    // The final desired membership: the explicit set (or, additively, the current
    // one) plus the inline tags.
    let mut desired = match action.explicit {
        Some(ids) => ids,
        None => current_terms(state, post_id).await?,
    };
    desired.extend(resolved_new);
    desired.sort_unstable();
    desired.dedup();

    set_terms(state, post_id, &desired).await
}

/// Set-reconcile the to-MANY `Post.terms` edge to exactly `desired`: link the
/// missing targets FIRST, then unlink the removed ones — the fail-safe ordering
/// ([`set_featured`]'s, generalized to N): a mid-op fault can leave a transient
/// superset but never drops a kept term. NOT `reconcile_to_one` (which would strip
/// all-but-one member — the silent-data-loss shape the design review flagged).
async fn set_terms(
    state: &AppState,
    post_id: ObjectId,
    desired: &[ObjectId],
) -> Result<(), AdminError> {
    let edge = terms_edge(post_id);
    let existing: Vec<ObjectId> = state
        .store
        .get_links(&edge)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for id in desired {
        if !existing.contains(id) {
            state.store.link(&edge, *id, FieldMap::new()).await?;
        }
    }
    for old in existing {
        if !desired.contains(&old) {
            state.store.unlink(&edge, old).await?;
        }
    }
    Ok(())
}

/// Resolve a post's assigned terms into [`TermRefDto`]s (id + live name/slug +
/// taxonomy key), sorted (taxonomy key, case-folded name, id) for a stable client
/// order. Batched: one link read + one Term read + one taxonomy-link read + one
/// Taxonomy read.
async fn resolve_terms(state: &AppState, post_id: ObjectId) -> Result<Vec<TermRefDto>, AdminError> {
    let ids = current_terms(state, post_id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let objs = state
        .store
        .get_many(&TypeName::from(TERM_TYPE), &ids)
        .await?;
    let tax_links = state
        .store
        .get_links_many(&TypeName::from(TERM_TYPE), &ids, "taxonomy")
        .await?;
    let mut tax_ids: Vec<ObjectId> = tax_links
        .iter()
        .filter_map(|l| l.first().copied())
        .collect();
    tax_ids.sort_unstable();
    tax_ids.dedup();
    let tax_keys: HashMap<u64, String> = state
        .store
        .get_many(&TypeName::from(TAXONOMY_TYPE), &tax_ids)
        .await?
        .iter()
        .map(|o| (o.id.0, str_field(o, "key").unwrap_or_default()))
        .collect();

    let mut out: Vec<TermRefDto> = objs
        .iter()
        .zip(&tax_links)
        .map(|(obj, tax)| TermRefDto {
            id: obj.id.0,
            name: str_field(obj, "name").unwrap_or_default(),
            slug: str_field(obj, "slug").unwrap_or_default(),
            taxonomy: tax
                .first()
                .and_then(|t| tax_keys.get(&t.0).cloned())
                .unwrap_or_default(),
        })
        .collect();
    out.sort_by(|a, b| {
        a.taxonomy
            .cmp(&b.taxonomy)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.id.cmp(&b.id))
    });
    Ok(out)
}

/// `updated_at` if present, else `created_at` — the instant the list sorts on.
pub(super) fn effective_time(obj: &Object) -> Option<i64> {
    datetime_field(obj, "updated_at").or_else(|| datetime_field(obj, "created_at"))
}

/// Parse a snake_case status string into a [`Status`] via serde.
pub(super) fn parse_status(s: &str) -> Option<Status> {
    serde_json::from_value::<Status>(serde_json::Value::String(s.to_owned())).ok()
}
