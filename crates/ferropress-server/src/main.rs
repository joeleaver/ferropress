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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use uuid::Uuid;

use ferropress_core::ports::{BlobStore, CertSource, Scheduler, SecretStore};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value};
use ferropress_core::{Block, BlockKind, BlockTree, InlineRun, POST_TYPE, Status};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_cert_acme::{AcmeCertSource, AcmeConfig};
use ferropress_http::AppState;
use ferropress_plugin_host::PluginHost;
use ferropress_sched_tokiocron::TokioCronScheduler;
use ferropress_secrets_env::EnvSecretStore;
use ferropress_serve::{ServeEngine, default_theme};
use ferropress_store_embedded::EmbeddedStore;

use crate::config::{Cli, Command, PostArgs, ServerConfig, TlsMode};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::Serve(cfg) => run_server(cfg).await,
        Command::Post(args) => create_post_cmd(args).await,
    }
}

/// Run the owned HTTP server (the `serve` subcommand): SELECT the concrete
/// adapters, wire them into the ports, spawn the static-first regen loop, serve.
async fn run_server(cfg: ServerConfig) -> Result<()> {
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
    // Build the page-chrome theme once (its template registered) and share it
    // across BOTH the HTTP read path (`AppState`) and the regen loop
    // (`ServeEngine`) so a prerendered page is byte-for-byte what an on-demand
    // render would produce.
    let theme = Arc::new(default_theme().context("building the page-chrome theme")?);
    let serve = ServeEngine::new(Arc::clone(&store), Arc::clone(&blobs), Arc::clone(&theme));
    let plugins = PluginHost::new();
    let app_state = AppState::new(Arc::clone(&store), Arc::clone(&blobs), theme);

    // Wired but not yet driven in v1: the scheduler, secret store, cert source,
    // and the plugin host all come online in later increments. Named so the
    // composition seam is real and they stay constructed. (The regen loop IS now
    // driven — spawned below — so `serve` is no longer in this discard tuple.)
    let _ = (&secrets, &scheduler, &certs, &plugins);

    // 3. Spawn the static-first regeneration loop as a background task BEFORE the
    //    HTTP server boots. It subscribes to the change feed and write-throughs /
    //    evicts the prerender cache as content changes; the HTTP read path
    //    (`serve_path`) serves from that cache and renders-on-miss. `regen_loop`
    //    borrows `&self`, so move `serve` into the task and own it there.
    let serve = Arc::new(serve);
    let regen = Arc::clone(&serve);
    tokio::spawn(async move {
        if let Err(e) = regen.regen_loop().await {
            tracing::error!(error = %e, "regen loop exited");
        }
    });

    // 4. Boot the owned HTTP server. The static-first prerender cache is now live
    //    (cache-first read path + change-driven regen loop above).
    ferropress_http::serve(app_state, cfg.bind)
        .await
        .context("running the HTTP server")?;
    Ok(())
}

/// The `post` subcommand: create a published post in the embedded store, then
/// exit. Opens the SAME data dir the server reads, so a later `serve` (or a
/// running server's render-on-demand) picks the post up at `/<slug>`.
async fn create_post_cmd(args: PostArgs) -> Result<()> {
    let store: Arc<dyn RhypeStore> = Arc::new(
        EmbeddedStore::open(&args.data_dir)
            .with_context(|| format!("opening embedded store at {}", args.data_dir.display()))?,
    );
    let id = create_post(&store, &args.slug, &args.title, &args.body).await?;
    println!("created post id={} at /{}", id.0, args.slug);
    Ok(())
}

/// Build a one-paragraph published post and insert it through the store port.
async fn create_post(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    title: &str,
    body: &str,
) -> Result<ObjectId> {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: Uuid::now_v7().to_string(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: body.to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }]);
    let block_tree = tree
        .to_json_string()
        .context("serializing the block tree")?;

    let mut fields: FieldMap = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String(title.to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert("block_tree".to_owned(), Value::String(block_tree));

    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .context("creating the post")
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
