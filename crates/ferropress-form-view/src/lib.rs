//! # ferropress-form-view
//!
//! THE `FormSchema -> rinch edit-UI` renderer for the Ferropress admin SPA — the
//! editing-side mirror of `ferropress-render` (block-tree -> HTML). It takes the
//! declarative [`ferropress_render_form::FormSchema`] (rinch-free schema data) and
//! projects it into a rinch component tree, with the SINGLE `ControlKind -> control`
//! dispatch (marker `FERROPRESS-FORM-DISPATCH`, in [`view`]).
//!
//! It depends on the `rinch` facade, so — like `ferropress-admin`,
//! `ferropress-islands`, and `ferropress-editor-bridge` — it is EXCLUDED from the
//! workspace (the dep-graph lint bans rinch from every member) and is compiled for
//! wasm32 as a path dep of the admin. rinch is pinned to the exact rev the admin
//! uses so `NodeHandle` is one type across the cross-crate component call.
//!
//! The admin creates a [`FormValues`] from the server's current values, mounts
//! [`SchemaForm`], and reads `FormValues::snapshot()` on Save to PUT back. All value
//! VALIDATION lives in `ferropress-render-form` (host-tested); this crate is the
//! browser-verified DOM projection.

mod values;
mod view;

pub use values::FormValues;
pub use view::{MediaChosen, OnPickMedia, SchemaForm};
