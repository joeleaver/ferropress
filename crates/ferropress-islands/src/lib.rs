//! # ferropress-islands
//!
//! The public site's interactive **rinch islands**: small, independently-mounted
//! components (a semantic-search box, live comments, …) that hydrate into the
//! otherwise-static prerendered HTML. They are the only client-side interactivity
//! on the public site; the page chrome and content arrive as prerendered HTML
//! from `ferropress-render` + `ferropress-theme`, and each island attaches to its
//! own root element.
//!
//! ## rinch implementation pending upstream
//!
//! Islands require mounting several independent rinch roots into one already-
//! rendered document, which is upstream **rinch #53** (islands multi-root mount).
//! That is the furthest-out of the rinch blockers, so this crate is deliberately
//! the most minimal of the three rinch-facing placeholders.
//!
//! Until #53 lands this crate is a **rinch-FREE placeholder**: NO rinch /
//! rinch-web dependency, no cdylib, no wasm target — just enough to establish the
//! crate in the workspace and keep everything compiling on the host. The island
//! enumeration and `mount_islands()` shape below are real so the rest of the
//! system can reference them; the bodies are stubs.

/// The set of public-site islands. Each variant becomes an independently-mounted
/// rinch root (multi-root mount = rinch #53) once the rinch dependency is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Island {
    /// Semantic-search box (backed by `RhypeStore::vector_search` via the island
    /// API in `ferropress-http`).
    SemanticSearch,
    /// Live comments widget.
    LiveComments,
}

/// CSS-selector mount point convention: each island hydrates into the element
/// carrying this `data-` attribute, e.g. `<div data-ferropress-island="...">`.
pub const ISLAND_MOUNT_ATTR: &str = "data-ferropress-island";

/// Hydrate every island present in the prerendered document. PLACEHOLDER shape of
/// the future rinch multi-root mount: once rinch #53 lands this scans the DOM for
/// `ISLAND_MOUNT_ATTR` roots and mounts the matching island component into each.
pub fn mount_islands() -> ferropress_core::Result<()> {
    // Keep the island enumeration live until the rinch impl lands.
    let _islands = [Island::SemanticSearch, Island::LiveComments];
    // TODO(rinch #53): for each mount-point element, mount the matching island as
    // an independent rinch root via `rinch-web` multi-root mount.
    todo!("mount public-site rinch islands into prerendered HTML (pending rinch #53)")
}
