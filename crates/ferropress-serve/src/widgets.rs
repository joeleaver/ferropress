//! `widget_specs()` — the closed registry of v1 sidebar/rail widget kinds: each
//! kind's stable snake_case id, admin-facing label/description, and per-kind
//! config [`FormSchema`]. This is the SINGLE source both the admin composite
//! GET's `kinds` list and the write-path's `kind ∈ registry` validator read
//! (later slices) — there is no separate wasm-embedded copy (MF14): the admin
//! renders whatever schema this module ships, never a hand-maintained mirror.
//!
//! Compose functions (`config -> WidgetCtx`) are **not** here. They land in
//! Increment 2, once the serving-side handles a real compose reads from exist
//! (`MenuSet::compose_menu`, `ContentIndex`, a `MediaIndexHandle`, a
//! `CommentsHandle`) — see [`WidgetSpec`]'s doc. This module ships only the
//! closed roster + its config vocabulary.
//!
//! ## The v1 roster (12 kinds; owner-ruled 2026-08-19)
//!
//! `text`, `custom_html`, `newsletter`, `recent_posts`, `recent_comments`,
//! `categories`, `tag_cloud`, `nav_menu`, `pages`, `search`, `meta`, `image`.
//!
//! Archives, Calendar, RSS, Gallery, Audio, and Video are DELIBERATELY ABSENT
//! — a kind is registered here only once it is honestly composable (their
//! compose sources or link targets don't exist yet: no date-archive route
//! family, no in-core outbound-fetch containment, no repeater form control).
//! Adding a kind to this function without its Increment-2 (or later-track)
//! compose function landing in the same change would violate that rule.
//!
//! ## `title` is never a schema field (MF24)
//!
//! Every widget's display title is a plain entity column on `Widget` itself
//! (`Widget.title`, rendered by the admin host as one input per row), never a
//! key inside a kind's `config`. A kind schema that declared a `title` field
//! would fork the value across two sources; [`widget_specs_never_declare_a_title_field`]
//! is a permanent regression guard for this rule.

use ferropress_render_form::{ControlKind, Field, FormSchema, FormSection, TextFormat};

/// One entry in the widget kind registry: its identity, admin-facing copy, and
/// config schema. Deliberately carries NO `compose` member yet (Increment 2
/// adds it once every kind has a real `config -> WidgetCtx` function to
/// attach — see the module doc).
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetSpec {
    /// Stable snake_case id, stored verbatim in `Widget.kind`. Never renamed —
    /// a rename would strand every existing row as an "unknown kind" the next
    /// time the admin lists it or the theme composes it (MF9's totality rule).
    pub kind: &'static str,
    /// Short display name for the admin's add-widget picker and widget cards.
    pub label: &'static str,
    /// Longer admin-facing copy shown in the add-widget picker. For
    /// `newsletter` this is one of the T8 ruling's two mandatory places the
    /// submission-backend deferral is communicated (the other is the schema's
    /// own section `help`, below).
    pub description: &'static str,
    pub schema: FormSchema,
}

/// The closed v1 widget registry, in the admin add-picker's display order.
/// Deterministic and cheap (twelve small static schemas) — call per request
/// rather than caching, mirroring `schema_for_settings`.
pub fn widget_specs() -> Vec<WidgetSpec> {
    vec![
        text_spec(),
        custom_html_spec(),
        newsletter_spec(),
        recent_posts_spec(),
        recent_comments_spec(),
        categories_spec(),
        tag_cloud_spec(),
        nav_menu_spec(),
        pages_spec(),
        search_spec(),
        meta_spec(),
        image_spec(),
    ]
}

/// A `Text` field with the plain (unformatted) `TextFormat`, the common case
/// across the roster (short labels, headlines, button copy — none of which
/// are URLs or emails).
fn plain_text_field(key: &str, label: &str, help: &str, default: &str) -> Field {
    Field {
        key: key.to_owned(),
        label: label.to_owned(),
        help: Some(help.to_owned()),
        default: serde_json::Value::String(default.to_owned()),
        widget: ControlKind::Text {
            format: TextFormat::Plain,
        },
        visible_when: None,
    }
}

/// Wrap a kind's fields in the single [`FormSection`] every non-empty schema
/// here uses (one section per kind — the admin card IS the section header, so
/// a redundant in-form title would just be visual noise).
fn one_section(kind: &str, help: Option<&str>, fields: Vec<Field>) -> FormSchema {
    FormSchema {
        sections: vec![FormSection {
            id: kind.to_owned(),
            title: String::new(),
            help: help.map(str::to_owned),
            fields,
        }],
    }
}

fn text_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "text",
        label: "Text",
        description: "Arbitrary text or a short blurb.",
        schema: one_section(
            "text",
            None,
            vec![Field {
                key: "content".to_owned(),
                label: "Content".to_owned(),
                help: Some(
                    "Shown as plain text with paragraphs preserved — it is never rendered as \
                     HTML. For real markup, use the Custom HTML widget instead."
                        .to_owned(),
                ),
                default: serde_json::Value::String(String::new()),
                widget: ControlKind::TextArea,
                visible_when: None,
            }],
        ),
    }
}

fn custom_html_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "custom_html",
        label: "Custom HTML",
        description: "Arbitrary HTML, cleaned automatically when you save.",
        schema: one_section(
            "custom_html",
            None,
            vec![Field {
                key: "html".to_owned(),
                label: "HTML".to_owned(),
                help: Some(
                    "Cleaned when you save: disallowed tags, scripts, and event handlers are \
                     removed; embedded frames are sandboxed and must be served over https."
                        .to_owned(),
                ),
                default: serde_json::Value::String(String::new()),
                widget: ControlKind::TextArea,
                visible_when: None,
            }],
        ),
    }
}

fn newsletter_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "newsletter",
        label: "Newsletter Sign-up",
        // The add-picker's copy — the FIRST of T8's two mandatory deferral
        // locations (worded generically: no provider is named).
        description: "A sign-up form for a newsletter or mailing list. Sign-ups require a \
                       newsletter provider plugin to be installed and active — none ships with \
                       core yet, so the form renders visibly but disabled until one is.",
        schema: one_section(
            "newsletter",
            // The SECOND of T8's two mandatory deferral locations.
            Some(
                "Sign-ups are not yet active on this site: no newsletter provider plugin is \
                 installed. The form will render disabled to visitors until one is activated — \
                 no submissions are silently lost or fake-accepted.",
            ),
            vec![
                plain_text_field(
                    "headline",
                    "Headline",
                    "Shown above the sign-up form.",
                    "Stay in the loop",
                ),
                Field {
                    key: "blurb".to_owned(),
                    label: "Blurb".to_owned(),
                    help: Some("A sentence or two of supporting copy.".to_owned()),
                    default: serde_json::Value::String(
                        "Get the latest posts delivered straight to your inbox.".to_owned(),
                    ),
                    widget: ControlKind::TextArea,
                    visible_when: None,
                },
                plain_text_field(
                    "privacy_line",
                    "Privacy line",
                    "Small print shown beneath the form.",
                    "We respect your privacy. Unsubscribe at any time.",
                ),
                plain_text_field(
                    "button_label",
                    "Button label",
                    "The submit button's text.",
                    "Subscribe",
                ),
            ],
        ),
    }
}

fn recent_posts_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "recent_posts",
        label: "Recent Posts",
        description: "A list of the site's most recently published posts.",
        schema: one_section(
            "recent_posts",
            None,
            vec![
                Field {
                    key: "count".to_owned(),
                    label: "Number of posts".to_owned(),
                    help: None,
                    default: serde_json::Value::from(5),
                    widget: ControlKind::Number {
                        min: Some(1.0),
                        max: Some(20.0),
                        step: Some(1.0),
                        unit: Some("posts".to_owned()),
                    },
                    visible_when: None,
                },
                Field {
                    key: "show_date".to_owned(),
                    label: "Show date".to_owned(),
                    help: None,
                    default: serde_json::Value::Bool(false),
                    widget: ControlKind::Toggle {
                        text: Some("Show the publish date".to_owned()),
                    },
                    visible_when: None,
                },
            ],
        ),
    }
}

fn recent_comments_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "recent_comments",
        label: "Recent Comments",
        description: "A list of the site's most recently approved comments.",
        schema: one_section(
            "recent_comments",
            None,
            vec![Field {
                key: "count".to_owned(),
                label: "Number of comments".to_owned(),
                help: None,
                default: serde_json::Value::from(5),
                widget: ControlKind::Number {
                    min: Some(1.0),
                    max: Some(20.0),
                    step: Some(1.0),
                    unit: Some("comments".to_owned()),
                },
                visible_when: None,
            }],
        ),
    }
}

fn categories_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "categories",
        label: "Categories",
        description: "A list of the site's categories.",
        schema: one_section(
            "categories",
            None,
            vec![Field {
                key: "show_counts".to_owned(),
                label: "Show counts".to_owned(),
                help: None,
                default: serde_json::Value::Bool(false),
                widget: ControlKind::Toggle {
                    text: Some("Show a published-post count beside each category".to_owned()),
                },
                visible_when: None,
            }],
        ),
    }
}

fn tag_cloud_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "tag_cloud",
        label: "Tag Cloud",
        description: "A cloud of the site's most-used tags, sized by how often each is used.",
        schema: one_section(
            "tag_cloud",
            None,
            vec![Field {
                key: "max_tags".to_owned(),
                label: "Maximum tags shown".to_owned(),
                help: None,
                default: serde_json::Value::from(45),
                widget: ControlKind::Number {
                    min: Some(5.0),
                    max: Some(100.0),
                    step: Some(1.0),
                    unit: Some("tags".to_owned()),
                },
                visible_when: None,
            }],
        ),
    }
}

fn nav_menu_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "nav_menu",
        label: "Navigation Menu",
        description: "Any nav menu you've created, shown as a simple linked list.",
        schema: one_section(
            "nav_menu",
            None,
            vec![Field {
                key: "menu".to_owned(),
                label: "Menu".to_owned(),
                help: Some("The menu to display — managed under Menus.".to_owned()),
                default: serde_json::Value::Null,
                widget: ControlKind::EntityRef {
                    entity: "menu".to_owned(),
                },
                visible_when: None,
            }],
        ),
    }
}

/// No config beyond the host-rendered title (MF24) — WP's Pages widget has no
/// meaningful option that maps onto the existing control vocabulary, and
/// fewer fields beats inventing one (per the increment spec's own guidance).
fn pages_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "pages",
        label: "Pages",
        description: "A list of the site's published pages.",
        schema: FormSchema { sections: vec![] },
    }
}

fn search_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "search",
        label: "Search",
        description: "A search box.",
        schema: FormSchema { sections: vec![] },
    }
}

fn meta_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "meta",
        label: "Meta",
        description: "Login/logout, admin, and feed links — the classic \"Meta\" widget.",
        schema: FormSchema { sections: vec![] },
    }
}

fn image_spec() -> WidgetSpec {
    WidgetSpec {
        kind: "image",
        label: "Image",
        description: "A single image, optionally captioned and linked.",
        schema: one_section(
            "image",
            None,
            vec![
                Field {
                    key: "media".to_owned(),
                    label: "Image".to_owned(),
                    help: Some("The image to display.".to_owned()),
                    default: serde_json::Value::Null,
                    widget: ControlKind::MediaPicker,
                    visible_when: None,
                },
                plain_text_field(
                    "caption",
                    "Caption",
                    "Optional. Shown beneath the image.",
                    "",
                ),
                Field {
                    key: "link_url".to_owned(),
                    label: "Link URL".to_owned(),
                    help: Some("Optional. If set, the image links to this address.".to_owned()),
                    default: serde_json::Value::String(String::new()),
                    widget: ControlKind::Text {
                        format: TextFormat::Url,
                    },
                    visible_when: None,
                },
            ],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_twelve_kinds() {
        assert_eq!(widget_specs().len(), 12);
    }

    #[test]
    fn kind_ids_are_unique_and_snake_case() {
        let specs = widget_specs();
        let mut seen = std::collections::HashSet::new();
        for spec in &specs {
            assert!(
                is_snake_case(spec.kind),
                "{:?} is not snake_case",
                spec.kind
            );
            assert!(
                seen.insert(spec.kind),
                "duplicate widget kind id {:?}",
                spec.kind
            );
        }

        fn is_snake_case(s: &str) -> bool {
            !s.is_empty()
                && !s.starts_with('_')
                && !s.ends_with('_')
                && !s.contains("__")
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }
    }

    /// The exact v1 roster, order-independent — a guard against a silent
    /// rename or drop as much as against a silent addition.
    #[test]
    fn roster_matches_the_owner_ruled_v1_set() {
        let ids: std::collections::HashSet<&str> = widget_specs().iter().map(|s| s.kind).collect();
        let expected: std::collections::HashSet<&str> = [
            "text",
            "custom_html",
            "newsletter",
            "recent_posts",
            "recent_comments",
            "categories",
            "tag_cloud",
            "nav_menu",
            "pages",
            "search",
            "meta",
            "image",
        ]
        .into_iter()
        .collect();
        assert_eq!(ids, expected);
    }

    /// MF24 permanent guard: `title` is a `Widget` entity column, never a
    /// config key. If a future kind schema declares one, this must fail.
    #[test]
    fn widget_specs_never_declare_a_title_field() {
        for spec in widget_specs() {
            for field in spec.schema.fields() {
                assert_ne!(
                    field.key, "title",
                    "kind {:?} declares a config field named `title` — title is a \
                     host-rendered entity column (MF24), never a schema field",
                    spec.kind
                );
            }
        }
    }

    /// T8 ruling: `action` is compose-time-only context (`Option<String>`,
    /// filled by a future provider plugin), never a config key an author can
    /// set — its ABSENCE from the schema is what makes the deferral honest
    /// (an author can't fake-configure a submission endpoint that doesn't
    /// exist).
    #[test]
    fn newsletter_has_no_action_field() {
        let newsletter = widget_specs()
            .into_iter()
            .find(|s| s.kind == "newsletter")
            .expect("newsletter kind exists");
        assert!(newsletter.schema.field("action").is_none());
    }

    /// The newsletter deferral copy must land in BOTH of T8's mandatory
    /// places: the add-picker description AND the schema's section help.
    #[test]
    fn newsletter_deferral_copy_is_in_both_mandatory_places() {
        let newsletter = widget_specs()
            .into_iter()
            .find(|s| s.kind == "newsletter")
            .expect("newsletter kind exists");
        assert!(
            newsletter.description.contains("provider"),
            "add-picker description must name the deferral"
        );
        let section_help = newsletter
            .schema
            .sections
            .first()
            .and_then(|s| s.help.as_deref())
            .unwrap_or_default();
        assert!(
            section_help.contains("not yet active"),
            "schema section help must name the deferral"
        );
    }

    /// Every field's widget is one of the KNOWN `ControlKind` variants. This
    /// match is deliberately EXHAUSTIVE (no `_` arm): a future `ControlKind`
    /// variant fails this test to COMPILE, forcing every widget schema here
    /// to be reconsidered against the new control rather than silently
    /// missing it.
    #[test]
    fn every_field_uses_a_known_control_kind() {
        for spec in widget_specs() {
            for field in spec.schema.fields() {
                match &field.widget {
                    ControlKind::Text { .. }
                    | ControlKind::TextArea
                    | ControlKind::Toggle { .. }
                    | ControlKind::Number { .. }
                    | ControlKind::Select { .. }
                    | ControlKind::Radio { .. }
                    | ControlKind::MediaPicker
                    | ControlKind::EntityRef { .. }
                    | ControlKind::BlockEditor => {}
                }
            }
        }
    }

    /// Every schema round-trips through JSON unchanged — the same wire-shape
    /// guard `ferropress-render-form`'s own settings-schema test applies,
    /// proven per-kind here since each is a distinct static value.
    #[test]
    fn every_schema_round_trips_through_json() {
        for spec in widget_specs() {
            let wire = serde_json::to_string(&spec.schema).expect("serialize");
            let back: FormSchema = serde_json::from_str(&wire).expect("deserialize");
            assert_eq!(spec.schema, back, "{} schema must round-trip", spec.kind);
        }
    }

    /// `nav_menu`'s `EntityRef` names the "menu" entity — the resolve side
    /// (wiring it into `resolve_entity_options`) is a later slice's job, but
    /// the schema declaration is this slice's, and a typo here would silently
    /// break that later wiring.
    #[test]
    fn nav_menu_references_the_menu_entity() {
        let nav_menu = widget_specs()
            .into_iter()
            .find(|s| s.kind == "nav_menu")
            .expect("nav_menu kind exists");
        let field = nav_menu.schema.field("menu").expect("menu field exists");
        assert_eq!(
            field.widget,
            ControlKind::EntityRef {
                entity: "menu".to_owned(),
            }
        );
    }

    /// `defaults()` covers every declared key — the same drift guard
    /// `ferropress-render-form`'s settings-schema test applies, per kind.
    #[test]
    fn defaults_cover_every_declared_key() {
        for spec in widget_specs() {
            let defaults = spec.schema.defaults();
            for field in spec.schema.fields() {
                assert!(
                    defaults.contains_key(&field.key),
                    "{}: default missing for {}",
                    spec.kind,
                    field.key
                );
            }
        }
    }
}
