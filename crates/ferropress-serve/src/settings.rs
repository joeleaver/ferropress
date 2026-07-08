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

use ferropress_core::SETTING_TYPE;
use ferropress_core::error::Result;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
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
    let schema = schema_for_settings();
    let mut values = schema.defaults();

    for obj in store.scan(&TypeName::from(SETTING_TYPE)).await? {
        let (Some(Value::String(key)), Some(Value::String(raw))) =
            (obj.get("key"), obj.get("value"))
        else {
            continue;
        };
        // Only overlay keys the schema knows; unknown stored keys are ignored.
        if !values.contains_key(key) {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<JsonValue>(raw) {
            values.insert(key.clone(), parsed);
        }
    }
    Ok(values)
}

/// Read the settings and project them into the typed [`SiteSettings`] the theme
/// consumes.
pub async fn load_site_settings(store: &Arc<dyn RhypeStore>) -> Result<SiteSettings> {
    Ok(SiteSettings::from_values(&load_values(store).await?))
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
