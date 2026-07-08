//! THE `FormSchema -> rinch edit-UI` renderer. The single widget dispatch in the
//! whole system is the plain-Rust `match` in [`FieldRow`] (marker
//! `FERROPRESS-FORM-DISPATCH`); it must not be duplicated at any call site. Mirrors
//! `ferropress-render`'s one block->HTML dispatch, on the editing side.
//!
//! Reactivity model (see the rinch skill): a rinch component runs ONCE and builds the
//! DOM; dynamic bits are `{|| ...}` closures that surgically update. The tree is
//! `SchemaForm` → `SectionPanel` → `FieldRow` (+ `RadioOption` per radio button); a
//! component body runs once, so it can freely destructure owned props and run a
//! plain-Rust `match` (each arm returns an `rsx!{…}` control node embedded as a
//! `NodeHandle` child). Static optionals (a section note, a unit suffix, help text)
//! are built as a 0-or-1 `Vec<NodeHandle>` rather than a reactive `if let`, since they
//! never change.
//!
//! Text/number/select inputs are UNCONTROLLED — seeded once, edits written straight to
//! `values` (read back only on submit) — so typing never rebuilds them. The reactive
//! parts (radio/toggle `checked`, the switch on-class, a `visible_when` row's hidden
//! class) read a per-field `Signal<Value>` that mirrors the live value.

use std::collections::HashMap;
use std::rc::Rc;

use rinch::prelude::*;
use serde_json::Value;

use ferropress_render_form::{Field, FormSchema, FormSection, TextFormat, WidgetKind};

use crate::values::FormValues;

/// Shared context handed to every field row.
#[derive(Clone, Default)]
struct FormContext {
    /// The authoritative value map the caller reads on submit (writes on input).
    values: FormValues,
    /// One reactive `Signal<Value>` per field key, mirroring the live value so the
    /// reactive controls can track it without re-rendering text inputs.
    states: Rc<HashMap<String, Signal<Value>>>,
}

/// Build a 0-or-1 `Vec<NodeHandle>` from an optional string + a node builder — the
/// non-reactive way to embed a static optional child (a reactive `if let` in rsx is an
/// `Fn` closure that can't move the bound value out).
fn opt_node(value: Option<String>, build: impl FnOnce(String) -> NodeHandle) -> Vec<NodeHandle> {
    value.map(build).into_iter().collect()
}

/// Render a whole declarative form. Mount this in the admin SPA with the schema +
/// current values from `GET /admin/api/settings`; read `values.snapshot()` on Save.
#[component]
pub fn SchemaForm(schema: FormSchema, values: FormValues) -> NodeHandle {
    // One reactive state signal per field, seeded from the current value. Created at
    // build time (a component body is a reactive scope). Drives radio/toggle checked
    // and visible_when WITHOUT making text inputs reactive.
    let mut states = HashMap::new();
    for field in schema.fields() {
        states.insert(field.key.clone(), Signal::new(values.get(&field.key)));
    }
    let ctx = FormContext {
        values,
        states: Rc::new(states),
    };

    rsx! {
        div { class: "settings-form",
            for section in schema.sections.clone() {
                SectionPanel { key: section.id.clone(), section: section, ctx: ctx.clone() }
            }
        }
    }
}

/// One schema section = a titled panel of field rows.
#[component]
fn SectionPanel(section: FormSection, ctx: FormContext) -> NodeHandle {
    let title = section.title.clone();
    let note = opt_node(
        section.help.clone(),
        |n| rsx! { p { class: "panel__note", {n} } },
    );
    rsx! {
        section { class: "panel",
            h3 { class: "panel__title", {title} }
            {note}
            for field in section.fields.clone() {
                FieldRow { key: field.key.clone(), field: field, ctx: ctx.clone() }
            }
        }
    }
}

/// One field: a two-column set line (label/help + control), optionally hidden by a
/// `visible_when` condition. Holds the SINGLE widget dispatch.
#[component]
fn FieldRow(field: Field, ctx: FormContext) -> NodeHandle {
    // This field's own live-value signal (present for every field).
    let state = *ctx
        .states
        .get(&field.key)
        .expect("every field has a state signal");

    // The controlling signal + target value for a visible_when field (if any).
    let vis: Option<(Signal<Value>, Value)> = field
        .visible_when
        .as_ref()
        .and_then(|c| ctx.states.get(&c.key).map(|s| (*s, c.equals.clone())));

    let label = field.label.clone();
    let help = opt_node(
        field.help.clone(),
        |h| rsx! { p { class: "setrow__help", {h} } },
    );

    // FERROPRESS-FORM-DISPATCH — the one and only WidgetKind -> edit-control match.
    // A plain-Rust match (so each arm may use `let`) returning the control node. Moved
    // by value (a partial move of `field.widget`) — the arms only ever read `field.key`
    // afterward, so no clone of the widget's option vectors is needed.
    let control: NodeHandle = match field.widget {
        WidgetKind::Text { format } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let initial = state.get().as_str().unwrap_or_default().to_owned();
            let cls = match format {
                TextFormat::Email | TextFormat::Url => "input input--mono",
                TextFormat::Plain | TextFormat::Slug => "input",
            };
            let itype = match format {
                TextFormat::Email => "email",
                TextFormat::Url => "url",
                TextFormat::Plain | TextFormat::Slug => "text",
            };
            rsx! {
                input {
                    class: cls,
                    r#type: itype,
                    value: initial,
                    spellcheck: "false",
                    oninput: move |v: String| {
                        let val = Value::String(v);
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                }
            }
        }
        WidgetKind::TextArea => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let initial = state.get().as_str().unwrap_or_default().to_owned();
            rsx! {
                textarea {
                    class: "input",
                    rows: "4",
                    oninput: move |v: String| {
                        let val = Value::String(v);
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                    {initial}
                }
            }
        }
        WidgetKind::Number {
            min,
            max,
            step,
            unit,
        } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let initial = match state.get() {
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            let min_attr = min.map(|n| n.to_string()).unwrap_or_default();
            let max_attr = max.map(|n| n.to_string()).unwrap_or_default();
            let step_attr = step
                .map(|n| n.to_string())
                .unwrap_or_else(|| "1".to_owned());
            let unit_node = opt_node(unit, |u| rsx! { span { class: "numfield__unit", {u} } });
            rsx! {
                span { class: "numfield",
                    input {
                        class: "input input--number",
                        r#type: "number",
                        min: min_attr,
                        max: max_attr,
                        step: step_attr,
                        value: initial,
                        oninput: move |v: String| {
                            // Keep the prior value if the field is cleared / mid-edit;
                            // the server clamps + integralizes.
                            if let Ok(n) = v.trim().parse::<f64>()
                                && let Some(num) = serde_json::Number::from_f64(n)
                            {
                                let val = Value::Number(num);
                                state.set(val.clone());
                                values.set(&key, val);
                            }
                        },
                    }
                    {unit_node}
                }
            }
        }
        WidgetKind::Toggle { text } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let text_node = opt_node(text, |t| rsx! { span { class: "switch__text", {t} } });
            rsx! {
                label { class: "switch",
                    input {
                        r#type: "checkbox",
                        checked: move || state.get() == Value::Bool(true),
                        oninput: move |checked: String| {
                            let val = Value::Bool(checked == "true");
                            state.set(val.clone());
                            values.set(&key, val);
                        },
                    }
                    span {
                        class: {
                            move || if state.get() == Value::Bool(true) {
                                "switch__track is-on"
                            } else {
                                "switch__track"
                            }
                        },
                        span { class: "switch__knob" }
                    }
                    {text_node}
                }
            }
        }
        WidgetKind::Select { options } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let current = state.get().as_str().unwrap_or_default().to_owned();
            // Current-first ordering: the uncontrolled <select> shows its first option,
            // and rinch emits a boolean `selected` even when false — so per-option
            // `selected` can't mark just one.
            let mut ordered = Vec::with_capacity(options.len());
            if let Some(cur) = options.iter().find(|c| c.value == current) {
                ordered.push(cur.clone());
            }
            for c in &options {
                if c.value != current {
                    ordered.push(c.clone());
                }
            }
            let opts: Vec<NodeHandle> = ordered
                .into_iter()
                .map(|opt| rsx! { option { value: opt.value.clone(), {opt.label.clone()} } })
                .collect();
            rsx! {
                select {
                    class: "select",
                    oninput: move |v: String| {
                        let val = Value::String(v);
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                    {opts}
                }
            }
        }
        WidgetKind::Radio { options } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            rsx! {
                div { class: "radiogroup",
                    for opt in options.clone() {
                        RadioOption {
                            key: opt.value.clone(),
                            field_key: key.clone(),
                            value: opt.value.clone(),
                            label: opt.label.clone(),
                            state: state,
                            values: values.clone(),
                        }
                    }
                }
            }
        }
        // Forward-declared widgets not wired into the settings surface yet.
        WidgetKind::MediaPicker | WidgetKind::EntityRef { .. } | WidgetKind::BlockEditor => {
            rsx! { p { class: "setrow__help", "This field type isn\u{2019}t editable here yet." } }
        }
    };

    rsx! {
        div {
            class: {
                // Clone `vis` INTO the reactive closure — a component render is an
                // `FnMut`, so the closure must own a fresh copy, not move the outer one.
                let vis = vis.clone();
                move || match &vis {
                    Some((sig, equals)) if sig.get().ne(equals) => "setrow is-hidden".to_owned(),
                    _ => "setrow".to_owned(),
                }
            },
            div { class: "setrow__label", {label} }
            div { class: "setrow__control", {control} }
            {help}
        }
    }
}

/// One radio button in a group. Its own component so the reactive `checked` closure and
/// the oninput each capture an owned copy of the option value (a reactive `for`/`.map`
/// body can't move the non-`Copy` value into two closures).
#[component]
fn RadioOption(
    field_key: String,
    value: String,
    label: String,
    state: Signal<Value>,
    values: FormValues,
) -> NodeHandle {
    // Pre-extract distinct locals so the render `FnMut` uses each binding exactly once
    // inside `rsx!` (moved once for a static attr, or borrow-then-cloned once for a
    // reactive closure). Using any single binding twice inside the render makes it an
    // `FnOnce`, which rinch rejects.
    let name = field_key.clone();
    let attr_value = value.clone();
    let checked_value = value.clone();
    let set_value = value;
    let set_key = field_key;
    rsx! {
        label { class: "radio",
            input {
                r#type: "radio",
                name: name,
                value: attr_value,
                checked: {
                    let cv = checked_value.clone();
                    move || state.get() == Value::String(cv.clone())
                },
                oninput: {
                    let sv = set_value.clone();
                    let sk = set_key.clone();
                    let vals = values.clone();
                    move |checked: String| {
                        if checked == "true" {
                            let val = Value::String(sv.clone());
                            state.set(val.clone());
                            vals.set(&sk, val);
                        }
                    }
                },
            }
            {label}
        }
    }
}
