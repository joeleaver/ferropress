//! # ferropress-store-embedded
//!
//! The embedded rhypedb adapter. This is the ONE crate permitted to know rhypedb
//! exists; everything above it speaks the `ferropress-core::RhypeStore` port.
//!
//! Responsibilities:
//!   * Open the embedded engine (`Database::open_with_options`) on the canonical
//!     schema from `ferropress-schema-sdl`.
//!   * Build the `Vectorizer` (every entity has a `search` Vector field) and
//!     start the embed worker (every entity has `@vectorize`).
//!   * Implement `RhypeStore` by wrapping the engine's *synchronous* verbs in
//!     `tokio::task::spawn_blocking`, and bridging the synchronous
//!     `mpsc::Receiver<ChangeEvent>` change feed into an async `Stream` via a
//!     dedicated forwarder thread.
//!
//! Translation between rhypedb's value types and core's lives in `convert`.

// Private: these modules name rhypedb types, so they must NOT be reachable
// through this crate's public API — that is the whole point of the membrane.
// The only public escape hatch is `AdapterError`, re-exported below.
mod content_reader;
mod content_writer;
mod convert;
mod error;
mod store_impl;

use std::path::Path;
use std::sync::Arc;

use rhypedb_engine::database::{Database, OpenOptions};
use rhypedb_engine::vectorizer::Vectorizer;

use ferropress_core::Result as CoreResult;

pub use error::AdapterError;

/// The embedded store handle. Holds the engine `Arc<Database>` and the
/// `Arc<Vectorizer>` (the vectorizer is a SEPARATE object in rhypedb, not owned
/// by `Database`; both wrap the same underlying `Arc<LsmTree>`).
pub struct EmbeddedStore {
    db: Arc<Database>,
    vectorizer: Arc<Vectorizer>,
}

impl EmbeddedStore {
    /// Open (or create) the Ferropress database under `data_dir`.
    ///
    /// Mirrors the canonical server wiring in rhypedb's own `rhypedb-server`:
    /// open with `sync_on_commit` on, then build the vectorizer from the DB's
    /// catalog id maps and start the embed worker.
    pub fn open(data_dir: impl AsRef<Path>) -> CoreResult<Self> {
        // 1. Parse + validate the canonical schema (parse error -> Core error).
        let schema = ferropress_schema_sdl::parsed_schema().map_err(AdapterError::from)?;

        // 2. Open the embedded engine. Durable commits by default.
        //    `open_with_options` already returns `EngineResult<Arc<Database>>`,
        //    so we assign the `Arc` directly — NOT `Arc::new(...)`. The `schema`
        //    is consumed by value here, so we hand the open a `.clone()` and keep
        //    the original to construct the vectorizer below.
        let db = Database::open_with_options(
            schema.clone(),
            data_dir,
            OpenOptions {
                sync_on_commit: true,
                ..Default::default()
            },
        )
        .map_err(AdapterError::from)?;

        // 3. Build the vectorizer (schema has Vector fields everywhere) and start
        //    the embed worker (schema has @vectorize everywhere). Construct from
        //    the DB's catalog maps, sharing its storage Arc. `Vectorizer::new`
        //    consumes `schema` by value — this is the original (the open got the
        //    clone).
        let vectorizer = Arc::new(
            Vectorizer::new(
                Arc::clone(db.storage()),
                schema,
                db.type_ids().clone(),
                db.field_ids().clone(),
            )
            .map_err(AdapterError::from)?,
        );
        vectorizer.start_worker(1);

        Ok(Self { db, vectorizer })
    }

    /// Borrow the raw engine handle. `pub(crate)` so `store_impl`/`convert` can
    /// reach it without exposing rhypedb types beyond this crate.
    pub(crate) fn db(&self) -> &Arc<Database> {
        &self.db
    }

    pub(crate) fn vectorizer(&self) -> &Arc<Vectorizer> {
        &self.vectorizer
    }
}
