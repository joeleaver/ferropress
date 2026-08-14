//! The public **theme registry**: the set of themes available to frame the site, built once
//! at boot. Exactly one theme is baked in — the built-in "letterpress" default (its sources
//! are the [`crate::templates`] consts); every other theme is DATA, discovered at runtime by
//! [`ThemeRegistry::load_dir`] from a themes directory (mirroring how the plugin host scans
//! `plugins/dist/<id>/`). A theme on disk is a folder holding a `theme.toml` manifest
//! (`id` + optional `name`) and the four canonical template files
//! `base.html` / `single.html` / `home.html` / `page-wide.html`.
//!
//! The template NAMES + the page-template mapping ([`crate::templates`]) and the render
//! context (`SingleCtx`/`HomeCtx`/`SiteCtx` in [`crate::content`]) are SHARED across every
//! theme; only the SOURCES registered under those names differ. Because a page's cached
//! envelope stores only the theme-agnostic body HTML (chrome is composed live at request
//! time), switching theme evicts **nothing** — the next request frames the same body with the
//! new theme. A v1 theme restyles that fixed set of four slots; it cannot add partials or new
//! page-template slots (those still require a core change to [`crate::templates`]).
//!
//! Loading is deliberately strict so a broken disk theme can never brick a later boot or 500
//! every page: [`ThemeRegistry::load_dir`] compiles AND smoke-renders each candidate before
//! admitting it, so **registered ⇒ renderable**. A themes-dir problem never fails boot — the
//! built-in default is always present and is the fallback [`ThemeRegistry::build`] resolves an
//! unknown id to.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferropress_core::entity::is_valid_plugin_id;
use ferropress_render_form::{Choice, DEFAULT_THEME};
use ferropress_theme::{SandboxLimits, ThemeEngine, ThemeError};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::templates::{
    BASE_SRC, BASE_TEMPLATE, HOME_SRC, HOME_TEMPLATE, PAGE_WIDE_SRC, PAGE_WIDE_TEMPLATE,
    SINGLE_SRC, SINGLE_TEMPLATE,
};

/// A theme's human label + its four template sources + the nav locations it declares. A
/// theme is fully described by these (the template NAMES + the render-context contract are
/// shared across all themes).
#[derive(Clone)]
struct ThemeSources {
    /// Human label shown in the Appearance picker.
    label: String,
    base: String,
    single: String,
    home: String,
    page_wide: String,
    /// The nav locations this theme renders, `(key, human label)` in stable key order — the
    /// admin's *assign a menu to a location* surface reads these. A theme that declares none
    /// gets the default single `primary` location (every theme has a masthead). Purely an
    /// admin-facing catalogue: the render path reads `nav.<key>` straight from the ctx, so an
    /// undeclared-but-assigned location still renders (and a declared-but-unassigned one falls
    /// back), the same permissive contract WordPress uses.
    locations: Vec<(String, String)>,
}

/// The default nav location every theme has when its manifest declares no `[menus]` — a
/// single primary (masthead) menu.
fn default_locations() -> Vec<(String, String)> {
    vec![("primary".to_owned(), "Primary Navigation".to_owned())]
}

/// A disk theme's `theme.toml` manifest (mirrors the plugin host's `plugin.toml`).
#[derive(Debug, Deserialize)]
struct ThemeManifest {
    /// The theme id — the stored `appearance.theme` value + the picker option value. Must be
    /// a dot-free ASCII id (reuses the plugin-id charset), since it flows into an
    /// `<option value>`, a stored `Setting` string, and a registry key.
    id: String,
    /// Human label; defaults to the id when absent/blank.
    name: Option<String>,
    /// The nav locations the theme renders, as a `[menus]` table of `key = "Human Label"`.
    /// Absent/empty → the default single `primary` location.
    menus: Option<BTreeMap<String, String>>,
}

/// The built-in theme's sources — the "Composing Room" letterpress (the [`crate::templates`]
/// consts). Seeded under [`DEFAULT_THEME`] so it is the single source of truth for the
/// default id shared with `ferropress_render_form`.
fn builtin_sources() -> ThemeSources {
    ThemeSources {
        label: "Composing Room \u{2014} letterpress".to_owned(),
        base: BASE_SRC.to_owned(),
        single: SINGLE_SRC.to_owned(),
        home: HOME_SRC.to_owned(),
        page_wide: PAGE_WIDE_SRC.to_owned(),
        // The built-in renders a masthead (`primary`) + a colophon footer (`footer`).
        locations: vec![
            ("primary".to_owned(), "Primary Navigation".to_owned()),
            ("footer".to_owned(), "Footer Navigation".to_owned()),
        ],
    }
}

/// The set of themes available to frame the site: the always-present built-in default plus any
/// discovered on disk. Built once at boot and thereafter immutable (folder additions/edits need
/// a restart). Held (behind an `Arc`) by [`ThemeHandle`], which builds engines from it.
pub struct ThemeRegistry {
    /// id -> sources. Always contains [`DEFAULT_THEME`] (the built-in). A `BTreeMap` gives the
    /// stable alphabetical order [`choices`](Self::choices) promises.
    themes: BTreeMap<String, ThemeSources>,
}

impl ThemeRegistry {
    /// A registry with ONLY the built-in default theme — the test / pre-settings seam (and what
    /// [`crate::content::default_theme_handle`] uses). Production boots via [`load_dir`](Self::load_dir).
    pub fn builtin() -> Self {
        let mut themes = BTreeMap::new();
        themes.insert(DEFAULT_THEME.to_owned(), builtin_sources());
        Self { themes }
    }

    /// Scan `dir` for on-disk themes and register the ones that load cleanly, on top of the
    /// always-present built-in default. INFALLIBLE — a themes-dir problem must never fail boot:
    /// a missing dir logs and yields the built-in only; an unreadable dir warns and yields the
    /// built-in only; a single bad theme is skip+logged so it can't stop the others. Only
    /// themes that parse, carry all four templates, AND compile+smoke-render are admitted
    /// (registered ⇒ renderable), so a later `build` of any registered id cannot fail.
    ///
    /// Directory entries are sorted before scanning, so disk-vs-disk id collisions resolve
    /// deterministically (first wins) and the built-in (seeded first) is un-shadowable — the
    /// same unified "id already present → skip + warn" rule covers both.
    pub fn load_dir(dir: &Path) -> Self {
        let mut registry = Self::builtin();

        if !dir.is_dir() {
            tracing::info!(dir = %dir.display(), "no themes dir; using the built-in theme only");
            return registry;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "reading themes dir; using the built-in theme only");
                return registry;
            }
        };

        // Sort for deterministic precedence + the stable order `choices()` promises.
        let mut subdirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();

        for sub in subdirs {
            let manifest_path = sub.join("theme.toml");
            if !manifest_path.exists() {
                continue;
            }
            match load_theme(&sub, &manifest_path) {
                Ok((id, sources)) => {
                    // Unified collision rule: the built-in (seeded first) and any earlier disk
                    // theme win. A disk theme claiming "letterpress" is thus ignored here.
                    if registry.themes.contains_key(&id) {
                        tracing::warn!(id = %id, dir = %sub.display(), "theme id already registered; skipping");
                        continue;
                    }
                    tracing::info!(id = %id, dir = %sub.display(), "loaded theme");
                    registry.themes.insert(id, sources);
                }
                Err(e) => tracing::error!(dir = %sub.display(), error = %e, "skipping theme"),
            }
        }
        registry
    }

    /// Build a [`ThemeEngine`] for `id`, its four templates registered under the canonical
    /// names. An unknown/stale id resolves to the built-in default (always seeded), so a
    /// misspelled or removed `appearance.theme` degrades to the default rather than erroring.
    /// In practice infallible (only compilable themes are registered), but returns `Result`
    /// because template registration is fallible in principle.
    pub fn build(&self, id: &str) -> Result<ThemeEngine, ThemeError> {
        register_engine(self.resolve(id))
    }

    /// The nav locations the theme `id` declares (`(key, label)`), for the admin's assign UI.
    /// Resolves the id the SAME way [`build`](Self::build) does (an unknown/stale id falls back
    /// to the default), so the locations offered always match the theme actually rendered.
    pub fn locations(&self, id: &str) -> Vec<(String, String)> {
        self.resolve(id).locations.clone()
    }

    /// The sources for `id`, falling back to the always-seeded built-in default for an
    /// unknown/stale id (so a misspelled/removed `appearance.theme` degrades to the default
    /// rather than erroring). The single fallback rule shared by [`build`](Self::build) and
    /// [`locations`](Self::locations).
    fn resolve(&self, id: &str) -> &ThemeSources {
        self.themes.get(id).unwrap_or_else(|| {
            self.themes
                .get(DEFAULT_THEME)
                .expect("the built-in default theme is always seeded")
        })
    }

    /// The selectable themes as picker choices — the built-in default first, then the rest in
    /// alphabetical id order (the `BTreeMap`'s order).
    pub fn choices(&self) -> Vec<Choice> {
        let mut choices = Vec::with_capacity(self.themes.len());
        if let Some(src) = self.themes.get(DEFAULT_THEME) {
            choices.push(Choice {
                value: DEFAULT_THEME.to_owned(),
                label: src.label.clone(),
            });
        }
        for (id, src) in &self.themes {
            if id != DEFAULT_THEME {
                choices.push(Choice {
                    value: id.clone(),
                    label: src.label.clone(),
                });
            }
        }
        choices
    }
}

/// Register a theme's four sources into a fresh sandboxed [`ThemeEngine`] under the canonical
/// template names. Fails only if a source does not compile (MiniJinja parse error).
fn register_engine(sources: &ThemeSources) -> Result<ThemeEngine, ThemeError> {
    let mut theme = ThemeEngine::new(SandboxLimits::default());
    theme.add_template(BASE_TEMPLATE.to_owned(), sources.base.clone())?;
    theme.add_template(SINGLE_TEMPLATE.to_owned(), sources.single.clone())?;
    theme.add_template(HOME_TEMPLATE.to_owned(), sources.home.clone())?;
    theme.add_template(PAGE_WIDE_TEMPLATE.to_owned(), sources.page_wide.clone())?;
    Ok(theme)
}

/// Load + VALIDATE one on-disk theme from its dir + `theme.toml` path. Returns `(id, sources)`
/// only if the manifest parses with a valid id, all four canonical templates are present, and
/// the theme both compiles and smoke-renders — so the registry's `registered ⇒ renderable`
/// invariant holds and a broken theme is rejected at load rather than failing a later boot or
/// render. Any problem is an `Err(String)` the caller logs and skips.
fn load_theme(dir: &Path, manifest_path: &Path) -> Result<(String, ThemeSources), String> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
    let manifest: ThemeManifest =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;

    let id = manifest.id.trim().to_owned();
    if !is_valid_plugin_id(&id) {
        return Err(format!(
            "invalid theme id {id:?} (expected a non-empty, dot-free ASCII id)"
        ));
    }
    let label = manifest
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.clone());

    let read = |file: &str| -> Result<String, String> {
        let path = dir.join(file);
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
    };
    // A `[menus]` table declares this theme's nav locations (key → human label); absent/empty
    // falls back to the default single `primary`. Sorted by key for the stable order the admin
    // assign UI lists them in.
    let locations = match manifest.menus {
        Some(map) if !map.is_empty() => map.into_iter().collect(),
        _ => default_locations(),
    };
    let sources = ThemeSources {
        label,
        base: read("base.html")?,
        single: read("single.html")?,
        home: read("home.html")?,
        page_wide: read("page-wide.html")?,
        locations,
    };

    // registered ⇒ renderable: compile all four templates AND smoke-render each in EVERY
    // production shape a real request can hit, so a syntax error OR a render-time-only fault (a
    // bad `{% extends %}`, or an `{% include %}` of an unregistered partial — resolved lazily at
    // render, not by `add_template`) is caught HERE, never at a later boot's `ThemeHandle::new`
    // or on a live page. single/page-wide are only ever framed with `is_home = false` (the front
    // page is the sole `is_home = true`); the home galley runs both populated and empty. (The
    // engine + contexts are throwaway; only the validated sources are kept.)
    let engine = register_engine(&sources).map_err(|e| format!("compiling templates: {e}"))?;
    let empty = serde_json::json!([]);
    let one_post = serde_json::json!([{
        "title": "Sample Post", "url": "/sample-post", "excerpt": "An excerpt.",
        "dateline": "January 1, 2026", "author": "A. Writer",
        "terms": [{"name": "Fiction", "href": "/category/fiction"}, {"name": "Space Opera", "href": "/tag/space-opera"}]
    }]);
    let single_ctx = sample_context(false, empty.clone());
    let home_full = sample_context(true, one_post);
    let home_empty = sample_context(true, empty);
    for (template, ctx) in [
        (SINGLE_TEMPLATE, &single_ctx),
        (PAGE_WIDE_TEMPLATE, &single_ctx),
        (HOME_TEMPLATE, &home_full),
        (HOME_TEMPLATE, &home_empty),
    ] {
        engine
            .render(template, ctx)
            .map_err(|e| format!("rendering {template}: {e}"))?;
    }
    Ok((id, sources))
}

/// A representative render context covering the full shared contract (see `content.rs`), used to
/// smoke-render a candidate theme at load. `is_home` + `posts` are varied by the caller so every
/// branch a real request hits is exercised; all other fields are present so a well-formed theme
/// renders cleanly (MiniJinja is lenient on undefined, so the check flags structural errors — bad
/// `extends`/`include`/block/syntax — not missing optional data).
fn sample_context(is_home: bool, posts: serde_json::Value) -> serde_json::Value {
    // A representative nav map covering every shape the theme's `nav.*` loops must render: a
    // current top-level link, an external new-tab link, and an unresolvable PARENT (href:null)
    // that survives as a label-only entry keeping a resolvable child — plus a footer location.
    // So a theme whose masthead/footer loops are structurally broken (bad macro/extends/include)
    // is caught at load, not on a live page (the `registered ⇒ renderable` invariant now covers nav).
    let menus = serde_json::json!({
        "primary": [
            {"label": "Home", "href": "/", "new_tab": false, "aria_current": is_home, "children": []},
            {"label": "Guides", "href": null, "new_tab": false, "aria_current": false, "children": [
                {"label": "Getting Started", "href": "/guides/start", "new_tab": false, "aria_current": false, "children": []}
            ]},
            {"label": "External", "href": "https://example.com", "new_tab": true, "aria_current": false, "children": []}
        ],
        "footer": [
            {"label": "Colophon", "href": "/colophon", "new_tab": false, "aria_current": false, "children": []}
        ]
    });
    serde_json::json!({
        "page_title": "Sample",
        "page_description": "A sample page.",
        "canonical": "https://example.com/sample",
        "site": {"title": "Sample Site", "tagline": "A tagline", "url": "https://example.com", "logo": null, "noindex": false},
        "is_home": is_home,
        "preview_status": null,
        "nav": menus,
        "title": "Sample Post",
        "dateline": "January 1, 2026",
        "kicker": "Notes",
        "author": "A. Writer",
        "author_initials": "AW",
        "featured_image": null,
        "terms": [{"name": "Fiction", "href": "/category/fiction"}, {"name": "Space Opera", "href": "/tag/space-opera"}],
        "body": "<p>Sample <strong>body</strong>.</p>",
        "posts": posts
    })
}

/// The current live theme: the built [`ThemeEngine`] paired with the `appearance.theme` id it
/// was built from. The id is kept so the regen loop can rebuild only when the active theme
/// actually changes, not on every `Setting` write.
struct ThemeState {
    /// The **requested** `appearance.theme` id this engine was built for — used solely for
    /// change-detection. It may differ from the theme actually rendered: an unknown id resolves
    /// to the built-in default via [`ThemeRegistry::build`], yet the requested id is stored
    /// verbatim so that later fixing the typo (or restoring a removed theme's folder) is
    /// re-detected as a change (self-healing). Do NOT read it as "the theme currently on screen".
    id: String,
    engine: Arc<ThemeEngine>,
    /// The nav locations the CURRENTLY RENDERED theme declares (resolved via
    /// [`ThemeRegistry::locations`], so a fallback to the default carries the default's
    /// locations). Kept beside the engine so the admin assign UI + a theme swap stay in lockstep.
    locations: Vec<(String, String)>,
}

/// A cheaply-cloneable handle to the current live public theme, bundling the boot-immutable
/// [`ThemeRegistry`] with the currently-built engine. Shared between the HTTP read path (which
/// frames every page + renders the Appearance picker) and the [`ServeEngine`](crate::ServeEngine)
/// regen loop (which rebuilds + swaps the engine when `appearance.theme` changes). A theme
/// switch takes effect with no restart and — because cached page envelopes hold only
/// theme-agnostic body HTML — with no page-cache eviction.
///
/// Reads clone the inner engine `Arc` under a short read lock; a swap replaces it under a short
/// write lock. Holding the cloned `Arc` for one render means a concurrent swap can never tear an
/// in-flight render. The [`current_id`](Self::current_id) → build → set sequence in
/// [`swap_to`](Self::swap_to) is race-free ONLY because the sequential regen loop is the SOLE
/// writer (like the settings/authors/redirects handles); the admin PUT must persist the `Setting`
/// and let the change feed drive `swap_to`, never call it directly.
#[derive(Clone)]
pub struct ThemeHandle {
    registry: Arc<ThemeRegistry>,
    state: Arc<RwLock<ThemeState>>,
}

impl ThemeHandle {
    /// Seed the handle from `registry`, building the engine for `id` (at startup, from the live
    /// `appearance.theme` setting). On a build error falls back to the built-in default, so boot
    /// can never fail on a themes-dir problem (defensive — `registered ⇒ buildable` and an
    /// unknown id already resolves to the default inside [`ThemeRegistry::build`]). The stored
    /// id is the REQUESTED `id` verbatim.
    pub fn new(registry: ThemeRegistry, id: &str) -> Result<Self, ThemeError> {
        let engine = match registry.build(id) {
            Ok(engine) => engine,
            Err(e) => {
                tracing::error!(theme = %id, error = %e, "building the boot theme; falling back to the default");
                registry.build(DEFAULT_THEME)?
            }
        };
        let locations = registry.locations(id);
        Ok(Self {
            registry: Arc::new(registry),
            state: Arc::new(RwLock::new(ThemeState {
                id: id.to_owned(),
                engine: Arc::new(engine),
                locations,
            })),
        })
    }

    /// The current built theme engine. Cloning the `Arc` is cheap; hold it for one render.
    pub fn current(&self) -> Arc<ThemeEngine> {
        Arc::clone(&self.state.read().engine)
    }

    /// The `appearance.theme` id the current engine was built from — for change-detection (see
    /// [`ThemeState::id`]; it names the *requested* theme, which may differ from the rendered
    /// one when the id is unknown/removed).
    pub fn current_id(&self) -> String {
        self.state.read().id.clone()
    }

    /// Rebuild + swap to `id` via the registry, IFF the active id changed. Keeps the current
    /// theme on a build error (never serve a broken one). The REQUESTED `id` is stored verbatim,
    /// so restoring a removed theme (or fixing a typo) is re-detected as a change.
    ///
    /// Lock discipline: NO lock is held across `registry.build` (MiniJinja compilation) or across
    /// the read→write transition — `parking_lot::RwLock` is non-reentrant, and holding it would
    /// both self-deadlock and stall every HTTP reader for the whole compile. Race-free only under
    /// the single-writer discipline documented on the type.
    pub fn swap_to(&self, id: &str) {
        if self.current_id() == id {
            return;
        }
        match self.registry.build(id) {
            Ok(engine) => {
                self.set(id, engine);
                tracing::info!(theme = %id, "swapped live public theme");
            }
            Err(e) => {
                tracing::error!(theme = %id, error = %e, "building the new theme; keeping the current one")
            }
        }
    }

    /// The selectable theme choices for the Appearance picker. Always includes the CURRENT id:
    /// if the active theme's folder was removed/renamed after its id was saved, the registry no
    /// longer lists it, but the admin Save PUTs the full value set and `coerce_values` rejects an
    /// out-of-vocab `Select` — so append the current id as an "(unavailable)" option to keep
    /// every settings PUT valid while still letting the admin switch to a real theme.
    pub fn choices(&self) -> Vec<Choice> {
        let mut choices = self.registry.choices();
        let current = self.current_id();
        if !choices.iter().any(|c| c.value == current) {
            choices.push(Choice {
                value: current.clone(),
                label: format!("{current} (unavailable)"),
            });
        }
        choices
    }

    /// The nav locations the currently-rendered theme declares (`(key, human label)`), for
    /// the admin's *assign a menu to a location* surface. Read under a short read lock.
    pub fn locations(&self) -> Vec<(String, String)> {
        self.state.read().locations.clone()
    }

    /// Replace the current theme (id + engine + declared locations) under one short write lock.
    /// Private: the only caller is [`swap_to`](Self::swap_to), which enforces the lock +
    /// single-writer discipline.
    fn set(&self, id: &str, engine: ThemeEngine) {
        let locations = self.registry.locations(id);
        let mut state = self.state.write();
        state.id = id.to_owned();
        state.engine = Arc::new(engine);
        state.locations = locations;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Write a minimal valid on-disk theme (manifest + the four canonical templates) into
    /// `parent/<id>/`, whose base carries `marker` so a render can be attributed to it.
    fn write_theme(parent: &Path, id: &str, marker: &str) {
        let dir = parent.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("theme.toml"),
            format!("id = \"{id}\"\nname = \"{id} theme\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("base.html"),
            format!(
                "<!doctype html><html><head><title>{{{{ page_title }}}}</title><!--{marker}--></head>\
                 <body>{{% block main %}}{{% endblock %}}</body></html>"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("single.html"),
            "{% extends \"base.html\" %}{% block main %}<h1>{{ title }}</h1>{{ body | safe }}{% endblock %}",
        )
        .unwrap();
        std::fs::write(
            dir.join("home.html"),
            "{% extends \"base.html\" %}{% block main %}{% for post in posts %}<a href=\"{{ post.url }}\">{{ post.title }}</a>{% endfor %}{% endblock %}",
        )
        .unwrap();
        std::fs::write(
            dir.join("page-wide.html"),
            "{% extends \"base.html\" %}{% block main %}<article>{{ body | safe }}</article>{% endblock %}",
        )
        .unwrap();
    }

    #[test]
    fn builtin_registry_always_has_the_default_and_it_builds() {
        let reg = ThemeRegistry::builtin();
        // The invariant the fallback + boot depend on: DEFAULT_THEME is always seeded.
        assert!(reg.build(DEFAULT_THEME).is_ok(), "the default theme builds");
        let choices = reg.choices();
        assert_eq!(
            choices.first().map(|c| c.value.as_str()),
            Some(DEFAULT_THEME)
        );
        assert_eq!(choices.len(), 1, "builtin registry offers only the default");
    }

    #[test]
    fn builtin_nav_renders_apg_disclosure_and_no_aria_haspopup() {
        // The sample nav's "Guides" is a label-only PARENT (href:null, one child) — the exact WP
        // shape the a11y fix targets. In the PRIMARY (flyout) nav it must become a focusable
        // disclosure <button aria-expanded aria-controls>, and its child <ul> must carry that id.
        let engine = ThemeRegistry::builtin().build(DEFAULT_THEME).unwrap();
        let html = engine
            .render("home.html", &sample_context(true, serde_json::json!([])))
            .expect("home renders");

        // aria-haspopup is semantically wrong for a CSS/JS flyout of links — it must appear NOWHERE.
        assert!(
            !html.contains("aria-haspopup"),
            "nav must not assert a role=menu widget via aria-haspopup"
        );
        // The label-only parent is a disclosure button controlling its submenu list.
        assert!(
            html.contains("class=\"navtree__label navsub-toggle\"")
                && html.contains("aria-expanded=\"false\"")
                && html.contains("aria-controls=\"nav-primary-2\""),
            "the label-only primary parent renders an APG disclosure button: {html}"
        );
        assert!(
            html.contains("<ul class=\"navtree\" id=\"nav-primary-2\">"),
            "the controlled submenu <ul> carries the aria-controls id: {html}"
        );
        // Progressive enhancement: the fp-js gate + the disclosure controller ship in the page.
        assert!(
            html.contains("html:not(.fp-js)") && html.contains("classList.add('fp-js')"),
            "the no-JS fallback gate + JS controller are present"
        );
        // The FOOTER nav (flyout=false) gets NO disclosure buttons — its submenus render inline.
        // Bound the slice to the footer <nav>…</nav> only: the CSS `.colophon-nav` rule (in <head>)
        // and the JS controller (`.navsub-toggle`, after the footer) both mention the class.
        let f_start = html
            .find("aria-label=\"Footer\"")
            .expect("footer nav present");
        let f_end = f_start + html[f_start..].find("</nav>").expect("footer nav closes");
        let footer = &html[f_start..f_end];
        assert!(
            !footer.contains("navsub-toggle"),
            "footer nav must not emit disclosure toggles"
        );
    }

    #[test]
    fn load_dir_missing_is_builtin_only() {
        let reg = ThemeRegistry::load_dir(Path::new("/no/such/themes/dir"));
        assert_eq!(reg.choices().len(), 1);
        assert_eq!(reg.choices()[0].value, DEFAULT_THEME);
    }

    #[test]
    fn load_dir_admits_a_valid_theme_and_orders_choices() {
        let tmp = tempfile::tempdir().unwrap();
        write_theme(tmp.path(), "aurora", "AURORA-MARK");
        let reg = ThemeRegistry::load_dir(tmp.path());

        // Both present; default first, then the disk theme.
        let choices = reg.choices();
        assert_eq!(choices[0].value, DEFAULT_THEME);
        assert!(
            choices
                .iter()
                .any(|c| c.value == "aurora" && c.label == "aurora theme")
        );

        // The disk theme builds + renders with its own marker.
        let engine = reg.build("aurora").expect("aurora builds");
        let home = engine
            .render("home.html", &sample_context(true, serde_json::json!([])))
            .expect("aurora home renders");
        assert!(
            home.contains("AURORA-MARK"),
            "the aurora base framed it: {home}"
        );
    }

    #[test]
    fn a_broken_theme_is_skipped_not_admitted() {
        let tmp = tempfile::tempdir().unwrap();
        // Valid manifest + all four files, but base.html has a MiniJinja syntax error.
        let dir = tmp.path().join("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("theme.toml"), "id = \"broken\"\n").unwrap();
        std::fs::write(
            dir.join("base.html"),
            "<html>{% block main %}{% endblock %}",
        )
        .unwrap();
        // Unterminated tag -> add_template (compile) fails.
        std::fs::write(
            dir.join("single.html"),
            "{% extends \"base.html\" %}{% block main %}{% if %}oops{% endblock %}",
        )
        .unwrap();
        std::fs::write(
            dir.join("home.html"),
            "{% extends \"base.html\" %}{% block main %}{% endblock %}",
        )
        .unwrap();
        std::fs::write(
            dir.join("page-wide.html"),
            "{% extends \"base.html\" %}{% block main %}{% endblock %}",
        )
        .unwrap();

        let reg = ThemeRegistry::load_dir(tmp.path());
        assert!(
            !reg.choices().iter().any(|c| c.value == "broken"),
            "a theme that fails to compile must NOT be registered",
        );
    }

    #[test]
    fn a_disk_theme_cannot_shadow_the_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        write_theme(tmp.path(), DEFAULT_THEME, "IMPOSTER"); // claims "letterpress"
        let reg = ThemeRegistry::load_dir(tmp.path());
        // Still exactly the built-in, and it is the REAL built-in (no imposter marker).
        assert_eq!(reg.choices().len(), 1);
        let home = reg
            .build(DEFAULT_THEME)
            .unwrap()
            .render("home.html", &sample_context(true, serde_json::json!([])))
            .unwrap();
        assert!(!home.contains("IMPOSTER"), "the built-in is un-shadowable");
    }

    #[test]
    fn unknown_id_builds_the_default() {
        let reg = ThemeRegistry::builtin();
        let unknown = reg.build("does-not-exist").unwrap();
        let default = reg.build(DEFAULT_THEME).unwrap();
        assert_eq!(
            unknown
                .render("home.html", &sample_context(true, serde_json::json!([])))
                .unwrap(),
            default
                .render("home.html", &sample_context(true, serde_json::json!([])))
                .unwrap(),
            "an unknown id renders exactly the default theme",
        );
    }

    #[test]
    fn swap_to_rebuilds_only_on_a_real_change() {
        let tmp = tempfile::tempdir().unwrap();
        write_theme(tmp.path(), "aurora", "AURORA-MARK");
        let handle = ThemeHandle::new(ThemeRegistry::load_dir(tmp.path()), DEFAULT_THEME)
            .expect("handle builds");

        // Unchanged id: no rebuild (the engine Arc is pointer-identical).
        let before = handle.current();
        handle.swap_to(DEFAULT_THEME);
        assert!(Arc::ptr_eq(&before, &handle.current()));
        assert_eq!(handle.current_id(), DEFAULT_THEME);

        // Real change: swaps to the disk theme.
        handle.swap_to("aurora");
        assert_eq!(handle.current_id(), "aurora");
        let home = handle
            .current()
            .render("home.html", &sample_context(true, serde_json::json!([])))
            .unwrap();
        assert!(home.contains("AURORA-MARK"));

        // Build "error" path is exercised by an unknown id: build() resolves it to the default
        // engine, and the REQUESTED id is stored verbatim (self-healing).
        handle.swap_to("ghost");
        assert_eq!(handle.current_id(), "ghost");
        let home = handle
            .current()
            .render("home.html", &sample_context(true, serde_json::json!([])))
            .unwrap();
        assert!(
            !home.contains("AURORA-MARK"),
            "unknown id renders the default, not aurora"
        );
    }

    #[test]
    fn choices_appends_an_unavailable_current_theme() {
        // Active theme id that the registry does not know (its folder was removed).
        let handle = ThemeHandle::new(ThemeRegistry::builtin(), "vanished").expect("handle builds");
        let choices = handle.choices();
        let current = choices.iter().find(|c| c.value == "vanished");
        assert!(
            current.is_some(),
            "the current id is always offered so a PUT can't be rejected"
        );
        assert!(current.unwrap().label.contains("unavailable"));
    }
}
