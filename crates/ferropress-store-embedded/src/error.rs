//! Adapter error mapping. Every rhypedb error type is converted into
//! `ferropress_core::CoreError` here, so the port never surfaces an upstream
//! error type to callers. `AdapterError` is an internal convenience that
//! `From`-converts both ways.
//!
//! The `EngineError -> CoreError` mapping below matches the ACTUAL rhypedb
//! `EngineError` variants at the pinned rev: there is no `EngineError::NotFound`.
//! The "not found" cases are `ObjectNotFound { type_name, object_id }` and
//! `TypeNotFound(String)`; uniqueness is `UniqueViolation { type_name, field,
//! value }`; shape errors are `TypeMismatch { field, expected, got }`. Anything
//! we don't model precisely is collapsed to `CoreError::Store` carrying the
//! upstream *message* only (the upstream error *type* never crosses the
//! boundary — that is the whole point of the port).

use ferropress_core::CoreError;
use rhypedb_engine::error::EngineError;
use rhypedb_schema::SchemaError;
use thiserror::Error;

/// Internal adapter error. Crate-private in spirit; immediately mapped to
/// `CoreError` at the port boundary.
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),

    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),

    /// A value coming back from the engine had a shape core can't represent
    /// (should be impossible given our schema, but mapped rather than panicked).
    #[error("value conversion: {0}")]
    Conversion(String),
}

impl From<AdapterError> for CoreError {
    fn from(e: AdapterError) -> Self {
        match e {
            AdapterError::Engine(inner) => map_engine_error(inner),
            // Schema errors only surface at open/reconcile time; there is no
            // finer core variant for them, so carry the message.
            AdapterError::Schema(inner) => CoreError::Store(inner.to_string()),
            AdapterError::Conversion(msg) => CoreError::Store(msg),
        }
    }
}

/// Map the real `EngineError` variants onto core's vocabulary. Only the cases
/// core models precisely are lifted; the long tail (catalog/migration/storage
/// internals) collapses to `CoreError::Store(message)`.
fn map_engine_error(err: EngineError) -> CoreError {
    match err {
        // "Not found" is two distinct engine variants — both become NotFound.
        EngineError::ObjectNotFound {
            type_name,
            object_id,
        } => CoreError::NotFound {
            type_name,
            id: object_id,
        },
        EngineError::TypeNotFound(type_name) => CoreError::NotFound {
            type_name,
            // No object id for a missing *type*; 0 is the sentinel.
            id: 0,
        },
        // Engine's TypeMismatch carries `{ field, expected, got }` (no type
        // name); fold expected/got into core's `detail` and leave the type name
        // empty (the engine does not provide it on this variant).
        EngineError::TypeMismatch {
            field,
            expected,
            got,
        } => CoreError::TypeMismatch {
            type_name: String::new(),
            field,
            detail: format!("expected {expected}, got {got}"),
        },
        EngineError::UniqueViolation {
            type_name, field, ..
        } => CoreError::UniqueViolation { type_name, field },
        // Everything else: keep the message, drop the type.
        other => CoreError::Store(other.to_string()),
    }
}
