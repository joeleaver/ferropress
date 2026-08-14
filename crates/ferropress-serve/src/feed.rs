//! Syndication feeds (RSS 2.0 + Atom 1.0) as a **cached listing page**.
//!
//! The feed is the second consumer of the `prerender/listing/` cache namespace the
//! home-page slice reserved (the first was `/`). It follows the SAME discipline as
//! [`CachedFront`](crate::content): a **content-stable** envelope ([`CachedFeed`]) is cached
//! once (one blob, both formats), and everything site-configuration-dependent is composed
//! **live** at request time — so a `site.url` / `site.title` / `site.tagline` edit or an author
//! rename is reflected on the next request with **no feed regeneration**. The change-driven
//! regen loop only ever **evicts** the feed (see [`ServeEngine`](crate::ServeEngine)); this
//! read path ([`serve_feed`]) is the sole populator.
//!
//! ## What is cached vs composed live
//!
//! Cached (in [`CachedFeedItem`]): the post's title, slug, uuid, excerpt, the rendered +
//! media-rewritten HTML body, and the raw `published_at` / `updated_at` millis + `author_id`.
//! Composed **live** in [`compose_rss`] / [`compose_atom`]: the channel/feed metadata (title,
//! description, links) from the current [`SiteSettings`]; the RFC-822 / RFC-3339 date strings;
//! the byline NAME resolved from the [`AuthorDirectory`]; and — crucially — the ABSOLUTE form
//! of every URL. The cached body carries the same site-relative `src="/media/{uuid}"` /
//! `href="/slug"` a permalink caches; because a feed is fetched off-origin (a reader has no base
//! URI to resolve a relative URL against), those are absolutized against the site base at
//! compose time ([`absolutize_root_relative`]) — LIVE, never baked, so the envelope stays
//! base-independent and a `site.url` change needs no eviction.
//!
//! ## The site base
//!
//! Absolute URLs need a base origin. It is resolved ([`resolve_base`]) as `site.url` when set,
//! else the request's `{scheme}://{host}` (a standard CMS fallback so feeds work out of the box
//! before `site.url` is configured), else empty (a degraded relative-URL feed — the only case a
//! reader cannot navigate; documented). The Atom feed `<id>` deliberately does NOT use the
//! request-host fallback: identity must be stable, so it derives from `site.url` (or a fixed
//! absolute `tag:` URI) only.
//!
//! ## Security
//!
//! The feed bakes user-controlled title/excerpt (raw store scalars) and the rendered body into
//! XML. Every interpolation point — text nodes AND attribute values — goes through
//! [`xml_escape`], a single-pass classifier that entity-escapes `& < > " '` and DROPS
//! XML-1.0-illegal characters (C0 controls except tab/LF/CR, and U+FFFE/U+FFFF), so a stray or
//! malicious character can never make the document ill-formed (one illegal char rejects the
//! whole feed for every subscriber). The body is entity-escaped as TEXT inside
//! `<content:encoded>` / `<content type="html">` (the defined semantics of those elements) —
//! never CDATA, sidestepping the `]]>` termination hazard. The only raw HTML in a body comes
//! from `BlockKind::Custom` (operator-installed, trusted plugins), the same accepted trust
//! boundary the on-site page already carries — now also reaching reader contexts.

use std::sync::Arc;

use ferropress_core::BlockTree;
use ferropress_core::error::CoreError;
use ferropress_core::ports::{BlobKey, BlobStore};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::Value;
use ferropress_render::{CustomBlockRenderer, RenderMode};
use ferropress_render_form::SiteSettings;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::{Rfc2822, Rfc3339};

use crate::authors::AuthorDirectory;
use crate::content;
use crate::datefmt;

/// Which syndication format to serialize a [`CachedFeed`] into. Both share one cached envelope
/// and differ only in the compose step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedFormat {
    /// RSS 2.0, served at `/feed.xml`.
    Rss,
    /// Atom 1.0 (RFC 4287), served at `/feed.atom`.
    Atom,
}

impl FeedFormat {
    /// The `Content-Type` (with charset) the HTTP handler must set for this format.
    pub fn content_type(self) -> &'static str {
        match self {
            FeedFormat::Rss => "application/rss+xml; charset=utf-8",
            FeedFormat::Atom => "application/atom+xml; charset=utf-8",
        }
    }

    /// The feed's own site-relative path — the target of its `rel="self"` link and the HTML
    /// autodiscovery `<link>`. Single-sourced so the route, the self-link, and the chrome can
    /// never drift.
    pub fn path(self) -> &'static str {
        match self {
            FeedFormat::Rss => "/feed.xml",
            FeedFormat::Atom => "/feed.atom",
        }
    }
}

/// The cached feed envelope: content-stable rows only. Stored (serialized as JSON) at
/// [`feed_cache_key`]. ONE envelope backs BOTH formats (RSS + Atom compose from the same rows,
/// so the two feeds can never disagree). Like [`CachedFront`](crate::content), it holds nothing
/// site-configuration-dependent — the chrome, dates, byline names, and absolute URLs are all
/// composed live. `#[serde(deny_unknown_fields)]` (on the envelope AND the row) makes format
/// drift fail-closed → the read path re-renders and self-heals on first access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedFeed {
    /// The most-recent published posts, newest first (capped at `reading.feed_items`).
    pub items: Vec<CachedFeedItem>,
}

/// One content-stable feed row. The dateline is RFC-formatted, the byline NAME resolved, and
/// the body URLs absolutized LIVE at compose time — so only the raw millis + the author id +
/// the site-relative body are cached (a `site.url` edit or an author rename needs no feed
/// regeneration).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedFeedItem {
    /// The post title (`<title>`).
    pub title: String,
    /// The post slug — the permalink is `abs_url(base, "/{slug}")`, composed live.
    pub slug: String,
    /// The post's immutable public UUID — the stable, slug-independent guid / entry `<id>`
    /// (`urn:uuid:{uuid}`). Empty is tolerated: the guid/id falls back to the permalink.
    pub uuid: String,
    /// A short summary (`<description>` / `<summary>`).
    pub excerpt: String,
    /// The rendered, media-rewritten HTML body — carries site-RELATIVE `src`/`href` (absolutized
    /// live). Goes into `<content:encoded>` / `<content type="html">`, entity-escaped as text.
    pub content: String,
    /// Publish instant (epoch millis, UTC) → RSS `<pubDate>` / Atom `<published>`. Optional: a
    /// CLI-seeded post carries no `published_at`.
    pub published_at: Option<i64>,
    /// Last-modified instant (epoch millis, UTC) → Atom `<updated>` (required; coalesced with
    /// `published_at`, then the epoch). Also the source of the feed-level `<updated>` /
    /// `<lastBuildDate>` (their max).
    pub updated_at: Option<i64>,
    /// The author's `User` id; the byline NAME is resolved live from the author directory.
    pub author_id: Option<u64>,
}

/// The reserved prerender-cache key for the feed envelope: `prerender/listing/feed.json`.
///
/// Deliberately a DEDICATED key, NOT `cache_key("/feed.xml")` (which would produce
/// `prerender/permalink/feed.xml.html` — the exact key a real Post slugged `feed.xml` also
/// produces, a silent two-way cache collision). It lives in the `listing/` namespace the
/// home-page slice reserved: no permalink slug can reach `listing/` (permalinks live one level
/// below, under `permalink/`), and `feed.json` never collides with the front page's
/// `listing/index.html`. The `.json` suffix marks it as an envelope, not final output.
pub(crate) fn feed_cache_key() -> BlobKey {
    BlobKey(format!("{}/listing/feed.json", crate::CACHE_PREFIX))
}

/// Build the [`CachedFeed`] envelope: the most-recent published posts (capped at
/// `settings.feed_items`), each with its rendered body.
///
/// Reuses [`content::recent_published_posts`] (the SAME scan/filter/sort/truncate + batched
/// author read the front-page galley uses, so the feed and galley can never drift on membership)
/// and renders each body through [`content::render_body`] (the one-shared-renderer path, so a
/// feed item's HTML is byte-identical to its permalink's). A row whose `block_tree` is
/// missing/non-Json/unparseable is logged and SKIPPED — never aborting the whole feed (the same
/// per-object tolerance the plugin-eviction scan uses).
///
/// `pub(crate)` so the read path ([`serve_feed`]) builds it on a miss. The regen loop never
/// calls this — it only *evicts* the feed.
pub(crate) async fn build_feed(
    store: &Arc<dyn RhypeStore>,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
) -> Result<CachedFeed, CoreError> {
    // The feed does not (yet) surface term chips — `recent_published_posts` also batches term
    // links for the galley/single-page chip resolution, ignored here.
    let (objects, author_links, _term_links) =
        content::recent_published_posts(store, settings.feed_items as usize).await?;

    let mut items = Vec::with_capacity(objects.len());
    for (obj, author_ids) in objects.iter().zip(author_links.iter()) {
        let tree = match obj.get("block_tree") {
            Some(Value::Json(json)) => match BlockTree::from_json_value(json.clone()) {
                Ok(tree) => tree,
                Err(e) => {
                    tracing::warn!(
                        error = %e, object_id = obj.id.0,
                        "skipping a feed post whose block tree failed to parse",
                    );
                    continue;
                }
            },
            // No block tree at all -> nothing to syndicate for this row; skip.
            _ => continue,
        };
        items.push(CachedFeedItem {
            title: content::str_field(obj, "title"),
            slug: content::str_field(obj, "slug"),
            uuid: content::str_field(obj, "uuid"),
            excerpt: content::str_field(obj, "excerpt"),
            content: content::render_body(&tree, RenderMode::Publish, custom),
            published_at: obj.get("published_at").and_then(Value::as_datetime),
            updated_at: obj.get("updated_at").and_then(Value::as_datetime),
            author_id: author_ids.first().map(|id| id.0),
        });
    }
    Ok(CachedFeed { items })
}

/// Cache-first resolution of the syndication feed — the static-first hot path, mirroring
/// [`serve_front`](crate::content) for `/`.
///
/// 1. Read the prerender cache ([`feed_cache_key`]). A hit deserializes the [`CachedFeed`] and
///    composes the requested `format` live (no store scan, no body re-render). A corrupt/legacy
///    entry that fails to deserialize is treated as a miss (self-heal).
/// 2. On a miss, [`build_feed`] the envelope, write it *through* to the cache, and compose.
///
/// Returns the composed XML for `format`. **Best-effort** cache: a blob read/write fault degrades
/// to a live build, never an error. The regen loop keeps the feed fresh by *evicting* it on a
/// reshaping change; this read path is the sole *populator*, so there is no eager-regen writer to
/// race a PUT against (a narrow read-vs-evict window remains — the same class
/// [`serve_front`](crate::content) documents — self-clearing on the next reshaping change).
///
/// `request_origin` is the request's `{scheme}://{host}` (from the HTTP handler), used as the
/// absolute-base fallback when `site.url` is unset. See [`resolve_base`].
pub async fn serve_feed(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
    custom: &dyn CustomBlockRenderer,
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    format: FeedFormat,
    request_origin: Option<&str>,
) -> Result<String, CoreError> {
    let key = feed_cache_key();

    let cached = match blobs.get(&key).await {
        Ok(bytes) => match serde_json::from_slice::<CachedFeed>(&bytes) {
            Ok(feed) => Some(feed),
            Err(e) => {
                tracing::warn!(error = %e, "feed cache entry not in the current format; re-rendering");
                None
            }
        },
        Err(CoreError::NotFound { .. }) => None,
        Err(e) => {
            tracing::warn!(error = %e, "feed cache read failed; falling back to a live build");
            None
        }
    };

    let feed = match cached {
        Some(feed) => feed,
        None => {
            let built = build_feed(store, custom, settings).await?;
            match serde_json::to_vec(&built) {
                Ok(bytes) => {
                    if let Err(e) = blobs.put(&key, bytes).await {
                        tracing::warn!(error = %e, "feed cache write-through failed; serving uncached build");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not serialize the feed envelope for the cache; serving uncached build");
                }
            }
            built
        }
    };

    let base = resolve_base(settings, request_origin);
    Ok(match format {
        FeedFormat::Rss => compose_rss(settings, authors, &feed, &base),
        FeedFormat::Atom => compose_atom(settings, authors, &feed, &base),
    })
}

// ─── base-URL resolution ──────────────────────────────────────────────────────────────────

/// The absolute base origin for the feed's navigational URLs (channel/feed link, `rel="self"`,
/// item permalinks, and in-body absolutization).
///
/// `site.url` (trailing `/` trimmed) when set; else the request's `{scheme}://{host}` — a
/// standard CMS fallback so a fresh site's feed is still navigable before `site.url` is
/// configured; else `""` (a degraded relative-URL feed, the only case a reader cannot resolve
/// links from). NOT used for the Atom feed `<id>` — see [`feed_identity`].
fn resolve_base(settings: &SiteSettings, request_origin: Option<&str>) -> String {
    let site = settings.url.trim().trim_end_matches('/');
    if !site.is_empty() {
        return site.to_owned();
    }
    match request_origin {
        Some(origin) if !origin.trim().is_empty() => origin.trim().trim_end_matches('/').to_owned(),
        _ => String::new(),
    }
}

/// Join an absolute `base` and a root-relative `path` (`"/slug"`). When `base` is empty, returns
/// the path unchanged (degraded relative URL). `base` is already trailing-`/`-trimmed and `path`
/// is leading-`/`-prefixed, so the join introduces exactly one separator.
fn abs_url(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_owned()
    } else {
        format!("{base}{path}")
    }
}

/// A stable, ABSOLUTE identity IRI for the Atom feed `<id>` (RFC 4287 §4.2.6 requires an
/// absolute IRI; a relative ref is invalid). Uses `site.url` when set (stable per origin); else a
/// fixed `tag:` URI (RFC 4151) — absolute and unchanging, so the feed identity survives before
/// `site.url` is configured (unlike the navigational base, identity must NOT follow the request
/// host, which would re-identify the feed per access host).
fn feed_identity(settings: &SiteSettings) -> String {
    let site = settings.url.trim().trim_end_matches('/');
    if site.is_empty() {
        "tag:ferropress,2020:/feed.atom".to_owned()
    } else {
        format!("{site}/feed.atom")
    }
}

// ─── XML helpers ──────────────────────────────────────────────────────────────────────────

/// Entity-escape `& < > " '` and DROP XML-1.0-illegal characters, in a SINGLE pass.
///
/// The sole escaper for both text nodes and attribute values — one correctly-ordered pass (each
/// char mapped independently, so no double-escape is possible; sequential `String::replace`
/// would turn `<`→`&lt;`→`&amp;lt;`). It also drops characters outside XML 1.0's `Char`
/// production: C0 controls except tab/LF/CR (`< 0x20`), and the two BMP noncharacters
/// U+FFFE/U+FFFF (valid Rust scalars, outside the C0 range, not among the five entities). One
/// such character makes the ENTIRE document ill-formed — a conformant reader rejects the whole
/// feed for every subscriber — so dropping them is a well-formedness guarantee, not cosmetic.
/// (Surrogates cannot occur in a Rust `str`; all `>= 0x10000` scalars are legal `Char`s.)
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            // Other C0 controls + the two illegal BMP noncharacters: XML-illegal, dropped.
            c if (c as u32) < 0x20 => {}
            '\u{FFFE}' | '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Absolutize root-relative `src`/`href` attribute values in a rendered HTML body against
/// `base`, at COMPOSE time (never baked — keeps the cached body base-independent).
///
/// A feed is fetched off-origin, so a reader has no base URI to resolve `src="/media/{uuid}"` or
/// `href="/slug"` against → images and internal links break. This prefixes `base` onto any
/// `src`/`href` value that begins with a single `/`. Left untouched: protocol-relative `//host`,
/// fragments `#`, `mailto:`/`https:`/other schemes, and already-absolute URLs (none start with a
/// lone `/`). A no-op when `base` is empty (degraded relative feed).
///
/// **Only rewrites inside a start tag** (`<…>`). The rewrite is done per-tag, never over the
/// text between tags: the renderer escapes literal angle brackets in TEXT and CODE to
/// `&lt;`/`&gt;` (`html_escape::encode_text`), so a `src="/…"` a reader typed as prose or pasted
/// into a code sample is never inside a real tag and is correctly skipped — only genuine element
/// attributes absolutize. (Within a tag, attribute VALUES have their `"` escaped to `&quot;`, so
/// a literal `src="/` there can't occur either; the only matches are real attributes.)
///
/// Runs BEFORE [`xml_escape`] (an HTML-level rewrite on the raw body, then the whole string is
/// text-escaped into the content element).
fn absolutize_root_relative(html: &str, base: &str) -> String {
    if base.is_empty() {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        // Text before the tag (prose / escaped code) is NEVER rewritten.
        out.push_str(&rest[..lt]);
        let from_lt = &rest[lt..];
        match from_lt.find('>') {
            Some(gt) => {
                // Rewrite `src`/`href` only within this `<…>` start tag.
                let tag = &from_lt[..=gt];
                let step = prefix_root_relative(tag, "src", base);
                out.push_str(&prefix_root_relative(&step, "href", base));
                rest = &from_lt[gt + 1..];
            }
            // Unterminated `<` (not expected from the renderer): emit the remainder verbatim.
            None => {
                out.push_str(from_lt);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Prefix `base` onto every `{attr}="/..."` value that is root-relative (a single leading `/`,
/// not protocol-relative `//`). One pass; leaves all other values untouched. Helper for
/// [`absolutize_root_relative`].
fn prefix_root_relative(html: &str, attr: &str, base: &str) -> String {
    let head = format!("{attr}=\"");
    let needle = format!("{head}/"); // e.g. `src="/`
    if !html.contains(&needle) {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len() + base.len());
    let mut rest = html;
    while let Some(pos) = rest.find(&needle) {
        let after = pos + needle.len(); // index just past the leading '/'
        if rest[after..].starts_with('/') {
            // Protocol-relative `//host` — leave untouched; copy through the needle and advance.
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            continue;
        }
        // Rewrite `{attr}="/...` -> `{attr}="{base}/...`.
        out.push_str(&rest[..pos]);
        out.push_str(&head);
        out.push_str(base);
        out.push('/');
        rest = &rest[after..];
    }
    out.push_str(rest);
    out
}

/// Format epoch millis as an RFC-2822 date (RSS `<pubDate>` / `<lastBuildDate>`), or `None` if
/// the instant is out of the formatter's range (clamp-to-epoch already applied). A `None` lets
/// the caller OMIT the optional date element rather than crash — no stored date can 500 the feed.
fn rfc2822(millis: i64) -> Option<String> {
    datefmt::millis_to_utc(millis).format(&Rfc2822).ok()
}

/// Format epoch millis as an RFC-3339 date (Atom `<published>` / `<updated>`), or `None` on a
/// range error. See [`rfc2822`].
fn rfc3339(millis: i64) -> Option<String> {
    datefmt::millis_to_utc(millis).format(&Rfc3339).ok()
}

/// The feed-level modified instant = the max of each item's `updated_at` (coalesced with
/// `published_at`); the Unix epoch when the feed is empty. Content-derived and deterministic
/// (NOT request-time "now"), so two composes of the same cached envelope agree.
fn feed_updated_millis(feed: &CachedFeed) -> i64 {
    feed.items
        .iter()
        .filter_map(|i| i.updated_at.or(i.published_at))
        .max()
        .unwrap_or(0)
}

/// The stable id for an item, and whether it is itself a permalink (RSS `guid isPermaLink`).
///
///   * `urn:uuid:{uuid}` when the post carries its immutable uuid (the common case — permanent,
///     slug-independent, always absolute) → non-permalink.
///   * the absolute permalink when the uuid is somehow empty but a `base` is resolvable → a true
///     permalink.
///   * a stable absolute `tag:` URI when uuid AND base are both empty → non-permalink. A bare
///     relative `/slug` here would violate RFC 4287 §4.2.6 (atom:id MUST be an absolute IRI) and
///     would misrepresent an RSS guid as a resolvable permalink; the `tag:` fallback keeps the id
///     absolute in every configuration, mirroring [`feed_identity`].
fn item_id(item: &CachedFeedItem, base: &str) -> (String, bool) {
    if !item.uuid.trim().is_empty() {
        (format!("urn:uuid:{}", item.uuid), false)
    } else if base.is_empty() {
        (format!("tag:ferropress,2020:/{}", item.slug), false)
    } else {
        (abs_url(base, &format!("/{}", item.slug)), true)
    }
}

/// The byline display name for a row, resolved LIVE from the author directory (`None` when the
/// post is author-less or the id is unknown — e.g. a since-deleted user).
fn author_name<'a>(item: &CachedFeedItem, authors: &'a AuthorDirectory) -> Option<&'a str> {
    item.author_id.and_then(|id| authors.name(id))
}

// ─── RSS 2.0 ────────────────────────────────────────────────────────────────────────────────

/// Compose the RSS 2.0 document from the cached envelope, live. Channel metadata comes from the
/// current settings; dates, byline names, and absolute URLs are composed here.
fn compose_rss(
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    feed: &CachedFeed,
    base: &str,
) -> String {
    let title = settings.title_or_default();
    // A fresh site has an empty tagline; fall back to the title so the feed is never subtitle-less.
    let description = if settings.tagline.trim().is_empty() {
        title
    } else {
        settings.tagline.as_str()
    };
    let site_link = if base.is_empty() { "/" } else { base };
    let self_link = abs_url(base, FeedFormat::Rss.path());

    let mut s = String::with_capacity(1024 + feed.items.len() * 512);
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    s.push_str(
        "<rss version=\"2.0\" \
xmlns:content=\"http://purl.org/rss/1.0/modules/content/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:atom=\"http://www.w3.org/2005/Atom\">\n",
    );
    s.push_str("<channel>\n");
    s.push_str(&format!("<title>{}</title>\n", xml_escape(title)));
    s.push_str(&format!("<link>{}</link>\n", xml_escape(site_link)));
    s.push_str(&format!(
        "<description>{}</description>\n",
        xml_escape(description)
    ));
    s.push_str("<language>en</language>\n");
    s.push_str("<generator>Ferropress</generator>\n");
    s.push_str(&format!(
        "<atom:link href=\"{}\" rel=\"self\" type=\"application/rss+xml\"/>\n",
        xml_escape(&self_link)
    ));
    if let Some(built) = rfc2822(feed_updated_millis(feed)) {
        s.push_str(&format!(
            "<lastBuildDate>{}</lastBuildDate>\n",
            xml_escape(&built)
        ));
    }

    for item in &feed.items {
        let permalink = abs_url(base, &format!("/{}", item.slug));
        let (id, is_permalink) = item_id(item, base);
        let body = xml_escape(&absolutize_root_relative(&item.content, base));

        s.push_str("<item>\n");
        s.push_str(&format!("<title>{}</title>\n", xml_escape(&item.title)));
        s.push_str(&format!("<link>{}</link>\n", xml_escape(&permalink)));
        s.push_str(&format!(
            "<guid isPermaLink=\"{}\">{}</guid>\n",
            is_permalink,
            xml_escape(&id)
        ));
        if let Some(date) = item.published_at.or(item.updated_at).and_then(rfc2822) {
            s.push_str(&format!("<pubDate>{}</pubDate>\n", xml_escape(&date)));
        }
        if let Some(name) = author_name(item, authors) {
            s.push_str(&format!("<dc:creator>{}</dc:creator>\n", xml_escape(name)));
        }
        if !item.excerpt.trim().is_empty() {
            s.push_str(&format!(
                "<description>{}</description>\n",
                xml_escape(&item.excerpt)
            ));
        }
        s.push_str(&format!("<content:encoded>{body}</content:encoded>\n"));
        s.push_str("</item>\n");
    }

    s.push_str("</channel>\n</rss>\n");
    s
}

// ─── Atom 1.0 (RFC 4287) ───────────────────────────────────────────────────────────────────

/// Compose the Atom 1.0 document from the cached envelope, live.
fn compose_atom(
    settings: &SiteSettings,
    authors: &AuthorDirectory,
    feed: &CachedFeed,
    base: &str,
) -> String {
    let title = settings.title_or_default();
    let subtitle = if settings.tagline.trim().is_empty() {
        title
    } else {
        settings.tagline.as_str()
    };
    let site_link = if base.is_empty() { "/" } else { base };
    let self_link = abs_url(base, FeedFormat::Atom.path());
    // Feed-level <updated> is REQUIRED; feed_updated_millis is never absent (epoch fallback).
    let updated =
        rfc3339(feed_updated_millis(feed)).unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());

    let mut s = String::with_capacity(1024 + feed.items.len() * 512);
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    s.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"en\">\n");
    s.push_str(&format!("<title>{}</title>\n", xml_escape(title)));
    s.push_str(&format!("<subtitle>{}</subtitle>\n", xml_escape(subtitle)));
    s.push_str(&format!(
        "<id>{}</id>\n",
        xml_escape(&feed_identity(settings))
    ));
    s.push_str(&format!("<updated>{}</updated>\n", xml_escape(&updated)));
    s.push_str(&format!(
        "<link rel=\"self\" type=\"application/atom+xml\" href=\"{}\"/>\n",
        xml_escape(&self_link)
    ));
    s.push_str(&format!(
        "<link rel=\"alternate\" href=\"{}\"/>\n",
        xml_escape(site_link)
    ));
    s.push_str("<generator>Ferropress</generator>\n");
    // Feed-level author: satisfies RFC 4287 §4.1.1 unconditionally, so an author-less ENTRY can
    // never invalidate the feed (a hard MUST). The site title is the natural feed-wide author.
    s.push_str(&format!(
        "<author><name>{}</name></author>\n",
        xml_escape(title)
    ));

    for item in &feed.items {
        let permalink = abs_url(base, &format!("/{}", item.slug));
        let (id, _) = item_id(item, base);
        let body = xml_escape(&absolutize_root_relative(&item.content, base));
        // <updated> is a REQUIRED entry child: coalesce to published_at, then the epoch.
        let entry_updated = rfc3339(item.updated_at.or(item.published_at).unwrap_or(0))
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());

        s.push_str("<entry>\n");
        s.push_str(&format!("<title>{}</title>\n", xml_escape(&item.title)));
        s.push_str(&format!("<id>{}</id>\n", xml_escape(&id)));
        s.push_str(&format!(
            "<updated>{}</updated>\n",
            xml_escape(&entry_updated)
        ));
        if let Some(date) = item.published_at.and_then(rfc3339) {
            s.push_str(&format!("<published>{}</published>\n", xml_escape(&date)));
        }
        s.push_str(&format!(
            "<link rel=\"alternate\" href=\"{}\"/>\n",
            xml_escape(&permalink)
        ));
        if let Some(name) = author_name(item, authors) {
            s.push_str(&format!(
                "<author><name>{}</name></author>\n",
                xml_escape(name)
            ));
        }
        if !item.excerpt.trim().is_empty() {
            s.push_str(&format!(
                "<summary>{}</summary>\n",
                xml_escape(&item.excerpt)
            ));
        }
        s.push_str(&format!("<content type=\"html\">{body}</content>\n"));
        s.push_str("</entry>\n");
    }

    s.push_str("</feed>\n");
    s
}

#[cfg(test)]
mod tests;
