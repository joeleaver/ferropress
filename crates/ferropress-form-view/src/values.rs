//! [`FormValues`] — the live value surface shared between the rendered form (which
//! WRITES on every input) and the caller (which READS a snapshot on submit).
//!
//! It is a plain `Rc<RefCell<Map>>`, deliberately **not** a reactive `Signal`: an
//! input write must not re-render the form, or a text field would rebuild mid-keystroke
//! and fight the caret. The form's only reactive parts (radio/toggle `checked`,
//! `visible_when`) are driven by separate per-field `Signal<Value>`s created inside the
//! renderer; those mirror the value for display, while this holds the authoritative map
//! the caller serializes back to the server.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Map, Value};

/// Fires after every [`FormValues::set`] — the hook a host uses to drive an
/// unsaved-changes ("dirty") indicator (MF15; `FormValues` has no other write
/// path a host could observe, since `install_beforeunload_guard` polls
/// `Signal<bool>`s at event time and a snapshot-diff-at-exit can't feed it).
///
/// A plain struct — never `Option<_>`, never a bare `Rc<dyn Fn>` prop — mirrors
/// [`OnPickMedia`](crate::OnPickMedia)'s exact shape: `rsx!` routes both
/// callable and `Option<_>` prop VALUES, and any prop whose FIELD NAME starts
/// with `on*`, through an event-handler coercion that recurses without
/// terminating. The type name reads `On*`; what must not start with `on*` is
/// the field name a caller passes it under in `rsx!` (`SchemaForm`'s prop is
/// named `notify`, never `on_change`).
#[derive(Clone)]
pub struct OnFormChange(Rc<dyn Fn()>);

impl OnFormChange {
    /// Wrap a callback to fire after every field write.
    pub fn new(notify: impl Fn() + 'static) -> Self {
        Self(Rc::new(notify))
    }

    fn fire(&self) {
        (self.0)()
    }
}

impl Default for OnFormChange {
    /// A no-op — a [`FormValues`] nobody installed a listener on (a test, or a
    /// form the host doesn't track dirtiness for) simply notifies into nothing.
    fn default() -> Self {
        Self(Rc::new(|| {}))
    }
}

/// A shared, cheaply-cloneable handle to a form's current `key -> JSON value` map.
/// (`Default` — an empty map, a no-op notify — is derived only to satisfy the
/// rinch `#[component]` macro, which generates a `Default` for a component's props.)
#[derive(Clone, Default)]
pub struct FormValues {
    inner: Rc<RefCell<Map<String, Value>>>,
    /// The listener [`set`](Self::set) fires, installed via
    /// [`set_notify`](Self::set_notify). `Rc<RefCell<_>>` (not a per-clone
    /// field) so installing it through ONE clone of this handle — typically
    /// `SchemaForm`'s prop wiring, once at mount — makes every other clone
    /// (including the caller's own copy, since `Clone` shares this same `Rc`
    /// exactly like `inner`) notify too.
    notify: Rc<RefCell<OnFormChange>>,
}

impl FormValues {
    /// Seed from the current values (schema defaults overlaid with stored values).
    pub fn new(initial: Map<String, Value>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(initial)),
            notify: Rc::new(RefCell::new(OnFormChange::default())),
        }
    }

    /// The current value for `key`, or `Null` if unset.
    pub fn get(&self, key: &str) -> Value {
        self.inner.borrow().get(key).cloned().unwrap_or(Value::Null)
    }

    /// Overwrite `key` with `value` (an input edit), then fire the installed
    /// [`OnFormChange`] listener. THE single choke point every field write in
    /// the renderer goes through (nine call sites across `view.rs`'s widget
    /// dispatch all route here) — the change-notify hook lives on THIS method,
    /// not sprinkled at each of them. The listener is cloned out from behind
    /// its `RefCell` BEFORE firing, so a listener that itself writes back into
    /// this same `FormValues` (a legal, if unusual, host reaction) can never
    /// hit a `RefCell` double-borrow.
    pub fn set(&self, key: &str, value: Value) {
        self.inner.borrow_mut().insert(key.to_owned(), value);
        let notify = self.notify.borrow().clone();
        notify.fire();
    }

    /// Install (or replace) the listener [`set`](Self::set) fires from now on.
    pub fn set_notify(&self, notify: OnFormChange) {
        *self.notify.borrow_mut() = notify;
    }

    /// A clone of the whole map — what the caller PUTs on submit.
    pub fn snapshot(&self) -> Map<String, Value> {
        self.inner.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn set_notify_fires_on_every_field_write() {
        let values = FormValues::new(Map::new());
        let calls = Rc::new(Cell::new(0));
        let calls_in_closure = Rc::clone(&calls);
        values.set_notify(OnFormChange::new(move || {
            calls_in_closure.set(calls_in_closure.get() + 1);
        }));

        values.set("a", Value::String("1".to_owned()));
        assert_eq!(calls.get(), 1);
        values.set("b", Value::Bool(true));
        assert_eq!(calls.get(), 2);
        // Overwriting an EXISTING key still counts as a write.
        values.set("a", Value::String("2".to_owned()));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_form_values_with_no_installed_notify_does_not_panic() {
        // The default (no-op) listener — proves `set` is safe to call before
        // any host ever installs one (a test double, or a schema a host
        // doesn't track dirtiness for).
        let values = FormValues::new(Map::new());
        values.set("a", Value::Null);
        assert_eq!(values.get("a"), Value::Null);
    }

    #[test]
    fn installing_notify_through_one_clone_is_visible_on_another() {
        // `SchemaForm` receives a CLONE of the caller's `FormValues` and calls
        // `set_notify` on it — the caller's OWN handle must still notify too,
        // since both share the same underlying `Rc`.
        let caller_values = FormValues::new(Map::new());
        let mount_values = caller_values.clone();

        let calls = Rc::new(Cell::new(0));
        let calls_in_closure = Rc::clone(&calls);
        mount_values.set_notify(OnFormChange::new(move || {
            calls_in_closure.set(calls_in_closure.get() + 1);
        }));

        caller_values.set("a", Value::String("x".to_owned()));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_notify_listener_that_writes_back_does_not_panic() {
        // A listener re-entering `set` on the SAME `FormValues` must not hit a
        // `RefCell` double-borrow (the notify closure is cloned out before
        // firing — see `set`'s doc). Guarded to re-enter exactly once: "fires
        // on every write" means an UNGUARDED re-entrant write recurses
        // forever BY DESIGN — that is a property of the contract, not a bug
        // this test is trying to catch; the borrow-safety is.
        let values = FormValues::new(Map::new());
        let inner = values.clone();
        let entered = Rc::new(Cell::new(false));
        let entered_in_closure = Rc::clone(&entered);
        values.set_notify(OnFormChange::new(move || {
            if !entered_in_closure.replace(true) {
                inner.set("re-entrant", Value::Bool(true));
            }
        }));

        values.set("a", Value::Null);
        assert_eq!(values.get("re-entrant"), Value::Bool(true));
    }
}
