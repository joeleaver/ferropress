//! Island ↔ server wire types, the `gloo-net` fetch helpers, and one-time CSS
//! injection.
//!
//! The DTOs mirror the JSON the island API emits/accepts in
//! `ferropress-http/src/island/` — they are the SOURCE-OF-TRUTH-by-mirroring on
//! the client side. serde ignores unknown fields, so the client deliberately
//! reads only the subset it renders; a future shared `ferropress-island-api`
//! types crate would remove the mirroring (deferred). Every fetch error collapses
//! to a `String` because the islands only ever surface a generic state to the
//! user — the real cause goes to the browser console via the panic hook / logs.

use serde::{Deserialize, Serialize};

/// A public comment as listed by `GET /api/comments` (PII fields are never sent).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CommentDto {
    pub id: u64,
    #[serde(default)]
    pub author_name: String,
    #[serde(default)]
    pub author_url: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub parent_id: Option<u64>,
}

/// One hit from `GET /api/search`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SearchHit {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub excerpt: String,
}

/// The `POST /api/comments` body. Omitting `author_email` when blank lets the
/// server treat it as absent (it stores email only when present).
#[derive(Serialize)]
pub struct NewComment<'a> {
    pub slug: &'a str,
    pub author_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_email: Option<&'a str>,
    pub body: &'a str,
}

/// The slug of the current page, derived from the URL path (permalinks v1 are
/// flat `/<slug>`, the same trim `ferropress_serve::slug_from_path` applies
/// server-side). Empty when at the site root.
pub fn current_slug() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p.trim_matches('/').to_owned())
        .unwrap_or_default()
}

/// `GET /api/comments?slug=…` → the approved comments for `slug`.
pub async fn fetch_comments(slug: &str) -> Result<Vec<CommentDto>, String> {
    let resp = gloo_net::http::Request::get("/api/comments")
        .query([("slug", slug)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("comments request failed: {}", resp.status()));
    }
    resp.json::<Vec<CommentDto>>()
        .await
        .map_err(|e| e.to_string())
}

/// `POST /api/comments` — submit a new (pending) comment.
pub async fn post_comment(body: &NewComment<'_>) -> Result<(), String> {
    let resp = gloo_net::http::Request::post("/api/comments")
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("comment submit failed: {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/search?q=…` → matching published posts, best-ranked first.
pub async fn fetch_search(query: &str) -> Result<Vec<SearchHit>, String> {
    let resp = gloo_net::http::Request::get("/api/search")
        .query([("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("search request failed: {}", resp.status()));
    }
    resp.json::<Vec<SearchHit>>().await.map_err(|e| e.to_string())
}

/// A display date from an RFC3339 timestamp: just the `YYYY-MM-DD` date part
/// (no date library in the wasm bundle for v1; relative time is a refinement).
pub fn fmt_date(created_at: &str) -> String {
    created_at
        .split('T')
        .next()
        .unwrap_or(created_at)
        .to_owned()
}

/// Inject the island stylesheet into `<head>` exactly once per page (guarded by
/// element id). The islands use raw HTML elements + these classes (rather than
/// the heavyweight rinch component library) so they blend into any host theme;
/// the palette is driven by rinch's theme CSS variables (injected by
/// `ThemeProviderProps`) with neutral fallbacks.
pub fn inject_island_styles() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if doc.get_element_by_id(STYLE_ID).is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    let _ = style.set_attribute("id", STYLE_ID);
    style.set_text_content(Some(ISLAND_CSS));
    if let Some(head) = doc.head() {
        let _ = head.append_child(&style);
    }
}

const STYLE_ID: &str = "fp-islands-style";

/// Island stylesheet. Ported from the validated HTML-first mockup
/// (`design/mockup.html`); references rinch theme vars with standalone fallbacks.
const ISLAND_CSS: &str = r#"
.fp-island {
  --fp-fg: var(--rinch-color-text, #1f2328);
  --fp-muted: var(--rinch-color-dimmed, #6b7280);
  --fp-border: var(--rinch-color-gray-3, #e5e7eb);
  --fp-surface: var(--rinch-color-body, #fff);
  --fp-surface-2: var(--rinch-color-gray-1, #f6f7f9);
  --fp-accent: var(--rinch-color-primary, #2f6f4f);
  --fp-danger: var(--rinch-color-red, #b42318);
  --fp-radius: 8px;
  color: var(--fp-fg);
  font-size: 0.95rem;
  line-height: 1.55;
}
.fp-h2 { font-size: 1.05rem; font-weight: 650; letter-spacing: -0.01em; margin: 0 0 16px; }
.fp-count { color: var(--fp-muted); font-weight: 400; font-size: 0.85rem; margin-left: 6px; }
.fp-state { color: var(--fp-muted); padding: 8px 0; }
.fp-state.err { color: var(--fp-danger); }
.fp-thread { display: grid; gap: 16px; }
.fp-comment { display: grid; gap: 4px; }
.fp-reply { margin-left: 6px; padding-left: 16px; border-left: 2px solid var(--fp-border); }
.fp-comment-head { display: flex; align-items: baseline; gap: 8px; }
.fp-author { font-weight: 600; }
.fp-author a { color: var(--fp-accent); text-decoration: none; }
.fp-author a:hover { text-decoration: underline; }
.fp-time { color: var(--fp-muted); font-size: 0.8rem; }
.fp-body { margin: 0; white-space: pre-wrap; }
.fp-form { margin-top: 28px; padding-top: 24px; border-top: 1px solid var(--fp-border); display: grid; gap: 10px; }
.fp-form-title { font-weight: 600; }
.fp-row { display: grid; gap: 10px; grid-template-columns: 1fr 1fr; }
.fp-field { display: grid; gap: 4px; }
.fp-field label { font-size: 0.8rem; color: var(--fp-muted); }
.fp-input, .fp-textarea {
  font: inherit; color: inherit; background: var(--fp-surface);
  border: 1px solid var(--fp-border); border-radius: var(--fp-radius); padding: 8px 10px;
}
.fp-input:focus, .fp-textarea:focus { outline: 2px solid var(--fp-accent); outline-offset: 1px; border-color: var(--fp-accent); }
.fp-textarea { min-height: 84px; resize: vertical; }
.fp-btn {
  justify-self: start; font: inherit; font-weight: 550; cursor: pointer;
  background: var(--fp-accent); color: #fff; border: 0; border-radius: var(--fp-radius); padding: 9px 16px;
}
.fp-btn:hover { filter: brightness(1.05); }
.fp-btn[disabled] { opacity: 0.6; cursor: default; }
.fp-confirm { margin-top: 28px; padding: 14px 16px; border-radius: var(--fp-radius); background: var(--fp-surface-2); border: 1px solid var(--fp-border); }
.fp-confirm strong { display: block; }
.fp-search-bar { display: flex; gap: 8px; }
.fp-search-bar .fp-input { flex: 1; }
.fp-results { margin: 16px 0 0; padding: 0; list-style: none; display: grid; gap: 14px; }
.fp-result { display: grid; gap: 2px; }
.fp-result a { color: var(--fp-accent); text-decoration: none; font-weight: 600; }
.fp-result a:hover { text-decoration: underline; }
.fp-excerpt { color: var(--fp-muted); font-size: 0.9rem; }
"#;
