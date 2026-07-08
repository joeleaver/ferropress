//! The single `BlockKind -> HTML` dispatch. ARCHITECTURE INVARIANT: this match
//! is the ONLY place a block becomes HTML in the whole workspace (CI greps for
//! the `FERROPRESS-RENDER-DISPATCH` marker to forbid a second one). The editor
//! preview and the public serve path both reach HTML through here.
//!
//! The renderer is pure: it has no database access, so it never resolves a
//! media id to a URL or executes a plugin block. Those blocks emit a typed
//! placeholder carrying the data a later pass (serve layer / plugin host) needs.

use ferropress_core::{Block, BlockKind, InlineRun};

use crate::{CustomBlockRenderer, Html, RenderMode};

/// Render one block (recursing into children) to HTML. Custom (plugin) blocks are
/// resolved through `custom` (see [`CustomBlockRenderer`]); all other blocks are
/// pure.
///
/// FERROPRESS-RENDER-DISPATCH — the one and only `BlockKind -> HTML` match.
pub fn render_block(block: &Block, mode: RenderMode, custom: &dyn CustomBlockRenderer) -> Html {
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
                items.push_str(render_block(child, mode, custom).as_str());
                items.push_str("</li>");
            }
            format!("<{tag}>{items}</{tag}>")
        }

        BlockKind::Image { media, alt } => {
            // No DB access here (the renderer is pure): the image ships `src`-less,
            // carrying only its media-reference token in `data-media-id`; the serve
            // layer rewrites that into a real `src` (see `ferropress_core::MEDIA_ID_ATTR`
            // + the `ferropress-serve` rewrite). Both `media` and `alt` come from
            // author-controlled content → attribute-escaped so a hand-crafted token can
            // never inject markup (the rewrite additionally ignores non-token values).
            // `loading="lazy"` is emitted here (pure, no DB) and rides through the
            // serve-layer `src` rewrite untouched — the rewrite only reconstructs the
            // `<img … data-media-id="TOKEN"` opener and copies the rest of the tag
            // (`alt`, `loading`, …) through verbatim. `data-media-id` MUST stay the
            // first attribute so the rewrite needle still matches.
            let attr = ferropress_core::MEDIA_ID_ATTR;
            let media = html_escape::encode_double_quoted_attribute(media);
            let alt = html_escape::encode_double_quoted_attribute(alt);
            format!("<figure><img {attr}=\"{media}\" alt=\"{alt}\" loading=\"lazy\"></figure>")
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
            // A click-to-load link that keeps the raw embed out of the static HTML
            // until an island hydrates it. Output is identical in Preview and Publish
            // (the WYSIWYP invariant — a preview must show exactly what will publish),
            // so `mode` is not consulted here. The embed `url` is
            // author/plugin/import-controlled, so it routes through the SAME gate as an
            // inline link (`is_renderable_href`): a `javascript:` (or other unsafe /
            // empty) URL renders as inert text, never a clickable anchor —
            // attribute-escaping alone does NOT neutralize a script scheme.
            let _ = mode;
            let provider = html_escape::encode_double_quoted_attribute(provider);
            let label = html_escape::encode_text(url);
            let inner = if is_renderable_href(url) {
                let href = html_escape::encode_double_quoted_attribute(url);
                format!("<a href=\"{href}\" rel=\"noopener\">{label}</a>")
            } else {
                format!("<span>{label}</span>")
            };
            format!("<div class=\"fp-embed\" data-provider=\"{provider}\">{inner}</div>")
        }

        BlockKind::Custom { plugin, name, data } => {
            // Ask the plugin host to render it. A returned `Html` is the plugin's
            // FINAL, trusted output — emitted raw (see `CustomBlockRenderer`). With
            // no host (or an unresolved block) we fall back to a typed placeholder
            // so the tree still renders with the plugin absent.
            match custom.render(plugin, name, data) {
                Some(rendered) => rendered.into_string(),
                None => {
                    let plugin = html_escape::encode_double_quoted_attribute(plugin);
                    let name = html_escape::encode_double_quoted_attribute(name);
                    format!(
                        "<div class=\"fp-custom\" data-plugin=\"{plugin}\" data-block=\"{name}\"></div>"
                    )
                }
            }
        }
    };
    Html(html)
}

/// Whether `href` should be emitted as a live `<a href>` target: it must be
/// present (non-empty) AND carry a script-safe scheme (see
/// [`ferropress_core::is_safe_href`]). A failing href — empty, or a `javascript:` /
/// `data:` / `vbscript:` scheme — renders as inert escaped text with no anchor.
///
/// This is the SINGLE place author/plugin/import URLs are gated before becoming
/// links: both inline link marks and embed blocks route through here, so no content
/// producer can route around the policy (attribute-escaping alone does not
/// neutralize a script scheme).
fn is_renderable_href(href: &str) -> bool {
    !href.is_empty() && ferropress_core::is_safe_href(href)
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
            // Only a present, script-safe URL becomes a live link (see
            // `is_renderable_href`). A rejected href — empty, or a `javascript:` /
            // `data:` scheme — renders as the plain escaped text with no `<a>` wrap,
            // losing nothing and leaking nothing.
            if is_renderable_href(href) {
                let href = html_escape::encode_double_quoted_attribute(href);
                piece = format!("<a href=\"{href}\">{piece}</a>");
            }
        }
        out.push_str(&piece);
    }
    out
}
