//! Reference-resolution DATA shipped alongside a [`FormSchema`](crate::FormSchema)
//! so the admin can render the two id-valued widgets — `MediaPicker` and
//! `EntityRef` — as real controls.
//!
//! A `MediaPicker`/`EntityRef` value is a bare object id (`u64` or null; see
//! [`WidgetKind::coerce`](crate::WidgetKind)). An id alone can't be *shown*: a
//! media picker needs the picked image's `/media/{uuid}` URL for its thumbnail, and
//! a page picker needs the list of pages to choose from. Neither is derivable from
//! the schema (which is deliberately store-free and deterministic) — both require a
//! store lookup. So the HTTP layer resolves them once and ships this **sidecar**
//! beside `{schema, values}`; the wasm admin reads it to render the controls.
//!
//! Like [`FormSchema`](crate::FormSchema) this is pure serde DATA (no rinch, no
//! store) so it crosses the wire and both ends share the exact type. The
//! resolution itself (the store queries) lives in `ferropress-http`; the rendering
//! in `ferropress-form-view`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The resolved references for one form: the candidate lists behind every
/// `EntityRef` and the display data for every `MediaPicker` selection currently in
/// the values map. Empty by default (a schema with neither widget ships an empty
/// sidecar), and `#[serde(default)]` on the DTO field so an older client that
/// doesn't know about it round-trips unaffected.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SettingRefs {
    /// For each `EntityRef` `entity` name referenced by the schema (e.g. `"page"`),
    /// the pickable objects of that type — the options for its dropdown. An entity
    /// the resolver doesn't support maps to an absent/empty list (the control shows
    /// no options rather than erroring).
    #[serde(default)]
    pub entity_options: BTreeMap<String, Vec<EntityOption>>,
    /// Display data for each media object currently selected by a `MediaPicker`
    /// field, keyed by the stored object id (as a decimal string, since JSON object
    /// keys are strings). A selection whose media has since been deleted is simply
    /// absent (the control falls back to the neutral "choose" state).
    #[serde(default)]
    pub media: BTreeMap<String, MediaRef>,
}

/// One pickable object for an `EntityRef` dropdown: the object id the widget
/// stores, plus a human label to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityOption {
    pub id: u64,
    pub label: String,
}

/// The display data for a selected `MediaPicker` object: the stored id (echoed on
/// save) and its public `/media/{uuid}` URL (for the thumbnail). Mirrors the post
/// editor's featured-image DTO — the id is the authenticated-admin handle, the
/// unguessable uuid lives only inside `url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub id: u64,
    pub url: String,
}

impl SettingRefs {
    /// The `/media/{uuid}` URL for a selected media id, if it resolved.
    pub fn media_url(&self, id: u64) -> Option<&str> {
        self.media.get(&id.to_string()).map(|m| m.url.as_str())
    }

    /// The pickable options for an entity name (empty slice if the resolver ships
    /// none for it).
    pub fn options_for(&self, entity: &str) -> &[EntityOption] {
        self.entity_options
            .get(entity)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}
