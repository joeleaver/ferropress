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

/// A shared, cheaply-cloneable handle to a form's current `key -> JSON value` map.
/// (`Default` — an empty map — is derived only to satisfy the rinch `#[component]`
/// macro, which generates a `Default` for a component's props.)
#[derive(Clone, Default)]
pub struct FormValues {
    inner: Rc<RefCell<Map<String, Value>>>,
}

impl FormValues {
    /// Seed from the current values (schema defaults overlaid with stored values).
    pub fn new(initial: Map<String, Value>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(initial)),
        }
    }

    /// The current value for `key`, or `Null` if unset.
    pub fn get(&self, key: &str) -> Value {
        self.inner.borrow().get(key).cloned().unwrap_or(Value::Null)
    }

    /// Overwrite `key` with `value` (an input edit).
    pub fn set(&self, key: &str, value: Value) {
        self.inner.borrow_mut().insert(key.to_owned(), value);
    }

    /// A clone of the whole map — what the caller PUTs on submit.
    pub fn snapshot(&self) -> Map<String, Value> {
        self.inner.borrow().clone()
    }
}
