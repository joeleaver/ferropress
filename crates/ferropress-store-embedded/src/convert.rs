//! Bidirectional translation between rhypedb's runtime types and core's
//! engine-shaped mirror. This is the membrane that keeps rhypedb types out of
//! the rest of the system: nothing here is re-exported.
//!
//! Mappings:
//!   core::Value  <-> rhypedb_engine::object::Value   (1:1 on the 10 shared variants;
//!                                                      read-only folds DateTime/Json -> String)
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
/// counterpart (core deliberately mirrors only the live rhypedb variants).
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
    }
}

/// rhypedb Value -> core::Value. Total: the 10 core-mirrored variants map 1:1; the
/// two rhypedb-native variants core does NOT model (`DateTime`, `Json`) fold to
/// `String` (see below).
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
        // rhypedb gained native DateTime/Json runtime values (formerly write-dead).
        // Ferropress's core model deliberately carries NEITHER — timestamps and JSON
        // travel as String (see `ferropress_core::value` module docs) — and
        // Ferropress never WRITES them, so this is a defensive read path: render via
        // the engine's canonical query-boundary projection (DateTime -> RFC3339 from
        // epoch-millis, Json -> compact text) so a field written by some other tool
        // still reads back as a String rather than panicking.
        DbValue::DateTime(ms) => CoreValue::String(rhypedb_engine::object::rfc3339_from_millis(ms)),
        DbValue::Json(j) => CoreValue::String(j.to_string()),
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
    fn from_db_value_folds_native_datetime_and_json_to_string() {
        // rhypedb-native DateTime (epoch millis) -> RFC3339 String.
        match from_db_value(DbValue::DateTime(0)) {
            CoreValue::String(s) => {
                assert!(s.starts_with("1970-01-01"), "RFC3339 of epoch 0: {s}")
            }
            other => panic!("DateTime must fold to String, got {other:?}"),
        }
        // rhypedb-native Json -> compact JSON text.
        match from_db_value(DbValue::Json(serde_json::json!({ "a": 1 }))) {
            CoreValue::String(s) => assert_eq!(s, "{\"a\":1}"),
            other => panic!("Json must fold to String, got {other:?}"),
        }
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
