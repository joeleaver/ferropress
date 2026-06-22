//! Engine-shaped value types — Ferropress's OWN mirror of the object/field model
//! a typed object store exposes, defined here so the public API never names a
//! rhypedb type. `ferropress-store-embedded` converts between these and
//! `rhypedb_engine::object::{Value, Object, FieldMap}` at the boundary.
//!
//! Design note: we mirror rhypedb's *runtime* `Value` variants — the ones that
//! actually round-trip end-to-end. As of rhypedb rev `2a9bf28` that set gained
//! native `Json(serde_json::Value)` and `DateTime(i64)` (epoch millis, UTC) on
//! top of the original ten scalars; `validate_value` now accepts both on write
//! and they read back faithfully. So this mirror carries `Value::Json` and
//! `Value::DateTime` too: JSON blobs (the block tree, plugin `meta`, `seo`) and
//! timestamps are no longer flattened to `Value::String`. (`Json` has no total
//! order, so it cannot be `@indexed`; `DateTime` has an ordered secondary index
//! and supports range pushdown — see `ferropress-schema-sdl`.)

use std::collections::HashMap;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The name of an object type as the store knows it (e.g. `"Post"`). A newtype
/// so we never pass a raw `&str` type-name where an id is expected and vice
/// versa.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TypeName(pub String);

impl TypeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TypeName {
    fn from(s: &str) -> Self {
        TypeName(s.to_owned())
    }
}

/// A store-assigned object identity. rhypedb uses `u64` object ids; we expose
/// that width but as a distinct type so an id can never be confused with a
/// count, a version, or another entity's id.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ObjectId(pub u64);

/// A dynamically-typed field value. Mirrors the rhypedb runtime `Value` set
/// (the variants that actually persist), including the native `Json`/`DateTime`
/// values rhypedb gained at rev `2a9bf28` (see module docs).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    String(String),
    U32(u32),
    U64(u64),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    /// Opaque bytes. Carried as a `Vec<u8>` in core (rhypedb uses `bytes::Bytes`;
    /// the adapter converts). Used sparingly — most binary lives in `BlobStore`.
    Bytes(Vec<u8>),
    /// Native JSON. Backs JSON-shaped fields that used to be `String`-encoded —
    /// the canonical block tree, plugin `meta`, `seo` — so they flow through the
    /// JSON API boundary without double-encoding. Cannot be `@indexed` (no total
    /// order).
    Json(serde_json::Value),
    /// A timestamp as epoch **milliseconds**, UTC (mirrors the engine's i64
    /// `DateTime`). Sorts/range-queries via the engine's ordered index; format to
    /// RFC3339 only at the API boundary with [`datetime_to_rfc3339`].
    DateTime(i64),
}

impl Value {
    /// The epoch-millis payload of a [`Value::DateTime`], else `None`.
    pub fn as_datetime(&self) -> Option<i64> {
        match self {
            Value::DateTime(ms) => Some(*ms),
            _ => None,
        }
    }

    /// The inner value of a [`Value::Json`], else `None`.
    pub fn as_json(&self) -> Option<&serde_json::Value> {
        match self {
            Value::Json(j) => Some(j),
            _ => None,
        }
    }
}

/// Format epoch-millis (UTC) as an RFC3339 string for an API/wire boundary.
/// Returns `None` if the millis are out of the representable range. This is the
/// single conversion the public JSON DTOs use so storage stays an i64 while the
/// wire contract stays RFC3339.
pub fn datetime_to_rfc3339(millis: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos((millis as i128) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

/// Parse an RFC3339 string to epoch-millis (UTC), truncating sub-millisecond
/// precision. Returns `None` if it does not parse. Inverse of
/// [`datetime_to_rfc3339`] at millisecond resolution.
pub fn rfc3339_to_millis(s: &str) -> Option<i64> {
    let dt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    i64::try_from(dt.unix_timestamp_nanos() / 1_000_000).ok()
}

/// The current UTC instant as epoch-millis — the canonical "now" for
/// [`Value::DateTime`] write sites.
pub fn now_millis() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// A bag of field values for create/update and read results. Same shape as
/// rhypedb's `FieldMap` but over core's `Value`.
pub type FieldMap = HashMap<String, Value>;

/// A materialized object read back from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub type_name: TypeName,
    pub id: ObjectId,
    pub fields: FieldMap,
}

impl Object {
    pub fn get(&self, field: &str) -> Option<&Value> {
        self.fields.get(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_rfc3339_round_trips_at_millis() {
        // 2026-06-22T12:34:56.789Z
        let ms = 1_781_181_296_789;
        let s = datetime_to_rfc3339(ms).expect("format");
        assert_eq!(rfc3339_to_millis(&s), Some(ms));
        // Epoch round-trips and renders as an RFC3339 1970 instant.
        let epoch = datetime_to_rfc3339(0).expect("format epoch");
        assert!(epoch.starts_with("1970-01-01T00:00:00"), "epoch: {epoch}");
        assert_eq!(rfc3339_to_millis(&epoch), Some(0));
    }

    #[test]
    fn rfc3339_to_millis_truncates_sub_millis_and_rejects_garbage() {
        // Sub-millisecond precision is truncated, not rounded.
        assert_eq!(rfc3339_to_millis("1970-01-01T00:00:00.000500Z"), Some(0));
        assert_eq!(rfc3339_to_millis("not a date"), None);
    }

    #[test]
    fn as_datetime_and_as_json_are_variant_gated() {
        assert_eq!(Value::DateTime(42).as_datetime(), Some(42));
        assert_eq!(Value::String("x".into()).as_datetime(), None);
        let j = serde_json::json!({"a": 1});
        assert_eq!(Value::Json(j.clone()).as_json(), Some(&j));
        assert_eq!(Value::U64(1).as_json(), None);
    }
}
