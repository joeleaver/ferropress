//! First-class SEO fields — an ADD relative to WP core (which leaves these to
//! Yoast/RankMath in postmeta). Modeled as a value object embedded on content
//! entities, persisted as a JSON String field (`seo`). Keeping it typed here
//! means the renderer/theme can emit `<meta>` / OpenGraph / canonical / robots
//! deterministically at build time and generate sitemap.xml from `sitemap_*`.

/// Robots directives (noindex/nofollow). Defaults to fully indexable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Robots {
    pub noindex: bool,
    pub nofollow: bool,
}

/// Per-content SEO metadata. All optional; falls back to derived defaults
/// (title from `title`, description from excerpt, canonical from the permalink).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Seo {
    pub meta_title: Option<String>,
    pub meta_description: Option<String>,
    pub canonical_url: Option<String>,
    pub robots: Robots,
    pub og_title: Option<String>,
    pub og_description: Option<String>,
    pub og_image_media_id: Option<u64>,
    pub twitter_card: Option<String>,
    pub schema_type: Option<String>,
    /// Whether to include this object in the generated sitemap, and at what
    /// priority (0.0–1.0). `None` priority = default.
    pub sitemap_include: bool,
    pub sitemap_priority: Option<f32>,
}
