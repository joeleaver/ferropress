//! Ferropress schema tool. Prints the canonical SDL, validates it, and drives the
//! additive open-time reconcile against a data directory.
//!
//! `print`   — emit the canonical rhypedb SDL (the single source of truth in
//!             `ferropress-schema-sdl`).
//! `check`   — parse + validate the canonical SDL (cheap CI/pre-commit guard).
//! `migrate` — open the embedded store under `--data-dir`, which runs rhypedb's
//!             additive open-time reconcile (creating any missing types/fields),
//!             then SEED the default taxonomies (idempotently) and report success.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use ferropress_core::TAXONOMY_TYPE;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, TypeName, Value};

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
            let store = ferropress_store_embedded::EmbeddedStore::open(&data_dir)
                .with_context(|| format!("opening embedded store at {}", data_dir.display()))?;
            // Provisioning the default taxonomies is part of the migration (NOT a
            // boot-time create: check-then-create races two concurrent boots, and
            // an uncaught @unique violation would crash a server; here it runs once,
            // serially). The store verbs are async — one small runtime drives them.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building the seed runtime")?
                .block_on(seed_default_taxonomies(Arc::new(store)))
                .context("seeding the default taxonomies")?;
            eprintln!(
                "ok: store at {} opened + reconciled against the canonical schema",
                data_dir.display()
            );
            Ok(())
        }
    }
}

/// The two WP-default taxonomies every Ferropress site starts with:
/// `category` (hierarchical) and `tag` (flat), both multi-assign. Idempotent —
/// a taxonomy whose `@unique` key already exists is left untouched, and a create
/// that STILL collides (a concurrent writer) is re-checked and treated as
/// already-present rather than failing the migration.
async fn seed_default_taxonomies(store: Arc<dyn RhypeStore>) -> Result<()> {
    for (key, label, hierarchical) in [("category", "Categories", true), ("tag", "Tags", false)] {
        if taxonomy_exists(&store, key).await? {
            eprintln!("seed: taxonomy {key:?} already present");
            continue;
        }
        let mut fields: FieldMap = FieldMap::new();
        fields.insert("key".to_owned(), Value::String(key.to_owned()));
        fields.insert("label".to_owned(), Value::String(label.to_owned()));
        fields.insert("hierarchical".to_owned(), Value::Bool(hierarchical));
        fields.insert("multiple".to_owned(), Value::Bool(true));
        fields.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
        match store.create(&TypeName::from(TAXONOMY_TYPE), fields).await {
            Ok(_) => eprintln!("seed: created taxonomy {key:?}"),
            // `Taxonomy.key` is @unique — a losing race means someone else seeded
            // it, which is exactly the desired end state.
            Err(e) if taxonomy_exists(&store, key).await.unwrap_or(false) => {
                eprintln!("seed: taxonomy {key:?} appeared concurrently ({e}); keeping it");
            }
            Err(e) => return Err(e).with_context(|| format!("creating taxonomy {key:?}")),
        }
    }
    Ok(())
}

/// Whether a taxonomy with `key` already exists (the `@unique` key lookup).
async fn taxonomy_exists(store: &Arc<dyn RhypeStore>, key: &str) -> Result<bool> {
    let rows = store
        .filter(FilterSpec {
            type_name: TypeName::from(TAXONOMY_TYPE),
            field: "key".to_owned(),
            op: Compare::Eq,
            value: Value::String(key.to_owned()),
            limit: Some(1),
        })
        .await
        .context("filtering taxonomies by key")?;
    Ok(!rows.is_empty())
}
