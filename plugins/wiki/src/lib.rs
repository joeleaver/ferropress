//! Reference custom-block plugin: a **wiki block** that resolves `[[WikiLinks]]`.
//!
//! This is the first reference plugin to use a CAPABILITY. It is granted
//! `content:read` (`read_store` in `plugin.toml`), so the host wires the
//! `fp_lookup_slug` host function into its sandbox. The block body is plain text
//! with wiki links — `[[Page Title]]` or `[[Page Title|shown label]]`. For each,
//! the plugin slugifies the target, asks the host whether a PUBLISHED page exists
//! at that slug, and renders:
//!   * an existing target → `<a class="wiki-link" …>` (with the real page title as
//!     a tooltip), or
//!   * a missing target → `<a class="wiki-link wiki-link-new" …>` (a "red link").
//!
//! All text and attributes are escaped here (the host emits plugin output raw).
//! Deny-by-default is structural: WITHOUT the `read_store` grant the host function
//! is absent and this plugin would fail to instantiate — it cannot reach content
//! it was not granted.

use extism_pdk::*;
use serde::Deserialize;

// The `content:read` host function: given a slug, returns the JSON of the
// published entity at that slug (`{id, type, title, slug}`), or the literal
// `null`. Linked from the default host namespace, which the host registers it in.
#[host_fn]
extern "ExtismHost" {
    fn fp_lookup_slug(slug: String) -> String;
}

/// The host's block-render input envelope: `{ name, data }`.
#[derive(Deserialize)]
struct BlockInput {
    #[serde(default)]
    name: String,
    #[serde(default)]
    data: WikiData,
}

/// The `wiki` block's attributes: the wiki-markup body text.
#[derive(Default, Deserialize)]
struct WikiData {
    #[serde(default)]
    text: String,
}

/// What `fp_lookup_slug` returns when the page exists (a subset of the host's
/// `PublishedRef`). `null` deserializes to `None`.
#[derive(Deserialize)]
struct Lookup {
    title: String,
}

/// Render a `wiki` block: the body text with its `[[links]]` resolved.
#[plugin_fn]
pub fn render_block(Json(input): Json<BlockInput>) -> FnResult<String> {
    // This plugin only renders the "wiki" block; emit nothing for anything else
    // (the host trusts our output, so wrong markup would be worse than empty).
    if !input.name.is_empty() && input.name != "wiki" {
        return Ok(String::new());
    }
    Ok(render_wiki(&input.data.text)?)
}

/// Render wiki body text to HTML: plain text is escaped; each `[[...]]` becomes a
/// resolved link. An unmatched `[[` is treated as literal text.
fn render_wiki(text: &str) -> Result<String, Error> {
    let mut out = String::from("<div class=\"wiki\">");
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        out.push_str(&html_escape::encode_text(&rest[..start]));
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                out.push_str(&render_link(&after[..end])?);
                rest = &after[end + 2..];
            }
            None => {
                // No closing `]]`: emit a literal `[[` and continue past it.
                out.push_str("[[");
                rest = after;
            }
        }
    }
    out.push_str(&html_escape::encode_text(rest));
    out.push_str("</div>");
    Ok(out)
}

/// Render one `[[inner]]` wiki link. `inner` is `Target` or `Target|Label`.
fn render_link(inner: &str) -> Result<String, Error> {
    let (target, label) = match inner.split_once('|') {
        Some((t, l)) => (t.trim(), l.trim()),
        None => {
            let t = inner.trim();
            (t, t)
        }
    };
    let slug = slugify(target);
    // A target that slugifies to nothing (e.g. "[[ !!! ]]") is not a link.
    if slug.is_empty() {
        return Ok(html_escape::encode_text(label).into_owned());
    }

    // Ask the host whether a published page exists at this slug.
    let json = unsafe { fp_lookup_slug(slug.clone())? };
    let found: Option<Lookup> = serde_json::from_str(&json).unwrap_or(None);

    let href_raw = format!("/{slug}");
    let href = html_escape::encode_double_quoted_attribute(&href_raw);
    let label_esc = html_escape::encode_text(label);
    match found {
        Some(page) => {
            // Existing page: a normal link, with the real title as a tooltip.
            let title = html_escape::encode_double_quoted_attribute(&page.title);
            Ok(format!(
                "<a href=\"{href}\" class=\"wiki-link\" title=\"{title}\">{label_esc}</a>"
            ))
        }
        None => {
            // Missing page: a "red link" (no such published page yet).
            Ok(format!(
                "<a href=\"{href}\" class=\"wiki-link wiki-link-new\" title=\"Page does not exist\">{label_esc}</a>"
            ))
        }
    }
}

/// Slugify a wiki target the same way content slugs are formed: lowercase ASCII
/// alphanumerics, runs of space/`-`/`_` collapse to a single `-`, other characters
/// dropped, no leading/trailing `-`.
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
        // Any other character is dropped.
    }
    slug
}
