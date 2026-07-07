//! # ferropress-editor-bridge
//!
//! The **BlockTree ⇄ rinch `DocNode`** converter — the seam between Ferropress's
//! persisted content model ([`ferropress_core::block::BlockTree`]) and the rinch
//! rich-text editor's durable wire shape ([`rinch_editor_core::serialize::DocNode`]).
//!
//! The admin SPA loads a post's `BlockTree` JSON, converts it to a `DocNode`, and
//! hands that to [`Schema::node_from_doc`] to get an editor [`Node`] it can
//! `load_doc`. On save it reads the editor `Node`, serializes it with `to_doc`, and
//! converts the `DocNode` back to a `BlockTree` to `PUT`. This crate owns both
//! directions so the correctness-critical mapping is one host-testable unit.
//!
//! ## The impedance the mapping bridges
//!
//! Ferropress's [`BlockKind`] is a *flat* prose model (paragraph / heading / quote
//! carry inline runs directly; a list's children ARE its items). rinch's editor is
//! a ProseMirror-style *nested* tree whose node/mark **type names must match
//! `Schema::starter_kit()`** or `node_from_doc` hard-errors. The mapping is:
//!
//! | Ferropress `BlockKind` | editor node(s) |
//! |------------------------|----------------|
//! | `Paragraph { runs }`   | `paragraph > (text …)` |
//! | `Heading { level, runs }` | `heading{level} > (text …)` |
//! | `Quote { runs }`       | `blockquote > paragraph > (text …)` (blockquote holds *block* content) |
//! | `List { ordered }` + children | `bullet_list`/`ordered_list` > `list_item` > ‹child block› |
//! | `Code { language, source }` | `code_block{language} > text` |
//! | `Image { media_id, alt }` | `paragraph > image{src,alt}` (image is an *inline* atom) |
//! | `Embed` / `Custom`     | a `code_block` with a reserved `language` sentinel (see below) |
//!
//! Marks are limited to the set BOTH the editor AND the public renderer
//! ([`ferropress_render` `render_runs`](ferropress-render)) understand — **bold,
//! italic, underline, strike, code** + **link** (via `href`). Marks the editor has
//! but the renderer drops (subscript / superscript / highlight / text_color) are
//! dropped here too, so the in-editor preview and the published page never disagree.
//!
//! ## Documented MVP boundaries (never silent corruption)
//!
//! - **Block uids are regenerated on every save.** The editor's `DocNode` carries no
//!   uid, so a saved tree gets fresh v4 uids. Stable-uid-across-edits matters only
//!   to the (not-yet-wired) revision/diff feature; acceptable for the MVP.
//! - **Embed / Custom** have no starter-kit node, so they are parked losslessly in a
//!   `code_block` whose `language` is a reserved sentinel and reconstructed exactly on
//!   the way back. They *display* as a code block in the editor (a rough edge), but no
//!   author data is lost.
//! - **Inline images mixed with text** — what the toolbar's insert-at-caret produces
//!   inside a non-empty paragraph, heading, or blockquote — are **split out** into their
//!   own top-level `Image` blocks, in document order, so no image is ever dropped. rinch
//!   models an image as an inline atom but a Ferropress `Image` is a block; the split
//!   bridges that (a quote is split *around* the image, since `Quote` can't nest one).
//!   Whitespace-only prose around the image is discarded (it was just spacing). The split
//!   is stable across edits: `block_to_doc` re-wraps each `Image` in its own paragraph,
//!   so a re-load converges rather than churning.
//! - **Editor-only constructs** (horizontal rule, tables, task-list checkbox state,
//!   multi-block list items) have no Ferropress representation. The MVP toolbar cannot
//!   create them; a markdown-input-rule one degrades gracefully (dropped / flattened)
//!   rather than corrupting surrounding prose. A `hard_break` (Shift+Enter) flattens to
//!   a newline run so it reads as whitespace without merging the words it separated.

use std::collections::BTreeMap;

use ferropress_core::block::{Block, BlockKind, BlockTree, InlineRun};
use rinch_editor_core::serialize::{DocMark, DocNode, JsonAttr};
use rinch_editor_core::{Node, Schema};
use uuid::Uuid;

// ── The `src` convention for images + the Embed/Custom sentinels ─────────────────

// An editor `image` node's `src` is the media original's REAL served URL
// (`ferropress_core::media_url` → `/media/{token}`), so the live editor actually
// displays the image — and it is the SAME URL the public page renders (the serve layer
// rewrites `data-media-id` into it), so the in-editor preview never disagrees with the
// published page. On save the token is recovered from that URL
// (`ferropress_core::media_token_from_url`); the persisted `BlockKind::Image` keeps only
// the opaque media token (a uuid), never a URL.

/// Reserved `code_block` `language` values that mark a parked non-prose block.
const SENTINEL_EMBED: &str = "fp:embed";
const SENTINEL_CUSTOM: &str = "fp:custom";

// ── Public API ───────────────────────────────────────────────────────────────────

/// The error from the two convenience helpers ([`block_tree_json_to_node`] /
/// [`node_to_block_tree_json`]). The pure converters ([`block_tree_to_doc`] /
/// [`doc_to_block_tree`]) are total and never fail.
#[derive(Debug, Clone)]
pub enum BridgeError {
    /// The stored `BlockTree` JSON did not parse / version-check, or re-serializing
    /// the converted tree failed.
    BlockTree(String),
    /// The editor rejected the converted document (schema validation), or serializing
    /// the editor `Node` failed.
    Editor(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::BlockTree(m) => write!(f, "block tree: {m}"),
            BridgeError::Editor(m) => write!(f, "editor: {m}"),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Convert a persisted `BlockTree` (its `serde_json::Value` form, as the admin API
/// serves it) into a validated editor [`Node`] ready for `EditorHandle::load_doc`.
pub fn block_tree_json_to_node(
    schema: &Schema,
    value: &serde_json::Value,
) -> Result<Node, BridgeError> {
    let tree = BlockTree::from_json_value(value.clone())
        .map_err(|e| BridgeError::BlockTree(e.to_string()))?;
    let doc = block_tree_to_doc(&tree);
    schema
        .node_from_doc(&doc)
        .map_err(|e| BridgeError::Editor(e.to_string()))
}

/// Convert an editor [`Node`] (from `EditorHandle::doc()`) into a persisted
/// `BlockTree` (its `serde_json::Value` form, to `PUT` back to the admin API).
pub fn node_to_block_tree_json(node: &Node) -> Result<serde_json::Value, BridgeError> {
    let doc = node
        .to_doc()
        .map_err(|e| BridgeError::Editor(e.to_string()))?;
    let tree = doc_to_block_tree(&doc);
    tree.to_json_value()
        .map_err(|e| BridgeError::BlockTree(e.to_string()))
}

/// Convert a [`BlockTree`] into a root `doc` [`DocNode`]. Total — always produces a
/// schema-valid document (an empty tree becomes a single empty paragraph, since the
/// `doc` node's content is `block+`).
pub fn block_tree_to_doc(tree: &BlockTree) -> DocNode {
    let mut content: Vec<DocNode> = tree.blocks.iter().map(block_to_doc).collect();
    if content.is_empty() {
        content.push(branch("paragraph", BTreeMap::new(), Vec::new()));
    }
    branch("doc", BTreeMap::new(), content)
}

/// Convert a root `doc` [`DocNode`] (from the editor) back into a [`BlockTree`].
/// Total — unmappable nodes are dropped rather than corrupting their neighbours.
/// Fresh v4 uids are minted for every block (the wire shape carries none).
pub fn doc_to_block_tree(doc: &DocNode) -> BlockTree {
    let blocks = doc.content.iter().flat_map(blocks_from_node).collect();
    BlockTree::from_blocks(blocks)
}

// ── DocNode constructors ──────────────────────────────────────────────────────────

fn branch(node_type: &str, attrs: BTreeMap<String, JsonAttr>, content: Vec<DocNode>) -> DocNode {
    DocNode {
        node_type: node_type.to_owned(),
        attrs,
        content,
        text: None,
        marks: Vec::new(),
    }
}

fn text_node(text: String, marks: Vec<DocMark>) -> DocNode {
    DocNode {
        node_type: "text".to_owned(),
        attrs: BTreeMap::new(),
        content: Vec::new(),
        text: Some(text),
        marks,
    }
}

fn attrs1(key: &str, value: JsonAttr) -> BTreeMap<String, JsonAttr> {
    let mut m = BTreeMap::new();
    m.insert(key.to_owned(), value);
    m
}

// ── BlockTree → DocNode ────────────────────────────────────────────────────────────

fn block_to_doc(block: &Block) -> DocNode {
    match &block.kind {
        BlockKind::Paragraph { runs } => branch("paragraph", BTreeMap::new(), runs_to_inline(runs)),

        BlockKind::Heading { level, runs } => {
            let level = i64::from((*level).clamp(1, 6));
            branch(
                "heading",
                attrs1("level", JsonAttr::Int(level)),
                runs_to_inline(runs),
            )
        }

        // A blockquote holds BLOCK content (`block+`), not inline — wrap the runs in a
        // paragraph so it is schema-valid.
        BlockKind::Quote { runs } => branch(
            "blockquote",
            BTreeMap::new(),
            vec![branch("paragraph", BTreeMap::new(), runs_to_inline(runs))],
        ),

        BlockKind::List { ordered } => {
            let mut items: Vec<DocNode> = block
                .children
                .iter()
                .map(|child| branch("list_item", BTreeMap::new(), vec![block_to_doc(child)]))
                .collect();
            if items.is_empty() {
                // `list_item+` requires at least one item.
                items.push(branch(
                    "list_item",
                    BTreeMap::new(),
                    vec![branch("paragraph", BTreeMap::new(), Vec::new())],
                ));
            }
            let ty = if *ordered {
                "ordered_list"
            } else {
                "bullet_list"
            };
            branch(ty, BTreeMap::new(), items)
        }

        BlockKind::Code { language, source } => {
            let content = if source.is_empty() {
                Vec::new()
            } else {
                vec![text_node(source.clone(), Vec::new())]
            };
            branch(
                "code_block",
                attrs1(
                    "language",
                    JsonAttr::Str(language.clone().unwrap_or_default()),
                ),
                content,
            )
        }

        // `image` is an INLINE atom; wrap it in a paragraph to be a valid `doc` child.
        BlockKind::Image { media, alt } => {
            let mut a = BTreeMap::new();
            a.insert("src".to_owned(), JsonAttr::Str(media_src(media)));
            a.insert("alt".to_owned(), JsonAttr::Str(alt.clone()));
            let image = DocNode {
                node_type: "image".to_owned(),
                attrs: a,
                content: Vec::new(),
                text: None,
                marks: Vec::new(),
            };
            branch("paragraph", BTreeMap::new(), vec![image])
        }

        BlockKind::Embed { provider, url } => sentinel(
            SENTINEL_EMBED,
            &serde_json::json!({ "provider": provider, "url": url }),
        ),

        BlockKind::Custom { plugin, name, data } => sentinel(
            SENTINEL_CUSTOM,
            &serde_json::json!({ "plugin": plugin, "name": name, "data": data }),
        ),
    }
}

fn runs_to_inline(runs: &[InlineRun]) -> Vec<DocNode> {
    runs.iter().filter_map(run_to_text).collect()
}

/// One inline run → a `text` node carrying its marks, or `None` for an empty run (an
/// empty text node is invalid in the editor model).
fn run_to_text(run: &InlineRun) -> Option<DocNode> {
    if run.text.is_empty() {
        return None;
    }
    let mut marks = Vec::new();
    for m in &run.marks {
        if let Some(canonical) = simple_mark(m) {
            marks.push(DocMark {
                mark_type: canonical.to_owned(),
                attrs: BTreeMap::new(),
            });
        }
    }
    if let Some(href) = &run.href {
        marks.push(DocMark {
            mark_type: "link".to_owned(),
            attrs: attrs1("href", JsonAttr::Str(href.clone())),
        });
    }
    Some(text_node(run.text.clone(), marks))
}

/// A Ferropress mark string → the editor's canonical mark-type name, for the marks
/// BOTH the editor and the public renderer support. Aliases the renderer accepts are
/// folded in; everything else returns `None` (dropped, exactly as `render_runs` drops
/// unknown marks).
fn simple_mark(name: &str) -> Option<&'static str> {
    match name {
        "bold" | "strong" => Some("bold"),
        "italic" | "em" => Some("italic"),
        "underline" => Some("underline"),
        "strike" | "strikethrough" => Some("strike"),
        "code" => Some("code"),
        _ => None,
    }
}

fn media_src(media: &str) -> String {
    ferropress_core::media_url(media)
}

/// Park a non-prose block's JSON in a `code_block` under a reserved `language`
/// sentinel, to be revived verbatim by [`code_or_revived`].
fn sentinel(tag: &str, payload: &serde_json::Value) -> DocNode {
    let text = serde_json::to_string(payload).unwrap_or_default();
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![text_node(text, Vec::new())]
    };
    branch(
        "code_block",
        attrs1("language", JsonAttr::Str(tag.to_owned())),
        content,
    )
}

// ── DocNode → BlockTree ────────────────────────────────────────────────────────────

/// Convert one editor `DocNode` into zero or more Ferropress blocks, in document order.
///
/// Most node types map 1:1. A `paragraph` or `heading` whose inline content mixes text
/// with `image` atoms is SPLIT into a run of prose + `Image` blocks (see
/// [`split_inline_blocks`]) so an inline image — what the toolbar's insert-at-caret
/// produces inside a non-empty paragraph — is never silently dropped. rinch models an
/// image as an inline atom, but a Ferropress `Image` is a top-level block; the split
/// bridges that. Unmappable nodes yield an empty vec (dropped, never corrupting a
/// neighbour).
fn blocks_from_node(node: &DocNode) -> Vec<Block> {
    match node.node_type.as_str() {
        "paragraph" => split_inline_blocks(&node.content, |runs| BlockKind::Paragraph { runs }),

        "heading" => {
            let level = attr_int(node, "level").unwrap_or(1).clamp(1, 6) as u8;
            split_inline_blocks(&node.content, move |runs| BlockKind::Heading {
                level,
                runs,
            })
        }

        // A blockquote flattens to a `Quote` of inline runs. If an image was inserted
        // inside it, lift the image OUT into its own block (`Quote` can't nest one), just
        // as a paragraph is split; the image-free path keeps the simpler flattening.
        "blockquote" => {
            if contains_image(&node.content) {
                quote_blocks(&node.content)
            } else {
                vec![new_block(
                    BlockKind::Quote {
                        runs: collect_prose_runs(&node.content),
                    },
                    Vec::new(),
                )]
            }
        }

        "bullet_list" => vec![list_block(false, node)],
        "ordered_list" => vec![list_block(true, node)],
        // A task list has no Ferropress equivalent; degrade to a plain list (the
        // per-item checkbox state is lost, documented). The toolbar can't create one.
        "task_list" => vec![list_block(false, node)],

        "code_block" => vec![code_or_revived(node)],

        // horizontal_rule / table / hard_break-as-block / anything unknown: no
        // Ferropress representation. Drop rather than corrupt surrounding prose.
        _ => Vec::new(),
    }
}

/// Split a block's inline `content` into a sequence of Ferropress blocks, breaking at
/// every inline `image` atom: consecutive non-image inline nodes become one prose block
/// (built by `wrap` — e.g. `Paragraph`/`Heading`), and each image becomes its own
/// top-level `Image` block, in document order.
///
/// With NO image this is exactly `[wrap(inline_to_runs(content))]` — a single block,
/// even when empty (so a blank paragraph is preserved). With an image, whitespace-only
/// prose segments around it are discarded (they were just spacing), matching how a
/// lone-image paragraph tolerated surrounding whitespace — so the split never manufactures
/// blank paragraphs. Because [`block_to_doc`] re-wraps each `Image` in its own paragraph,
/// the split is stable: it converges after one load rather than churning on every save.
fn split_inline_blocks(
    content: &[DocNode],
    wrap: impl Fn(Vec<InlineRun>) -> BlockKind,
) -> Vec<Block> {
    if !content.iter().any(|n| n.node_type == "image") {
        return vec![new_block(wrap(inline_to_runs(content)), Vec::new())];
    }
    let mut out = Vec::new();
    let mut runs: Vec<InlineRun> = Vec::new();
    for n in content {
        if n.node_type == "image" {
            flush_prose(&mut runs, &wrap, &mut out);
            out.push(image_block(n));
        } else if let Some(r) = inline_node_to_run(n) {
            runs.push(r);
        }
    }
    flush_prose(&mut runs, &wrap, &mut out);
    out
}

/// Emit the buffered prose `runs` as one block via `wrap`, then clear the buffer —
/// unless the segment is empty or entirely whitespace (spacing around an image, not real
/// content), in which case nothing is emitted.
fn flush_prose(
    runs: &mut Vec<InlineRun>,
    wrap: &impl Fn(Vec<InlineRun>) -> BlockKind,
    out: &mut Vec<Block>,
) {
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        runs.clear();
        return;
    }
    out.push(new_block(wrap(std::mem::take(runs)), Vec::new()));
}

/// An editor inline `image` atom → a top-level Ferropress `Image` block. The `src` is a
/// [`media_url`]; its token is recovered via [`parse_media_ref`] (a non-media `src`
/// collapses to an empty token, as documented there).
fn image_block(node: &DocNode) -> Block {
    new_block(
        BlockKind::Image {
            media: parse_media_ref(&attr_str(node, "src")),
            alt: attr_str(node, "alt"),
        },
        Vec::new(),
    )
}

/// A `code_block` node → a `Code` block, or the `Embed`/`Custom` it parks when its
/// `language` is a reserved sentinel (the reverse of [`sentinel`]).
fn code_or_revived(node: &DocNode) -> Block {
    let lang = attr_str(node, "language");
    let source = code_text(&node.content);
    if lang == SENTINEL_EMBED
        && let Some(block) = revive_embed(&source)
    {
        return block;
    }
    if lang == SENTINEL_CUSTOM
        && let Some(block) = revive_custom(&source)
    {
        return block;
    }
    new_block(
        BlockKind::Code {
            language: if lang.is_empty() { None } else { Some(lang) },
            source,
        },
        Vec::new(),
    )
}

/// Map an editor list node to a Ferropress `List` block: each `list_item`'s block
/// child(ren) become the list's children (each rendered inside an `<li>` by the
/// renderer). The common case (one paragraph per item) is 1:1.
fn list_block(ordered: bool, list: &DocNode) -> Block {
    let mut children = Vec::new();
    for item in &list.content {
        if item.node_type != "list_item" && item.node_type != "task_item" {
            continue;
        }
        for block_child in &item.content {
            children.extend(blocks_from_node(block_child));
        }
    }
    new_block(BlockKind::List { ordered }, children)
}

/// Flatten a blockquote's block content to a single run list — Ferropress's `Quote`
/// holds flat inline runs (the renderer puts them directly inside `<blockquote>`).
/// A single-paragraph quote (the common case) round-trips exactly; multiple
/// paragraphs are concatenated (documented). Used only when the quote has NO image;
/// otherwise [`quote_blocks`] takes over so the image isn't flattened away.
fn collect_prose_runs(content: &[DocNode]) -> Vec<InlineRun> {
    let mut runs = Vec::new();
    for child in content {
        match child.node_type.as_str() {
            "paragraph" | "heading" => runs.extend(inline_to_runs(&child.content)),
            "text" => {
                if let Some(r) = text_to_run(child) {
                    runs.push(r);
                }
            }
            _ => runs.extend(collect_prose_runs(&child.content)),
        }
    }
    runs
}

/// Whether any inline `image` atom is nested anywhere in `content` — the guard that
/// decides between the flattening path ([`collect_prose_runs`]) and the splitting path
/// ([`quote_blocks`]) for a blockquote.
fn contains_image(content: &[DocNode]) -> bool {
    content
        .iter()
        .any(|n| n.node_type == "image" || contains_image(&n.content))
}

/// Split a blockquote's block content into a sequence of blocks, lifting each inline
/// `image` atom nested inside it OUT into its own top-level `Image` block and splitting
/// the quote around it. Ferropress's `Quote` holds flat inline runs — it cannot nest an
/// image — so, exactly as [`split_inline_blocks`] does for a paragraph, an image inserted
/// into a quote is preserved as a sibling `Image` block rather than silently dropped. A
/// quote fragment with no prose (e.g. a quote holding only an image) emits no empty
/// `Quote`, mirroring how a lone-image paragraph yields just the `Image`.
fn quote_blocks(content: &[DocNode]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut runs: Vec<InlineRun> = Vec::new();
    collect_quote(content, &mut runs, &mut out);
    flush_quote(&mut runs, &mut out);
    out
}

/// Walk a blockquote's (block-level) content in document order: accumulate its prose into
/// `runs`, and at each nested inline `image` atom flush the buffered prose as a `Quote`
/// block then emit the image as its own `Image` block.
fn collect_quote(content: &[DocNode], runs: &mut Vec<InlineRun>, out: &mut Vec<Block>) {
    for child in content {
        match child.node_type.as_str() {
            "image" => {
                flush_quote(runs, out);
                out.push(image_block(child));
            }
            "paragraph" | "heading" => {
                for n in &child.content {
                    if n.node_type == "image" {
                        flush_quote(runs, out);
                        out.push(image_block(n));
                    } else if let Some(r) = inline_node_to_run(n) {
                        runs.push(r);
                    }
                }
            }
            "text" => {
                if let Some(r) = text_to_run(child) {
                    runs.push(r);
                }
            }
            _ => collect_quote(&child.content, runs, out),
        }
    }
}

/// Flush buffered quote prose as one `Quote` block (dropping an empty/whitespace-only
/// segment), reusing [`flush_prose`]'s emit-unless-blank rule.
fn flush_quote(runs: &mut Vec<InlineRun>, out: &mut Vec<Block>) {
    flush_prose(runs, &|runs| BlockKind::Quote { runs }, out);
}

fn inline_to_runs(content: &[DocNode]) -> Vec<InlineRun> {
    content.iter().filter_map(inline_node_to_run).collect()
}

/// One inline editor node → a Ferropress run. A `hard_break` (the Shift+Enter soft
/// line break) has no Ferropress inline representation, so it degrades to a **newline
/// run** — the break flattens to whitespace on the published page, but critically it
/// never merges the words on either side (dropping it outright would turn
/// "Hello"+break+"World" into "HelloWorld"). Other non-text inline nodes (a lone
/// image is handled at the block level) yield `None`.
fn inline_node_to_run(node: &DocNode) -> Option<InlineRun> {
    if node.node_type == "hard_break" {
        return Some(InlineRun {
            text: "\n".to_owned(),
            marks: Vec::new(),
            href: None,
        });
    }
    text_to_run(node)
}

/// A `text` node → one inline run. Non-text nodes (inline image, hard break) yield
/// `None` (dropped from mixed prose; a lone image is handled at the block level).
fn text_to_run(node: &DocNode) -> Option<InlineRun> {
    let text = node.text.clone()?;
    if text.is_empty() {
        return None;
    }
    let mut marks = Vec::new();
    let mut href = None;
    for m in &node.marks {
        match m.mark_type.as_str() {
            "bold" => marks.push("bold".to_owned()),
            "italic" => marks.push("italic".to_owned()),
            "underline" => marks.push("underline".to_owned()),
            "strike" => marks.push("strike".to_owned()),
            "code" => marks.push("code".to_owned()),
            "link" => href = href.or_else(|| mark_attr_str(m, "href")),
            // subscript / superscript / highlight / text_color: not in Ferropress's
            // rendered vocabulary — drop the mark, keep the text.
            _ => {}
        }
    }
    Some(InlineRun { text, marks, href })
}

fn code_text(content: &[DocNode]) -> String {
    let mut s = String::new();
    for n in content {
        if let Some(t) = &n.text {
            s.push_str(t);
        }
    }
    s
}

fn revive_embed(source: &str) -> Option<Block> {
    let v: serde_json::Value = serde_json::from_str(source).ok()?;
    let provider = v.get("provider")?.as_str()?.to_owned();
    let url = v.get("url")?.as_str()?.to_owned();
    Some(new_block(BlockKind::Embed { provider, url }, Vec::new()))
}

fn revive_custom(source: &str) -> Option<Block> {
    let v: serde_json::Value = serde_json::from_str(source).ok()?;
    let plugin = v.get("plugin")?.as_str()?.to_owned();
    let name = v.get("name")?.as_str()?.to_owned();
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    Some(new_block(
        BlockKind::Custom { plugin, name, data },
        Vec::new(),
    ))
}

/// Recover a media reference token from an image node's `src`. A `src` that isn't a
/// media URL (e.g. an externally-pasted image the model can't represent) yields an
/// empty string — "no media" — so an unmappable image collapses to a blank ref rather
/// than corrupting a neighbour.
fn parse_media_ref(src: &str) -> String {
    ferropress_core::media_token_from_url(src)
        .unwrap_or_default()
        .to_owned()
}

fn new_block(kind: BlockKind, children: Vec<Block>) -> Block {
    Block {
        uid: Uuid::new_v4().to_string(),
        kind,
        children,
    }
}

// ── attr readers ────────────────────────────────────────────────────────────────────

fn attr_str(node: &DocNode, key: &str) -> String {
    match node.attrs.get(key) {
        Some(JsonAttr::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

fn attr_int(node: &DocNode, key: &str) -> Option<i64> {
    match node.attrs.get(key) {
        Some(JsonAttr::Int(i)) => Some(*i),
        _ => None,
    }
}

fn mark_attr_str(mark: &DocMark, key: &str) -> Option<String> {
    match mark.attrs.get(key) {
        Some(JsonAttr::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinch_editor_core::Schema;

    fn run(text: &str, marks: &[&str], href: Option<&str>) -> InlineRun {
        InlineRun {
            text: text.to_owned(),
            marks: marks.iter().map(|m| (*m).to_owned()).collect(),
            href: href.map(str::to_owned),
        }
    }

    fn blk(kind: BlockKind, children: Vec<Block>) -> Block {
        Block {
            uid: "orig".to_owned(),
            kind,
            children,
        }
    }

    // ── normalization: zero uids + sort marks so a freshly-uid'd, possibly
    //    mark-reordered round trip compares structurally to the original. ──

    fn norm_runs(runs: &[InlineRun]) -> Vec<InlineRun> {
        runs.iter()
            .map(|r| {
                let mut marks = r.marks.clone();
                marks.sort();
                InlineRun {
                    text: r.text.clone(),
                    marks,
                    href: r.href.clone(),
                }
            })
            .collect()
    }

    fn norm_block(b: &Block) -> Block {
        let kind = match &b.kind {
            BlockKind::Paragraph { runs } => BlockKind::Paragraph {
                runs: norm_runs(runs),
            },
            BlockKind::Heading { level, runs } => BlockKind::Heading {
                level: *level,
                runs: norm_runs(runs),
            },
            BlockKind::Quote { runs } => BlockKind::Quote {
                runs: norm_runs(runs),
            },
            other => other.clone(),
        };
        Block {
            uid: String::new(),
            kind,
            children: b.children.iter().map(norm_block).collect(),
        }
    }

    fn normalize(tree: &BlockTree) -> BlockTree {
        BlockTree {
            schema_version: tree.schema_version,
            blocks: tree.blocks.iter().map(norm_block).collect(),
        }
    }

    /// The FULL fidelity loop: `BlockTree` → `DocNode` → (real editor)
    /// `node_from_doc` → `to_doc` → `DocNode` → `BlockTree`. Proves the bridge emits
    /// schema-valid DocNodes AND that content survives a real editor load + save.
    fn round_trip(tree: &BlockTree) -> BlockTree {
        let schema = Schema::starter_kit();
        let doc = block_tree_to_doc(tree);
        let node = schema
            .node_from_doc(&doc)
            .expect("bridge output must be schema-valid");
        let doc2 = node.to_doc().expect("editor doc must serialize");
        doc_to_block_tree(&doc2)
    }

    #[test]
    fn round_trips_prose_with_marks_and_link() {
        let tree = BlockTree::from_blocks(vec![
            blk(
                BlockKind::Heading {
                    level: 2,
                    runs: vec![run("The Sheet", &[], None)],
                },
                vec![],
            ),
            blk(
                BlockKind::Paragraph {
                    runs: vec![
                        run("plain ", &[], None),
                        run("bold", &["bold"], None),
                        run(" and ", &[], None),
                        run("emphasis", &["italic"], None),
                        run(" then a ", &[], None),
                        run("link", &[], Some("https://example.com")),
                    ],
                },
                vec![],
            ),
            blk(
                BlockKind::Quote {
                    runs: vec![run("Keep the soul. Modernize the press.", &[], None)],
                },
                vec![],
            ),
        ]);
        assert_eq!(normalize(&round_trip(&tree)), normalize(&tree));
    }

    #[test]
    fn round_trips_bullet_and_ordered_lists() {
        let item = |t: &str| {
            blk(
                BlockKind::Paragraph {
                    runs: vec![run(t, &[], None)],
                },
                vec![],
            )
        };
        let tree = BlockTree::from_blocks(vec![
            blk(
                BlockKind::List { ordered: false },
                vec![item("first"), item("second")],
            ),
            blk(
                BlockKind::List { ordered: true },
                vec![item("one"), item("two")],
            ),
        ]);
        assert_eq!(normalize(&round_trip(&tree)), normalize(&tree));
    }

    #[test]
    fn round_trips_code_block_including_multiline_and_empty() {
        let tree = BlockTree::from_blocks(vec![
            blk(
                BlockKind::Code {
                    language: Some("rust".to_owned()),
                    source: "fn main() {\n    println!(\"hi\");\n}".to_owned(),
                },
                vec![],
            ),
            blk(
                BlockKind::Code {
                    language: None,
                    source: "plain code".to_owned(),
                },
                vec![],
            ),
        ]);
        assert_eq!(normalize(&round_trip(&tree)), normalize(&tree));
    }

    #[test]
    fn round_trips_image_via_media_url_scheme() {
        let tree = BlockTree::from_blocks(vec![blk(
            BlockKind::Image {
                media: "018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90".to_owned(),
                alt: "a proof on paper".to_owned(),
            },
            vec![],
        )]);
        assert_eq!(normalize(&round_trip(&tree)), normalize(&tree));
    }

    #[test]
    fn round_trips_embed_and_custom_via_sentinel() {
        let tree = BlockTree::from_blocks(vec![
            blk(
                BlockKind::Embed {
                    provider: "youtube".to_owned(),
                    url: "https://youtu.be/abc".to_owned(),
                },
                vec![],
            ),
            blk(
                BlockKind::Custom {
                    plugin: "callout".to_owned(),
                    name: "note".to_owned(),
                    data: serde_json::json!({ "tone": "warn", "body": "careful" }),
                },
                vec![],
            ),
        ]);
        assert_eq!(normalize(&round_trip(&tree)), normalize(&tree));
    }

    #[test]
    fn empty_tree_becomes_a_single_empty_paragraph() {
        // `doc` content is `block+`; the editor is never left block-less.
        let out = round_trip(&BlockTree::from_blocks(vec![]));
        assert_eq!(out.blocks.len(), 1);
        assert!(matches!(
            out.blocks[0].kind,
            BlockKind::Paragraph { ref runs } if runs.is_empty()
        ));
    }

    #[test]
    fn bridge_output_is_schema_valid_for_every_kind() {
        // Each kind converts to a DocNode the real schema accepts (no hard error).
        let schema = Schema::starter_kit();
        let kinds = vec![
            BlockKind::Paragraph {
                runs: vec![run("p", &["bold", "italic", "code"], None)],
            },
            BlockKind::Heading {
                level: 3,
                runs: vec![run("h", &[], None)],
            },
            BlockKind::Quote {
                runs: vec![run("q", &[], None)],
            },
            BlockKind::List { ordered: false },
            BlockKind::Code {
                language: Some("toml".to_owned()),
                source: String::new(),
            },
            BlockKind::Image {
                media: "1".to_owned(),
                alt: String::new(),
            },
            BlockKind::Embed {
                provider: "x".to_owned(),
                url: "https://x".to_owned(),
            },
            BlockKind::Custom {
                plugin: "p".to_owned(),
                name: "n".to_owned(),
                data: serde_json::json!({}),
            },
        ];
        for kind in kinds {
            let tree = BlockTree::from_blocks(vec![blk(kind, vec![])]);
            let doc = block_tree_to_doc(&tree);
            schema
                .node_from_doc(&doc)
                .expect("every kind must produce a schema-valid doc");
        }
    }

    #[test]
    fn unknown_marks_are_dropped_but_text_survives() {
        // "highlight" is a real editor mark but not in Ferropress's rendered set — it
        // must be dropped (like the renderer does) while the text is preserved.
        let tree = BlockTree::from_blocks(vec![blk(
            BlockKind::Paragraph {
                runs: vec![run("keep me", &["highlight", "bold"], None)],
            },
            vec![],
        )]);
        let out = round_trip(&tree);
        let BlockKind::Paragraph { runs } = &out.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(runs[0].text, "keep me");
        assert!(runs[0].marks.contains(&"bold".to_owned()));
        assert!(!runs[0].marks.iter().any(|m| m == "highlight"));
    }

    #[test]
    fn markup_in_text_is_preserved_verbatim_never_interpreted() {
        // The bridge moves DATA, never HTML — a script-looking string stays literal
        // text on both sides (escaping is the renderer's job, proven in that crate).
        let payload = "<script>alert('x')</script> & <b>not bold</b>";
        let tree = BlockTree::from_blocks(vec![blk(
            BlockKind::Paragraph {
                runs: vec![run(payload, &[], None)],
            },
            vec![],
        )]);
        let out = round_trip(&tree);
        let BlockKind::Paragraph { runs } = &out.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(runs[0].text, payload);
        assert!(runs[0].marks.is_empty());
    }

    #[test]
    fn hard_break_degrades_to_a_separator_not_a_word_merge() {
        // Shift+Enter yields a `hard_break` inline atom between two text runs. Dropping
        // it silently would merge the words ("Hello"+"World" → "HelloWorld"); it must
        // degrade to a newline run so the words stay separated.
        let hard_break = DocNode {
            node_type: "hard_break".to_owned(),
            attrs: BTreeMap::new(),
            content: Vec::new(),
            text: None,
            marks: Vec::new(),
        };
        let para = branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("Hello".to_owned(), Vec::new()),
                hard_break,
                text_node("World".to_owned(), Vec::new()),
            ],
        );
        let doc = branch("doc", BTreeMap::new(), vec![para]);
        let tree = doc_to_block_tree(&doc);
        let BlockKind::Paragraph { runs } = &tree.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, "Hello\nWorld");
        assert!(!joined.contains("HelloWorld"), "the words must not merge");
    }

    // ── inline-image splitting: a rinch `image` is an inline atom, but a Ferropress
    //    `Image` is a top-level block. The toolbar's insert-at-caret drops an image
    //    inline; on save the paragraph must SPLIT so no image is lost. Tokens are
    //    hex/uuid so `is_media_token` accepts them (else `parse_media_ref` blanks). ──

    /// An inline `image` DocNode carrying `media_url(token)` as its `src` (exactly what
    /// `block_to_doc` emits and the editor round-trips) plus `alt`.
    fn image_node(token: &str, alt: &str) -> DocNode {
        let mut attrs = BTreeMap::new();
        attrs.insert("src".to_owned(), JsonAttr::Str(media_src(token)));
        attrs.insert("alt".to_owned(), JsonAttr::Str(alt.to_owned()));
        DocNode {
            node_type: "image".to_owned(),
            attrs,
            content: Vec::new(),
            text: None,
            marks: Vec::new(),
        }
    }

    fn doc_of(nodes: Vec<DocNode>) -> DocNode {
        branch("doc", BTreeMap::new(), nodes)
    }

    #[test]
    fn inline_image_mixed_with_text_splits_into_blocks() {
        // Writing a paragraph then inserting an image yields `paragraph > [text, image,
        // text]`. The OLD bridge only recognised a LONE-image paragraph, so this dropped
        // the image; now it splits into prose / Image / prose and nothing is lost.
        let para = branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("Before ".to_owned(), Vec::new()),
                image_node("018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90", "a proof on paper"),
                text_node(" after".to_owned(), Vec::new()),
            ],
        );
        let tree = doc_to_block_tree(&doc_of(vec![para]));
        assert_eq!(tree.blocks.len(), 3);
        assert!(matches!(
            &tree.blocks[0].kind,
            BlockKind::Paragraph { runs } if runs.len() == 1 && runs[0].text == "Before "
        ));
        let BlockKind::Image { media, alt } = &tree.blocks[1].kind else {
            panic!(
                "middle block must be an Image, got {:?}",
                tree.blocks[1].kind
            );
        };
        assert_eq!(media, "018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90");
        assert_eq!(alt, "a proof on paper");
        assert!(matches!(
            &tree.blocks[2].kind,
            BlockKind::Paragraph { runs } if runs.len() == 1 && runs[0].text == " after"
        ));
    }

    #[test]
    fn image_at_paragraph_edges_splits_cleanly() {
        // Leading image: `[image, text]` → Image, Paragraph.
        let lead = doc_of(vec![branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                image_node("cafe", ""),
                text_node("caption".to_owned(), Vec::new()),
            ],
        )]);
        let t = doc_to_block_tree(&lead);
        assert_eq!(t.blocks.len(), 2);
        assert!(matches!(t.blocks[0].kind, BlockKind::Image { .. }));
        assert!(matches!(
            &t.blocks[1].kind,
            BlockKind::Paragraph { runs } if runs[0].text == "caption"
        ));

        // Trailing image: `[text, image]` → Paragraph, Image.
        let trail = doc_of(vec![branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("caption".to_owned(), Vec::new()),
                image_node("cafe", ""),
            ],
        )]);
        let t = doc_to_block_tree(&trail);
        assert_eq!(t.blocks.len(), 2);
        assert!(matches!(
            &t.blocks[0].kind,
            BlockKind::Paragraph { runs } if runs[0].text == "caption"
        ));
        assert!(matches!(t.blocks[1].kind, BlockKind::Image { .. }));
    }

    #[test]
    fn two_inline_images_each_become_their_own_block() {
        let para = branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("a".to_owned(), Vec::new()),
                image_node("cafe", "one"),
                text_node("b".to_owned(), Vec::new()),
                image_node("beef", "two"),
            ],
        );
        let tree = doc_to_block_tree(&doc_of(vec![para]));
        assert_eq!(tree.blocks.len(), 4);
        let tokens: Vec<&str> = tree
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Image { media, .. } => Some(media.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec!["cafe", "beef"]);
        assert!(matches!(tree.blocks[0].kind, BlockKind::Paragraph { .. }));
        assert!(matches!(tree.blocks[2].kind, BlockKind::Paragraph { .. }));
    }

    #[test]
    fn lone_image_paragraph_still_maps_to_a_single_image_block() {
        // The pre-existing lone-image path (an image alone in a paragraph, as
        // `block_to_doc` emits) must keep mapping 1:1 after the split refactor.
        let para = branch(
            "paragraph",
            BTreeMap::new(),
            vec![image_node("cafe", "solo")],
        );
        let tree = doc_to_block_tree(&doc_of(vec![para]));
        assert_eq!(tree.blocks.len(), 1);
        let BlockKind::Image { media, alt } = &tree.blocks[0].kind else {
            panic!("expected a single Image block");
        };
        assert_eq!(media, "cafe");
        assert_eq!(alt, "solo");
    }

    #[test]
    fn whitespace_only_prose_around_an_image_is_dropped() {
        // A lone image formerly tolerated surrounding whitespace (dropping it); the split
        // keeps that — whitespace-only segments must not become blank paragraphs.
        let para = branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("  ".to_owned(), Vec::new()),
                image_node("cafe", ""),
                text_node("   ".to_owned(), Vec::new()),
            ],
        );
        let tree = doc_to_block_tree(&doc_of(vec![para]));
        assert_eq!(
            tree.blocks.len(),
            1,
            "only the Image survives; no blank paragraphs"
        );
        assert!(matches!(tree.blocks[0].kind, BlockKind::Image { .. }));
    }

    #[test]
    fn inline_image_in_a_heading_splits_out_too() {
        // The caret can sit in a heading when the image button is clicked; the same split
        // applies so the image survives as its own block after the heading text.
        let heading = branch(
            "heading",
            attrs1("level", JsonAttr::Int(2)),
            vec![
                text_node("Title".to_owned(), Vec::new()),
                image_node("cafe", ""),
            ],
        );
        let tree = doc_to_block_tree(&doc_of(vec![heading]));
        assert_eq!(tree.blocks.len(), 2);
        let BlockKind::Heading { level, runs } = &tree.blocks[0].kind else {
            panic!("expected a heading");
        };
        assert_eq!(*level, 2);
        assert_eq!(runs[0].text, "Title");
        assert!(matches!(tree.blocks[1].kind, BlockKind::Image { .. }));
    }

    #[test]
    fn inline_image_in_a_blockquote_splits_out_too() {
        // A blockquote is block-content in the editor; inserting an image inside yields
        // `blockquote > paragraph > [text, image, text]`. `Quote` holds only flat runs, so
        // the image is lifted OUT into its own Image block (splitting the quote around it)
        // rather than being flattened away — the case the review caught.
        let quote = branch(
            "blockquote",
            BTreeMap::new(),
            vec![branch(
                "paragraph",
                BTreeMap::new(),
                vec![
                    text_node("look ".to_owned(), Vec::new()),
                    image_node("cafe", "q"),
                    text_node(" here".to_owned(), Vec::new()),
                ],
            )],
        );
        let tree = doc_to_block_tree(&doc_of(vec![quote]));
        assert_eq!(tree.blocks.len(), 3);
        assert!(matches!(
            &tree.blocks[0].kind,
            BlockKind::Quote { runs } if runs[0].text == "look "
        ));
        let BlockKind::Image { media, alt } = &tree.blocks[1].kind else {
            panic!("expected an Image block between the quote fragments");
        };
        assert_eq!(media, "cafe");
        assert_eq!(alt, "q");
        assert!(matches!(
            &tree.blocks[2].kind,
            BlockKind::Quote { runs } if runs[0].text == " here"
        ));
    }

    #[test]
    fn inline_image_in_a_blockquote_survives_the_real_editor_round_trip() {
        // Same fidelity gate as the paragraph case, through the REAL schema: the editor
        // accepts an inline image in a blockquote and the split recovers it on save.
        let schema = Schema::starter_kit();
        let doc = doc_of(vec![branch(
            "blockquote",
            BTreeMap::new(),
            vec![branch(
                "paragraph",
                BTreeMap::new(),
                vec![
                    text_node("look ".to_owned(), Vec::new()),
                    image_node("018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90", "a proof"),
                    text_node(" here".to_owned(), Vec::new()),
                ],
            )],
        )]);
        let node = schema
            .node_from_doc(&doc)
            .expect("editor must accept an inline image inside a blockquote");
        let doc2 = node.to_doc().expect("editor doc must serialize");
        let tree = doc_to_block_tree(&doc2);
        let img_count = tree
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Image { .. }))
            .count();
        assert_eq!(
            img_count, 1,
            "the blockquote image must survive as one Image block"
        );
    }

    #[test]
    fn inline_image_survives_the_real_editor_round_trip() {
        // Against the REAL schema: (a) the editor accepts an inline image mixed with text
        // and (b) our split recovers it from the serialized `DocNode`. Same fidelity gate
        // as `round_trip`, but starting from the mixed shape the toolbar produces.
        let schema = Schema::starter_kit();
        let doc = doc_of(vec![branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("look ".to_owned(), Vec::new()),
                image_node("018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90", "a proof"),
                text_node(" here".to_owned(), Vec::new()),
            ],
        )]);
        let node = schema
            .node_from_doc(&doc)
            .expect("editor must accept an inline image inside a paragraph");
        let doc2 = node.to_doc().expect("editor doc must serialize");
        let tree = doc_to_block_tree(&doc2);
        let imgs: Vec<(String, String)> = tree
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Image { media, alt } => Some((media.clone(), alt.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            imgs,
            vec![(
                "018f3c2a-7b19-7c44-9e0d-2a1f6b8e5d90".to_owned(),
                "a proof".to_owned()
            )]
        );
        // The surrounding words are preserved on both sides (order via the block run).
        let prose: String = tree
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Paragraph { runs } => {
                    Some(runs.iter().map(|r| r.text.clone()).collect::<String>())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(prose, "look | here");
    }

    #[test]
    fn split_is_stable_image_gets_its_own_paragraph_on_reload() {
        // After save #1 the mixed paragraph becomes [Para, Image, Para]. Loading that back
        // (`block_to_doc` wraps each Image in its own paragraph) and saving again must
        // yield the IDENTICAL tree — the split converges, it doesn't churn every save.
        let mixed = doc_of(vec![branch(
            "paragraph",
            BTreeMap::new(),
            vec![
                text_node("Before ".to_owned(), Vec::new()),
                image_node("cafe", "x"),
                text_node(" after".to_owned(), Vec::new()),
            ],
        )]);
        let first = doc_to_block_tree(&mixed);
        // Loading + re-saving through the REAL editor is stable (structurally identical).
        assert_eq!(normalize(&round_trip(&first)), normalize(&first));
        // And on load the image is its own lone-image paragraph, not inline any more.
        let reloaded = block_tree_to_doc(&first);
        assert_eq!(reloaded.content.len(), 3);
        assert!(reloaded.content.iter().any(|n| {
            n.node_type == "paragraph" && n.content.len() == 1 && n.content[0].node_type == "image"
        }));
    }
}
