//! # ferropress-islands
//!
//! The public site's interactive **rinch islands** — small, independently-mounted
//! reactive components hydrated into the otherwise-static prerendered HTML. They
//! are the only client-side interactivity on the public site; page chrome +
//! content arrive as prerendered HTML, and each island mounts into its own
//! placeholder element via `rinch_web::mount_selector`.
//!
//! Two islands:
//!   * [`comments::comments_island`] → `#fp-comments` — live comments over
//!     `/api/comments` (list approved + post a pending comment).
//!   * [`search::search_island`] → `#fp-search` — semantic search over
//!     `/api/search`.
//!
//! Built to a wasm32 `cdylib` by `cargo xtask build-islands` and served at
//! `/_fp/islands`; the page chrome emits the placeholder elements + the ESM
//! `<script>` that calls [`start`]. See the crate `Cargo.toml` for why this lives
//! outside the host workspace. The islands use raw HTML elements + a single
//! injected stylesheet (driven by rinch theme vars) rather than the heavyweight
//! rinch component library, so they blend into any host theme.

mod api;
mod comments;
mod search;

use rinch_core::element::ThemeProviderProps;
use wasm_bindgen::prelude::*;

/// WASM entry point: inject the island stylesheet, then mount each island into
/// its placeholder element (a no-op when an element is absent, so a page that
/// omits one island simply doesn't get it).
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    api::inject_island_styles();

    let theme = ThemeProviderProps::default();
    rinch_web::mount_selector("#fp-search", theme.clone(), search::search_island);
    rinch_web::mount_selector("#fp-comments", theme, comments::comments_island);
}
