//! # ferropress-core
//!
//! The rhypedb-agnostic heart of Ferropress. Everything here is pure data and
//! trait definitions — **no I/O, no database engine, no GUI**. This crate is the
//! one place that must stay portable: it names no rhypedb type, no rinch type,
//! and no jkbase type (CI enforces this). The embedded storage adapter
//! (`ferropress-store-embedded`) is the *only* crate that knows rhypedb exists,
//! and it translates to/from the engine-shaped value types defined here.
//!
//! Key surfaces:
//! - **Domain entities** (`entity::*`): `Post`, `Page`, `Media`, `Taxonomy`,
//!   `Term`, `User`, `Comment`, `Menu`, `MenuItem`, `Setting`, `Revision`,
//!   `Redirect` — the typed WordPress content model.
//! - **Value objects**: `BlockTree` (typed JSON block tree, a native `Value::Json`),
//!   `Status` / `CommentStatus` state machines, `Role` / `Capability`,
//!   `Slug`, `SchemaVersion`, `Seo`.
//! - **The `RhypeStore` port** (`store`): an async trait mirroring rhypedb's
//!   engine verbs in *core's own* value types. This is the seam that keeps
//!   rhypedb out of the public API.
//! - **Edge ports** (`ports`): `SecretStore`, `BlobStore`, `Scheduler`,
//!   `CertSource` — each with exactly one baseline adapter in the workspace.

pub mod block;
pub mod entity;
pub mod error;
pub mod hook;
pub mod ids;
pub mod plugin_caps;
pub mod ports;
pub mod query;
pub mod role;
pub mod seo;
pub mod status;
pub mod store;
pub mod value;

pub use block::{BLOCK_SCHEMA_VERSION, Block, BlockKind, BlockTree, InlineRun};
pub use entity::*;
pub use error::{CoreError, Result};
pub use hook::{HookDispatcher, HookEvent, HookKind, NoHooks};
pub use ids::{SchemaVersion, Slug};
pub use plugin_caps::{ContentReader, PublishedRef};
pub use ports::{
    BlobKey, BlobStore, BoxFuture, CertSource, Certificate, ScheduleId, ScheduledJob, Scheduler,
    SecretRef, SecretStore,
};
pub use query::{
    Change, ChangeKind, Compare, Edge, FilterSpec, ScoredId, SubscribeFilter, VectorQuery,
};
pub use role::{Capability, Role};
pub use seo::{Robots, Seo};
pub use status::{CommentStatus, Status};
pub use store::RhypeStore;
pub use value::{FieldMap, Object, ObjectId, TypeName, Value};
