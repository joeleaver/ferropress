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
  display: grid; grid-template-columns: 36px 1fr auto auto auto; align-items: center; gap: 1rem;
  padding: .85rem .5rem .85rem .25rem; border-bottom: 1px solid var(--rule);
  text-decoration: none; color: inherit; width: 100%; background: transparent; border-left: 0; border-right: 0; border-top: 0;
  text-align: left; font: inherit; transition: background .12s;
}
.row:hover { background: var(--paper-raise); }
.row__mark { color: var(--minium); opacity: 0; transition: opacity .12s; }
.row:hover .row__mark { opacity: 1; }
.row__thumb { width: 32px; height: 32px; object-fit: cover; display: block; border: 1px solid var(--rule); border-radius: 2px; }
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
.featured { display: inline-flex; align-items: center; gap: .5rem; }
.featured__thumb { width: 56px; height: 56px; object-fit: cover; display: block; border: 1px solid var(--rule); border-radius: 2px; }
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

/* ── TAXONOMY PANELS (Categories/Tags, Post-only) ────────────────────
   SF5: ONE copy of the panel markup, always in the same DOM position
   (right after .sheet) — repositioned into a sticky right-hand rail at
   >=1280px by CSS Grid alone (grid-column/row placement), never a second
   copy of the markup and never a JS-tracked viewport signal. .editor__body
   is the sheet+panels' shared grid parent; below 1280px it stays a plain
   block flow (the panels render in normal DOM order, below the sheet). */
.editor__body { display: block; }
.editor__below { max-width: calc(var(--measure) + 6rem); margin: 1.5rem auto 0; }
@media (min-width: 1280px) {
  .editor__body {
    display: grid; grid-template-columns: 1fr 19rem; gap: 1.75rem; align-items: start;
    max-width: calc(var(--measure) + 6rem + 19rem + 1.75rem); margin: 0 auto;
  }
  .editor__body .sheet { grid-column: 1; grid-row: 1; max-width: none; margin: 0; }
  .editor__body .editor__below {
    grid-column: 2; grid-row: 1; max-width: none; margin: 0;
    position: sticky; top: 1rem;
  }
}

.chipline { display: flex; flex-wrap: wrap; gap: .4rem; margin: 0 0 .7rem; min-height: 1.9rem; }
.chip {
  display: inline-flex; align-items: center; gap: .4rem; padding: .28rem .35rem .28rem .7rem;
  background: var(--paper-sink); border: 1px solid var(--rule); border-radius: 999px;
  font-family: var(--ff-ui); font-size: .88rem; color: var(--ink-2); line-height: 1.2; max-width: 100%;
}
.chip small { font-family: var(--ff-mono); font-size: .64rem; letter-spacing: .03em; color: var(--steel); white-space: nowrap; }
.chip--new { border-style: dashed; border-color: #C6B27A; color: var(--ochre); background: transparent; }
.chip--new small { color: var(--ochre); }
.chip--oov { border-style: dashed; border-color: var(--steel-2); color: var(--steel); background: transparent; }
.chip--oov small { color: var(--steel-2); }
.chip--rejected { border-color: var(--minium); color: var(--minium-deep); background: #F3E7E2; }
.chip--rejected small { color: var(--minium-deep); }
.chip__x {
  width: 16px; height: 16px; flex: none; display: inline-flex; align-items: center; justify-content: center;
  border: 0; background: transparent; border-radius: 50%; color: var(--steel); font-size: .82rem; line-height: 1; padding: 0;
}
.chip__x:hover { background: var(--paper); color: var(--minium-deep); }
.chip--rejected .chip__x:hover { background: #EAD2C8; }

.termcheck { border: 0; margin: 0; padding: 0; }
.termcheck__legend { font-size: .78rem; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--steel); padding: 0; margin: 0 0 .7rem; }
.termcheck__search { margin-bottom: .7rem; }
.termcheck__scroll { max-height: 16rem; overflow-y: auto; border: 1px solid var(--rule); border-radius: var(--radius); background: var(--paper-sink); padding: .35rem; }
.termcheck__row { display: flex; align-items: center; gap: .55rem; padding: .42rem .5rem; border-radius: 2px; cursor: pointer; font-size: .92rem; color: var(--ink-2); }
.termcheck__row:hover { background: var(--paper-raise); }
.termcheck__row input { accent-color: var(--minium); width: 1rem; height: 1rem; margin: 0; flex: none; }
.termcheck__row:has(input:focus-visible) { outline: 2px solid var(--minium); outline-offset: -2px; background: var(--paper-raise); }
.termcheck__row--ctx { color: var(--steel); font-style: italic; }
.termcheck__row--ctx .termcheck__name { font-style: italic; }
.termcheck__row--match .termcheck__name { font-weight: 500; color: var(--ink); }

.tokenfield { border: 1px solid transparent; border-radius: var(--radius); padding: .25rem; margin: 0 -.25rem; position: relative; }
.tokenfield:focus-within { outline: 2px solid var(--minium); outline-offset: 1px; }
.tokenfield__input { max-width: 100%; }
.combobox__list { list-style: none; margin: .35rem 0 0; padding: .3rem; background: var(--paper-raise); border: 1px solid var(--rule); border-radius: var(--radius); box-shadow: var(--shadow); max-height: 13rem; overflow-y: auto; }
.combobox__opt { padding: .42rem .6rem; border-radius: 2px; font-size: .9rem; color: var(--ink-2); cursor: pointer; display: flex; align-items: center; justify-content: space-between; gap: .6rem; }
.combobox__opt.is-active { background: var(--minium); color: #FCF6F2; }
.combobox__opt.is-active .combobox__hint { color: #F3D9CD; }
.combobox__hint { font-family: var(--ff-mono); font-size: .68rem; letter-spacing: .04em; text-transform: uppercase; color: var(--steel); }

.panelstate { padding: .9rem .2rem; color: var(--steel); font-family: var(--ff-mono); font-size: .8rem; letter-spacing: .03em; }
.panelerror { padding: .65rem .75rem; margin: 0; color: var(--minium-deep); background: #F3E7E2; border: 1px solid #D8C4BC; border-radius: var(--radius); font-size: .85rem; line-height: 1.5; }

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

/* ── SETTINGS ("the forms drawer") — the schema-driven edit UI ─────── */
/* A schema section = a titled proof panel. */
.panel {
  background: var(--paper-raise); border: 1px solid var(--rule);
  border-radius: var(--radius); box-shadow: var(--shadow);
  padding: 1.4rem 1.6rem .4rem; margin-bottom: 1.5rem;
}
.panel__title {
  font-family: var(--ff-display); font-weight: 600; font-size: 1.2rem;
  color: var(--ink); margin: 0 0 .2rem; padding-bottom: .7rem;
  border-bottom: 2px solid var(--ink);
}
.panel__note { margin: .6rem 0 0; font-size: .85rem; color: var(--steel); }

/* One field = a two-column set line: label/help left, control right. */
.setrow {
  display: grid; grid-template-columns: 13rem 1fr; gap: .4rem 1.6rem;
  align-items: start; padding: 1rem 0; border-bottom: 1px solid var(--rule);
}
.setrow:last-child { border-bottom: 0; }
.setrow.is-hidden { display: none; }
.setrow__label {
  font-size: .78rem; font-weight: 600; letter-spacing: .06em;
  text-transform: uppercase; color: var(--steel); padding-top: .5rem;
}
.setrow__control { min-width: 0; }
.setrow__control .input, .setrow__control .select { max-width: 28rem; }
.setrow__help {
  grid-column: 2; margin: .45rem 0 0; font-size: .82rem; line-height: 1.4;
  color: var(--steel); max-width: 34rem;
}

/* number + unit suffix */
.numfield { display: inline-flex; align-items: baseline; gap: .5rem; }
.input--number { width: 5.5rem; text-align: right; font-family: var(--ff-mono); }
.numfield__unit { font-family: var(--ff-mono); font-size: .82rem; letter-spacing: .06em; color: var(--steel); }

/* toggle — a composing-stick lever */
.switch { display: inline-flex; align-items: center; gap: .7rem; cursor: pointer; }
.switch input { position: absolute; opacity: 0; width: 0; height: 0; }
.switch__track {
  position: relative; flex: none; width: 42px; height: 23px;
  background: var(--paper-sink); border: 1px solid #C6C7C0; border-radius: 999px;
  transition: background .15s, border-color .15s;
}
.switch__knob {
  position: absolute; top: 2px; left: 2px; width: 17px; height: 17px;
  border-radius: 50%; background: #FCFCF9; box-shadow: 0 1px 2px rgba(23,25,28,.35);
  transition: transform .18s cubic-bezier(.3,.7,.3,1);
}
.switch__track.is-on { background: var(--minium); border-color: var(--minium-deep); }
.switch__track.is-on .switch__knob { transform: translateX(19px); }
.switch input:focus-visible + .switch__track { outline: 2px solid var(--minium); outline-offset: 2px; }
.switch__text { font-size: .92rem; color: var(--ink-2); }

/* radio group */
.radiogroup { display: flex; flex-direction: column; gap: .55rem; }
.radio { display: inline-flex; align-items: center; gap: .6rem; cursor: pointer; font-size: .95rem; color: var(--ink-2); }
.radio input { accent-color: var(--minium); width: 1rem; height: 1rem; margin: 0; }

.fp-admin .is-hidden { display: none !important; }

/* MediaPicker widget — a proof thumbnail + Choose/Replace/Remove (mirrors the
   editor's featured control at a larger size). */
.mediapick { display: inline-flex; align-items: center; gap: .9rem; flex-wrap: wrap; }
.mediapick__thumb { width: 72px; height: 72px; object-fit: cover; display: block;
  border: 1px solid var(--rule); border-radius: 2px; background: var(--paper-sink); }
.mediapick__thumb.is-empty { display: grid; place-items: center; border-style: dashed;
  border-color: #C6C7C0; color: var(--steel-2); font-size: 1.4rem; }
.mediapick__actions { display: inline-flex; align-items: center; gap: .4rem; }

/* Media library modal — a paper plate over an ink scrim, ruled like a forms drawer. */
.media-modal { position: fixed; inset: 0; z-index: 80; display: grid; place-items: center;
  padding: 2rem; background: rgba(23,25,28,.55); }
.media-modal__plate { width: 100%; max-width: 46rem; max-height: 82vh; display: flex;
  flex-direction: column; background: var(--paper-raise); border: 1px solid var(--rule);
  border-radius: var(--radius); box-shadow: 0 24px 60px -18px rgba(0,0,0,.55); }
.media-modal__head { display: flex; align-items: baseline; justify-content: space-between;
  gap: 1rem; padding: 1.2rem 1.4rem .7rem; border-bottom: 2px solid var(--ink); }
.media-modal__title { font-family: var(--ff-display); font-weight: 600; font-size: 1.3rem;
  color: var(--ink); margin: 0; }
.media-modal__sub { font-family: var(--ff-mono); font-size: .72rem; letter-spacing: .1em;
  text-transform: uppercase; color: var(--steel); }
.media-modal__body { padding: 1.2rem 1.4rem; overflow: auto; }
.media-modal__foot { display: flex; align-items: center; justify-content: flex-end;
  gap: .6rem; padding: .9rem 1.4rem; border-top: 1px solid var(--rule); }
.media-modal__state { padding: 2.4rem .25rem; text-align: center; font-family: var(--ff-mono);
  font-size: .8rem; letter-spacing: .08em; text-transform: uppercase; color: var(--steel-2); }
.media-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(9rem, 1fr)); gap: .9rem; }
.media-cell { display: flex; flex-direction: column; gap: .4rem; padding: .5rem;
  background: var(--paper); border: 1px solid var(--rule); border-radius: var(--radius);
  text-align: left; color: inherit; transition: border-color .12s, background .12s; }
.media-cell:hover { border-color: var(--minium); background: var(--paper-raise); }
.media-cell__thumb { width: 100%; aspect-ratio: 4 / 3; object-fit: cover; display: block;
  border: 1px solid var(--rule); border-radius: 2px; background: var(--paper-sink); }
.media-cell__name { font-family: var(--ff-mono); font-size: .72rem; color: var(--steel);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.media-cell--upload { align-items: center; justify-content: center; gap: .5rem;
  border-style: dashed; border-color: #C6B27A; color: var(--ochre); min-height: 100%;
  font-family: var(--ff-mono); font-size: .78rem; letter-spacing: .06em; text-transform: uppercase; }
.media-cell--upload:hover { border-color: var(--minium); color: var(--minium-deep); background: var(--paper-raise); }
.media-cell--upload .plus { font-size: 1.6rem; line-height: 1; }

/* ── MENUS ("the routing table") — nav-menu editor ─────────────────── */
/* Location assignment reuses the settings set-line (.setrow); a stranded
   (undeclared) binding is inked in ochre so it reads as a leftover. */
.setrow--stranded .setrow__label { color: var(--ochre); }
.setrow--stranded .setrow__label small {
  display: block; font-family: var(--ff-mono); font-size: .66rem; letter-spacing: .08em;
  text-transform: uppercase; font-weight: 500; margin-top: .15rem; color: var(--steel-2);
}

/* The menu-editor meta bar (name + slug + delete) reuses .editor__meta. */
.btn--danger { background: transparent; color: var(--minium-deep); border-color: #D8C4BC; }
.btn--danger:hover { background: #F3E7E2; border-color: var(--minium); }

/* the item tree */
.tree { list-style: none; margin: 1rem 0 0; padding: 0; }
.menurow {
  display: grid; grid-template-columns: auto 1fr auto; gap: .75rem; align-items: start;
  padding: .55rem .65rem; background: var(--paper-raise); border: 1px solid var(--rule);
  border-radius: var(--radius); position: relative;
}
.menurow.is-child::before {
  content: ""; position: absolute; left: calc(-0.75rem + 2px); top: -.45rem; bottom: 50%;
  width: 1px; background: var(--rule);
}
/* drag-and-drop reorder/nest (the button reorder controls remain the keyboard path) */
.menurow__grip {
  display: inline-flex; align-items: center; justify-content: center; min-width: 24px;
  min-height: 24px; margin-right: 2px; color: var(--steel-2); cursor: grab; user-select: none;
  border-radius: 2px; font-size: .9rem; line-height: 1; touch-action: none;
}
.menurow__grip:hover { color: var(--ink-2); background: var(--paper-sink); }
.menurow.is-dragging { opacity: .45; }
.menurow.is-droptarget { outline: 2px solid var(--minium); outline-offset: 1px; }
/* Gaps provide ALL inter-row spacing (the row carries no margin), so each is a clean, non-
   overlapping drop target between rows — a subtree dropped here becomes a sibling at this spot. */
.dropgap { list-style: none; height: .7rem; border-radius: 3px; position: relative; }
.dropgap.is-hint::before {
  content: ""; position: absolute; left: 0; right: 0; top: 50%; transform: translateY(-50%);
  height: 3px; background: var(--minium); border-radius: 2px; box-shadow: 0 0 6px var(--minium);
}
.dropgap--end {
  height: 1.6rem; margin: .1rem 0 0; border: 1px dashed transparent; border-radius: var(--radius);
}
.dropgap--end.is-hint { border-color: var(--minium); }
.menurow__reorder { display: inline-flex; gap: 2px; align-self: center; }
.rbtn {
  width: 26px; height: 26px; display: inline-flex; align-items: center; justify-content: center;
  background: transparent; border: 1px solid var(--rule); border-radius: 2px; color: var(--ink-2);
  font-size: .8rem; line-height: 1;
}
.rbtn:hover:not(.is-off) { background: var(--paper-sink); border-color: var(--steel-2); }
.rbtn.is-off { color: var(--steel-2); border-color: transparent; opacity: .4; cursor: default; }
.menurow__body { min-width: 0; display: flex; flex-direction: column; gap: .3rem; }
.menurow__labelinput {
  font-family: var(--ff-display); font-size: 1.02rem; max-width: 22rem; width: 100%;
  padding: .3rem .5rem; background: var(--paper-sink); border: 1px solid var(--rule);
  border-bottom: 2px solid #C6C7C0; border-radius: var(--radius); color: var(--ink);
}
.menurow__labelinput:focus { outline: none; background: var(--paper-raise); border-color: var(--steel); border-bottom-color: var(--minium); }
.menurow__meta { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; font-family: var(--ff-mono); font-size: .72rem; color: var(--steel); }
.menurow__urlinput {
  font-family: var(--ff-mono); font-size: .8rem; max-width: 16rem; padding: .15rem .4rem;
  background: var(--paper-sink); border: 1px solid var(--rule); border-radius: 2px; color: var(--ink);
}
.menurow__urlinput:focus { outline: none; border-color: var(--minium); }
.tag {
  font-family: var(--ff-mono); font-size: .64rem; font-weight: 500; letter-spacing: .1em;
  text-transform: uppercase; padding: .12rem .4rem; border-radius: 2px; border: 1.5px solid currentColor; white-space: nowrap;
}
.tag--page { color: var(--green); }
.tag--post { color: var(--steel-tag); }
.tag--custom { color: var(--ochre); }
.tag--term { color: var(--minium-deep); border-style: dashed; }
.menurow__href { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 22rem; }
.menurow__warn { color: var(--minium-deep); }
.menurow__actions { display: inline-flex; align-items: center; gap: .6rem; align-self: center; }
.xbtn {
  width: 26px; height: 26px; display: inline-flex; align-items: center; justify-content: center;
  background: transparent; border: 1px solid transparent; border-radius: 2px; color: var(--steel); font-size: 1rem;
}
.xbtn:hover { color: var(--minium-deep); border-color: #D8C4BC; background: #F3E7E2; }
.menurow__newtab { position: absolute; opacity: 0; width: 0; height: 0; }
.emptytree { padding: 1.6rem; text-align: center; color: var(--steel); border: 1px dashed var(--rule); border-radius: var(--radius); margin-top: 1rem; }
.menucfg { margin: .7rem 0 .2rem; }
.menubar { display: flex; align-items: center; justify-content: space-between; margin: .5rem 0 0; }
.savebar { margin-top: 1.2rem; display: flex; gap: .8rem; align-items: center; }
.dirtydot { font-family: var(--ff-mono); font-size: .72rem; letter-spacing: .06em; color: var(--ochre); }

/* the link-candidate picker (reuses .media-modal chrome) */
.tabs { display: inline-flex; gap: .3rem; margin-bottom: .8rem; }
.tab {
  padding: .35rem .8rem; border: 1px solid var(--rule); border-radius: 2px; background: transparent;
  font-family: var(--ff-mono); font-size: .72rem; letter-spacing: .06em; text-transform: uppercase; color: var(--steel);
}
.tab.is-on { background: var(--ink); color: var(--paper); border-color: var(--ink); }
.candidate {
  display: flex; align-items: center; gap: .6rem; width: 100%;
  text-align: left; padding: .5rem .6rem; background: transparent; border: 0; border-bottom: 1px solid var(--rule); color: inherit; font: inherit; cursor: pointer;
}
.candidate:hover { background: var(--paper-sink); }
.candidate.is-selected { background: var(--paper-sink); }
.candidate.is-added { cursor: default; opacity: .55; }
.candidate__check { flex: 0 0 auto; width: 1.15rem; font-size: 1rem; line-height: 1; color: var(--steel); }
.candidate.is-selected .candidate__check { color: var(--minium-deep); }
.candidate.is-added .candidate__check { color: var(--green); }
.candidate__title { flex: 1 1 auto; font-family: var(--ff-display); font-size: 1rem; color: var(--ink); }
.candidate__href { flex: 0 0 auto; font-family: var(--ff-mono); font-size: .72rem; color: var(--steel); }
.truncnote { font-family: var(--ff-mono); font-size: .72rem; color: var(--ochre); padding: .5rem .6rem; }
.pickerfield { margin-bottom: .9rem; }
.pickerfield > label { display: block; font-size: .7rem; font-weight: 600; letter-spacing: .1em; text-transform: uppercase; color: var(--steel); margin-bottom: .35rem; }

@media (max-width: 640px) {
  .row { grid-template-columns: 36px 1fr auto; }
  .row__time, .row__edit { display: none; }
  .sheet { padding: 1.5rem; }
  .sheet__title { font-size: 1.7rem; }
  .sheet [data-pm-editor] h1 { font-size: 1.7rem; }
  .setrow { grid-template-columns: 1fr; gap: .3rem; }
  .setrow__label { padding-top: 0; }
  .setrow__help { grid-column: 1; }
}
@media (prefers-reduced-motion: reduce) {
  .fp-admin *, .toast { transition: none !important; }
}
"#;
