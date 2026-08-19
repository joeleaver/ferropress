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
//! Text/textarea inputs are CONTROLLED: a reactive `value: {|| state.get()…}` binding
//! keeps them in sync with the field's `Signal<Value>` and stays caret-safe — the pinned
//! rinch (`2ea7625`, upstream #100) reflects the DOM *property* (not just the attribute)
//! and only when it differs, so the type→oninput→signal echo is a no-op. `Select`/`EntityRef`
//! render their options in natural order and mark the current one with a per-option
//! `selected` bool (again property-reflected, so the stored value's option shows on load; a
//! value no longer among the options falls back to a synthetic selected entry, as EntityRef
//! does). `Number` is the deliberate exception — it stays UNCONTROLLED (seeded once): a
//! numeric `value:` must round-trip the in-progress string through a numeric `Value`, which
//! can't preserve what the user is typing (an integer gains a spurious `.0`; a half-typed
//! "1." snaps to "1.0", jumping the caret to the end), and this form never re-seeds a field
//! externally, so controlled buys it nothing.
//! The reactive parts (radio/toggle `checked`, the switch on-class, a `visible_when` row's
//! hidden class) read the same per-field `Signal<Value>` that mirrors the live value.

use std::collections::HashMap;
use std::rc::Rc;

use rinch::prelude::*;
use serde_json::Value;

use ferropress_render_form::{
    ControlKind, Field, FormSchema, FormSection, SettingRefs, TextFormat,
};

use crate::values::FormValues;

/// The sink the host invokes with a picked media's `(object id, /media/{uuid} URL)`.
pub type MediaChosen = Rc<dyn Fn(u64, String)>;

/// A host-provided media picker. The `MediaPicker` widget can't open a file dialog or
/// upload — this crate is deliberately free of `web-sys`/`gloo-net` (see the crate
/// doc) — so the admin supplies this: given a [`MediaChosen`] sink, it opens its
/// picker (library + upload) and calls the sink with the chosen media's id + URL.
/// Threading it as a prop is what keeps form-view a pure DOM projection.
///
/// It is a plain struct — not a bare `Rc<dyn Fn>` and never wrapped in `Option` as a
/// prop — because `rsx!` routes both callable and `Option<_>` prop values through an
/// event-handler coercion that recurses without terminating. A default (no-op) picker
/// stands in for "no host picker" so the prop can stay a non-`Option` struct.
#[derive(Clone)]
pub struct OnPickMedia(Rc<dyn Fn(MediaChosen)>);

impl OnPickMedia {
    /// Wrap a picker closure (called with the field's sink when the user clicks Choose).
    pub fn new(open_picker: impl Fn(MediaChosen) + 'static) -> Self {
        Self(Rc::new(open_picker))
    }

    /// Open the picker for `sink` (invoked by the `MediaPicker` widget on Choose).
    fn open(&self, sink: MediaChosen) {
        (self.0)(sink)
    }
}

impl Default for OnPickMedia {
    /// A no-op picker: the `MediaPicker` "Choose" button does nothing. Used when a form
    /// is mounted without a host picker (a test, or a schema that has no `MediaPicker`).
    fn default() -> Self {
        Self(Rc::new(|_sink| {}))
    }
}

/// Shared context handed to every field row.
#[derive(Clone, Default)]
struct FormContext {
    /// The authoritative value map the caller reads on submit (writes on input).
    values: FormValues,
    /// One reactive `Signal<Value>` per field key, mirroring the live value so the
    /// reactive controls can track it without re-rendering text inputs.
    states: Rc<HashMap<String, Signal<Value>>>,
    /// Resolved references for the id-valued widgets: `EntityRef` dropdown options
    /// and `MediaPicker` thumbnail URLs (see [`SettingRefs`]).
    refs: Rc<SettingRefs>,
    /// The host's media picker; drives the `MediaPicker` "Choose" button (a no-op
    /// default when the host provides none). NOT named `on_*` — `rsx!` treats any
    /// `on*` prop as an event handler and mis-coerces it.
    media_picker: OnPickMedia,
}

/// Build a 0-or-1 `Vec<NodeHandle>` from an optional string + a node builder — the
/// non-reactive way to embed a static optional child (a reactive `if let` in rsx is an
/// `Fn` closure that can't move the bound value out).
fn opt_node(value: Option<String>, build: impl FnOnce(String) -> NodeHandle) -> Vec<NodeHandle> {
    value.map(build).into_iter().collect()
}

/// Render a whole declarative form. Mount this in the admin SPA with the schema,
/// current values, and resolved `refs` from `GET /admin/api/settings`; read
/// `values.snapshot()` on Save. `on_pick_media` wires the `MediaPicker` widget to the
/// host's file-dialog + upload + library modal (see [`OnPickMedia`]); pass `None`
/// when a schema uses no `MediaPicker` (the button then no-ops).
#[component]
pub fn SchemaForm(
    schema: FormSchema,
    values: FormValues,
    refs: SettingRefs,
    media_picker: OnPickMedia,
) -> NodeHandle {
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
        refs: Rc::new(refs),
        media_picker,
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

    // FERROPRESS-FORM-DISPATCH — the one and only ControlKind -> edit-control match.
    // A plain-Rust match (so each arm may use `let`) returning the control node. Moved
    // by value (a partial move of `field.widget`) — the arms only ever read `field.key`
    // afterward, so no clone of the widget's option vectors is needed.
    let control: NodeHandle = match field.widget {
        ControlKind::Text { format } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
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
                    // Controlled: the reactive `value` mirrors the signal (a string round-
                    // trips exactly, so the oninput echo is a no-op write) and is caret-safe.
                    value: {move || state.get().as_str().unwrap_or_default().to_owned()},
                    spellcheck: "false",
                    oninput: move |v: String| {
                        let val = Value::String(v);
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                }
            }
        }
        ControlKind::TextArea => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            rsx! {
                textarea {
                    class: "input",
                    rows: "4",
                    // Controlled: rinch reflects `value` onto the <textarea>'s DOM property
                    // (web_document `sync_reflected_property`, only-when-differs), so the
                    // reactive binding seeds and stays in sync WITHOUT a child text node.
                    value: {move || state.get().as_str().unwrap_or_default().to_owned()},
                    oninput: move |v: String| {
                        let val = Value::String(v);
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                }
            }
        }
        ControlKind::Number {
            min,
            max,
            step,
            unit,
        } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            // Uncontrolled ON PURPOSE (unlike Text/Select): seed once, then let the browser
            // own the string. A controlled numeric `value:` would have to re-derive the
            // display from the `Value::Number` on every keystroke, and that round-trip can't
            // preserve the in-progress text — `Number::from_f64(10.0).to_string()` is
            // "10.0", so "10" would snap to "10.0" and a half-typed "1." (parsed to 1.0)
            // would snap to "1.0", each jumping the caret to the end mid-entry. Since this
            // form never re-seeds a field externally, controlled would add that hazard for no
            // benefit. The `oninput` keeps the prior value on a clear/mid-edit; the server
            // clamps + integralizes.
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
        ControlKind::Toggle { text } => {
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
        ControlKind::Select { options } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let current = state.get().as_str().unwrap_or_default().to_owned();
            // Whether the stored value is still among the options (checked before the map
            // consumes `options`), so a value dropped by a schema revision gets a fallback.
            let current_present = options.iter().any(|c| c.value == current);
            // Natural order + a per-option static `selected` bool marking the stored value:
            // rinch reflects the `selected` property and unsets a stringified `false` (pin
            // `2ea7625`, #100), so the stored value's option shows on load regardless of DOM
            // order. A `Copy` bool is enough — the initial mark is all a <select> needs (the
            // browser tracks live selection after, and a form re-seed rebuilds this arm
            // fresh); the non-`Copy` `String` value rules out a reactive per-option closure.
            let mut opts: Vec<NodeHandle> = options
                .into_iter()
                .map(|opt| {
                    let selected = opt.value == current;
                    rsx! { option { value: opt.value, selected: selected, {opt.label} } }
                })
                .collect();
            // If the stored value is no longer one of the options (a schema revision dropped
            // it after it was saved), append it as its own selected option so the control
            // reflects the real value instead of silently defaulting to the first — mirrors
            // the EntityRef "(not published)" fallback, so exactly one option stays marked.
            if !current.is_empty() && !current_present {
                let label = format!("{current} (unavailable)");
                opts.push(rsx! { option { value: current, selected: true, {label} } });
            }
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
        ControlKind::Radio { options } => {
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
        // Picks a Media object id: a thumbnail of the current selection (resolved via
        // `refs.media`) plus Choose/Replace/Remove. The dialog + upload + library live
        // in the host (via `on_pick_media`); this widget only shows + writes the id.
        ControlKind::MediaPicker => {
            // The current selection's thumbnail URL, empty when unset (an `Option` prop
            // trips the rsx event-handler coercion, so "" stands in for "no thumbnail").
            let initial_url = state
                .get()
                .as_u64()
                .and_then(|id| ctx.refs.media_url(id))
                .map(str::to_owned)
                .unwrap_or_default();
            rsx! {
                MediaField {
                    field_key: field.key.clone(),
                    state: state,
                    values: ctx.values.clone(),
                    initial_url: initial_url,
                    media_picker: ctx.media_picker.clone(),
                }
            }
        }
        // Picks an object id of a named entity (a `Page` today) from a dropdown of the
        // candidates the server resolved into `refs.entity_options`. Stores a `u64` id
        // or null (the empty option) — the shape `ControlKind::coerce` validates.
        ControlKind::EntityRef { entity } => {
            let key = field.key.clone();
            let values = ctx.values.clone();
            let options = ctx.refs.options_for(&entity);
            let current_id = state.get().as_u64();
            let current_key = current_id.map(|id| id.to_string()).unwrap_or_default();

            // (value, label) entries: a placeholder for "none", each candidate, and —
            // if the stored id is no longer a candidate (e.g. a page since unpublished)
            // — the stored id itself, so the control still reflects the real value.
            let mut entries: Vec<(String, String)> = Vec::with_capacity(options.len() + 2);
            entries.push((String::new(), "\u{2014} Select a page \u{2014}".to_owned()));
            for o in options {
                entries.push((o.id.to_string(), o.label.clone()));
            }
            if let Some(id) = current_id
                && !options.iter().any(|o| o.id == id)
            {
                entries.push((id.to_string(), format!("Page #{id} (not published)")));
            }
            // Natural order + a per-option `selected` bool marking the stored id (mirrors
            // the `Select` arm; rinch reflects the `selected` property so exactly one is
            // marked — pin `2ea7625`, #100).
            let opts: Vec<NodeHandle> = entries
                .into_iter()
                .map(|(v, l)| {
                    let selected = v == current_key;
                    rsx! { option { value: v, selected: selected, {l} } }
                })
                .collect();
            rsx! {
                select {
                    class: "select",
                    oninput: move |v: String| {
                        // Empty option → null; else parse the id (a malformed value
                        // can't occur from our own options, but fall back to null).
                        let val = if v.is_empty() {
                            Value::Null
                        } else {
                            v.parse::<u64>().ok().map(Value::from).unwrap_or(Value::Null)
                        };
                        state.set(val.clone());
                        values.set(&key, val);
                    },
                    {opts}
                }
            }
        }
        // The rich block-tree body editor isn't mounted through this form (the post
        // editor mounts the rinch editor directly).
        ControlKind::BlockEditor => {
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

/// The `MediaPicker` control: a thumbnail of the current selection (or a neutral
/// placeholder) plus Choose/Replace + Remove. Its own component so its click handlers
/// can OWN their non-`Copy` captures (`values`, the field key, the host callback) —
/// the component body runs once, so each is moved into exactly one handler, while the
/// reactive bits (thumbnail visibility + button labels) read only the `Copy` `thumb`
/// signal (mirroring how [`RadioOption`] isolates its captures).
///
/// The stored VALUE is the media object id; `thumb` is the local view of its URL,
/// seeded from the server-resolved `refs` and updated when the host reports a pick.
/// All browser work (file dialog, upload, library) is the host's, reached through
/// [`OnPickMedia`] — this widget only shows the selection and writes the id.
#[component]
fn MediaField(
    field_key: String,
    state: Signal<Value>,
    values: FormValues,
    initial_url: String,
    media_picker: OnPickMedia,
) -> NodeHandle {
    // Local reactive view of the selected media's URL (`None` = unset). A Copy signal,
    // so the reactive class/src/label closures below read it without moving anything.
    let thumb = Signal::new(if initial_url.is_empty() {
        None
    } else {
        Some(initial_url)
    });

    // The sink the host calls after a pick: write the id into both the live value map
    // and this field's state signal, and show the new thumbnail.
    let chosen: MediaChosen = {
        let values = values.clone();
        let key = field_key.clone();
        Rc::new(move |id: u64, url: String| {
            let val = Value::from(id);
            state.set(val.clone());
            values.set(&key, val);
            thumb.set(Some(url));
        })
    };

    // Choose/Replace: hand the host our sink; it opens its picker and calls back (the
    // default picker is a no-op, so this is safe even with no host picker).
    let choose = move || media_picker.open(chosen.clone());

    // Remove: clear the value + state + thumbnail (a null id = "no media").
    let remove = {
        let values = values.clone();
        let key = field_key.clone();
        move || {
            state.set(Value::Null);
            values.set(&key, Value::Null);
            thumb.set(None);
        }
    };

    rsx! {
        div { class: "mediapick",
            // Placeholder (shown when unset) and thumbnail (shown when set) are both
            // present; a reactive class toggles which is visible, so there is never a
            // broken <img> with an empty src.
            span {
                class: {
                    move || if thumb.get().is_some() {
                        "mediapick__thumb is-hidden"
                    } else {
                        "mediapick__thumb is-empty"
                    }
                },
                "\u{2295}"
            }
            img {
                class: {
                    move || if thumb.get().is_some() {
                        "mediapick__thumb"
                    } else {
                        "mediapick__thumb is-hidden"
                    }
                },
                src: { move || thumb.get().unwrap_or_default() },
                alt: "",
            }
            div { class: "mediapick__actions",
                button {
                    class: "btn btn--ghost", style: "width:auto",
                    onclick: choose,
                    { move || if thumb.get().is_some() { "Replace\u{2026}" } else { "Choose image\u{2026}" } }
                }
                button {
                    class: {
                        move || if thumb.get().is_some() {
                            "btn btn--quiet"
                        } else {
                            "btn btn--quiet is-hidden"
                        }
                    },
                    style: "width:auto",
                    onclick: remove,
                    "Remove"
                }
            }
        }
    }
}
