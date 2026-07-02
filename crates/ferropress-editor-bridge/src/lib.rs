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
//! - **Editor-only constructs** (horizontal rule, tables, task-list checkbox state,
//!   inline images mixed with text, multi-block list items) have no Ferropress
//!   representation. The MVP toolbar cannot create them; a markdown-input-rule one
//!   degrades gracefully (dropped / flattened) rather than corrupting surrounding
//!   prose. A `hard_break` (Shift+Enter) flattens to a newline run so it reads as
//!   whitespace without merging the words it separated.

use std::collections::BTreeMap;

use ferropress_core::block::{Block, BlockKind, BlockTree, InlineRun};
use rinch_editor_core::serialize::{DocMark, DocNode, JsonAttr};
use rinch_editor_core::{Node, Schema};
use uuid::Uuid;

// ── The `src` convention for images + the Embed/Custom sentinels ─────────────────

/// An editor `image` node's `src` is the media original's REAL served URL
/// ([`ferropress_core::media_url`] → `/media/{id}`), so the live editor actually
/// displays the image — and it is the SAME URL the public page renders (the serve
/// layer rewrites `data-media-id` into it), so the in-editor preview never disagrees
/// with the published page. On save the id is recovered from that URL
/// ([`ferropress_core::media_id_from_url`]); the persisted `BlockKind::Image` keeps
/// only the opaque `media_id`, never a URL.

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
    let blocks = doc.content.iter().filter_map(doc_node_to_block).collect();
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
/// sentinel, to be revived verbatim by [`doc_node_to_block`].
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

fn doc_node_to_block(node: &DocNode) -> Option<Block> {
    let kind = match node.node_type.as_str() {
        "paragraph" => match lone_image(&node.content) {
            Some(img) => BlockKind::Image {
                media: parse_media_ref(&attr_str(img, "src")),
                alt: attr_str(img, "alt"),
            },
            None => BlockKind::Paragraph {
                runs: inline_to_runs(&node.content),
            },
        },

        "heading" => BlockKind::Heading {
            level: attr_int(node, "level").unwrap_or(1).clamp(1, 6) as u8,
            runs: inline_to_runs(&node.content),
        },

        "blockquote" => BlockKind::Quote {
            runs: collect_prose_runs(&node.content),
        },

        "bullet_list" => return Some(list_block(false, node)),
        "ordered_list" => return Some(list_block(true, node)),
        // A task list has no Ferropress equivalent; degrade to a plain list (the
        // per-item checkbox state is lost, documented). The toolbar can't create one.
        "task_list" => return Some(list_block(false, node)),

        "code_block" => {
            let lang = attr_str(node, "language");
            let source = code_text(&node.content);
            if lang == SENTINEL_EMBED
                && let Some(block) = revive_embed(&source)
            {
                return Some(block);
            }
            if lang == SENTINEL_CUSTOM
                && let Some(block) = revive_custom(&source)
            {
                return Some(block);
            }
            BlockKind::Code {
                language: if lang.is_empty() { None } else { Some(lang) },
                source,
            }
        }

        // horizontal_rule / table / hard_break-as-block / anything unknown: no
        // Ferropress representation. Drop rather than corrupt surrounding prose.
        _ => return None,
    };
    Some(new_block(kind, Vec::new()))
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
            if let Some(b) = doc_node_to_block(block_child) {
                children.push(b);
            }
        }
    }
    new_block(BlockKind::List { ordered }, children)
}

/// Flatten a blockquote's block content to a single run list — Ferropress's `Quote`
/// holds flat inline runs (the renderer puts them directly inside `<blockquote>`).
/// A single-paragraph quote (the common case) round-trips exactly; multiple
/// paragraphs are concatenated (documented).
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

/// The single image node in `content`, iff `content` is exactly one image plus only
/// whitespace text (so a lone-image paragraph maps to a Ferropress `Image` block).
fn lone_image(content: &[DocNode]) -> Option<&DocNode> {
    let mut image = None;
    for n in content {
        match n.node_type.as_str() {
            "image" => {
                if image.is_some() {
                    return None;
                }
                image = Some(n);
            }
            "text" => {
                if !n.text.as_deref().unwrap_or("").trim().is_empty() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    image
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
}
