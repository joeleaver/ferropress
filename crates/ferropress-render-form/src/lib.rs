//! # ferropress-render-form
//!
//! THE single `form-schema (JSON) -> edit-UI` renderer for the admin — the exact
//! mirror of `ferropress-render` (block-tree -> HTML), but for the editing side.
//! A declarative widget schema describes each editable field; this crate turns it
//! into the admin's edit UI. ARCHITECTURE INVARIANT (mirror of the render rule):
//! there is exactly ONE `WidgetKind -> edit-UI` dispatch in the entire workspace
//! and it lives here. CI greps for the `FERROPRESS-FORM-DISPATCH` marker to forbid
//! a second implementation anywhere else.
//!
//! ## rinch implementation pending upstream
//!
//! The real implementation renders the widget schema into a **rinch** component
//! tree (the admin SPA is a rinch WASM app). That work is BLOCKED on, and will
//! land together with, these upstream rinch issues:
//!   - rinch #50 — content-editor (CE) serde, so editor state round-trips JSON.
//!   - rinch #51 — content-editor in the WASM-DOM backend, so the editor runs in
//!     the browser at all.
//!
//! Until those land, this crate is a **rinch-FREE placeholder**: it carries NO
//! rinch dependency so the whole workspace compiles on the host. The output type
//! (`EditUi`) is a local placeholder that will be replaced by the rinch view node
//! once the rinch dep is added (alongside a wasm target). The signatures here are
//! real and coherent so dependents (`ferropress-admin`) can already name them.

use ferropress_core::block::BlockTree;
// The form renderer shares the public render path's mode so the in-editor
// preview embedded in the edit UI matches what gets published (WYSIWYG parity).
// This also makes the `ferropress-render` dependency load-bearing rather than
// decorative.
use ferropress_render::RenderMode;

/// The declarative description of one editable widget in the admin form. This is
/// the *data* enum; the single widget -> edit-UI dispatch lives in
/// [`FormSchemaRenderer::render_widget`] (NOT inline at call sites) — keeping the
/// schema render-agnostic is what makes "one form renderer" enforceable.
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetKind {
    /// Single-line text input (titles, slugs, meta fields).
    Text { label: String },
    /// Multi-line text input.
    TextArea { label: String },
    /// Boolean toggle.
    Toggle { label: String },
    /// Closed choice from a fixed option list.
    Select { label: String, options: Vec<String> },
    /// Media picker (resolves to a `Media` object id).
    MediaPicker { label: String },
    /// The block-tree editor itself — the rich content body widget.
    BlockEditor { label: String },
}

/// A full form schema: an ordered list of widgets describing one editable entity
/// (a `Post`, `Page`, `Setting`, …). Mirrors `BlockTree` on the render side.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormSchema {
    pub widgets: Vec<WidgetKind>,
}

/// Opaque rendered edit UI. PLACEHOLDER: today a wrapped `String`; once the rinch
/// dependency is added (pending rinch #50/#51) this becomes the rinch view node
/// the admin SPA mounts. Kept as a newtype so a raw `String` cannot be mistaken
/// for render-ready edit UI.
#[derive(Debug, Clone, PartialEq)]
pub struct EditUi(pub String);

/// THE form-schema -> edit-UI renderer. Mirror of `ferropress_render::render`.
#[derive(Debug, Clone, Default)]
pub struct FormSchemaRenderer;

impl FormSchemaRenderer {
    pub fn new() -> Self {
        Self
    }

    /// Render a whole form schema to its edit UI. The single public entry point;
    /// dispatches each widget through [`Self::render_widget`]. `mode` mirrors the
    /// public render path so a `BlockEditor` widget can host a live preview that
    /// matches the published output.
    pub fn render(&self, _schema: &FormSchema, _mode: RenderMode) -> EditUi {
        // TODO(rinch #50/#51): walk schema.widgets, dispatch each via
        // render_widget, assemble into the rinch view node. Placeholder until the
        // rinch dependency + wasm target are added.
        todo!("walk form schema -> edit UI via the single dispatch in render_widget")
    }

    /// Render a single widget. This `match` is THE form-schema -> edit-UI
    /// dispatch; it must exist in exactly one place in the workspace.
    pub fn render_widget(&self, _widget: &WidgetKind) -> EditUi {
        // FERROPRESS-FORM-DISPATCH  (do not duplicate this match elsewhere)
        // match widget {
        //     WidgetKind::Text { label }       => text_input(label),
        //     WidgetKind::TextArea { label }    => textarea(label),
        //     WidgetKind::Toggle { label }      => toggle(label),
        //     WidgetKind::Select { label, .. }  => select(label, options),
        //     WidgetKind::MediaPicker { label } => media_picker(label),
        //     WidgetKind::BlockEditor { label } => block_editor(label),  // rinch CE
        // }
        let _ = WidgetKind::Toggle {
            label: String::new(),
        }; // keep enum live until impl
        todo!("render one widget into the rinch edit UI (pending rinch #50/#51)")
    }
}

/// Derive the editable form schema for a content body, given its persisted block
/// tree. The admin uses this to seed the editor. PLACEHOLDER signature so
/// dependents can name it; real mapping lands with the rinch editor.
pub fn schema_for_body(_body: &BlockTree) -> FormSchema {
    // TODO(rinch #50/#51): map the block tree + entity field set to widgets.
    todo!("derive form schema from a content body (pending rinch CE serde)")
}
