//! Resolve the id-valued widgets in a [`FormSchema`] against the store, producing
//! the [`SettingRefs`] sidecar shipped beside `{schema, values}`.
//!
//! `MediaPicker` and `EntityRef` store a bare object id (see
//! [`ControlKind::coerce`](ferropress_render_form::ControlKind)). An id alone can't be
//! rendered: a media picker needs the picked image's `/media/{uuid}` URL for its
//! thumbnail, and a page picker needs the list of pages to choose from. The schema
//! is deliberately store-free, so this — the ONE place that reaches the store for
//! these — resolves them once per response. It runs for both site settings and
//! plugin settings (it inspects only the schema + values), so a plugin that declares
//! a `MediaPicker`/`EntityRef` in its `[settings]` form gets working pickers for free.
//!
//! [`build_settings_dto`] is the single assembly point every `GET`/`PUT` settings
//! handler goes through, so the sidecar can never be forgotten at one call site.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value as JsonValue};

use ferropress_core::value::{ObjectId, TypeName};
use ferropress_core::{CoreError, MEDIA_TYPE, MENU_TYPE, PAGE_TYPE, Status, media_url};
use ferropress_render_form::{ControlKind, EntityOption, FormSchema, MediaRef, SettingRefs};

use super::settings::SettingsDto;
use super::{AdminError, str_field};
use crate::AppState;

/// Assemble the full settings response: the declarative `schema`, the current
/// `values`, and the resolved `refs` the id-valued widgets need to render. Every
/// settings `GET`/`PUT` (site AND plugin) returns through here so all four responses
/// carry a consistent sidecar.
pub(crate) async fn build_settings_dto(
    state: &AppState,
    schema: FormSchema,
    values: Map<String, JsonValue>,
) -> Result<SettingsDto, AdminError> {
    let refs = resolve_refs(state, &schema, &values).await?;
    Ok(SettingsDto {
        schema,
        values,
        refs,
    })
}

/// Resolve every id-valued widget the schema declares: the candidate list behind each
/// distinct `EntityRef` entity, and the display data for each `MediaPicker` value
/// currently set. Fields whose widget is neither are ignored.
///
/// `pub(crate)` (not just called via [`build_settings_dto`]) so `admin::widgets`'s
/// composite GET can route its own `refs` sidecar through this SAME resolver (MF4
/// admin half) — a widget's config uses the identical bare-id `EntityRef`/
/// `MediaPicker` encoding settings/plugin config does, so there is exactly one
/// place that ever turns an id into a display option.
pub(crate) async fn resolve_refs(
    state: &AppState,
    schema: &FormSchema,
    values: &Map<String, JsonValue>,
) -> Result<SettingRefs, AdminError> {
    // Gather the distinct entities to enumerate and the media ids to resolve in one
    // schema pass, so each entity type is queried once even if several fields use it.
    let mut entities: BTreeSet<&str> = BTreeSet::new();
    let mut media_ids: BTreeSet<u64> = BTreeSet::new();
    for field in schema.fields() {
        match &field.widget {
            ControlKind::EntityRef { entity } => {
                entities.insert(entity.as_str());
            }
            ControlKind::MediaPicker => {
                if let Some(id) = values.get(&field.key).and_then(JsonValue::as_u64) {
                    media_ids.insert(id);
                }
            }
            _ => {}
        }
    }

    let mut entity_options = BTreeMap::new();
    for entity in entities {
        entity_options.insert(
            entity.to_owned(),
            resolve_entity_options(state, entity).await?,
        );
    }

    let mut media = BTreeMap::new();
    for id in media_ids {
        if let Some(m) = resolve_media(state, id).await? {
            media.insert(id.to_string(), m);
        }
    }

    Ok(SettingRefs {
        entity_options,
        media,
    })
}

/// The pickable options for an `EntityRef` entity: `"page"` (the PUBLISHED pages —
/// the only ones that render as a public front page) or `"menu"` (EVERY nav menu,
/// unfiltered — a `Widget`'s Nav Menu kind has no publish-state concept the way a
/// front-page target does; a menu with no items or no location assignment is still
/// a legal pick, and simply composes to nothing per the widgets design's totality
/// rule). Both labelled for a stable, deterministic list. An unsupported entity
/// yields an empty list, so the control renders with no choices rather than erroring.
async fn resolve_entity_options(
    state: &AppState,
    entity: &str,
) -> Result<Vec<EntityOption>, AdminError> {
    match entity {
        "page" => {
            let mut opts: Vec<EntityOption> = state
                .store
                .scan(&TypeName::from(PAGE_TYPE))
                .await?
                .iter()
                .filter(|o| str_field(o, "status").as_deref() == Some(Status::Published.as_str()))
                .map(|o| EntityOption {
                    id: o.id.0,
                    label: page_label(o),
                })
                .collect();
            // Alphabetical by label (id as a tiebreak) — a deterministic, human order,
            // since the store has no ORDER BY primitive.
            opts.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
            Ok(opts)
        }
        "menu" => {
            let mut opts: Vec<EntityOption> = state
                .store
                .scan(&TypeName::from(MENU_TYPE))
                .await?
                .iter()
                .map(|o| EntityOption {
                    id: o.id.0,
                    label: menu_label(o),
                })
                .collect();
            opts.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
            Ok(opts)
        }
        _ => Ok(Vec::new()),
    }
}

/// A display label for a page option: its title, falling back to its slug then id so a
/// titleless page is still selectable.
fn page_label(obj: &ferropress_core::value::Object) -> String {
    let title = str_field(obj, "title").unwrap_or_default();
    if !title.trim().is_empty() {
        return title;
    }
    match str_field(obj, "slug") {
        Some(slug) if !slug.is_empty() => format!("/{slug}"),
        _ => format!("Page #{}", obj.id.0),
    }
}

/// A display label for a menu option: its name, falling back to its slug then id.
fn menu_label(obj: &ferropress_core::value::Object) -> String {
    let name = str_field(obj, "name").unwrap_or_default();
    if !name.trim().is_empty() {
        return name;
    }
    match str_field(obj, "slug") {
        Some(slug) if !slug.is_empty() => slug,
        _ => format!("Menu #{}", obj.id.0),
    }
}

/// Resolve one selected media id to `{id, url}`, or `None` if the media no longer
/// exists (a settings value is a plain id with no `@on_delete` backing, so it can
/// dangle — the picker then shows the neutral "choose" state). Mirrors the post
/// editor's `resolve_featured`.
async fn resolve_media(state: &AppState, id: u64) -> Result<Option<MediaRef>, AdminError> {
    match state
        .store
        .get(&TypeName::from(MEDIA_TYPE), ObjectId(id))
        .await
    {
        Ok(obj) => Ok(Some(MediaRef {
            id,
            url: media_url(&str_field(&obj, "uuid").unwrap_or_default()),
        })),
        Err(CoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
