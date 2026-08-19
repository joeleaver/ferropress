//! `sanitize_widget_html` — the ONE shared HTML sanitizer for widget-authored
//! content (the Custom HTML kind's raw markup). Wraps `ammonia`, the same
//! single-source discipline [`crate::entity::menu::sanitize_href`] uses for
//! menu-item URLs: everyone who needs this guarantee calls the same function,
//! so the write-time and later-reload verdicts can never drift.
//!
//! Call sites (later slices — this one ships the function + its adversarial
//! corpus only, no callers yet): (a) the admin widget-write path, which
//! stores the cleaned HTML AND echoes it in the authoritative response, so
//! the author sees exactly what survived; and (b) `WidgetHandle` LOAD time
//! (serving layer), so the in-memory widget set only ever holds post-sanitize
//! strings and a row written under an older policy self-heals on its next
//! reload. NEVER called per-request/per-compose — widgets are live chrome on
//! every request, and a hot-path ammonia parse would buy nothing over the
//! store-to-memory boundary the two call sites above already cover.
//!
//! # Invariants (T1 / SC3 rulings — pinned here, binding forever)
//!
//! * Sanitization is UNCONDITIONAL for every role. Ferropress has no
//!   `unfiltered_html` tier and never will — [`crate::role::Capability::ManageWidgets`]
//!   gates WHO may author a widget, never WHETHER its HTML gets cleaned.
//! * The Text widget kind is escape-then-wrap ONLY (HTML-escape the raw text,
//!   then wrap it in paragraph/`<br>` markup) — never a "light HTML" mode,
//!   and it never calls this function; only Custom HTML's raw markup does.
//!
//! # Policy
//!
//! Ammonia defaults, plus:
//! * `<iframe>` is allowed, with attributes restricted to EXACTLY `{src,
//!   width, height, title, loading}` — `srcdoc`, `allow`, `name`,
//!   `allowfullscreen`, and every `on*` handler are stripped by ammonia's
//!   default-deny (none of them is ever explicitly allow-listed).
//! * `sandbox` is FORCED on every surviving `<iframe>` to [`SANDBOX_TOKENS`]:
//!   an author-supplied `sandbox` value is OVERWRITTEN, never merged, so
//!   `allow-top-navigation` can never survive.
//! * `<iframe src>` is HTTPS-ONLY: a value whose scheme is not `https` (or
//!   that has no scheme at all — a relative or protocol-relative reference)
//!   has its `src` attribute dropped outright, so the iframe survives
//!   src-less rather than pointing anywhere unvetted.
//! * `<a>`/`<img>` URLs go through ammonia's own default scheme allow-list,
//!   UNCHANGED — only `<iframe src>` gets the stricter https-only rule.
//! * Links get `rel="noopener noreferrer"` — ammonia's own default, pinned
//!   explicitly here so the policy does not silently depend on an upstream
//!   default changing under us.
//!
//! Deliberately carries NO host allow-list anywhere (CLAUDE.md §4: core ships
//! nothing consumer-specific) — every test fixture below uses a generic
//! `https://example.com` iframe src, never a real vendor host. A consumer
//! theme that embeds a specific vendor's widget runs its own real-payload
//! parity check in its own repo.

use std::borrow::Cow;

use ammonia::{Builder, Url};

/// Forced onto every `<iframe>` this sanitizer allows through, overwriting
/// whatever `sandbox` value (if any) the author supplied. No
/// `allow-top-navigation` — a sandboxed iframe can never navigate the host
/// page.
pub const SANDBOX_TOKENS: &str = "allow-scripts allow-same-origin allow-popups allow-forms";

/// Clean `raw` widget-authored HTML per the policy documented on this module.
/// Total: never panics; an empty or fully-stripped input yields `""`.
pub fn sanitize_widget_html(raw: &str) -> String {
    Builder::new()
        .add_tags(&["iframe"])
        .add_tag_attributes("iframe", &["src", "width", "height", "title", "loading"])
        .set_tag_attribute_value("iframe", "sandbox", SANDBOX_TOKENS)
        .attribute_filter(|element, attribute, value| {
            if element == "iframe" && attribute == "src" {
                // https-only: anything else (a non-https scheme, or a
                // relative/protocol-relative value that fails to parse as an
                // absolute URL at all) drops the attribute rather than
                // letting the iframe point anywhere unvetted.
                match Url::parse(value) {
                    Ok(url) if url.scheme() == "https" => Some(Cow::Borrowed(value)),
                    _ => None,
                }
            } else {
                Some(Cow::Borrowed(value))
            }
        })
        .link_rel(Some("noopener noreferrer"))
        .clean(raw)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_tags_are_stripped() {
        let out = sanitize_widget_html("<p>hi</p><script>alert(document.cookie)</script>");
        assert!(!out.contains("<script"), "{out}");
        assert!(!out.contains("alert"), "{out}"); // clean_content_tags drops the text too
        assert!(out.contains("<p>hi</p>"), "{out}");
    }

    #[test]
    fn iframe_srcdoc_is_stripped() {
        let out = sanitize_widget_html(
            r#"<iframe src="https://example.com/embed" srcdoc="<script>alert(1)</script>"></iframe>"#,
        );
        assert!(!out.contains("srcdoc"), "{out}");
        assert!(!out.contains("<script"), "{out}");
    }

    #[test]
    fn iframe_allow_is_stripped() {
        let out = sanitize_widget_html(
            r#"<iframe src="https://example.com/embed" allow="camera; microphone"></iframe>"#,
        );
        assert!(!out.contains("allow="), "{out}");
    }

    #[test]
    fn iframe_name_and_allowfullscreen_are_stripped() {
        let out = sanitize_widget_html(
            r#"<iframe src="https://example.com/embed" name="x" allowfullscreen></iframe>"#,
        );
        assert!(!out.contains("name="), "{out}");
        assert!(!out.contains("allowfullscreen"), "{out}");
    }

    #[test]
    fn on_star_handlers_are_stripped_everywhere() {
        let out = sanitize_widget_html(
            r#"<img src="https://example.com/x.png" onerror="alert(1)"><a href="/x" onclick="alert(1)">go</a><iframe src="https://example.com/embed" onload="alert(1)"></iframe>"#,
        );
        assert!(!out.contains("onerror"), "{out}");
        assert!(!out.contains("onclick"), "{out}");
        assert!(!out.contains("onload"), "{out}");
    }

    #[test]
    fn javascript_and_data_hrefs_are_rejected() {
        let out = sanitize_widget_html(r#"<a href="javascript:alert(1)">go</a>"#);
        assert!(!out.contains("javascript:"), "{out}");
        let out = sanitize_widget_html(r#"<img src="data:text/html,<script>alert(1)</script>">"#);
        assert!(!out.contains("data:"), "{out}");
    }

    #[test]
    fn javascript_and_data_iframe_srcs_are_rejected() {
        let out = sanitize_widget_html(r#"<iframe src="javascript:alert(1)"></iframe>"#);
        assert!(!out.contains("javascript:"), "{out}");
        assert!(!out.contains("src="), "{out}"); // dropped, not passed through
        let out =
            sanitize_widget_html(r#"<iframe src="data:text/html,<script>1</script>"></iframe>"#);
        assert!(!out.contains("data:"), "{out}");
    }

    #[test]
    fn embedded_tab_scheme_smuggling_is_rejected() {
        // "java\tscript:" — a naive string-prefix check misses this, but the
        // WHATWG URL parser (which both ammonia's own scheme gate and our
        // attribute_filter use) strips embedded tabs/newlines before reading
        // the scheme, so it resolves to plain "javascript:" either way.
        let out = sanitize_widget_html("<a href=\"java\tscript:alert(1)\">go</a>");
        assert!(!out.contains("javascript:"), "{out}");
        // The tab survives html5ever's serializer, so "javascript:" (contiguous)
        // never literally appears in the output EVEN IF the href passed through
        // completely unsanitized — that "contains" check alone is vacuous here
        // (unlike the untabbed literal in `javascript_and_data_hrefs_are_rejected`).
        // The real sanitizer drops the href attribute outright; a pass-through
        // regression would retain it, so this is what actually catches the bypass.
        assert!(!out.contains("href="), "{out}");
        let out = sanitize_widget_html("<iframe src=\"java\tscript:alert(1)\"></iframe>");
        assert!(!out.contains("javascript:"), "{out}");
        assert!(!out.contains("src="), "{out}");
    }

    #[test]
    fn a_non_https_iframe_src_is_dropped() {
        // http (not just javascript:/data:) fails the iframe-specific
        // https-only rule even though ammonia's own default scheme
        // allow-list would accept it for an <a>/<img>.
        let out = sanitize_widget_html(r#"<iframe src="http://example.com/embed"></iframe>"#);
        assert!(!out.contains("src="), "{out}");
    }

    #[test]
    fn a_relative_iframe_src_is_dropped() {
        let out = sanitize_widget_html(r#"<iframe src="//example.com/embed"></iframe>"#);
        assert!(!out.contains("src="), "{out}");
        let out = sanitize_widget_html(r#"<iframe src="/embed"></iframe>"#);
        assert!(!out.contains("src="), "{out}");
    }

    #[test]
    fn nested_form_mxss_shapes_do_not_survive() {
        // Classic mXSS-family shapes that abuse HTML5 parser foster-parenting
        // / namespace-switching quirks to smuggle markup across a naive
        // sanitizer's serialize/re-parse boundary. ammonia parses+serializes
        // through the same html5ever tree once, so none of this should
        // resurface as live markup.
        let out = sanitize_widget_html(
            r#"<form><math><mtext></form><form><mglyph><style></math><img src=x onerror=alert(1)>"#,
        );
        assert!(!out.contains("onerror"), "{out}");
        assert!(!out.contains("<style"), "{out}");

        let out = sanitize_widget_html(
            r#"<noscript><p title="</noscript><img src=x onerror=alert(1)>"></noscript>"#,
        );
        assert!(!out.contains("onerror"), "{out}");
    }

    #[test]
    fn sandbox_is_forced_and_author_value_is_overwritten() {
        let out = sanitize_widget_html(
            r#"<iframe src="https://example.com/embed" sandbox="allow-top-navigation"></iframe>"#,
        );
        assert!(!out.contains("allow-top-navigation"), "{out}");
        assert!(
            out.contains(&format!("sandbox=\"{SANDBOX_TOKENS}\"")),
            "{out}"
        );
    }

    #[test]
    fn sandbox_is_present_even_when_the_author_omitted_it() {
        let out = sanitize_widget_html(r#"<iframe src="https://example.com/embed"></iframe>"#);
        assert!(
            out.contains(&format!("sandbox=\"{SANDBOX_TOKENS}\"")),
            "{out}"
        );
    }

    #[test]
    fn a_generic_https_iframe_with_allowed_attrs_survives() {
        let out = sanitize_widget_html(
            r#"<iframe src="https://example.com/embed" width="560" height="315" title="Embed" loading="lazy"></iframe>"#,
        );
        assert!(out.contains("<iframe"), "{out}");
        assert!(out.contains(r#"src="https://example.com/embed""#), "{out}");
        assert!(out.contains(r#"width="560""#), "{out}");
        assert!(out.contains(r#"height="315""#), "{out}");
        assert!(out.contains(r#"title="Embed""#), "{out}");
        assert!(out.contains(r#"loading="lazy""#), "{out}");
        assert!(
            out.contains(&format!("sandbox=\"{SANDBOX_TOKENS}\"")),
            "{out}"
        );
    }

    #[test]
    fn plain_benign_markup_passes_through() {
        let out = sanitize_widget_html(
            r#"<h5>Heading</h5><p>Some <a href="https://example.com">link</a> and <img src="https://example.com/x.png" alt="x"></p>"#,
        );
        assert!(out.contains("<h5>Heading</h5>"), "{out}");
        assert!(out.contains("<p>Some"), "{out}");
        assert!(out.contains(r#"href="https://example.com""#), "{out}");
        assert!(out.contains(r#"src="https://example.com/x.png""#), "{out}");
        assert!(out.contains(r#"alt="x""#), "{out}");
    }

    #[test]
    fn links_get_noopener_noreferrer() {
        let out = sanitize_widget_html(r#"<a href="https://example.com">go</a>"#);
        assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(sanitize_widget_html(""), "");
    }
}
