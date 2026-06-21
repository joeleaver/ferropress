//! Reference comment-moderation plugin: a **spam filter**.
//!
//! The host (`ferropress-plugin-host`) dispatches the `comment.create` **filter**
//! hook to the `comment_create` export with the proposed comment as a JSON object
//! (`{ slug, author_name, author_email?, author_url?, body, parent_id?,
//! user_agent?, status }`, `status` defaulting to `"pending"`). This plugin runs a
//! keyword + link-flood heuristic over the body/author fields and, when the
//! comment looks like spam, returns the SAME payload with `status` changed to
//! `"spam"`. The host reads `status` back and persists the comment with it — so a
//! flagged comment is held out of the public (approved-only) listing.
//!
//! It is pure-compute and capability-zero: it reads only its input payload (no
//! store, no network), exactly the WordPress "Akismet-style" moderation shape but
//! sandboxed. As a faithful filter it passes the rest of the payload through
//! untouched (only `status` is ever written), so the host's read-back stays robust
//! to fields it does not yet send.

use extism_pdk::*;
use serde_json::Value;

/// Lowercased substrings that, if present in the body / author name / author URL,
/// classify the comment as spam. A deliberately small, illustrative list — a real
/// deployment would ship a larger/updatable corpus (or a capability-granted plugin
/// calling an external service).
const SPAM_KEYWORDS: &[&str] = &[
    "viagra",
    "cialis",
    "casino",
    "porn",
    "xxx",
    "free money",
    "make money fast",
    "crypto giveaway",
    "weight loss",
    "payday loan",
    "click here",
    "buy now",
    "100% free",
];

/// More than this many URLs in the body is treated as link-farming spam. A normal
/// comment cites a source or two; a wall of links almost never is.
const MAX_LINKS: usize = 2;

/// The `comment.create` filter: classify the proposed comment, marking it `spam`
/// when the heuristics fire, and return the (possibly updated) payload.
#[plugin_fn]
pub fn comment_create(Json(mut payload): Json<Value>) -> FnResult<Json<Value>> {
    if is_spam(&payload)
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("status".to_owned(), Value::String("spam".to_owned()));
    }
    Ok(Json(payload))
}

/// Whether the payload looks like spam: any spam keyword across the visible text,
/// or more than [`MAX_LINKS`] URLs in the body.
fn is_spam(payload: &Value) -> bool {
    let body = str_field(payload, "body");
    let name = str_field(payload, "author_name");
    let url = str_field(payload, "author_url");

    let haystack = format!("{body}\n{name}\n{url}").to_lowercase();
    if SPAM_KEYWORDS.iter().any(|kw| haystack.contains(kw)) {
        return true;
    }

    let body_lower = body.to_lowercase();
    let links = body_lower.matches("http://").count() + body_lower.matches("https://").count();
    links > MAX_LINKS
}

/// Read a string field off the payload object, or `""` if absent / not a string.
fn str_field<'a>(payload: &'a Value, field: &str) -> &'a str {
    payload.get(field).and_then(Value::as_str).unwrap_or("")
}
