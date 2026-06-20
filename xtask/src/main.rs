//! xtask — Ferropress CI lint tool.
//!
//! Audits the workspace dependency graph to enforce the portability invariants
//! (see CLAUDE.md and the `ferropress-portability` design notes):
//!
//!   * `ferropress-core` depends on NO upstream (rhypedb / rinch / jkbase).
//!   * Only `ferropress-store-embedded` and `ferropress-schema-sdl` may depend
//!     on rhypedb.
//!   * No crate may depend on rinch yet (rinch is deferred).
//!   * jkbase must never appear in the graph at all.
//!
//! Run from the repo root:
//!
//! ```text
//! cargo run --manifest-path xtask/Cargo.toml -- dep-graph
//! ```
//!
//! Uses `cargo metadata --no-deps` so the audit is fast and offline — it reads
//! the *declared* dependencies of each workspace member without resolving or
//! fetching the (git) upstreams it is auditing.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The only crates permitted to depend on rhypedb.
const RHYPEDB_ALLOWED: &[&str] = &["ferropress-store-embedded", "ferropress-schema-sdl"];

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("dep-graph") | None => dep_graph_lint(),
        Some(other) => bail!("unknown xtask subcommand {other:?} (expected `dep-graph`)"),
    }
}

/// Enforce the dependency-graph invariants. Returns `Ok(())` when clean, or an
/// error listing every violation (so CI fails with a useful message).
fn dep_graph_lint() -> Result<()> {
    // Audit the *root* workspace regardless of the current directory.
    let root_manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest has no parent directory")?
        .join("Cargo.toml");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--manifest-path"])
        .arg(&root_manifest)
        .output()
        .context("failed to run `cargo metadata`")?;

    if !output.status.success() {
        bail!(
            "`cargo metadata` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let meta: Metadata =
        serde_json::from_slice(&output.stdout).context("failed to parse `cargo metadata` JSON")?;

    let mut violations: Vec<String> = Vec::new();
    let mut audited = 0usize;

    for pkg in &meta.packages {
        if !pkg.name.starts_with("ferropress-") {
            continue;
        }
        audited += 1;

        for dep in &pkg.dependencies {
            let d = dep.name.as_str();
            let is_rhypedb = d.starts_with("rhypedb");
            let is_rinch = d == "rinch" || d.starts_with("rinch-");
            let is_jkbase = d.starts_with("jkbase");

            if is_jkbase {
                violations.push(format!(
                    "`{}` depends on jkbase (`{d}`): jkbase must never appear in the graph",
                    pkg.name
                ));
            }
            if is_rinch {
                violations.push(format!(
                    "`{}` depends on rinch (`{d}`): rinch is deferred — no crate may depend on it yet",
                    pkg.name
                ));
            }
            if pkg.name == "ferropress-core" && is_rhypedb {
                violations.push(format!(
                    "`ferropress-core` depends on rhypedb (`{d}`): the core must carry no upstream"
                ));
            }
            if is_rhypedb && !RHYPEDB_ALLOWED.contains(&pkg.name.as_str()) {
                violations.push(format!(
                    "`{}` depends on rhypedb (`{d}`): only {RHYPEDB_ALLOWED:?} may",
                    pkg.name
                ));
            }
        }
    }

    if violations.is_empty() {
        println!("dep-graph lint: OK ({audited} ferropress crates audited)");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("  ✗ {v}");
        }
        bail!("dep-graph lint failed with {} violation(s)", violations.len());
    }
}
