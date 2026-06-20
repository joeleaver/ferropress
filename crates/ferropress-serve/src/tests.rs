//! Tests for the static-first prerender cache + the regen write-through.
//!
//! These drive the REAL collaborators — a `tempfile`-isolated [`EmbeddedStore`]
//! and a [`LocalFsBlobStore`] over the same crate's `default_theme()` — so the
//! cache read-through, the cache hit, and the regen write-through/eviction are
//! proven against actual store + blob backends, not mocks.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ferropress_core::ports::BlobStore;
use ferropress_core::query::{Change, ChangeKind};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{ObjectId, TypeName, Value};
use ferropress_core::{Block, BlockKind, BlockTree, InlineRun, POST_TYPE, Status};

use ferropress_blob_localfs::LocalFsBlobStore;
use ferropress_store_embedded::EmbeddedStore;
use ferropress_theme::ThemeEngine;

use crate::{OutputPage, ServeEngine, cache_key, content, serve_path};

const PARAGRAPH_TEXT: &str = "Hello from the Ferropress cache test.";
const SLUG: &str = "hello-world";

/// The block-tree JSON String for a one-paragraph body. Built via the domain
/// types so it round-trips through `BlockTree::from_json_str` exactly.
fn paragraph_block_tree_json() -> String {
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
    tree.to_json_string()
        .expect("block tree serializes to JSON")
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
        Value::String(paragraph_block_tree_json()),
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
    let served = match serve_path(&store, &blobs, &theme, &path).await {
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
    match serve_path(&store, &blobs, &theme, &path).await {
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

    let engine = ServeEngine::new(Arc::clone(&store), Arc::clone(&blobs), Arc::clone(&theme));
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
