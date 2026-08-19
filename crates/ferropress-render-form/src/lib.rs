//! # ferropress-render-form
//!
//! The declarative **form-schema DATA + validation** layer — the editing-side
//! counterpart to `ferropress-core::block` (the block-tree data). A [`FormSchema`]
//! describes an editable surface (site settings today; plugin config later) as
//! serde data; the SERVER produces one, ships it over the wire, and validates a
//! submission against it, while the wasm admin renders the very same type.
//!
//! ## Where the single dispatch lives
//!
//! The one `ControlKind -> edit-UI` dispatch is NOT here. It emits rinch nodes, and
//! the dep-graph lint bans rinch from every workspace member, so it lives in the
//! excluded `ferropress-form-view` crate (marker `FERROPRESS-FORM-DISPATCH`). This
//! crate stays rinch-free so the whole host workspace — the server included — can
//! depend on the schema types and the settings schema. That split (data here,
//! rinch dispatch out-of-workspace) mirrors `ferropress-core::block` (data) +
//! `ferropress-render` (the one block->HTML dispatch); the difference is only that
//! form UI is interactive, so its renderer needs rinch and must be excluded.

mod catalog;
mod refs;
mod schema;
mod settings;
mod site_settings;

pub use catalog::{NoPlugins, PluginCatalog, PluginDescriptor};
pub use refs::{EntityOption, MediaRef, SettingRefs};
pub use schema::{
    Choice, Condition, ControlKind, Field, FieldError, FormSchema, FormSection, TextFormat,
};
pub use settings::{DEFAULT_THEME, THEME_LETTERPRESS, schema_for_settings};
pub use site_settings::SiteSettings;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn settings_schema_round_trips_through_json() {
        let schema = schema_for_settings(&[]);
        let wire = serde_json::to_string(&schema).expect("serialize");
        let back: FormSchema = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(schema, back, "FormSchema must survive a JSON round-trip");
    }

    #[test]
    fn control_kind_is_internally_tagged() {
        // The wire shape both ends rely on: `{"type":"number", ...}`.
        let w = ControlKind::Number {
            min: Some(1.0),
            max: Some(100.0),
            step: Some(1.0),
            unit: Some("posts".to_owned()),
        };
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(v["type"], "number");
        assert_eq!(v["unit"], "posts");

        let t = serde_json::to_value(ControlKind::Text {
            format: TextFormat::Url,
        })
        .unwrap();
        assert_eq!(t["type"], "text");
        assert_eq!(t["format"], "url");

        let toggle = serde_json::to_value(ControlKind::TextArea).unwrap();
        assert_eq!(toggle["type"], "text_area");
    }

    #[test]
    fn defaults_cover_every_key() {
        let schema = schema_for_settings(&[]);
        let defaults = schema.defaults();
        for f in schema.fields() {
            assert!(
                defaults.contains_key(&f.key),
                "default missing for {}",
                f.key
            );
        }
        assert_eq!(defaults["reading.posts_per_page"], json!(10));
        assert_eq!(defaults["reading.search_engine_visible"], json!(true));
        assert_eq!(defaults["site.timezone"], json!("UTC"));
    }

    #[test]
    fn coerce_accepts_valid_values() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.title".into(), json!("Ferropress"));
        raw.insert("site.admin_email".into(), json!("jane@example.com"));
        raw.insert("site.url".into(), json!("https://example.com"));
        raw.insert("reading.posts_per_page".into(), json!(25));
        raw.insert("reading.search_engine_visible".into(), json!(false));
        raw.insert("site.timezone".into(), json!("Europe/London"));
        raw.insert("site.date_format".into(), json!("Y-m-d"));

        let clean = schema.coerce_values(&raw).expect("all valid");
        assert_eq!(clean["site.title"], json!("Ferropress"));
        assert_eq!(clean["reading.posts_per_page"], json!(25));
        assert_eq!(clean["reading.search_engine_visible"], json!(false));
        assert_eq!(clean["site.date_format"], json!("Y-m-d"));
    }

    #[test]
    fn coerce_rejects_unsafe_url() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.url".into(), json!("javascript:alert(1)"));
        let errs = schema.coerce_values(&raw).expect_err("must reject");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "site.url");
    }

    #[test]
    fn coerce_rejects_bad_email_and_type_mismatch() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.admin_email".into(), json!("not-an-email"));
        // a bool where a number is expected
        raw.insert("reading.posts_per_page".into(), json!(true));
        let errs = schema.coerce_values(&raw).expect_err("must reject");
        let keys: Vec<_> = errs.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"site.admin_email"));
        assert!(keys.contains(&"reading.posts_per_page"));
    }

    #[test]
    fn coerce_clamps_number_into_range_and_stores_integer() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("reading.posts_per_page".into(), json!(9999));
        let clean = schema.coerce_values(&raw).unwrap();
        // clamped to max 100, stored as an integer (not 100.0)
        assert_eq!(clean["reading.posts_per_page"], json!(100));

        let mut low = serde_json::Map::new();
        low.insert("reading.posts_per_page".into(), json!(0));
        let clean = schema.coerce_values(&low).unwrap();
        assert_eq!(clean["reading.posts_per_page"], json!(1));
    }

    #[test]
    fn coerce_rejects_out_of_vocabulary_choice() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.timezone".into(), json!("Mars/Olympus_Mons"));
        let errs = schema.coerce_values(&raw).expect_err("must reject");
        assert_eq!(errs[0].key, "site.timezone");

        let mut df = serde_json::Map::new();
        df.insert("site.date_format".into(), json!("bogus"));
        assert!(schema.coerce_values(&df).is_err());
    }

    #[test]
    fn coerce_ignores_unknown_keys() {
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.title".into(), json!("ok"));
        raw.insert("evil.rce".into(), json!("rm -rf /"));
        let clean = schema.coerce_values(&raw).unwrap();
        assert!(clean.contains_key("site.title"));
        assert!(
            !clean.contains_key("evil.rce"),
            "unknown keys must never be persisted (schema is the whitelist)"
        );
    }

    #[test]
    fn coerce_accepts_empty_url_and_email() {
        // empty is "unset", not invalid.
        let schema = schema_for_settings(&[]);
        let mut raw = serde_json::Map::new();
        raw.insert("site.url".into(), json!(""));
        raw.insert("site.admin_email".into(), json!(""));
        let clean = schema.coerce_values(&raw).expect("empty is allowed");
        assert_eq!(clean["site.url"], Value::String(String::new()));
    }

    #[test]
    fn visible_when_is_carried_on_the_custom_format_field() {
        let schema = schema_for_settings(&[]);
        let f = schema
            .field("site.date_format_custom")
            .expect("field exists");
        let cond = f.visible_when.as_ref().expect("has a condition");
        assert_eq!(cond.key, "site.date_format");
        assert_eq!(cond.equals, json!("custom"));
    }
}
