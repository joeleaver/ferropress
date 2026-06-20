//! Crate error type. House style: one `thiserror` enum per library crate plus a
//! `Result<T>` alias. Adapters convert their foreign errors (rhypedb's
//! `EngineError`, an FS `io::Error`, …) INTO `CoreError` so the rest of the
//! system speaks one error vocabulary and never sees an upstream error type.

use thiserror::Error;

/// The single error type returned across `ferropress-core`'s public API,
/// including from every PORT trait method. Adapters map their backend errors
/// into these variants — that mapping is what keeps rhypedb/jkbase/etc. error
/// types from leaking through the ports.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The requested object does not exist.
    #[error("not found: {type_name} #{id}")]
    NotFound { type_name: String, id: u64 },

    /// A field held the wrong value shape for its declared type.
    #[error("type mismatch on {type_name}.{field}: {detail}")]
    TypeMismatch {
        type_name: String,
        field: String,
        detail: String,
    },

    /// A uniqueness constraint (e.g. slug, email) was violated.
    #[error("unique constraint violated on {type_name}.{field}")]
    UniqueViolation { type_name: String, field: String },

    /// An illegal lifecycle transition was attempted (state machine guard).
    #[error("illegal status transition: {from} -> {to}")]
    IllegalTransition { from: String, to: String },

    /// A value object failed validation (bad slug, bad block-tree JSON, …).
    #[error("validation error: {0}")]
    Validation(String),

    /// The block-tree JSON could not be parsed/serialized.
    #[error("block tree (de)serialization: {0}")]
    BlockTree(#[from] serde_json::Error),

    /// The storage backend failed in a way that does not map to a more specific
    /// variant. Carries a backend-supplied message (already stripped of the
    /// upstream error *type*).
    #[error("store backend error: {0}")]
    Store(String),

    /// A capability/permission check denied the operation.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A port's backend is unavailable / misconfigured.
    #[error("port unavailable: {0}")]
    Unavailable(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, CoreError>;
