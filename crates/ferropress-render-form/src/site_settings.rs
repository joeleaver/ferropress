//! The typed, public-facing projection of the site settings the theme consumes.
//!
//! [`schema_for_settings`](crate::schema_for_settings) declares the well-known
//! `Setting` keys; this is their typed *view*. The serve/theme layer holds a
//! [`SiteSettings`] and reads `title`, `posts_per_page`, `date_format`, etc.
//! directly, so it never string-indexes the raw settings map — the key strings
//! live in exactly one crate (this one), beside the schema that declares them.
//!
//! Built with [`SiteSettings::from_values`] from the same defaults-overlaid map
//! the admin API produces (stored `Setting`s over the schema defaults). Missing
//! or wrong-typed values fall back to the schema default, so a partial or
//! corrupt store can never crash a render — it just reads as the default.

use serde::Serialize;
use serde_json::{Map, Value};

/// The default date pattern when none is stored, or when "Custom" is selected
/// with an empty custom string. Mirrors the `site.date_format` schema default.
const DEFAULT_DATE_FORMAT: &str = "F j, Y";

/// A neutral site name shown when `site.title` is unset (a fresh install), so
/// the masthead is never blank.
const DEFAULT_TITLE: &str = "Ferropress";

/// Typed site settings the public theme consumes.
///
/// `date_format` is the **resolved** PHP `date()` pattern: when the author picks
/// "Custom", it is the custom string (or the default if that is blank); otherwise
/// it is the chosen preset. Callers format dates with it directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SiteSettings {
    pub title: String,
    pub tagline: String,
    pub url: String,
    pub admin_email: String,
    /// `reading.search_engine_visible`: when false the theme emits a
    /// `noindex, nofollow` robots meta (an honour-system request).
    pub search_engine_visible: bool,
    pub posts_per_page: u32,
    pub feed_items: u32,
    pub timezone: String,
    pub date_format: String,
}

impl SiteSettings {
    /// Project a settings values map (schema defaults overlaid with stored
    /// `Setting`s) into typed fields. Absent or wrong-typed keys fall back to the
    /// schema default.
    pub fn from_values(values: &Map<String, Value>) -> Self {
        let string = |k: &str| values.get(k).and_then(Value::as_str).map(str::to_owned);
        let boolean =
            |k: &str, default: bool| values.get(k).and_then(Value::as_bool).unwrap_or(default);
        let count = |k: &str, default: u32| {
            values
                .get(k)
                .and_then(Value::as_u64)
                .map(|n| n.min(u32::MAX as u64) as u32)
                .unwrap_or(default)
        };

        // Resolve the date format: "custom" expands to the custom string (or the
        // default when it is blank); any preset is used verbatim.
        let choice = string("site.date_format").unwrap_or_else(|| DEFAULT_DATE_FORMAT.to_owned());
        let date_format = if choice == "custom" {
            match string("site.date_format_custom") {
                Some(custom) if !custom.trim().is_empty() => custom,
                _ => DEFAULT_DATE_FORMAT.to_owned(),
            }
        } else {
            choice
        };

        SiteSettings {
            title: string("site.title").unwrap_or_default(),
            tagline: string("site.tagline").unwrap_or_default(),
            url: string("site.url").unwrap_or_default(),
            admin_email: string("site.admin_email").unwrap_or_default(),
            search_engine_visible: boolean("reading.search_engine_visible", true),
            posts_per_page: count("reading.posts_per_page", 10),
            feed_items: count("reading.feed_items", 10),
            timezone: string("site.timezone").unwrap_or_else(|| "UTC".to_owned()),
            date_format,
        }
    }

    /// The settings a fresh site has before any `Setting` is written — every
    /// field at its schema default.
    pub fn defaults() -> Self {
        Self::from_values(&Map::new())
    }

    /// The site title, or a neutral fallback when it is unset, so the masthead is
    /// never empty.
    pub fn title_or_default(&self) -> &str {
        if self.title.trim().is_empty() {
            DEFAULT_TITLE
        } else {
            &self.title
        }
    }
}

impl Default for SiteSettings {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_for_settings;
    use serde_json::json;

    #[test]
    fn empty_map_yields_schema_defaults() {
        let s = SiteSettings::defaults();
        assert_eq!(s.posts_per_page, 10);
        assert_eq!(s.feed_items, 10);
        assert!(s.search_engine_visible);
        assert_eq!(s.timezone, "UTC");
        assert_eq!(s.date_format, "F j, Y");
        assert_eq!(s.title, "");
        assert_eq!(s.title_or_default(), "Ferropress");
    }

    #[test]
    fn typed_view_matches_schema_defaults_no_drift() {
        // The inline fallbacks here must equal the schema's declared defaults;
        // this guards against the two drifting apart.
        let from_schema = SiteSettings::from_values(&schema_for_settings().defaults());
        assert_eq!(from_schema, SiteSettings::defaults());
    }

    #[test]
    fn reads_stored_values() {
        let mut m = Map::new();
        m.insert("site.title".into(), json!("The Composing Room"));
        m.insert("site.tagline".into(), json!("Type & ink"));
        m.insert("reading.posts_per_page".into(), json!(5));
        m.insert("reading.search_engine_visible".into(), json!(false));
        m.insert("site.timezone".into(), json!("Europe/London"));

        let s = SiteSettings::from_values(&m);
        assert_eq!(s.title, "The Composing Room");
        assert_eq!(s.title_or_default(), "The Composing Room");
        assert_eq!(s.tagline, "Type & ink");
        assert_eq!(s.posts_per_page, 5);
        assert!(!s.search_engine_visible);
        assert_eq!(s.timezone, "Europe/London");
    }

    #[test]
    fn custom_date_format_resolves_and_falls_back_when_blank() {
        let mut custom = Map::new();
        custom.insert("site.date_format".into(), json!("custom"));
        custom.insert("site.date_format_custom".into(), json!("l, F jS Y"));
        assert_eq!(SiteSettings::from_values(&custom).date_format, "l, F jS Y");

        // "custom" selected but the custom string is blank → default pattern.
        let mut blank = Map::new();
        blank.insert("site.date_format".into(), json!("custom"));
        blank.insert("site.date_format_custom".into(), json!("   "));
        assert_eq!(SiteSettings::from_values(&blank).date_format, "F j, Y");
    }

    #[test]
    fn wrong_typed_values_fall_back_to_defaults() {
        let mut m = Map::new();
        m.insert("reading.posts_per_page".into(), json!("lots")); // string, not number
        m.insert("reading.search_engine_visible".into(), json!(1)); // number, not bool
        let s = SiteSettings::from_values(&m);
        assert_eq!(s.posts_per_page, 10);
        assert!(s.search_engine_visible);
    }
}
