//! Reference action-hook plugin: an **audit log**.
//!
//! The change-feed → action bridge (`ferropress-serve::HookBridge`) dispatches a
//! `<type>.<created|updated|deleted>` **action** for every committed change. This
//! plugin is wired (in `plugin.toml`) to `comment.created` and logs each one — the
//! post-persist counterpart to the `comment.create` FILTER in `comment-mod`.
//!
//! It is pure-compute and capability-zero: an action that only OBSERVES. A
//! capability-zero action can log but cannot cause side effects (store writes,
//! webhooks) — those need capability host-functions, a later increment. The host
//! IGNORES an action's return value, so this returns `()`.

use extism_pdk::*;
use serde_json::Value;

/// Handle a change action: log the change identity from the bridge's payload
/// (`{ version, type, kind, object_id, fields? }`).
#[plugin_fn]
pub fn on_change(Json(change): Json<Value>) -> FnResult<()> {
    let ty = change.get("type").and_then(Value::as_str).unwrap_or("?");
    let kind = change.get("kind").and_then(Value::as_str).unwrap_or("?");
    let id = change.get("object_id").and_then(Value::as_u64).unwrap_or(0);
    let version = change.get("version").and_then(Value::as_u64).unwrap_or(0);
    info!("audit-log: {ty} #{id} {kind} (v{version})");
    Ok(())
}
