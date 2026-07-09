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
use ferropress_core::query::{Change, ChangeKind};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{
    Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, PAGE_TYPE, POST_TYPE, SETTING_TYPE,
    Status,
};
use ferropress_render_form::SiteSettings;

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;
use ferropress_theme::ThemeEngine;

use ferropress_render::NoCustomBlocks;

use crate::{OutputPage, ServeEngine, SettingsHandle, cache_key, content, serve_path};

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

/// A `Setting`-typed change, as the loop sees it on the feed.
fn settings_change(kind: ChangeKind) -> Change {
    Change {
        version: 1,
        kind,
        type_name: TypeName::from(SETTING_TYPE),
        object_id: ObjectId(0),
        fields: None,
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

/// A `Setting` change on the feed refreshes the live snapshot the read path
/// holds — and does NOT touch the page cache (settings are composed live).
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

    // A settings edit lands in the store, then the change arrives on the feed.
    seed_setting(&store, "site.title", "\"Live Title\"").await;
    engine
        .apply_change(&settings_change(ChangeKind::Update))
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
        author: None,
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
    let public = serve_path(&store, &blobs, &theme, &NoCustomBlocks, &settings, &path).await;
    assert!(
        matches!(public, content::Resolved::NotFound),
        "a draft must 404 on the public path"
    );

    // ... but the preview renders it through the same pipeline.
    let obj = store
        .get(&TypeName::from(POST_TYPE), id)
        .await
        .expect("get the draft object");
    let html =
        match content::render_preview(&store, &theme, &NoCustomBlocks, &settings, POST_TYPE, &obj)
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

    match content::resolve_path(&store, &theme, &NoCustomBlocks, &settings, "/").await {
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

    match content::resolve_path(&store, &theme, &NoCustomBlocks, &settings, "/").await {
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
