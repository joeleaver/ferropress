//! The single `BlockKind -> HTML` dispatch. ARCHITECTURE INVARIANT: this match
//! is the ONLY place a block becomes HTML in the whole workspace (CI greps for
//! the `FERROPRESS-RENDER-DISPATCH` marker to forbid a second one). The editor
//! preview and the public serve path both reach HTML through here.
//!
//! The renderer is pure: it has no database access, so it never resolves a
//! media id to a URL or executes a plugin block. Those blocks emit a typed
//! placeholder carrying the data a later pass (serve layer / plugin host) needs.

use ferropress_core::{Block, BlockKind, InlineRun};

use crate::{Html, RenderMode};

/// Render one block (recursing into children) to HTML.
///
/// FERROPRESS-RENDER-DISPATCH — the one and only `BlockKind -> HTML` match.
pub fn render_block(block: &Block, mode: RenderMode) -> Html {
    let html = match &block.kind {
        BlockKind::Paragraph { runs } => format!("<p>{}</p>", render_runs(runs)),

        BlockKind::Heading { level, runs } => {
            let lvl = (*level).clamp(1, 6);
            format!("<h{lvl}>{}</h{lvl}>", render_runs(runs))
        }

        BlockKind::Quote { runs } => {
            format!("<blockquote>{}</blockquote>", render_runs(runs))
        }

        BlockKind::List { ordered } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let mut items = String::new();
            for child in &block.children {
                items.push_str("<li>");
                items.push_str(render_block(child, mode).as_str());
                items.push_str("</li>");
            }
            format!("<{tag}>{items}</{tag}>")
        }

        BlockKind::Image { media_id, alt } => {
            // No DB access here: the serve layer rewrites `data-media-id` into a
            // real `src`. Alt text is author-supplied → attribute-escaped.
            let alt = html_escape::encode_double_quoted_attribute(alt);
            format!("<figure><img data-media-id=\"{media_id}\" alt=\"{alt}\"></figure>")
        }

        BlockKind::Code { language, source } => {
            let class = match language {
                Some(l) => format!(
                    " class=\"language-{}\"",
                    html_escape::encode_double_quoted_attribute(l)
                ),
                None => String::new(),
            };
            format!(
                "<pre><code{class}>{}</code></pre>",
                html_escape::encode_text(source)
            )
        }

        BlockKind::Embed { provider, url } => {
            // Click-to-load link on publish, static placeholder in preview; both
            // keep the raw embed out of the static HTML until hydrated.
            let _ = mode;
            let provider = html_escape::encode_double_quoted_attribute(provider);
            let href = html_escape::encode_double_quoted_attribute(url);
            format!(
                "<div class=\"fp-embed\" data-provider=\"{provider}\">\
                 <a href=\"{href}\" rel=\"noopener\">{}</a></div>",
                html_escape::encode_text(url)
            )
        }

        BlockKind::Custom { plugin, name, .. } => {
            // Plugin-defined block. The real HTML comes from the plugin's render
            // hook (ferropress-plugin-host); the pure renderer emits a typed
            // placeholder so the tree still renders with the plugin absent.
            let plugin = html_escape::encode_double_quoted_attribute(plugin);
            let name = html_escape::encode_double_quoted_attribute(name);
            format!(
                "<div class=\"fp-custom\" data-plugin=\"{plugin}\" data-block=\"{name}\"></div>"
            )
        }
    };
    Html(html)
}

/// Render a sequence of inline runs (escaping text, applying marks and links).
///
/// Unknown marks are dropped rather than emitted, so a hostile or unrecognized
/// mark name can never inject a tag.
fn render_runs(runs: &[InlineRun]) -> String {
    let mut out = String::new();
    for run in runs {
        let mut piece = html_escape::encode_text(&run.text).into_owned();
        for mark in &run.marks {
            piece = match mark.as_str() {
                "bold" | "strong" => format!("<strong>{piece}</strong>"),
                "italic" | "em" => format!("<em>{piece}</em>"),
                "code" => format!("<code>{piece}</code>"),
                "strikethrough" | "strike" => format!("<s>{piece}</s>"),
                "underline" => format!("<u>{piece}</u>"),
                _ => piece,
            };
        }
        if let Some(href) = &run.href {
            let href = html_escape::encode_double_quoted_attribute(href);
            piece = format!("<a href=\"{href}\">{piece}</a>");
        }
        out.push_str(&piece);
    }
    out
}
