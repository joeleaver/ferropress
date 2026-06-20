//! Small validated value objects shared across entities.

/// A URL slug. TODO: enforce `^[a-z0-9]+(?:-[a-z0-9]+)*$` on construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Slug(String);

impl Slug {
    pub fn parse(_s: &str) -> crate::error::Result<Self> {
        todo!("validate slug charset + non-empty; lower-case-normalize")
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The block-tree format version carried in `BlockTree`. Re-exported newtype for
/// call sites that pass it around independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaVersion(pub u32);
