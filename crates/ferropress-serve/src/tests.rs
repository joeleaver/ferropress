//! Tests for the static-first prerender cache + the regen write-through.
//!
//! These drive the REAL collaborators — a `tempfile`-isolated [`EmbeddedStore`]
//! and a [`LocalFsBlobStore`] over the same crate's `default_theme()` — so the
//! cache read-through, the cache hit, and the regen write-through/eviction are
//! proven against actual store + blob backends, not mocks.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ferropress_core::hook::{HookDispatcher, HookEvent, HookKind};
use ferropress_core::ports::BlobStore;
use ferropress_core::query::{Change, ChangeKind, Edge};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{
    Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, PAGE_TYPE, POST_TYPE, REDIRECT_TYPE,
    SETTING_TYPE, Status, USER_TYPE,
};
use ferropress_render_form::SiteSettings;

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;
use ferropress_theme::ThemeEngine;

use ferropress_render::NoCustomBlocks;

use crate::{
    AuthorDirectory, AuthorsHandle, OutputPage, ServeEngine, SettingsHandle, cache_key, content,
    serve_path,
};

const PARAGRAPH_TEXT: &str = "Hello from the Ferropress cache test.";
const SLUG: &str = "hello-world";

/// The block-tree JSON for a one-paragraph body. Built via the domain types so
/// it round-trips through `BlockTree::from_json_value` exactly.
fn paragraph_block_tree_json() -> serde_json::Value {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000TEST".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: PARAGRAPH_TEXT.to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }]);
    tree.to_json_value().expect("block tree serializes to JSON")
}

/// Boot a real embedded store + local-FS blobs + the shared default theme into a
/// `tempfile` dir. Returns the three handles the cache/regen paths take.
fn boot(dir: &Path) -> (Arc<dyn RhypeStore>, Arc<dyn BlobStore>, Arc<ThemeEngine>) {
    let store: Arc<dyn RhypeStore> =
        Arc::new(EmbeddedStore::open(dir.join("db")).expect("open embedded store"));
    let blobs: Arc<dyn BlobStore> = Arc::new(LocalFsBlobStore::new(dir.join("blobs")));
    let theme = Arc::new(content::default_theme().expect("default theme builds"));
    (store, blobs, theme)
}

/// Seed one Post with the given slug + status + a single-paragraph body; return
/// its id (so a test can `update` its status later). Only the fields the serve
/// path reads are populated.
async fn seed_post(store: &Arc<dyn RhypeStore>, slug: &str, status: Status) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Hello World".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );

    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a post must succeed")
}

/// A synthetic change matching what the embedded adapter publishes: the right
/// kind/type/id, and `fields: None` (the regen loop must re-`get` to read the
/// slug, exactly as in production).
fn change(kind: ChangeKind, id: ObjectId) -> Change {
    Change {
        version: 1,
        kind,
        type_name: TypeName::from(POST_TYPE),
        object_id: id,
        fields: None,
        origin: None,
    }
}

/// Seed one `Setting` row, matching how the admin API stores them: `value` is a
/// JSON-encoded String (so a string setting is passed here quoted, e.g. `"\"x\""`).
/// Returns the new row's id so a test can later `delete` it (most callers ignore it).
async fn seed_setting(
    store: &Arc<dyn RhypeStore>,
    key: &str,
    json_encoded_value: &str,
) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("key".to_owned(), Value::String(key.to_owned()));
    fields.insert(
        "value".to_owned(),
        Value::String(json_encoded_value.to_owned()),
    );
    fields.insert("autoload".to_owned(), Value::Bool(true));
    store
        .create(&TypeName::from(SETTING_TYPE), fields)
        .await
        .expect("seeding a setting must succeed")
}

/// Seed one Page with the given slug + status + a single-paragraph body carrying
/// `text`, so a test can tell the page's render apart from the galley. Returns its id.
async fn seed_page(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    status: Status,
    text: &str,
) -> ObjectId {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000PAGE".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: text.to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }]);
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert(
        "title".to_owned(),
        Value::String("About the Press".to_owned()),
    );
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(tree.to_json_value().expect("page block tree serializes")),
    );
    store
        .create(&TypeName::from(PAGE_TYPE), fields)
        .await
        .expect("seeding a page must succeed")
}

/// Seed one Media row with the given uuid (a valid media token), enough for the logo
/// resolution to map its id -> `/media/{uuid}`. Returns its id.
async fn seed_media(store: &Arc<dyn RhypeStore>, uuid: &str) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
    fields.insert(
        "blob_key".to_owned(),
        Value::String(format!("media/{uuid}.png")),
    );
    fields.insert(
        "mime_type".to_owned(),
        Value::String("image/png".to_owned()),
    );
    store
        .create(&TypeName::from("Media"), fields)
        .await
        .expect("seeding a media must succeed")
}

/// A one-block body whose single block is a `Custom` block owned by `plugin` (a
/// callout, say) — so a test post/page's block tree references that plugin id and
/// `BlockTree::referenced_plugin_ids` picks it up. The `NoCustomBlocks` renderer bakes
/// a placeholder for it, which is irrelevant to eviction (eviction reads the block TREE
/// from the store, not the rendered body).
fn custom_block_tree_json(plugin: &str) -> serde_json::Value {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000CUST".to_owned(),
        kind: BlockKind::Custom {
            plugin: plugin.to_owned(),
            name: plugin.to_owned(),
            data: serde_json::json!({ "text": "hi" }),
        },
        children: Vec::new(),
    }]);
    tree.to_json_value().expect("custom block tree serializes")
}

/// Seed a Post with an explicit block tree (so a test can give it a `Custom` block).
async fn seed_post_with_block_tree(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    status: Status,
    block_tree: serde_json::Value,
) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Callout Post".to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert("block_tree".to_owned(), Value::Json(block_tree));
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a post must succeed")
}

/// Seed a Page with an explicit block tree AND a materialized `path` (a Page's cache key
/// derives from `path`, not `slug`), so a test can evict its permalink by path.
async fn seed_page_with_block_tree(
    store: &Arc<dyn RhypeStore>,
    path: &str,
    status: Status,
    block_tree: serde_json::Value,
) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(path.to_owned()));
    fields.insert("path".to_owned(), Value::String(path.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Callout Page".to_owned()));
    fields.insert("block_tree".to_owned(), Value::Json(block_tree));
    store
        .create(&TypeName::from(PAGE_TYPE), fields)
        .await
        .expect("seeding a page must succeed")
}

/// A `Setting`-typed change carrying its row `key` on the change fields — matching what the
/// engine publishes (the full merged scalar snapshot). The regen loop reads `key` to decide
/// whether the changed setting reshapes the cached front page.
fn setting_change_with_key(kind: ChangeKind, key: &str) -> Change {
    Change {
        version: 1,
        kind,
        type_name: TypeName::from(SETTING_TYPE),
        object_id: ObjectId(0),
        fields: Some(serde_json::json!({ "key": key })),
        origin: None,
    }
}

/// `load_site_settings` overlays stored `Setting`s onto the schema defaults and
/// projects the typed view the theme consumes.
#[tokio::test]
async fn load_site_settings_overlays_stored_on_defaults() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, _theme) = boot(tmp.path());

    // Before any Setting exists, every field is the schema default.
    let defaults = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(defaults.title, "");
    assert_eq!(defaults.posts_per_page, 10);
    assert_eq!(defaults.timezone, "UTC");

    seed_setting(&store, "site.title", "\"The Composing Room\"").await;
    seed_setting(&store, "reading.posts_per_page", "5").await;
    seed_setting(&store, "reading.search_engine_visible", "false").await;

    let loaded = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(loaded.title, "The Composing Room");
    assert_eq!(loaded.posts_per_page, 5);
    assert!(!loaded.search_engine_visible);
}

/// A `Setting` change on the feed refreshes the live snapshot the read path holds. A
/// CHROME-only key (`site.title`) composes live, so it does NOT bust the page cache — the
/// front page's cache-eviction dimension is covered by
/// [`setting_change_evicts_home_only_for_front_shaping_keys`].
#[tokio::test]
async fn regen_loop_refreshes_settings_snapshot_on_setting_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    let handle = SettingsHandle::new(SiteSettings::defaults());
    assert_eq!(handle.current().title_or_default(), "Ferropress");

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(handle.clone());

    // A settings edit lands in the store, then the change arrives on the feed carrying its key.
    seed_setting(&store, "site.title", "\"Live Title\"").await;
    engine
        .apply_change(&setting_change_with_key(ChangeKind::Update, "site.title"))
        .await
        .expect("a Setting change must refresh cleanly");

    // The read path's handle now sees the new value — no cache regeneration.
    assert_eq!(handle.current().title, "Live Title");
}

/// A minimal `HomeCtx`-shaped context (see `content.rs` / `themes.rs`) as JSON, enough to
/// render `home.html` so a test can assert *which* theme framed it (by a theme-only marker).
fn minimal_home_ctx() -> serde_json::Value {
    serde_json::json!({
        "page_title": "Home",
        "page_description": null,
        "site": {"title": "Site", "tagline": "", "url": "", "logo": null, "noindex": false},
        "is_home": true,
        "preview_status": null,
        "posts": []
    })
}

/// Write a minimal, valid on-disk theme (`theme.toml` + the four canonical templates) under
/// `dir/<id>/`, its base carrying `marker` so a render can be attributed to it. Local to this
/// module (`#[cfg(test)]` helpers don't cross module boundaries; the registry unit tests carry
/// their own copy).
fn write_test_theme(dir: &Path, id: &str, marker: &str) {
    let d = dir.join(id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("theme.toml"),
        format!("id = \"{id}\"\nname = \"{id} theme\"\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("base.html"),
        format!(
            "<!doctype html><html><head><title>{{{{ page_title }}}}</title><!--{marker}--></head>\
             <body>{{% block main %}}{{% endblock %}}</body></html>"
        ),
    )
    .unwrap();
    std::fs::write(
        d.join("single.html"),
        "{% extends \"base.html\" %}{% block main %}<h1>{{ title }}</h1>{{ body | safe }}{% endblock %}",
    )
    .unwrap();
    std::fs::write(
        d.join("home.html"),
        "{% extends \"base.html\" %}{% block main %}{% for post in posts %}<a href=\"{{ post.url }}\">{{ post.title }}</a>{% endfor %}{% endblock %}",
    )
    .unwrap();
    std::fs::write(
        d.join("page-wide.html"),
        "{% extends \"base.html\" %}{% block main %}<article>{{ body | safe }}</article>{% endblock %}",
    )
    .unwrap();
}

/// A `ThemeHandle` over a registry loaded from `themes_dir`, seeded at the default theme.
fn themed_handle(themes_dir: &Path) -> crate::ThemeHandle {
    crate::ThemeHandle::new(
        crate::ThemeRegistry::load_dir(themes_dir),
        ferropress_render_form::DEFAULT_THEME,
    )
    .expect("theme handle builds")
}

/// A `Setting` change to `appearance.theme` rebuilds + swaps the live theme handle the read
/// path frames pages with — with no restart. Because cached envelopes hold only theme-agnostic
/// body HTML, this evicts nothing; the next request just re-frames the same body with the new
/// theme. The twin of [`regen_loop_refreshes_settings_snapshot_on_setting_change`] for the
/// theme half of the `Setting` branch.
#[tokio::test]
async fn setting_change_swaps_the_live_theme() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let themes = tmp.path().join("themes");
    write_test_theme(&themes, "aurora", "AURORA-MARK");

    let theme = themed_handle(&themes);
    assert_eq!(theme.current_id(), ferropress_render_form::DEFAULT_THEME);

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()))
    .with_theme(theme.clone());

    // The admin writes appearance.theme=aurora (JSON-encoded), then the change arrives.
    seed_setting(&store, "appearance.theme", "\"aurora\"").await;
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "appearance.theme",
        ))
        .await
        .expect("a theme Setting change must apply cleanly");

    // The shared handle now NAMES and RENDERS the loaded disk theme — no page regeneration.
    assert_eq!(theme.current_id(), "aurora");
    let home = theme
        .current()
        .render("home.html", &minimal_home_ctx())
        .expect("home renders through the swapped theme");
    assert!(
        home.contains("AURORA-MARK"),
        "the live engine is now the aurora disk theme, not the default: {home}"
    );
}

/// A live theme swap evicts NOTHING: cached page envelopes hold theme-agnostic body HTML, so
/// switching theme must not touch the prerender cache. Guards the no-page-eviction guardrail
/// (and the doc-comment's explicit "this evicts nothing" claim) against a future edit that wires
/// the swap to an eviction, or adds `appearance.*` to a `setting_reshapes_*` gate — either of
/// which would ship green past the other three tests (they only check `current_id` / engine
/// `Arc` identity / render output).
#[tokio::test]
async fn theme_swap_evicts_no_cached_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let themes = tmp.path().join("themes");
    write_test_theme(&themes, "aurora", "AURORA-MARK");

    // Warm a cached page envelope (any key in the permalink namespace).
    let key = cache_key("/hello");
    blobs
        .put(&key, b"cached-envelope".to_vec())
        .await
        .expect("warming the cache must succeed");

    let theme = themed_handle(&themes);
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()))
    .with_theme(theme.clone());

    seed_setting(&store, "appearance.theme", "\"aurora\"").await;
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "appearance.theme",
        ))
        .await
        .expect("a theme Setting change must apply cleanly");

    // The theme actually swapped…
    assert_eq!(theme.current_id(), "aurora");
    // …yet the warmed cache entry is untouched — a swap regenerates/evicts no page.
    assert!(
        blobs.exists(&key).await.unwrap(),
        "a theme swap must evict no cached page (envelopes are theme-agnostic)",
    );
}

/// A `Setting` change that does NOT alter `appearance.theme` (here `site.title`) leaves the
/// live theme untouched — `swap_to` short-circuits on the unchanged id, so the engine is NOT
/// rebuilt (the `Arc` is pointer-identical before and after).
#[tokio::test]
async fn non_theme_setting_change_does_not_rebuild_the_theme() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let themes = tmp.path().join("themes");
    write_test_theme(&themes, "aurora", "AURORA-MARK");

    let theme = themed_handle(&themes);
    let before = theme.current(); // capture the current engine's Arc identity

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()))
    .with_theme(theme.clone());

    seed_setting(&store, "site.title", "\"Live Title\"").await;
    engine
        .apply_change(&setting_change_with_key(ChangeKind::Update, "site.title"))
        .await
        .expect("a non-theme Setting change must apply cleanly");

    // appearance.theme is unchanged (still the default), so the engine is NOT rebuilt.
    assert_eq!(theme.current_id(), ferropress_render_form::DEFAULT_THEME);
    assert!(
        Arc::ptr_eq(&before, &theme.current()),
        "the theme engine must not be rebuilt when appearance.theme did not change",
    );
}

/// Deleting the `appearance.theme` row reverts the live theme to the default: the reload sees
/// the row gone, `SiteSettings.theme` falls back to `DEFAULT_THEME`, and `swap_to` rebuilds the
/// default engine — a theme "unset" is a real state, not a stuck override.
#[tokio::test]
async fn deleting_the_theme_setting_reverts_to_the_default_theme() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let themes = tmp.path().join("themes");
    write_test_theme(&themes, "aurora", "AURORA-MARK");

    let theme = themed_handle(&themes);
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()))
    .with_theme(theme.clone());

    // Switch to the disk theme first…
    let setting_id = seed_setting(&store, "appearance.theme", "\"aurora\"").await;
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "appearance.theme",
        ))
        .await
        .expect("the theme switch must apply");
    assert_eq!(theme.current_id(), "aurora");

    // …then delete the row; the reload no longer sees it, so the theme reverts to default.
    store
        .delete(&TypeName::from(SETTING_TYPE), setting_id)
        .await
        .expect("deleting the setting row");
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Delete,
            "appearance.theme",
        ))
        .await
        .expect("a theme Setting delete must apply cleanly");
    assert_eq!(theme.current_id(), ferropress_render_form::DEFAULT_THEME);
}

/// On a cache MISS, `serve_path` renders the published post AND populates the
/// cache (read-through / write-on-miss): afterwards the cache holds the rendered
/// HTML and it equals the served body.
#[tokio::test]
async fn serve_path_read_through_populates_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);

    // Cache is empty before the first request.
    assert!(
        !blobs.exists(&key).await.unwrap(),
        "cache must be empty before the first serve",
    );

    // MISS -> render-on-demand -> populate.
    let served = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(html) => html,
        other => panic!("expected Found on a published post, got {other:?}"),
    };
    assert!(
        served.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "served body must contain the rendered paragraph; was:\n{served}",
    );

    // The cache holds the ENVELOPE (not the composed page): chrome is applied live
    // at request time, so the cache stores only the per-object body + metadata.
    let cached = blobs
        .get(&key)
        .await
        .expect("cache must be populated after a read-through miss");
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&cached).expect("cache holds a page envelope, not raw HTML");
    assert!(
        envelope.body.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "the cached envelope's body must hold the rendered paragraph; was:\n{}",
        envelope.body,
    );
    assert!(
        served.contains(&envelope.body),
        "the served page must embed the cached envelope's body",
    );
}

/// A cache HIT composes chrome around the STORED ENVELOPE — proving the hot path
/// reads the envelope from the cache and does NOT re-render the block body. The
/// sentinel body is deliberately NOT the real render, so its presence in the
/// response can only come from the cached envelope.
#[tokio::test]
async fn serve_path_cache_hit_composes_from_stored_envelope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    // Publish a post too, so a cache MISS would have rendered the real body —
    // making the sentinel the only way the assertion passes if we hit the cache.
    seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    const SENTINEL_BODY: &str = "<p>SENTINEL BODY STRAIGHT FROM THE ENVELOPE</p>";

    // Pre-put a page ENVELOPE whose body is the sentinel.
    let envelope = crate::content::CachedPage {
        title: "Cached Title".to_owned(),
        excerpt: String::new(),
        published_at: None,
        author_id: None,
        featured_image: None,
        is_post: true,
        term_ids: Vec::new(),
        template: None,
        seo: None,
        body: SENTINEL_BODY.to_owned(),
    };
    blobs
        .put(&key, serde_json::to_vec(&envelope).unwrap())
        .await
        .expect("seeding the cache entry");

    // serve_path must compose from the cached envelope, not re-render the post.
    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(html) => {
            assert!(
                html.contains(SENTINEL_BODY),
                "served body must come from the cached envelope:\n{html}",
            );
            assert!(
                !html.contains(PARAGRAPH_TEXT),
                "a cache hit must NOT re-render the real post body",
            );
            assert!(
                html.contains("<!doctype html>"),
                "chrome must be composed live around the cached body",
            );
            assert!(
                html.contains("Cached Title"),
                "the cached envelope's title appears in the composed page",
            );
        }
        other => panic!("expected Found (cache hit), got {other:?}"),
    }
}

/// One regen step (via the loop's per-change handler) WRITES THROUGH a published
/// post's HTML to the cache; flipping it to a draft and re-running the step
/// EVICTS (deletes) the cache entry.
#[tokio::test]
async fn regen_write_through_then_eviction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );
    let path = format!("/{SLUG}");
    let key = cache_key(&path);

    // Nothing cached yet.
    assert!(!blobs.exists(&key).await.unwrap(), "cache starts empty");

    // --- WRITE-THROUGH: a Create/Update of a published post regenerates it. ---
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("regen step must succeed for a published post");

    let cached = blobs
        .get(&key)
        .await
        .expect("regen must write the envelope through to the cache");
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&cached).expect("regen cache entry is a page envelope");
    assert!(
        envelope.body.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "regenerated envelope must hold the rendered body; was:\n{}",
        envelope.body,
    );
    // It matches a direct build of the same page (regen == on-demand build).
    let built = engine
        .build_page(&OutputPage { path: path.clone() })
        .await
        .expect("build_page ok")
        .expect("published page builds to Some");
    assert_eq!(
        envelope, built,
        "regen envelope must equal an on-demand build"
    );

    // --- EVICTION: flip to a draft; the next regen step deletes the entry. ---
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "status".to_owned(),
        Value::String(Status::Draft.as_str().to_owned()),
    );
    store
        .update(&TypeName::from(POST_TYPE), id, patch)
        .await
        .expect("unpublishing the post");

    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("regen step must succeed for an unpublished post");

    assert!(
        !blobs.exists(&key).await.unwrap(),
        "regen must EVICT the cache entry once the entity is unpublished",
    );
}

/// A `plugin.{id}.*` config change EVICTS exactly the cached pages whose block tree bakes a
/// `Custom` block owned by that plugin — and leaves pages that don't use it untouched.
#[tokio::test]
async fn plugin_setting_change_evicts_only_pages_using_that_plugin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    // One post BAKES a callout block; one is plain prose.
    let callout_id = seed_post_with_block_tree(
        &store,
        "callout-post",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    let plain_id = seed_post(&store, "plain-post", Status::Published).await;
    // A third post bakes a DIFFERENT plugin's block — it must survive a plugin.callout change.
    // This pins the discrimination that the `!referenced_plugin_ids().contains(id)` guard makes:
    // a bug like `!...is_empty()` would still evict this page (its plugin set is non-empty) and
    // pass every other assertion here.
    let gallery_id = seed_post_with_block_tree(
        &store,
        "gallery-post",
        Status::Published,
        custom_block_tree_json("gallery"),
    )
    .await;

    // A galley front (default settings → front_page_id = None) so the conservative `/`
    // eviction does NOT fire and we can assert precisely on the permalink blobs.
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));

    let callout_key = cache_key("/callout-post");
    let plain_key = cache_key("/plain-post");
    let gallery_key = cache_key("/gallery-post");

    // Warm all three caches with a normal content write-through.
    for id in [callout_id, plain_id, gallery_id] {
        engine
            .apply_change(&change(ChangeKind::Update, id))
            .await
            .expect("warm cache");
    }
    assert!(
        blobs.exists(&callout_key).await.unwrap(),
        "callout post cached"
    );
    assert!(blobs.exists(&plain_key).await.unwrap(), "plain post cached");
    assert!(
        blobs.exists(&gallery_key).await.unwrap(),
        "gallery post cached"
    );

    // The callout plugin's config changes.
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        !blobs.exists(&callout_key).await.unwrap(),
        "a plugin.callout.* change must EVICT the page baking a callout block",
    );
    assert!(
        blobs.exists(&plain_key).await.unwrap(),
        "a page using NO plugin must SURVIVE the plugin-config change",
    );
    assert!(
        blobs.exists(&gallery_key).await.unwrap(),
        "a page baking a DIFFERENT plugin's block must SURVIVE a plugin.callout change",
    );
}

/// A core (non-plugin) `Setting` change must regenerate NO content pages — the plugin
/// branch is gated on the key parsing to a valid plugin id, preserving the "a settings
/// change regenerates no pages" guardrail for `site.*` / `reading.*` (non-front) keys.
#[tokio::test]
async fn non_plugin_setting_change_evicts_no_content_pages() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post_with_block_tree(
        &store,
        "callout-post",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));
    let key = cache_key("/callout-post");
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("warm cache");
    assert!(blobs.exists(&key).await.unwrap(), "cached");

    engine
        .apply_change(&setting_change_with_key(ChangeKind::Update, "site.title"))
        .await
        .expect("core setting change applies");

    assert!(
        blobs.exists(&key).await.unwrap(),
        "a core (non-plugin) setting change must not evict content pages",
    );
}

/// The eviction fires on a plugin-config DELETE too, not only Create/Update: clearing a
/// plugin's config reverts `fp_get_setting` to the plugin's compiled-in default, an equally
/// stale change. (The `SETTING_TYPE` branch is deliberately kind-agnostic.)
#[tokio::test]
async fn plugin_setting_delete_change_also_evicts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post_with_block_tree(
        &store,
        "callout-post",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));
    let key = cache_key("/callout-post");
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("warm cache");
    assert!(blobs.exists(&key).await.unwrap(), "cached");

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Delete,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting delete applies");

    assert!(
        !blobs.exists(&key).await.unwrap(),
        "a plugin-config DELETE must also evict pages baking that plugin's blocks",
    );
}

/// A static front page that bakes the plugin's block: a plugin-config change must evict BOTH
/// the page's own permalink blob AND the `/` LISTING blob (a `CachedFront::Static` bakes the
/// page body under a different key than its permalink).
#[tokio::test]
async fn plugin_setting_change_evicts_the_static_front_listing_blob() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    // A published static front page that bakes a callout block.
    let page_id = seed_page_with_block_tree(
        &store,
        "home",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &page_id.0.to_string()).await;
    let settings = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(
        settings.front_page_id,
        Some(page_id.0),
        "front page configured"
    );

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings));

    // Simulate both the `/` listing blob and the page's own permalink being cached.
    let front_key = cache_key("/");
    let page_key = cache_key("/home");
    blobs
        .put(&front_key, b"cached-front".to_vec())
        .await
        .unwrap();
    blobs.put(&page_key, b"cached-page".to_vec()).await.unwrap();

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        !blobs.exists(&front_key).await.unwrap(),
        "the static-front `/` LISTING blob must be evicted when the front page bakes the plugin",
    );
    assert!(
        !blobs.exists(&page_key).await.unwrap(),
        "the front page's own permalink blob must be evicted",
    );
}

/// Without a settings handle the engine cannot identify the static front page, so it evicts
/// `/` CONSERVATIVELY on a plugin-config change — parity with `invalidate_front_for_content`'s
/// None branch (the read path caches `/` regardless of whether this engine holds a handle).
#[tokio::test]
async fn plugin_setting_change_without_settings_handle_evicts_front_conservatively() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    seed_post_with_block_tree(
        &store,
        "callout-post",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    // No `.with_settings(...)`.
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );

    let front_key = cache_key("/");
    blobs
        .put(&front_key, b"cached-front".to_vec())
        .await
        .unwrap();

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        !blobs.exists(&front_key).await.unwrap(),
        "without a settings handle, `/` must be evicted conservatively on a plugin-config change",
    );
}

/// A nested Page (multi-segment materialized `path`) is evicted by its FULL path — the
/// headline case for the `/parent/child` hierarchy. Its cache key derives from `path` (not
/// `slug`), so eviction must key on `prerender/permalink/docs/guide.html`.
#[tokio::test]
async fn plugin_setting_change_evicts_a_nested_page_by_its_full_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    seed_page_with_block_tree(
        &store,
        "docs/guide",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    // Default settings (galley front) so the conservative `/` eviction does not fire.
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));

    let nested_key = cache_key("/docs/guide");
    assert_eq!(
        nested_key.0, "prerender/permalink/docs/guide.html",
        "a nested page keys on its full materialized path"
    );
    blobs
        .put(&nested_key, b"cached-nested".to_vec())
        .await
        .unwrap();

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        !blobs.exists(&nested_key).await.unwrap(),
        "a nested page baking the plugin must be evicted by its full /parent/child path",
    );
}

/// A row whose block tree fails to PARSE must be logged-and-skipped, NEVER abort the scan —
/// else one bad row would silently leave the rest of the plugin's pages stale (a partial miss
/// worse than the pre-fix status quo). The corrupt row is seeded FIRST (lower id → scanned
/// first), so a regression that `?`-propagated the parse error would abort before reaching the
/// healthy callout page and this test would catch it.
#[tokio::test]
async fn a_malformed_block_tree_row_does_not_abort_the_plugin_eviction_scan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    // Valid JSON that does NOT deserialize into a BlockTree (`schema_version` must be a u32),
    // so `BlockTree::from_json_value` returns Err on it during the eviction scan.
    seed_post_with_block_tree(
        &store,
        "corrupt-post",
        Status::Published,
        serde_json::json!({ "schema_version": "not-a-number", "blocks": [] }),
    )
    .await;
    // A healthy callout page AFTER it (higher id → scanned later).
    let good_id = seed_post_with_block_tree(
        &store,
        "good-callout",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));

    let good_key = cache_key("/good-callout");
    engine
        .apply_change(&change(ChangeKind::Update, good_id))
        .await
        .expect("warm good cache");
    assert!(blobs.exists(&good_key).await.unwrap(), "good post cached");

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("the plugin change must apply despite a corrupt row");

    assert!(
        !blobs.exists(&good_key).await.unwrap(),
        "the corrupt row must be skipped (not abort the scan) so the healthy callout page is still evicted",
    );
}

/// The LIVE regeneration loop — `subscribe` -> `StreamExt::next` -> `apply_change`,
/// exactly as `ferropress-server` spawns it — regenerates a page's cache entry
/// after a real content change. This covers the subscription wiring that the
/// per-change-handler tests above do not exercise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_regen_loop_regenerates_on_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    assert!(!blobs.exists(&key).await.unwrap(), "cache starts empty");

    // Spawn the real loop. It subscribes to the change feed and runs for the
    // task's lifetime — the same shape `ferropress-server` boots.
    let engine = Arc::new(ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    ));
    let regen = Arc::clone(&engine);
    let handle = tokio::spawn(async move {
        let _ = regen.regen_loop().await;
    });

    // Drive real changes and wait (bounded) for the loop to regenerate the page.
    // Each iteration makes a genuine field change (so the engine always publishes
    // a change) and re-touches, so that even if the first update races the loop's
    // subscription, a later one is delivered post-subscribe.
    let mut regenerated = false;
    for i in 0..200u32 {
        let mut patch: HashMap<String, Value> = HashMap::new();
        patch.insert("title".to_owned(), Value::String(format!("touch {i}")));
        store
            .update(&TypeName::from(POST_TYPE), id, patch)
            .await
            .expect("touch update");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if blobs.exists(&key).await.unwrap() {
            regenerated = true;
            break;
        }
    }
    handle.abort();

    assert!(
        regenerated,
        "the live regen loop must regenerate the page's cache entry after a change",
    );
    let cached = blobs.get(&key).await.expect("cache populated");
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&cached).expect("regenerated cache entry is a page envelope");
    assert!(
        envelope.body.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "regenerated envelope must hold the rendered body; was:\n{}",
        envelope.body,
    );
}

/// A Delete change carrying the deleted object's slug (the engine now publishes it)
/// EVICTS the cached page — the per-change-handler proof of delete-eviction.
#[tokio::test]
async fn regen_evicts_cache_on_delete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );
    let key = cache_key(&format!("/{SLUG}"));

    // Populate the cache (write-through), then confirm it's present.
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("write-through");
    assert!(blobs.exists(&key).await.unwrap(), "cache populated");

    // A delete carrying the slug evicts it.
    let del = Change {
        version: 2,
        kind: ChangeKind::Delete,
        type_name: TypeName::from(POST_TYPE),
        object_id: id,
        fields: Some(serde_json::json!({ "slug": SLUG })),
        origin: None,
    };
    engine.apply_change(&del).await.expect("evict on delete");
    assert!(
        !blobs.exists(&key).await.unwrap(),
        "the deleted page's cache entry must be evicted"
    );
}

/// The PRIMARY Create/Update path reads the slug straight off the change feed (no
/// re-`get`). Proven deterministically: the change references a NON-EXISTENT object
/// id but carries the slug on its `fields`, and a published entity lives at that
/// slug. Reading the slug off the feed renders + caches `/<slug>`; the fallback
/// re-`get` of the missing id would instead error (`NotFound`) and cache nothing —
/// so a written cache entry can ONLY mean the slug came from the feed.
#[tokio::test]
async fn regen_uses_slug_from_change_feed_without_reget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    // The published entity lives at SLUG (resolved by slug, not by object id).
    seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );
    let key = cache_key(&format!("/{SLUG}"));
    assert!(!blobs.exists(&key).await.unwrap(), "cache starts empty");

    // Object id 99_999 does not exist — a fallback re-`get` would error. The slug
    // comes off the feed instead, so the page renders and is cached.
    let change = Change {
        version: 1,
        kind: ChangeKind::Update,
        type_name: TypeName::from(POST_TYPE),
        object_id: ObjectId(99_999),
        fields: Some(serde_json::json!({ "slug": SLUG })),
        origin: None,
    };
    engine
        .apply_change(&change)
        .await
        .expect("apply must succeed via the slug-from-feed path (no re-get)");
    assert!(
        blobs.exists(&key).await.unwrap(),
        "slug read off the feed -> /{SLUG} rendered + cached without a re-get"
    );
}

/// The LIVE proof that consuming the upstream delete-fields fix works end to end:
/// a real `store.delete` makes the engine publish a Delete `ChangeEvent` carrying
/// the deleted object's slug, our adapter forwards it, and the regen loop evicts
/// the cached page — the former persistent-stale gap, now closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_regen_evicts_on_delete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;
    let key = cache_key(&format!("/{SLUG}"));

    let engine = Arc::new(ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    ));
    let regen = Arc::clone(&engine);
    let handle = tokio::spawn(async move {
        let _ = regen.regen_loop().await;
    });

    // 1. Get the page cached (touch-update loop, post-subscribe — same shape as
    //    live_regen_loop_regenerates_on_change, to dodge the subscribe race).
    let mut cached = false;
    for i in 0..200u32 {
        let mut patch: HashMap<String, Value> = HashMap::new();
        patch.insert("title".to_owned(), Value::String(format!("touch {i}")));
        store
            .update(&TypeName::from(POST_TYPE), id, patch)
            .await
            .expect("touch update");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if blobs.exists(&key).await.unwrap() {
            cached = true;
            break;
        }
    }
    assert!(cached, "page must be cached before the delete");

    // 2. Delete it; the Delete change carries the slug, so the loop evicts.
    store
        .delete(&TypeName::from(POST_TYPE), id)
        .await
        .expect("delete");
    let mut evicted = false;
    for _ in 0..200u32 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if !blobs.exists(&key).await.unwrap() {
            evicted = true;
            break;
        }
    }
    handle.abort();
    assert!(
        evicted,
        "the live regen loop must evict the deleted page's cache entry"
    );
}

// ---------------------------------------------------------------------------
// Change-feed -> ACTION hook bridge
// ---------------------------------------------------------------------------

use crate::HookBridge;
use crate::hook_bridge::{action_name, change_payload};
use ferropress_core::ContentWriter;

/// A [`HookDispatcher`] double that records every dispatched event and answers
/// [`has_hooks`] from a fixed allow-set — so a test can both prove the gate and
/// inspect exactly what the bridge emitted.
struct RecordingDispatcher {
    allow: HashSet<String>,
    seen: Mutex<Vec<(String, HookKind, serde_json::Value)>>,
}

impl RecordingDispatcher {
    fn new<const N: usize>(allow: [&str; N]) -> Self {
        Self {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn events(&self) -> Vec<(String, HookKind, serde_json::Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl HookDispatcher for RecordingDispatcher {
    fn dispatch(&self, event: HookEvent) -> ferropress_core::error::Result<HookEvent> {
        self.seen
            .lock()
            .unwrap()
            .push((event.name.clone(), event.kind, event.payload.clone()));
        Ok(event)
    }
    fn has_hooks(&self, name: &str) -> bool {
        self.allow.contains(name)
    }
}

/// A synthetic change of an arbitrary type (the `change` helper above is Post-only).
fn change_of(kind: ChangeKind, type_name: &str, id: u64, version: u64) -> Change {
    Change {
        version,
        kind,
        type_name: TypeName::from(type_name),
        object_id: ObjectId(id),
        fields: None,
        origin: None,
    }
}

#[test]
fn action_name_maps_type_and_kind() {
    assert_eq!(
        action_name(&change_of(ChangeKind::Create, POST_TYPE, 1, 1)),
        "post.created"
    );
    assert_eq!(
        action_name(&change_of(ChangeKind::Update, PAGE_TYPE, 1, 1)),
        "page.updated"
    );
    assert_eq!(
        action_name(&change_of(ChangeKind::Delete, COMMENT_TYPE, 1, 1)),
        "comment.deleted"
    );
}

#[test]
fn change_payload_carries_identity() {
    let payload = change_payload(&change_of(ChangeKind::Update, COMMENT_TYPE, 42, 7));
    assert_eq!(payload["version"], 7);
    assert_eq!(payload["type"], "Comment");
    assert_eq!(payload["kind"], "update");
    assert_eq!(payload["object_id"], 42);
    // The embedded feed carries no fields, so the key is omitted (not null).
    assert!(payload.get("fields").is_none(), "no fields key: {payload}");
}

#[test]
fn change_payload_forwards_present_fields() {
    // The change's scalar `fields` (the engine's JSON projection) reach the plugin
    // as ordinary JSON, forwarded verbatim into the action payload.
    let change = Change {
        version: 3,
        kind: ChangeKind::Update,
        type_name: TypeName::from(POST_TYPE),
        object_id: ObjectId(9),
        fields: Some(serde_json::json!({ "title": "hi", "n": 42 })),
        origin: None,
    };

    let payload = change_payload(&change);
    assert_eq!(payload["fields"]["title"], "hi", "{payload}");
    assert_eq!(payload["fields"]["n"], 42, "{payload}");
}

#[tokio::test]
async fn dispatch_change_is_gated_by_has_hooks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, _theme) = boot(tmp.path());
    // Only `post.created` is registered.
    let rec = Arc::new(RecordingDispatcher::new(["post.created"]));
    let bridge = HookBridge::new(store, rec.clone());

    // Registered action -> dispatched.
    bridge
        .dispatch_change(&change_of(ChangeKind::Create, POST_TYPE, 5, 1))
        .await;
    // Unregistered action (no `post.updated` hook) -> skipped (no payload built).
    bridge
        .dispatch_change(&change_of(ChangeKind::Update, POST_TYPE, 5, 2))
        .await;

    let events = rec.events();
    assert_eq!(events.len(), 1, "only the registered action dispatches");
    let (name, kind, payload) = &events[0];
    assert_eq!(name, "post.created");
    assert_eq!(*kind, HookKind::Action);
    assert_eq!(payload["object_id"], 5);
    assert_eq!(payload["type"], "Post");
    assert_eq!(payload["kind"], "create");
}

/// The LIVE bridge: a real `subscribe` -> action dispatch, exactly as
/// `ferropress-server` spawns it. Proves the subscription wiring + gating end to
/// end against a real embedded store change feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_bridge_dispatches_action_on_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, _theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let rec = Arc::new(RecordingDispatcher::new(["post.created", "post.updated"]));
    let bridge = Arc::new(HookBridge::new(Arc::clone(&store), rec.clone()));
    let run = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        let _ = run.run().await;
    });

    // Drive real updates (post-subscribe) until the bridge records a `post.updated`
    // action — the same touch-loop shape the live regen test uses to dodge the
    // first-event-races-subscription gap.
    let mut got = None;
    for i in 0..200u32 {
        let mut patch: HashMap<String, Value> = HashMap::new();
        patch.insert("title".to_owned(), Value::String(format!("touch {i}")));
        store
            .update(&TypeName::from(POST_TYPE), id, patch)
            .await
            .expect("touch update");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Some(ev) = rec
            .events()
            .into_iter()
            .find(|(name, _, _)| name == "post.updated")
        {
            got = Some(ev);
            break;
        }
    }
    handle.abort();

    let (name, kind, payload) = got.expect("the bridge must dispatch a post.updated action");
    assert_eq!(name, "post.updated");
    assert_eq!(kind, HookKind::Action);
    assert_eq!(
        payload["object_id"].as_u64(),
        Some(id.0),
        "action payload identifies the changed object: {payload}"
    );
    assert_eq!(payload["type"], "Post");
    assert_eq!(payload["kind"], "update");
}

/// A [`HookDispatcher`] that, on each `post.created`/`post.updated` action, does a
/// real `content:write`-style write (`set_meta`) via the injected [`ContentWriter`]
/// — exactly what the backlink-index plugin does — and counts every dispatch. Used
/// to prove the feed-loop guard.
struct WriteOnActionDispatcher {
    writer: Arc<dyn ContentWriter>,
    post_id: u64,
    dispatches: Mutex<u64>,
}

impl HookDispatcher for WriteOnActionDispatcher {
    fn dispatch(&self, event: HookEvent) -> ferropress_core::error::Result<HookEvent> {
        let seq = {
            let mut n = self.dispatches.lock().unwrap();
            *n += 1;
            *n
        };
        // A plugin-style write on the SAME post. The ContentWriter impl stamps it
        // with PLUGIN_ORIGIN, which the bridge's subscription excludes — so this
        // write must NOT come back around as another action dispatch.
        let _ = self.writer.set_meta(
            POST_TYPE,
            self.post_id,
            "loop-test",
            "touched",
            serde_json::json!(seq),
        );
        Ok(event)
    }
    fn has_hooks(&self, name: &str) -> bool {
        name == "post.created" || name == "post.updated"
    }
}

/// THE feed-loop correctness proof (what rhypedb#13 unblocked): a write from an
/// ACTION does not cause unbounded re-dispatch. A real `HookBridge` runs over a
/// real store with a write-capable action; every external post update dispatches
/// the action, which writes back via `content:write`. Because that write is stamped
/// PLUGIN_ORIGIN and the bridge subscribes with `exclude_origin = PLUGIN_ORIGIN`,
/// the action's OWN write never re-dispatches — so total dispatches can never exceed
/// the number of external (untagged) updates. A broken guard would self-amplify past
/// that bound and never quiesce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_capable_action_does_not_feed_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let concrete = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open"));
    let store: Arc<dyn RhypeStore> = concrete.clone();
    let writer: Arc<dyn ContentWriter> = concrete.clone();

    let id = seed_post(&store, SLUG, Status::Published).await;

    let disp = Arc::new(WriteOnActionDispatcher {
        writer,
        post_id: id.0,
        dispatches: Mutex::new(0),
    });
    let bridge = Arc::new(HookBridge::new(Arc::clone(&store), disp.clone()));
    let run = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        let _ = run.run().await;
    });

    // Drive EXTERNAL (untagged) updates until the action fires at least once — the
    // same touch-loop the live-bridge test uses to dodge the subscribe race. Count
    // every external update issued; each can dispatch the action at most once.
    let mut external = 0u64;
    let mut fired = false;
    for i in 0..200u32 {
        let mut patch: HashMap<String, Value> = HashMap::new();
        patch.insert("title".to_owned(), Value::String(format!("touch {i}")));
        store
            .update(&TypeName::from(POST_TYPE), id, patch)
            .await
            .expect("touch update");
        external += 1;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if *disp.dispatches.lock().unwrap() >= 1 {
            fired = true;
            break;
        }
    }
    assert!(fired, "the write-capable action must fire at least once");

    // Quiesce: stop issuing external updates and let any (erroneous) self-dispatch
    // propagate. With a broken guard the count would keep climbing here with no new
    // external write; with the guard it holds steady. Wait for it to stabilize.
    let mut last = *disp.dispatches.lock().unwrap();
    let mut stable = 0u32;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let now = *disp.dispatches.lock().unwrap();
        if now == last {
            stable += 1;
            if stable >= 4 {
                break;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    handle.abort();

    let dispatches = *disp.dispatches.lock().unwrap();
    // (a) the guard was actually exercised — an action ran and did a plugin write.
    assert!(dispatches >= 1, "the action never dispatched");
    // (b) THE PROOF: dispatches never exceed external updates. Each external
    //     (untagged) update dispatches at most once; each plugin write is filtered
    //     (PLUGIN_ORIGIN excluded) and adds ZERO dispatches. Self-amplification
    //     would push this past `external`.
    assert!(
        dispatches <= external,
        "feed loop detected: {dispatches} dispatches for only {external} external updates"
    );
    // (c) the plugin write REALLY landed (so (b) isn't vacuously true because the
    //     write silently failed and emitted no event to filter): the action's meta
    //     key is present on the post.
    let obj = RhypeStore::get(store.as_ref(), &TypeName::from(POST_TYPE), id)
        .await
        .expect("get post");
    let meta = match obj.get("meta") {
        Some(Value::Json(serde_json::Value::Object(m))) => m.clone(),
        other => panic!("meta must be a JSON object, got {other:?}"),
    };
    assert!(
        meta.get("loop-test").is_some(),
        "the action's content:write must have landed (else the guard is untested): {meta:?}"
    );
}

/// `render_preview` renders an UNPUBLISHED draft through the real theme — the
/// publish gate `serve_path` enforces is bypassed — WITHOUT touching the cache, and
/// frames it with the preview banner + a forced `noindex` (even though the default
/// settings are search-engine-visible).
#[tokio::test]
async fn render_preview_serves_a_draft_uncached_with_banner_and_noindex() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Draft).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    let settings = SiteSettings::defaults();
    assert!(
        settings.search_engine_visible,
        "sanity: defaults are indexable, so a preview-forced noindex is meaningful"
    );

    // The public read path hides a draft (the publish gate) ...
    let public = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await;
    assert!(
        matches!(public, content::Resolved::NotFound),
        "a draft must 404 on the public path"
    );

    // ... but the preview renders it through the same pipeline.
    let obj = store
        .get(&TypeName::from(POST_TYPE), id)
        .await
        .expect("get the draft object");
    let html = match content::render_preview(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        POST_TYPE,
        &obj,
    )
    .await
    {
        content::Resolved::Found(html) => html,
        other => panic!("preview must render a draft, got {other:?}"),
    };

    assert!(
        html.contains(PARAGRAPH_TEXT),
        "the draft body must be rendered: {html}"
    );
    assert!(
        html.contains("preview-bar"),
        "the preview banner must be present"
    );
    assert!(
        html.contains(r#"name="robots" content="noindex"#),
        "a preview must be noindex regardless of the (visible) site setting"
    );
    assert!(
        !html.contains(r#"id="fp-comments""#),
        "comments must be suppressed in a draft preview"
    );

    // Preview must NOT populate the prerender cache.
    assert!(
        !blobs.exists(&key).await.unwrap(),
        "rendering a preview must never write the prerender cache"
    );
}

/// With a static front page configured (show_on_front = "page" + a published page id),
/// the site root renders THAT page's body — not the latest-posts galley.
#[tokio::test]
async fn front_page_renders_configured_static_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, theme) = boot(tmp.path());

    let page_id = seed_page(&store, "about", Status::Published, "About the Press body.").await;
    // A published post too, so the galley would have content to render if we hit it.
    seed_post(&store, "a-post", Status::Published).await;
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &page_id.0.to_string()).await;

    let settings = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(settings.front_page_id, Some(page_id.0));

    match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(html) => {
            assert!(
                html.contains("About the Press body."),
                "the front page must render the static page's body; was:\n{html}"
            );
            assert!(
                !html.contains("Latest from the galley"),
                "a static front page must NOT show the posts galley"
            );
        }
        other => panic!("expected Found rendering the static front page, got {other:?}"),
    }
}

/// A configured static front page that is NOT published (draft/pending/…) must never
/// render publicly — the site root falls back to the latest-posts galley instead.
#[tokio::test]
async fn front_page_falls_back_to_galley_for_unpublished_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, theme) = boot(tmp.path());

    let page_id = seed_page(&store, "draft-home", Status::Draft, "Secret draft body.").await;
    seed_post(&store, "a-post", Status::Published).await;
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &page_id.0.to_string()).await;

    let settings = crate::settings::load_site_settings(&store).await.unwrap();
    // The id is still resolved (from the setting), but rendering must gate on publish.
    assert_eq!(settings.front_page_id, Some(page_id.0));

    match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(html) => {
            assert!(
                !html.contains("Secret draft body."),
                "an unpublished page must never render as the public front page; was:\n{html}"
            );
            assert!(
                html.contains("Latest from the galley"),
                "the front page must fall back to the galley when the target is unpublished"
            );
        }
        other => panic!("expected Found (galley fallback), got {other:?}"),
    }
}

/// `site.logo` (a Media object id) resolves to its `/media/{uuid}` URL on the typed
/// settings, and the masthead renders it as an `<img>` in place of the text title.
#[tokio::test]
async fn logo_resolves_and_renders_in_masthead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, theme) = boot(tmp.path());

    let uuid = "0192abcd-0000-7000-8000-000000000001";
    let media_id = seed_media(&store, uuid).await;
    seed_post(&store, SLUG, Status::Published).await;
    seed_setting(&store, "site.logo", &media_id.0.to_string()).await;

    let settings = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(
        settings.logo_url.as_deref(),
        Some(format!("/media/{uuid}").as_str())
    );

    // The chrome (composed live from the settings) renders the logo image. The URL's
    // slashes are HTML-entity-escaped by the theme's autoescape (`&#x2f;`, as for every
    // other chrome URL — canonical, featured image), which the browser decodes back to
    // `/`; assert on the logo variant + the (unescaped, hex-only) uuid instead.
    match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &format!("/{SLUG}"),
    )
    .await
    {
        crate::Resolved::Found(html) => {
            assert!(
                html.contains("class=\"nameplate nameplate--logo\""),
                "the masthead must use the logo nameplate variant; was:\n{html}"
            );
            assert!(
                html.contains(uuid),
                "the logo <img> must point at the media uuid; was:\n{html}"
            );
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

/// A dangling `site.logo` id (no such Media) leaves the logo unset — the masthead
/// falls back to the text title rather than emitting a broken image.
#[tokio::test]
async fn dangling_logo_id_falls_back_to_text_title() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, _theme) = boot(tmp.path());

    // An id that no Media has.
    seed_setting(&store, "site.logo", "999999").await;
    let settings = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(settings.logo_url, None);
}

// --- Cross-entity byline resolution (the live author directory) --------------

/// Seed one `User` with a unique uuid + a display name; return its id (so a test can
/// link a post's `author` to it, or rename it later). Only the byline-relevant fields
/// are populated.
async fn seed_user(store: &Arc<dyn RhypeStore>, uuid: &str, display_name: &str) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
    fields.insert(
        "display_name".to_owned(),
        Value::String(display_name.to_owned()),
    );
    store
        .create(&TypeName::from(USER_TYPE), fields)
        .await
        .expect("seeding a user must succeed")
}

/// Link a post's to-one `author` relation to a user.
async fn link_author(store: &Arc<dyn RhypeStore>, post_id: ObjectId, user_id: ObjectId) {
    let edge = Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "author".to_owned(),
    };
    store
        .link(&edge, user_id, HashMap::new())
        .await
        .expect("linking the post author must succeed");
}

/// A `User`-typed change as the loop sees it on the feed: the full scalar snapshot in
/// `fields` (incl. the current `display_name`), matching what the engine publishes.
fn user_change(kind: ChangeKind, id: ObjectId, display_name: Option<&str>) -> Change {
    Change {
        version: 1,
        kind,
        type_name: TypeName::from(USER_TYPE),
        object_id: id,
        fields: display_name.map(|n| serde_json::json!({ "display_name": n })),
        origin: None,
    }
}

/// THE cross-entity byline fix: a post's byline is resolved LIVE from the author
/// directory, so renaming the author is reflected on the ALREADY-CACHED page with no
/// regeneration. The envelope caches only the author *id*, never the name.
#[tokio::test]
async fn byline_resolves_live_from_the_author_directory_without_regen() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let user_id = seed_user(&store, "user-ada", "Ada Lovelace").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    link_author(&store, post_id, user_id).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    let settings = SiteSettings::defaults();

    // First render (a MISS): the directory reflects the original name → byline present,
    // and the write-through envelope caches the author id (not the name).
    let dir1 = crate::authors::load_author_directory(&store).await.unwrap();
    assert_eq!(dir1.name(user_id.0), Some("Ada Lovelace"));
    let html1 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir1,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html1.contains("By Ada Lovelace"),
        "the byline must show the author's name; was:\n{html1}"
    );
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(
        envelope.author_id,
        Some(user_id.0),
        "the envelope must cache the author id, not the name"
    );

    // Rename the author in the store, and rebuild the directory as the feed refresh would.
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "display_name".to_owned(),
        Value::String("Ada, Countess of Lovelace".to_owned()),
    );
    store
        .update(&TypeName::from(USER_TYPE), user_id, patch)
        .await
        .unwrap();
    let dir2 = crate::authors::load_author_directory(&store).await.unwrap();

    // Render AGAIN — a cache HIT (no regeneration): the byline reflects the NEW name
    // purely because it is composed live from the directory.
    let html2 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir2,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html2.contains("By Ada, Countess of Lovelace"),
        "the renamed byline must show live from the directory; was:\n{html2}"
    );
    assert!(
        !html2.contains("By Ada Lovelace"),
        "the stale name must be gone; was:\n{html2}"
    );

    // Prove no regeneration happened: the cached envelope is byte-for-byte unchanged.
    let envelope_after: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(
        envelope_after, envelope,
        "the byline changed with NO page regeneration — the cached envelope is unchanged"
    );
}

/// The regen loop routes a `User` change on the feed into the live author directory
/// (upsert on create/update from the change's scalar snapshot, forget on delete) — the
/// mechanism that keeps the byline fresh in production, with no page-cache eviction.
#[tokio::test]
async fn regen_loop_refreshes_author_directory_on_user_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());

    let user_id = seed_user(&store, "user-grace", "Grace").await;
    let handle = AuthorsHandle::new(crate::authors::load_author_directory(&store).await.unwrap());
    assert_eq!(handle.current().name(user_id.0), Some("Grace"));

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_authors(handle.clone());

    // A rename arrives on the feed carrying the full scalars — the directory updates,
    // with no store round-trip and no cache regeneration.
    engine
        .apply_change(&user_change(
            ChangeKind::Update,
            user_id,
            Some("Grace Hopper"),
        ))
        .await
        .expect("a User change must refresh the directory cleanly");
    assert_eq!(handle.current().name(user_id.0), Some("Grace Hopper"));

    // A delete forgets the author (unresolved id → no byline).
    engine
        .apply_change(&user_change(ChangeKind::Delete, user_id, None))
        .await
        .expect("a User delete must refresh cleanly");
    assert_eq!(handle.current().name(user_id.0), None);
}

/// The front-page galley resolves its bylines from the SAME author directory as the
/// single-page path, so the two surfaces can never show an author under different names.
#[tokio::test]
async fn home_galley_byline_resolves_from_the_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, theme) = boot(tmp.path());

    let user_id = seed_user(&store, "user-grace", "Grace Hopper").await;
    let post_id = seed_post(&store, "post-a", Status::Published).await;
    link_author(&store, post_id, user_id).await;

    let settings = SiteSettings::defaults();
    let dir = crate::authors::load_author_directory(&store).await.unwrap();

    match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(html) => {
            assert!(
                html.contains("Grace Hopper"),
                "the galley byline must resolve live from the directory; was:\n{html}"
            );
        }
        other => panic!("expected Found (galley), got {other:?}"),
    }
}

/// A pre-existing (legacy-format) cache envelope — one that stored the resolved
/// `author` NAME and no `author_id`, as the pre-fix build did — must NOT be served
/// byline-less. `deny_unknown_fields` rejects its stray `author` key, so `serve_path`
/// treats it as a miss and re-renders live: the cache self-heals to the current format
/// and the byline resolves from the directory. (Guards the deploy-over-a-cached-site
/// regression the adversarial review surfaced.)
#[tokio::test]
async fn legacy_format_envelope_is_rejected_and_self_heals() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let user_id = seed_user(&store, "user-live", "Live Name").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    link_author(&store, post_id, user_id).await;

    // Hand-write a LEGACY envelope at the cache key: the old `author` name string, no
    // `author_id`, and a sentinel body distinct from the real post's render.
    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    let legacy = serde_json::json!({
        "title": "Hello World",
        "excerpt": "",
        "published_at": null,
        "author": "Stale Baked Name",
        "featured_image": null,
        "is_post": true,
        "seo": null,
        "body": "<p>OLD CACHED BODY FROM THE LEGACY ENVELOPE</p>",
    });
    // Sanity: the legacy shape must NOT deserialize as the current envelope.
    assert!(
        serde_json::from_value::<crate::content::CachedPage>(legacy.clone()).is_err(),
        "the legacy envelope must be rejected by deny_unknown_fields"
    );
    blobs
        .put(&key, serde_json::to_vec(&legacy).unwrap())
        .await
        .expect("seed the legacy cache entry");

    // Serve: the rejected legacy entry falls through to a live re-render.
    let dir = crate::authors::load_author_directory(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (self-heal re-render), got {other:?}"),
    };
    assert!(
        html.contains("By Live Name"),
        "the byline must self-heal to the live name, not vanish; was:\n{html}"
    );
    assert!(
        !html.contains("Stale Baked Name"),
        "the legacy baked name must be gone; was:\n{html}"
    );
    assert!(
        html.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "the body must be re-rendered from the store, not the legacy blob; was:\n{html}"
    );
    assert!(
        !html.contains("OLD CACHED BODY"),
        "the legacy body must not be served; was:\n{html}"
    );

    // The cache is now a current-format envelope (author id, no name).
    let healed: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&key).await.unwrap())
            .expect("the re-rendered entry is a current-format envelope");
    assert_eq!(healed.author_id, Some(user_id.0));
}

// --- Home-page caching (track 3a) --------------------------------------------
//
// `/` is now a first-class cache entry: a `CachedFront` envelope (a static Page or the
// latest-posts galley), built + write-through on a read miss, and EVICTED by the regen loop
// when a content/settings change can reshape it (never eagerly rebuilt — the read path is the
// sole populator). These tests drive the real store + blob backends.

/// On a cache MISS for `/`, `serve_front` builds the galley, composes it live, AND populates
/// the cache with a `CachedFront::Galley` envelope carrying content-stable rows (raw
/// `published_at` + author id — never the formatted dateline or resolved name).
#[tokio::test]
async fn serve_front_galley_read_through_populates_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, "post-a", Status::Published).await;
    seed_post(&store, "post-b", Status::Published).await;

    let home_key = cache_key("/");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "home cache starts empty"
    );

    let served = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (galley), got {other:?}"),
    };
    assert!(
        served.contains("Latest from the galley"),
        "the galley rendered: {served}"
    );
    assert!(
        served.contains("Hello World"),
        "the galley lists the published posts: {served}"
    );

    let front: crate::content::CachedFront = serde_json::from_slice(
        &blobs
            .get(&home_key)
            .await
            .expect("home cache populated on a read-through miss"),
    )
    .expect("the home cache holds a CachedFront envelope, not composed HTML");
    match front {
        crate::content::CachedFront::Galley(rows) => {
            assert_eq!(
                rows.len(),
                2,
                "both published posts are in the galley envelope"
            );
            assert!(
                rows.iter().all(|r| r.title == "Hello World"),
                "rows carry content-stable titles"
            );
        }
        other => panic!("expected a Galley envelope, got {other:?}"),
    }
}

/// A cache HIT for `/` composes from the STORED `CachedFront` — no galley re-scan. The
/// sentinel row is not in the store, so its presence in the response can only come from the
/// cache; a real published post's title must be ABSENT (the store was never scanned).
#[tokio::test]
async fn serve_front_cache_hit_composes_from_stored_galley() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    // A real published post, so a MISS would render "Hello World" — making the sentinel the
    // only way the assertions pass if the cache is genuinely consulted.
    seed_post(&store, "real", Status::Published).await;

    let home_key = cache_key("/");
    const SENTINEL: &str = "SENTINEL GALLEY ROW STRAIGHT FROM THE ENVELOPE";
    let front = crate::content::CachedFront::Galley(vec![crate::content::CachedHomePost {
        title: SENTINEL.to_owned(),
        url: "/sentinel".to_owned(),
        excerpt: String::new(),
        published_at: None,
        author_id: None,
        term_ids: Vec::new(),
    }]);
    blobs
        .put(&home_key, serde_json::to_vec(&front).unwrap())
        .await
        .expect("seed the home cache entry");

    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => {
            assert!(
                h.contains(SENTINEL),
                "the served galley must come from the cached envelope: {h}"
            );
            assert!(
                !h.contains("Hello World"),
                "a cache hit must NOT re-scan the store for posts: {h}"
            );
        }
        other => panic!("expected Found (cache hit), got {other:?}"),
    }
}

/// With a static front page configured, the site root caches a `CachedFront::Static` and a
/// later request serves it FROM the cache. Proven by unpublishing the page in the store AFTER
/// caching (without driving the regen loop): a live render would fall back to the galley, but
/// the cache hit still serves the page body.
#[tokio::test]
async fn serve_front_static_read_through_then_hit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let page_id = seed_page(&store, "about", Status::Published, "About the Press body.").await;
    seed_post(&store, "a-post", Status::Published).await; // galley would have content if hit
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &page_id.0.to_string()).await;
    let settings = crate::settings::load_site_settings(&store).await.unwrap();

    let home_key = cache_key("/");

    // MISS -> a Static envelope is cached.
    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => assert!(
            h.contains("About the Press body."),
            "the static front page rendered: {h}"
        ),
        other => panic!("expected Found (static front), got {other:?}"),
    }
    let front: crate::content::CachedFront =
        serde_json::from_slice(&blobs.get(&home_key).await.unwrap()).unwrap();
    assert!(
        matches!(front, crate::content::CachedFront::Static(_)),
        "the home cache holds a Static envelope"
    );

    // Unpublish the page WITHOUT the regen loop: a live render would now fall back to the
    // galley, but the cache HIT still serves the page body -> proves `/` is served cached.
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "status".to_owned(),
        Value::String(Status::Draft.as_str().to_owned()),
    );
    store
        .update(&TypeName::from(PAGE_TYPE), page_id, patch)
        .await
        .unwrap();

    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => {
            assert!(
                h.contains("About the Press body."),
                "the cached static front is served on a HIT: {h}"
            );
            assert!(
                !h.contains("Latest from the galley"),
                "a cache hit must not fall back to a live galley: {h}"
            );
        }
        other => panic!("expected Found (cache hit), got {other:?}"),
    }
}

/// A POST change EVICTS the home cache (the post may be in the galley), so the next request
/// rebuilds it — the regen loop invalidates `/` rather than eagerly rebuilding it.
#[tokio::test]
async fn post_change_evicts_home_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let settings = SiteSettings::defaults();
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings.clone()));

    let home_key = cache_key("/");
    // Warm the home cache via a read.
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    assert!(
        blobs.exists(&home_key).await.unwrap(),
        "home cache warmed by the read"
    );

    // A post change evicts it.
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("apply post change");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "a POST change must evict the home cache"
    );

    // The next read rebuilds it.
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    assert!(
        blobs.exists(&home_key).await.unwrap(),
        "the next request rebuilds the home cache"
    );
}

/// A PAGE change evicts the home cache ONLY when that page is the configured static front
/// page; an edit to any other page leaves `/` cached (pages never appear in the galley).
#[tokio::test]
async fn page_change_evicts_home_only_for_the_configured_front_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let front_id = seed_page(&store, "home-page", Status::Published, "Front body.").await;
    let other_id = seed_page(&store, "other", Status::Published, "Other body.").await;
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &front_id.0.to_string()).await;
    let settings = crate::settings::load_site_settings(&store).await.unwrap();

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings.clone()));

    let home_key = cache_key("/");
    // Warm the cache (a Static envelope).
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    assert!(blobs.exists(&home_key).await.unwrap(), "home cache warmed");

    // A change to a NON-front page does NOT evict `/`.
    engine
        .apply_change(&change_of(ChangeKind::Update, PAGE_TYPE, other_id.0, 1))
        .await
        .expect("apply non-front page change");
    assert!(
        blobs.exists(&home_key).await.unwrap(),
        "a non-front page edit must not evict the home cache"
    );

    // A change to the CONFIGURED front page DOES evict `/`.
    engine
        .apply_change(&change_of(ChangeKind::Update, PAGE_TYPE, front_id.0, 2))
        .await
        .expect("apply front page change");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "editing the configured front page must evict the home cache"
    );
}

/// A chrome-only Setting change (`site.title`) refreshes the live snapshot but does NOT bust
/// the home cache — chrome composes live. A front-shaping Setting change
/// (`reading.posts_per_page`) evicts `/` so the next request rebuilds it. This is the
/// settings dimension the design's "settings compose live, no page regen" invariant hinges on.
#[tokio::test]
async fn setting_change_evicts_home_only_for_front_shaping_keys() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, SLUG, Status::Published).await;

    let settings = SiteSettings::defaults();
    let handle = SettingsHandle::new(settings.clone());
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(handle.clone());

    let home_key = cache_key("/");
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    assert!(blobs.exists(&home_key).await.unwrap(), "home cache warmed");

    // Chrome key -> snapshot refreshes, cache untouched.
    seed_setting(&store, "site.title", "\"Live Title\"").await;
    engine
        .apply_change(&setting_change_with_key(ChangeKind::Update, "site.title"))
        .await
        .expect("chrome setting refresh");
    assert_eq!(
        handle.current().title,
        "Live Title",
        "the snapshot refreshed"
    );
    assert!(
        blobs.exists(&home_key).await.unwrap(),
        "a chrome setting must NOT bust the home cache"
    );

    // Front-shaping key -> evict.
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "reading.posts_per_page",
        ))
        .await
        .expect("front-shaping setting");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "a reading.* front-shaping setting must evict the home cache"
    );
}

/// THE home-page byline guarantee: the galley byline resolves LIVE from the author directory,
/// so renaming the author reflects on the ALREADY-CACHED `/` with NO regeneration — the
/// cached envelope stores only the author id. The single-page proof does not cover `/`.
#[tokio::test]
async fn home_galley_byline_stays_live_on_cached_front_without_regen() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let user_id = seed_user(&store, "user-ada", "Ada Lovelace").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    link_author(&store, post_id, user_id).await;

    let home_key = cache_key("/");
    let settings = SiteSettings::defaults();

    // First render (a MISS): byline present, envelope caches the id (not the name).
    let dir1 = crate::authors::load_author_directory(&store).await.unwrap();
    let html1 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir1,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html1.contains("Ada Lovelace"),
        "the galley byline is present: {html1}"
    );
    let envelope_bytes = blobs.get(&home_key).await.unwrap();
    let front: crate::content::CachedFront = serde_json::from_slice(&envelope_bytes).unwrap();
    match &front {
        crate::content::CachedFront::Galley(rows) => assert_eq!(
            rows[0].author_id,
            Some(user_id.0),
            "the envelope caches the author id, not the name"
        ),
        other => panic!("expected Galley, got {other:?}"),
    }

    // Rename the author; rebuild the directory as the feed refresh would.
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "display_name".to_owned(),
        Value::String("Ada, Countess of Lovelace".to_owned()),
    );
    store
        .update(&TypeName::from(USER_TYPE), user_id, patch)
        .await
        .unwrap();
    let dir2 = crate::authors::load_author_directory(&store).await.unwrap();

    // Render AGAIN — a cache HIT: the byline reflects the NEW name, purely from the directory.
    let html2 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir2,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html2.contains("Ada, Countess of Lovelace"),
        "the renamed byline shows live from the directory: {html2}"
    );
    assert!(
        !html2.contains("Ada Lovelace"),
        "the stale name is gone: {html2}"
    );

    // Prove NO regeneration: the cached envelope is byte-for-byte unchanged.
    assert_eq!(
        blobs.get(&home_key).await.unwrap(),
        envelope_bytes,
        "the byline changed with NO `/` regeneration — the cached envelope is unchanged"
    );
}

/// A corrupt / non-`CachedFront` entry at the home key is rejected (treated as a miss) and
/// `serve_front` re-renders live + write-throughs the current format — the home cache
/// self-heals on first access, mirroring the single-page envelope self-heal.
#[tokio::test]
async fn corrupt_home_cache_entry_is_rejected_and_self_heals() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, SLUG, Status::Published).await;

    let home_key = cache_key("/");
    // A blob that is not a CachedFront envelope (e.g. legacy raw HTML or garbage).
    blobs
        .put(&home_key, b"<html>not an envelope</html>".to_vec())
        .await
        .expect("seed a corrupt home entry");

    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (self-heal), got {other:?}"),
    };
    assert!(
        html.contains("Latest from the galley"),
        "the galley re-rendered live: {html}"
    );
    // The cache now holds a valid CachedFront (self-healed to the current format).
    let _: crate::content::CachedFront =
        serde_json::from_slice(&blobs.get(&home_key).await.unwrap())
            .expect("the home cache self-healed to a CachedFront envelope");
}

/// Deleting a post evicts the home cache even when the Delete change carries NO slug — the
/// home invalidation runs BEFORE the slug-dependent permalink handling, so a slug-less delete
/// cannot strand `/` showing a post that no longer exists.
#[tokio::test]
async fn delete_evicts_home_even_without_a_slug_on_the_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let settings = SiteSettings::defaults();
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings.clone()));

    let home_key = cache_key("/");
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    assert!(blobs.exists(&home_key).await.unwrap(), "home cache warmed");

    // A Delete with NO fields (no slug): the home eviction must still fire.
    let del = Change {
        version: 2,
        kind: ChangeKind::Delete,
        type_name: TypeName::from(POST_TYPE),
        object_id: id,
        fields: None,
        origin: None,
    };
    engine
        .apply_change(&del)
        .await
        .expect("apply a slug-less delete");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "a slug-less post delete must still evict the home cache"
    );
}

/// The front page (empty slug) and a permalink whose slug is literally `index` (or a nested
/// slug) must NOT share a cache key. Before the two-namespace scheme both mapped to
/// `prerender/index.html`, silently defeating each other's cache — this pins the split.
#[test]
fn front_page_and_index_slug_do_not_share_a_cache_key() {
    let home = cache_key("/");
    let index = cache_key("/index");
    assert_ne!(
        home, index,
        "the home page and a slug-'index' permalink must use distinct keys"
    );
    assert!(
        home.0.contains("/listing/"),
        "the front page lives under the reserved listing namespace: {}",
        home.0
    );
    assert!(
        index.0.contains("/permalink/"),
        "a permalink lives under the permalink namespace: {}",
        index.0
    );
    // A nested slug cannot escape into the listing subtree either.
    assert_ne!(cache_key("/listing/index"), home);
    // The root's trailing-slash variants all resolve to the one listing key.
    assert_eq!(cache_key("/"), cache_key("///"));
}

/// The `setting_reshapes_front` classifier that gates home eviction: all three front-shaping
/// `reading.*` keys evict; other settings (incl. non-front `reading.*` keys) do not; a change
/// carrying no key evicts conservatively. Guards the exact regression the reviewer named — a
/// dropped key from the match would silently stop evicting `/` on a static-vs-galley flip.
#[test]
fn setting_reshapes_front_classifies_every_key() {
    for key in [
        "reading.posts_per_page",
        "reading.show_on_front",
        "reading.page_on_front",
    ] {
        assert!(
            crate::setting_reshapes_front(&setting_change_with_key(ChangeKind::Update, key)),
            "{key} must reshape the front page and evict /"
        );
    }
    for key in [
        "site.title",
        "site.timezone",
        "reading.feed_items",
        "reading.search_engine_visible",
    ] {
        assert!(
            !crate::setting_reshapes_front(&setting_change_with_key(ChangeKind::Update, key)),
            "{key} composes live and must NOT bust the home cache"
        );
    }
    // A Setting change whose fields carry no `key` evicts conservatively.
    let no_key = Change {
        version: 1,
        kind: ChangeKind::Update,
        type_name: TypeName::from(SETTING_TYPE),
        object_id: ObjectId(0),
        fields: None,
        origin: None,
    };
    assert!(
        crate::setting_reshapes_front(&no_key),
        "a keyless setting change must evict conservatively"
    );
}

/// After a `reading.posts_per_page` change evicts `/`, the next request REBUILDS the galley
/// honoring the NEW cap — proving the evict→rebuild loop reflects the change, not just that
/// the entry was deleted.
#[tokio::test]
async fn home_rebuild_honors_new_posts_per_page_after_eviction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    for slug in ["p-a", "p-b", "p-c"] {
        seed_post(&store, slug, Status::Published).await;
    }

    let settings0 = SiteSettings::defaults(); // posts_per_page = 10
    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings0.clone()));

    let home_key = cache_key("/");
    // Warm at the default cap: all three posts.
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings0,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    let front0: crate::content::CachedFront =
        serde_json::from_slice(&blobs.get(&home_key).await.unwrap()).unwrap();
    match front0 {
        crate::content::CachedFront::Galley(rows) => assert_eq!(rows.len(), 3),
        other => panic!("expected Galley, got {other:?}"),
    }

    // Lower the cap in the store, then apply the setting change (evicts `/`).
    seed_setting(&store, "reading.posts_per_page", "2").await;
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "reading.posts_per_page",
        ))
        .await
        .expect("apply posts_per_page change");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "the reading.posts_per_page change must evict /"
    );

    // Re-read with the reloaded settings: the rebuilt galley honors the new cap of 2.
    let settings1 = crate::settings::load_site_settings(&store).await.unwrap();
    assert_eq!(settings1.posts_per_page, 2);
    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings1,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await;
    let front1: crate::content::CachedFront =
        serde_json::from_slice(&blobs.get(&home_key).await.unwrap()).unwrap();
    match front1 {
        crate::content::CachedFront::Galley(rows) => assert_eq!(
            rows.len(),
            2,
            "the rebuilt galley must honor the new posts_per_page cap"
        ),
        other => panic!("expected Galley, got {other:?}"),
    }
}

/// After editing the configured static front page and applying its change (which evicts `/`),
/// the next request rebuilds `/` with the NEW page body — the static analogue of the galley
/// rebuild test.
#[tokio::test]
async fn home_rebuild_reflects_edited_static_front_after_eviction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let page_id = seed_page(&store, "home-page", Status::Published, "Original body.").await;
    seed_setting(&store, "reading.show_on_front", "\"page\"").await;
    seed_setting(&store, "reading.page_on_front", &page_id.0.to_string()).await;
    let settings = crate::settings::load_site_settings(&store).await.unwrap();

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(settings.clone()));

    let home_key = cache_key("/");
    // Warm the Static front.
    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => assert!(h.contains("Original body."), "{h}"),
        other => panic!("expected Found, got {other:?}"),
    }

    // Edit the page body in the store.
    let updated = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000PAGE".to_owned(),
        kind: BlockKind::Paragraph {
            runs: vec![InlineRun {
                text: "Updated body.".to_owned(),
                marks: Vec::new(),
                href: None,
            }],
        },
        children: Vec::new(),
    }]);
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "block_tree".to_owned(),
        Value::Json(updated.to_json_value().unwrap()),
    );
    store
        .update(&TypeName::from(PAGE_TYPE), page_id, patch)
        .await
        .unwrap();

    // The page change evicts `/` (object id == configured front page).
    engine
        .apply_change(&change_of(ChangeKind::Update, PAGE_TYPE, page_id.0, 2))
        .await
        .expect("apply front-page edit");
    assert!(
        !blobs.exists(&home_key).await.unwrap(),
        "editing the configured static front page must evict /"
    );

    // Re-read: the rebuilt static front shows the new body.
    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => {
            assert!(h.contains("Updated body."), "the new body is served: {h}");
            assert!(!h.contains("Original body."), "the old body is gone: {h}");
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

/// Structurally-drifted (valid-JSON-but-wrong-shape) home envelopes are rejected and
/// self-heal — not just non-JSON garbage. Covers the invariants the design leans on: an
/// unknown variant tag, a galley row with a stray field (`deny_unknown_fields`), and a
/// `Static` wrapping a legacy `CachedPage` that carries a baked `author` name.
#[tokio::test]
async fn structurally_drifted_home_envelope_is_rejected_and_self_heals() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, SLUG, Status::Published).await;
    let home_key = cache_key("/");

    let drifted = [
        // Unknown variant tag.
        serde_json::json!({ "Grid": [] }),
        // Galley row with a stray field (rejected by deny_unknown_fields on CachedHomePost).
        serde_json::json!({ "Galley": [{
            "title": "x", "url": "/x", "excerpt": "", "published_at": null,
            "author_id": null, "extra": 1
        }]}),
        // Static wrapping a legacy CachedPage (baked `author` name, no `author_id`).
        serde_json::json!({ "Static": {
            "title": "x", "excerpt": "", "published_at": null, "author": "Legacy Baked Name",
            "featured_image": null, "is_post": false, "seo": null, "body": "OLD CACHED BODY"
        }}),
    ];

    for bad in drifted {
        // Sanity: the drifted shape must NOT deserialize as the current CachedFront.
        assert!(
            serde_json::from_value::<crate::content::CachedFront>(bad.clone()).is_err(),
            "drifted shape must be rejected: {bad}"
        );
        blobs
            .put(&home_key, serde_json::to_vec(&bad).unwrap())
            .await
            .expect("seed a drifted home entry");

        // Serve: the drifted entry is treated as a miss → re-rendered live.
        match serve_path(
            &store,
            &blobs,
            &theme,
            &NoCustomBlocks,
            &SiteSettings::defaults(),
            &AuthorDirectory::default(),
            &crate::MenuSet::default(),
            &crate::ContentIndex::default(),
            &crate::TaxonomySet::default(),
            "/",
        )
        .await
        {
            crate::Resolved::Found(h) => {
                assert!(
                    h.contains("Latest from the galley"),
                    "self-healed live: {h}"
                );
                assert!(
                    !h.contains("OLD CACHED BODY"),
                    "the drifted body is not served: {h}"
                );
            }
            other => panic!("expected Found (self-heal), got {other:?}"),
        }
        // The cache now holds a valid current-format CachedFront.
        let _: crate::content::CachedFront =
            serde_json::from_slice(&blobs.get(&home_key).await.unwrap())
                .expect("the home cache self-healed to a valid CachedFront");
    }
}

/// The refactor's central invariant: the cached (`serve_front`) and uncached
/// (`resolve_path`/`front_page`) front pages are byte-for-byte identical, and the cache-hit
/// (envelope round-trip) render equals the cache-miss (in-memory) render. Uses a post with a
/// non-empty excerpt + a real published_at + a byline so every `CachedHomePost` field is
/// exercised — a dropped field on the round-trip would diverge the HIT from the MISS.
#[tokio::test]
async fn cached_and_uncached_front_page_are_byte_for_byte_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());

    let user_id = seed_user(&store, "user-ada", "Ada Lovelace").await;
    // A post exercising all CachedHomePost fields.
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String("rich".to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String("Rich Post".to_owned()));
    fields.insert(
        "excerpt".to_owned(),
        Value::String("A meaningful excerpt.".to_owned()),
    );
    fields.insert(
        "published_at".to_owned(),
        Value::DateTime(1_700_000_000_000),
    );
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    let post_id = store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .unwrap();
    link_author(&store, post_id, user_id).await;

    let settings = SiteSettings::defaults();
    let dir = crate::authors::load_author_directory(&store).await.unwrap();

    let uncached = match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (uncached), got {other:?}"),
    };
    let miss = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (cache miss), got {other:?}"),
    };
    let hit = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (cache hit), got {other:?}"),
    };

    // Sanity: the exercised fields actually appear (so parity is not vacuously over blanks).
    assert!(
        hit.contains("A meaningful excerpt."),
        "excerpt rendered: {hit}"
    );
    assert!(hit.contains("Ada Lovelace"), "byline rendered: {hit}");

    assert_eq!(
        uncached, miss,
        "the uncached (resolve_path) and cache-miss (serve_front build) front pages must match"
    );
    assert_eq!(
        miss, hit,
        "the cache-hit (envelope round-trip) render must equal the cache-miss render"
    );
}

// --- Page hierarchy: materialized-path backfill + the self-inverse `children` edge ---

/// Link `child`'s `parent` to-one edge to `parent` (page hierarchy).
async fn link_parent(store: &Arc<dyn RhypeStore>, child: ObjectId, parent: ObjectId) {
    let edge = Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id: child,
        field: "parent".to_owned(),
    };
    store
        .link(&edge, parent, HashMap::new())
        .await
        .expect("linking a page parent must succeed");
}

/// Read a page's stored `path` scalar (empty when unset).
async fn page_path(store: &Arc<dyn RhypeStore>, id: ObjectId) -> String {
    match store
        .get(&TypeName::from(PAGE_TYPE), id)
        .await
        .expect("get page")
        .get("path")
    {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn backfill_computes_flat_and_nested_paths() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());

    // A three-deep chain about -> team -> history, all created BEFORE any `path`
    // (seed_page writes none), i.e. the legacy/pre-field state the backfill repairs.
    let about = seed_page(&store, "about", Status::Published, "About.").await;
    let team = seed_page(&store, "team", Status::Published, "Team.").await;
    let history = seed_page(&store, "history", Status::Published, "History.").await;
    link_parent(&store, team, about).await;
    link_parent(&store, history, team).await;

    assert_eq!(
        page_path(&store, history).await,
        "",
        "no path before backfill"
    );

    let report = crate::backfill_page_paths(&store).await.expect("backfill");
    assert_eq!(report.scanned, 3);
    assert_eq!(report.updated, 3);
    assert!(report.collisions.is_empty());
    assert_eq!(report.cyclic, 0);

    assert_eq!(page_path(&store, about).await, "about");
    assert_eq!(page_path(&store, team).await, "about/team");
    assert_eq!(page_path(&store, history).await, "about/team/history");
}

#[tokio::test]
async fn backfill_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());
    let about = seed_page(&store, "about", Status::Published, "About.").await;
    let team = seed_page(&store, "team", Status::Published, "Team.").await;
    link_parent(&store, team, about).await;

    let first = crate::backfill_page_paths(&store).await.expect("backfill");
    assert_eq!(first.updated, 2);
    let second = crate::backfill_page_paths(&store).await.expect("backfill");
    assert_eq!(second.updated, 0, "a steady-state re-run writes nothing");
    assert_eq!(second.scanned, 2);
}

#[tokio::test]
async fn backfill_reports_a_cycle_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());
    // Two pages each the other's parent — a corrupt cycle. The bounded walk must report
    // them and never spin.
    let a = seed_page(&store, "a", Status::Published, "A.").await;
    let b = seed_page(&store, "b", Status::Published, "B.").await;
    link_parent(&store, a, b).await;
    link_parent(&store, b, a).await;

    let report = crate::backfill_page_paths(&store).await.expect("backfill");
    assert_eq!(report.scanned, 2);
    assert_eq!(report.cyclic, 2, "both pages are on the cycle");
    assert_eq!(report.updated, 0, "no path is written for a cyclic page");
}

#[tokio::test]
async fn backfill_detects_duplicate_paths() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());
    // Two top-level pages sharing a slug (legacy: Page.slug was @indexed-not-unique) both
    // compute path "dup" — a collision the report surfaces for reconciliation.
    seed_page(&store, "dup", Status::Published, "One.").await;
    seed_page(&store, "dup", Status::Published, "Two.").await;

    let report = crate::backfill_page_paths(&store).await.expect("backfill");
    assert_eq!(report.collisions, vec!["dup".to_owned()]);
}

#[tokio::test]
async fn children_inverse_edge_traverses() {
    // Prove the self-referential `children: [Page] @inverse(Page.parent)` edge returns a
    // parent's children — the Phase-4 cascade relies on this reverse traversal, and there
    // is no other self-inverse in the schema to lean on for precedent.
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());
    let parent = seed_page(&store, "parent", Status::Published, "P.").await;
    let child_a = seed_page(&store, "child-a", Status::Published, "A.").await;
    let child_b = seed_page(&store, "child-b", Status::Published, "B.").await;
    link_parent(&store, child_a, parent).await;
    link_parent(&store, child_b, parent).await;

    let edge = Edge {
        type_name: TypeName::from(PAGE_TYPE),
        id: parent,
        field: "children".to_owned(),
    };
    let mut kids: Vec<u64> = store
        .get_links(&edge)
        .await
        .expect("children inverse traverses")
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    kids.sort();
    let mut want = vec![child_a.0, child_b.0];
    want.sort();
    assert_eq!(kids, want, "the children inverse returns both children");
}

// --- Nested-permalink resolution + regen path-keying (Phase 2) ---

/// A synthetic Page change carrying its full nested `path` in `fields`, matching what the
/// embedded adapter publishes for a page write — so the regen loop keys the cache off the
/// nested path directly (no re-`get`).
fn page_change(kind: ChangeKind, id: ObjectId, path: &str) -> Change {
    Change {
        version: 1,
        kind,
        type_name: TypeName::from(PAGE_TYPE),
        object_id: id,
        fields: Some(serde_json::json!({ "path": path })),
        origin: None,
    }
}

#[tokio::test]
async fn nested_page_resolves_at_its_full_path_only() {
    let dir = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(dir.path());
    let _about = seed_page(&store, "about", Status::Published, "About.").await;
    let team = seed_page(&store, "team", Status::Published, "The team.").await;
    link_parent(&store, team, _about).await;
    crate::backfill_page_paths(&store).await.expect("backfill");

    let serve = |path: &'static str| {
        let (store, blobs, theme) = (store.clone(), blobs.clone(), theme.clone());
        async move {
            serve_path(
                &store,
                &blobs,
                &theme,
                &NoCustomBlocks,
                &SiteSettings::defaults(),
                &AuthorDirectory::default(),
                &crate::MenuSet::default(),
                &crate::ContentIndex::default(),
                &crate::TaxonomySet::default(),
                path,
            )
            .await
        }
    };

    // The nested page is served at /about/team AND cached at the NESTED blob key.
    assert!(
        matches!(serve("/about/team").await, crate::Resolved::Found(_)),
        "nested page resolves at its full path",
    );
    assert!(
        blobs.exists(&cache_key("/about/team")).await.unwrap(),
        "nested page is cached at the nested permalink key",
    );

    // It is NOT reachable at its bare leaf slug — posts own the flat namespace; a nested
    // page has no flat /team URL.
    assert!(
        matches!(serve("/team").await, crate::Resolved::NotFound),
        "a nested page is not served at its leaf slug",
    );

    // The parent still resolves at its own top-level path.
    assert!(
        matches!(serve("/about").await, crate::Resolved::Found(_)),
        "the parent resolves at /about",
    );
}

#[tokio::test]
async fn single_segment_post_wins_over_a_top_level_page() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _blobs, _theme) = boot(dir.path());
    // A published Post AND a published top-level Page both claim the flat key "x".
    seed_post(&store, "x", Status::Published).await;
    seed_page(&store, "x", Status::Published, "Page x.").await;
    crate::backfill_page_paths(&store).await.expect("backfill");

    let resolved = content::resolve_published_entity(&store, "x")
        .await
        .expect("resolve");
    assert_eq!(
        resolved.map(|(ty, _)| ty),
        Some(POST_TYPE),
        "a single-segment path resolves the Post first (established precedence)",
    );
}

#[tokio::test]
async fn regen_keys_a_page_on_its_nested_path() {
    let dir = tempfile::tempdir().unwrap();
    let (store, blobs, _theme) = boot(dir.path());
    let _about = seed_page(&store, "about", Status::Published, "About.").await;
    let team = seed_page(&store, "team", Status::Published, "The team.").await;
    link_parent(&store, team, _about).await;
    crate::backfill_page_paths(&store).await.expect("backfill");

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );
    let nested = cache_key("/about/team");
    let leaf = cache_key("/team");

    // Update: the regen step writes the envelope through to the NESTED key, never the slug.
    engine
        .apply_change(&page_change(ChangeKind::Update, team, "about/team"))
        .await
        .expect("regen a published nested page");
    assert!(
        blobs.exists(&nested).await.unwrap(),
        "regen writes the nested key",
    );
    assert!(
        !blobs.exists(&leaf).await.unwrap(),
        "regen must NOT key a page on its bare slug",
    );

    // Delete: the regen step evicts the NESTED key (the old slug-keyed path would miss it).
    engine
        .apply_change(&page_change(ChangeKind::Delete, team, "about/team"))
        .await
        .expect("evict a deleted nested page");
    assert!(
        !blobs.exists(&nested).await.unwrap(),
        "delete evicts the nested key",
    );
}

// --- Redirect table live reload (Phase 3) ---

/// Seed one `Redirect` row (from_path/to_path/status_code), matching the admin handler shape.
async fn seed_redirect(store: &Arc<dyn RhypeStore>, from: &str, to: &str, status: u32) {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("from_path".to_owned(), Value::String(from.to_owned()));
    fields.insert("to_path".to_owned(), Value::String(to.to_owned()));
    fields.insert("status_code".to_owned(), Value::U32(status));
    store
        .create(&TypeName::from(REDIRECT_TYPE), fields)
        .await
        .expect("seeding a redirect must succeed");
}

#[tokio::test]
async fn regen_reloads_the_redirect_table_on_a_redirect_change() {
    let dir = tempfile::tempdir().unwrap();
    let (store, blobs, _theme) = boot(dir.path());
    let handle = crate::RedirectHandle::default();
    assert!(
        handle.lookup("/about").is_none(),
        "empty before any redirect"
    );

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_redirects(handle.clone());

    // A rename records a Redirect; the change feed reloads the whole table off the store.
    seed_redirect(&store, "/about", "/company", 301).await;
    engine
        .apply_change(&Change {
            version: 1,
            kind: ChangeKind::Create,
            type_name: TypeName::from(REDIRECT_TYPE),
            object_id: ObjectId(1),
            fields: None,
            origin: None,
        })
        .await
        .expect("reload the redirect table");

    let hit = handle
        .lookup("/about")
        .expect("redirect now present after the feed reload");
    assert_eq!(hit.to, "/company");
    assert_eq!(hit.status, 301);
    // A trailing slash still matches (normalized keying).
    assert_eq!(
        handle.lookup("/about/").map(|t| t.to),
        Some("/company".to_owned())
    );
}

// --- Page theme templates (Phase 5) ---

/// Set a page's `template` scalar (as the admin save does).
async fn set_template(store: &Arc<dyn RhypeStore>, id: ObjectId, template: &str) {
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert("template".to_owned(), Value::String(template.to_owned()));
    store
        .update(&TypeName::from(PAGE_TYPE), id, patch)
        .await
        .expect("setting a page template must succeed");
}

#[tokio::test]
async fn a_page_template_renders_a_different_layout() {
    let dir = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(dir.path());
    let _plain = seed_page(&store, "about", Status::Published, "About.").await;
    let wide = seed_page(&store, "showcase", Status::Published, "Showcase.").await;
    set_template(&store, wide, "page-wide").await;
    crate::backfill_page_paths(&store).await.expect("backfill");

    let render = |path: &'static str| {
        let (store, blobs, theme) = (store.clone(), blobs.clone(), theme.clone());
        async move {
            match serve_path(
                &store,
                &blobs,
                &theme,
                &NoCustomBlocks,
                &SiteSettings::defaults(),
                &AuthorDirectory::default(),
                &crate::MenuSet::default(),
                &crate::ContentIndex::default(),
                &crate::TaxonomySet::default(),
                path,
            )
            .await
            {
                crate::Resolved::Found(html) => html,
                other => panic!("expected Found, got {other:?}"),
            }
        }
    };

    let default_html = render("/about").await;
    let wide_html = render("/showcase").await;
    // Match the class USAGE, not the substring — `.article--wide` also appears as a CSS
    // selector in every page's inlined stylesheet.
    assert!(
        !default_html.contains("class=\"article--wide\""),
        "the default page uses the single template",
    );
    assert!(
        wide_html.contains("class=\"article--wide\""),
        "the page-wide page renders through the full-width template",
    );
}

#[test]
fn legacy_envelope_without_template_deserializes_to_none() {
    // An envelope written before `template` existed carries no `template` key.
    // `deny_unknown_fields` rejects only EXTRA keys, and serde defaults a missing `Option` to
    // `None` — so a legacy envelope loads cleanly with `template: None` and renders the
    // default, with NO re-render herd.
    let json = serde_json::json!({
        "title": "Legacy",
        "excerpt": "",
        "published_at": null,
        "author_id": null,
        "featured_image": null,
        "is_post": false,
        "seo": null,
        "body": "<p>x</p>"
    });
    let page: crate::content::CachedPage =
        serde_json::from_value(json).expect("a pre-template envelope must still deserialize");
    assert_eq!(page.template, None);
}

#[tokio::test]
async fn front_page_that_is_a_page_honors_its_template() {
    let dir = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(dir.path());
    let front = seed_page(&store, "landing", Status::Published, "Welcome.").await;
    set_template(&store, front, "page-wide").await;
    crate::backfill_page_paths(&store).await.expect("backfill");

    // Configure this page as the static front page.
    let mut settings = SiteSettings::defaults();
    settings.front_page_id = Some(front.0);

    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/",
    )
    .await
    {
        crate::Resolved::Found(html) => html,
        other => panic!("expected Found at the front page, got {other:?}"),
    };
    assert!(
        html.contains("class=\"article--wide\""),
        "a page set as the front page renders with its chosen template (is_home)",
    );
    // And it is framed as the home page (the nav marks the front-page link current).
    assert!(
        html.contains("aria-current=\"page\""),
        "the front page still marks the home nav current",
    );
}

// --- Syndication feeds (RSS + Atom cached listing) ---------------------------

/// Seed a PUBLISHED post with the fields the feed reads: uuid, title, a paragraph body, and a
/// `published_at`/`updated_at` instant (millis) so ordering + dates are deterministic.
async fn seed_feed_post(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    title: &str,
    uuid: &str,
    published_at_millis: i64,
) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String(title.to_owned()));
    fields.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
    fields.insert(
        "excerpt".to_owned(),
        Value::String("A short summary.".to_owned()),
    );
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    fields.insert(
        "published_at".to_owned(),
        Value::DateTime(published_at_millis),
    );
    fields.insert(
        "updated_at".to_owned(),
        Value::DateTime(published_at_millis),
    );
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a feed post must succeed")
}

#[tokio::test]
async fn build_feed_lists_published_newest_first_capped_at_feed_items() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, _blobs, _) = boot(tmp.path());

    seed_feed_post(&store, "oldest", "Oldest", "uuid-a", 1_000).await;
    seed_feed_post(&store, "middle", "Middle", "uuid-b", 2_000).await;
    seed_feed_post(&store, "newest", "Newest", "uuid-c", 3_000).await;
    // A draft must never appear in the feed.
    seed_post(&store, "a-draft", Status::Draft).await;

    let mut settings = SiteSettings::defaults();
    settings.feed_items = 2;

    let feed = crate::feed::build_feed(&store, &NoCustomBlocks, &settings)
        .await
        .expect("build_feed");

    assert_eq!(feed.items.len(), 2, "capped at feed_items");
    assert_eq!(feed.items[0].slug, "newest", "newest first");
    assert_eq!(feed.items[1].slug, "middle");
    assert_eq!(feed.items[0].uuid, "uuid-c");
    assert!(
        feed.items[0]
            .content
            .contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "the item carries the rendered body: {}",
        feed.items[0].content,
    );
}

#[tokio::test]
async fn serve_feed_read_through_populates_cache_and_composes_both_formats() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    seed_feed_post(&store, "welcome", "Welcome", "uuid-1", 1_700_000_000_000).await;

    let settings = SiteSettings::defaults();
    let authors = AuthorDirectory::default();
    let key = crate::feed::feed_cache_key();

    assert!(
        !blobs.exists(&key).await.unwrap(),
        "feed cache empty before first serve"
    );

    // Miss -> build -> write-through. The RSS output names the post + is absolute (request origin).
    let rss = crate::serve_feed(
        &store,
        &blobs,
        &NoCustomBlocks,
        &settings,
        &authors,
        crate::FeedFormat::Rss,
        Some("https://press.example"),
    )
    .await
    .expect("serve rss");
    assert!(
        rss.contains("<title>Welcome</title>"),
        "rss names the post: {rss}"
    );
    assert!(
        rss.contains("https://press.example/welcome"),
        "absolute item link"
    );

    // The cache now holds the ENVELOPE (a CachedFeed), not the composed XML.
    let bytes = blobs.get(&key).await.expect("feed cache populated");
    let envelope: crate::feed::CachedFeed =
        serde_json::from_slice(&bytes).expect("cache holds a CachedFeed envelope, not raw XML");
    assert_eq!(envelope.items.len(), 1);

    // Atom reads the SAME envelope (a hit) and composes the post too.
    let atom = crate::serve_feed(
        &store,
        &blobs,
        &NoCustomBlocks,
        &settings,
        &authors,
        crate::FeedFormat::Atom,
        Some("https://press.example"),
    )
    .await
    .expect("serve atom");
    assert!(
        atom.contains("<title>Welcome</title>"),
        "atom names the post"
    );
    assert!(
        atom.contains("urn:uuid:uuid-1"),
        "atom entry id from the post uuid"
    );
}

/// Warm the feed cache via a read, returning the engine + the feed key for an eviction assertion.
async fn warm_feed(
    store: &Arc<dyn RhypeStore>,
    blobs: &Arc<dyn BlobStore>,
) -> (ServeEngine, ferropress_core::ports::BlobKey) {
    let key = crate::feed::feed_cache_key();
    crate::serve_feed(
        store,
        blobs,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        crate::FeedFormat::Rss,
        Some("https://press.example"),
    )
    .await
    .expect("warm the feed cache");
    assert!(blobs.exists(&key).await.unwrap(), "feed cache warm");
    let engine = ServeEngine::new(
        Arc::clone(store),
        Arc::clone(blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_settings(SettingsHandle::new(SiteSettings::defaults()));
    (engine, key)
}

#[tokio::test]
async fn a_post_change_evicts_the_feed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let post_id = seed_feed_post(&store, "welcome", "Welcome", "uuid-1", 1_000).await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    engine
        .apply_change(&change(ChangeKind::Update, post_id))
        .await
        .expect("post change applies");

    assert!(
        !blobs.exists(&key).await.unwrap(),
        "a post change must EVICT the feed",
    );
}

#[tokio::test]
async fn a_page_change_does_not_evict_the_feed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    seed_feed_post(&store, "welcome", "Welcome", "uuid-1", 1_000).await;
    let page_id = seed_page(&store, "about", Status::Published, "About text").await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    engine
        .apply_change(&page_change(ChangeKind::Update, page_id, "about"))
        .await
        .expect("page change applies");

    assert!(
        blobs.exists(&key).await.unwrap(),
        "a PAGE change must NOT touch the feed (feeds list posts only)",
    );
}

#[tokio::test]
async fn a_user_rename_does_not_evict_the_feed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    let user_id = seed_user(&store, "user-ada", "Ada").await;
    seed_feed_post(&store, "welcome", "Welcome", "uuid-1", 1_000).await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    engine
        .apply_change(&user_change(
            ChangeKind::Update,
            user_id,
            Some("Ada Lovelace"),
        ))
        .await
        .expect("user change applies");

    assert!(
        blobs.exists(&key).await.unwrap(),
        "an author rename must NOT evict the feed (byline resolved live)",
    );
}

#[tokio::test]
async fn feed_items_setting_change_evicts_the_feed_but_other_settings_do_not() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    seed_feed_post(&store, "welcome", "Welcome", "uuid-1", 1_000).await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    // A CHROME setting (composed live in the feed) must NOT bust it.
    engine
        .apply_change(&setting_change_with_key(ChangeKind::Update, "site.title"))
        .await
        .expect("site.title change applies");
    assert!(
        blobs.exists(&key).await.unwrap(),
        "site.title composes live in the feed → no eviction",
    );

    // `reading.feed_items` reshapes the cached row count → evict.
    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "reading.feed_items",
        ))
        .await
        .expect("reading.feed_items change applies");
    assert!(
        !blobs.exists(&key).await.unwrap(),
        "reading.feed_items must EVICT the feed",
    );
}

#[tokio::test]
async fn plugin_setting_change_evicts_the_feed_when_a_published_post_bakes_the_plugin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    seed_post_with_block_tree(
        &store,
        "callout-post",
        Status::Published,
        custom_block_tree_json("callout"),
    )
    .await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        !blobs.exists(&key).await.unwrap(),
        "a plugin-config change must EVICT the feed when a published post bakes that plugin's block",
    );
}

#[tokio::test]
async fn plugin_setting_change_does_not_evict_the_feed_for_a_draft_only_match() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, _) = boot(tmp.path());
    // Only a DRAFT bakes the plugin — it is NOT in the feed, so the feed must survive.
    seed_post_with_block_tree(
        &store,
        "draft-callout",
        Status::Draft,
        custom_block_tree_json("callout"),
    )
    .await;
    let (engine, key) = warm_feed(&store, &blobs).await;

    engine
        .apply_change(&setting_change_with_key(
            ChangeKind::Update,
            "plugin.callout.default_variant",
        ))
        .await
        .expect("plugin-setting change applies");

    assert!(
        blobs.exists(&key).await.unwrap(),
        "a plugin used only by a DRAFT post must NOT evict the feed (published-only membership)",
    );
}

// ---------------------------------------------------------------------------
// Taxonomies: the live TaxonomyHandle reload + the blunt archive-subtree evict
// ---------------------------------------------------------------------------

/// Seed a Taxonomy row + one Term linked to it (the migrate tool's / admin's
/// shape, reduced to the fields the serve loader reads). Returns the term's id.
async fn seed_term(
    store: &Arc<dyn RhypeStore>,
    taxonomy_key: &str,
    slug: &str,
    name: &str,
) -> ObjectId {
    let mut t: HashMap<String, Value> = HashMap::new();
    t.insert("key".to_owned(), Value::String(taxonomy_key.to_owned()));
    t.insert("label".to_owned(), Value::String(taxonomy_key.to_owned()));
    t.insert("hierarchical".to_owned(), Value::Bool(true));
    t.insert("multiple".to_owned(), Value::Bool(true));
    t.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let tax_id = store
        .create(&TypeName::from(ferropress_core::TAXONOMY_TYPE), t)
        .await
        .expect("seed taxonomy");

    let mut f: HashMap<String, Value> = HashMap::new();
    f.insert("slug".to_owned(), Value::String(slug.to_owned()));
    f.insert("name".to_owned(), Value::String(name.to_owned()));
    f.insert("description".to_owned(), Value::String(String::new()));
    f.insert("plaintext".to_owned(), Value::String(name.to_owned()));
    f.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let term_id = store
        .create(&TypeName::from(ferropress_core::TERM_TYPE), f)
        .await
        .expect("seed term");
    store
        .link(
            &Edge {
                type_name: TypeName::from(ferropress_core::TERM_TYPE),
                id: term_id,
                field: "taxonomy".to_owned(),
            },
            tax_id,
            HashMap::new(),
        )
        .await
        .expect("link term to taxonomy");
    term_id
}

#[tokio::test]
async fn a_term_change_reloads_the_handle_and_blunt_evicts_only_the_archive_subtree() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, _theme) = boot(tmp.path());
    let term_id = seed_term(&store, "category", "news", "News").await;

    // A stale handle (empty) + a warmed cache: two fake archive entries under the
    // term subtree, plus a permalink and the front page that must SURVIVE.
    let handle = crate::TaxonomyHandle::default();
    assert!(handle.current().is_empty());
    let archive1 =
        ferropress_core::ports::BlobKey("prerender/listing/term/category/news.html".to_owned());
    let archive2 = ferropress_core::ports::BlobKey(
        "prerender/listing/term/category/news/page/2.html".to_owned(),
    );
    let permalink = cache_key("/hello-world");
    let front = cache_key("/");
    for key in [&archive1, &archive2, &permalink, &front] {
        blobs.put(key, b"cached".to_vec()).await.unwrap();
    }

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    )
    .with_taxonomies(handle.clone());

    engine
        .apply_change(&Change {
            version: 1,
            kind: ChangeKind::Update,
            type_name: TypeName::from(ferropress_core::TERM_TYPE),
            object_id: term_id,
            fields: None,
            origin: None,
        })
        .await
        .expect("term change applies");

    // The handle was full-reloaded from the store…
    let set = handle.current();
    assert_eq!(
        set.archive_href(term_id.0).as_deref(),
        Some("/category/news"),
        "the reloaded set resolves the seeded term",
    );
    assert_eq!(set.resolve_archive_path("category/news"), Some(term_id.0));
    // …the whole archive subtree is gone…
    assert!(!blobs.exists(&archive1).await.unwrap());
    assert!(!blobs.exists(&archive2).await.unwrap());
    // …and nothing else was touched.
    assert!(
        blobs.exists(&permalink).await.unwrap(),
        "permalinks survive"
    );
    assert!(
        blobs.exists(&front).await.unwrap(),
        "the front page survives"
    );
}

#[tokio::test]
async fn a_post_change_blunt_evicts_the_archive_subtree_too() {
    // A post's archive membership lives in eventless Post.terms links absent from
    // the change snapshot, so ANY post change must evict every archive (the
    // pinned blunt strategy) — while permalink handling proceeds as before.
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, _theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let archive =
        ferropress_core::ports::BlobKey("prerender/listing/term/category/news.html".to_owned());
    blobs.put(&archive, b"cached".to_vec()).await.unwrap();

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::new(NoCustomBlocks),
    );
    engine
        .apply_change(&change(ChangeKind::Update, id))
        .await
        .expect("post change applies");

    assert!(
        !blobs.exists(&archive).await.unwrap(),
        "a post change must blunt-evict the term-archive subtree",
    );
    assert!(
        blobs.exists(&cache_key(&format!("/{SLUG}"))).await.unwrap(),
        "the post's own permalink regen still ran (write-through)",
    );
}

// ---------------------------------------------------------------------------
// Term chips: baked ids, resolved LIVE from the TaxonomySet at compose time
// ---------------------------------------------------------------------------

/// Link a post's to-many `terms` relation to a term — the M:N membership
/// [`content::cached_page_from_object`] bakes into `CachedPage::term_ids` /
/// [`content::recent_published_posts`] batches into `CachedHomePost::term_ids`.
async fn link_term(store: &Arc<dyn RhypeStore>, post_id: ObjectId, term_id: ObjectId) {
    let edge = Edge {
        type_name: TypeName::from(POST_TYPE),
        id: post_id,
        field: "terms".to_owned(),
    };
    store
        .link(&edge, term_id, HashMap::new())
        .await
        .expect("linking the post term must succeed");
}

/// Seed a Term as a CHILD of `parent_id`, in the SAME taxonomy as the parent — the
/// ancestor-chain case [`seed_term`] (roots only) can't produce. Returns the child's id.
async fn seed_child_term(
    store: &Arc<dyn RhypeStore>,
    parent_id: ObjectId,
    slug: &str,
    name: &str,
) -> ObjectId {
    let mut f: HashMap<String, Value> = HashMap::new();
    f.insert("slug".to_owned(), Value::String(slug.to_owned()));
    f.insert("name".to_owned(), Value::String(name.to_owned()));
    f.insert("description".to_owned(), Value::String(String::new()));
    f.insert("plaintext".to_owned(), Value::String(name.to_owned()));
    f.insert("meta".to_owned(), Value::Json(serde_json::json!({})));
    let term_id = store
        .create(&TypeName::from(ferropress_core::TERM_TYPE), f)
        .await
        .expect("seed child term");

    let tax_links = store
        .get_links(&Edge {
            type_name: TypeName::from(ferropress_core::TERM_TYPE),
            id: parent_id,
            field: "taxonomy".to_owned(),
        })
        .await
        .expect("read the parent term's taxonomy link");
    let tax_id = tax_links
        .first()
        .expect("the parent term must have a taxonomy")
        .0;
    store
        .link(
            &Edge {
                type_name: TypeName::from(ferropress_core::TERM_TYPE),
                id: term_id,
                field: "taxonomy".to_owned(),
            },
            tax_id,
            HashMap::new(),
        )
        .await
        .expect("link child term to the parent's taxonomy");
    store
        .link(
            &Edge {
                type_name: TypeName::from(ferropress_core::TERM_TYPE),
                id: term_id,
                field: "parent".to_owned(),
            },
            parent_id,
            HashMap::new(),
        )
        .await
        .expect("link child term to its parent");
    term_id
}

/// A post's chips render the term NAME and its canonical archive href, resolved live from
/// the [`TaxonomySet`](crate::TaxonomySet) — the envelope bakes only ids, never the resolved
/// name/href (mirrors the byline id/name split). Covers a term in each of two DIFFERENT
/// taxonomies, a CHILD term whose href must be the full ancestor chain (not just its own
/// slug), and the name-sorted, case-insensitive DISPLAY ORDER (`Debut` / `Fiction` / `Space
/// Opera`) — deterministic regardless of link-creation order.
#[tokio::test]
async fn term_chips_render_with_name_and_href() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let fiction_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let space_opera_id = seed_child_term(&store, fiction_id, "space-opera", "Space Opera").await;
    let debut_id = seed_term(&store, "tag", "debut", "Debut").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    // Link in a DELIBERATELY non-alphabetical order — the render order must come from the
    // name sort, not the link order.
    link_term(&store, post_id, space_opera_id).await;
    link_term(&store, post_id, fiction_id).await;
    link_term(&store, post_id, debut_id).await;

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let settings = SiteSettings::defaults();
    let path = format!("/{SLUG}");

    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };

    assert!(
        html.contains("class=\"chips\""),
        "the chip list must render; was:\n{html}"
    );
    // Hrefs are HTML-entity-escaped by the theme's autoescape (`/` -> `&#x2f;`, the same
    // discipline every other chrome URL — canonical, featured image, logo — already carries),
    // which the browser decodes back to `/`.
    let debut_chip = "class=\"chip\" href=\"&#x2f;tag&#x2f;debut\">Debut</a>";
    let fiction_chip = "class=\"chip\" href=\"&#x2f;category&#x2f;fiction\">Fiction</a>";
    let space_opera_chip =
        "class=\"chip\" href=\"&#x2f;category&#x2f;fiction&#x2f;space-opera\">Space Opera</a>";
    assert!(
        html.contains(debut_chip),
        "the Debut chip (a different taxonomy) must show its name + archive href; was:\n{html}"
    );
    assert!(
        html.contains(fiction_chip),
        "the Fiction chip must show its name + archive href; was:\n{html}"
    );
    assert!(
        html.contains(space_opera_chip),
        "the Space Opera chip's href must be the FULL ancestor chain, not just its own slug; was:\n{html}"
    );
    let (debut_pos, fiction_pos, space_opera_pos) = (
        html.find(debut_chip).unwrap(),
        html.find(fiction_chip).unwrap(),
        html.find(space_opera_chip).unwrap(),
    );
    assert!(
        debut_pos < fiction_pos && fiction_pos < space_opera_pos,
        "chips must render name-sorted (Debut, Fiction, Space Opera), not link order; was:\n{html}"
    );

    // The envelope bakes only the ids — Term.slug/name/parent are NOT stored redundantly.
    // Link order is unspecified (Term has no ordinal column), so compare as a set.
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&cache_key(&path)).await.unwrap()).unwrap();
    let baked: HashSet<u64> = envelope.term_ids.iter().copied().collect();
    assert_eq!(
        baked,
        HashSet::from([fiction_id.0, space_opera_id.0, debut_id.0]),
        "the envelope must bake exactly the linked term ids"
    );
}

/// THE term-chip fix: a chip is resolved LIVE from the taxonomy set, so renaming a term
/// updates its chip on the ALREADY-CACHED page with NO regeneration — the envelope caches
/// only the term id, never the name (mirrors the byline live-resolve pair).
#[tokio::test]
async fn term_chips_resolve_live_and_survive_a_rename_without_regen() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let term_id = seed_term(&store, "category", "news", "News").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    link_term(&store, post_id, term_id).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    let settings = SiteSettings::defaults();

    // First render (a MISS): the taxonomy set reflects the original name.
    let taxonomies1 = crate::load_taxonomies(&store).await.unwrap();
    let html1 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies1,
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html1.contains(">News</a>"),
        "the original term name must render; was:\n{html1}"
    );
    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();

    // Rename the term in the store, then rebuild the taxonomy set exactly as the regen
    // loop's `Term`-change handler does (`load_taxonomies` full-reload) — WITHOUT touching
    // the page cache at all.
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert(
        "name".to_owned(),
        Value::String("Current Events".to_owned()),
    );
    store
        .update(&TypeName::from(ferropress_core::TERM_TYPE), term_id, patch)
        .await
        .unwrap();
    let taxonomies2 = crate::load_taxonomies(&store).await.unwrap();

    // Render AGAIN — a cache HIT (no regeneration): the chip reflects the new name purely
    // because it is composed live from the taxonomy set.
    let html2 = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies2,
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html2.contains(">Current Events</a>"),
        "the renamed term must render live; was:\n{html2}"
    );
    assert!(
        !html2.contains(">News</a>"),
        "the stale name must be gone; was:\n{html2}"
    );

    // Prove no regeneration happened: the cached envelope is byte-for-byte unchanged.
    let envelope_after: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(
        envelope_after, envelope,
        "the chip changed with NO page regeneration — the cached envelope is unchanged"
    );
}

/// A term id the current [`TaxonomySet`](crate::TaxonomySet) can't resolve (a deleted term
/// whose cache eviction hasn't landed yet, or corrupt data) is silently dropped from the
/// chip list — a listing degrades, it never breaks the page.
#[tokio::test]
async fn an_unresolvable_term_id_is_dropped_from_chips() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);

    // Pre-put an envelope whose term_ids references a term nobody seeded.
    let envelope = crate::content::CachedPage {
        title: "Title".to_owned(),
        excerpt: String::new(),
        published_at: None,
        author_id: None,
        featured_image: None,
        is_post: true,
        term_ids: vec![999_999],
        template: None,
        seo: None,
        body: "<p>Body</p>".to_owned(),
    };
    blobs
        .put(&key, serde_json::to_vec(&envelope).unwrap())
        .await
        .unwrap();

    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        &path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        !html.contains("class=\"chips\""),
        "an unresolvable term id must be dropped, not rendered as a broken chip; was:\n{html}"
    );
}

/// The front-page galley's chips are the twin of the single-page chips: baked per-row term
/// ids, resolved live in `compose_front` — exercising [`content::build_galley`]'s batched
/// `terms` link read alongside its batched `author` read.
#[tokio::test]
async fn galley_rows_render_term_chips_from_baked_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _blobs, theme) = boot(tmp.path());

    let term_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let post_id = seed_post(&store, SLUG, Status::Published).await;
    link_term(&store, post_id, term_id).await;

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        html.contains("class=\"chip\" href=\"&#x2f;category&#x2f;fiction\">Fiction</a>"),
        "the galley row's chip must show its name + archive href; was:\n{html}"
    );
}

/// An envelope written before `term_ids` existed carries no `term_ids` key.
/// `deny_unknown_fields` rejects only EXTRA keys, and `#[serde(default)]` defaults a missing
/// `Vec` to empty — so a legacy envelope loads cleanly with `term_ids: vec![]` and renders no
/// chips, with NO re-render herd (mirrors `legacy_envelope_without_template_deserializes_to_none`).
#[test]
fn legacy_envelope_without_term_ids_deserializes_to_empty() {
    let json = serde_json::json!({
        "title": "Legacy",
        "excerpt": "",
        "published_at": null,
        "author_id": null,
        "featured_image": null,
        "is_post": true,
        "template": null,
        "seo": null,
        "body": "<p>x</p>"
    });
    let page: crate::content::CachedPage =
        serde_json::from_value(json).expect("a pre-term_ids envelope must still deserialize");
    assert_eq!(page.term_ids, Vec::<u64>::new());
}

/// A `Page` has no `terms` relation at all — only `Post` carries taxonomy membership (WP
/// parity) — so its served HTML must never contain chip markup, and its envelope must bake
/// an empty `term_ids` (the `is_post` gate mirroring `author_id`'s).
#[tokio::test]
async fn a_page_renders_no_chip_markup() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    seed_page(&store, "about", Status::Published, "About the Press body.").await;
    crate::backfill_page_paths(&store)
        .await
        .expect("backfill page paths");

    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &crate::TaxonomySet::default(),
        "/about",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(
        !html.contains("class=\"chips\""),
        "a Page must never render chip markup; was:\n{html}"
    );

    let envelope: crate::content::CachedPage =
        serde_json::from_slice(&blobs.get(&cache_key("/about")).await.unwrap()).unwrap();
    assert!(
        envelope.term_ids.is_empty(),
        "a Page's envelope must bake no term ids; was: {:?}",
        envelope.term_ids
    );
}

// ---------------------------------------------------------------------------
// Term archives: claim-only-on-resolve routing + the rollup
// ---------------------------------------------------------------------------

/// Seed one PUBLISHED (or otherwise) post with a distinct title — [`seed_post`]'s title is a
/// fixed "Hello World", which can't distinguish rows in a multi-post archive/rollup assertion.
async fn seed_titled_post(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    title: &str,
    status: Status,
) -> ObjectId {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    fields.insert("title".to_owned(), Value::String(title.to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "block_tree".to_owned(),
        Value::Json(paragraph_block_tree_json()),
    );
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seeding a titled post must succeed")
}

/// A root archive rolls up its OWN direct posts `∪` every descendant's, DEDUPED: a post
/// directly assigned to both the parent and the child appears exactly once.
#[tokio::test]
async fn root_archive_lists_own_and_descendant_posts_deduped() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let fiction_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let space_opera_id = seed_child_term(&store, fiction_id, "space-opera", "Space Opera").await;

    let post_a = seed_titled_post(&store, "post-a", "Post A", Status::Published).await; // fiction only
    let post_b = seed_titled_post(&store, "post-b", "Post B", Status::Published).await; // space-opera only
    let post_c = seed_titled_post(&store, "post-c", "Post C", Status::Published).await; // BOTH — dedup target
    link_term(&store, post_a, fiction_id).await;
    link_term(&store, post_b, space_opera_id).await;
    link_term(&store, post_c, fiction_id).await;
    link_term(&store, post_c, space_opera_id).await;

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };

    for title in ["Post A", "Post B", "Post C"] {
        assert_eq!(
            html.matches(title).count(),
            1,
            "{title} must appear exactly once (deduped); was:\n{html}"
        );
    }

    let key = crate::archive_cache_key("category/fiction");
    let envelope: crate::content::CachedTermArchive =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(
        envelope.total, 3,
        "the rollup must count all 3 posts, deduped"
    );
}

/// A CHILD archive lists only ITS OWN direct posts — the parent's posts do not leak down.
#[tokio::test]
async fn child_archive_lists_only_its_own_posts() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let fiction_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let space_opera_id = seed_child_term(&store, fiction_id, "space-opera", "Space Opera").await;

    let post_a = seed_titled_post(&store, "post-a", "Post A", Status::Published).await;
    let post_b = seed_titled_post(&store, "post-b", "Post B", Status::Published).await;
    link_term(&store, post_a, fiction_id).await; // fiction ONLY
    link_term(&store, post_b, space_opera_id).await; // space-opera ONLY

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction/space-opera",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };

    assert!(
        html.contains("Post B"),
        "the child's own post must appear; was:\n{html}"
    );
    assert!(
        !html.contains("Post A"),
        "the PARENT's post must NOT roll DOWN to the child archive; was:\n{html}"
    );
}

/// A draft post linked to a term is excluded from its archive — published-only, same gate
/// [`recent_published_posts`] applies to the home galley/feed.
#[tokio::test]
async fn a_draft_post_is_excluded_from_the_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let fiction_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let published = seed_titled_post(&store, "pub-post", "Published Post", Status::Published).await;
    let draft = seed_titled_post(&store, "draft-post", "Draft Post", Status::Draft).await;
    link_term(&store, published, fiction_id).await;
    link_term(&store, draft, fiction_id).await;

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(html.contains("Published Post"), "was:\n{html}");
    assert!(!html.contains("Draft Post"), "was:\n{html}");

    let key = crate::archive_cache_key("category/fiction");
    let envelope: crate::content::CachedTermArchive =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(envelope.total, 1, "the draft must not count toward total");
}

/// Claim-only-on-resolve: a path shaped like an archive chain that does NOT actually resolve
/// (no such term under that taxonomy key) falls through untouched — a real Page at that path
/// still serves.
#[tokio::test]
async fn an_unresolvable_archive_chain_falls_through_to_the_page() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    // The "category" taxonomy key is live (has a "fiction" term), but "other-team" is not a
    // term under it — so `/category/other-team` cannot resolve as an archive.
    seed_term(&store, "category", "fiction", "Fiction").await;

    let category_page = seed_page(&store, "category", Status::Published, "Category landing.").await;
    let child_page = seed_page(&store, "other-team", Status::Published, "The other team.").await;
    link_parent(&store, child_page, category_page).await;
    crate::backfill_page_paths(&store)
        .await
        .expect("backfill page paths");

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/other-team",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (the page), got {other:?}"),
    };
    assert!(
        html.contains("The other team."),
        "the page must serve when the chain doesn't resolve as an archive; was:\n{html}"
    );
}

/// When a path DOES resolve as a live archive, it wins even if a Page happens to occupy the
/// identical materialized path (the admin write path's own redirect-guard tests cover
/// PREVENTING this collision; this proves the serve-time PRECEDENCE directly).
#[tokio::test]
async fn the_archive_wins_over_a_page_at_the_same_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    seed_term(&store, "category", "fiction", "Fiction").await;

    let category_page = seed_page(&store, "category", Status::Published, "Category landing.").await;
    let colliding_page = seed_page(
        &store,
        "fiction",
        Status::Published,
        "SHOULD NEVER RENDER — shadowed by the archive.",
    )
    .await;
    link_parent(&store, colliding_page, category_page).await;
    crate::backfill_page_paths(&store)
        .await
        .expect("backfill page paths");

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (the archive), got {other:?}"),
    };
    assert!(
        !html.contains("SHOULD NEVER RENDER"),
        "the archive must win over a page at the same path; was:\n{html}"
    );
    assert!(
        html.contains("Fiction"),
        "the archive heading must render; was:\n{html}"
    );
}

/// An archive with zero rolled-up posts is a VALID page (the empty-galley shape), not a 404.
#[tokio::test]
async fn an_empty_archive_is_a_valid_page_not_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    seed_term(&store, "category", "fiction", "Fiction").await; // zero posts linked

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let html = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction",
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("an empty archive must still be Found, not {other:?}"),
    };
    assert!(
        html.contains("No proofs filed under Fiction yet."),
        "the empty-archive copy must render; was:\n{html}"
    );
}

/// The uncached form ([`content::resolve_path`]), a cache MISS build, and a cache HIT must all
/// produce byte-for-byte identical HTML for an archive — mirrors
/// `cached_and_uncached_front_page_are_byte_for_byte_identical`.
#[tokio::test]
async fn cached_and_uncached_archive_are_byte_for_byte_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());

    let fiction_id = seed_term(&store, "category", "fiction", "Fiction").await;
    let user_id = seed_user(&store, "user-ada", "Ada Lovelace").await;
    let post = seed_titled_post(&store, "rich", "Rich Post", Status::Published).await;
    link_term(&store, post, fiction_id).await;
    link_author(&store, post, user_id).await;

    let taxonomies = crate::load_taxonomies(&store).await.unwrap();
    let settings = SiteSettings::defaults();
    let dir = crate::authors::load_author_directory(&store).await.unwrap();
    let path = "/category/fiction";

    let uncached = match content::resolve_path(
        &store,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (uncached), got {other:?}"),
    };
    let miss = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (cache miss), got {other:?}"),
    };
    let hit = match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &settings,
        &dir,
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        path,
    )
    .await
    {
        crate::Resolved::Found(h) => h,
        other => panic!("expected Found (cache hit), got {other:?}"),
    };

    // Sanity: the exercised fields actually appear (parity is not vacuously over blanks).
    assert!(hit.contains("Rich Post"), "post rendered: {hit}");
    assert!(hit.contains("Ada Lovelace"), "byline rendered: {hit}");
    assert!(hit.contains("Fiction"), "archive heading rendered: {hit}");

    assert_eq!(
        uncached, miss,
        "resolve_path and serve_path's cache-miss build must match"
    );
    assert_eq!(
        miss, hit,
        "the cache-hit render must equal the cache-miss render"
    );
}

/// A miss write-throughs under the TERM-ARCHIVE cache key ([`archive_cache_key`]) — never the
/// ordinary permalink key, so the two namespaces can never collide.
#[tokio::test]
async fn archive_write_through_uses_the_term_archive_cache_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    seed_term(&store, "category", "fiction", "Fiction").await;
    let taxonomies = crate::load_taxonomies(&store).await.unwrap();

    let key = crate::archive_cache_key("category/fiction");
    assert!(
        !blobs.exists(&key).await.unwrap(),
        "cache must be empty before the first serve"
    );

    let _ = serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category/fiction",
    )
    .await;

    assert!(
        blobs.exists(&key).await.unwrap(),
        "a miss must write-through under the term-archive cache key"
    );
    assert!(
        !blobs.exists(&cache_key("/category/fiction")).await.unwrap(),
        "an archive must NOT also populate the ordinary permalink cache key"
    );
    let envelope: crate::content::CachedTermArchive =
        serde_json::from_slice(&blobs.get(&key).await.unwrap()).unwrap();
    assert_eq!(envelope.page, 1);
}

/// A bare taxonomy key with no chain (`/category`, no term slug) is nobody's canonical
/// archive path — it falls through to the ordinary 404 flow (nothing else claims it either).
#[tokio::test]
async fn a_bare_taxonomy_key_falls_through_to_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, blobs, theme) = boot(tmp.path());
    seed_term(&store, "category", "fiction", "Fiction").await;
    let taxonomies = crate::load_taxonomies(&store).await.unwrap();

    match serve_path(
        &store,
        &blobs,
        &theme,
        &NoCustomBlocks,
        &SiteSettings::defaults(),
        &AuthorDirectory::default(),
        &crate::MenuSet::default(),
        &crate::ContentIndex::default(),
        &taxonomies,
        "/category",
    )
    .await
    {
        crate::Resolved::NotFound => {}
        other => panic!("a bare taxonomy key must fall through to 404, got {other:?}"),
    }
}
