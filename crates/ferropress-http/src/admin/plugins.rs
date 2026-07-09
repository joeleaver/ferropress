//! Plugin configuration: list installed plugins and read/write a plugin's settings
//! through the SAME declarative [`FormSchema`] machinery as the site-settings page.
//! Gated on [`Capability::ManagePlugins`] (Administrator only).
//!
//! The plugin ships the schema (a `[settings]` table in its `plugin.toml`, surfaced
//! by the [`PluginCatalog`] port); the admin renders it with the very same
//! `FormSchemaRenderer` and this validates a submission against it. So an untrusted
//! `PUT` can only ever write keys the plugin's schema declares, coerced to each
//! widget's type — and only under the plugin's OWN `Setting`-key namespace.
//!
//! ## Namespacing — the security boundary
//! A plugin's config uses BARE field keys (e.g. `default_variant`). This handler
//! owns the persisted key: `plugin.{id}.{bare}` (via
//! [`plugin_setting_key`](ferropress_core::entity::plugin_setting_key)). The plugin
//! never supplies the prefix, so a plugin's settings can neither collide with core
//! (`site.*`) nor with another plugin's — mirroring the `content:write` `set_meta`
//! namespacing. The plugin id in the URL is validated against the catalog before any
//! write (an unknown id 404s), so a write only ever lands under a real plugin's
//! namespace.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::Value as JsonValue;

use ferropress_core::entity::{plugin_setting_key, plugin_setting_prefix};
use ferropress_core::role::Capability;
use ferropress_render_form::{FormSchema, PluginDescriptor};

use super::setting_store::upsert_setting;
use super::settings::{PutSettingsRequest, SettingsDto};
use super::{AdminError, AdminJson, AuthedUser};
use crate::AppState;

/// `GET /admin/api/plugins` — the installed plugins (id, name, whether configurable),
/// sorted by name for a stable admin list.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<PluginDescriptor>>, AdminError> {
    who.require(Capability::ManagePlugins)?;
    let mut plugins = state.plugins.plugins();
    plugins.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(Json(plugins))
}

/// `GET /admin/api/plugins/{id}/settings` — the plugin's config schema + current
/// values (defaults overlaid with its stored `plugin.{id}.*` settings). 404 if the
/// plugin is unknown or ships no settings form.
pub async fn get(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<SettingsDto>, AdminError> {
    who.require(Capability::ManagePlugins)?;
    let schema = state
        .plugins
        .settings_schema(&id)
        .ok_or(AdminError::NotFound)?;
    let values = read_plugin_values(&state, &id, &schema).await?;
    Ok(Json(SettingsDto { schema, values }))
}

/// `PUT /admin/api/plugins/{id}/settings` — validate a submission against the
/// plugin's schema and persist each changed key under the plugin's namespace.
/// Returns the fresh schema + values so the client re-syncs.
pub async fn put(
    State(state): State<AppState>,
    who: AuthedUser,
    Path(id): Path<String>,
    AdminJson(body): AdminJson<PutSettingsRequest>,
) -> Result<Json<SettingsDto>, AdminError> {
    // Authorize, then resolve the schema (unknown id → 404) BEFORE validating or
    // writing anything — a write can only ever target a real plugin's namespace.
    who.require(Capability::ManagePlugins)?;
    let schema = state
        .plugins
        .settings_schema(&id)
        .ok_or(AdminError::NotFound)?;

    // Validate + normalize against the plugin's schema. Unknown keys dropped (the
    // schema is the whitelist); a type/URL/vocabulary error is a 400 (never persisted).
    let clean = schema.coerce_values(&body.values).map_err(|errs| {
        let detail = errs
            .iter()
            .map(|e| format!("{}: {}", e.key, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        AdminError::BadRequest(format!("invalid settings — {detail}"))
    })?;

    // Persist each BARE key under the host-owned `plugin.{id}.` namespace.
    for (bare, value) in &clean {
        upsert_setting(&state, &plugin_setting_key(&id, bare), value).await?;
    }

    let values = read_plugin_values(&state, &id, &schema).await?;
    Ok(Json(SettingsDto { schema, values }))
}

/// The current value for every schema key of plugin `id`: the schema defaults (BARE
/// keys) overlaid with any stored `plugin.{id}.*` `Setting` row (prefix stripped back
/// to the bare key). Delegates to the shared [`ferropress_serve::overlay_settings`]
/// loader — the SAME single overlay rule the site-settings read uses — passing a
/// prefix-strip mapping, so the form and the plugin's own `fp_get_setting` reads
/// resolve a setting identically and the two paths can never drift.
async fn read_plugin_values(
    state: &AppState,
    id: &str,
    schema: &FormSchema,
) -> Result<serde_json::Map<String, JsonValue>, AdminError> {
    let prefix = plugin_setting_prefix(id);
    Ok(
        ferropress_serve::overlay_settings(&state.store, schema.defaults(), |key| {
            key.strip_prefix(&prefix).map(str::to_owned)
        })
        .await?,
    )
}
