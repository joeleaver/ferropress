//! Post read/list/save for the editor. Every handler requires a session with the
//! `EditOthersContent` capability (Editor+); the MVP is a single trusted authoring
//! surface, so per-author ownership scoping (`EditOwnContent`) is a later slice.
//!
//! The wire body is Ferropress's own `BlockTree` JSON (`block_tree`), never rinch's
//! `DocNode` — the editor↔BlockTree conversion is client-side in the wasm SPA.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use ferropress_core::POST_TYPE;
use ferropress_core::block::BlockTree;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::role::Capability;
use ferropress_core::status::Status;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value, now_millis};

use super::{AdminError, AdminJson, AuthedUser, datetime_field, json_field, str_field};
use crate::AppState;

/// One row in the editor's post list.
#[derive(Serialize)]
pub struct PostSummary {
    pub id: u64,
    pub title: String,
    pub slug: String,
    pub status: String,
    /// Effective last-touched instant (updated_at, else created_at), epoch millis.
    pub updated_at: Option<i64>,
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
}

/// `GET /admin/api/posts` — every non-trashed post, most-recently-touched first.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<PostSummary>>, AdminError> {
    who.require(Capability::EditOthersContent)?;

    let mut posts: Vec<PostSummary> = state
        .store
        .scan(&TypeName::from(POST_TYPE))
        .await?
        .iter()
        .filter(|o| str_field(o, "status").as_deref() != Some(Status::Trashed.as_str()))
        .map(summary)
        .collect();

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
    who.require(Capability::EditOthersContent)?;

    let obj = state
        .store
        .get(&TypeName::from(POST_TYPE), ObjectId(id))
        .await?;

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
    who.require(Capability::EditOthersContent)?;

    let slug = body.slug.trim();
    if slug.is_empty() {
        return Err(AdminError::BadRequest("slug must not be empty".to_owned()));
    }
    let new_status = parse_status(&body.status)
        .ok_or_else(|| AdminError::BadRequest(format!("unknown status {:?}", body.status)))?;
    let tree = BlockTree::from_json_value(body.block_tree.clone())
        .map_err(|e| AdminError::BadRequest(format!("invalid block tree: {e}")))?;

    // Enforce slug uniqueness. `Post.slug` is `@indexed`, NOT `@unique`, so the DB
    // won't reject a duplicate — but the public router resolves a slug to exactly
    // ONE post (`filter(slug ==, limit 1)`), so a collision would make one post
    // permanently unreachable. Reject a slug already held by a DIFFERENT post.
    let clash = state
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
        .any(|o| o.id != ObjectId(id));
    if clash {
        return Err(AdminError::Conflict(format!(
            "the slug {slug:?} is already used by another post"
        )));
    }

    // The post must exist; its current status gates the transition.
    let current = state
        .store
        .get(&TypeName::from(POST_TYPE), ObjectId(id))
        .await?;
    let current_status =
        parse_status(&str_field(&current, "status").unwrap_or_default()).unwrap_or(Status::Draft);
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

    Ok(Json(SaveResponse {
        id,
        updated_at: now,
    }))
}

/// Build a list-row summary from a post object.
fn summary(obj: &Object) -> PostSummary {
    PostSummary {
        id: obj.id.0,
        title: str_field(obj, "title").unwrap_or_default(),
        slug: str_field(obj, "slug").unwrap_or_default(),
        status: str_field(obj, "status").unwrap_or_default(),
        updated_at: effective_time(obj),
    }
}

/// `updated_at` if present, else `created_at` — the instant the list sorts on.
fn effective_time(obj: &Object) -> Option<i64> {
    datetime_field(obj, "updated_at").or_else(|| datetime_field(obj, "created_at"))
}

/// Parse a snake_case status string into a [`Status`] via serde.
fn parse_status(s: &str) -> Option<Status> {
    serde_json::from_value::<Status>(serde_json::Value::String(s.to_owned())).ok()
}
