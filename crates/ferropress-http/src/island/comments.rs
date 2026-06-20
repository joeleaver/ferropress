//! The **live-comments** island API: list the approved comments on a published
//! page, and accept a new comment (held for moderation).
//!
//! Both endpoints key off the public **slug** and resolve it through
//! [`ferropress_serve::resolve_published_entity`] — the SAME published-Post-then-
//! Page rule the page read path uses. A comment can therefore only ever attach to
//! (or be listed for) content that is actually publicly served; drafts and
//! unknown slugs are a clean 404.
//!
//! ## Moderation
//!
//! A newly POSTed comment is created with [`CommentStatus::Pending`] — it is NOT
//! publicly visible until a moderator approves it. The list endpoint returns ONLY
//! [`CommentStatus::Approved`] comments, so pending / spam / trashed comments
//! never reach the public island. This mirrors WordPress's default and keeps the
//! unauthenticated POST endpoint from being a direct publish vector.
//!
//! ## Threading
//!
//! A comment may carry a `parent` (another comment ON THE SAME entity), and the
//! list DTO exposes that `parent_id` so the island can render a thread. Parent ids
//! for the whole listing are resolved in ONE batched
//! [`get_links_many`](ferropress_core::store::RhypeStore::get_links_many) call
//! (not N+1 traversals).

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use ferropress_core::error::CoreError;
use ferropress_core::query::Edge;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value};
use ferropress_core::{COMMENT_TYPE, CommentStatus, PAGE_TYPE, POST_TYPE};

use crate::AppState;
use crate::island::{ApiError, ApiJson, ApiQuery, status_is, string_field};

/// Defensive field-length caps — the POST endpoint is public and unauthenticated.
const MAX_BODY_LEN: usize = 10_000;
const MAX_NAME_LEN: usize = 100;
/// RFC 5321 max email length.
const MAX_EMAIL_LEN: usize = 254;
const MAX_URL_LEN: usize = 2_048;
/// User-Agent is stored verbatim for moderation; cap it so a hostile client can't
/// store an unbounded header.
const MAX_UA_LEN: usize = 512;
/// Hard cap on APPROVED comments returned for one entity in v1 (pagination is a
/// later increment). The cap is applied AFTER the approved-status filter, to a
/// chronologically-sorted set, keeping the MOST RECENT `MAX_COMMENTS` approved
/// comments — so older non-approved comments can never push approved content out
/// of the window. A reply whose approved parent falls outside the kept window has
/// its `parent_id` dropped (rendered top-level) so the returned thread graph stays
/// closed.
const MAX_COMMENTS: usize = 500;

/// The relation field that lists an entity's comments. Declared
/// `@inverse(Comment.post)` on `Post` and `@inverse(Comment.page)` on `Page`, so
/// the same name traverses either entity's thread via the reverse-edge index.
const COMMENTS_FIELD: &str = "comments";
/// The `Comment.parent` forward relation (threading).
const PARENT_FIELD: &str = "parent";

/// `GET /api/comments?slug=…` query. `slug` is modeled as `Option` so a
/// *missing* param is not an extractor rejection: the handler treats absent and
/// blank identically and returns one uniform 400 JSON error.
#[derive(Deserialize)]
pub struct ListQuery {
    slug: Option<String>,
}

/// One comment as the public island sees it. `author_email` / `author_ip` /
/// `user_agent` are moderation-only PII and are deliberately NOT exposed.
#[derive(Serialize)]
pub struct CommentDto {
    id: u64,
    author_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_url: Option<String>,
    body: String,
    /// RFC3339 UTC timestamp.
    created_at: String,
    /// The comment this one replies to, if any (threading).
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<u64>,
}

/// `POST /api/comments` body. The required text fields are `Option` so a missing
/// key becomes a uniform in-handler 400 ("… is required") rather than axum's
/// plain-text 422 deserialize rejection; presence is validated below.
#[derive(Deserialize)]
pub struct CreateBody {
    slug: Option<String>,
    author_name: Option<String>,
    author_email: Option<String>,
    author_url: Option<String>,
    body: Option<String>,
    /// Optional parent comment id (a reply). Must reference a comment on the same
    /// entity.
    parent_id: Option<u64>,
}

/// `POST /api/comments` response (201). Communicates that the comment is held for
/// moderation rather than published immediately.
#[derive(Serialize)]
pub struct CreateResponse {
    id: u64,
    status: &'static str,
    message: &'static str,
}

/// `GET /api/comments?slug=…` — list the APPROVED comments on the published
/// entity behind `slug`, oldest first, each with its `parent_id` for threading.
pub async fn list(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<CommentDto>>, ApiError> {
    let slug = query.slug.as_deref().unwrap_or("").trim();
    if slug.is_empty() {
        return Err(ApiError::BadRequest("missing or empty `slug`".to_owned()));
    }

    // Only PUBLISHED content has a public comment thread.
    let (entity_type, entity) = ferropress_serve::resolve_published_entity(&state.store, slug)
        .await?
        .ok_or(ApiError::NotFound)?;

    // 1. Gather this entity's comment ids via the inverse-edge traversal.
    let links = state
        .store
        .get_links(&Edge {
            type_name: TypeName::from(entity_type),
            id: entity.id,
            field: COMMENTS_FIELD.to_owned(),
        })
        .await?;
    if links.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let comment_ids: Vec<ObjectId> = links.into_iter().map(|(id, _edge)| id).collect();

    // 2. Materialize the comments and keep ONLY the approved ones. The status
    //    filter MUST run before any cap: capping the raw id list first would let a
    //    flood of older spam/pending comments push genuinely-approved (newer)
    //    comments out of the window and silently drop them from public output.
    let comment_type = TypeName::from(COMMENT_TYPE);
    let approved: Vec<_> = state
        .store
        .get_many(&comment_type, &comment_ids)
        .await?
        .into_iter()
        .filter(|obj| status_is(obj, CommentStatus::Approved.as_str()))
        .collect();
    if approved.is_empty() {
        return Ok(Json(Vec::new()));
    }

    // 3. Order chronologically and cap to the MOST RECENT `MAX_COMMENTS` approved
    //    comments (drop the oldest overflow — never approved content behind
    //    non-approved rows; real pagination is a later increment). Decorate with
    //    the `created_at` sort key so the comparator reads each field once.
    //    `created_at` is RFC3339, which sorts lexicographically == chronologically;
    //    id is a stable tiebreak.
    let mut keyed: Vec<(String, u64, _)> = approved
        .into_iter()
        .map(|obj| {
            (
                string_field(&obj, "created_at").unwrap_or_default(),
                obj.id.0,
                obj,
            )
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    if keyed.len() > MAX_COMMENTS {
        let overflow = keyed.len() - MAX_COMMENTS;
        keyed.drain(0..overflow);
    }
    let ordered: Vec<_> = keyed.into_iter().map(|(_, _, obj)| obj).collect();

    // 4. Resolve every comment's parent in ONE batched traversal. The groups come
    //    back in the SAME order as `approved_ids`, built from `ordered`'s order, so
    //    the `zip` below stays aligned. `approved_set` lets the DTO mapping drop a
    //    `parent_id` that points outside the returned (approved) set.
    let approved_ids: Vec<ObjectId> = ordered.iter().map(|obj| obj.id).collect();
    let approved_set: HashSet<u64> = approved_ids.iter().map(|ObjectId(n)| *n).collect();
    let parent_groups = state
        .store
        .get_links_many(&comment_type, &approved_ids, PARENT_FIELD)
        .await?;

    // 5. Map to DTOs (a comment has at most one parent — a 1:1 forward relation).
    //    Only surface `parent_id` when the parent is itself in the returned
    //    approved set, so the public thread graph is closed: a reply whose parent
    //    is pending / spam / trashed renders as top-level rather than dangling.
    let dtos: Vec<CommentDto> = ordered
        .into_iter()
        .zip(parent_groups)
        .map(|(obj, parents)| {
            let parent_id = parents
                .into_iter()
                .next()
                .map(|ObjectId(n)| n)
                .filter(|pid| approved_set.contains(pid));
            CommentDto {
                id: obj.id.0,
                author_name: string_field(&obj, "author_name").unwrap_or_default(),
                author_url: string_field(&obj, "author_url").filter(|s| !s.is_empty()),
                // Prefer the plain `body`; fall back to the derived `plaintext`.
                body: string_field(&obj, "body")
                    .filter(|s| !s.is_empty())
                    .or_else(|| string_field(&obj, "plaintext"))
                    .unwrap_or_default(),
                created_at: string_field(&obj, "created_at").unwrap_or_default(),
                parent_id,
            }
        })
        .collect();

    Ok(Json(dtos))
}

/// `POST /api/comments` — accept a new comment on the published entity behind
/// `slug`. Created as [`CommentStatus::Pending`] (held for moderation), so it does
/// not appear in [`list`] until approved.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreateBody>,
) -> Result<(StatusCode, Json<CreateResponse>), ApiError> {
    // --- validate inputs (messages are author-written and client-safe) ---
    let slug = body.slug.as_deref().unwrap_or("").trim();
    if slug.is_empty() {
        return Err(ApiError::BadRequest("`slug` is required".to_owned()));
    }

    let author_name = body.author_name.as_deref().unwrap_or("").trim();
    if author_name.is_empty() {
        return Err(ApiError::BadRequest("`author_name` is required".to_owned()));
    }
    if author_name.chars().count() > MAX_NAME_LEN {
        return Err(ApiError::BadRequest(format!(
            "`author_name` exceeds {MAX_NAME_LEN} characters"
        )));
    }
    // No control characters (incl. CR/LF) in a stored name.
    if has_control(author_name) {
        return Err(ApiError::BadRequest(
            "`author_name` contains control characters".to_owned(),
        ));
    }

    let comment_body = body.body.as_deref().unwrap_or("").trim();
    if comment_body.is_empty() {
        return Err(ApiError::BadRequest("`body` is required".to_owned()));
    }
    if comment_body.chars().count() > MAX_BODY_LEN {
        return Err(ApiError::BadRequest(format!(
            "`body` exceeds {MAX_BODY_LEN} characters"
        )));
    }

    let author_email = match opt_trimmed(&body.author_email) {
        Some(email) => {
            if email.chars().count() > MAX_EMAIL_LEN || !looks_like_email(email) {
                return Err(ApiError::BadRequest(
                    "`author_email` is not a valid email".to_owned(),
                ));
            }
            Some(email.to_owned())
        }
        None => None,
    };

    let author_url = match opt_trimmed(&body.author_url) {
        Some(url) => {
            // Reject control/whitespace and a non-http(s) scheme. The scheme guard
            // is a stored-XSS defense: `author_url` is echoed in public list output,
            // so a `javascript:` URL must never be persisted.
            if url.chars().count() > MAX_URL_LEN
                || has_control_or_space(url)
                || !(url.starts_with("http://") || url.starts_with("https://"))
            {
                return Err(ApiError::BadRequest(
                    "`author_url` must be an http(s) URL".to_owned(),
                ));
            }
            Some(url.to_owned())
        }
        None => None,
    };

    // --- resolve the target (published Post / Page) ---
    let (entity_type, entity) = ferropress_serve::resolve_published_entity(&state.store, slug)
        .await?
        .ok_or(ApiError::NotFound)?;

    // --- if it is a reply, the parent must be an existing comment on THIS entity ---
    if let Some(parent_id) = body.parent_id {
        let siblings = state
            .store
            .get_links(&Edge {
                type_name: TypeName::from(entity_type),
                id: entity.id,
                field: COMMENTS_FIELD.to_owned(),
            })
            .await?;
        let belongs = siblings.iter().any(|(ObjectId(id), _)| *id == parent_id);
        if !belongs {
            return Err(ApiError::BadRequest(
                "`parent_id` does not reference a comment on this content".to_owned(),
            ));
        }
    }

    // --- build the comment (PENDING moderation) ---
    let created_at = OffsetDateTime::now_utc().format(&Rfc3339).map_err(|e| {
        // A formatting failure is an internal fault, not a client error.
        ApiError::Internal(CoreError::Store(format!("timestamp format failed: {e}")))
    })?;
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(MAX_UA_LEN).collect::<String>())
        .filter(|s| !s.is_empty());

    let mut fields: FieldMap = FieldMap::new();
    fields.insert(
        "status".to_owned(),
        Value::String(CommentStatus::Pending.as_str().to_owned()),
    );
    // The island form is plain text: store it as `body` and mirror it into the
    // `@vectorize` source `plaintext` so moderation search sees it.
    fields.insert("body".to_owned(), Value::String(comment_body.to_owned()));
    fields.insert(
        "plaintext".to_owned(),
        Value::String(comment_body.to_owned()),
    );
    fields.insert(
        "author_name".to_owned(),
        Value::String(author_name.to_owned()),
    );
    if let Some(email) = author_email {
        fields.insert("author_email".to_owned(), Value::String(email));
    }
    if let Some(url) = author_url {
        fields.insert("author_url".to_owned(), Value::String(url));
    }
    if let Some(ua) = user_agent {
        fields.insert("user_agent".to_owned(), Value::String(ua));
    }
    fields.insert("created_at".to_owned(), Value::String(created_at));

    // Inline relation: the comment attaches to EXACTLY ONE of post/page (the
    // resolved type), enforcing the schema's app-level mutual exclusion. Relation
    // values are the target id as an integer (the engine stages the forward +
    // reverse edges in the same txn as the object).
    let relation_field = match entity_type {
        POST_TYPE => "post",
        PAGE_TYPE => "page",
        // `resolve_published_entity` only ever returns POST_TYPE / PAGE_TYPE.
        other => {
            return Err(ApiError::Internal(CoreError::Store(format!(
                "unexpected entity type {other} from slug resolution"
            ))));
        }
    };
    fields.insert(relation_field.to_owned(), Value::U64(entity.id.0));
    if let Some(parent_id) = body.parent_id {
        fields.insert(PARENT_FIELD.to_owned(), Value::U64(parent_id));
    }

    let id = state
        .store
        .create(&TypeName::from(COMMENT_TYPE), fields)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateResponse {
            id: id.0,
            status: CommentStatus::Pending.as_str(),
            message: "comment received and awaiting moderation",
        }),
    ))
}

/// Trim an optional string and drop it if it is empty after trimming.
fn opt_trimmed(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Whether `s` contains any control character (incl. CR/LF). Stored values must
/// be free of control bytes so a downstream consumer (e.g. a moderation mailer)
/// can never be tricked into header injection by a CR/LF in the data.
fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// Whether `s` contains any control character OR whitespace. Used for fields that
/// must be a single unbroken token (email, URL).
fn has_control_or_space(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Minimal email sanity check: exactly one `@`, a non-empty local part, a domain
/// that contains a dot and is not dot-bounded, and NO control/whitespace anywhere
/// (so a CR/LF can't smuggle into the stored PII). Deliberately permissive on the
/// structural side — full RFC 5322 validation is neither possible with a simple
/// check nor useful here; the goal is to reject obvious garbage and unsafe bytes,
/// not to guarantee deliverability.
fn looks_like_email(s: &str) -> bool {
    if has_control_or_space(s) {
        return false;
    }
    let mut parts = s.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        _ => false,
    }
}
