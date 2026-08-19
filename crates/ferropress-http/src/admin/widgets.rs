//! Widget admin CRUD — the WordPress *Appearance → Widgets* surface, backend half.
//!
//! A [`Widget`](ferropress_core::Widget) is a classic typed sidebar/rail widget:
//! one relation-free row with a `kind`-tagged `config` blob, an `area` string
//! binding it to a theme-declared (or the reserved `_inactive` parking) rail, and
//! a `widget_order` ordinal within that area. Unlike menus/terms, a `Widget` has
//! NO SDL relations — every mutation (create/update/delete/reorder) is therefore
//! its OWN eventful `ChangeEvent`, so there is no `touch_*` settle-write anywhere
//! in this module (contrast `menus.rs`'s `touch_menu`, needed only because a menu
//! edit's relation `link`/`unlink` calls are eventless).
//!
//! The write contract is deliberately SPLIT (the T7 design ruling), not a
//! whole-area reconcile PUT like menus' whole-tree save:
//!   * per-widget lifecycle — [`create`] / [`update`] / [`delete`] — each acts on
//!     exactly one widget; [`update`] carries a terms-style `expected_rev`
//!     (mirrors `terms.rs`'s optimistic-concurrency token) and touches `title`+
//!     `config` ONLY, never `area`/`widget_order`.
//!   * ONE whole-board ordering endpoint — [`reorder`] — reassigns `area` +
//!     `widget_order` for LISTED EXISTING ids only; it can never create or
//!     delete a widget.
//!
//! Importing the menus whole-tree-reconcile machinery here would import its
//! delete-on-absent semantics into a domain of flat, relation-free rows — a
//! cross-area move or any stale client view would then silently destroy a
//! widget's config. The split contract removes that hazard structurally:
//! destruction requires an explicit [`delete`] call.
//!
//! Every endpoint is gated on [`Capability::ManageWidgets`] (Editor+) and
//! serialized under the ISOLATED [`AppState::widget_lock`] (see its doc: never
//! nested with `hierarchy_lock`/`taxonomy_lock`/`menu_lock` — a `Widget` touches
//! no page, term, or menu row).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use ferropress_core::role::Capability;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value};
use ferropress_core::{WIDGET_TYPE, sanitize_widget_html};
use ferropress_render_form::{ControlKind, Field, FormSchema, FormSection, SettingRefs};
use ferropress_serve::{WidgetSpec, widget_specs};

use super::setting_refs::resolve_refs;
use super::{AdminError, AdminJson, AuthedUser, i32_field, json_field, str_field};
use crate::AppState;

// ---- caps + key hygiene (MF17) -----------------------------------------------

/// A hard cap on widgets per area — applies to `_inactive` too (Owner Q&A #4: the
/// parking area is bounded like any other, not exempt).
const MAX_WIDGETS_PER_AREA: usize = 50;
/// A site-wide cap: the composed `widgets` ctx map ships every area on every
/// request under the permissive (no-`WidgetArea`-catalog) contract, so the total
/// count is bounded independently of any one area.
const MAX_WIDGETS_TOTAL: usize = 500;
/// Custom HTML's raw `html`, checked PRE-sanitize. Sanitization can EXPAND the
/// string (entity-encoding), so [`build_config`] asserts the cap AGAIN on the
/// sanitized output before it ever reaches the store.
const MAX_CUSTOM_HTML_BYTES: usize = 64 * 1024;
/// Every OTHER `TextArea` field's cap (custom_html's `html` uses the larger,
/// dedicated cap above instead).
const MAX_TEXT_BYTES: usize = 32 * 1024;
/// A widget's plain-text title (`Widget.title`, never a config key — MF24).
const MAX_WIDGET_TITLE: usize = 200;
/// Total serialized `config` size, checked after document-replace + sanitize.
const MAX_WIDGET_CONFIG_BYTES: usize = 128 * 1024;
/// Area keys: `MAX_AREA_KEY_LEN` chars, the SAME ASCII charset theme location
/// keys use (`themes.rs`'s own doc: "reuses the plugin-id charset" — a dot-free
/// ASCII id).
const MAX_AREA_KEY_LEN: usize = 100;
/// The one accepted `_`-prefixed (reserved) area key: the non-rendering PARKING
/// column WordPress calls "Inactive Widgets" (Owner Q&A #4). The `_` prefix is
/// reserved for system areas core may add later, so an admin-typed area key can
/// never collide with one; every OTHER `_`-prefixed key is refused.
pub(crate) const INACTIVE_AREA: &str = "_inactive";

/// Trim + validate a widget area key: non-empty, ≤ [`MAX_AREA_KEY_LEN`] chars, the
/// plugin-id ASCII charset (alphanumeric / `-` / `_` — `themes.rs`'s own location-key
/// precedent), and the `_` prefix reserved for [`INACTIVE_AREA`] alone. Otherwise
/// permissive: NO `WidgetArea` catalog backs this (a stranded/unknown area string is
/// a supported, surfaced concept, not an error) — this only rejects a MALFORMED key,
/// never an unrecognized one.
fn validate_area_key(raw: &str) -> Result<String, AdminError> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(AdminError::BadRequest("an area key is required".to_owned()));
    }
    if key.chars().count() > MAX_AREA_KEY_LEN {
        return Err(AdminError::BadRequest(format!(
            "area key must be at most {MAX_AREA_KEY_LEN} characters"
        )));
    }
    if !ferropress_core::is_valid_plugin_id(key) {
        return Err(AdminError::BadRequest(format!(
            "area key {key:?} may only contain letters, digits, `-`, and `_`"
        )));
    }
    if key.starts_with('_') && key != INACTIVE_AREA {
        return Err(AdminError::BadRequest(format!(
            "the `_` prefix is reserved for system areas; {key:?} is not one"
        )));
    }
    Ok(key.to_owned())
}

/// Trim + validate a widget title: no charset restriction (it is plain text,
/// escaped at render time like any other typed field), just the length cap. An
/// empty title is legal — WordPress renders no title chrome for a blank one.
fn validate_title(raw: &str) -> Result<String, AdminError> {
    let title = raw.trim();
    if title.chars().count() > MAX_WIDGET_TITLE {
        return Err(AdminError::BadRequest(format!(
            "title must be at most {MAX_WIDGET_TITLE} characters"
        )));
    }
    Ok(title.to_owned())
}

// ---- DTOs -------------------------------------------------------------------

/// One widget row on the wire — the composite GET's `widgets` array AND every
/// per-widget write's authoritative response. `rev` is `Widget.meta._rev` (the
/// [`UpdateWidgetRequest::expected_rev`] optimistic-concurrency token, terms.rs's
/// SF12 idiom).
#[derive(Clone, Serialize)]
pub struct WidgetRow {
    pub id: u64,
    pub area: String,
    pub widget_order: i32,
    /// The stored kind string, ALWAYS included — even when it names no kind the
    /// current build's [`widget_specs`] registry knows (MF9's admin half): the
    /// row is never filtered out, so it stays editable-by-delete and visible.
    pub kind: String,
    pub title: String,
    pub config: serde_json::Value,
    pub rev: i64,
}

/// One area row in the composite GET's `areas` list: a theme-DECLARED area, the
/// reserved [`INACTIVE_AREA`] parking column (`system: true`), or a STRANDED area
/// (a distinct `Widget.area` value that is neither) — the menus `locations`
/// `declared: false` row shape (`menus.rs:917-941`), with `system` as the extra
/// flag distinguishing the permanent parking column from a genuinely orphaned one.
#[derive(Serialize)]
pub struct WidgetAreaRow {
    pub key: String,
    pub label: String,
    pub declared: bool,
    pub system: bool,
}

/// One entry in the composite GET's `kinds` catalogue — straight off
/// [`widget_specs`], serialized the same way settings/plugin config ship a
/// [`FormSchema`]. The admin NEVER compiles its own copy of this list into wasm.
#[derive(Serialize)]
pub struct WidgetKindDto {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub schema: FormSchema,
}

/// `GET /admin/api/widgets` response — the ONE cold-session payload (MF14).
#[derive(Serialize)]
pub struct WidgetsCompositeDto {
    pub areas: Vec<WidgetAreaRow>,
    pub widgets: Vec<WidgetRow>,
    pub kinds: Vec<WidgetKindDto>,
    pub refs: SettingRefs,
}

/// `POST /admin/api/widgets` body.
#[derive(Deserialize)]
pub struct CreateWidgetRequest {
    pub area: String,
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub config: Option<serde_json::Map<String, JsonValue>>,
}

/// `PUT /admin/api/widgets/{id}` body — title+config ONLY, never area/order (T7).
/// `expected_rev` is REQUIRED (unlike `terms.rs`'s optional back-compat token):
/// this is a brand-new contract with no pre-existing client to stay compatible
/// with, so every save is precondition-checked, always.
#[derive(Deserialize)]
pub struct UpdateWidgetRequest {
    pub expected_rev: i64,
    pub title: String,
    pub config: serde_json::Map<String, JsonValue>,
}

// ---- handlers -----------------------------------------------------------------

/// `GET /admin/api/widgets` — the single composite cold-session payload (MF14):
/// `{areas, widgets, kinds, refs}`. `open_widgets` (the Inc-3 admin view) always
/// refetches this on entry rather than relying on any other view's warmed state.
pub async fn composite_get(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<WidgetsCompositeDto>, AdminError> {
    who.require(Capability::ManageWidgets)?;

    let widgets = load_widget_rows(&state).await?;
    let areas = build_area_rows(&widgets, &declared_areas());
    let kinds = widget_specs()
        .into_iter()
        .map(|s| WidgetKindDto {
            id: s.kind,
            label: s.label,
            description: s.description,
            schema: s.schema,
        })
        .collect();
    let refs = compute_refs(&state, &widgets).await?;

    Ok(Json(WidgetsCompositeDto {
        areas,
        widgets,
        kinds,
        refs,
    }))
}

/// `POST /admin/api/widgets` — create a widget: `kind` ∈ the registry (else 400),
/// `config` = the kind's schema defaults overlaid with the coerced submission
/// (MF11's document-replace), appended at `(max widget_order in area) + 1`.
/// Returns the full authoritative row.
pub async fn create(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<CreateWidgetRequest>,
) -> Result<Json<WidgetRow>, AdminError> {
    who.require(Capability::ManageWidgets)?;

    // ---- pure, pre-lock validation (MF17) --------------------------------
    let area = validate_area_key(&body.area)?;
    let title = validate_title(body.title.as_deref().unwrap_or(""))?;
    let specs = widget_specs();
    let spec = specs
        .iter()
        .find(|s| s.kind == body.kind)
        .ok_or_else(|| AdminError::BadRequest(format!("unknown widget kind {:?}", body.kind)))?;
    let submitted = body.config.unwrap_or_default();
    let config = build_config(spec, &submitted)?;

    // ---- capacity + placement, atomic under widget_lock ------------------
    let _guard = state.widget_lock.lock().await;
    let all = state.store.scan(&TypeName::from(WIDGET_TYPE)).await?;
    if all.len() >= MAX_WIDGETS_TOTAL {
        return Err(AdminError::BadRequest(format!(
            "the site may hold at most {MAX_WIDGETS_TOTAL} widgets"
        )));
    }
    let in_area: Vec<&Object> = all
        .iter()
        .filter(|o| str_field(o, "area").as_deref() == Some(area.as_str()))
        .collect();
    if in_area.len() >= MAX_WIDGETS_PER_AREA {
        return Err(AdminError::BadRequest(format!(
            "the area {area:?} may hold at most {MAX_WIDGETS_PER_AREA} widgets"
        )));
    }
    // `saturating_add` defends a poisoned row (widget_order == i32::MAX, only
    // reachable out-of-band — the API itself can never write past 50 per area)
    // from panicking a debug build or wrapping to i32::MIN in release, which
    // would silently render the "appended" widget FIRST after the next sort.
    let next_order = in_area
        .iter()
        .map(|o| i32_field(o, "widget_order"))
        .max()
        .map_or(0, |m| m.saturating_add(1));

    let mut fields: FieldMap = FieldMap::new();
    fields.insert("area".to_owned(), Value::String(area.clone()));
    fields.insert("widget_order".to_owned(), Value::I32(next_order));
    fields.insert("kind".to_owned(), Value::String(spec.kind.to_owned()));
    fields.insert("title".to_owned(), Value::String(title.clone()));
    fields.insert(
        "config".to_owned(),
        Value::Json(JsonValue::Object(config.clone())),
    );
    // No relations to settle (T7) — this Create IS the only event needed.
    fields.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let id = state
        .store
        .create(&TypeName::from(WIDGET_TYPE), fields)
        .await?;

    Ok(Json(WidgetRow {
        id: id.0,
        area,
        widget_order: next_order,
        kind: spec.kind.to_owned(),
        title,
        config: JsonValue::Object(config),
        rev: 0,
    }))
}

/// `PUT /admin/api/widgets/{id}` — update `title` + `config` ONLY. Terms-style
/// `expected_rev`: 409 BEFORE any other work on a mismatch (`terms.rs:427-432`
/// precedent), so a stale form can never silently clobber a concurrent edit.
/// Document-replace + kind-specific sanitize (MF11); the response echoes the
/// FINAL stored config so the author sees exactly what survived.
pub async fn update(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
    AdminJson(body): AdminJson<UpdateWidgetRequest>,
) -> Result<Json<WidgetRow>, AdminError> {
    who.require(Capability::ManageWidgets)?;

    // Kind-independent, so this half of validation can run before the lock.
    let title = validate_title(&body.title)?;

    let _guard = state.widget_lock.lock().await;
    let current = state
        .store
        .get(&TypeName::from(WIDGET_TYPE), ObjectId(id))
        .await?;

    // Stale-rev refusal FIRST — right after the fresh read, before any other
    // work (the terms.rs SF12 discipline).
    let current_rev = rev_of(&current);
    if body.expected_rev != current_rev {
        return Err(AdminError::Conflict(
            "this widget changed elsewhere — reload and re-apply".to_owned(),
        ));
    }

    let kind = str_field(&current, "kind").unwrap_or_default();
    let specs = widget_specs();
    let spec = specs.iter().find(|s| s.kind == kind).ok_or_else(|| {
        // MF9: an unknown-kind row is never silently dropped, but there is no
        // schema left to validate a config edit against — only DELETE is
        // offered for it (the composite GET still returns it, config intact).
        AdminError::BadRequest(format!(
            "widget {id} has kind {kind:?}, which this build does not recognize — it can only \
             be deleted, not edited"
        ))
    })?;

    let config = build_config(spec, &body.config)?;
    let (meta, new_rev) = bumped_meta(&current);

    let mut patch: FieldMap = FieldMap::new();
    patch.insert("title".to_owned(), Value::String(title.clone()));
    patch.insert(
        "config".to_owned(),
        Value::Json(JsonValue::Object(config.clone())),
    );
    patch.insert("meta".to_owned(), Value::Json(meta));
    state
        .store
        .update(&TypeName::from(WIDGET_TYPE), ObjectId(id), patch)
        .await?;

    Ok(Json(WidgetRow {
        id,
        area: str_field(&current, "area").unwrap_or_default(),
        widget_order: i32_field(&current, "widget_order"),
        kind: spec.kind.to_owned(),
        title,
        config: JsonValue::Object(config),
        rev: new_rev,
    }))
}

/// `DELETE /admin/api/widgets/{id}` — the ONLY way a widget is destroyed. 404 for
/// an id that never existed (an explicit `get` first, the menus.rs precedent, so
/// this answers 404 rather than any store-specific delete-of-missing behavior).
pub async fn delete(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<u64>,
) -> Result<StatusCode, AdminError> {
    who.require(Capability::ManageWidgets)?;
    let _guard = state.widget_lock.lock().await;
    state
        .store
        .get(&TypeName::from(WIDGET_TYPE), ObjectId(id))
        .await?;
    state
        .store
        .delete(&TypeName::from(WIDGET_TYPE), ObjectId(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /admin/api/widgets/order` — body `{area_key: [id, ...], ...}`, covering
/// only the areas it touches. Reassigns `area` + `widget_order` for LISTED
/// EXISTING ids only, renumbering `0..n-1` per listed area; NEVER creates, NEVER
/// deletes. Structural 400s (T7/MF1):
///   * any listed id that does not exist;
///   * any widget CURRENTLY stored in a LISTED area but absent from the WHOLE
///     body (catches a concurrent add the client hasn't seen — an id moving IN
///     from an UNLISTED area is legal and needs no prior appearance);
///   * an id listed more than once anywhere in the body.
///
/// Returns the authoritative board state (every widget, the composite GET's
/// `widgets` shape).
pub async fn reorder(
    State(state): State<AppState>,
    who: AuthedUser,
    AdminJson(body): AdminJson<BTreeMap<String, Vec<u64>>>,
) -> Result<Json<Vec<WidgetRow>>, AdminError> {
    who.require(Capability::ManageWidgets)?;

    // ---- pure, pre-lock validation ---------------------------------------
    // The submitted list length for a listed area IS its exact resulting size
    // (this endpoint only ever moves EXISTING ids), so the per-area cap needs
    // no store read — `_inactive` gets no exemption (Owner Q&A #4).
    let mut validated: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut seen_ids: HashSet<u64> = HashSet::new();
    for (raw_area, ids) in &body {
        let area = validate_area_key(raw_area)?;
        if ids.len() > MAX_WIDGETS_PER_AREA {
            return Err(AdminError::BadRequest(format!(
                "the area {area:?} may hold at most {MAX_WIDGETS_PER_AREA} widgets"
            )));
        }
        for &wid in ids {
            if !seen_ids.insert(wid) {
                return Err(AdminError::BadRequest(format!(
                    "widget {wid} is listed more than once"
                )));
            }
        }
        // Two raw keys that TRIM to the same area (e.g. "sidebar" and " sidebar")
        // must not silently let the later one clobber the earlier one's list —
        // that would apply only half the body while returning 200.
        if validated.insert(area.clone(), ids.clone()).is_some() {
            return Err(AdminError::BadRequest(format!(
                "area {area:?} appears more than once in the request"
            )));
        }
    }

    let _guard = state.widget_lock.lock().await;
    let all = state.store.scan(&TypeName::from(WIDGET_TYPE)).await?;
    let by_id: HashMap<u64, &Object> = all.iter().map(|o| (o.id.0, o)).collect();

    // Every listed id must exist.
    for ids in validated.values() {
        for &wid in ids {
            if !by_id.contains_key(&wid) {
                return Err(AdminError::BadRequest(format!(
                    "widget {wid} does not exist"
                )));
            }
        }
    }

    // Every widget CURRENTLY in a LISTED area must appear somewhere in the body.
    let listed_areas: HashSet<&str> = validated.keys().map(String::as_str).collect();
    let mentioned: HashSet<u64> = validated.values().flatten().copied().collect();
    for obj in &all {
        let area = str_field(obj, "area").unwrap_or_default();
        if listed_areas.contains(area.as_str()) && !mentioned.contains(&obj.id.0) {
            return Err(AdminError::BadRequest(format!(
                "widget {} is currently in the listed area {area:?} but missing from the \
                 request — refetch and retry",
                obj.id.0
            )));
        }
    }

    // Apply: renumber 0..n-1 per listed area; skip a write that changes nothing.
    for (area, ids) in &validated {
        for (order, &wid) in ids.iter().enumerate() {
            let obj = by_id[&wid];
            let new_order = order as i32;
            if str_field(obj, "area").as_deref() == Some(area.as_str())
                && i32_field(obj, "widget_order") == new_order
            {
                continue;
            }
            let mut patch: FieldMap = FieldMap::new();
            patch.insert("area".to_owned(), Value::String(area.clone()));
            patch.insert("widget_order".to_owned(), Value::I32(new_order));
            state
                .store
                .update(&TypeName::from(WIDGET_TYPE), ObjectId(wid), patch)
                .await?;
        }
    }

    Ok(Json(load_widget_rows(&state).await?))
}

// ---- config building (MF11 document-replace + MF17 caps) --------------------

/// Build the stored `config` for `kind` from a submitted values map: the kind's
/// schema DEFAULTS overlaid with the coerced submission — never `coerce_values`'
/// bare merge, which leaves an un-submitted key's stored/default value untouched
/// (built for Settings PATCH semantics, not this document-replace contract). No
/// ghost key survives a schema evolution; an absent-key behavior is deterministic.
///
/// Custom HTML's `html` is sanitized as a KIND-SPECIFIC post-coerce step OUTSIDE
/// the form machinery (no `ControlKind` sanitizes HTML — `TextArea` accepts any
/// string verbatim). For an EMPTY-schema kind (`pages`/`search`/`meta`) this
/// naturally returns an empty map regardless of what was submitted — `fields()`
/// and `defaults()` are both empty, so there is nothing to coerce or reject.
fn build_config(
    spec: &WidgetSpec,
    submitted: &serde_json::Map<String, JsonValue>,
) -> Result<serde_json::Map<String, JsonValue>, AdminError> {
    // Pre-sanitize size guard on the SUBMITTED bytes — sanitize can only be
    // assessed after coerce/sanitize run, but the raw-input cap must reject a
    // huge payload before spending any work on it.
    if spec.kind == "custom_html"
        && let Some(JsonValue::String(html)) = submitted.get("html")
        && html.len() > MAX_CUSTOM_HTML_BYTES
    {
        return Err(AdminError::BadRequest(format!(
            "custom HTML must be at most {MAX_CUSTOM_HTML_BYTES} bytes"
        )));
    }

    let coerced = spec.schema.coerce_values(submitted).map_err(|errs| {
        let detail = errs
            .iter()
            .map(|e| format!("{}: {}", e.key, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        AdminError::BadRequest(format!("invalid {} config — {detail}", spec.kind))
    })?;

    let mut config = spec.schema.defaults();
    for (k, v) in coerced {
        config.insert(k, v);
    }

    if spec.kind == "custom_html"
        && let Some(JsonValue::String(html)) = config.get("html")
    {
        let clean = sanitize_widget_html(html);
        // Sanitization can EXPAND via entity-encoding — assert the cap again on
        // the OUTPUT before it ever reaches the store.
        if clean.len() > MAX_CUSTOM_HTML_BYTES {
            return Err(AdminError::BadRequest(format!(
                "sanitized HTML exceeds {MAX_CUSTOM_HTML_BYTES} bytes"
            )));
        }
        config.insert("html".to_owned(), JsonValue::String(clean));
    }

    // Every OTHER TextArea field (custom_html's `html`, handled above, is
    // excluded) is capped at MAX_TEXT_BYTES.
    for field in spec.schema.fields() {
        if spec.kind == "custom_html" && field.key == "html" {
            continue;
        }
        if matches!(field.widget, ControlKind::TextArea)
            && let Some(JsonValue::String(s)) = config.get(&field.key)
            && s.len() > MAX_TEXT_BYTES
        {
            return Err(AdminError::BadRequest(format!(
                "{} must be at most {MAX_TEXT_BYTES} bytes",
                field.label
            )));
        }
    }

    let total_bytes = serde_json::to_vec(&config)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if total_bytes > MAX_WIDGET_CONFIG_BYTES {
        return Err(AdminError::BadRequest(format!(
            "widget config must be at most {MAX_WIDGET_CONFIG_BYTES} bytes"
        )));
    }

    Ok(config)
}

// ---- composite GET assembly ---------------------------------------------------

/// The active theme's declared widget areas. `ThemeRegistry`/`ThemeHandle` do not
/// expose them yet (that is Increment 2's `theme.toml [widget_areas]` manifest
/// work) — this is a CLEARLY-MARKED SEAM, not a stub: the endpoint's `areas`
/// contract is complete today (`_inactive` + every stranded area a stored
/// `Widget.area` names), the theme just declares nothing yet. Increment 2 plugs
/// `state.theme.widget_areas()` in here with no reshaping of [`build_area_rows`]
/// or the DTO.
fn declared_areas() -> Vec<(String, String)> {
    Vec::new()
}

/// Assemble the composite GET's `areas`: every DECLARED area (`declared: true`),
/// the reserved [`INACTIVE_AREA`] parking column (`system: true`), then every
/// STRANDED area — a distinct stored `Widget.area` that is neither declared nor
/// `_inactive` — alphabetically (the menus `locations` `declared: false` shape).
fn build_area_rows(widgets: &[WidgetRow], declared: &[(String, String)]) -> Vec<WidgetAreaRow> {
    let declared_keys: HashSet<&str> = declared.iter().map(|(k, _)| k.as_str()).collect();

    let mut areas: Vec<WidgetAreaRow> = declared
        .iter()
        .map(|(key, label)| WidgetAreaRow {
            key: key.clone(),
            label: label.clone(),
            declared: true,
            system: false,
        })
        .collect();

    areas.push(WidgetAreaRow {
        key: INACTIVE_AREA.to_owned(),
        label: "Inactive".to_owned(),
        declared: false,
        system: true,
    });

    let stored_areas: BTreeSet<&str> = widgets.iter().map(|w| w.area.as_str()).collect();
    for area in stored_areas {
        if declared_keys.contains(area) || area == INACTIVE_AREA {
            continue;
        }
        areas.push(WidgetAreaRow {
            key: area.to_owned(),
            label: area.to_owned(),
            declared: false,
            system: false,
        });
    }

    areas
}

/// Resolve the composite GET's `refs` sidecar through the SAME resolver settings/
/// plugin config use (MF4 admin half) — never a second, drifting implementation.
/// [`resolve_refs`] takes exactly one `(schema, values)` pair, so this builds ONE
/// synthetic schema covering every distinct `EntityRef` entity the REGISTRY's
/// schemas declare (today: `"menu"` — resolved from the registry, not from stored
/// rows, so the picker's option list is available even with zero widgets yet) and
/// every distinct `MediaPicker` id a STORED row actually has set (walked
/// generically per-kind, not a hardcoded field name, so a future kind gaining a
/// second `MediaPicker` field needs no change here).
async fn compute_refs(state: &AppState, widgets: &[WidgetRow]) -> Result<SettingRefs, AdminError> {
    let specs = widget_specs();
    let spec_by_kind: HashMap<&str, &WidgetSpec> = specs.iter().map(|s| (s.kind, s)).collect();

    let entities: BTreeSet<String> = specs
        .iter()
        .flat_map(|s| s.schema.fields())
        .filter_map(|f| match &f.widget {
            ControlKind::EntityRef { entity } => Some(entity.clone()),
            _ => None,
        })
        .collect();

    let mut media_ids: BTreeSet<u64> = BTreeSet::new();
    for w in widgets {
        let Some(spec) = spec_by_kind.get(w.kind.as_str()) else {
            continue; // unknown kind (MF9): nothing to resolve
        };
        for field in spec.schema.fields() {
            if matches!(field.widget, ControlKind::MediaPicker)
                && let Some(id) = w.config.get(&field.key).and_then(JsonValue::as_u64)
            {
                media_ids.insert(id);
            }
        }
    }

    let mut fields = Vec::with_capacity(entities.len() + media_ids.len());
    let mut values = serde_json::Map::new();
    for entity in entities {
        let key = format!("__entity_ref_{entity}");
        values.insert(key.clone(), JsonValue::Null);
        fields.push(synthetic_field(key, ControlKind::EntityRef { entity }));
    }
    for (i, id) in media_ids.into_iter().enumerate() {
        let key = format!("__media_{i}");
        values.insert(key.clone(), JsonValue::from(id));
        fields.push(synthetic_field(key, ControlKind::MediaPicker));
    }

    let synthetic = FormSchema {
        sections: vec![FormSection {
            id: "widgets-refs".to_owned(),
            title: String::new(),
            help: None,
            fields,
        }],
    };
    resolve_refs(state, &synthetic, &values).await
}

fn synthetic_field(key: String, widget: ControlKind) -> Field {
    Field {
        key,
        label: String::new(),
        help: None,
        default: JsonValue::Null,
        widget,
        visible_when: None,
    }
}

// ---- shared row/rev helpers ---------------------------------------------------

/// Every widget, sorted `(area, widget_order, id)` (MF21 determinism — the
/// deterministic tiebreak two widgets sharing an ordinal need).
async fn load_widget_rows(state: &AppState) -> Result<Vec<WidgetRow>, AdminError> {
    let mut rows: Vec<WidgetRow> = state
        .store
        .scan(&TypeName::from(WIDGET_TYPE))
        .await?
        .iter()
        .map(widget_row_from_object)
        .collect();
    rows.sort_by(|a, b| {
        a.area
            .cmp(&b.area)
            .then(a.widget_order.cmp(&b.widget_order))
            .then(a.id.cmp(&b.id))
    });
    Ok(rows)
}

fn widget_row_from_object(obj: &Object) -> WidgetRow {
    WidgetRow {
        id: obj.id.0,
        area: str_field(obj, "area").unwrap_or_default(),
        widget_order: i32_field(obj, "widget_order"),
        kind: str_field(obj, "kind").unwrap_or_default(),
        title: str_field(obj, "title").unwrap_or_default(),
        config: json_field(obj, "config").unwrap_or_else(|| serde_json::json!({})),
        rev: rev_of(obj),
    }
}

/// The current `Widget.meta._rev` counter (default `0` for a never-touched
/// widget) — the [`UpdateWidgetRequest::expected_rev`] token, and what
/// [`WidgetRow`] echoes. Unlike `terms.rs`'s `rev_of`, a freshly created widget
/// starts at `0` (not `1`): create never calls a settle-touch (T7 — no relation
/// to settle), so there is no second write to bump it during creation.
fn rev_of(obj: &Object) -> i64 {
    match obj.get("meta") {
        Some(Value::Json(j)) => j
            .get("_rev")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        _ => 0,
    }
}

/// The NEXT `meta` JSON value with `_rev` incremented by one from `obj`'s CURRENT
/// meta (preserving any other meta keys), plus that new rev. Mirrors `terms.rs`'s
/// `bumped_meta` — bumped in the SAME patch as the scalar update, so `update`'s
/// own write IS what advances the precondition token (no separate touch call:
/// a `Widget` has no relation to settle).
fn bumped_meta(obj: &Object) -> (serde_json::Value, i64) {
    let mut meta = match obj.get("meta") {
        Some(Value::Json(j)) if j.is_object() => j.clone(),
        _ => serde_json::json!({}),
    };
    let rev = rev_of(obj) + 1;
    meta.as_object_mut()
        .expect("meta is an object by construction above")
        .insert("_rev".to_owned(), serde_json::Value::from(rev));
    (meta, rev)
}
