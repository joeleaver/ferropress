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
    Block, BlockKind, BlockTree, COMMENT_TYPE, InlineRun, PAGE_TYPE, POST_TYPE, Status,
};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;
use ferropress_theme::ThemeEngine;

use ferropress_render::NoCustomBlocks;

use crate::{OutputPage, ServeEngine, cache_key, content, serve_path};

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
    }
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
    let served = match serve_path(&store, &blobs, &theme, &NoCustomBlocks, &path).await {
        crate::Resolved::Found(html) => html,
        other => panic!("expected Found on a published post, got {other:?}"),
    };
    assert!(
        served.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "served body must contain the rendered paragraph; was:\n{served}",
    );

    // The cache is now populated AND equals exactly what was served.
    let cached = blobs
        .get(&key)
        .await
        .expect("cache must be populated after a read-through miss");
    assert_eq!(
        String::from_utf8(cached).unwrap(),
        served,
        "cached bytes must equal the served HTML",
    );
}

/// A pre-populated cache entry is served VERBATIM — proving the hot path reads
/// from the cache and does NOT re-render. The sentinel is deliberately NOT the
/// real render, so its presence in the response can only come from the cache.
#[tokio::test]
async fn serve_path_cache_hit_serves_stored_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    // Publish a post too, so a cache MISS would have rendered the real body —
    // making the sentinel the only way the assertion passes if we hit the cache.
    seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    const SENTINEL: &str = "<!-- SENTINEL: served straight from the prerender cache -->";

    // Pre-put the sentinel at the page's cache key.
    blobs
        .put(&key, SENTINEL.as_bytes().to_vec())
        .await
        .expect("seeding the cache entry");

    // serve_path must return the sentinel, not a fresh render.
    match serve_path(&store, &blobs, &theme, &NoCustomBlocks, &path).await {
        crate::Resolved::Found(html) => {
            assert_eq!(html, SENTINEL, "must serve the cached bytes verbatim");
            assert!(
                !html.contains(PARAGRAPH_TEXT),
                "a cache hit must NOT re-render the post body",
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
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::clone(&theme),
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
        .expect("regen must write the rendered HTML through to the cache");
    let cached = String::from_utf8(cached).unwrap();
    assert!(
        cached.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "regenerated cache entry must hold the rendered body; was:\n{cached}",
    );
    // It matches a direct render of the same page (regen == on-demand render).
    let rendered = engine
        .render_page(&OutputPage { path: path.clone() })
        .await
        .expect("render_page ok")
        .expect("published page renders to Some");
    assert_eq!(
        cached, rendered,
        "regen output must equal an on-demand render"
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
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let path = format!("/{SLUG}");
    let key = cache_key(&path);
    assert!(!blobs.exists(&key).await.unwrap(), "cache starts empty");

    // Spawn the real loop. It subscribes to the change feed and runs for the
    // task's lifetime — the same shape `ferropress-server` boots.
    let engine = Arc::new(ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        theme,
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
    let cached = String::from_utf8(blobs.get(&key).await.expect("cache populated")).unwrap();
    assert!(
        cached.contains(&format!("<p>{PARAGRAPH_TEXT}</p>")),
        "regenerated cache entry must hold the rendered body; was:\n{cached}",
    );
}

/// A Delete change carrying the deleted object's slug (the engine now publishes it)
/// EVICTS the cached page — the per-change-handler proof of delete-eviction.
#[tokio::test]
async fn regen_evicts_cache_on_delete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        Arc::clone(&theme),
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
    let (store, blobs, theme) = boot(tmp.path());
    // The published entity lives at SLUG (resolved by slug, not by object id).
    seed_post(&store, SLUG, Status::Published).await;

    let engine = ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        theme,
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
    let (store, blobs, theme) = boot(tmp.path());
    let id = seed_post(&store, SLUG, Status::Published).await;
    let key = cache_key(&format!("/{SLUG}"));

    let engine = Arc::new(ServeEngine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        theme,
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
