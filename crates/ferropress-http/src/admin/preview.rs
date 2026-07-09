//! The WordPress-style **draft preview**: render one (possibly unpublished) post
//! through the REAL public theme, in a new browser tab, without publishing or
//! caching it.
//!
//! Unlike the JSON editor API in [`posts`](super::posts), this route returns a full
//! HTML document. It reuses the exact public serve pipeline via
//! [`ferropress_serve::render_preview`] — the one shared block renderer, the media
//! rewrite, and the live site-settings chrome — so what the author previews is
//! byte-for-byte what publishing produces, plus a preview banner and a forced
//! `noindex`. The two differences from the public
//! read path are deliberate and confined to this authenticated route: it skips the
//! publish gate (a draft renders instead of 404ing) and it never touches the
//! prerender cache.
//!
//! Authorization mirrors [`posts::get_one`](super::posts): a valid session is
//! required (the [`AuthedUser`] extractor → 401), and access to the specific post is
//! gated by [`AuthedUser::require_post_access`], which masks a denial as **404** so a
//! lower role cannot use the route as an existence oracle for another author's
//! unpublished drafts. It is served under `/admin` so the `Path=/admin` session
//! cookie is sent on the new-tab navigation.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};

use ferropress_core::POST_TYPE;
use ferropress_core::value::{ObjectId, TypeName};
use ferropress_serve::Resolved;

use super::{AdminError, AuthedUser, posts};
use crate::AppState;

/// `GET /admin/preview/{id}` — render post `id` (any status) through the public theme
/// in preview mode. The response carries `Cache-Control: no-store` and
/// `X-Robots-Tag: noindex, nofollow` so this authenticated, unpublished view is never
/// stored by a shared cache nor indexed by a crawler (belt-and-suspenders with the
/// `noindex` meta the preview chrome already emits).
pub async fn preview(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<Response, AdminError> {
    // Load first (a missing post is 404 regardless of who asks), then authorize against
    // its author: Editor+ may preview any post, a lower role only their own. Denial is
    // masked as 404 (not 403) so this can't distinguish another author's draft from a
    // missing id — matching the scoped list + `get_one`. Note: preview does NOT backfill
    // a null author (it is a read, not "open to edit"; the editor already backfilled on
    // the `get_one` that loaded this post).
    let obj = state
        .store
        .get(&TypeName::from(POST_TYPE), ObjectId(id))
        .await?;
    let author = posts::author_of(&state, ObjectId(id)).await?;
    who.require_post_access(author)?;

    match ferropress_serve::render_preview(
        &state.store,
        &state.theme,
        state.custom.as_ref(),
        &state.settings.current(),
        &state.authors.current(),
        POST_TYPE,
        &obj,
    )
    .await
    {
        Resolved::Found(html) => {
            let mut resp = Html(html).into_response();
            let headers = resp.headers_mut();
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(
                "X-Robots-Tag",
                HeaderValue::from_static("noindex, nofollow"),
            );
            Ok(resp)
        }
        // We already hold the object, so `render_preview` never reports it missing; a
        // `NotFound` here would be a render-layer surprise, so surface it as 404.
        Resolved::NotFound => Err(AdminError::NotFound),
        Resolved::Error(e) => Err(AdminError::Internal(e)),
    }
}
