//! The first-party **site-settings** form schema — the single source of truth for
//! which `Setting` keys the admin's Settings page exposes, their widgets, and
//! their defaults. WordPress's General + Reading + Formatting, retyped as a
//! declarative [`FormSchema`]. The HTTP layer calls [`schema_for_settings`] to
//! (a) ship the schema to the admin and (b) validate a submission against it; the
//! wasm admin renders the very same schema. Adding a setting is a data edit here,
//! not new UI code.

use crate::schema::{Choice, Condition, Field, FormSchema, FormSection, TextFormat, WidgetKind};
use serde_json::Value;

fn text(key: &str, label: &str, help: &str, format: TextFormat) -> Field {
    Field {
        key: key.to_owned(),
        label: label.to_owned(),
        help: Some(help.to_owned()),
        default: Value::String(String::new()),
        widget: WidgetKind::Text { format },
        visible_when: None,
    }
}

fn choice(value: &str, label: &str) -> Choice {
    Choice {
        value: value.to_owned(),
        label: label.to_owned(),
    }
}

/// A small, curated set of common IANA zones (not the full ~350-entry database —
/// enough to be useful, trivially extended). `UTC` is the safe default.
fn timezones() -> Vec<Choice> {
    [
        "UTC",
        "America/New_York",
        "America/Chicago",
        "America/Denver",
        "America/Los_Angeles",
        "America/Sao_Paulo",
        "Europe/London",
        "Europe/Paris",
        "Europe/Berlin",
        "Europe/Moscow",
        "Africa/Johannesburg",
        "Asia/Dubai",
        "Asia/Kolkata",
        "Asia/Shanghai",
        "Asia/Tokyo",
        "Australia/Sydney",
        "Pacific/Auckland",
    ]
    .iter()
    .map(|z| choice(z, z))
    .collect()
}

/// Build the site-settings [`FormSchema`]. Deterministic and cheap; call per
/// request rather than caching.
pub fn schema_for_settings() -> FormSchema {
    FormSchema {
        sections: vec![
            FormSection {
                id: "identity".to_owned(),
                title: "Site identity".to_owned(),
                help: None,
                fields: vec![
                    text(
                        "site.title",
                        "Site title",
                        "The name of your site — shown in the masthead and the browser tab.",
                        TextFormat::Plain,
                    ),
                    text(
                        "site.tagline",
                        "Tagline",
                        "In a few words, what this site is about.",
                        TextFormat::Plain,
                    ),
                    text(
                        "site.admin_email",
                        "Administration email",
                        "Used for administrative notices.",
                        TextFormat::Email,
                    ),
                    text(
                        "site.url",
                        "Site address",
                        "The public base URL of your site.",
                        TextFormat::Url,
                    ),
                ],
            },
            FormSection {
                id: "reading".to_owned(),
                title: "Reading".to_owned(),
                help: None,
                fields: vec![
                    Field {
                        key: "reading.posts_per_page".to_owned(),
                        label: "Posts per page".to_owned(),
                        help: Some(
                            "How many posts a listing page shows before paginating.".to_owned(),
                        ),
                        default: Value::from(10),
                        widget: WidgetKind::Number {
                            min: Some(1.0),
                            max: Some(100.0),
                            step: Some(1.0),
                            unit: Some("posts".to_owned()),
                        },
                        visible_when: None,
                    },
                    Field {
                        key: "reading.feed_items".to_owned(),
                        label: "Feed items".to_owned(),
                        help: Some("Most-recent items included in syndication feeds.".to_owned()),
                        default: Value::from(10),
                        widget: WidgetKind::Number {
                            min: Some(1.0),
                            max: Some(100.0),
                            step: Some(1.0),
                            unit: Some("items".to_owned()),
                        },
                        visible_when: None,
                    },
                    Field {
                        key: "reading.search_engine_visible".to_owned(),
                        label: "Search engines".to_owned(),
                        help: Some(
                            "When off, Ferropress asks search engines not to index the site (an \
                             honour-system request)."
                                .to_owned(),
                        ),
                        default: Value::Bool(true),
                        widget: WidgetKind::Toggle {
                            text: Some("Allow search engines to index this site".to_owned()),
                        },
                        visible_when: None,
                    },
                ],
            },
            FormSection {
                id: "formatting".to_owned(),
                title: "Formatting".to_owned(),
                help: None,
                fields: vec![
                    Field {
                        key: "site.timezone".to_owned(),
                        label: "Timezone".to_owned(),
                        help: Some(
                            "Choose a city or region that shares your local time.".to_owned(),
                        ),
                        default: Value::String("UTC".to_owned()),
                        widget: WidgetKind::Select {
                            options: timezones(),
                        },
                        visible_when: None,
                    },
                    Field {
                        key: "site.date_format".to_owned(),
                        label: "Date format".to_owned(),
                        help: None,
                        default: Value::String("F j, Y".to_owned()),
                        widget: WidgetKind::Radio {
                            options: vec![
                                choice("F j, Y", "January 8, 2026"),
                                choice("Y-m-d", "2026-01-08"),
                                choice("d/m/Y", "08/01/2026"),
                                choice("custom", "Custom"),
                            ],
                        },
                        visible_when: None,
                    },
                    Field {
                        key: "site.date_format_custom".to_owned(),
                        label: "Custom format".to_owned(),
                        help: Some(
                            "A PHP date()-style pattern used when \u{201c}Custom\u{201d} is \
                             selected — e.g. l, F jS Y."
                                .to_owned(),
                        ),
                        default: Value::String(String::new()),
                        widget: WidgetKind::Text {
                            format: TextFormat::Plain,
                        },
                        visible_when: Some(Condition {
                            key: "site.date_format".to_owned(),
                            equals: Value::String("custom".to_owned()),
                        }),
                    },
                ],
            },
        ],
    }
}
