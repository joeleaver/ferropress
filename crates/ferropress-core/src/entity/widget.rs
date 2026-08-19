//! `Widget` — a classic typed sidebar/rail widget. A relation-free row: `area`
//! binds it to a theme-declared area key (or the reserved `_inactive` parking
//! area), `widget_order` is its ordinal within that area, `kind` is the stable
//! snake_case registry id (`"text"`, `"recent_posts"`, …) resolved against the
//! serve-layer `widget_specs()` registry, and `config` is the per-kind settings
//! blob (schema-validated + document-replaced at admin write time).
//!
//! Unlike `Menu`/`MenuItem`, `Widget` carries NO SDL relations: a config value
//! that references another entity (a menu id for the Nav Menu kind, a media id
//! for Image) is stored as a plain id inside the `config` `Json` blob, never an
//! edge. This trades `@on_delete` referential integrity for eventfulness —
//! every mutation (create/update/delete/reorder) is a store event, so a
//! full-reload `WidgetHandle` (serving layer, a later increment) always
//! converges with no settle-write, and a dangling referenced id degrades to an
//! absent widget at compose time rather than corrupting a link.
//!
//! `title` is a plain entity column, never a `config` key — the host renders
//! one Title input per widget row from it, so the value can never fork between
//! two sources. Field names mirror the SDL columns 1:1 (`area`, `widget_order`,
//! `kind`, `title`, `config`, `meta`) — the store hand-builds `FieldMap`s from
//! literal SDL keys, so a struct field renamed out of step with the SDL would
//! silently address the wrong column.

use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    pub id: Option<ObjectId>,
    /// Theme-declared area key, or the reserved `_inactive` parking area.
    /// Otherwise permissive (no `WidgetArea` catalog) — stranded rows (an area
    /// no longer declared by the active theme) are surfaced, not dropped.
    pub area: String,
    /// Ordinal WITHIN `area` (contiguous `0..n` after a reorder write; may go
    /// non-contiguous after a delete). Load/compose sort on `(area,
    /// widget_order, id)` — `id` is the deterministic tiebreak for two widgets
    /// sharing an ordinal.
    pub widget_order: i32,
    /// Stable snake_case registry id (`"text"`, `"custom_html"`, …). An id not
    /// in the current `widget_specs()` registry (a row from a newer/older
    /// build) is preserved verbatim, never dropped or rewritten.
    pub kind: String,
    /// Plain-text display title. Never stored inside `config`.
    pub title: String,
    /// Per-kind settings blob, shaped by that kind's `FormSchema`.
    pub config: serde_json::Value,
    pub meta: serde_json::Value,
}
