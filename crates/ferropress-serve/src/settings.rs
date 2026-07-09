//! Live site-settings for the public read path.
//!
//! The theme consumes settings (title, tagline, robots, date format, timezone)
//! *live*, so the prerender cache never bakes them in — a settings change is
//! reflected on the next request with no page regeneration (the serving model's
//! "don't couple a global setting to every cached page" guardrail). To keep the
//! read path allocation-light, the current [`SiteSettings`] is held in a
//! [`SettingsHandle`] (an `Arc`-swapped snapshot): the regen loop refreshes it
//! when a `Setting` change arrives on the feed; the read path clones the current
//! `Arc`.
//!
//! [`load_site_settings`] / [`load_values`] read the settings from the store —
//! one scan of the tiny `Setting` table, the schema defaults overlaid with the
//! stored rows — reused by both the startup seed and the feed-driven refresh.

use std::sync::Arc;

use ferropress_core::error::Result;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{MEDIA_TYPE, SETTING_TYPE, is_media_token, media_url};
use ferropress_render_form::{SiteSettings, schema_for_settings};
use parking_lot::RwLock;
use serde_json::{Map, Value as JsonValue};

/// Read the settings **values map** from the store: the schema defaults overlaid
/// with any stored `Setting` rows (each `value` is a JSON-encoded String). Only
/// keys the schema declares are surfaced (the schema is the whitelist). One scan
/// of the tiny `Setting` table; a stored value that fails to parse is skipped so
/// its default stands (never an error).
///
/// This is the serve-side twin of the admin API's settings read; the admin's
/// handler and this share the same overlay rule so the form and the public page
/// agree on what a setting resolves to.
pub async fn load_values(store: &Arc<dyn RhypeStore>) -> Result<Map<String, JsonValue>> {
    // Site settings store keys verbatim, so the store key IS the schema key.
    overlay_settings(store, schema_for_settings().defaults(), |key| {
        Some(key.to_owned())
    })
    .await
}

/// THE single settings overlay rule, factored so every config surface resolves a
/// stored value the same way (the form and the served page can never disagree).
/// Overlays stored `Setting` rows onto `defaults`: for each row, `to_schema_key`
/// maps the stored key to a schema key (returning `None` to skip the row — e.g. a
/// row outside a plugin's namespace); a mapped key not present in `defaults` is
/// ignored (the schema is the whitelist); a `value` String that fails to parse is
/// skipped so its default stands (never an error). One scan of the tiny `Setting`
/// table. Site settings pass an identity mapping; plugin config strips the
/// `plugin.{id}.` prefix (see `ferropress_http`'s plugin route).
pub async fn overlay_settings<F>(
    store: &Arc<dyn RhypeStore>,
    mut defaults: Map<String, JsonValue>,
    to_schema_key: F,
) -> Result<Map<String, JsonValue>>
where
    F: Fn(&str) -> Option<String>,
{
    for obj in store.scan(&TypeName::from(SETTING_TYPE)).await? {
        let (Some(Value::String(key)), Some(Value::String(raw))) =
            (obj.get("key"), obj.get("value"))
        else {
            continue;
        };
        let Some(schema_key) = to_schema_key(key) else {
            continue;
        };
        // Only overlay keys the schema knows; unknown stored keys are ignored.
        if !defaults.contains_key(&schema_key) {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<JsonValue>(raw) {
            defaults.insert(schema_key, parsed);
        }
    }
    Ok(defaults)
}

/// Read the settings and project them into the typed [`SiteSettings`] the theme
/// consumes.
///
/// One field can't be projected by [`SiteSettings::from_values`] alone: `site.logo`
/// stores a `Media` object id, and the theme needs its `/media/{uuid}` URL. That
/// needs a store lookup, so it is resolved HERE (once per load) and stashed on
/// [`SiteSettings::logo_url`]. Because the snapshot is rebuilt on every `Setting`
/// change (the regen loop calls this), the resolved URL stays current, and the read
/// path composes chrome without a per-request media lookup. A dangling/missing id
/// simply leaves the logo unset (the masthead falls back to the text title).
pub async fn load_site_settings(store: &Arc<dyn RhypeStore>) -> Result<SiteSettings> {
    let values = load_values(store).await?;
    let mut settings = SiteSettings::from_values(&values);
    if let Some(id) = values.get("site.logo").and_then(JsonValue::as_u64) {
        settings.logo_url = media_url_by_id(store, id).await;
    }
    Ok(settings)
}

/// Resolve a `Media` object id to its public `/media/{uuid}` URL, or `None` if the
/// media is missing or lacks a uuid-shaped token. Mirrors the serve layer's
/// `featured_image_url`, but keyed by a raw id (a settings value) rather than a
/// relation.
async fn media_url_by_id(store: &Arc<dyn RhypeStore>, id: u64) -> Option<String> {
    let media = store
        .get(&TypeName::from(MEDIA_TYPE), ObjectId(id))
        .await
        .ok()?;
    match media.get("uuid") {
        Some(Value::String(uuid)) if is_media_token(uuid) => Some(media_url(uuid)),
        _ => None,
    }
}

/// A cheaply-cloneable handle to the current live [`SiteSettings`], shared
/// between the read path (which composes chrome) and the regen loop (which
/// refreshes it off the change feed).
///
/// Reads clone the inner `Arc` under a short read lock; a refresh swaps the `Arc`
/// under a short write lock. The snapshot is small and read far more often than
/// written, so a plain `RwLock<Arc<_>>` is ample (no need for a watch channel —
/// the read path only ever wants the latest value).
#[derive(Clone)]
pub struct SettingsHandle(Arc<RwLock<Arc<SiteSettings>>>);

impl SettingsHandle {
    /// Seed the handle with an initial snapshot (built at startup from the store).
    pub fn new(initial: SiteSettings) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current snapshot. Cloning the `Arc` is cheap; hold it for the duration
    /// of one render.
    pub fn current(&self) -> Arc<SiteSettings> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current snapshot (called by the regen loop on a `Setting`
    /// change).
    pub fn set(&self, next: SiteSettings) {
        *self.0.write() = Arc::new(next);
    }
}

impl Default for SettingsHandle {
    /// A handle seeded with the schema defaults — the state before the store is
    /// read (and what tests get without wiring settings).
    fn default() -> Self {
        Self::new(SiteSettings::default())
    }
}
