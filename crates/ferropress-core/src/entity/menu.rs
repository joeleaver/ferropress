//! `Menu` + `MenuItem` + `MenuLocation` — dedicated typed nav menus, replacing
//! WP's post-row + nav_menu-taxonomy + 6-postmeta-keys overload.
//!
//! A `Menu` is a named, ordered tree of `MenuItem`s with a typed [`LinkTarget`]
//! union (internal ref / external URL / taxonomy term). Which theme *location*
//! (`"primary"`, `"footer"`, …) a menu appears in is NOT a property of the menu:
//! following WordPress's `nav_menu_locations`, a separate [`MenuLocation`]
//! assignment binds a location to a menu, so ONE menu can be shown in MANY
//! locations, and a location holds at most one menu (`location` is `@unique`).

use crate::value::ObjectId;

/// A named nav menu — a container for an ordered [`MenuItem`] tree. The menu
/// itself carries no location; a [`MenuLocation`] assignment binds it to one or
/// more theme locations.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub id: Option<ObjectId>,
    pub slug: String,
    pub name: String,
    pub meta: serde_json::Value,
}

/// Where a menu item points. A typed union instead of WP's
/// `_menu_item_type`/`_menu_item_object`/`_menu_item_object_id` meta triple.
///
/// Serialized (tagged) into the `MenuItem.target` `String` column — the SDL has
/// no union scalar — and read back the same way, so the wire form and the stored
/// form are one type. `Term` targets a taxonomy term (resolvable once taxonomies
/// land); the other three resolve today.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkTarget {
    Post { id: u64 },
    Page { id: u64 },
    Term { id: u64 },
    Custom { url: String },
}

/// One item in a menu. `item_order` is the ordinal WITHIN its `(menu, parent)`
/// sibling group (contiguous `0..n`); `parent` nests it under another item in the
/// same menu. The field name mirrors the SDL column 1:1 (`item_order`) so a
/// hand-built store `FieldMap` can't address the wrong key.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: Option<ObjectId>,
    pub label: String,
    pub item_order: i32,
    pub target: LinkTarget,
    pub meta: serde_json::Value,

    pub menu: Option<ObjectId>,   // -> Menu
    pub parent: Option<ObjectId>, // -> MenuItem (nesting)
}

/// The assignment of a theme location to a menu — WP's `nav_menu_locations`,
/// modeled as a first-class row instead of a serialized theme-mod option.
/// `location` is `@unique` (a location holds one menu); many `MenuLocation` rows
/// may point at the SAME `menu` (one menu shown in many locations). Deleting a
/// menu cascades its assignment rows away, so the location falls back to nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuLocation {
    pub id: Option<ObjectId>,
    pub location: String,
    pub menu: Option<ObjectId>, // -> Menu
}

/// Validate a menu-item `Custom` link URL against a scheme allow-list, returning it
/// trimmed on success or `None` when it must be rejected. THE single source for the
/// two-layer XSS guard's URL half: the admin write path enforces it before storing a
/// `Custom` target ([`sanitize_href`] there maps `None` to a 400), and the serve
/// compose path re-checks it defensively before emitting an `href` (a stored value is
/// never trusted blindly). Because both sides call THIS function, the write-time and
/// render-time verdicts can never drift.
///
/// Permits a site-relative path (`/…`), a bare `#fragment` / `?query`, a scheme-less
/// relative reference, and the `http` / `https` / `mailto` / `tel` schemes. Rejects a
/// protocol-relative `//host` (and backslash variants some browsers treat alike), any
/// control/whitespace char (which can smuggle a scheme past the check yet still be
/// honored by a browser once stripped — `java\tscript:`), and every other scheme
/// (`javascript:`, `data:`, `vbscript:`, `file:`, `blob:`, …).
pub fn sanitize_href(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() {
        return None;
    }
    // No raw control/whitespace: an embedded tab/newline can smuggle a scheme past the
    // check below yet still be honored by a browser once stripped.
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    // Protocol-relative `//host` (and backslash variants) inherit the page scheme — reject.
    if url.starts_with("//") || url.starts_with("/\\") || url.starts_with('\\') {
        return None;
    }
    match url_scheme(url) {
        // No scheme → a relative / site-relative / fragment reference: safe.
        None => Some(url.to_owned()),
        Some(scheme) => match scheme.as_str() {
            "http" | "https" | "mailto" | "tel" => Some(url.to_owned()),
            _ => None,
        },
    }
}

/// The URL scheme (lowercased) iff `url` begins with a valid RFC-3986 scheme
/// (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`), else `None` (a relative ref).
fn url_scheme(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            return Some(url[..i].to_ascii_lowercase());
        }
        if !(b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.') {
            return None; // a non-scheme char before any ':' → no scheme
        }
    }
    None
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_href;

    #[test]
    fn allows_safe_targets() {
        for ok in [
            "/about",
            "/",
            "#top",
            "?q=1",
            "about/team",
            "http://example.com",
            "https://example.com/x?y=1#z",
            "HTTPS://EXAMPLE.COM",
            "mailto:a@b.com",
            "tel:+15551234",
            "  /trimmed  ",
        ] {
            assert!(sanitize_href(ok).is_some(), "expected {ok:?} to be allowed");
        }
        // The returned value is trimmed.
        assert_eq!(sanitize_href("  /x  ").as_deref(), Some("/x"));
    }

    #[test]
    fn rejects_dangerous_targets() {
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>",
            "vbscript:msgbox",
            "file:///etc/passwd",
            "blob:https://x",
            "//evil.example.com",
            "/\\evil.example.com",
            "\\\\evil",
            "  ",
            "",
        ] {
            assert!(
                sanitize_href(bad).is_none(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
