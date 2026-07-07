//! # ferropress-render
//!
//! The ONE place a block tree becomes HTML. ARCHITECTURE INVARIANT: there is
//! exactly one `BlockKind -> HTML` dispatch in the entire workspace and it lives
//! here (in [`blocks`]). The editor preview and the public serve path both call
//! [`render`], guaranteeing what-you-see-is-what-you-publish. CI greps for the
//! dispatch marker (`FERROPRESS-RENDER-DISPATCH`) to forbid a second
//! implementation anywhere else — the three diverged match statements in rinch
//! are the cautionary tale this rule exists to prevent.
//!
//! Templates (MiniJinja, in `ferropress-theme`) receive the single pre-rendered
//! HTML string from here; they never see blocks.
//!
//! ## Escaping
//!
//! Every piece of user/author-supplied text is passed through `html-escape`
//! before it reaches the output buffer. The renderer never concatenates raw
//! author text into the HTML stream. The resulting [`Html`] newtype marks a
//! string as already-escaped, render-ready output so it cannot be confused with
//! a raw `String` further down the pipeline.

pub mod blocks;

use ferropress_core::BlockTree;

/// Opaque rendered HTML. Newtype so a raw `String` can't be mistaken for
/// already-escaped, render-ready output.
#[derive(Debug, Clone, PartialEq)]
pub struct Html(pub String);

impl Html {
    /// An empty fragment.
    pub fn empty() -> Self {
        Html(String::new())
    }

    /// Borrow the inner HTML string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Whether we are rendering for the public site or the in-editor preview. Some
/// blocks render differently (e.g. preview shows placeholders for not-yet-
/// uploaded media; embeds may be click-to-load on publish).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Publish,
    Preview,
}

/// Resolves a custom (plugin) block to HTML. The renderer itself is pure and has
/// no plugin runtime, so it delegates `BlockKind::Custom` through this seam:
/// `ferropress-plugin-host` implements it (running the plugin's `render_block`
/// export); the render crate gains no wasmtime/extism dependency.
///
/// Returning `None` falls back to the built-in typed placeholder (so a tree still
/// renders with the plugin absent). A returned [`Html`] is the plugin's FINAL
/// output and is emitted **raw** — plugins are operator-installed, trusted code
/// (output sanitization / trust tiers are a separate concern), exactly as a
/// WordPress shortcode emits arbitrary HTML.
///
/// `Send + Sync` so it can be shared as `Arc<dyn CustomBlockRenderer>` across the
/// async serve path + the regen loop (axum state must be `Send + Sync`).
pub trait CustomBlockRenderer: Send + Sync {
    /// Render the custom block identified by `(plugin, name)` with its opaque JSON
    /// `data`, or `None` to use the placeholder.
    fn render(&self, plugin: &str, name: &str, data: &serde_json::Value) -> Option<Html>;
}

/// A [`CustomBlockRenderer`] that resolves nothing — every custom block falls back
/// to the placeholder. Used by [`render`] when no plugin host is wired (tests,
/// the editor before a host exists).
pub struct NoCustomBlocks;

impl CustomBlockRenderer for NoCustomBlocks {
    fn render(&self, _plugin: &str, _name: &str, _data: &serde_json::Value) -> Option<Html> {
        None
    }
}

/// Render a whole block tree to HTML with no custom-block resolution (custom
/// blocks render as placeholders). Equivalent to [`render_with`] using
/// [`NoCustomBlocks`]. The single public entry point for the pure path.
pub fn render(tree: &BlockTree, mode: RenderMode) -> Html {
    render_with(tree, mode, &NoCustomBlocks)
}

/// Render a whole block tree to HTML, resolving custom (plugin) blocks through
/// `custom`. The serve/publish path passes the plugin host here so
/// `BlockKind::Custom` blocks get their real HTML.
///
/// Walks `tree.blocks` in order, dispatches each top-level block through the
/// single [`blocks::render_block`] match (which recurses into children), and
/// concatenates the fragments. Returns ready-to-embed [`Html`].
pub fn render_with(tree: &BlockTree, mode: RenderMode, custom: &dyn CustomBlockRenderer) -> Html {
    let mut out = String::new();
    for block in &tree.blocks {
        out.push_str(blocks::render_block(block, mode, custom).as_str());
    }
    Html(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferropress_core::{Block, BlockKind, InlineRun};

    fn run(text: &str) -> InlineRun {
        InlineRun {
            text: text.to_owned(),
            marks: Vec::new(),
            href: None,
        }
    }

    fn linked(text: &str, href: &str) -> InlineRun {
        InlineRun {
            text: text.to_owned(),
            marks: Vec::new(),
            href: Some(href.to_owned()),
        }
    }

    fn para(runs: Vec<InlineRun>) -> BlockTree {
        BlockTree::from_blocks(vec![block(BlockKind::Paragraph { runs })])
    }

    fn block(kind: BlockKind) -> Block {
        Block {
            uid: "test-uid".to_owned(),
            kind,
            children: Vec::new(),
        }
    }

    #[test]
    fn renders_paragraph_and_escapes_text() {
        let tree = BlockTree::from_blocks(vec![block(BlockKind::Paragraph {
            runs: vec![run("hello <b> & 'world'")],
        })]);
        let html = render(&tree, RenderMode::Publish);
        // The angle brackets and ampersand must be escaped; the <p> wrapper is
        // emitted by the renderer (not author text), so it stays literal.
        assert!(html.as_str().starts_with("<p>"));
        assert!(html.as_str().contains("&lt;b&gt;"));
        assert!(html.as_str().contains("&amp;"));
        assert!(!html.as_str().contains("<b>"));
    }

    #[test]
    fn renders_heading_with_clamped_level() {
        let tree = BlockTree::from_blocks(vec![block(BlockKind::Heading {
            level: 9, // out of range; renderer clamps into 1..=6
            runs: vec![run("Title")],
        })]);
        let html = render(&tree, RenderMode::Publish);
        assert!(html.as_str().contains("<h6>Title</h6>"));
    }

    #[test]
    fn renders_ordered_list_from_children() {
        let item = |t: &str| Block {
            uid: "li".to_owned(),
            kind: BlockKind::Paragraph { runs: vec![run(t)] },
            children: Vec::new(),
        };
        let list = Block {
            uid: "list".to_owned(),
            kind: BlockKind::List { ordered: true },
            children: vec![item("one"), item("two")],
        };
        let tree = BlockTree::from_blocks(vec![list]);
        let html = render(&tree, RenderMode::Publish);
        assert!(html.as_str().starts_with("<ol>"));
        assert!(html.as_str().contains("<li>"));
        assert!(html.as_str().contains("one"));
        assert!(html.as_str().contains("two"));
        assert!(html.as_str().trim_end().ends_with("</ol>"));
    }

    #[test]
    fn renders_safe_link_as_anchor() {
        let html = render(&para(vec![linked("docs", "https://example.com/a?b=c")]), RenderMode::Publish);
        assert_eq!(
            html.as_str(),
            "<p><a href=\"https://example.com/a?b=c\">docs</a></p>"
        );
    }

    #[test]
    fn renders_relative_and_mailto_links() {
        let rel = render(&para(vec![linked("about", "/about#team")]), RenderMode::Publish);
        assert!(rel.as_str().contains("<a href=\"/about#team\">about</a>"));
        let mail = render(&para(vec![linked("mail", "mailto:hi@example.com")]), RenderMode::Publish);
        assert!(mail.as_str().contains("<a href=\"mailto:hi@example.com\">mail</a>"));
    }

    #[test]
    fn drops_script_scheme_link_but_keeps_text() {
        // A `javascript:` href must NOT become an executable anchor; the visible
        // (escaped) text survives so no content is lost. This is the stored-XSS
        // guard the link toolbar directly exposes.
        for href in ["javascript:alert(1)", "data:text/html,<script>alert(1)</script>", "vbscript:x"] {
            let html = render(&para(vec![linked("click me", href)]), RenderMode::Publish);
            assert!(!html.as_str().contains("<a "), "{href} must not produce an anchor: {}", html.as_str());
            assert!(!html.as_str().contains("javascript:"), "scheme must not appear: {}", html.as_str());
            assert!(html.as_str().contains("click me"), "link text must survive: {}", html.as_str());
        }
    }

    #[test]
    fn drops_empty_href_link() {
        // An empty href is not a link — render the text plain, no degenerate
        // `<a href="">` (which would reload the current page on click).
        let html = render(&para(vec![linked("x", "")]), RenderMode::Publish);
        assert_eq!(html.as_str(), "<p>x</p>");
    }

    #[test]
    fn embed_safe_url_renders_anchor() {
        let tree = BlockTree::from_blocks(vec![block(BlockKind::Embed {
            provider: "youtube".to_owned(),
            url: "https://youtu.be/abc".to_owned(),
        })]);
        let html = render(&tree, RenderMode::Publish);
        assert!(html.as_str().contains("<a href=\"https://youtu.be/abc\" rel=\"noopener\">"));
    }

    #[test]
    fn embed_script_or_empty_url_is_inert() {
        // The embed url is author/plugin/import-controlled and flows through the same
        // link guard: a script scheme (or empty url) must NOT become a clickable
        // anchor, but the label text is preserved.
        for url in ["javascript:alert(1)", "data:text/html,x", ""] {
            let tree = BlockTree::from_blocks(vec![block(BlockKind::Embed {
                provider: "p".to_owned(),
                url: url.to_owned(),
            })]);
            let html = render(&tree, RenderMode::Publish);
            // No anchor at all, and specifically no executable href. The url may
            // still appear as inert escaped label text — that's harmless.
            assert!(!html.as_str().contains("<a "), "{url:?} must not produce an anchor: {}", html.as_str());
            assert!(!html.as_str().contains("href="), "{url:?} must not emit an href: {}", html.as_str());
        }
    }

    #[test]
    fn link_href_is_attribute_escaped() {
        // A safe scheme whose href carries attribute-breaking characters (but no
        // whitespace, which would reject it) stays inert: the double-quote and
        // angle brackets are escaped so the payload can't close the attribute and
        // inject a tag.
        let html = render(
            &para(vec![linked("x", "https://example.com/\"><script>alert(1)</script>")]),
            RenderMode::Publish,
        );
        assert!(!html.as_str().contains("<script"), "must not break out of href: {}", html.as_str());
        assert!(html.as_str().contains("&quot;"), "quote must be escaped: {}", html.as_str());
    }
}
