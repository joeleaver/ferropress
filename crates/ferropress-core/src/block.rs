//! The typed JSON block tree — Ferropress's content body representation.
//!
//! ARCHITECTURE INVARIANT: the canonical block tree is stored as a native
//! **`Value::Json`** field in the database (rhypedb gained write-capable `Json`
//! values at rev `2a9bf28`; it was previously a JSON `String` only because
//! rhypedb's `Json` scalar was write-dead at the time — `String` was the only
//! way to round-trip JSON through the store). Storing it as JSON means it flows
//! through the JSON API
//! boundary without double-encoding — the editor reads `"block_tree": {…}`, not
//! an escaped `"{\"blocks\":…}"` string. Every block carries a stable UID so
//! edits/diffs/revisions can address individual blocks across versions, and the
//! tree carries a `schema_version` from commit #1 so the format can evolve with
//! an explicit migration rather than ambiguous best-effort parsing.
//!
//! This model is defined INDEPENDENTLY of rinch's content-editor types
//! (`BlockData`/`InlineRunData`/…). Core has no rinch dependency; the admin SPA
//! serializes rinch's editor state into this shape over the wire. (rinch CE
//! serde is rinch issue #50, in-flight upstream — but core does not wait on it.)

use std::collections::BTreeSet;

/// Bumped whenever the on-the-wire block JSON shape changes incompatibly. Stored
/// in every `BlockTree` so a reader can refuse / migrate older trees explicitly.
pub const BLOCK_SCHEMA_VERSION: u32 = 1;

/// The opaque, validated wrapper around the canonical block tree as persisted
/// (a native `Value::Json`). Construct via `from_blocks` (stamps the version) or
/// `from_json_value` (validates it parses + version-checks). The renderer
/// (`ferropress-render`) is the only consumer that walks the parsed form.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlockTree {
    pub schema_version: u32,
    pub blocks: Vec<Block>,
}

impl BlockTree {
    /// Build a tree from blocks, stamping the current schema version.
    pub fn from_blocks(blocks: Vec<Block>) -> Self {
        Self {
            schema_version: BLOCK_SCHEMA_VERSION,
            blocks,
        }
    }

    /// Parse + validate the persisted `Value::Json` form.
    pub fn from_json_value(value: serde_json::Value) -> crate::error::Result<Self> {
        let tree: BlockTree = serde_json::from_value(value)?;
        // TODO: if tree.schema_version > BLOCK_SCHEMA_VERSION -> Validation error;
        // if older, route through a registered block-tree migration.
        Ok(tree)
    }

    /// Serialize to a `serde_json::Value` for storage in a `Value::Json` field.
    pub fn to_json_value(&self) -> crate::error::Result<serde_json::Value> {
        Ok(serde_json::to_value(self)?)
    }

    /// Flatten the tree to its plain reading text — the projection the persisted
    /// `plaintext` field carries for `@vectorize` semantic search. Every editor
    /// save re-derives this so the search index tracks the edited body. Prose runs
    /// (paragraph / heading / quote / list items) and code/image-alt contribute;
    /// structural-only kinds (`List` wrapper, `Embed`, `Custom`) do not (their
    /// prose, if any, rides on child blocks). Blocks are separated by newlines.
    pub fn plaintext(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            block.push_plaintext(&mut out);
        }
        out.trim().to_owned()
    }

    /// The set of plugin ids referenced by [`Custom`](BlockKind::Custom) blocks
    /// anywhere in the tree.
    ///
    /// A custom block's rendered output is BAKED into the prerendered page HTML (the
    /// owning plugin may read its own configuration at render time via
    /// `fp_get_setting`), unlike site chrome which is composed live. The serve layer
    /// uses this to answer "which pages bake output from plugin X?" so a change to
    /// that plugin's `plugin.X.*` config can invalidate exactly those pages.
    ///
    /// A `BTreeSet` (deduped + ordered) so repeated uses of one plugin collapse and
    /// the result is deterministic for tests/logs. Note the asymmetry that governs
    /// the walk: OVER-collection would only cost a harmless cache rebuild, whereas
    /// UNDER-collection would leave a page serving stale baked HTML — so the
    /// recursion into `children` is UNCONDITIONAL (see [`Block::collect_plugin_ids`]).
    pub fn referenced_plugin_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for block in &self.blocks {
            block.collect_plugin_ids(&mut ids);
        }
        ids
    }
}

impl Block {
    /// Append this block's reading text (then recurse into children) to `out`.
    fn push_plaintext(&self, out: &mut String) {
        match &self.kind {
            BlockKind::Paragraph { runs }
            | BlockKind::Heading { runs, .. }
            | BlockKind::Quote { runs } => {
                for run in runs {
                    out.push_str(&run.text);
                }
                out.push('\n');
            }
            BlockKind::Code { source, .. } => {
                out.push_str(source);
                out.push('\n');
            }
            BlockKind::Image { alt, .. } if !alt.is_empty() => {
                out.push_str(alt);
                out.push('\n');
            }
            // `List` holds its items as children; `Embed`/`Custom` carry no prose
            // to index (a URL / opaque plugin payload). `Image` with empty alt: nil.
            BlockKind::Image { .. }
            | BlockKind::List { .. }
            | BlockKind::Embed { .. }
            | BlockKind::Custom { .. } => {}
        }
        for child in &self.children {
            child.push_plaintext(out);
        }
    }

    /// Collect this block's plugin id (when it is a [`Custom`](BlockKind::Custom)
    /// block) then recurse into EVERY child, mirroring [`Block::push_plaintext`]'s
    /// traversal shape: the `for child in &self.children` recursion runs
    /// unconditionally AFTER inspecting `kind`. That is load-bearing — a `Custom`
    /// block can nest arbitrary blocks (including another plugin's `Custom` block)
    /// in its own `children`, and missing a nested plugin id would leave that
    /// plugin's page stale on a config change (the one failure this feature exists
    /// to prevent).
    fn collect_plugin_ids(&self, ids: &mut BTreeSet<String>) {
        if let BlockKind::Custom { plugin, .. } = &self.kind {
            ids.insert(plugin.clone());
        }
        for child in &self.children {
            child.collect_plugin_ids(ids);
        }
    }
}

/// A single block in the tree. `uid` is stable across edits.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Block {
    /// Stable per-block identifier (UUIDv7 string). Never reused, never
    /// reassigned on edit.
    pub uid: String,
    pub kind: BlockKind,
    /// Child blocks (e.g. list items, columns). Empty for leaf blocks.
    #[serde(default)]
    pub children: Vec<Block>,
}

/// The discriminant of a block. This is the *data* enum; the single
/// block->HTML dispatch lives in `ferropress-render` (NOT here — keeping the
/// data model render-agnostic is what makes "one renderer" enforceable).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Paragraph {
        runs: Vec<InlineRun>,
    },
    Heading {
        level: u8,
        runs: Vec<InlineRun>,
    },
    Image {
        /// The referenced `Media`'s `uuid` (its unguessable, stable public handle) —
        /// NOT the sequential object id, so `/media/{media}` URLs aren't enumerable.
        /// A single URL-safe path segment; see [`crate::is_media_token`].
        media: String,
        alt: String,
    },
    Quote {
        runs: Vec<InlineRun>,
    },
    List {
        ordered: bool,
    },
    Code {
        language: Option<String>,
        source: String,
    },
    Embed {
        provider: String,
        url: String,
    },
    /// Escape hatch for plugin-defined block types (Tier-1): carries an opaque
    /// JSON payload the owning plugin understands. Rendered via a plugin hook.
    Custom {
        plugin: String,
        name: String,
        data: serde_json::Value,
    },
}

/// An inline text run with optional marks (bold/italic/link/…). Kept minimal;
/// the editor maps richer rinch inline state down to this.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InlineRun {
    pub text: String,
    #[serde(default)]
    pub marks: Vec<String>,
    /// Present when the run is a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> InlineRun {
        InlineRun {
            text: text.to_owned(),
            marks: Vec::new(),
            href: None,
        }
    }
    fn block(kind: BlockKind, children: Vec<Block>) -> Block {
        Block {
            uid: "u".to_owned(),
            kind,
            children,
        }
    }

    #[test]
    fn plaintext_flattens_prose_and_recurses_children() {
        let tree = BlockTree::from_blocks(vec![
            block(
                BlockKind::Heading {
                    level: 1,
                    runs: vec![run("Title")],
                },
                vec![],
            ),
            block(
                BlockKind::Paragraph {
                    runs: vec![run("Hello "), run("world")],
                },
                vec![],
            ),
            block(
                BlockKind::Quote {
                    runs: vec![run("A quote")],
                },
                vec![],
            ),
            // A list's items ride on children; the wrapper itself has no prose.
            block(
                BlockKind::List { ordered: false },
                vec![
                    block(
                        BlockKind::Paragraph {
                            runs: vec![run("first")],
                        },
                        vec![],
                    ),
                    block(
                        BlockKind::Paragraph {
                            runs: vec![run("second")],
                        },
                        vec![],
                    ),
                ],
            ),
            block(
                BlockKind::Code {
                    language: None,
                    source: "let x = 1;".to_owned(),
                },
                vec![],
            ),
            block(
                BlockKind::Image {
                    media: "a-cat-uuid".to_owned(),
                    alt: "a cat".to_owned(),
                },
                vec![],
            ),
            // No prose to index.
            block(
                BlockKind::Embed {
                    provider: "y".to_owned(),
                    url: "http://x".to_owned(),
                },
                vec![],
            ),
            block(
                BlockKind::Custom {
                    plugin: "p".to_owned(),
                    name: "n".to_owned(),
                    data: serde_json::json!({}),
                },
                vec![],
            ),
        ]);

        let text = tree.plaintext();
        assert_eq!(
            text,
            "Title\nHello world\nA quote\nfirst\nsecond\nlet x = 1;\na cat"
        );
    }

    #[test]
    fn plaintext_of_empty_tree_is_empty() {
        assert_eq!(BlockTree::from_blocks(vec![]).plaintext(), "");
    }

    fn custom(plugin: &str, name: &str, children: Vec<Block>) -> Block {
        block(
            BlockKind::Custom {
                plugin: plugin.to_owned(),
                name: name.to_owned(),
                data: serde_json::json!({}),
            },
            children,
        )
    }

    #[test]
    fn referenced_plugin_ids_recurses_children_including_nested_custom() {
        // The load-bearing case: a plugin-B `Custom` block nested inside a plugin-A
        // `Custom` block's OWN children, plus a plugin-C `Custom` buried under a
        // List -> Quote child chain, plus a duplicate plugin-A to prove dedup. A
        // walk that stopped recursing at a `Custom` block would miss `beta`.
        let tree = BlockTree::from_blocks(vec![
            custom("alpha", "a", vec![custom("beta", "b", vec![])]),
            block(
                BlockKind::List { ordered: false },
                vec![block(
                    BlockKind::Quote {
                        runs: vec![run("q")],
                    },
                    vec![custom("gamma", "c", vec![])],
                )],
            ),
            custom("alpha", "a-again", vec![]),
        ]);

        // BTreeSet -> sorted + deduped.
        assert_eq!(
            tree.referenced_plugin_ids().into_iter().collect::<Vec<_>>(),
            vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()],
        );
    }

    #[test]
    fn referenced_plugin_ids_empty_without_custom_blocks() {
        let tree = BlockTree::from_blocks(vec![block(
            BlockKind::Paragraph {
                runs: vec![run("no plugins here")],
            },
            vec![],
        )]);
        assert!(tree.referenced_plugin_ids().is_empty());
        assert!(
            BlockTree::from_blocks(vec![])
                .referenced_plugin_ids()
                .is_empty()
        );
    }
}
