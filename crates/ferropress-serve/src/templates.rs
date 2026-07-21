//! The built-in public theme's MiniJinja templates + stylesheet, ported from the
//! HTML-first design mockup (`crates/ferropress-theme/design/mockup.html`,
//! CLAUDE.md §2) — the reading counterpart to the admin's letterpress
//! "composing room" language.
//!
//! Three templates share one chrome: [`BASE_SRC`] holds the `<head>`, the
//! masthead (site title/tagline + island search mount), the colophon footer, the
//! inline stylesheet, and the island boot script; [`SINGLE_SRC`] and
//! [`HOME_SRC`] `{% extends %}` it and fill the `main` block. The chrome reads
//! **live** site settings (`site.*`) so a settings change needs no page
//! regeneration.
//!
//! The stylesheet is wrapped in `{% raw %}` so its CSS braces are never mistaken
//! for MiniJinja delimiters. The block body arrives already rendered by
//! `ferropress-render`; the template only frames it (marked `| safe`).

/// Template name for the shared chrome.
pub const BASE_TEMPLATE: &str = "base.html";
/// Template name for a single post/page (the default).
pub const SINGLE_TEMPLATE: &str = "single.html";
/// Template name for the front-page galley (post list).
pub const HOME_TEMPLATE: &str = "home.html";
/// Template name for the full-width **page** template (an alternate a Page may select).
pub const PAGE_WIDE_TEMPLATE: &str = "page-wide.html";

/// The page templates a Page author may choose from, as `(value, label)`. The empty value is
/// the default (the shared [`SINGLE_TEMPLATE`]); each other value is a registered alternate.
/// The admin editor renders these in its Template `<select>`, the page save validates the
/// submitted value against this set, and [`template_name_for`] maps a stored value to the
/// MiniJinja template to render. Adding a theme template means adding a row here + registering
/// it in [`default_theme`](crate::content::default_theme) + mapping it in [`template_name_for`].
pub fn page_templates() -> &'static [(&'static str, &'static str)] {
    &[("", "Default"), ("page-wide", "Full width")]
}

/// Map a stored Page `template` value to the registered MiniJinja template NAME to render.
/// The empty value, `None`, OR an unknown value all fall back to the default single template —
/// so a theme change that drops a template degrades gracefully (a stale `template` scalar
/// becomes a harmless dead reference) rather than erroring at compose time.
pub fn template_name_for(value: Option<&str>) -> &'static str {
    match value {
        Some("page-wide") => PAGE_WIDE_TEMPLATE,
        _ => SINGLE_TEMPLATE,
    }
}

/// The shared chrome: `<head>` (title/description/robots/canonical), the letterpress
/// masthead (live `site.title`/`site.tagline` + the `#fp-search` island mount), the
/// colophon, the inline stylesheet, and the island boot script. Child templates fill
/// `{% block main %}`.
pub const BASE_SRC: &str = r##"<!doctype html>
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
@import url('https://fonts.googleapis.com/css2?family=Zilla+Slab:ital,wght@0,400;0,500;0,600;0,700;1,400;1,500&family=IBM+Plex+Sans:wght@400;500;600&family=IBM+Plex+Mono:wght@400;500&display=swap');
:root {
  --paper: #EEEFEA; --paper-sink: #E5E6E0; --paper-raise: #F5F6F1;
  --ink: #17191C; --ink-2: #383B40; --steel: #6B6F76; --steel-2: #9CA0A6;
  --rule: #D3D4CD; --rule-ink: #2C2F34;
  --minium: #C23A18; --minium-hi: #D6431E; --minium-deep: #9A2E11;
  --measure: 34rem; --radius: 3px;
  --shadow: 0 1px 0 var(--rule), 0 12px 32px -20px rgba(23,25,28,.4);
  --ff-display: "Zilla Slab", Georgia, serif;
  --ff-ui: "IBM Plex Sans", system-ui, sans-serif;
  --ff-mono: "IBM Plex Mono", ui-monospace, monospace;
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0; background: var(--paper); color: var(--ink-2);
  font-family: var(--ff-ui); font-size: 16px; line-height: 1.5;
  -webkit-font-smoothing: antialiased; text-rendering: optimizeLegibility;
}
a { color: inherit; }
.ruler { height: 6px; opacity: .5;
  background-image: repeating-linear-gradient(90deg, var(--rule-ink) 0 1px, transparent 1px 12px); }
.regmark { display: inline-flex; align-items: center; color: var(--minium); }
.regmark svg { display: block; }
.gauge { width: 100%; max-width: 52rem; margin: 0 auto; padding: 0 1.5rem; }
.skip { position: absolute; left: -999px; top: 0; }
.skip:focus { left: 1rem; top: .6rem; background: var(--ink); color: var(--paper);
  padding: .5rem .8rem; border-radius: var(--radius); z-index: 60; }
/* Draft-preview banner (authenticated new-tab preview only) */
.preview-bar { position: sticky; top: 0; z-index: 70; display: flex; align-items: center;
  justify-content: center; gap: .7rem; flex-wrap: wrap; padding: .5rem 1rem;
  background: var(--minium); color: #fff; font-family: var(--ff-mono); font-size: .72rem;
  letter-spacing: .1em; text-transform: uppercase; }
.preview-bar__tag { font-weight: 600; letter-spacing: .18em;
  border: 1px solid rgba(255,255,255,.55); padding: .12rem .45rem; border-radius: 2px; }
.preview-bar__msg { color: rgba(255,255,255,.9); letter-spacing: .07em; }
/* Masthead */
.masthead { padding: 2.6rem 0 0; text-align: center; }
.masthead__mark { display: flex; justify-content: center; margin-bottom: .9rem; }
.nameplate { font-family: var(--ff-display); font-weight: 700;
  font-size: clamp(2rem, 6vw, 3.1rem); line-height: 1; letter-spacing: .04em;
  text-transform: uppercase; color: var(--ink); margin: 0; text-decoration: none; display: inline-block; }
.nameplate:hover { color: var(--minium-deep); }
.nameplate--logo { padding: 0; line-height: 0; }
.nameplate--logo img { display: block; width: auto; height: auto;
  max-height: 4.5rem; max-width: min(100%, 22rem); }
.tagline { font-family: var(--ff-mono); font-size: .74rem; letter-spacing: .26em;
  text-transform: uppercase; color: var(--steel); margin: .85rem 0 0; }
.tagline::before, .tagline::after { content: "—"; margin: 0 .6rem; color: var(--steel-2); }
.mastnav { display: flex; align-items: center; justify-content: center; flex-wrap: wrap;
  gap: 1.2rem; margin: 1.5rem 0 1.3rem; }
.mastnav a { font-family: var(--ff-ui); font-size: .78rem; font-weight: 600;
  letter-spacing: .09em; text-transform: uppercase; color: var(--steel); text-decoration: none;
  padding: .15rem 0; border-bottom: 1.5px solid transparent; transition: color .14s, border-color .14s; }
.mastnav a:hover { color: var(--ink); border-bottom-color: var(--minium); }
.mastnav a[aria-current="page"] { color: var(--ink); border-bottom-color: var(--ink); }
/* Nav menus (masthead primary + footer) — a flat bar of links with hover/focus submenus. */
.mastnav .navtree { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap;
  align-items: center; justify-content: center; gap: 1.2rem; }
.mastnav .navtree li { position: relative; }
.navtree__label { font-family: var(--ff-ui); font-size: .78rem; font-weight: 600;
  letter-spacing: .09em; text-transform: uppercase; color: var(--steel-2); padding: .15rem 0; }
.mastnav .navtree .navtree { position: absolute; top: 100%; left: 50%; transform: translateX(-50%);
  flex-direction: column; align-items: flex-start; gap: .6rem; min-width: 11rem; margin-top: .55rem;
  padding: .75rem .95rem; background: var(--paper-raise); border: 1px solid var(--rule);
  border-radius: var(--radius); box-shadow: var(--shadow); display: none; z-index: 50; }
.mastnav .navtree li:hover > .navtree, .mastnav .navtree li:focus-within > .navtree { display: flex; }
.colophon-nav .navtree { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap;
  align-items: center; justify-content: center; gap: 1.3rem; }
.colophon-nav .navtree a { font-family: var(--ff-mono); font-size: .72rem; font-weight: 500;
  letter-spacing: .1em; text-transform: uppercase; color: var(--steel); text-decoration: none; }
.colophon-nav .navtree a:hover { color: var(--minium-deep); }
.colophon-nav .navtree .navtree { gap: .5rem; }
#fp-search:not(:empty) { display: inline-flex; }
/* Front page — the galley */
main { padding: 2.4rem 0 3rem; }
.eyebrow { font-family: var(--ff-mono); font-size: .7rem; font-weight: 500;
  letter-spacing: .22em; text-transform: uppercase; color: var(--steel-2);
  display: flex; align-items: center; gap: .8rem; margin: 0 0 1.6rem; }
.eyebrow::after { content: ""; flex: 1; height: 1px; background: var(--rule); }
.galley { list-style: none; margin: 0; padding: 0; }
.proof { padding: 1.9rem 0; border-bottom: 1px solid var(--rule); }
.proof:first-child { padding-top: 0; }
.slugline { font-family: var(--ff-mono); font-size: .72rem; font-weight: 500;
  letter-spacing: .12em; text-transform: uppercase; color: var(--steel);
  display: inline-flex; align-items: center; gap: .55rem; margin-bottom: .6rem; }
.slugline b { color: var(--minium-deep); font-weight: 500; }
.slugline .dot { width: 3px; height: 3px; border-radius: 50%; background: var(--steel-2); }
.proof__title { margin: 0 0 .5rem; }
.proof__title a { font-family: var(--ff-display); font-weight: 600; font-size: 1.72rem;
  line-height: 1.16; letter-spacing: .003em; color: var(--ink); text-decoration: none;
  background-image: linear-gradient(var(--minium), var(--minium)); background-size: 0% 2px;
  background-position: 0 100%; background-repeat: no-repeat; transition: background-size .22s ease; }
.proof__title a:hover { background-size: 100% 2px; color: var(--minium-deep); }
.proof__excerpt { font-family: var(--ff-ui); font-size: 1rem; line-height: 1.6;
  color: var(--ink-2); margin: 0; max-width: var(--measure); }
.proof__more { display: inline-block; margin-top: .7rem; font-family: var(--ff-mono);
  font-size: .74rem; font-weight: 500; letter-spacing: .1em; text-transform: uppercase;
  color: var(--steel); text-decoration: none; }
.proof__more:hover { color: var(--minium-deep); }
.leaf { display: flex; align-items: center; justify-content: space-between;
  margin-top: 2.2rem; padding-top: 1.4rem; }
.leaf a, .leaf span { font-family: var(--ff-mono); font-size: .76rem; font-weight: 500;
  letter-spacing: .1em; text-transform: uppercase; text-decoration: none; }
.leaf a { color: var(--ink-2); }
.leaf a:hover { color: var(--minium-deep); }
.leaf .spent { color: var(--steel-2); }
.empty { padding: 3rem 0; text-align: center; font-family: var(--ff-mono);
  font-size: .8rem; letter-spacing: .1em; text-transform: uppercase; color: var(--steel-2); }
/* Single post — the proof */
.article { max-width: var(--measure); margin: 0 auto; }
/* Full-width page template: the article breaks out of the reading gauge to span the
   viewport, then re-centers its body at a wider comfortable measure. */
.article--wide { width: 100vw; max-width: 100vw; margin-left: calc(50% - 50vw);
  margin-right: calc(50% - 50vw); padding: 0 1.5rem; }
.article--wide .article__head, .article--wide .figure, .article--wide .proof-body,
.article--wide .article__foot { max-width: 60rem; margin-left: auto; margin-right: auto; }
.article__head { margin-bottom: 1.8rem; }
.article__title { font-family: var(--ff-display); font-weight: 700;
  font-size: clamp(2rem, 5vw, 2.7rem); line-height: 1.1; letter-spacing: .004em;
  color: var(--ink); margin: .7rem 0 0; }
.byline { display: flex; align-items: center; gap: .6rem; margin-top: 1.1rem;
  font-family: var(--ff-mono); font-size: .74rem; letter-spacing: .08em;
  text-transform: uppercase; color: var(--steel); }
.byline__avatar { width: 26px; height: 26px; border-radius: 50%; background: var(--minium);
  color: #fff; display: grid; place-items: center; font-weight: 600; font-size: .74rem;
  font-family: var(--ff-ui); letter-spacing: 0; }
.figure { margin: 0 0 2rem; }
.figure img { width: 100%; height: auto; display: block;
  border: 1px solid var(--rule); border-radius: var(--radius); }
.proof-body { font-family: var(--ff-display); color: var(--ink); }
.proof-body h2 { font-size: 1.4rem; font-weight: 600; margin: 2.2rem 0 .6rem; color: var(--ink); }
.proof-body h3 { font-size: 1.15rem; font-weight: 600; margin: 1.6rem 0 .5rem; color: var(--ink); }
.proof-body p { font-size: 1.14rem; line-height: 1.66; margin: 0 0 1.15rem; color: var(--ink-2); }
.proof-body a { color: var(--minium-deep); text-underline-offset: 2px; text-decoration-thickness: 1px; }
.proof-body strong { font-weight: 700; color: var(--ink); }
.proof-body em { font-style: italic; }
.proof-body blockquote { margin: 1.7rem 0; padding: .3rem 0 .3rem 1.4rem;
  border-left: 3px solid var(--minium); font-size: 1.24rem; font-style: italic; color: var(--ink); }
.proof-body ul, .proof-body ol { margin: 0 0 1.15rem; padding-left: 1.4rem; }
.proof-body li { font-size: 1.14rem; line-height: 1.6; margin-bottom: .35rem; }
.proof-body li::marker { color: var(--minium); }
.proof-body img { max-width: 100%; height: auto; border: 1px solid var(--rule); border-radius: var(--radius); }
.proof-body code { font-family: var(--ff-mono); font-size: .86em; background: var(--paper-sink);
  padding: .12em .38em; border-radius: 2px; }
.proof-body pre { background: var(--ink); color: #E7E8E2; padding: 1.1rem 1.2rem;
  border-radius: var(--radius); overflow: auto; font-size: .86rem; line-height: 1.6; margin: 0 0 1.3rem; }
.proof-body pre code { background: transparent; padding: 0; color: inherit; }
.proof-body hr { border: 0; border-top: 1px solid var(--rule); margin: 2rem 0; }
.article__foot { max-width: var(--measure); margin: 2.6rem auto 0; }
.press-rule { height: 6px; margin-bottom: 1.6rem; opacity: .4;
  background-image: repeating-linear-gradient(90deg, var(--rule-ink) 0 1px, transparent 1px 12px); }
/* Colophon */
.colophon { margin-top: 3rem; padding: 2rem 0 3rem; }
.colophon__inner { display: flex; align-items: center; justify-content: space-between;
  flex-wrap: wrap; gap: 1rem; padding-top: 1.6rem; font-family: var(--ff-mono);
  font-size: .72rem; letter-spacing: .1em; text-transform: uppercase; color: var(--steel); }
.colophon a { color: var(--steel); text-decoration: none; }
.colophon a:hover { color: var(--minium-deep); }
.colophon__mark { display: inline-flex; align-items: center; gap: .5rem; }
@media (max-width: 640px) {
  .mastnav { gap: 1rem; }
  .proof__title a { font-size: 1.45rem; }
  .colophon__inner { justify-content: center; text-align: center; }
}
@media (prefers-reduced-motion: reduce) { * { transition: none !important; } }
{% endraw %}</style>
</head>
<body>
{%- macro navtree(items) -%}<ul class="navtree">{% for item in items %}<li>{% if item.href %}<a href="{{ item.href }}"{% if item.aria_current %} aria-current="page"{% endif %}{% if item.new_tab %} target="_blank" rel="noopener noreferrer"{% endif %}>{{ item.label }}</a>{% else %}<span class="navtree__label">{{ item.label }}</span>{% endif %}{% if item.children %}{{ navtree(item.children) }}{% endif %}</li>{% endfor %}</ul>{%- endmacro -%}
<a class="skip" href="#main">Skip to content</a>
{% if preview_status %}<div class="preview-bar" role="status"><span class="preview-bar__tag">Preview</span><span class="preview-bar__msg">{{ preview_status }} &middot; a private draft, not the public page</span></div>
{% endif %}<div class="ruler" aria-hidden="true"></div>
<header class="masthead">
  <div class="gauge">
    <div class="masthead__mark" aria-hidden="true">
      <span class="regmark"><svg width="20" height="20" viewBox="0 0 20 20" fill="none"><circle cx="10" cy="10" r="6.2" stroke="currentColor" stroke-width="1.4"/><path d="M10 0v6.2M10 13.8V20M0 10h6.2M13.8 10H20" stroke="currentColor" stroke-width="1.4"/></svg></span>
    </div>
    {% if site.logo %}<a href="/" class="nameplate nameplate--logo"><img src="{{ site.logo }}" alt="{{ site.title }}"></a>
    {% else %}<a href="/" class="nameplate">{{ site.title }}</a>
    {% endif %}{% if site.tagline %}<p class="tagline">{{ site.tagline }}</p>{% endif %}
    <nav class="mastnav" aria-label="Primary">
      {% if nav.primary %}{{ navtree(nav.primary) }}{% else %}<a href="/" {% if is_home %}aria-current="page"{% endif %}>Front page</a>{% endif %}
      <div id="fp-search"></div>
    </nav>
  </div>
</header>
<div class="ruler" aria-hidden="true"></div>
<main id="main">
  <div class="gauge">
{% block main %}{% endblock %}
  </div>
</main>
<footer class="colophon">
  <div class="ruler" aria-hidden="true" style="opacity:.35"></div>
  <div class="gauge">
    {% if nav.footer %}<nav class="colophon-nav" aria-label="Footer">{{ navtree(nav.footer) }}</nav>
    {% else %}<div class="colophon__inner">
      <span class="colophon__mark"><span class="regmark"><svg width="13" height="13" viewBox="0 0 20 20" fill="none"><circle cx="10" cy="10" r="6.2" stroke="currentColor" stroke-width="1.6"/><path d="M10 1.5v5M10 13.5v5M1.5 10h5M13.5 10h5" stroke="currentColor" stroke-width="1.6"/></svg></span> &copy; {{ site.title }}</span>
      <span>Set in Ferropress</span>
    </div>{% endif %}
  </div>
</footer>
<script type="module">
import init from '/_fp/islands/ferropress_islands.js';
init({ module_or_path: '/_fp/islands/ferropress_islands_bg.wasm' });
</script>
</body>
</html>
"##;

/// A single post/page: the dateline slug-line, headline, optional byline (posts
/// only) + featured image, the rendered proof body, and the comments island mount.
pub const SINGLE_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <article>
      <header class="article__head">
        {% if dateline or kicker %}<span class="slugline">{% if dateline %}<b>{{ dateline }}</b>{% endif %}{% if dateline and kicker %}<span class="dot"></span>{% endif %}{% if kicker %}{{ kicker }}{% endif %}</span>{% endif %}
        <h1 class="article__title">{{ title }}</h1>
        {% if author %}<div class="byline"><span class="byline__avatar" aria-hidden="true">{{ author_initials }}</span> By {{ author }}</div>{% endif %}
      </header>
      {% if featured_image %}<figure class="figure"><img src="{{ featured_image }}" alt="{{ title }}" loading="lazy"></figure>{% endif %}
      <div class="proof-body">{{ body | safe }}</div>
      <div class="article__foot">
        <div class="press-rule" aria-hidden="true"></div>
        {% if not preview_status %}<div id="fp-comments"></div>{% endif %}
      </div>
    </article>
{% endblock %}
"##;

/// The front page: a galley of published posts (newest first), each a slug-line +
/// headline link + excerpt. Falls back to an empty-state line when there are none.
pub const HOME_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <p class="eyebrow">Latest from the galley</p>
    {% if posts %}
    <ol class="galley">
      {% for post in posts %}
      <li class="proof">
        <span class="slugline">{% if post.dateline %}<b>{{ post.dateline }}</b>{% endif %}{% if post.dateline and post.author %}<span class="dot"></span>{% endif %}{% if post.author %}{{ post.author }}{% endif %}</span>
        <h2 class="proof__title"><a href="{{ post.url }}">{{ post.title }}</a></h2>
        {% if post.excerpt %}<p class="proof__excerpt">{{ post.excerpt }}</p>{% endif %}
        <a class="proof__more" href="{{ post.url }}">Read the proof &rarr;</a>
      </li>
      {% endfor %}
    </ol>
    {% else %}
    <p class="empty">Nothing set in type yet.</p>
    {% endif %}
{% endblock %}
"##;

/// The full-width **page** template ([`PAGE_WIDE_TEMPLATE`]): structurally identical to
/// [`SINGLE_SRC`] and consuming the IDENTICAL `SingleCtx`, differing only in the article's
/// `article--wide` class — so a Page that selects it breaks out of the reading gauge to a
/// full-width layout while every context variable (`is_home`, `preview_status`, byline,
/// dateline, featured image) composes exactly as on the default template.
pub const PAGE_WIDE_SRC: &str = r##"{% extends "base.html" %}
{% block main %}
    <article class="article--wide">
      <header class="article__head">
        {% if dateline or kicker %}<span class="slugline">{% if dateline %}<b>{{ dateline }}</b>{% endif %}{% if dateline and kicker %}<span class="dot"></span>{% endif %}{% if kicker %}{{ kicker }}{% endif %}</span>{% endif %}
        <h1 class="article__title">{{ title }}</h1>
        {% if author %}<div class="byline"><span class="byline__avatar" aria-hidden="true">{{ author_initials }}</span> By {{ author }}</div>{% endif %}
      </header>
      {% if featured_image %}<figure class="figure"><img src="{{ featured_image }}" alt="{{ title }}" loading="lazy"></figure>{% endif %}
      <div class="proof-body">{{ body | safe }}</div>
      <div class="article__foot">
        <div class="press-rule" aria-hidden="true"></div>
        {% if not preview_status %}<div id="fp-comments"></div>{% endif %}
      </div>
    </article>
{% endblock %}
"##;
