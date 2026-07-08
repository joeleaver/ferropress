//! The declarative form-schema DATA model — the serde types a `FormSchema` is
//! made of, plus the untrusted-input validation/coercion that guards a write.
//!
//! This module is deliberately **render-agnostic**: it knows nothing about rinch
//! or HTML. The single `WidgetKind -> edit-UI` dispatch lives in the excluded
//! `ferropress-form-view` crate (it emits rinch nodes, which the dep-graph lint
//! bans from any workspace member). Keeping the schema here — rinch-free and
//! serde-serializable — is what lets the SERVER produce a schema, ship it over
//! the wire, and validate a submission against it, while the wasm admin renders
//! the very same type. Mirrors how `ferropress-core::block` holds the block DATA
//! and `ferropress-render` holds the one block->HTML dispatch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A whole declarative form: ordered, titled sections of fields. This is what a
/// first-party surface (site settings) or, later, a plugin ships; the admin
/// mounts one renderer over it regardless of shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FormSchema {
    pub sections: Vec<FormSection>,
}

/// One titled group of fields (a panel in the UI, e.g. "Site identity").
/// (`Default` is derived only to satisfy the rinch `#[component]` macro, which
/// generates a `Default` for a component's props; it is never constructed at runtime.)
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FormSection {
    /// Stable id (not shown; useful for anchors/keys).
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub fields: Vec<Field>,
}

/// One editable field. The shared attributes (key binding, label, help, default,
/// visibility) live here; the `widget` carries only the UI-specific config. This
/// split is why adding a widget kind never touches the per-field plumbing.
/// (`Default` is derived only to satisfy the rinch `#[component]` macro's props.)
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Field {
    /// The persisted `Setting` key this field reads/writes, e.g. `"site.title"`.
    pub key: String,
    /// Human label (the left column / control caption).
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Value used when no stored value exists yet (seeds both the form and, on
    /// first save, the persisted row).
    pub default: Value,
    pub widget: WidgetKind,
    /// Render this field only when another field's current value equals a target.
    /// A hidden field's value is still persisted (never silently dropped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<Condition>,
}

/// A visibility predicate: show the field iff the value at `key` equals `equals`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub key: String,
    pub equals: Value,
}

/// A `(value, label)` choice for `Select` / `Radio`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// How a `Text` field is validated + hinted. Purely a normalization/UX concern;
/// all four store a plain string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    #[default]
    Plain,
    Email,
    Url,
    Slug,
}

/// THE widget vocabulary. A pure DATA enum: the single `WidgetKind -> edit-UI`
/// dispatch lives in `ferropress-form-view` (marker `FERROPRESS-FORM-DISPATCH`),
/// NEVER inline at a call site — that is what keeps "one form renderer" true.
///
/// Internally tagged (`{"type":"number", ...}`) so the JSON is legible on the
/// wire and both ends (server producer, wasm renderer) share this exact type.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WidgetKind {
    /// Single-line text (titles, slugs, emails, URLs — see `TextFormat`).
    Text {
        #[serde(default)]
        format: TextFormat,
    },
    /// Multi-line text. (The arbitrary `Default` variant — chosen only so the type
    /// impls `Default` for the rinch `#[component]` macro; not a meaningful default.)
    #[default]
    TextArea,
    /// Boolean switch. `text` is the affirmative phrase shown beside the lever.
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Numeric input; optional bounds/step/unit. An integral `step` stores an int.
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// Closed choice from a dropdown.
    Select { options: Vec<Choice> },
    /// Closed choice from a radio group (visually distinct from `Select`).
    Radio { options: Vec<Choice> },
    /// Picks a `Media` object id (resolves to an unguessable media URL). Not used
    /// by the settings schema yet — forward-declared for later consumers.
    MediaPicker,
    /// Picks an object id of the named entity (e.g. a `Page`). Forward-declared.
    EntityRef { entity: String },
    /// The rich block-tree body editor. Forward-declared; the post editor mounts
    /// the rinch editor directly today.
    BlockEditor,
}

/// A per-field validation failure, surfaced by [`FormSchema::coerce_values`].
#[derive(Debug, Clone, PartialEq)]
pub struct FieldError {
    pub key: String,
    pub message: String,
}

impl FormSchema {
    /// Every field across all sections, in document order.
    pub fn fields(&self) -> impl Iterator<Item = &Field> {
        self.sections.iter().flat_map(|s| s.fields.iter())
    }

    /// Look up a field by its `Setting` key.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields().find(|f| f.key == key)
    }

    /// The default value for every field, keyed by `Setting` key. Used to seed the
    /// form (and persistence) when no stored value exists yet.
    pub fn defaults(&self) -> serde_json::Map<String, Value> {
        self.fields()
            .map(|f| (f.key.clone(), f.default.clone()))
            .collect()
    }

    /// Validate + normalize a raw values map from an **untrusted** submission
    /// against this schema. Unknown keys are IGNORED (the schema is the
    /// whitelist), so a client can never write a `Setting` the schema doesn't
    /// declare. Each known key is type-checked and normalized per its widget.
    /// Returns the clean, ready-to-persist map, or the list of per-field errors.
    pub fn coerce_values(
        &self,
        raw: &serde_json::Map<String, Value>,
    ) -> Result<serde_json::Map<String, Value>, Vec<FieldError>> {
        let mut out = serde_json::Map::new();
        let mut errs = Vec::new();
        for field in self.fields() {
            let Some(v) = raw.get(&field.key) else {
                continue; // absent → leave the stored/default value untouched
            };
            match field.widget.coerce(v) {
                Ok(clean) => {
                    out.insert(field.key.clone(), clean);
                }
                Err(message) => errs.push(FieldError {
                    key: field.key.clone(),
                    message,
                }),
            }
        }
        if errs.is_empty() { Ok(out) } else { Err(errs) }
    }
}

impl WidgetKind {
    /// Validate + normalize one submitted value against this widget's type.
    /// Rejects the wrong JSON type, an out-of-vocabulary choice, an unsafe URL,
    /// or an implausible email; clamps a number into range. Returns the canonical
    /// stored form (e.g. an integral `Number` becomes a JSON integer).
    pub fn coerce(&self, v: &Value) -> Result<Value, String> {
        match self {
            WidgetKind::Text { format } => {
                let s = v.as_str().ok_or("expected a string")?;
                match format {
                    TextFormat::Url => {
                        if !s.is_empty() && !ferropress_core::is_safe_href(s) {
                            return Err("not a valid or allowed URL".to_owned());
                        }
                    }
                    TextFormat::Email => {
                        if !s.is_empty() && !is_plausible_email(s) {
                            return Err("not a valid email address".to_owned());
                        }
                    }
                    TextFormat::Plain | TextFormat::Slug => {}
                }
                Ok(Value::String(s.to_owned()))
            }
            WidgetKind::TextArea => {
                let s = v.as_str().ok_or("expected a string")?;
                Ok(Value::String(s.to_owned()))
            }
            WidgetKind::Toggle { .. } => {
                let b = v.as_bool().ok_or("expected a boolean")?;
                Ok(Value::Bool(b))
            }
            WidgetKind::Number { min, max, step, .. } => {
                let mut n = v.as_f64().ok_or("expected a number")?;
                if !n.is_finite() {
                    return Err("not a finite number".to_owned());
                }
                if let Some(mn) = min {
                    n = n.max(*mn);
                }
                if let Some(mx) = max {
                    n = n.min(*mx);
                }
                // An integral (or unspecified) step stores a JSON integer so a
                // count like posts-per-page never round-trips as `10.0`.
                let integral = step.map(|s| s.fract() == 0.0).unwrap_or(true);
                if integral {
                    Ok(Value::from(n.round() as i64))
                } else {
                    serde_json::Number::from_f64(n)
                        .map(Value::Number)
                        .ok_or_else(|| "not a representable number".to_owned())
                }
            }
            WidgetKind::Select { options } | WidgetKind::Radio { options } => {
                let s = v.as_str().ok_or("expected a string")?;
                if options.iter().any(|c| c.value == s) {
                    Ok(Value::String(s.to_owned()))
                } else {
                    Err("not one of the allowed options".to_owned())
                }
            }
            WidgetKind::MediaPicker | WidgetKind::EntityRef { .. } => match v {
                Value::Null => Ok(Value::Null),
                Value::Number(n) if n.is_u64() => Ok(v.clone()),
                _ => Err("expected an object id or null".to_owned()),
            },
            WidgetKind::BlockEditor => {
                Err("block content is not a plain settings value".to_owned())
            }
        }
    }
}

/// A deliberately-lenient email check: one `@`, non-empty local + domain parts, a
/// dot in the domain, and no whitespace/control characters. Strict RFC validation
/// rejects addresses people actually use; this only rules out obvious garbage.
fn is_plausible_email(s: &str) -> bool {
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}
