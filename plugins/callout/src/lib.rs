//! Reference custom-block plugin: a **callout** box.
//!
//! The host (`ferropress-plugin-host`) calls the `render_block` export with JSON
//! `{ "name": <block name>, "data": <block payload> }` and uses the returned
//! string as the block's HTML. This is a pure-compute, capability-zero plugin: it
//! reads only its input and produces final HTML, escaping its own text +
//! constraining the variant so it can't inject markup (the host emits plugin
//! output raw, so escaping is the plugin's responsibility).

use extism_pdk::*;
use serde::Deserialize;

/// The host's block-render input envelope: `{ name, data }`.
#[derive(Deserialize)]
struct BlockInput {
    /// The custom block's name. This plugin only renders `"callout"`, but the
    /// field is part of the host ABI (a plugin may render several block names).
    #[serde(default)]
    name: String,
    #[serde(default)]
    data: CalloutData,
}

/// The `callout` block's attributes.
#[derive(Default, Deserialize)]
struct CalloutData {
    /// Visual variant (note / warning / …) — emitted into a CSS class.
    #[serde(default)]
    variant: String,
    /// The callout body text.
    #[serde(default)]
    text: String,
}

/// Render a custom block to HTML.
#[plugin_fn]
pub fn render_block(Json(input): Json<BlockInput>) -> FnResult<String> {
    // This plugin only knows the "callout" block; anything else is a no-op empty
    // string (the host falls back to its placeholder for unknown output? no — the
    // host trusts our output, so emit nothing rather than wrong markup).
    if !input.name.is_empty() && input.name != "callout" {
        return Ok(String::new());
    }
    let variant = sanitize_variant(&input.data.variant);
    let text = html_escape::encode_text(&input.data.text);
    Ok(format!(
        "<div class=\"fp-callout fp-callout-{variant}\"><p>{text}</p></div>"
    ))
}

/// Constrain the variant to `[a-z0-9-]` (lowercased) so it can't break out of the
/// class attribute; default to `note`.
fn sanitize_variant(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if cleaned.is_empty() {
        "note".to_owned()
    } else {
        cleaned
    }
}
