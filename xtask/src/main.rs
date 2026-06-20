//! xtask — Ferropress dev/CI tooling.
//!
//! Subcommands:
//!   * `dep-graph` (alias `dep-graph-lint`) — audit the workspace dependency
//!     graph to enforce the portability invariants (see CLAUDE.md and the
//!     `ferropress-portability` notes):
//!       * `ferropress-core` depends on NO upstream (rhypedb / rinch / jkbase).
//!       * Only `ferropress-store-embedded` and `ferropress-schema-sdl` may
//!         depend on rhypedb.
//!       * No *workspace member* may depend on rinch (rinch is consumed only by
//!         the EXCLUDED `ferropress-islands` wasm crate, which the workspace
//!         metadata never sees).
//!       * jkbase must never appear in the graph at all.
//!   * `build-islands` — compile the excluded `ferropress-islands` wasm `cdylib`
//!     and run `wasm-bindgen` into `crates/ferropress-islands/dist/` (the bundle
//!     the server serves at `/_fp/islands`).
//!
//! Run from anywhere:
//!
//! ```text
//! cargo run --manifest-path xtask/Cargo.toml -- dep-graph
//! cargo run --manifest-path xtask/Cargo.toml -- build-islands
//! ```
//!
//! The lint uses `cargo metadata --no-deps` so it is fast and offline — it reads
//! the *declared* dependencies of each workspace member without resolving or
//! fetching the (git) upstreams it is auditing.

use std::path::{Path, PathBuf};
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
        // `dep-graph-lint` is the name CI invokes; `dep-graph` is the short form.
        Some("dep-graph") | Some("dep-graph-lint") | None => dep_graph_lint(),
        Some("build-islands") => build_islands(),
        Some(other) => bail!(
            "unknown xtask subcommand {other:?} (expected `dep-graph` or `build-islands`)"
        ),
    }
}

/// The repo root (the directory holding the root `Cargo.toml`), derived from this
/// crate's manifest dir so xtask works regardless of the current directory.
fn repo_root() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest has no parent directory")?
        .to_path_buf())
}

/// Build the public-site wasm islands bundle: compile the excluded
/// `ferropress-islands` `cdylib` for `wasm32-unknown-unknown` (release), then run
/// `wasm-bindgen --target web` to emit the JS + `_bg.wasm` into
/// `crates/ferropress-islands/dist/`.
///
/// `wasm-bindgen` (the CLI) must match the crate's `wasm-bindgen` version exactly;
/// a mismatch produces a clear error from the tool. Install with
/// `cargo install wasm-bindgen-cli --version <crate version>`.
fn build_islands() -> Result<()> {
    let root = repo_root()?;
    let manifest = root.join("crates/ferropress-islands/Cargo.toml");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());

    // 1. Compile the wasm cdylib (release).
    let status = Command::new(&cargo)
        .args([
            "build",
            "--manifest-path",
            &manifest.to_string_lossy(),
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .status()
        .context("failed to run `cargo build` for ferropress-islands")?;
    if !status.success() {
        bail!("ferropress-islands wasm build failed");
    }

    // 2. wasm-bindgen into dist/. The excluded crate has its OWN target dir.
    let wasm = root
        .join("crates/ferropress-islands/target/wasm32-unknown-unknown/release/ferropress_islands.wasm");
    let dist = root.join("crates/ferropress-islands/dist");
    let status = Command::new("wasm-bindgen")
        .args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            "ferropress_islands",
            "--out-dir",
            &dist.to_string_lossy(),
            &wasm.to_string_lossy(),
        ])
        .status()
        .context(
            "failed to run `wasm-bindgen` — install the matching CLI with \
             `cargo install wasm-bindgen-cli --version <ferropress-islands' wasm-bindgen version>`",
        )?;
    if !status.success() {
        bail!("wasm-bindgen failed");
    }

    println!("build-islands: OK -> {}", dist.display());
    Ok(())
}

/// Enforce the dependency-graph invariants. Returns `Ok(())` when clean, or an
/// error listing every violation (so CI fails with a useful message).
fn dep_graph_lint() -> Result<()> {
    // Audit the *root* workspace regardless of the current directory.
    let root_manifest = repo_root()?.join("Cargo.toml");

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
