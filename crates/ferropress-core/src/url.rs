//! URL-related text helpers: safety-checking author-supplied hyperlinks, and
//! deriving URL-safe slugs from arbitrary text.
//!
//! The renderer turns an [`InlineRun`](crate::InlineRun)'s `href` into an
//! `<a href>`, and the admin editor lets an author type an arbitrary link URL. A
//! script-bearing scheme (`javascript:`, `data:`, `vbscript:`) in that position is
//! stored XSS, so every href is checked with [`is_safe_href`] before it becomes a
//! live link: at the renderer (the load-bearing chokepoint — the one place a block
//! becomes HTML, reached by editor preview, the public page, AND plugin- or
//! import-authored content), and, for immediate feedback, in the editor client.
//!
//! This mirrors the stricter `author_url` guard on comments (`ferropress-http`),
//! which only permits absolute `http(s)`; content links additionally allow
//! `mailto:` and relative/anchor URLs so authors can link within the site.
//!
//! [`slugify`] is unrelated to safety (its output is always a plain
//! lowercase-alphanumeric-and-dash string, never a scheme) but lives here as the
//! other URL-shaped text transform every consumer needs a SINGLE, lockstep copy
//! of: `ferropress-http`'s admin surfaces (menu names, media filenames, term
//! names) and, from Inc 3 on, the wasm admin-SPA client (matching the server's
//! own reuse-by-slug decision for inline tag creation — see `posts.rs`'s
//! `apply_terms`/`resolve_term_slug`) both call this SAME function, so a client
//! guess ("will this become a new tag or reuse an existing one?") can never
//! diverge from what the server actually decides.

/// Whether `href` is safe to emit as an `<a href>` target.
///
/// A URL with **no scheme** — site-relative (`/about`), anchor (`#top`),
/// protocol-relative (`//cdn.example.com/x`), or a bare relative path
/// (`page.html`) — cannot execute script, so it is always allowed. A URL **with** a
/// scheme is allowed only when that scheme is `http`, `https`, or `mailto`
/// (case-insensitive); every other scheme — including `javascript:`, `data:`, and
/// `vbscript:` — is rejected.
///
/// Any control character or whitespace anywhere also rejects the URL: browsers
/// strip those before parsing the scheme, so `java&#9;script:…` (a literal tab)
/// could otherwise sneak a script scheme past a naive prefix check.
///
/// Note an empty string has no scheme and is therefore "safe" here; callers that
/// treat an empty href as "no link" should check emptiness separately (both the
/// renderer and the editor already do).
pub fn is_safe_href(href: &str) -> bool {
    if href.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    match scheme_of(href) {
        Some(scheme) => matches!(scheme.as_str(), "http" | "https" | "mailto"),
        None => true,
    }
}

/// The lowercased URL scheme (the text before the first `:`), or `None` when there
/// is no scheme — the `:` is absent, or a `/`, `?`, `#`, or any other non-scheme
/// byte appears first, marking the URL as relative. Follows RFC 3986: a scheme is
/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn scheme_of(url: &str) -> Option<String> {
    let mut chars = url.chars();
    let mut scheme = String::new();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => scheme.push(c.to_ascii_lowercase()),
        _ => return None,
    }
    for c in chars {
        match c {
            ':' => return Some(scheme),
            c if c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.' => {
                scheme.push(c.to_ascii_lowercase());
            }
            // Anything else (`/`, `?`, `#`, …) before a `:` means there is no
            // scheme — the URL is relative.
            _ => return None,
        }
    }
    None
}

/// Derive a URL-safe single-segment slug from arbitrary text: lowercase ASCII
/// alphanumerics are kept, every other run collapses to a single `-`, and leading/
/// trailing dashes are trimmed. `None` when nothing survives (e.g. an all-symbol
/// input, or an all-non-ASCII input like "日本語").
///
/// The LOCKSTEP pair every "does this text match an existing slug?" decision must
/// share: `ferropress-http`'s admin surfaces (media filenames, menu names, term
/// names — including the inline tag-creation reuse-by-slug decision,
/// `posts.rs`'s `apply_terms`) and the wasm admin-SPA client (matching a typed
/// tag name against the loaded vocabulary BEFORE save, so the "will be created"
/// chip is never a lie the server's own slug match then contradicts). Never
/// re-implement this — import it.
pub fn slugify(text: &str) -> Option<String> {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() {
            pending_dash = true;
        }
    }
    if slug.is_empty() { None } else { Some(slug) }
}

#[cfg(test)]
mod tests {
    use super::{is_safe_href, slugify};

    #[test]
    fn allows_http_https_and_mailto_any_case() {
        for url in [
            "http://example.com/a",
            "https://example.com/a?b=c#d",
            "HTTPS://EXAMPLE.COM",
            "MailTo:hi@example.com",
        ] {
            assert!(is_safe_href(url), "{url} should be safe");
        }
    }

    #[test]
    fn allows_relative_anchor_and_protocol_relative() {
        // No scheme → cannot execute script → always allowed.
        for url in [
            "/about",
            "#section",
            "./page.html",
            "../up",
            "page.html",
            "//cdn.example.com/lib.js",
            "",
        ] {
            assert!(is_safe_href(url), "{url} should be safe");
        }
    }

    #[test]
    fn rejects_script_bearing_schemes() {
        for url in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "  javascript:alert(1)", // leading whitespace also rejects outright
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
        ] {
            assert!(!is_safe_href(url), "{url} should be rejected");
        }
    }

    #[test]
    fn rejects_control_or_whitespace_anywhere() {
        // Browsers strip these before scheme parsing, so `java\tscript:` would
        // resolve to the javascript scheme — reject the whole URL up front.
        for url in [
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "http://exa mple.com",
            "http://example.com/\u{0000}",
        ] {
            assert!(!is_safe_href(url), "{url:?} should be rejected");
        }
    }

    #[test]
    fn rejects_other_absolute_schemes() {
        // Not script vectors, but outside the allow-list — they render as plain
        // text rather than a link. `tel:`/`ftp:` can be added later if wanted.
        for url in ["tel:+15551234", "ftp://example.com/f", "sms:12345"] {
            assert!(!is_safe_href(url), "{url} should be rejected");
        }
    }

    #[test]
    fn slugify_lowercases_and_collapses_separators() {
        // The fixture set `ferropress-http`'s pre-move copy was exercised against
        // (menu names, media filenames, term names) — same inputs, same outputs,
        // proving the moved function is a byte-for-byte behavioral match, not a
        // reimplementation.
        assert_eq!(slugify("Sci-Fi").as_deref(), Some("sci-fi"));
        assert_eq!(slugify("Sci Fi").as_deref(), Some("sci-fi"));
        assert_eq!(
            slugify("  Leading and trailing  ").as_deref(),
            Some("leading-and-trailing")
        );
        assert_eq!(
            slugify("Multiple---Dashes").as_deref(),
            Some("multiple-dashes")
        );
        assert_eq!(
            slugify("Under_Score & Punct!").as_deref(),
            Some("under-score-punct")
        );
        assert_eq!(slugify("already-a-slug").as_deref(), Some("already-a-slug"));
        assert_eq!(slugify("2024").as_deref(), Some("2024"));
        assert_eq!(
            slugify("Ada Lovelace's Notes").as_deref(),
            Some("ada-lovelace-s-notes")
        );
    }

    #[test]
    fn slugify_none_when_nothing_survives() {
        for text in ["", "   ", "---", "!!!", "日本語", "★★★"] {
            assert_eq!(slugify(text), None, "{text:?} should slugify to None");
        }
    }
}
