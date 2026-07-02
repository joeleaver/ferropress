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

use ferropress_auth::{SigningKey, hash_password};
use ferropress_core::ports::{BlobStore, CertSource, Scheduler, SecretRef, SecretStore};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value, now_millis};
use ferropress_core::{
    Block, BlockKind, BlockTree, ContentReader, ContentWriter, InlineRun, POST_TYPE, Status,
    USER_TYPE,
};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_cert_acme::{AcmeCertSource, AcmeConfig};
use ferropress_http::{AdminConfig, AppState};
use ferropress_plugin_host::PluginHost;
use ferropress_sched_tokiocron::TokioCronScheduler;
use ferropress_secrets_env::EnvSecretStore;
use ferropress_serve::{HookBridge, ServeEngine, default_theme};
use ferropress_store_embedded::EmbeddedStore;

use crate::config::{Cli, Command, CreateUserArgs, PostArgs, ServerConfig, TlsMode};

/// The `SecretStore` key holding the admin session signing secret. When unset, the
/// admin API is disabled (a public-only deployment).
const ADMIN_SECRET_KEY: &str = "FERROPRESS_ADMIN_SECRET";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::Serve(cfg) => run_server(cfg).await,
        Command::Post(args) => create_post_cmd(args).await,
        Command::CreateUser(args) => create_user_cmd(args).await,
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
    // The concrete store backs THREE ports. Coerce it to the synchronous
    // `content:read` and `content:write` capabilities while it is still concrete,
    // then re-bind `store` to the async data port the rest of the wiring uses.
    // (`store.clone()` coerces on the result; `Arc::clone(&store)` would force
    // `T = dyn …` and not unsize.)
    let store = select_store(&cfg)?;
    let content_reader: Arc<dyn ContentReader> = store.clone();
    let content_writer: Arc<dyn ContentWriter> = store.clone();
    let store: Arc<dyn RhypeStore> = store;

    // 2. Build the owned subsystems over the ports.
    // Build the page-chrome theme once (its template registered) and share it
    // across BOTH the HTTP read path (`AppState`) and the regen loop
    // (`ServeEngine`) so a prerendered page is byte-for-byte what an on-demand
    // render would produce.
    let theme = Arc::new(default_theme().context("building the page-chrome theme")?);

    // The embedded plugin host: load installed plugins from the plugins dir, then
    // share it as the custom-block renderer for BOTH the read path (`AppState`) and
    // the regen loop (`ServeEngine`) — so a plugin-rendered block is byte-for-byte
    // identical whichever path produced the page. (`Arc<PluginHost>` coerces to
    // `Arc<dyn CustomBlockRenderer>` at each call site.)
    // The same concrete store backs the synchronous `content:read` capability a
    // plugin granted `read_store` reaches via `fp_lookup_slug`, AND the
    // `content:write` capability a plugin granted `write_store` reaches via
    // `fp_create_page_stub` / `fp_set_meta`. content:write is safe to wire here
    // because every plugin write is stamped with PLUGIN_ORIGIN and the action-hook
    // bridge (spawned below) subscribes with `exclude_origin = PLUGIN_ORIGIN`, so a
    // plugin's own writes never re-trigger an action (the feed-loop guard); the
    // regen loop stays unfiltered and still prerenders them. Deny-by-default is
    // structural: an ungranted plugin never gets the host functions.
    let mut plugins = PluginHost::new()
        .with_content_reader(content_reader)
        .with_content_writer(content_writer);
    plugins
        .load_dir(&cfg.plugins_dir)
        .context("loading plugins")?;
    let plugins = Arc::new(plugins);

    // `plugins.clone()` yields `Arc<PluginHost>`, which coerces to
    // `Arc<dyn CustomBlockRenderer>` at each call site (the `PluginHost` is the
    // renderer for both the regen loop and the read path).
    let serve = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::clone(&theme),
        plugins.clone(),
    );
    // Serve the built wasm island bundle at `/_fp/islands` (the page chrome emits
    // the matching mount points + boot script). Built by `cargo xtask build-islands`.
    // The same plugin host is the custom-block renderer AND the hook dispatcher
    // (the `comment.create` moderation filter runs through it); `plugins.clone()`
    // coerces `Arc<PluginHost>` to each trait object at the call site.
    let mut app_state = AppState::new(Arc::clone(&store), Arc::clone(&blobs), theme)
        .with_islands_dir(cfg.islands_dir.clone())
        .with_custom_renderer(plugins.clone())
        .with_hook_dispatcher(plugins.clone());

    // Admin API + SPA: enabled ONLY when a signing secret is configured (from the
    // SecretStore). No secret -> the admin surface is not mounted (public-only),
    // which is the safe default (never a weak/guessable key). The SPA bundle is
    // served only if its build dir exists, so the API is usable before the wasm is
    // built.
    if let Some(admin) = select_admin_config(&cfg, secrets.as_ref()).await? {
        app_state = app_state.with_admin(admin);
    } else {
        tracing::warn!(
            "{ADMIN_SECRET_KEY} is not set — the admin API is DISABLED (public-only). \
             Set it (e.g. in the env file) to enable /admin."
        );
    }

    // Wired but not yet driven in v1: the scheduler and cert source come online in
    // later increments. Named so the composition seam is real and they stay
    // constructed. (`serve` is spawned below; `plugins` is now injected.)
    let _ = (&scheduler, &certs);

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

    // 3b. Spawn the change-feed -> ACTION hook bridge: an INDEPENDENT subscription
    //     (the engine hub fans out to every subscriber) that dispatches a
    //     `<type>.<created|updated|deleted>` action per committed change, so plugins
    //     react to writes after the fact — decoupled from regen above. Shares the
    //     same `Arc<PluginHost>` dispatcher as the comment-create filter.
    let bridge = Arc::new(HookBridge::new(Arc::clone(&store), plugins.clone()));
    tokio::spawn(async move {
        if let Err(e) = bridge.run().await {
            tracing::error!(error = %e, "hook bridge exited");
        }
    });

    // 4. Boot the owned HTTP server. The static-first prerender cache is now live
    //    (cache-first read path + change-driven regen loop above).
    ferropress_http::serve(app_state, cfg.bind)
        .await
        .context("running the HTTP server")?;
    Ok(())
}

/// Build the admin config from the server config + secret store, or `None` when no
/// signing secret is set (admin disabled). Fetching the secret is the ONLY reason
/// this is async.
async fn select_admin_config(
    cfg: &ServerConfig,
    secrets: &dyn SecretStore,
) -> Result<Option<AdminConfig>> {
    let secret = secrets
        .try_get(&SecretRef(ADMIN_SECRET_KEY.to_owned()))
        .await
        .with_context(|| format!("reading {ADMIN_SECRET_KEY}"))?;
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(AdminConfig {
        signing_key: Arc::new(SigningKey::derive_from_secret(&secret)),
        // Serve the SPA bundle only if it has been built; the API works regardless.
        bundle_dir: cfg.admin_dir.exists().then(|| cfg.admin_dir.clone()),
        cookie_secure: cfg.admin_cookie_secure,
        session_ttl_ms: i64::from(cfg.admin_session_hours) * 60 * 60 * 1000,
    }))
}

/// The `create-user` subcommand: seed a user (username + Argon2-hashed password +
/// role) into the embedded store, then exit — so someone can sign in to the admin.
async fn create_user_cmd(args: CreateUserArgs) -> Result<()> {
    // Validate the role up front (a clear message beats a store-side failure). The
    // snake_case strings mirror `Role`'s serde representation.
    let role = args.role.to_ascii_lowercase();
    if !matches!(
        role.as_str(),
        "subscriber" | "contributor" | "author" | "editor" | "administrator"
    ) {
        anyhow::bail!(
            "invalid role {:?} (expected subscriber|contributor|author|editor|administrator)",
            args.role
        );
    }

    let store: Arc<dyn RhypeStore> = Arc::new(
        EmbeddedStore::open(&args.data_dir)
            .with_context(|| format!("opening embedded store at {}", args.data_dir.display()))?,
    );

    // Friendly duplicate check (slug is @unique, so the engine would reject it too).
    let taken = !store
        .filter(ferropress_core::query::FilterSpec {
            type_name: TypeName::from(USER_TYPE),
            field: "slug".to_owned(),
            op: ferropress_core::query::Compare::Eq,
            value: Value::String(args.username.clone()),
            limit: Some(1),
        })
        .await
        .context("checking for an existing username")?
        .is_empty();
    if taken {
        anyhow::bail!("username {:?} already exists", args.username);
    }

    let hash =
        hash_password(&args.password).map_err(|e| anyhow::anyhow!("hashing password: {e}"))?;
    let display_name = args.display_name.unwrap_or_else(|| args.username.clone());
    let email = args
        .email
        .unwrap_or_else(|| format!("{}@localhost", args.username));

    let mut fields: FieldMap = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(args.username.clone()));
    fields.insert("uuid".to_owned(), Value::String(Uuid::now_v7().to_string()));
    fields.insert("email".to_owned(), Value::String(email));
    fields.insert("display_name".to_owned(), Value::String(display_name));
    fields.insert("role".to_owned(), Value::String(role.clone()));
    fields.insert("password_hash".to_owned(), Value::String(hash));
    fields.insert("created_at".to_owned(), Value::DateTime(now_millis()));

    let id = store
        .create(&TypeName::from(USER_TYPE), fields)
        .await
        .context("creating the user")?;
    println!(
        "created user id={} username={} role={role}",
        id.0, args.username
    );
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
    let block_tree = tree.to_json_value().context("serializing the block tree")?;

    let mut fields: FieldMap = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String(title.to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert("block_tree".to_owned(), Value::Json(block_tree));

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

/// Select the embedded rhypedb store (the only adapter today). Returns the
/// CONCRETE `Arc<EmbeddedStore>` — not a trait object — because it backs two ports:
/// it coerces to `Arc<dyn RhypeStore>` for the async data path AND to
/// `Arc<dyn ContentReader>` for the plugin host's synchronous `content:read`
/// capability. Opening runs the additive schema reconcile.
fn select_store(cfg: &ServerConfig) -> Result<Arc<EmbeddedStore>> {
    let store = EmbeddedStore::open(&cfg.data_dir)
        .with_context(|| format!("opening embedded store at {}", cfg.data_dir.display()))?;
    Ok(Arc::new(store))
}
