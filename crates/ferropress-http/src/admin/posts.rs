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
use ferropress_core::query::{Compare, Edge, FilterSpec};
use ferropress_core::role::Capability;
use ferropress_core::status::Status;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value, now_millis};
use ferropress_core::{CoreError, MEDIA_TYPE, POST_TYPE, media_url};

use super::{AdminError, AdminJson, AuthedUser, datetime_field, json_field, str_field};
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
}

#[derive(Serialize)]
pub struct SaveResponse {
    pub id: u64,
    pub updated_at: i64,
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
    // Validate the request shape (independent of the stored resource) first.
    let slug = body.slug.trim();
    if slug.is_empty() {
        return Err(AdminError::BadRequest("slug must not be empty".to_owned()));
    }
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

    // Reject a slug already held by a DIFFERENT post (see [`slug_taken`]).
    if slug_taken(&state, slug, Some(ObjectId(id))).await? {
        return Err(AdminError::Conflict(format!(
            "the slug {slug:?} is already used by another post"
        )));
    }

    if !current_status.can_transition_to(new_status) {
        return Err(AdminError::BadRequest(format!(
            "cannot change status from {} to {}",
            current_status.as_str(),
            new_status.as_str()
        )));
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

    // Reconcile the to-one `featured_media` relation to match the request (validated
    // above): drop whatever was featured, then link the new target if any.
    set_featured(&state, ObjectId(id), body.featured_media).await?;

    // A legacy null-author post that was just successfully edited is now attributed to
    // its editor (backfill-on-touch); best-effort so it never fails the save.
    backfill_author(&state, ObjectId(id), &who, author).await;

    Ok(Json(SaveResponse {
        id,
        updated_at: now,
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
}

#[derive(Serialize)]
pub struct CreateResponse {
    pub id: u64,
    pub created_at: i64,
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

    let slug = body.slug.trim();
    if slug.is_empty() {
        return Err(AdminError::BadRequest("slug must not be empty".to_owned()));
    }
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

    // No self to exclude on create: ANY post already at this slug is a clash.
    if slug_taken(&state, slug, None).await? {
        return Err(AdminError::Conflict(format!(
            "the slug {slug:?} is already used by another post"
        )));
    }

    let now = now_millis();
    let mut fields: FieldMap = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(Uuid::now_v7().to_string()));
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
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

    Ok(Json(CreateResponse {
        id: id.0,
        created_at: now,
    }))
}

/// Validate the initial status of a NEW post. A post is born a `Draft`; an
/// explicit status must be a legal departure from `Draft` (so `Published`,
/// `Pending`, `Scheduled` are allowed) and never `Trashed`. Absent/empty → `Draft`.
fn initial_status(raw: Option<&str>) -> Result<Status, AdminError> {
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

/// Whether `slug` is already held by a post OTHER than `exclude` (or by ANY post
/// when `exclude` is `None`). `Post.slug` is `@indexed`, NOT `@unique`, so the DB
/// won't reject a duplicate — but the public router resolves a slug to exactly ONE
/// post (`filter(slug ==, limit 1)`), so a collision would make one permanently
/// unreachable. `save` excludes the post being edited; `create` has no self.
async fn slug_taken(
    state: &AppState,
    slug: &str,
    exclude: Option<ObjectId>,
) -> Result<bool, AdminError> {
    // limit 2 so that `self` plus one other are both visible to the exclusion.
    Ok(state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(POST_TYPE),
            field: "slug".to_owned(),
            op: Compare::Eq,
            value: Value::String(slug.to_owned()),
            limit: Some(2),
        })
        .await?
        .iter()
        .any(|o| Some(o.id) != exclude))
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
async fn ensure_media_exists(state: &AppState, media_id: u64) -> Result<(), AdminError> {
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
        state.store.link(&edge, ObjectId(mid), FieldMap::new()).await?;
    }
    for (old, _) in existing {
        if Some(old.0) != media_id {
            state.store.unlink(&edge, old).await?;
        }
    }
    Ok(())
}

/// `updated_at` if present, else `created_at` — the instant the list sorts on.
fn effective_time(obj: &Object) -> Option<i64> {
    datetime_field(obj, "updated_at").or_else(|| datetime_field(obj, "created_at"))
}

/// Parse a snake_case status string into a [`Status`] via serde.
fn parse_status(s: &str) -> Option<Status> {
    serde_json::from_value::<Status>(serde_json::Value::String(s.to_owned())).ok()
}
