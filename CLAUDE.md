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

## 4. Core ships no consumer-specific anything — themes included

Ferropress core is a published, general-purpose CMS; the dependency direction only ever runs
consumer project → core, never the reverse. Core must never carry a specific consumer theme
project's name, its assertions on that project's own copy/wording, or any other consumer-specific
content. (Enforced: `xtask -- no-consumer-names-lint`, wired into CI, fails the build if a tracked
file mentions a banned consumer-project name.)

What core DOES ship is a **generic, env-driven external-theme contract checker** —
`crates/ferropress-serve/src/tests.rs`'s `external_theme_engines()` and its `external_theme_*`
tests — structural checks only (the `data-fp-*` hook attributes; that `terms`/`archive` context
fields reach the render somewhere via injected probe values), asserting nothing about any theme's
own copy, class names, or scripts. A theme author (in their OWN project, not this one) points it at
their theme dir to verify they still honor the shared render contract:

```sh
FERROPRESS_EXTERNAL_THEME_DIR=/path/to/your-theme-project/theme \
cargo run --manifest-path xtask/Cargo.toml -- theme-contract-check
```

A consumer theme project's OWN specific assertions (its eyebrow copy, empty-state text, chip
class names, script byte-parity against the built-in theme, or anything else specific to that
project's own markup) belong entirely in that project, run from there — never in this repo.
Ferropress's own CI never checks out a consumer theme project.
