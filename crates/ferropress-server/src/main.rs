//! Ferropress server entrypoint — THE composition root.
//!
//! This is the ONLY place concrete adapters are selected and injected into the
//! ports (invariant #6). It boots, in one owned process:
//!   * the embedded rhypedb store (`EmbeddedStore` -> `RhypeStore`),
//!   * the local-FS blob store (`LocalFsBlobStore` -> `BlobStore`),
//!   * the env/dotenv secret store (`EnvSecretStore` -> `SecretStore`),
//!   * the tokio-cron scheduler (`TokioCronScheduler` -> `Scheduler`),
//!   * the cert source (`AcmeCertSource` -> `CertSource`),
//!   * the static-serve regen loop (`ServeEngine`),
//!   * the plugin host (`PluginHost`),
//!   * the owned HTTP server (`ferropress_http::serve`).
//!
//! HTTP / serve / render / plugin host / DB engine are OWNED in process — not
//! behind a port. Only the five data/edge ports above are injected.

mod config;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use ferropress_core::ports::{BlobStore, CertSource, Scheduler, SecretStore};
use ferropress_core::store::RhypeStore;

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_cert_acme::{AcmeCertSource, AcmeConfig};
use ferropress_http::AppState;
use ferropress_plugin_host::PluginHost;
use ferropress_sched_tokiocron::TokioCronScheduler;
use ferropress_secrets_env::EnvSecretStore;
use ferropress_serve::{ServeEngine, default_theme};
use ferropress_store_embedded::EmbeddedStore;

use crate::config::{ServerConfig, TlsMode};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = ServerConfig::parse();

    // ----------------------------------------------------------------------
    // 1. SELECT adapters and inject them into the ports. This is the ONLY place
    //    concrete adapter types are named; everything downstream sees only the
    //    `Arc<dyn Port>` trait objects. The block below is the load-bearing seam
    //    — it must typecheck. (Several adapter bodies are still `todo!()`, so the
    //    *boot* is deferred at the bottom rather than panicking mid-wiring.)
    // ----------------------------------------------------------------------
    let secrets = select_secret_store(&cfg)?;
    let blobs = select_blob_store(&cfg);
    let scheduler = select_scheduler();
    let certs = select_cert_source(&cfg);
    let store = select_store(&cfg)?;

    // 2. Build the owned subsystems over the ports.
    let serve = ServeEngine::new(Arc::clone(&store), Arc::clone(&blobs));
    let plugins = PluginHost::new();
    // Build the page-chrome theme once (its template registered) and share it.
    let theme = Arc::new(default_theme().context("building the page-chrome theme")?);
    let app_state = AppState::new(Arc::clone(&store), Arc::clone(&blobs), theme);

    // Wired but not yet driven in v1: the scheduler, secret store, cert source,
    // the prerender/regen `ServeEngine`, and the plugin host all come online in
    // later increments. Named so the composition seam is real and they stay
    // constructed (the regen loop is intentionally NOT spawned — its body is a
    // stub today and would panic).
    let _ = (&secrets, &scheduler, &certs, &serve, &plugins);

    // 3. Boot the owned HTTP server (v1 SSR-on-demand: every page renders on
    //    request; the static-first prerender cache + change-driven regen is a
    //    later increment).
    ferropress_http::serve(app_state, cfg.bind)
        .await
        .context("running the HTTP server")?;
    Ok(())
}

/// Select the `SecretStore` adapter: dotenv-seeded env when an env file is
/// configured, otherwise plain process env.
fn select_secret_store(cfg: &ServerConfig) -> Result<Arc<dyn SecretStore>> {
    let store = match &cfg.env_file {
        Some(path) => EnvSecretStore::load_from(path)
            .with_context(|| format!("loading env file {}", path.display()))?,
        None => EnvSecretStore::from_env(),
    };
    Ok(Arc::new(store))
}

/// Select the `BlobStore` adapter (local filesystem, the portable default).
fn select_blob_store(cfg: &ServerConfig) -> Arc<dyn BlobStore> {
    Arc::new(LocalFsBlobStore::new(cfg.blob_dir.clone()))
}

/// Select the `Scheduler` adapter (in-process tokio cron).
fn select_scheduler() -> Arc<dyn Scheduler> {
    Arc::new(TokioCronScheduler::new())
}

/// Select the `CertSource` adapter from the configured TLS mode.
fn select_cert_source(cfg: &ServerConfig) -> Arc<dyn CertSource> {
    let source = match cfg.tls {
        TlsMode::Proxy => AcmeCertSource::proxy(),
        TlsMode::Acme => AcmeCertSource::acme(AcmeConfig {
            domains: cfg.acme_domain.clone(),
            contact: cfg.acme_contact.clone(),
            cache_dir: cfg.acme_cache.clone(),
            directory_url: None,
        }),
    };
    Arc::new(source)
}

/// Select the `RhypeStore` adapter (the embedded rhypedb engine — the only one
/// today). Opening runs the additive schema reconcile.
fn select_store(cfg: &ServerConfig) -> Result<Arc<dyn RhypeStore>> {
    let store = EmbeddedStore::open(&cfg.data_dir)
        .with_context(|| format!("opening embedded store at {}", cfg.data_dir.display()))?;
    Ok(Arc::new(store))
}
