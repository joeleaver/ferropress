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
    Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, PAGE_TYPE, POST_TYPE, SETTING_TYPE,
    Status, USER_TYPE,
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
async fn seed_setting(store: &Arc<dyn RhypeStore>, key: &str, json_encoded_value: &str) {
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
        .expect("seeding a setting must succeed");
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

    match content::resolve_path(&store, &theme, &NoCustomBlocks, &settings, &dir, "/").await {
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

    let uncached =
        match content::resolve_path(&store, &theme, &NoCustomBlocks, &settings, &dir, "/").await {
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
