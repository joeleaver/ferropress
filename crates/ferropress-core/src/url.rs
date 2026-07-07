//! URL-safety helpers for author-supplied hyperlinks.
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

#[cfg(test)]
mod tests {
    use super::is_safe_href;

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
}
