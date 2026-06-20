//! # ferropress-admin
//!
//! The Ferropress admin + editor SPA. The finished crate is a **rinch** WASM
//! application mounted into the browser via `rinch-web::mount()`, with
//! `crate-type = ["cdylib"]` and a `wasm32-unknown-unknown` target. Its editor
//! preview deliberately calls the SAME `ferropress-render` (block-tree -> HTML)
//! path as the public site, so what an author sees is exactly what gets
//! published (WYSIWYG parity); its edit UI comes from `ferropress-render-form`.
//!
//! ## rinch implementation pending upstream
//!
//! The rinch SPA cannot be built until these upstream rinch issues land:
//!   - rinch #50 — content-editor (CE) serde (editor state <-> JSON over the wire).
//!   - rinch #51 — content-editor in the WASM-DOM backend (the editor in-browser).
//!
//! Until then this crate is a **rinch-FREE placeholder**: NO rinch / rinch-web
//! dependency, no cdylib, no wasm target — just enough structure to establish the
//! crate in the workspace and keep everything compiling on the host. The
//! `app()` entry point below has the real shape the rinch mount will call; its
//! body is a stub. Once rinch is wired up, `app()` becomes the rinch root
//! component and `mount()` hands it to `rinch-web`.

use ferropress_render::RenderMode;
use ferropress_render_form::FormSchemaRenderer;

/// Placeholder for the admin SPA's root view. Once rinch lands this becomes the
/// rinch view node (the root component) that `rinch-web::mount()` renders into
/// the document. Newtype so the eventual swap to the rinch type is contained.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdminApp;

/// Build the admin SPA root. PLACEHOLDER shape of the future rinch entry point:
/// `rinch-web::mount()` will call this to obtain the root component. Editor
/// preview will render through `ferropress_render::render(.., RenderMode::Preview)`
/// (the same path the public site uses) and the edit UI through
/// `ferropress_render_form::FormSchemaRenderer`.
pub fn app() -> AdminApp {
    // Keep the cross-crate wiring intent live until the rinch impl lands: the
    // editor preview uses the public render path, the forms use the one form
    // renderer.
    let _preview_mode = RenderMode::Preview;
    let _forms = FormSchemaRenderer::new();
    // TODO(rinch #50/#51): assemble the rinch admin/editor component tree and
    // return it as the root node for `rinch-web::mount()`.
    todo!("build the rinch admin SPA root (pending rinch #50/#51)")
}
