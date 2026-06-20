//! `Menu` + `MenuItem` — dedicated typed nav menus, replacing WP's
//! post-row + nav_menu-taxonomy + 6-postmeta-keys overload. A `Menu` is named
//! and assigned to a theme `location`; `MenuItem`s form an ordered tree with a
//! typed `LinkTarget` union (internal ref / external URL / taxonomy term).

use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub id: Option<ObjectId>,
    pub slug: String,
    pub name: String,
    /// Theme location key (e.g. `"primary"`, `"footer"`).
    pub location: String,
    pub meta: serde_json::Value,
}

/// Where a menu item points. A typed union instead of WP's
/// `_menu_item_type`/`_menu_item_object`/`_menu_item_object_id` meta triple.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkTarget {
    Post { id: u64 },
    Page { id: u64 },
    Term { id: u64 },
    Custom { url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: Option<ObjectId>,
    pub label: String,
    pub order: i32,
    pub target: LinkTarget,
    pub meta: serde_json::Value,

    pub menu: Option<ObjectId>,   // -> Menu
    pub parent: Option<ObjectId>, // -> MenuItem (nesting)
}
