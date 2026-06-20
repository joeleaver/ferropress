//! Server configuration — the knobs the composition root reads to SELECT and
//! parameterize adapters. Parsed from CLI flags + environment (clap `env`), so a
//! self-host deployment can configure purely through env vars.
//!
//! This is the only place that names deployment-shaped concepts (data dir, bind
//! address, ACME mode); the ports themselves stay engine-shaped.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// How TLS is handled. Mirrors the two `ferropress-cert-acme` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TlsMode {
    /// Serve plain HTTP behind a TLS-terminating reverse proxy (default).
    Proxy,
    /// Terminate TLS in-process via embedded ACME (rustls-acme).
    Acme,
}

/// Top-level CLI: the `serve` and `post` subcommands.
#[derive(Debug, Parser)]
#[command(name = "ferropress-server", about = "Ferropress server + admin")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// CLI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the owned HTTP server.
    Serve(ServerConfig),
    /// Create a published post, then exit (admin / seeding).
    Post(PostArgs),
}

/// Arguments for `post`: create a published post from the command line.
#[derive(Debug, Args)]
pub struct PostArgs {
    /// Directory holding the embedded rhypedb database (must match the server's).
    #[arg(long, env = "FERROPRESS_DATA_DIR", default_value = "./data/db")]
    pub data_dir: PathBuf,

    /// URL slug — the post is served at `/<slug>`.
    #[arg(long)]
    pub slug: String,

    /// Post title (the page `<title>`).
    #[arg(long)]
    pub title: String,

    /// Body text, wrapped as a single paragraph block.
    #[arg(long)]
    pub body: String,
}

/// Server (serve) configuration — the knobs the composition root reads to SELECT
/// and parameterize adapters.
#[derive(Debug, Clone, Args)]
pub struct ServerConfig {
    /// Directory holding the embedded rhypedb database.
    #[arg(long, env = "FERROPRESS_DATA_DIR", default_value = "./data/db")]
    pub data_dir: PathBuf,

    /// Directory holding blobs: media originals + prerendered HTML output.
    #[arg(long, env = "FERROPRESS_BLOB_DIR", default_value = "./data/blobs")]
    pub blob_dir: PathBuf,

    /// Address the HTTP server binds.
    #[arg(long, env = "FERROPRESS_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,

    /// Optional dotenv file to seed the `SecretStore` before reading env.
    #[arg(long, env = "FERROPRESS_ENV_FILE")]
    pub env_file: Option<PathBuf>,

    /// TLS handling mode.
    #[arg(long, env = "FERROPRESS_TLS", value_enum, default_value_t = TlsMode::Proxy)]
    pub tls: TlsMode,

    /// ACME contact (e.g. `mailto:ops@example.com`), required when `--tls acme`.
    #[arg(long, env = "FERROPRESS_ACME_CONTACT")]
    pub acme_contact: Vec<String>,

    /// Public domain(s) for ACME issuance, required when `--tls acme`.
    #[arg(long, env = "FERROPRESS_ACME_DOMAIN")]
    pub acme_domain: Vec<String>,

    /// Cache directory for issued ACME certs + account key.
    #[arg(long, env = "FERROPRESS_ACME_CACHE", default_value = "./data/acme")]
    pub acme_cache: PathBuf,

    /// Directory holding the built wasm island bundle (the `dist/` output of
    /// `cargo xtask build-islands`). Served at `/_fp/islands`.
    #[arg(
        long,
        env = "FERROPRESS_ISLANDS_DIR",
        default_value = "./crates/ferropress-islands/dist"
    )]
    pub islands_dir: PathBuf,
}
