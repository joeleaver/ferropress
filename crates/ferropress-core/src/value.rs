//! Engine-shaped value types — Ferropress's OWN mirror of the object/field model
//! a typed object store exposes, defined here so the public API never names a
//! rhypedb type. `ferropress-store-embedded` converts between these and
//! `rhypedb_engine::object::{Value, Object, FieldMap}` at the boundary.
//!
//! Design note: we intentionally mirror rhypedb's *runtime* `Value` variants
//! (the 10 that actually round-trip end-to-end) rather than its declared
//! `ScalarType` set. rhypedb's `Json` and `DateTime` scalar types are
//! parse-only / write-dead (verified against `validate_value`), so there is no
//! `Value::Json`/`Value::DateTime` here either: timestamps and JSON travel as
//! `Value::String`. This keeps the core honest about what the backend can store.

use std::collections::HashMap;

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
/// (the variants that actually persist). No `Json`/`DateTime` variant by design
/// (see module docs): those are encoded as `String`.
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
}

impl Value {
    /// Convert to a **natural** `serde_json::Value` (bare scalars) — as opposed to
    /// the externally-tagged form the `Serialize` derive emits (`Value::U64(1)` ->
    /// `{"U64": 1}`). Use this whenever a `Value` / [`FieldMap`] must be presented
    /// to a consumer as ordinary JSON: e.g. the change-feed action-hook payload, so
    /// a plugin sees `"n": 1`, not `"n": {"U64": 1}`.
    ///
    /// Non-finite floats (`NaN` / ±∞, which JSON cannot represent) become `null`;
    /// opaque [`Value::Bytes`] become a JSON array of byte values (lossless, and
    /// rare — most binary lives in the blob store, see the enum docs).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::String(s) => J::String(s.clone()),
            Value::U32(n) => J::from(*n),
            Value::U64(n) => J::from(*n),
            Value::I32(n) => J::from(*n),
            Value::I64(n) => J::from(*n),
            Value::F32(x) => f64_to_json(*x as f64),
            Value::F64(x) => f64_to_json(*x),
            Value::Bool(b) => J::Bool(*b),
            Value::Bytes(b) => J::Array(b.iter().map(|byte| J::from(*byte)).collect()),
        }
    }
}

/// A finite `f64` becomes a JSON number; `NaN`/±∞ (unrepresentable in JSON) become
/// `null` rather than producing invalid JSON.
fn f64_to_json(x: f64) -> serde_json::Value {
    serde_json::Number::from_f64(x)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
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
    fn to_json_emits_bare_scalars_not_tagged_variants() {
        // The whole point: NOT the `{"U64": 1}` shape the derive would produce.
        assert_eq!(Value::U64(42).to_json(), serde_json::json!(42));
        assert_eq!(Value::I32(-7).to_json(), serde_json::json!(-7));
        assert_eq!(
            Value::String("hi".to_owned()).to_json(),
            serde_json::json!("hi")
        );
        assert_eq!(Value::Bool(true).to_json(), serde_json::json!(true));
        assert_eq!(Value::Null.to_json(), serde_json::Value::Null);
        assert_eq!(Value::F64(1.5).to_json(), serde_json::json!(1.5));
        // Bytes -> array of byte values (lossless).
        assert_eq!(
            Value::Bytes(vec![1, 2, 255]).to_json(),
            serde_json::json!([1, 2, 255])
        );
    }

    #[test]
    fn to_json_maps_non_finite_floats_to_null() {
        // JSON cannot represent NaN / ±∞ — fail safe to null, never invalid JSON.
        assert_eq!(Value::F64(f64::NAN).to_json(), serde_json::Value::Null);
        assert_eq!(Value::F64(f64::INFINITY).to_json(), serde_json::Value::Null);
        assert_eq!(Value::F32(f32::NAN).to_json(), serde_json::Value::Null);
    }
}
