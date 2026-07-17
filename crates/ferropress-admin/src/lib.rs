//! # ferropress-admin
//!
//! The Ferropress admin + editor SPA — a rinch WASM app mounted whole-page via
//! `rinch_web::mount`. Login → a content list → the rich-text editor (shared across
//! flat Posts and hierarchical Pages), plus Settings/Plugins; all call the rinch-free
//! admin API in `ferropress-http` (`/admin/api/*`), and the HttpOnly session cookie
//! rides along automatically on same-origin fetches.
//!
//! Content crosses the wire as Ferropress `BlockTree` JSON and is converted to/from
//! the rinch editor's `DocNode` by `ferropress-editor-bridge`. The public site's
//! WYSIWYG parity comes for free: the editor and the public renderer share the same
//! `BlockKind` vocabulary (the bridge only round-trips marks both understand).
//!
//! Built to a wasm32 `cdylib` by `cargo xtask build-admin` and served at
//! `/_fp/admin`; the server-rendered shell at `/admin` boots it. See the crate
//! `Cargo.toml` for why this lives outside the host workspace.

mod api;
mod app;
mod styles;

use rinch_core::element::ThemeProviderProps;
use wasm_bindgen::prelude::*;

/// WASM entry point: install the panic hook, inject the admin stylesheet, then mount
/// the whole-page rinch app (which decides login vs list from the session cookie).
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    styles::inject_admin_styles();
    rinch_web::mount(ThemeProviderProps::default(), app::app);
}
