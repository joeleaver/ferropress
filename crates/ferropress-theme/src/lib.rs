//! # ferropress-theme
//!
//! The MiniJinja host that renders a page's *chrome* (head, nav, layout, footer)
//! around content that has **already** been turned into HTML by
//! [`ferropress_render`]. This is the second half of the one-shared-renderer
//! invariant: blocks become HTML in exactly one place (`ferropress-render`), and
//! templates here only position the resulting string — they never see blocks.
//!
//! ## Sandbox
//!
//! Themes are authored by untrusted third parties, so the template host is
//! sandboxed. MiniJinja gives us the in-engine guards ([`set_recursion_limit`],
//! autoescape, a restricted function set); Ferropress owns the out-of-engine
//! guards (a wall-clock render budget enforced on a worker thread, and an
//! output-size cap). The recursion limit and output cap are wired here; the
//! worker-thread timeout harness is tracked as a TODO below.
//!
//! [`set_recursion_limit`]: minijinja::Environment::set_recursion_limit

use std::time::Duration;

use minijinja::Environment;
use serde::Serialize;
use thiserror::Error;

/// Limits the theme sandbox enforces on untrusted templates.
#[derive(Debug, Clone)]
pub struct SandboxLimits {
    /// Hard cap on template include/macro recursion (MiniJinja in-engine guard).
    pub recursion_limit: usize,
    /// Wall-clock budget for a single render (enforced by the worker-thread
    /// harness — see the TODO in [`ThemeEngine::render`]).
    pub render_timeout: Duration,
    /// Maximum size of a rendered page, in bytes. Larger output is rejected.
    pub max_output_bytes: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            recursion_limit: 64,
            render_timeout: Duration::from_millis(250),
            max_output_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Errors raised while rendering page chrome.
#[derive(Debug, Error)]
pub enum ThemeError {
    /// The underlying MiniJinja template failed to parse or render.
    #[error("template error: {0}")]
    Template(#[from] minijinja::Error),
    /// The rendered page exceeded the sandbox output cap.
    #[error("rendered output exceeded the {limit}-byte sandbox cap")]
    OutputTooLarge { limit: usize },
    // TODO: a `Timeout` variant once the worker-thread render budget is enforced.
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, ThemeError>;

/// A sandboxed MiniJinja host that owns a set of theme templates and renders
/// page chrome around pre-rendered content.
pub struct ThemeEngine {
    env: Environment<'static>,
    limits: SandboxLimits,
}

impl ThemeEngine {
    /// Build a theme host with the given sandbox limits, applying the in-engine
    /// guards MiniJinja supports.
    pub fn new(limits: SandboxLimits) -> Self {
        let mut env = Environment::new();
        env.set_recursion_limit(limits.recursion_limit);
        // TODO: install the function allow-list and confirm the autoescape
        // policy for `.html` templates before loading untrusted theme sources.
        Self { env, limits }
    }

    /// Register a (theme-author-supplied, untrusted) template by name.
    pub fn add_template(&mut self, name: String, source: String) -> Result<()> {
        self.env.add_template_owned(name, source)?;
        Ok(())
    }

    /// Render the named template with a serializable context, enforcing the
    /// output cap.
    ///
    /// The caller shapes the context (site settings, page fields, and the
    /// already-rendered block body from `ferropress-render`); the template frames
    /// it. Any pre-rendered HTML in the context is emitted through the `| safe`
    /// filter — everything else auto-escapes, since the templates are `.html`.
    pub fn render<C: Serialize>(&self, template: &str, ctx: &C) -> Result<String> {
        // TODO: run this render on a worker thread and abort it if it exceeds
        // `self.limits.render_timeout` (MiniJinja has no internal time guard).
        let _budget = self.limits.render_timeout;

        let tmpl = self.env.get_template(template)?;
        let rendered = tmpl.render(ctx)?;

        if rendered.len() > self.limits.max_output_bytes {
            return Err(ThemeError::OutputTooLarge {
                limit: self.limits.max_output_bytes,
            });
        }
        Ok(rendered)
    }
}
