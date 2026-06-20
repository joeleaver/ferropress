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
