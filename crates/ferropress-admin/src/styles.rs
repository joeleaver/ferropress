//! One-time injection of the admin stylesheet — the letterpress "composing room"
//! design, ported from the validated HTML-first mockup
//! (`crates/ferropress-admin/design/mockup.html`, CLAUDE.md §2).
//!
//! One thing differs from the mockup, by construction: view switching is rinch's
//! reactive `match` (one view in the DOM at a time), so there is no
//! `.view { display: none }` toggle to get wrong.
//!
//! Status is the mockup's native `<select>` (rinch#95 delivers its change via
//! `oninput`), and the title is an in-sheet headline input (`.sheet__title`).
//!
//! The editor content (`[data-pm-editor]`, projected by rinch-editor-view) is styled
//! under `.sheet` so those rules out-specify the editor's own injected defaults.

use web_sys::Document;

const STYLE_ID: &str = "fp-admin-style";

/// Inject the admin stylesheet into `<head>` exactly once (guarded by element id).
pub fn inject_admin_styles() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if doc.get_element_by_id(STYLE_ID).is_some() {
        return;
    }
    append_style(&doc);
}

fn append_style(doc: &Document) {
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    let _ = style.set_attribute("id", STYLE_ID);
    style.set_text_content(Some(ADMIN_CSS));
    if let Some(head) = doc.head() {
        let _ = head.append_child(&style);
    }
}

const ADMIN_CSS: &str = r#"
@import url('https://fonts.googleapis.com/css2?family=Zilla+Slab:wght@400;500;600;700&family=IBM+Plex+Sans:wght@400;500;600&family=IBM+Plex+Mono:wght@400;500&display=swap');

:root {
  --paper: #EEEFEA;
  --paper-sink: #E5E6E0;
  --paper-raise: #F5F6F1;
  --ink: #17191C;
  --ink-2: #383B40;
  --steel: #6B6F76;
  --steel-2: #9CA0A6;
  --rule: #D3D4CD;
  --rule-ink: #2C2F34;
  --minium: #C23A18;
  --minium-hi: #D6431E;
  --minium-deep: #9A2E11;
  --ochre: #8A6412;
  --green: #2E6A4E;
  --steel-tag: #566069;
  --measure: 34rem;
  --radius: 3px;
  --shadow: 0 1px 0 var(--rule), 0 12px 32px -20px rgba(23,25,28,.4);
  --ff-display: "Zilla Slab", Georgia, serif;
  --ff-ui: "IBM Plex Sans", system-ui, sans-serif;
  --ff-mono: "IBM Plex Mono", ui-monospace, monospace;
}

#rinch-body, .fp-admin { min-height: 100vh; }
.fp-admin * { box-sizing: border-box; }
.fp-admin {
  background: var(--paper);
  color: var(--ink-2);
  font-family: var(--ff-ui);
  font-size: 15px;
  line-height: 1.5;
  -webkit-font-smoothing: antialiased;
}
.fp-admin a { color: inherit; }
.fp-admin button { font-family: inherit; cursor: pointer; }

.ruler {
  height: 6px;
  background-image: repeating-linear-gradient(90deg, var(--rule-ink) 0 1px, transparent 1px 12px);
  opacity: .5;
}
.regmark { display: inline-flex; align-items: center; color: var(--minium); }
.regmark svg { display: block; }

/* ── LOGIN ─────────────────────────────────────────────────────────── */
.login { min-height: 100vh; display: grid; grid-template-rows: 6px 1fr auto; }
.login__stage { display: grid; place-items: center; padding: 2rem; }
.login__plate { width: 100%; max-width: 25rem; }
.brand { display: flex; align-items: center; gap: .6rem; justify-content: center; margin-bottom: .35rem; }
.brand__word {
  font-family: var(--ff-display); font-weight: 700; font-size: 2rem;
  letter-spacing: .06em; text-transform: uppercase; color: var(--ink); margin: 0;
}
.brand__sub {
  text-align: center; font-family: var(--ff-mono); font-size: .72rem;
  letter-spacing: .28em; text-transform: uppercase; color: var(--steel); margin: 0 0 1.75rem;
}
.brand__sub::before, .brand__sub::after { content: "—"; margin: 0 .5rem; color: var(--steel-2); }

.card {
  background: var(--paper-raise); border: 1px solid var(--rule);
  border-radius: var(--radius); box-shadow: var(--shadow); padding: 1.75rem;
}
.field { margin-bottom: 1.1rem; }
.field:last-of-type { margin-bottom: 1.5rem; }
.field > label {
  display: block; font-size: .72rem; font-weight: 600; letter-spacing: .1em;
  text-transform: uppercase; color: var(--steel); margin-bottom: .4rem;
}
.input {
  width: 100%; padding: .6rem .7rem; background: var(--paper-sink);
  border: 1px solid var(--rule); border-bottom: 2px solid #C6C7C0;
  border-radius: var(--radius); color: var(--ink); font-family: var(--ff-ui);
  font-size: .95rem; transition: border-color .15s, background .15s;
}
.input--mono { font-family: var(--ff-mono); }
.input::placeholder { color: var(--steel-2); }
.input:focus {
  outline: none; background: var(--paper-raise);
  border-color: var(--steel); border-bottom-color: var(--minium);
}

.btn {
  display: inline-flex; align-items: center; justify-content: center; gap: .5rem;
  padding: .62rem 1.1rem; border: 1px solid transparent; border-radius: var(--radius);
  font-weight: 600; font-size: .92rem; letter-spacing: .02em;
  transition: background .15s, transform .06s, box-shadow .15s, color .15s, border-color .15s;
}
.btn:active { transform: translateY(1px); }
.btn--primary {
  width: 100%; background: var(--minium); color: #FCF6F2;
  box-shadow: inset 0 1px 0 rgba(255,255,255,.18), 0 1px 0 var(--minium-deep);
}
.btn--primary:hover { background: var(--minium-hi); }
.btn--primary[disabled] { opacity: .65; cursor: default; }
.btn--ghost { background: transparent; color: var(--ink-2); border-color: var(--rule); }
.btn--ghost:hover { background: var(--paper-sink); border-color: var(--steel-2); }
.btn--quiet { background: transparent; color: var(--steel); padding: .4rem .5rem; }
.btn--quiet:hover { color: var(--ink); }

.login__footer {
  text-align: center; padding: 1.5rem; font-family: var(--ff-mono);
  font-size: .72rem; letter-spacing: .16em; text-transform: uppercase; color: var(--steel-2);
}
.note { margin: .2rem 0 1rem; font-size: .82rem; color: var(--minium-deep); }

/* ── MASTHEAD ─────────────────────────────────────────────────────── */
.masthead { background: var(--ink); color: #E7E8E2; border-bottom: 1px solid #000; }
.masthead__bar {
  display: flex; align-items: center; gap: 1rem; padding: 0 1.25rem;
  height: 52px; max-width: 68rem; margin: 0 auto;
}
.masthead__brand {
  display: flex; align-items: center; gap: .5rem; font-family: var(--ff-display);
  font-weight: 700; letter-spacing: .08em; text-transform: uppercase; font-size: 1.02rem; color: #F3F4EE;
}
.masthead__sep { color: #4A4D53; }
.masthead__here { color: #A9ADB2; font-weight: 500; font-size: .9rem; }
.masthead__spacer { flex: 1; }
.masthead__user { display: flex; align-items: center; gap: .5rem; color: #C7CACF; font-size: .88rem; }
.masthead__avatar {
  width: 24px; height: 24px; border-radius: 50%; background: var(--minium); color: #fff;
  display: grid; place-items: center; font-weight: 600; font-size: .72rem; text-transform: uppercase;
}
.masthead .btn--quiet { color: #A9ADB2; }
.masthead .btn--quiet:hover { color: #fff; }

.wrap { max-width: 68rem; margin: 0 auto; padding: 2rem 1.25rem 4rem; }

/* ── POST LIST (GALLEY) ───────────────────────────────────────────── */
.galley__head {
  display: flex; align-items: baseline; justify-content: space-between;
  border-bottom: 2px solid var(--ink); padding-bottom: .6rem; margin-bottom: .25rem;
}
.galley__title { font-family: var(--ff-display); font-weight: 600; font-size: 1.7rem; color: var(--ink); margin: 0; }
.galley__count { font-family: var(--ff-mono); font-size: .78rem; letter-spacing: .1em; text-transform: uppercase; color: var(--steel); }
.galley__state { padding: 1.2rem .25rem; color: var(--steel); }
.galley__state.err { color: var(--minium-deep); }

.row {
  display: grid; grid-template-columns: 22px 1fr auto auto auto; align-items: center; gap: 1rem;
  padding: .85rem .5rem .85rem .25rem; border-bottom: 1px solid var(--rule);
  text-decoration: none; color: inherit; width: 100%; background: transparent; border-left: 0; border-right: 0; border-top: 0;
  text-align: left; font: inherit; transition: background .12s;
}
.row:hover { background: var(--paper-raise); }
.row__mark { color: var(--minium); opacity: 0; transition: opacity .12s; }
.row:hover .row__mark { opacity: 1; }
.row__title { font-family: var(--ff-display); font-weight: 500; font-size: 1.12rem; color: var(--ink); line-height: 1.25; }
.row__slug { display: block; font-family: var(--ff-mono); font-size: .78rem; color: var(--steel); margin-top: .1rem; }
.row__time { font-family: var(--ff-mono); font-size: .78rem; color: var(--steel); white-space: nowrap; }
.row__edit { font-size: .82rem; font-weight: 600; color: var(--steel); display: inline-flex; align-items: center; gap: .3rem; }
.row:hover .row__edit { color: var(--minium-deep); }

.stamp {
  font-family: var(--ff-mono); font-size: .68rem; font-weight: 500; letter-spacing: .12em;
  text-transform: uppercase; padding: .18rem .5rem; border-radius: 2px; border: 1.5px solid currentColor; white-space: nowrap;
}
.stamp--published { color: var(--green); }
.stamp--draft { color: var(--ochre); border-style: dashed; }
.stamp--private { color: var(--steel-tag); }

/* ── EDITOR (THE SHEET) ───────────────────────────────────────────── */
.editor__meta {
  display: flex; align-items: center; gap: 1.5rem; flex-wrap: wrap;
  padding-bottom: 1rem; margin-bottom: 1rem; border-bottom: 1px solid var(--rule);
}
.metaitem { display: flex; align-items: center; gap: .55rem; }
.metaitem > label { font-size: .7rem; font-weight: 600; letter-spacing: .1em; text-transform: uppercase; color: var(--steel); }
.slugbox {
  display: inline-flex; align-items: center; background: var(--paper-sink);
  border: 1px solid var(--rule); border-radius: var(--radius); padding: .3rem .5rem;
  font-family: var(--ff-mono); font-size: .85rem; color: var(--ink);
}
.slugbox__host { color: var(--steel-2); }
.slugbox input { border: 0; background: transparent; font: inherit; color: var(--ink); width: 16ch; padding: 0; }
.slugbox input:focus { outline: none; }

.select {
  font-family: var(--ff-mono); font-size: .82rem;
  padding: .35rem 1.6rem .35rem .6rem;
  border: 1px solid var(--rule); border-radius: var(--radius); color: var(--ink);
  background: var(--paper-sink)
    url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='10' height='7' viewBox='0 0 10 7'><path d='M1 1l4 4 4-4' stroke='%236B6F76' stroke-width='1.5' fill='none'/></svg>")
    no-repeat right .55rem center;
  appearance: none; -webkit-appearance: none;
}
.select:focus { outline: none; border-color: var(--minium); }

.toolbar {
  display: flex; align-items: center; gap: .15rem; flex-wrap: wrap;
  background: var(--paper-raise); border: 1px solid var(--rule); border-radius: var(--radius);
  padding: .3rem; margin: 0 auto 1rem; max-width: calc(var(--measure) + 4rem);
  position: sticky; top: .75rem; z-index: 5; box-shadow: var(--shadow);
}
.tool {
  min-width: 32px; height: 32px; padding: 0 .5rem; display: inline-flex; align-items: center; justify-content: center; gap: .3rem;
  background: transparent; border: 1px solid transparent; border-radius: 2px; color: var(--ink-2); font-size: .85rem; font-weight: 600;
}
.tool:hover { background: var(--paper-sink); }
.tool--label { font-family: var(--ff-mono); font-size: .78rem; }
.tool--i b { font-weight: 700; }
.tool--i i { font-style: italic; font-family: var(--ff-display); }
.toolbar__sep { width: 1px; height: 20px; background: var(--rule); margin: 0 .3rem; }
.toolbar__spacer { flex: 1; }
.toolbar__measure { font-family: var(--ff-mono); font-size: .68rem; letter-spacing: .1em; text-transform: uppercase; color: var(--steel-2); padding-right: .4rem; display: inline-flex; align-items: center; gap: .35rem; }

.sheet {
  background: var(--paper-raise); border: 1px solid var(--rule); border-radius: var(--radius);
  box-shadow: var(--shadow); padding: 3rem; max-width: calc(var(--measure) + 6rem); margin: 0 auto;
}
.sheet__inner { max-width: var(--measure); margin: 0 auto; }
.editor__error { max-width: calc(var(--measure) + 6rem); margin: 0 auto 1rem; color: var(--minium-deep); font-size: .88rem; text-align: center; }

/* The title is the headline on the sheet — a borderless input set in the display
   serif, matching the editor's own h1 so it reads as one continuous proof. */
.sheet__title {
  width: 100%; border: 0; background: transparent; padding: 0; margin: 0 0 1.25rem;
  font-family: var(--ff-display); font-weight: 700; font-size: 2.1rem; line-height: 1.12;
  letter-spacing: .005em; color: var(--ink);
}
.sheet__title::placeholder { color: var(--steel-2); font-weight: 700; }
.sheet__title:focus { outline: none; }

/* editor content — set in the reading serif, like a proof on paper. Scoped under
   .sheet so these out-specify rinch-editor-view's own injected defaults. */
.sheet [data-pm-editor] { font-family: var(--ff-display); color: var(--ink); outline: none; min-height: 12rem; }
.sheet [data-pm-editor] h1 { font-size: 2.1rem; font-weight: 700; line-height: 1.12; margin: 0 0 1rem; }
.sheet [data-pm-editor] h2 { font-size: 1.4rem; font-weight: 600; margin: 2rem 0 .6rem; }
.sheet [data-pm-editor] h3 { font-size: 1.15rem; font-weight: 600; margin: 1.5rem 0 .5rem; }
.sheet [data-pm-editor] p { font-size: 1.12rem; line-height: 1.62; margin: 0 0 1.1rem; color: var(--ink-2); }
.sheet [data-pm-editor] a { color: var(--minium-deep); text-underline-offset: 2px; }
.sheet [data-pm-editor] blockquote {
  margin: 1.6rem 0; padding: .2rem 0 .2rem 1.3rem; border-left: 3px solid var(--minium);
  font-size: 1.22rem; font-style: italic; color: var(--ink);
}
.sheet [data-pm-editor] ul, .sheet [data-pm-editor] ol { margin: 0 0 1.1rem; padding-left: 1.3rem; }
.sheet [data-pm-editor] li { font-size: 1.12rem; line-height: 1.55; margin-bottom: .3rem; }
.sheet [data-pm-editor] code { font-family: var(--ff-mono); font-size: .88em; background: var(--paper-sink); padding: .1em .35em; border-radius: 2px; }
.sheet [data-pm-editor] pre { background: var(--paper-sink); padding: 1rem; border-radius: var(--radius); overflow: auto; }
.sheet [data-pm-editor] pre code { background: transparent; padding: 0; }

/* ── TOAST ────────────────────────────────────────────────────────── */
.toast {
  position: fixed; left: 50%; bottom: 1.5rem; transform: translateX(-50%) translateY(260%);
  background: var(--ink); color: var(--paper); font-family: var(--ff-mono); font-size: .8rem;
  letter-spacing: .12em; text-transform: uppercase; padding: .6rem 1rem; border-radius: var(--radius);
  display: inline-flex; align-items: center; gap: .55rem; box-shadow: 0 12px 30px -12px rgba(0,0,0,.5);
  transition: transform .3s cubic-bezier(.2,.8,.2,1); z-index: 40;
}
.toast.is-shown { transform: translateX(-50%) translateY(0); }
.toast .regmark { color: var(--minium-hi); }

@media (max-width: 640px) {
  .row { grid-template-columns: 18px 1fr auto; }
  .row__time, .row__edit { display: none; }
  .sheet { padding: 1.5rem; }
  .sheet__title { font-size: 1.7rem; }
  .sheet [data-pm-editor] h1 { font-size: 1.7rem; }
}
@media (prefers-reduced-motion: reduce) {
  .fp-admin *, .toast { transition: none !important; }
}
"#;
