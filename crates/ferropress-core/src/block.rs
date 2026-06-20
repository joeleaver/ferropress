//! The typed JSON block tree — Ferropress's content body representation.
//!
//! ARCHITECTURE INVARIANT: the canonical block tree is stored as a JSON
//! **String** in the database (not `Bytes`, not the decorative `Json` scalar —
//! both verified write-dead in rhypedb). Every block carries a stable UID so
//! edits/diffs/revisions can address individual blocks across versions, and the
//! tree carries a `schema_version` from commit #1 so the format can evolve with
//! an explicit migration rather than ambiguous best-effort parsing.
//!
//! This model is defined INDEPENDENTLY of rinch's content-editor types
//! (`BlockData`/`InlineRunData`/…). Core has no rinch dependency; the admin SPA
//! serializes rinch's editor state into this shape over the wire. (rinch CE
//! serde is rinch issue #50, in-flight upstream — but core does not wait on it.)

/// Bumped whenever the on-the-wire block JSON shape changes incompatibly. Stored
/// in every `BlockTree` so a reader can refuse / migrate older trees explicitly.
pub const BLOCK_SCHEMA_VERSION: u32 = 1;

/// The opaque, validated wrapper around the canonical block-tree JSON string as
/// persisted. Construct via `from_blocks` (serializes + stamps the version) or
/// `from_json_str` (validates it parses + version-checks). The renderer
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

    /// Parse + validate the persisted JSON string form.
    pub fn from_json_str(s: &str) -> crate::error::Result<Self> {
        let tree: BlockTree = serde_json::from_str(s)?;
        // TODO: if tree.schema_version > BLOCK_SCHEMA_VERSION -> Validation error;
        // if older, route through a registered block-tree migration.
        Ok(tree)
    }

    /// Serialize to the canonical JSON string for storage in a `Value::String`.
    pub fn to_json_string(&self) -> crate::error::Result<String> {
        Ok(serde_json::to_string(self)?)
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
        media_id: u64,
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
