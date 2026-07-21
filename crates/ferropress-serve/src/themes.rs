//! The public **theme registry**: a set of named themes, each a bundle of the four
//! MiniJinja template sources registered under the canonical template names
//! ([`BASE_TEMPLATE`]/[`SINGLE_TEMPLATE`]/[`HOME_TEMPLATE`]/[`PAGE_WIDE_TEMPLATE`]).
//! The active theme is chosen by the `appearance.theme` setting; [`build_theme`]
//! resolves an id to a ready [`ThemeEngine`]. An unknown id falls back to the
//! default theme, so a stale/typo'd setting never fails a boot — it serves the
//! default rather than erroring.
//!
//! The template NAMES + the page-template mapping ([`crate::templates`]) and the
//! render context (`SingleCtx`/`HomeCtx`/`SiteCtx` in [`crate::content`]) are SHARED
//! across themes; only the SOURCES registered under those names differ. Because a
//! page's cached envelope stores only the theme-agnostic body HTML (chrome is
//! composed live at request time), switching theme evicts **nothing** — the next
//! request just frames the same body with the new theme.
//!
//! Adding a theme = a new id + label in `ferropress_render_form` (the Appearance
//! picker's single source of truth) and a matching [`ThemeDef`] here.

use std::sync::Arc;

use ferropress_render_form::THEME_FELLSTONE;
use ferropress_theme::{SandboxLimits, ThemeEngine, ThemeError};
use parking_lot::RwLock;

use crate::templates::{
    BASE_SRC, BASE_TEMPLATE, HOME_SRC, HOME_TEMPLATE, PAGE_WIDE_SRC, PAGE_WIDE_TEMPLATE,
    SINGLE_SRC, SINGLE_TEMPLATE,
};

/// The current live theme: the built [`ThemeEngine`] paired with the `appearance.theme`
/// id it was built from. The id is kept so the regen loop can rebuild the engine ONLY
/// when the active theme actually changes, not on every `Setting` write.
struct ThemeState {
    /// The **requested** `appearance.theme` id this engine was built for — used solely
    /// for change-detection. It may differ from the theme actually rendered: an unknown
    /// id resolves to the default theme via [`theme_def`], yet the requested id is stored
    /// verbatim so that later fixing the typo is re-detected as a change (self-healing).
    /// Do NOT read it as "the theme currently on screen".
    id: String,
    engine: Arc<ThemeEngine>,
}

/// A cheaply-cloneable handle to the current live public [`ThemeEngine`], shared between
/// the HTTP read path (which frames every page) and the [`ServeEngine`](crate::ServeEngine)
/// regen loop (which rebuilds + swaps it when `appearance.theme` changes). This makes a
/// theme switch take effect with no restart and — because cached page envelopes hold only
/// theme-agnostic body HTML (chrome is composed live) — with no page-cache eviction.
///
/// Mirrors [`SettingsHandle`](crate::SettingsHandle): reads clone the inner `Arc` under a
/// short read lock; a swap replaces it under a short write lock. The engine is read on every
/// request and rebuilt only on a (rare) theme change, so a plain `RwLock` is ample. Holding
/// the cloned `Arc` for the duration of one render means a concurrent swap can never tear an
/// in-flight render — it keeps its engine; the next request sees the new one.
///
/// The [`current_id`](Self::current_id) → build → [`set`](Self::set) sequence the regen loop
/// runs is race-free ONLY because that loop is the SOLE writer (the same single-consumer
/// assumption the settings/authors/redirects handles already rely on). Parallelizing the
/// consumer would need a compare-and-swap here instead.
#[derive(Clone)]
pub struct ThemeHandle(Arc<RwLock<ThemeState>>);

impl ThemeHandle {
    /// Seed the handle with the theme built for `id` (at startup, from the live
    /// `appearance.theme` setting).
    pub fn new(id: impl Into<String>, engine: ThemeEngine) -> Self {
        Self(Arc::new(RwLock::new(ThemeState {
            id: id.into(),
            engine: Arc::new(engine),
        })))
    }

    /// The current built theme engine. Cloning the `Arc` is cheap; hold it for the
    /// duration of one render.
    pub fn current(&self) -> Arc<ThemeEngine> {
        Arc::clone(&self.0.read().engine)
    }

    /// The `appearance.theme` id the current engine was built from — for change-detection
    /// (see [`ThemeState::id`]; it names the *requested* theme, which may differ from the
    /// rendered one when the id is unknown).
    pub fn current_id(&self) -> String {
        self.0.read().id.clone()
    }

    /// Replace the current theme (called by the regen loop on an `appearance.theme`
    /// change). Swaps the id and the engine together under one write lock.
    pub fn set(&self, id: impl Into<String>, engine: ThemeEngine) {
        let mut state = self.0.write();
        state.id = id.into();
        state.engine = Arc::new(engine);
    }
}

/// A public theme: the four template sources registered under the canonical
/// template names. The context contract + template names are shared, so a theme is
/// fully described by its four sources.
struct ThemeDef {
    base: &'static str,
    single: &'static str,
    home: &'static str,
    page_wide: &'static str,
}

/// The built-in "Composing Room" letterpress theme (sources in [`crate::templates`]).
const LETTERPRESS: ThemeDef = ThemeDef {
    base: BASE_SRC,
    single: SINGLE_SRC,
    home: HOME_SRC,
    page_wide: PAGE_WIDE_SRC,
};

/// The "Fellstone Tales" theme — a faithful reproduction of fellstonetales.com.
const FELLSTONE: ThemeDef = ThemeDef {
    base: FELLSTONE_BASE_SRC,
    single: FELLSTONE_SINGLE_SRC,
    home: FELLSTONE_HOME_SRC,
    page_wide: FELLSTONE_PAGE_WIDE_SRC,
};

/// Resolve a theme id to its bundle, falling back to the default (letterpress) on an
/// unknown id — a stale or misspelled `appearance.theme` degrades to the default
/// rather than failing the render.
fn theme_def(id: &str) -> &'static ThemeDef {
    match id {
        THEME_FELLSTONE => &FELLSTONE,
        _ => &LETTERPRESS,
    }
}

/// Build a [`ThemeEngine`] with the named theme's four templates registered under
/// the canonical template names. An unknown id resolves to the default theme.
pub fn build_theme(id: &str) -> Result<ThemeEngine, ThemeError> {
    let def = theme_def(id);
    let mut theme = ThemeEngine::new(SandboxLimits::default());
    theme.add_template(BASE_TEMPLATE.to_owned(), def.base.to_owned())?;
    theme.add_template(SINGLE_TEMPLATE.to_owned(), def.single.to_owned())?;
    theme.add_template(HOME_TEMPLATE.to_owned(), def.home.to_owned())?;
    theme.add_template(PAGE_WIDE_TEMPLATE.to_owned(), def.page_wide.to_owned())?;
    Ok(theme)
}

// ─────────────────────────────────────────────────────────────────────────────
// Fellstone Tales theme sources.
//
// A faithful reproduction of fellstonetales.com: a parchment "page" floating on a
// near-black field, an elegant Gowun Batang serif throughout, a bronze logotype
// masthead (from the `site.logo` setting), and a white serif nav bar. Consumes the
// SAME context as the letterpress theme (`SiteCtx`/`SingleCtx`/`HomeCtx`), so no
// serve-side change is needed to render it.
//
// INTERIM (this is theme-slice step 1): the nav is the single Front-page link
// (the menu system is a later step), the single template is single-column (the
// sidebar + widgets are a later step), and the galley has no term chips or
// pagination yet (taxonomies + pagination are later steps). Each of those extends
// this template + the render context when its backend lands.
// ─────────────────────────────────────────────────────────────────────────────

/// Fellstone shared chrome: `<head>`, the masthead (logo or title), the nav bar +
/// `#fp-search` island mount, the colophon, the inline stylesheet, and the island
/// boot script. Reads **live** `site.*` settings, so a settings change needs no
/// page regeneration.
pub const FELLSTONE_BASE_SRC: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="alternate" type="application/rss+xml" title="RSS feed" href="/feed.xml">
<link rel="alternate" type="application/atom+xml" title="Atom feed" href="/feed.atom">
<title>{{ page_title }}</title>
{% if page_description %}<meta name="description" content="{{ page_description }}">
{% endif %}{% if site.noindex %}<meta name="robots" content="noindex, nofollow">
{% endif %}{% if canonical %}<link rel="canonical" href="{{ canonical }}">
{% endif %}<style>{% raw %}
@import url('https://fonts.googleapis.com/css2?family=Gowun+Batang:wght@400;700&family=Open+Sans:ital,wght@0,400;0,600;0,700;1,400&display=swap');
:root {
  --frame:#1B1C21; --paper:#EDE8CB; --line:rgba(40,32,16,.14); --white:#fff;
  --ink:#2B2723; --body:#5C5245; --muted:#8A7F6C; --bronze:#A9702F; --bronze-deep:#7C4E1D;
  --blue:#2EA3F2; --orange:#E8912E; --radius:12px; --radius-sm:6px;
  --ff-display:"Gowun Batang", Georgia, "Times New Roman", serif;
  --ff-ui:"Open Sans", system-ui, -apple-system, sans-serif;
  --measure:44rem;
}
* { box-sizing:border-box; }
html { -webkit-text-size-adjust:100%; }
body { margin:0; background:var(--frame); color:var(--body); font-family:var(--ff-ui);
  font-size:16px; line-height:1.6; -webkit-font-smoothing:antialiased; text-rendering:optimizeLegibility;
  padding:28px 16px; }
a { color:inherit; }
img { max-width:100%; }
.skip { position:absolute; left:-999px; top:0; }
.skip:focus { left:1rem; top:1rem; z-index:60; background:var(--ink); color:var(--paper);
  padding:.5rem .8rem; border-radius:var(--radius-sm); }
/* Draft-preview banner (authenticated new-tab preview only) */
.preview-bar { position:sticky; top:0; z-index:70; display:flex; align-items:center;
  justify-content:center; gap:.7rem; flex-wrap:wrap; padding:.5rem 1rem; margin:-28px -16px 28px;
  background:var(--orange); color:#fff; font-family:var(--ff-ui); font-size:.75rem;
  letter-spacing:.06em; text-transform:uppercase; }
.preview-bar__tag { font-weight:700; letter-spacing:.14em; border:1px solid rgba(255,255,255,.6);
  padding:.12rem .45rem; border-radius:3px; }
.preview-bar__msg { color:rgba(255,255,255,.92); }
/* The parchment page floating on the dark field */
.page { max-width:1180px; margin:0 auto; background:var(--paper); border-radius:var(--radius);
  overflow:hidden; box-shadow:0 24px 60px -28px rgba(0,0,0,.7); }
/* Masthead — the bronze logotype or the site title */
.masthead { padding:34px 24px 18px; text-align:center; }
.brand { font-family:var(--ff-display); font-weight:700; font-size:clamp(2rem,6vw,3rem);
  line-height:1.05; color:var(--ink); text-decoration:none; display:inline-block; }
.brand:hover { color:var(--bronze-deep); }
.brand--logo { line-height:0; }
.brand--logo img { max-width:min(460px,80%); width:auto; height:auto; }
.brand__tagline { font-family:var(--ff-ui); font-size:.82rem; letter-spacing:.1em;
  text-transform:uppercase; color:var(--muted); margin:.8rem 0 0; }
/* Nav — white bar, serif small-caps */
.nav { background:var(--white); border-top:1px solid var(--line); border-bottom:1px solid var(--line);
  display:flex; align-items:center; justify-content:center; flex-wrap:wrap; gap:1.4rem; padding:10px 20px; }
.nav ul { list-style:none; margin:0; padding:0; display:flex; flex-wrap:wrap; align-items:center;
  justify-content:center; gap:1.65rem; }
.nav a { font-family:var(--ff-display); font-size:1rem; letter-spacing:.06em; text-transform:uppercase;
  color:#5b5240; text-decoration:none; padding:2px 0; border-bottom:2px solid transparent;
  transition:color .15s, border-color .15s; }
.nav a:hover { color:var(--bronze-deep); border-bottom-color:var(--bronze); }
.nav a[aria-current="page"] { color:var(--ink); }
.nav__search:not(:empty) { display:inline-flex; align-items:center; }
/* Content shell */
main { display:block; padding:36px 75px 24px; }
.eyebrow { font-family:var(--ff-ui); font-size:.72rem; font-weight:600; letter-spacing:.18em;
  text-transform:uppercase; color:var(--muted); margin:0 0 1.4rem; }
/* Front page — the galley */
.galley { list-style:none; margin:0; padding:0; }
.entry { padding:1.7rem 0; border-bottom:1px solid var(--line); }
.entry:first-child { padding-top:0; }
.entry__meta { font-family:var(--ff-ui); font-size:.72rem; font-weight:600; letter-spacing:.08em;
  text-transform:uppercase; color:var(--muted); margin:0 0 .5rem; }
.entry__title { margin:0 0 .4rem; }
.entry__title a { font-family:var(--ff-display); font-weight:400; font-size:1.7rem; line-height:1.18;
  color:var(--ink); text-decoration:none;
  background-image:linear-gradient(var(--bronze),var(--bronze)); background-size:0% 1.5px;
  background-position:0 100%; background-repeat:no-repeat; transition:background-size .22s ease, color .15s; }
.entry__title a:hover { background-size:100% 1.5px; color:var(--bronze-deep); }
.entry__excerpt { margin:.2rem 0 0; color:var(--body); max-width:var(--measure); }
.entry__more { display:inline-block; margin-top:.7rem; color:var(--blue); text-decoration:none;
  font-weight:600; font-size:.92rem; }
.entry__more:hover { text-decoration:underline; }
.empty { font-family:var(--ff-ui); color:var(--muted); padding:2rem 0; }
/* Single post / page */
.article__title { font-family:var(--ff-display); font-weight:400; color:var(--ink);
  font-size:clamp(1.9rem,4vw,2.4rem); line-height:1.15; margin:0 0 .8rem; }
.article__meta { font-family:var(--ff-ui); font-size:.74rem; font-weight:600; letter-spacing:.08em;
  text-transform:uppercase; color:var(--muted); margin:0 0 .6rem; }
.article__byline { display:flex; align-items:center; gap:.55rem; font-family:var(--ff-ui);
  font-size:.8rem; color:var(--body); margin:0 0 1.4rem; }
.byline__avatar { width:26px; height:26px; border-radius:50%; background:var(--bronze); color:#fff;
  display:grid; place-items:center; font-weight:700; font-size:.72rem; }
.figure { margin:0 0 1.6rem; }
.figure img { display:block; border-radius:4px; box-shadow:0 8px 22px -12px rgba(0,0,0,.5); }
/* Article body — the semantic HTML from ferropress-render */
.prose { font-family:var(--ff-display); color:var(--body); font-size:1.08rem; line-height:1.75;
  max-width:var(--measure); }
.prose p { margin:0 0 1.15rem; }
.prose h2 { font-family:var(--ff-display); font-weight:700; color:var(--ink); font-size:1.5rem;
  margin:2rem 0 .6rem; }
.prose h3 { font-family:var(--ff-display); font-weight:700; color:var(--ink); font-size:1.22rem;
  margin:1.6rem 0 .5rem; }
.prose a { color:var(--blue); text-decoration:none; }
.prose a:hover { text-decoration:underline; }
.prose strong { color:var(--ink); font-weight:700; }
.prose em { font-style:italic; }
.prose blockquote { margin:1.7rem 0; padding:.3rem 0 .3rem 1.4rem; border-left:3px solid var(--bronze);
  font-style:italic; color:var(--ink); }
.prose ul, .prose ol { margin:0 0 1.15rem; padding-left:1.4rem; }
.prose li { margin-bottom:.4rem; }
.prose li::marker { color:var(--bronze); }
.prose img { max-width:100%; height:auto; border-radius:4px; }
.prose code { font-family:ui-monospace, Menlo, Consolas, monospace; font-size:.86em;
  background:rgba(0,0,0,.06); padding:.12em .38em; border-radius:3px; }
.prose pre { background:var(--ink); color:#EDE8CB; padding:1.1rem 1.2rem; border-radius:var(--radius-sm);
  overflow:auto; font-size:.86rem; line-height:1.6; margin:0 0 1.3rem; }
.prose pre code { background:transparent; padding:0; color:inherit; }
.prose hr { border:0; border-top:1px solid var(--line); margin:2rem 0; }
.article--wide .prose { max-width:none; }
.article__foot { margin-top:2.4rem; padding-top:1.4rem; border-top:1px solid var(--line); }
#fp-comments:empty { display:none; }
/* Colophon */
.colophon { background:var(--paper); border-top:1px solid var(--line); padding:26px 75px 30px;
  text-align:center; font-family:var(--ff-ui); font-size:.78rem; color:var(--muted); }
.colophon a { color:var(--bronze-deep); text-decoration:none; }
.colophon a:hover { text-decoration:underline; }
@media (max-width:900px) { main { padding:28px 28px 20px; } .colophon { padding:24px 28px 28px; } }
@media (prefers-reduced-motion:reduce) { * { transition:none !important; } }
{% endraw %}</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
{% if preview_status %}<div class="preview-bar" role="status"><span class="preview-bar__tag">Preview</span><span class="preview-bar__msg">{{ preview_status }} &middot; a private draft, not the public page</span></div>
{% endif %}<div class="page">
  <header class="masthead">
    {% if site.logo %}<a href="/" class="brand brand--logo"><img src="{{ site.logo }}" alt="{{ site.title }}"></a>
    {% else %}<a href="/" class="brand">{{ site.title }}</a>
    {% endif %}{% if site.tagline %}<p class="brand__tagline">{{ site.tagline }}</p>{% endif %}
  </header>
  <nav class="nav" aria-label="Primary">
    <ul>
      <li><a href="/" {% if is_home %}aria-current="page"{% endif %}>Front page</a></li>
    </ul>
    <div id="fp-search" class="nav__search"></div>
  </nav>
  <main id="main">
{% block main %}{% endblock %}
  </main>
  <footer class="colophon">
    &copy; {{ site.title }} &middot; <a href="/feed.xml">Feed</a> &middot; Set in Ferropress
  </footer>
</div>
<script type="module">
import init from '/_fp/islands/ferropress_islands.js';
init({ module_or_path: '/_fp/islands/ferropress_islands_bg.wasm' });
</script>
</body>
</html>
"##;

/// Fellstone single post/page: title, dateline/kicker, optional byline (posts) +
/// featured image, the rendered body, and the comments island mount.
pub const FELLSTONE_SINGLE_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <article class="article">
      <header class="article__head">
        <h1 class="article__title">{{ title }}</h1>
        {% if dateline or kicker %}<p class="article__meta">{% if dateline %}{{ dateline }}{% endif %}{% if dateline and kicker %} &middot; {% endif %}{% if kicker %}{{ kicker }}{% endif %}</p>{% endif %}
        {% if author %}<p class="article__byline"><span class="byline__avatar" aria-hidden="true">{{ author_initials }}</span> by {{ author }}</p>{% endif %}
      </header>
      {% if featured_image %}<figure class="figure"><img src="{{ featured_image }}" alt="{{ title }}" loading="lazy"></figure>{% endif %}
      <div class="prose">{{ body | safe }}</div>
      <div class="article__foot">
        {% if not preview_status %}<div id="fp-comments"></div>{% endif %}
      </div>
    </article>
{% endblock %}
"##;

/// Fellstone front page: a galley of published posts (newest first), each a
/// dateline/byline line + headline link + excerpt.
pub const FELLSTONE_HOME_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <p class="eyebrow">Latest posts</p>
    {% if posts %}
    <ol class="galley">
      {% for post in posts %}
      <li class="entry">
        {% if post.dateline or post.author %}<p class="entry__meta">{% if post.dateline %}{{ post.dateline }}{% endif %}{% if post.dateline and post.author %} &middot; {% endif %}{% if post.author %}{{ post.author }}{% endif %}</p>{% endif %}
        <h2 class="entry__title"><a href="{{ post.url }}">{{ post.title }}</a></h2>
        {% if post.excerpt %}<p class="entry__excerpt">{{ post.excerpt }}</p>{% endif %}
        <a class="entry__more" href="{{ post.url }}">More&hellip;</a>
      </li>
      {% endfor %}
    </ol>
    {% else %}
    <p class="empty">No posts yet.</p>
    {% endif %}
{% endblock %}
"##;

/// Fellstone full-width **page** template: structurally identical to the single
/// template and consuming the identical context, differing only in the article's
/// `article--wide` class (the body breaks out of the reading measure).
pub const FELLSTONE_PAGE_WIDE_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <article class="article article--wide">
      <header class="article__head">
        <h1 class="article__title">{{ title }}</h1>
        {% if dateline or kicker %}<p class="article__meta">{% if dateline %}{{ dateline }}{% endif %}{% if dateline and kicker %} &middot; {% endif %}{% if kicker %}{{ kicker }}{% endif %}</p>{% endif %}
        {% if author %}<p class="article__byline"><span class="byline__avatar" aria-hidden="true">{{ author_initials }}</span> by {{ author }}</p>{% endif %}
      </header>
      {% if featured_image %}<figure class="figure"><img src="{{ featured_image }}" alt="{{ title }}" loading="lazy"></figure>{% endif %}
      <div class="prose">{{ body | safe }}</div>
      <div class="article__foot">
        {% if not preview_status %}<div id="fp-comments"></div>{% endif %}
      </div>
    </article>
{% endblock %}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use ferropress_render_form::{THEME_FELLSTONE, THEME_LETTERPRESS};
    use serde_json::json;

    /// A SingleCtx-shaped context (see `content.rs`), as JSON so the test needn't
    /// reach the private ctx structs — MiniJinja resolves dotted access into it.
    fn single_ctx() -> serde_json::Value {
        json!({
            "page_title": "The Sleeping Doll — Fellstone Tales",
            "page_description": "A book review",
            "canonical": null,
            "site": {"title": "Fellstone Tales", "tagline": "", "url": "https://x", "logo": null, "noindex": false},
            "is_home": false,
            "preview_status": null,
            "title": "The Sleeping Doll",
            "dateline": "July 20, 2026",
            "kicker": null,
            "author": "Liam Kincaid",
            "author_initials": "LK",
            "featured_image": null,
            "body": "<p>Hello <strong>world</strong>.</p>"
        })
    }

    /// A HomeCtx-shaped context with one galley row.
    fn home_ctx() -> serde_json::Value {
        json!({
            "page_title": "Fellstone Tales",
            "page_description": null,
            "site": {"title": "Fellstone Tales", "tagline": "", "url": "", "logo": null, "noindex": false},
            "is_home": true,
            "preview_status": null,
            "posts": [{
                "title": "The Sleeping Doll",
                "url": "/the-sleeping-doll",
                "excerpt": "An excerpt.",
                "dateline": "July 20, 2026",
                "author": "Liam Kincaid"
            }]
        })
    }

    #[test]
    fn fellstone_theme_renders_single_and_home() {
        let theme = build_theme(THEME_FELLSTONE).expect("fellstone builds");

        let single = theme
            .render("single.html", &single_ctx())
            .expect("single renders");
        assert!(single.contains("Gowun Batang"), "fellstone display font");
        assert!(single.contains("class=\"article__title\""));
        assert!(single.contains("The Sleeping Doll"));
        assert!(
            single.contains("<strong>world</strong>"),
            "pre-rendered body is emitted verbatim via | safe"
        );

        let home = theme
            .render("home.html", &home_ctx())
            .expect("home renders");
        assert!(home.contains("class=\"entry__title\""));
        // The permalink appears in the galley link. (The leading `/` is HTML-escaped
        // to `&#x2f;` by MiniJinja's `.html` autoescape — as in the letterpress theme
        // — so match the slug substring, which is escaping-agnostic.)
        assert!(home.contains("the-sleeping-doll"));
    }

    #[test]
    fn unknown_theme_falls_back_to_the_default() {
        // A bogus / stale `appearance.theme` must never fail a build — it resolves to
        // the default (letterpress), whose galley chrome differs from fellstone's.
        let fallback = build_theme("does-not-exist").expect("fallback builds");
        let letterpress = build_theme(THEME_LETTERPRESS).expect("letterpress builds");
        let fb = fallback.render("home.html", &home_ctx()).unwrap();
        let lp = letterpress.render("home.html", &home_ctx()).unwrap();
        assert_eq!(fb, lp, "unknown id renders exactly the default theme");
        // …and the default is NOT fellstone (no Gowun Batang display face).
        assert!(!fb.contains("Gowun Batang"));
    }
}
