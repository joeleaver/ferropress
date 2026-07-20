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

use crate::settings::DEFAULT_THEME;

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
    /// The object id of the `Page` to show as the front page, or `None` for the
    /// latest-posts galley. Resolved from `reading.show_on_front` + `reading.page_on_front`:
    /// only `Some` when the author picked "A static page" *and* named one. The serve
    /// layer still checks the page is published before rendering it (a dangling or
    /// unpublished id falls back to the galley).
    pub front_page_id: Option<u64>,
    /// The active public theme id (`appearance.theme`) — which theme's templates
    /// frame the content. Projected as a plain string; the serve layer's theme
    /// registry maps it to template sources, falling back to the default theme on an
    /// unknown id. Content-independent: a theme change re-frames every page live, so
    /// it evicts no cached envelope (they store only the theme-agnostic body).
    pub theme: String,
    /// The `/media/{uuid}` URL of the site logo, or `None`. Unlike every other field,
    /// this is NOT projected from the values map — `site.logo` stores a media object
    /// id, and resolving it to a URL needs a store lookup. [`from_values`](Self::from_values)
    /// leaves it `None`; the serve layer ([`ferropress_serve::load_site_settings`])
    /// fills it after resolving the id. The theme reads it directly for the masthead.
    pub logo_url: Option<String>,
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

        // A static front page only applies when "A static page" is selected AND a
        // page id is stored; otherwise the front page is the latest-posts galley.
        let front_page_id = if string("reading.show_on_front").as_deref() == Some("page") {
            values.get("reading.page_on_front").and_then(Value::as_u64)
        } else {
            None
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
            front_page_id,
            theme: string("appearance.theme").unwrap_or_else(|| DEFAULT_THEME.to_owned()),
            // Not projectable from the values map (a store lookup on the id); the
            // serve layer fills it. See the field doc.
            logo_url: None,
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
        assert_eq!(s.theme, "letterpress");
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
    fn front_page_id_resolves_only_when_static_page_selected() {
        // Default: latest posts, no static front page.
        assert_eq!(SiteSettings::defaults().front_page_id, None);

        // A page id is stored but "latest posts" is selected → still the galley.
        let mut latest = Map::new();
        latest.insert("reading.show_on_front".into(), json!("posts"));
        latest.insert("reading.page_on_front".into(), json!(42));
        assert_eq!(SiteSettings::from_values(&latest).front_page_id, None);

        // "A static page" selected with an id → that page.
        let mut static_page = Map::new();
        static_page.insert("reading.show_on_front".into(), json!("page"));
        static_page.insert("reading.page_on_front".into(), json!(42));
        assert_eq!(
            SiteSettings::from_values(&static_page).front_page_id,
            Some(42)
        );

        // "A static page" selected but no page chosen (null) → galley fallback.
        let mut unset = Map::new();
        unset.insert("reading.show_on_front".into(), json!("page"));
        unset.insert("reading.page_on_front".into(), Value::Null);
        assert_eq!(SiteSettings::from_values(&unset).front_page_id, None);
    }

    #[test]
    fn logo_url_is_not_projected_from_values() {
        // `site.logo` stores an id; resolving it to a URL is the serve layer's job,
        // so from_values always leaves logo_url None (never reads the id as a URL).
        let mut m = Map::new();
        m.insert("site.logo".into(), json!(7));
        assert_eq!(SiteSettings::from_values(&m).logo_url, None);
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
