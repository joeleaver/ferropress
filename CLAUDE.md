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

## 1. Correctness over speed — always

We have **unlimited time and unlimited budget**. When the choice is between the easy way and the
complete, technically correct way, **the correct way ALWAYS wins.** No shortcuts. No "good enough for
now," no stubbing-around a problem, no deferring correctness for convenience. If the proper fix is
bigger, harder, or slower, do the proper fix anyway. If something can't be done correctly yet, say so
plainly rather than shipping a shortcut.

## 2. Sibling repos (rinch, jkbase, rhypedb) are UPSTREAM — read-only from here

We also own `rinch`, `jkbase`, and `rhypedb` (sibling dirs under `/home/joe/dev`), so you MAY read
them for reference. But treat them as **real external upstreams**, not as part of this working tree:

- **Never write to or edit files in those repos directly.** Not even a one-line fix.
- **Never use relative paths** (`../rinch`, …) to reach or depend on them. Use absolute paths
  (`/home/joe/dev/rinch`) when reading them, and depend on them the way you'd depend on any published
  upstream — *not* via Cargo `path = "../…"` dependencies.
- **To change them, act like an upstream contributor:** file an issue, or create a branch and open a
  PR against that repo. Do not reach across and mutate them in place to unblock Ferropress.

## 3. UX design: HTML first, then port to rinch

rinch is HTML/CSS under the hood. When designing new UI/UX, prefer to design it in plain HTML/CSS
first (use the **frontend-design** skill), get the structure and visual design right, *then* port it
to rinch. This keeps design iteration fast and decoupled from the framework port.

## 4. Use the rinch skill

Whenever writing or editing rinch code (`rsx!`, `Signal`, `#[component]`, rinch components), invoke
the **`rinch:rinch`** skill. Don't hand-write rinch from memory — let the skill guide correct use of
the macro, reactive signals, props, and state management.
