//! Bidirectional translation between rhypedb's runtime types and core's
//! engine-shaped mirror. This is the membrane that keeps rhypedb types out of
//! the rest of the system: nothing here is re-exported.
//!
//! Mappings:
//!   core::Value  <-> rhypedb_engine::object::Value   (1:1 on every shared variant,
//!                                                      including native DateTime/Json)
//!   core::Object <-  rhypedb_engine::object::Object
//!   core::Change <-  rhypedb_subscribe::ChangeEvent  (one direction; read-only feed,
//!                                                      fields forwarded as JSON)
//!   core::Compare -> rhypedb_engine::CompareOp
//!   core::VectorQuery.restrict -> HashSet<u64> for `Vectorizer::search_text`

use std::collections::HashSet;

use bytes::Bytes;

use ferropress_core::query::{Change, ChangeKind, Compare, SubscribeFilter};
use ferropress_core::value::{
    FieldMap as CoreFieldMap, Object as CoreObject, ObjectId, TypeName, Value as CoreValue,
};

use rhypedb_engine::CompareOp;
use rhypedb_engine::object::{FieldMap as DbFieldMap, Object as DbObject, Value as DbValue};
use rhypedb_subscribe::{ChangeEvent, ChangeKind as DbChangeKind, SubscriptionFilter};

/// core::Value -> rhypedb Value. Total: every core variant has a 1:1 rhypedb
/// counterpart (core mirrors exactly rhypedb's live runtime variants).
pub fn to_db_value(v: CoreValue) -> DbValue {
    match v {
        CoreValue::Null => DbValue::Null,
        CoreValue::String(s) => DbValue::String(s),
        CoreValue::U32(n) => DbValue::U32(n),
        CoreValue::U64(n) => DbValue::U64(n),
        CoreValue::I32(n) => DbValue::I32(n),
        CoreValue::I64(n) => DbValue::I64(n),
        CoreValue::F32(n) => DbValue::F32(n),
        CoreValue::F64(n) => DbValue::F64(n),
        CoreValue::Bool(b) => DbValue::Bool(b),
        CoreValue::Bytes(b) => DbValue::Bytes(Bytes::from(b)),
        CoreValue::Json(j) => DbValue::Json(j),
        CoreValue::DateTime(ms) => DbValue::DateTime(ms),
    }
}

/// rhypedb Value -> core::Value. Total + 1:1: core mirrors every rhypedb runtime
/// variant, including the native `DateTime`/`Json` rhypedb gained at rev `2a9bf28`
/// (no longer folded to `String`).
pub fn from_db_value(v: DbValue) -> CoreValue {
    match v {
        DbValue::Null => CoreValue::Null,
        DbValue::String(s) => CoreValue::String(s),
        DbValue::U32(n) => CoreValue::U32(n),
        DbValue::U64(n) => CoreValue::U64(n),
        DbValue::I32(n) => CoreValue::I32(n),
        DbValue::I64(n) => CoreValue::I64(n),
        DbValue::F32(n) => CoreValue::F32(n),
        DbValue::F64(n) => CoreValue::F64(n),
        DbValue::Bool(b) => CoreValue::Bool(b),
        DbValue::Bytes(b) => CoreValue::Bytes(b.to_vec()),
        DbValue::DateTime(ms) => CoreValue::DateTime(ms),
        DbValue::Json(j) => CoreValue::Json(j),
    }
}

pub fn to_db_fields(fields: CoreFieldMap) -> DbFieldMap {
    fields
        .into_iter()
        .map(|(k, v)| (k, to_db_value(v)))
        .collect()
}

pub fn from_db_fields(fields: DbFieldMap) -> CoreFieldMap {
    fields
        .into_iter()
        .map(|(k, v)| (k, from_db_value(v)))
        .collect()
}

/// Materialize a core Object from a rhypedb Object. rhypedb objects read via the
/// lazy `raw_fields` fast path have an empty `fields` map until decoded, so we
/// call `ensure_fields_deserialized` BEFORE reading them — otherwise we would
/// silently produce an object with no fields.
pub fn from_db_object(mut obj: DbObject) -> CoreObject {
    obj.ensure_fields_deserialized();
    CoreObject {
        type_name: TypeName(obj.type_name),
        id: ObjectId(obj.id),
        fields: from_db_fields(obj.fields),
    }
}

pub fn to_compare_op(op: Compare) -> CompareOp {
    match op {
        Compare::Eq => CompareOp::Eq,
        Compare::Ne => CompareOp::Ne,
        Compare::Lt => CompareOp::Lt,
        Compare::Le => CompareOp::Le,
        Compare::Gt => CompareOp::Gt,
        Compare::Ge => CompareOp::Ge,
    }
}

/// Build the `restrict` id set for a vector search. `Vectorizer::search_text`
/// takes `Option<&HashSet<u64>>`, so a caller materializes the owned set with
/// this helper and then passes `set.as_ref()` (i.e. `Option<&HashSet<u64>>`):
///
/// ```ignore
/// let restrict = convert::to_restrict_set(query.restrict);
/// vectorizer.search_text(ty, field, &text, k, ef, rerank, restrict.as_ref())?;
/// ```
pub fn to_restrict_set(restrict: Option<Vec<ObjectId>>) -> Option<HashSet<u64>> {
    restrict.map(|ids| ids.into_iter().map(|ObjectId(n)| n).collect())
}

/// core::SubscribeFilter -> rhypedb `SubscriptionFilter`. The two structs are
/// field-for-field equivalent (type/object are optional narrowings, `kinds` empty
/// means "all kinds"); this just unwraps the core newtypes and maps the kind enum.
pub fn to_subscription_filter(filter: SubscribeFilter) -> SubscriptionFilter {
    SubscriptionFilter {
        type_name: filter.type_name.map(|TypeName(s)| s),
        object_id: filter.object_id.map(|ObjectId(n)| n),
        kinds: filter.kinds.into_iter().map(to_db_change_kind).collect(),
    }
}

/// core::ChangeKind -> rhypedb `ChangeKind`. Used when narrowing a subscription.
fn to_db_change_kind(kind: ChangeKind) -> DbChangeKind {
    match kind {
        ChangeKind::Create => DbChangeKind::Create,
        ChangeKind::Update => DbChangeKind::Update,
        ChangeKind::Delete => DbChangeKind::Delete,
    }
}

/// rhypedb ChangeEvent -> core::Change. One-directional (the feed is read-only).
///
/// The engine's `ChangeEvent.fields` is already the scalar fields as JSON
/// (`Option<HashMap<String, serde_json::Value>>`, the same `value_to_query_json`
/// projection the query boundary uses). We forward it VERBATIM as a JSON object —
/// no lossy json->core::Value coercion (the engine's `Bytes`->base64 /
/// `DateTime`->RFC3339 conventions don't round-trip through core `Value`). The
/// serve regen loop reads the slug straight off this (no re-`get`), and on a
/// **delete** it is the only way to learn which page to evict.
pub fn from_change_event(ev: ChangeEvent) -> Change {
    let kind = match ev.kind {
        DbChangeKind::Create => ChangeKind::Create,
        DbChangeKind::Update => ChangeKind::Update,
        DbChangeKind::Delete => ChangeKind::Delete,
    };
    Change {
        version: ev.version,
        kind,
        type_name: TypeName(ev.type_name),
        object_id: ObjectId(ev.object_id),
        // Forward the scalar fields as a JSON object (or `None` if the event
        // carried none — e.g. a delete with no captured fields).
        fields: ev
            .fields
            .map(|m| serde_json::Value::Object(m.into_iter().collect())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_and_json_round_trip_1_to_1() {
        // DateTime <-> DateTime, preserving the i64 epoch-millis payload.
        let ms = 1_781_181_296_789_i64;
        assert!(matches!(to_db_value(CoreValue::DateTime(ms)), DbValue::DateTime(m) if m == ms));
        assert_eq!(
            from_db_value(DbValue::DateTime(ms)),
            CoreValue::DateTime(ms)
        );

        // Json <-> Json, preserving structure (no String folding).
        let j = serde_json::json!({ "a": 1, "b": ["x", true] });
        assert!(matches!(to_db_value(CoreValue::Json(j.clone())), DbValue::Json(v) if v == j));
        assert_eq!(from_db_value(DbValue::Json(j.clone())), CoreValue::Json(j));
    }

    #[test]
    fn from_change_event_forwards_fields_as_json_object() {
        use std::collections::HashMap;
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        fields.insert("slug".to_owned(), serde_json::json!("hello"));
        let ev = ChangeEvent {
            version: 1,
            kind: DbChangeKind::Delete,
            type_name: "Post".to_owned(),
            object_id: 7,
            fields: Some(fields),
        };
        let change = from_change_event(ev);
        // Delete now carries the (pre-delete) scalar fields, forwarded as JSON.
        let fields = change.fields.expect("fields forwarded");
        assert_eq!(fields["slug"], "hello");
    }
}
