# Ferropress

A Rust-powered, WordPress-style CMS — familiar block-based authoring, modern static-first delivery.

> **Status:** early scaffolding. The architecture is settled; the workspace and content model are being built out.

## What it is

Ferropress aims to give you WordPress's authoring experience (a block editor, themes, plugins) without
WordPress's biggest weaknesses — security surface, performance, and plugin sprawl. It is a single,
self-contained Rust server that owns its HTTP serving, rendering, plugin runtime, and (by default) its
database engine, all in-process.

## Architecture at a glance

- **Content model:** a typed JSON block tree (Portable Text / Editor.js style) with a stable schema
  version, stored in **rhypedb** — a typed object database with first-class relationships and native
  vector / semantic search.
- **Delivery:** *static-first hybrid* — pages are pre-rendered to static HTML and regenerated only when
  content changes (driven off the database's change-subscription stream); interactive bits (semantic
  search, live comments) are mounted as islands.
- **One shared renderer:** a single `block-tree → HTML` crate renders both the editor preview and the
  public site, so what you see is what you publish.
- **Admin / interactive UI:** **rinch**, a Rust reactive GUI framework that renders to the browser DOM.
- **Extensibility:** plugins run in an embedded, capability-sandboxed WebAssembly runtime —
  deny-by-default, with structural capabilities.
- **Portable by construction:** the core carries no hosting-vendor lock-in; self-hostable as a single binary.

## License

TBD.
