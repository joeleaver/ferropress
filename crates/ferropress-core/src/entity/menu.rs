//! `Menu` + `MenuItem` + `MenuLocation` — dedicated typed nav menus, replacing
//! WP's post-row + nav_menu-taxonomy + 6-postmeta-keys overload.
//!
//! A `Menu` is a named, ordered tree of `MenuItem`s with a typed [`LinkTarget`]
//! union (internal ref / external URL / taxonomy term). Which theme *location*
//! (`"primary"`, `"footer"`, …) a menu appears in is NOT a property of the menu:
//! following WordPress's `nav_menu_locations`, a separate [`MenuLocation`]
//! assignment binds a location to a menu, so ONE menu can be shown in MANY
//! locations, and a location holds at most one menu (`location` is `@unique`).

use crate::value::ObjectId;

/// A named nav menu — a container for an ordered [`MenuItem`] tree. The menu
/// itself carries no location; a [`MenuLocation`] assignment binds it to one or
/// more theme locations.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub id: Option<ObjectId>,
    pub slug: String,
    pub name: String,
    pub meta: serde_json::Value,
}

/// Where a menu item points. A typed union instead of WP's
/// `_menu_item_type`/`_menu_item_object`/`_menu_item_object_id` meta triple.
///
/// Serialized (tagged) into the `MenuItem.target` `String` column — the SDL has
/// no union scalar — and read back the same way, so the wire form and the stored
/// form are one type. `Term` targets a taxonomy term (resolvable once taxonomies
/// land); the other three resolve today.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkTarget {
    Post { id: u64 },
    Page { id: u64 },
    Term { id: u64 },
    Custom { url: String },
}

/// One item in a menu. `item_order` is the ordinal WITHIN its `(menu, parent)`
/// sibling group (contiguous `0..n`); `parent` nests it under another item in the
/// same menu. The field name mirrors the SDL column 1:1 (`item_order`) so a
/// hand-built store `FieldMap` can't address the wrong key.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: Option<ObjectId>,
    pub label: String,
    pub item_order: i32,
    pub target: LinkTarget,
    pub meta: serde_json::Value,

    pub menu: Option<ObjectId>,   // -> Menu
    pub parent: Option<ObjectId>, // -> MenuItem (nesting)
}

/// The assignment of a theme location to a menu — WP's `nav_menu_locations`,
/// modeled as a first-class row instead of a serialized theme-mod option.
/// `location` is `@unique` (a location holds one menu); many `MenuLocation` rows
/// may point at the SAME `menu` (one menu shown in many locations). Deleting a
/// menu cascades its assignment rows away, so the location falls back to nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuLocation {
    pub id: Option<ObjectId>,
    pub location: String,
    pub menu: Option<ObjectId>, // -> Menu
}
