//! The edge PORTS: `SecretStore`, `BlobStore`, `Scheduler`, `CertSource`.
//!
//! House rule (PORT memory): traits stay *engine-shaped, not host-shaped*. No
//! `project_id` parameter on `SecretStore`, no DNS-01 challenge on `CertSource`,
//! no trigger header on `Scheduler` — those are jkbase-isms that would leak a
//! specific host's shape into the abstraction. Each port has exactly ONE
//! baseline adapter in the workspace today (env, localfs, tokio-cron,
//! no-op/acme); jkbase/S3/etc. adapters are deferred until a real second target
//! exists.

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use crate::error::Result;

/// A boxed, `Send`, type-erased future — core's own alias.
///
/// We define this locally rather than importing it: `futures_core` does NOT
/// expose a `BoxFuture` (only `stream::BoxStream`), and `futures_util`'s
/// `BoxFuture` would mean pulling that whole crate into `ferropress-core`'s
/// otherwise-tiny dependency surface. The alias is two lines; keeping core lean
/// is worth more than the re-use. `ScheduledJob` below is the one public user.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// SecretStore
// ---------------------------------------------------------------------------

/// A logical reference to a secret (e.g. `"SMTP_PASSWORD"`). Opaque newtype so
/// secret names don't get confused with their values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretRef(pub String);

/// Read-only access to deployment secrets. Baseline adapter:
/// `ferropress-secrets-env` (process env + dotenv).
#[async_trait]
pub trait SecretStore: Send + Sync + 'static {
    /// Fetch a secret value, or `Unavailable` if unset.
    async fn get(&self, key: &SecretRef) -> Result<String>;

    /// Fetch a secret if present (None when simply absent — distinct from an
    /// error fetching it).
    async fn try_get(&self, key: &SecretRef) -> Result<Option<String>>;
}

// ---------------------------------------------------------------------------
// BlobStore
// ---------------------------------------------------------------------------

/// A content-addressed-or-pathed key for a blob (media original, a rendered
/// HTML page, etc.). The store decides the namespacing; callers treat it opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobKey(pub String);

/// Binary object storage for media originals AND prerendered HTML output.
/// Baseline adapter: `ferropress-blob-localfs`. Bytes never live in the DB —
/// the DB stores the `BlobKey`.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Store bytes under `key`, overwriting if present.
    async fn put(&self, key: &BlobKey, bytes: Vec<u8>) -> Result<()>;

    /// Fetch bytes; `NotFound` if absent.
    async fn get(&self, key: &BlobKey) -> Result<Vec<u8>>;

    /// Delete a blob (idempotent — deleting a missing key is Ok).
    async fn delete(&self, key: &BlobKey) -> Result<()>;

    /// Whether a blob exists (cheap existence check for the serve cache).
    async fn exists(&self, key: &BlobKey) -> Result<bool>;
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Handle to a registered schedule, so it can be cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScheduleId(pub u64);

/// Time-based job scheduling — scheduled publishes ("future" status) and
/// periodic rebuilds. Baseline adapter: `ferropress-sched-tokiocron` (an
/// in-process tokio cron loop). The callback is a plain boxed future factory;
/// no trigger metadata leaks in.
#[async_trait]
pub trait Scheduler: Send + Sync + 'static {
    /// Register a job to run at each tick of `cron_expr`. Returns its id.
    async fn schedule(&self, cron_expr: &str, job: ScheduledJob) -> Result<ScheduleId>;

    /// Run `job` once at the given UTC unix-millis instant (scheduled publish).
    async fn schedule_once(&self, at_unix_millis: i64, job: ScheduledJob) -> Result<ScheduleId>;

    /// Cancel a previously-registered schedule.
    async fn cancel(&self, id: ScheduleId) -> Result<()>;
}

/// A unit of scheduled work. Boxed so it is `dyn`-storable; returns a boxed
/// future so adapters can `await` it on their runtime.
pub type ScheduledJob =
    Box<dyn Fn() -> BoxFuture<'static, crate::error::Result<()>> + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// CertSource
// ---------------------------------------------------------------------------

/// A resolved TLS certificate + private key (PEM), as the HTTP server needs it.
#[derive(Debug, Clone)]
pub struct Certificate {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// Source of TLS certificates. Baseline adapter: `ferropress-cert-acme` — either
/// a no-op (Ferropress sits behind a TLS-terminating reverse proxy) or embedded
/// `rustls-acme` (ALPN/HTTP-01). NO DNS-01 / host-coupled challenge surface.
#[async_trait]
pub trait CertSource: Send + Sync + 'static {
    /// Resolve (issuing/renewing as needed) the certificate for `domain`.
    /// Returns `None` in no-op/proxy mode (the server then serves plain HTTP and
    /// lets the proxy terminate TLS).
    async fn certificate(&self, domain: &str) -> Result<Option<Certificate>>;
}
