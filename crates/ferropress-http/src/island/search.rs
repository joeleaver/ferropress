//! The **semantic-search** island API: a query string in, the best-matching
//! published posts out, ranked by vector similarity.
//!
//! The match runs over `Post.search` — the `@vectorize`d field whose source is the
//! server-derived `plaintext` — via
//! [`RhypeStore::vector_search`](ferropress_core::store::RhypeStore::vector_search).
//!
//! ## Runtime requirement (ONNX)
//!
//! `vector_search` embeds the query text with the same model that vectorized the
//! content (`all-MiniLM-L6-v2`, 384-dim). rhypedb-engine is built `onnx-dynamic`,
//! so the ONNX Runtime shared library is **dlopen-ed at runtime** via the
//! `ORT_DYLIB_PATH` environment variable, and the content must already be embedded
//! (the embed worker runs in the background after a write). Where the model is not
//! reachable the store returns an error, which surfaces here as a logged 500 — the
//! endpoint is wired and correct, but real results require the model at runtime.
//! The handler's mapping / ranking / publish-gating logic is exercised in tests
//! with a store double, independent of ONNX.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use ferropress_core::query::VectorQuery;
use ferropress_core::value::{ObjectId, TypeName};
use ferropress_core::{POST_TYPE, Status};

use crate::AppState;
use crate::island::{ApiError, ApiQuery, status_is, string_field};

/// The `@vectorize`d field on `Post` (and every content type) per the SDL.
const SEARCH_FIELD: &str = "search";
/// Default result count when the client does not specify `k`.
const DEFAULT_K: usize = 10;
/// Upper bound on `k` (a public endpoint must not let a client request an
/// arbitrarily large scan).
const MAX_K: usize = 50;
/// HNSW search breadth (`ef`): a fixed, generous v1 value. Higher = better recall
/// at more cost; per-deployment tuning is a later increment.
const SEARCH_EF: usize = 64;
/// Cap the query text length (defensive; embedding cost scales with input).
const MAX_QUERY_LEN: usize = 1_000;

/// `GET /api/search?q=…&k=…` query. `q` is `Option` so a *missing* param is not
/// an extractor rejection: the handler treats absent and blank identically and
/// returns one uniform 400 JSON error.
#[derive(Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    k: Option<usize>,
}

/// One search result. `score` is the backend similarity score (higher = closer);
/// it is surfaced so the island can show / debug ranking.
#[derive(Serialize)]
pub struct SearchHit {
    id: u64,
    slug: String,
    title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    excerpt: String,
    score: f32,
}

/// `GET /api/search?q=…` — semantic search over published posts, ranked by vector
/// similarity. Drafts are filtered out so unpublished content can never surface.
pub async fn search(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<SearchParams>,
) -> Result<Json<Vec<SearchHit>>, ApiError> {
    let query_text = params.q.as_deref().unwrap_or("").trim();
    if query_text.is_empty() {
        return Err(ApiError::BadRequest("missing or empty `q`".to_owned()));
    }
    if query_text.chars().count() > MAX_QUERY_LEN {
        return Err(ApiError::BadRequest(format!(
            "`q` exceeds {MAX_QUERY_LEN} characters"
        )));
    }
    let k = params.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K);

    let post_type = TypeName::from(POST_TYPE);

    // Semantic search over Post.search. Requires the ONNX model at runtime (see
    // the module docs); a missing model surfaces as a store error -> 500.
    let hits = state
        .store
        .vector_search(VectorQuery {
            type_name: post_type.clone(),
            vector_field: SEARCH_FIELD.to_owned(),
            query_text: query_text.to_owned(),
            k,
            ef: SEARCH_EF,
            rerank: false,
            restrict: None,
        })
        .await?;
    if hits.is_empty() {
        return Ok(Json(Vec::new()));
    }

    // Resolve the scored ids to posts. `get_many` returns objects sorted+deduped
    // by id (NOT in score order), so index by id to restore the ranking below.
    let ids: Vec<ObjectId> = hits.iter().map(|hit| hit.id).collect();
    let by_id: HashMap<u64, _> = state
        .store
        .get_many(&post_type, &ids)
        .await?
        .into_iter()
        .map(|obj| (obj.id.0, obj))
        .collect();

    // Walk the hits in score order, keeping only PUBLISHED posts (a draft must
    // never surface in public search) that still resolve.
    let results: Vec<SearchHit> = hits
        .into_iter()
        .filter_map(|scored| {
            let obj = by_id.get(&scored.id.0)?;
            if !status_is(obj, Status::Published.as_str()) {
                return None;
            }
            Some(SearchHit {
                id: obj.id.0,
                slug: string_field(obj, "slug").unwrap_or_default(),
                title: string_field(obj, "title").unwrap_or_default(),
                excerpt: string_field(obj, "excerpt").unwrap_or_default(),
                score: scored.score,
            })
        })
        .collect();

    Ok(Json(results))
}
