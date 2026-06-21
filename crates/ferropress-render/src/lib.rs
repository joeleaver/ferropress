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
}
