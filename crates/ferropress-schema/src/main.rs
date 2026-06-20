//! Ferropress schema tool. Prints the canonical SDL, validates it, and drives the
//! additive open-time reconcile against a data directory.
//!
//! `print`   — emit the canonical rhypedb SDL (the single source of truth in
//!             `ferropress-schema-sdl`).
//! `check`   — parse + validate the canonical SDL (cheap CI/pre-commit guard).
//! `migrate` — open the embedded store under `--data-dir`, which runs rhypedb's
//!             additive open-time reconcile (creating any missing types/fields),
//!             then report success.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ferropress-schema",
    about = "Ferropress schema + migration tool"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the canonical rhypedb SDL.
    Print,
    /// Parse + validate the canonical SDL.
    Check,
    /// Open `--data-dir` on the canonical schema (runs the additive reconcile).
    Migrate {
        #[arg(long, env = "FERROPRESS_DATA_DIR")]
        data_dir: std::path::PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Print => {
            println!("{}", ferropress_schema_sdl::SCHEMA_SDL);
            Ok(())
        }
        Cmd::Check => {
            ferropress_schema_sdl::parsed_schema()
                .context("canonical Ferropress SDL failed to parse/validate")?;
            eprintln!("ok: canonical schema parses + validates");
            Ok(())
        }
        Cmd::Migrate { data_dir } => {
            // Opening the embedded store materializes the canonical schema and runs
            // rhypedb's additive open-time reconcile (missing types/fields are
            // created; existing data is left in place). That IS the migration for
            // additive changes; a destructive cutover would go through
            // `run_migrations` in a later pass.
            ferropress_store_embedded::EmbeddedStore::open(&data_dir)
                .with_context(|| format!("opening embedded store at {}", data_dir.display()))?;
            eprintln!(
                "ok: store at {} opened + reconciled against the canonical schema",
                data_dir.display()
            );
            Ok(())
        }
    }
}
