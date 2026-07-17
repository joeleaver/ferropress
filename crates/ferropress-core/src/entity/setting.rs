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

/// Whether `id` is a well-formed plugin id: a non-empty run of ASCII
/// alphanumerics, `-`, or `_`. The [`plugin_setting_key`] namespacing DEPENDS on a
/// plugin id containing no `.`: because the key is the string join
/// `plugin.{id}.{bare}`, two ids where one is a dotted prefix of the other
/// (`acme` with bare `pro.color` vs `acme.pro` with bare `color`) would otherwise
/// build byte-identical `Setting` keys and a plugin could read/write across
/// namespaces. Restricting ids to a dot-free charset makes the boundary
/// unambiguous — the segment between `plugin.` and the FIRST following `.` is
/// always the id — so cross-plugin/core isolation is genuinely structural. The
/// plugin host rejects an id that fails this at load.
pub fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The `Setting`-key namespace prefix for a plugin's configuration:
/// `"plugin.{plugin_id}."`. **The host owns this prefix** — a plugin never
/// supplies it — so a plugin's settings can never collide with core keys
/// (`site.*` / `reading.*`) or another plugin's, exactly like the `content:write`
/// `set_meta` namespacing (the host passes the namespace, the guest can't forge
/// it). This isolation is structural PROVIDED the plugin id is well-formed
/// ([`is_valid_plugin_id`] — no `.`, enforced by the host at load), so the
/// id↔bare-key boundary is unambiguous. A plugin's config schema uses BARE keys
/// (e.g. `"default_variant"`); this prefix + the bare key is the persisted key.
pub fn plugin_setting_prefix(plugin_id: &str) -> String {
    format!("plugin.{plugin_id}.")
}

/// The fully-qualified `Setting` key for a plugin's bare config key — the prefix
/// ([`plugin_setting_prefix`]) joined with `bare_key`. Single source of truth
/// shared by the admin route (which persists) and the store adapter (which reads
/// the value back for the `plugin_settings` capability), so the two never disagree
/// about where a plugin setting lives.
pub fn plugin_setting_key(plugin_id: &str, bare_key: &str) -> String {
    format!("plugin.{plugin_id}.{bare_key}")
}

/// The inverse of [`plugin_setting_key`]: extract the plugin id from a fully-qualified
/// plugin setting key, or `None` if `key` is not a well-formed one.
///
/// Because a plugin id is dot-free ([`is_valid_plugin_id`]), the id is unambiguously
/// the segment between the `plugin.` prefix and the FIRST following `.`; everything
/// after it is the (possibly dotted) bare key. A key with no bare part
/// (`"plugin.callout"`), an empty/invalid id (`"plugin..x"`), or a non-plugin key
/// (`"site.title"`) returns `None`.
///
/// This is a deliberately fail-CLOSED parse: the caller (a change-feed consumer that
/// regenerates the pages using a plugin whose config changed) cannot act on a key it
/// can't map to a plugin id, so `None` must mean "not a plugin setting — do nothing",
/// never "assume all plugins". Contrast the site-front invalidation, which fails OPEN.
pub fn plugin_id_from_setting_key(key: &str) -> Option<&str> {
    let (id, bare) = key.strip_prefix("plugin.")?.split_once('.')?;
    (is_valid_plugin_id(id) && !bare.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::{is_valid_plugin_id, plugin_id_from_setting_key, plugin_setting_key};

    #[test]
    fn valid_plugin_ids_exclude_dots_and_empties() {
        for ok in ["callout", "wiki", "backlink-index", "my_plugin", "a1"] {
            assert!(is_valid_plugin_id(ok), "{ok} should be valid");
        }
        // A `.` is the killer case: it would make `plugin.{id}.{bare}` ambiguous
        // between a dotted-prefix id pair (`acme` + `pro.color` vs `acme.pro` + `color`).
        for bad in ["", "acme.pro", "a b", "a/b", "café", "a.", ".a"] {
            assert!(!is_valid_plugin_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn plugin_id_parses_from_a_well_formed_key() {
        assert_eq!(
            plugin_id_from_setting_key("plugin.callout.default_variant"),
            Some("callout")
        );
        // A bare key MAY contain dots — only the id is dot-free, so split on the FIRST.
        assert_eq!(
            plugin_id_from_setting_key("plugin.foo.bar.baz"),
            Some("foo")
        );
        // Round-trips with the constructor.
        assert_eq!(
            plugin_id_from_setting_key(&plugin_setting_key("backlink-index", "depth")),
            Some("backlink-index"),
        );
    }

    #[test]
    fn plugin_id_rejects_non_plugin_and_malformed_keys() {
        for none in [
            "site.title",             // a core key, not a plugin key
            "reading.posts_per_page", // "
            "plugin.callout",         // no bare key after the id
            "plugin.",                // no id, no bare
            "plugin..x",              // empty id
            "plugin.a b.x",           // invalid id (space)
            "callout.default",        // missing the `plugin.` prefix
            "",
        ] {
            assert_eq!(
                plugin_id_from_setting_key(none),
                None,
                "{none:?} must be None"
            );
        }
    }
}
