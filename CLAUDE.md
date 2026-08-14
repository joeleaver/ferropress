# Ferropress — project instructions

Ferropress is a brand-new, Rust-powered CMS: **WordPress-style authoring** over a **static-first hybrid
delivery model** — pre-render content to static HTML, regenerate only changed pages off rhypedb's
change-subscription stream, and serve dynamic bits (semantic search, live comments) as rinch islands.
Keep WordPress's authoring soul; modernize the delivery. (Serving-model rationale + guardrails live in
the `ferropress-serving-model` memory.) Stack: **rhypedb** (typed object DB),
**rinch** (the Rust reactive GUI framework — powers the admin SPA and public-site islands), and
**jkbase** (one recommended self-host target, *not* the substrate). Architecture decisions, the
content/block model, and upstream status live in the project memory — read it before designing.

These instructions OVERRIDE default behavior. Follow them exactly.

Machine-local rules — read-only access to the sibling upstream repos (rinch, jkbase, rhypedb) with
absolute local paths — live in an untracked, git-ignored `CLAUDE.local.md` alongside this file,
imported here so they still load:

@CLAUDE.local.md

## 1. Correctness over speed — always

We have **unlimited time and unlimited budget**. When the choice is between the easy way and the
complete, technically correct way, **the correct way ALWAYS wins.** No shortcuts. No "good enough for
now," no stubbing-around a problem, no deferring correctness for convenience. If the proper fix is
bigger, harder, or slower, do the proper fix anyway. If something can't be done correctly yet, say so
plainly rather than shipping a shortcut.

## 2. UX design: HTML first, then port to rinch

rinch is HTML/CSS under the hood. When designing new UI/UX, prefer to design it in plain HTML/CSS
first (use the **frontend-design** skill), get the structure and visual design right, *then* port it
to rinch. This keeps design iteration fast and decoupled from the framework port.

## 3. Use the rinch skill

Whenever writing or editing rinch code (`rsx!`, `Signal`, `#[component]`, rinch components), invoke
the **`rinch:rinch`** skill. Don't hand-write rinch from memory — let the skill guide correct use of
the macro, reactive signals, props, and state management.

## 4. Changing a shared theme mechanism (built-in + fellstone-site)? Run the parity check

Some markup/script is deliberately **byte-identical** between the built-in theme
(`crates/ferropress-serve/src/templates.rs`) and the `fellstone-site` sibling project's own theme
(the endless-scroll script is the current example). `crates/ferropress-serve/src/tests.rs` carries
parity fixtures that load fellstone-site's REAL theme files and assert they still match — but they
do **not** run in CI (`fellstone-site` has no pushed remote as of this writing, so CI has nothing to
check it out from; see the fixtures' own doc comments and `xtask/src/main.rs`'s `run_fellstone_parity`
for the full reasoning). Run them yourself before pushing any change to a shared theme mechanism:

```sh
cargo run --manifest-path xtask/Cargo.toml -- parity
```

This auto-discovers `fellstone-site` at the conventional sibling location (`../fellstone-site`) or
via `FERROPRESS_FELLSTONE_DIR` if it lives elsewhere.
