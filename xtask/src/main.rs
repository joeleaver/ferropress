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
//!   * `build-admin` — the same, for the excluded `ferropress-admin` admin-SPA
//!     `cdylib` → `crates/ferropress-admin/dist/` (served at `/_fp/admin`).
//!   * `theme-contract-check` — run ferropress-serve's GENERIC external-theme contract
//!     checker (`cargo test -p ferropress-serve --lib external_theme`) against a real theme
//!     project's dir, via `FERROPRESS_EXTERNAL_THEME_DIR`. Core carries no dependency on, or
//!     knowledge of, any particular consumer theme — this is for a theme author to run
//!     against their OWN project (locally, or in their own CI).
//!   * `no-consumer-names-lint` — self-enforcing guard: fails if any tracked file in this
//!     repo mentions a banned consumer-project name (see [`BANNED_CONSUMER_NAMES`]). Core is
//!     a published, general-purpose CMS; a private consumer project's name must never leak
//!     into it.
//!
//! Run from anywhere:
//!
//! ```text
//! cargo run --manifest-path xtask/Cargo.toml -- dep-graph
//! cargo run --manifest-path xtask/Cargo.toml -- build-islands
//! cargo run --manifest-path xtask/Cargo.toml -- build-admin
//! cargo run --manifest-path xtask/Cargo.toml -- theme-contract-check
//! cargo run --manifest-path xtask/Cargo.toml -- no-consumer-names-lint
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
        Some("build-admin") => build_admin(),
        Some("build-plugins") => build_plugins(),
        Some("theme-contract-check") => run_theme_contract_check(),
        Some("no-consumer-names-lint") => no_consumer_names_lint(),
        Some(other) => bail!(
            "unknown xtask subcommand {other:?} (expected `dep-graph`, `build-islands`, \
             `build-admin`, `build-plugins`, `theme-contract-check`, or `no-consumer-names-lint`)"
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

/// Build the admin SPA wasm bundle: compile the excluded `ferropress-admin`
/// `cdylib` for `wasm32-unknown-unknown` (release), then run `wasm-bindgen
/// --target web` to emit the JS + `_bg.wasm` into `crates/ferropress-admin/dist/`.
///
/// Same shape + `wasm-bindgen`-version caveat as [`build_islands`]: the CLI must
/// match the crate's `wasm-bindgen` dependency exactly (install with
/// `cargo install wasm-bindgen-cli --version <that>`). The server serves the
/// output at `/_fp/admin` and the shell boots `ferropress_admin.js` /
/// `ferropress_admin_bg.wasm`.
fn build_admin() -> Result<()> {
    let root = repo_root()?;
    let manifest = root.join("crates/ferropress-admin/Cargo.toml");
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
        .context("failed to run `cargo build` for ferropress-admin")?;
    if !status.success() {
        bail!("ferropress-admin wasm build failed");
    }

    // 2. wasm-bindgen into dist/. The excluded crate has its OWN target dir.
    let wasm = root
        .join("crates/ferropress-admin/target/wasm32-unknown-unknown/release/ferropress_admin.wasm");
    let dist = root.join("crates/ferropress-admin/dist");
    let status = Command::new("wasm-bindgen")
        .args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            "ferropress_admin",
            "--out-dir",
            &dist.to_string_lossy(),
            &wasm.to_string_lossy(),
        ])
        .status()
        .context(
            "failed to run `wasm-bindgen` — install the matching CLI with \
             `cargo install wasm-bindgen-cli --version <ferropress-admin' wasm-bindgen version>`",
        )?;
    if !status.success() {
        bail!("wasm-bindgen failed");
    }

    println!("build-admin: OK -> {}", dist.display());
    Ok(())
}

/// Build every reference plugin under `plugins/` to `wasm32-unknown-unknown`
/// (release) and assemble its loadable bundle — `plugin.toml` + the produced wasm
/// — into `plugins/dist/<name>/`, which the server loads via `--plugins-dir`.
///
/// A plugin dir qualifies if it has BOTH a `Cargo.toml` and a `plugin.toml`
/// (the `dist` dir itself is skipped). Extism loads raw wasm, so there is no
/// wasm-bindgen step.
fn build_plugins() -> Result<()> {
    let root = repo_root()?;
    let plugins_dir = root.join("plugins");
    let dist = plugins_dir.join("dist");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());

    if !plugins_dir.is_dir() {
        println!("build-plugins: no plugins/ dir; nothing to build");
        return Ok(());
    }

    let mut built = 0usize;
    for entry in std::fs::read_dir(&plugins_dir).context("reading plugins/")? {
        let dir = entry.context("reading a plugins/ entry")?.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == "dist" {
            continue;
        }
        let manifest = dir.join("Cargo.toml");
        let plugin_toml = dir.join("plugin.toml");
        if !manifest.exists() || !plugin_toml.exists() {
            continue;
        }

        // 1. Compile to wasm32 (release).
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
            .with_context(|| format!("running cargo build for plugin {name}"))?;
        if !status.success() {
            bail!("plugin {name} wasm build failed");
        }

        // 2. Locate the produced cdylib wasm (top of the release dir).
        let release = dir.join("target/wasm32-unknown-unknown/release");
        let wasm = find_wasm(&release)?;

        // 3. Assemble plugins/dist/<name>/{plugin.toml, <wasm>}.
        let out = dist.join(&name);
        std::fs::create_dir_all(&out)
            .with_context(|| format!("creating {}", out.display()))?;
        std::fs::copy(&plugin_toml, out.join("plugin.toml"))
            .with_context(|| format!("copying plugin.toml for {name}"))?;
        let wasm_file = wasm
            .file_name()
            .context("wasm path has no file name")?;
        std::fs::copy(&wasm, out.join(wasm_file))
            .with_context(|| format!("copying wasm for {name}"))?;

        built += 1;
        println!("  built plugin {name} -> {}", out.display());
    }

    println!("build-plugins: OK ({built} plugin(s)) -> {}", dist.display());
    Ok(())
}

/// Find the single cdylib `.wasm` at the top of a cargo release dir (deps live in
/// `release/deps/`, which this does not descend into).
fn find_wasm(release_dir: &Path) -> Result<PathBuf> {
    let read = std::fs::read_dir(release_dir)
        .with_context(|| format!("reading {}", release_dir.display()))?;
    for entry in read {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "wasm") {
            return Ok(path);
        }
    }
    bail!("no .wasm produced in {}", release_dir.display())
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

/// Run ferropress-serve's GENERIC external-theme contract checker against a real theme
/// project (`crates/ferropress-serve/src/tests.rs`'s `external_theme_engines()` and its three
/// `external_theme_*` tests).
///
/// Ferropress core carries NO knowledge of, or dependency on, any particular consumer theme
/// project — the checker is entirely env-driven and structural (hook attributes + injected
/// probe identities), asserting nothing about a consumer's own copy, wording, or class names
/// (see the `ferropress-no-fellstone-in-core` policy: a consumer must depend on core, never
/// the reverse, and no consumer project's name or specifics belong in this repo). A private
/// consumer theme project therefore is NOT checked out by ferropress's own CI — there is
/// nothing here for CI to reach — so this target exists for a THEME AUTHOR to run locally
/// (or wire into THEIR OWN CI) against their own theme dir before relying on the shared
/// render contract. The tests themselves skip cleanly (never fail) when
/// `FERROPRESS_EXTERNAL_THEME_DIR` is unset; this target requires it explicitly and errors
/// clearly if it doesn't resolve to a real directory — a human explicitly running
/// `theme-contract-check` wants a real answer, not a quiet no-op.
fn run_theme_contract_check() -> Result<()> {
    let root = repo_root()?;
    let dir = std::env::var_os("FERROPRESS_EXTERNAL_THEME_DIR").map(PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!(
            "FERROPRESS_EXTERNAL_THEME_DIR is not set. Point it at your theme project's \
             themes/ dir (one subfolder per theme, each a theme.toml + the four canonical \
             templates) and re-run, e.g.:\n  \
             FERROPRESS_EXTERNAL_THEME_DIR=/path/to/your-theme-project/theme \\\n  \
             cargo run --manifest-path xtask/Cargo.toml -- theme-contract-check"
        )
    })?;
    if !dir.is_dir() {
        bail!(
            "FERROPRESS_EXTERNAL_THEME_DIR ({}) is not a directory",
            dir.display()
        );
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(&cargo)
        .args(["test", "-p", "ferropress-serve", "--lib", "external_theme"])
        .env("FERROPRESS_EXTERNAL_THEME_DIR", &dir)
        .current_dir(&root)
        .status()
        .context("failed to run `cargo test` for the external-theme contract checker")?;
    if !status.success() {
        bail!(
            "external-theme contract check FAILED against {} — the theme doesn't (or no \
             longer does) honor the shared render contract; see output above",
            dir.display()
        );
    }

    println!("theme-contract-check: OK -> ran against {}", dir.display());
    Ok(())
}

/// Consumer-project names that must never appear in ferropress's own tracked CORE sources
/// (the `ferropress-no-fellstone-in-core` policy: ferropress is a published, general-purpose
/// CMS; a private consumer project's name — or assertions specific to it — must never leak
/// into it, since the dependency direction only ever runs consumer -> core, never the
/// reverse). Extend this list if another private consumer project's name is ever mentioned in
/// core by mistake.
const BANNED_CONSUMER_NAMES: &[&str] = &["fellstone"];

/// Self-enforcing guard for the policy above: grep every tracked file in the repo (via `git
/// ls-files`, so it automatically respects `.gitignore` and only checks PUBLISHED content)
/// for a case-insensitive match on any [`BANNED_CONSUMER_NAMES`] entry.
///
/// `xtask/` itself is excluded from the scan — this file has to name the banned terms
/// literally, right here, to define what's banned; a lint that couldn't state its own rule
/// would be untestable. Everything else in the repo (`crates/`, `CLAUDE.md`, `plugins/`,
/// docs, …) — the actual product this repo publishes — is scanned.
fn no_consumer_names_lint() -> Result<()> {
    let root = repo_root()?;
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(&root)
        .output()
        .context("failed to run `git ls-files`")?;
    if !output.status.success() {
        bail!(
            "`git ls-files` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let files = String::from_utf8_lossy(&output.stdout);

    let mut violations: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for rel_path in files.lines() {
        if rel_path.starts_with("xtask/") {
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(rel_path)) else {
            continue; // listed-but-unreadable (race, submodule, …) — not this lint's job
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue; // a binary asset — nothing to grep
        };
        scanned += 1;
        let lower = text.to_lowercase();
        for name in BANNED_CONSUMER_NAMES {
            if lower.contains(&name.to_lowercase()) {
                violations.push(format!("{rel_path}: mentions {name:?}"));
            }
        }
    }

    if violations.is_empty() {
        println!(
            "no-consumer-names lint: OK ({scanned} tracked file(s) scanned, 0 occurrences of \
             {BANNED_CONSUMER_NAMES:?})"
        );
        Ok(())
    } else {
        for v in &violations {
            eprintln!("  ✗ {v}");
        }
        bail!(
            "no-consumer-names lint failed: {} tracked file(s) mention a banned consumer \
             project name (see the ferropress-no-fellstone-in-core policy)",
            violations.len()
        );
    }
}
