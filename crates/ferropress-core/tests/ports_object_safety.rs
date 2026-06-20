//! Compile-tests: the PORT traits must stay object-safe so the composition root
//! can hold each as an `Arc<dyn ...>` and inject it. If any of these traits ever
//! gains a non-object-safe method (a generic param, a `Self`-by-value receiver,
//! an `impl Trait` return, …) this test fails to compile — which is the point.
//!
//! These are type-level assertions only; nothing is executed. The functions are
//! never called, but they must type-check, so they exercise the `dyn` coercion.

#![allow(dead_code)]

use std::sync::Arc;

use ferropress_core::{
    BlobStore, BoxFuture, CertSource, RhypeStore, ScheduledJob, Scheduler, SecretStore,
};

// Each `fn` proves the named trait is object-safe behind an `Arc`.

fn store_is_object_safe(s: Arc<dyn RhypeStore>) -> Arc<dyn RhypeStore> {
    s
}

fn secret_store_is_object_safe(s: Arc<dyn SecretStore>) -> Arc<dyn SecretStore> {
    s
}

fn blob_store_is_object_safe(s: Arc<dyn BlobStore>) -> Arc<dyn BlobStore> {
    s
}

fn scheduler_is_object_safe(s: Arc<dyn Scheduler>) -> Arc<dyn Scheduler> {
    s
}

fn cert_source_is_object_safe(s: Arc<dyn CertSource>) -> Arc<dyn CertSource> {
    s
}

/// `ScheduledJob` must be constructible from a plain closure returning a boxed
/// future — the shape every `Scheduler` adapter relies on.
fn scheduled_job_is_constructible() -> ScheduledJob {
    Box::new(|| -> BoxFuture<'static, ferropress_core::Result<()>> { Box::pin(async { Ok(()) }) })
}
