//! `Setting` — site/plugin configuration as typed key/value singletons (WP
//! `wp_options`). Values are JSON Strings. `autoload` preserves WP's eager-load
//! notion for the small set of settings read on every render. First-class site
//! config (title, tagline, timezone, locale, permalink structure) is stored as
//! well-known keys here; plugins use a namespaced key prefix.

use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub id: Option<ObjectId>,
    /// Unique, indexed setting key (e.g. `"site.title"`, `"plugin.foo.bar"`).
    pub key: String,
    /// JSON-encoded value.
    pub value: serde_json::Value,
    /// Whether to preload this setting at startup.
    pub autoload: bool,
}
