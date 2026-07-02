//! Reference action-hook plugin: a **backlink index** — the first plugin to use
//! the `content:write` capability (alongside `content:read`).
//!
//! The change-feed → action bridge dispatches `post.created` / `post.updated` to
//! `on_change`. This plugin reads the changed post's body straight from the action
//! payload (`fields.block_tree`, a native JSON scalar), collects the `[[targets]]`
//! in its `wiki` blocks, resolves each via `fp_lookup_slug` (read_store), and
//! records a backlink on each resolvable target via `fp_set_meta` (write_store).
//!
//! The backlink is keyed by the SOURCE post's id (`from:<id>`), so N different
//! pages linking to the same target accumulate N distinct meta keys — the plugin
//! never needs to read the target's current backlinks back (there is no read-meta
//! host function, and this keeps each source's contribution independent). The full
//! "pages that link here" set for a target is the union of its `from:*` keys.
//!
//! v1 limitation: if a source post later REMOVES a `[[link]]`, the stale
//! `from:<id>` key on the old target is not cleaned up (there is no delete-meta
//! capability yet). Documented; a full impl would diff old vs new targets.
//!
//! Deny-by-default is structural: without BOTH grants + wired backends the
//! `fp_lookup_slug` / `fp_set_meta` imports are absent and the plugin fails to
//! instantiate — it cannot reach content it was not granted.

use extism_pdk::*;
use serde::Deserialize;
use serde_json::Value;

// The `content:read` + `content:write` host functions. `fp_lookup_slug` returns a
// published entity's JSON (`{id, type, title, slug}`) or `null`; `fp_set_meta`
// takes a JSON request `{type, id, key, value}` and returns `true`/`false`.
#[host_fn]
extern "ExtismHost" {
    fn fp_lookup_slug(slug: String) -> String;
    fn fp_set_meta(req: String) -> String;
}

/// The subset of the host's `PublishedRef` this plugin needs to address a target.
#[derive(Deserialize)]
struct PublishedRef {
    id: u64,
    #[serde(rename = "type")]
    type_name: String,
}

/// Index the backlinks of a created/updated Post. The bridge payload is
/// `{ version, type, kind, object_id, fields? }`; the post body rides in
/// `fields.block_tree`.
#[plugin_fn]
pub fn on_change(Json(change): Json<Value>) -> FnResult<()> {
    // Only Post create/update carry a body worth indexing.
    if change.get("type").and_then(Value::as_str) != Some("Post") {
        return Ok(());
    }
    let kind = change.get("kind").and_then(Value::as_str).unwrap_or_default();
    if kind != "create" && kind != "update" {
        return Ok(());
    }
    let Some(source_id) = change.get("object_id").and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some(fields) = change.get("fields") else {
        return Ok(());
    };
    let source_slug = fields.get("slug").and_then(Value::as_str).unwrap_or_default();
    // block_tree may be absent (e.g. an update that didn't carry it) — then there
    // is nothing to (re)index on this change.
    let Some(block_tree) = fields.get("block_tree") else {
        return Ok(());
    };

    // Gather the text of every `wiki` block, then the `[[targets]]` within it.
    let mut wiki_text = String::new();
    if let Some(blocks) = block_tree.get("blocks").and_then(Value::as_array) {
        collect_wiki_text(blocks, &mut wiki_text);
    }
    let targets = extract_targets(&wiki_text);

    // Record a backlink on each resolvable (published) target, keyed by source id.
    for slug in targets {
        let json = unsafe { fp_lookup_slug(slug)? };
        let Some(target) = serde_json::from_str::<Option<PublishedRef>>(&json).unwrap_or(None) else {
            continue; // red link: target isn't a published page — nothing to link to.
        };
        let req = serde_json::json!({
            "type": target.type_name,
            "id": target.id,
            "key": format!("from:{source_id}"),
            "value": source_slug,
        });
        // Best-effort: a denied/failed write must not abort indexing the rest.
        let _ = unsafe { fp_set_meta(req.to_string())? };
    }
    Ok(())
}

/// Recursively append the text of every `wiki` custom block (a `Custom` block whose
/// owning plugin is `wiki`) to `out`.
fn collect_wiki_text(blocks: &[Value], out: &mut String) {
    for b in blocks {
        if let Some(kind) = b.get("kind") {
            let is_wiki = kind.get("type").and_then(Value::as_str) == Some("custom")
                && kind.get("plugin").and_then(Value::as_str) == Some("wiki");
            if is_wiki
                && let Some(text) = kind
                    .get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
            {
                out.push_str(text);
                out.push('\n');
            }
        }
        if let Some(children) = b.get("children").and_then(Value::as_array) {
            collect_wiki_text(children, out);
        }
    }
}

/// Extract the unique, slugified `[[target]]` links from wiki text, order-preserving.
/// `[[Target|Label]]` keys on the target; an unmatched `[[` ends the scan.
fn extract_targets(text: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        let target = inner.split_once('|').map(|(t, _)| t).unwrap_or(inner).trim();
        let slug = slugify(target);
        if !slug.is_empty() && !targets.contains(&slug) {
            targets.push(slug);
        }
        rest = &after[end + 2..];
    }
    targets
}

/// Slugify a wiki target the same way the wiki plugin (and content slugs) do:
/// lowercase ASCII alphanumerics, runs of space/`-`/`_` collapse to one `-`, other
/// characters dropped, no leading/trailing `-`.
fn slugify(s: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else if c == '-' || c == '_' || c.is_whitespace() {
            pending_dash = true;
        }
    }
    slug
}
