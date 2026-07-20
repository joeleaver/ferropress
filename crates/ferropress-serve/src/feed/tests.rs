//! Unit tests for the feed composers + helpers (no store — pure functions over a hand-built
//! [`CachedFeed`]). Well-formedness is asserted by actually PARSING the output with `roxmltree`
//! (a malformed feed fails the parse), plus targeted substring checks for specific elements,
//! escaping, and absolute-URL rewriting. The store-backed build/serve/eviction paths are covered
//! in the crate's integration tests (`crate::tests`).

use super::*;
use ferropress_render_form::SiteSettings;

/// A feed item with sensible defaults; override fields per test.
fn item(title: &str, slug: &str, uuid: &str) -> CachedFeedItem {
    CachedFeedItem {
        title: title.to_owned(),
        slug: slug.to_owned(),
        uuid: uuid.to_owned(),
        excerpt: "An excerpt.".to_owned(),
        content: "<p>Body.</p>".to_owned(),
        published_at: Some(1_700_000_000_000),
        updated_at: Some(1_700_000_500_000),
        author_id: None,
    }
}

fn settings(url: &str, title: &str, tagline: &str) -> SiteSettings {
    let mut s = SiteSettings::defaults();
    s.url = url.to_owned();
    s.title = title.to_owned();
    s.tagline = tagline.to_owned();
    s
}

/// Assert the XML parses (is well-formed); return nothing, panic with the source on failure.
fn assert_well_formed(xml: &str) {
    if let Err(e) = roxmltree::Document::parse(xml) {
        panic!("feed is not well-formed XML: {e}\n---\n{xml}");
    }
}

// ─── xml_escape ────────────────────────────────────────────────────────────────────────────

#[test]
fn xml_escape_maps_the_five_entities() {
    assert_eq!(
        xml_escape("a & b < c > d \" e ' f"),
        "a &amp; b &lt; c &gt; d &quot; e &apos; f"
    );
}

#[test]
fn xml_escape_does_not_double_escape() {
    // A single pass maps each char independently: an existing entity's '&' becomes '&amp;'.
    assert_eq!(xml_escape("&lt;"), "&amp;lt;");
}

#[test]
fn xml_escape_keeps_tab_lf_cr_but_drops_other_controls_and_noncharacters() {
    assert_eq!(xml_escape("a\tb\nc\rd"), "a\tb\nc\rd");
    // NUL, backspace, and the two illegal BMP noncharacters are dropped entirely.
    assert_eq!(xml_escape("x\u{0}\u{8}\u{FFFE}\u{FFFF}y"), "xy");
    // A dropped illegal char must not make the doc ill-formed even inside a title.
    let mut it = item("bad\u{FFFF}title", "s", "u");
    it.author_id = None;
    let feed = CachedFeed { items: vec![it] };
    let rss = compose_rss(
        &settings("https://e.com", "S", ""),
        &AuthorDirectory::default(),
        &feed,
        "https://e.com",
    );
    assert_well_formed(&rss);
    assert!(rss.contains("<title>badtitle</title>"));
}

// ─── absolutize_root_relative ───────────────────────────────────────────────────────────────

#[test]
fn absolutize_rewrites_root_relative_src_and_href() {
    let html = r#"<img src="/media/abc"><a href="/hello">x</a>"#;
    let out = absolutize_root_relative(html, "https://e.com");
    assert!(out.contains(r#"src="https://e.com/media/abc""#), "{out}");
    assert!(out.contains(r#"href="https://e.com/hello""#), "{out}");
}

#[test]
fn absolutize_leaves_non_root_relative_untouched() {
    let html = concat!(
        r#"<a href="//cdn.example/x">a</a>"#,
        r##"<a href="#frag">b</a>"##,
        r#"<a href="mailto:x@e.com">c</a>"#,
        r#"<a href="https://other/x">d</a>"#,
        r#"<img src="//cdn/y">"#,
    );
    // With a base, none of these root-relative-looking-but-not values change.
    assert_eq!(absolutize_root_relative(html, "https://e.com"), html);
}

#[test]
fn absolutize_is_a_noop_with_empty_base() {
    let html = r#"<img src="/media/abc">"#;
    assert_eq!(absolutize_root_relative(html, ""), html);
}

#[test]
fn absolutize_only_rewrites_inside_tags_not_prose_or_code() {
    let base = "https://e.com";
    // A genuine attribute inside a tag IS absolutized.
    assert_eq!(
        absolutize_root_relative(r#"<img src="/media/x">"#, base),
        r#"<img src="https://e.com/media/x">"#
    );
    // Prose (a text node) that merely CONTAINS the literal `src="/` must NOT be rewritten: the
    // renderer leaves `"` and `/` unescaped in text, so a naive substring pass would corrupt it.
    let prose = r#"<p>Set src="/logo.png" in your config.</p>"#;
    assert_eq!(absolutize_root_relative(prose, base), prose);
    // An HTML code sample escapes its angle brackets (`&lt;`/`&gt;`), so its `href="/…"` is never
    // inside a real tag — it must survive verbatim.
    let code = r#"<pre><code>&lt;link href="/style.css"&gt;</code></pre>"#;
    assert_eq!(absolutize_root_relative(code, base), code);
}

// ─── base / url helpers ─────────────────────────────────────────────────────────────────────

#[test]
fn resolve_base_prefers_site_url_then_request_origin() {
    let s = settings("https://site.example/", "S", "");
    // site.url wins (trailing slash trimmed) even when a request origin is present.
    assert_eq!(
        resolve_base(&s, Some("http://req.host")),
        "https://site.example"
    );
    // Empty site.url falls back to the request origin (also trimmed).
    let s0 = settings("", "S", "");
    assert_eq!(
        resolve_base(&s0, Some("http://req.host/")),
        "http://req.host"
    );
    // Neither → empty (degraded relative feed).
    assert_eq!(resolve_base(&s0, None), "");
}

#[test]
fn abs_url_joins_or_degrades() {
    assert_eq!(abs_url("https://e.com", "/a/b"), "https://e.com/a/b");
    assert_eq!(abs_url("", "/a/b"), "/a/b");
}

#[test]
fn feed_identity_is_absolute_even_without_site_url() {
    assert_eq!(
        feed_identity(&settings("https://e.com/", "S", "")),
        "https://e.com/feed.atom"
    );
    // No site.url → a fixed absolute tag: URI (never the request host, never relative).
    assert_eq!(
        feed_identity(&settings("", "S", "")),
        "tag:ferropress,2020:/feed.atom"
    );
}

#[test]
fn item_id_uses_urn_uuid_or_falls_back_to_permalink() {
    let with_uuid = item("t", "s", "018f-uuid");
    assert_eq!(
        item_id(&with_uuid, "https://e.com"),
        ("urn:uuid:018f-uuid".to_owned(), false)
    );
    let no_uuid = item("t", "hello", "");
    assert_eq!(
        item_id(&no_uuid, "https://e.com"),
        ("https://e.com/hello".to_owned(), true)
    );
    // No uuid AND no base → a stable ABSOLUTE tag: URI (never a relative "/hello"), non-permalink,
    // so the Atom entry <id> stays a valid absolute IRI (RFC 4287 §4.2.6) even in the degraded case.
    assert_eq!(
        item_id(&no_uuid, ""),
        ("tag:ferropress,2020:/hello".to_owned(), false)
    );
}

#[test]
fn feed_updated_is_max_of_coalesced_item_dates_else_epoch() {
    let mut a = item("a", "a", "ua");
    a.updated_at = Some(100);
    a.published_at = Some(50);
    let mut b = item("b", "b", "ub");
    b.updated_at = None;
    b.published_at = Some(300); // coalesces to published_at
    assert_eq!(feed_updated_millis(&CachedFeed { items: vec![a, b] }), 300);
    assert_eq!(feed_updated_millis(&CachedFeed { items: vec![] }), 0);
}

// ─── RSS compose ────────────────────────────────────────────────────────────────────────────

#[test]
fn rss_is_well_formed_with_channel_and_item_elements() {
    let mut it = item("First & Best <post>", "welcome", "uuid-1");
    it.author_id = Some(7);
    it.content = r#"<p>See <img src="/media/pic"></p>"#.to_owned();
    let feed = CachedFeed { items: vec![it] };
    let authors = AuthorDirectory::from_pairs([(7, "Ada Lovelace".to_owned())]);
    let base = "https://press.example";
    let rss = compose_rss(
        &settings(base, "The Press", "Ink & metal"),
        &authors,
        &feed,
        base,
    );

    assert_well_formed(&rss);
    assert!(rss.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    // Channel: escaped title + tagline, self link, language, generator.
    assert!(rss.contains("<title>The Press</title>"));
    assert!(rss.contains("<description>Ink &amp; metal</description>"));
    assert!(rss.contains(r#"<atom:link href="https://press.example/feed.xml" rel="self""#));
    assert!(rss.contains("<language>en</language>"));
    // Item: escaped title, absolute permalink, urn:uuid guid, dc:creator, absolutized body.
    assert!(rss.contains("<title>First &amp; Best &lt;post&gt;</title>"));
    assert!(rss.contains("<link>https://press.example/welcome</link>"));
    assert!(rss.contains(r#"<guid isPermaLink="false">urn:uuid:uuid-1</guid>"#));
    assert!(rss.contains("<dc:creator>Ada Lovelace</dc:creator>"));
    // The in-body /media URL is absolutized (live) then escaped as text.
    assert!(
        rss.contains("&lt;img src=&quot;https://press.example/media/pic&quot;&gt;"),
        "body not absolutized+escaped: {rss}"
    );
    // pubDate is RFC-822 (4-digit year).
    assert!(rss.contains("<pubDate>") && rss.contains("2023"));
}

#[test]
fn rss_description_falls_back_to_title_when_tagline_empty() {
    let feed = CachedFeed { items: vec![] };
    let rss = compose_rss(
        &settings("https://e.com", "Solo Title", ""),
        &AuthorDirectory::default(),
        &feed,
        "https://e.com",
    );
    assert_well_formed(&rss);
    assert!(rss.contains("<description>Solo Title</description>"));
}

// ─── Atom compose ───────────────────────────────────────────────────────────────────────────

#[test]
fn atom_is_well_formed_with_required_elements_and_feed_author() {
    // An author-LESS entry (author_id None) must still yield a valid feed via the feed-level author.
    let it = item("A Title", "a-title", "uuid-9");
    let feed = CachedFeed { items: vec![it] };
    let base = "https://press.example";
    let atom = compose_atom(
        &settings(base, "The Press", "sub"),
        &AuthorDirectory::default(),
        &feed,
        base,
    );

    assert_well_formed(&atom);
    assert!(atom.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    // Feed-level required id/title/updated + strongly-recommended self/alternate + M3 author.
    assert!(atom.contains("<id>https://press.example/feed.atom</id>"));
    assert!(atom.contains("<title>The Press</title>"));
    assert!(atom.contains("<updated>"));
    assert!(atom.contains(
        r#"<link rel="self" type="application/atom+xml" href="https://press.example/feed.atom"/>"#
    ));
    assert!(atom.contains(r#"<link rel="alternate" href="https://press.example"/>"#));
    assert!(atom.contains("<author><name>The Press</name></author>"));
    // Entry required id/title/updated + content type=html + alternate link.
    assert!(atom.contains("<id>urn:uuid:uuid-9</id>"));
    assert!(atom.contains(r#"<link rel="alternate" href="https://press.example/a-title"/>"#));
    assert!(atom.contains(r#"<content type="html">"#));
}

#[test]
fn atom_entry_author_is_emitted_when_resolved() {
    let mut it = item("t", "s", "u");
    it.author_id = Some(3);
    let feed = CachedFeed { items: vec![it] };
    let authors = AuthorDirectory::from_pairs([(3, "Grace Hopper".to_owned())]);
    let atom = compose_atom(
        &settings("https://e.com", "S", ""),
        &authors,
        &feed,
        "https://e.com",
    );
    assert_well_formed(&atom);
    // Both the feed-level author (site) and the entry-level author (resolved) are present.
    assert!(atom.contains("<author><name>Grace Hopper</name></author>"));
}

// ─── empty feed + format metadata ───────────────────────────────────────────────────────────

#[test]
fn empty_feed_is_well_formed_in_both_formats() {
    let feed = CachedFeed { items: vec![] };
    let s = settings("https://e.com", "S", "");
    let rss = compose_rss(&s, &AuthorDirectory::default(), &feed, "https://e.com");
    let atom = compose_atom(&s, &AuthorDirectory::default(), &feed, "https://e.com");
    assert_well_formed(&rss);
    assert_well_formed(&atom);
    // Atom feed <updated> is required even when empty (epoch fallback).
    assert!(atom.contains("<updated>"));
}

#[test]
fn empty_base_produces_still_well_formed_feed() {
    // No site.url and no request origin: the feed is degraded (relative links) but must not be
    // malformed.
    let it = item("t", "s", "u");
    let feed = CachedFeed { items: vec![it] };
    let s = settings("", "S", "");
    let rss = compose_rss(&s, &AuthorDirectory::default(), &feed, "");
    let atom = compose_atom(&s, &AuthorDirectory::default(), &feed, "");
    assert_well_formed(&rss);
    assert_well_formed(&atom);
    // Atom id still absolute via the tag: fallback (never a relative ref).
    assert!(atom.contains("<id>tag:ferropress,2020:/feed.atom</id>"));
}

#[test]
fn format_content_types_and_paths() {
    assert_eq!(
        FeedFormat::Rss.content_type(),
        "application/rss+xml; charset=utf-8"
    );
    assert_eq!(
        FeedFormat::Atom.content_type(),
        "application/atom+xml; charset=utf-8"
    );
    assert_eq!(FeedFormat::Rss.path(), "/feed.xml");
    assert_eq!(FeedFormat::Atom.path(), "/feed.atom");
}

#[test]
fn feed_cache_key_lands_in_listing_namespace_never_permalink() {
    // Guardrail: the feed envelope must live under listing/ (unreachable by any post slug),
    // never under permalink/ where cache_key("/feed.xml") would collide with a slug "feed.xml".
    let key = feed_cache_key().0;
    assert_eq!(key, "prerender/listing/feed.json");
    assert!(!key.contains("permalink/"));
}

#[test]
fn rss_dates_are_rfc2822_and_atom_dates_are_rfc3339() {
    // Sanity: a known instant formats in the expected style for each feed.
    let ms = 1_700_000_000_000; // 2023-11-14T22:13:20Z
    let r = rfc2822(ms).expect("rfc2822");
    assert!(
        r.contains("2023") && (r.contains("+0000") || r.contains("GMT")),
        "{r}"
    );
    let a = rfc3339(ms).expect("rfc3339");
    assert!(a.starts_with("2023-11-14T22:13:20"), "{a}");
}
