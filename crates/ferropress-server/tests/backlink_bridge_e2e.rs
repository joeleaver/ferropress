//! Full-stack e2e for the `content:write` feed path, wiring the SAME subsystems the
//! composition root does: a real [`EmbeddedStore`], a [`PluginHost`] with both the
//! `content:read` and `content:write` backends, the real `backlink-index` WASM plugin
//! loaded from `plugins/dist`, and a running [`HookBridge`].
//!
//! It proves, through the real change-feed → bridge → WASM → `content:write` path,
//! that:
//!   1. the real backlink-index plugin records a backlink on a linked target, and
//!   2. the plugin's own write does NOT feed-loop — the backlink `set_meta` on the
//!      target is stamped `PLUGIN_ORIGIN` and the bridge excludes it, so the target
//!      (which ONLY ever receives that plugin write) is dispatched ZERO action hooks.
//!      A counting dispatcher asserts exactly that: `target` never re-dispatches.
//!      Remove the guard and the target's write re-dispatches → the count is nonzero
//!      → this test fails (cleanly, not by hanging).
//!
//! Wasm-gated: skips (with a printed notice) unless the plugin is built — run
//! `cargo xtask build-plugins`. In CI this runs in the `plugins-wasm` job after the
//! plugins are built.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferropress_core::hook::{HookDispatcher, HookEvent};
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{FieldMap, ObjectId, TypeName, Value};
use ferropress_core::{
    Block, BlockKind, BlockTree, ContentReader, ContentWriter, POST_TYPE, Status,
};

use ferropress_plugin_host::PluginHost;
use ferropress_serve::HookBridge;
use ferropress_store_embedded::EmbeddedStore;

/// The repo's `plugins/dist` (this crate is `crates/ferropress-server`).
fn plugins_dist() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dist")
}

/// The built backlink-index wasm, or `None` if `build-plugins` hasn't run.
fn backlink_built() -> bool {
    plugins_dist()
        .join("backlink-index/ferropress_plugin_backlink_index.wasm")
        .exists()
}

/// Seed a published Post with the given slug/title and body, returning its id.
async fn seed_post(
    store: &Arc<dyn RhypeStore>,
    slug: &str,
    title: &str,
    body: serde_json::Value,
) -> ObjectId {
    let mut fields: FieldMap = FieldMap::new();
    fields.insert("slug".to_owned(), Value::String(slug.to_owned()));
    fields.insert("title".to_owned(), Value::String(title.to_owned()));
    fields.insert("post_type".to_owned(), Value::String("post".to_owned()));
    fields.insert(
        "status".to_owned(),
        Value::String(Status::Published.as_str().to_owned()),
    );
    fields.insert("block_tree".to_owned(), Value::Json(body));
    store
        .create(&TypeName::from(POST_TYPE), fields)
        .await
        .expect("seed post")
}

/// A [`HookDispatcher`] decorator that wraps the real plugin host, forwarding every
/// dispatch to it while counting actions per `object_id`. Lets the test assert the
/// loop-break directly: the `target` (written ONLY by the plugin's own
/// `PLUGIN_ORIGIN`-stamped `set_meta`) must be dispatched zero action hooks.
struct CountingDispatcher {
    inner: Arc<dyn HookDispatcher>,
    target_id: u64,
    target_dispatches: Mutex<u64>,
    total_dispatches: Mutex<u64>,
}

impl HookDispatcher for CountingDispatcher {
    fn dispatch(&self, event: HookEvent) -> ferropress_core::error::Result<HookEvent> {
        *self.total_dispatches.lock().unwrap() += 1;
        if event.payload.get("object_id").and_then(|v| v.as_u64()) == Some(self.target_id) {
            *self.target_dispatches.lock().unwrap() += 1;
        }
        self.inner.dispatch(event)
    }
    fn has_hooks(&self, name: &str) -> bool {
        self.inner.has_hooks(name)
    }
}

/// A body with a single `wiki` custom block whose text links `[[Target]]` — the
/// shape the backlink-index plugin parses out of `fields.block_tree`.
fn wiki_body_linking_target() -> serde_json::Value {
    let tree = BlockTree::from_blocks(vec![Block {
        uid: "01J0000000000000000000SRC0".to_owned(),
        kind: BlockKind::Custom {
            plugin: "wiki".to_owned(),
            name: "wiki".to_owned(),
            data: serde_json::json!({ "text": "See [[Target]] for details." }),
        },
        children: Vec::new(),
    }]);
    tree.to_json_value().expect("wiki block tree serializes")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlink_index_records_backlink_through_the_real_bridge_without_looping() {
    if !backlink_built() {
        eprintln!(
            "skipping backlink_index_records_backlink_through_the_real_bridge_without_looping: \
             backlink-index wasm not built — run `cargo xtask build-plugins`"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let concrete = Arc::new(EmbeddedStore::open(tmp.path().join("db")).expect("open store"));
    let store: Arc<dyn RhypeStore> = concrete.clone();
    let content_reader: Arc<dyn ContentReader> = concrete.clone();
    let content_writer: Arc<dyn ContentWriter> = concrete.clone();

    // A published target the wiki link resolves to, and a source post that links it.
    let target_id = seed_post(
        &store,
        "target",
        "Target",
        BlockTree::from_blocks(Vec::new())
            .to_json_value()
            .expect("empty tree"),
    )
    .await;
    let source_id = seed_post(&store, "source", "Source", wiki_body_linking_target()).await;

    // The plugin host wired exactly like production: both capabilities backed, the
    // real plugins loaded from dist. It is the bridge's hook dispatcher.
    let mut host = PluginHost::new()
        .with_content_reader(content_reader)
        .with_content_writer(content_writer);
    host.load_dir(plugins_dist()).expect("load plugins");
    let host: Arc<dyn HookDispatcher> = Arc::new(host);

    // Wrap the host so we can count action dispatches per object — the loop-break
    // assertion below reads `target_dispatches`.
    let dispatcher = Arc::new(CountingDispatcher {
        inner: host,
        target_id: target_id.0,
        target_dispatches: Mutex::new(0),
        total_dispatches: Mutex::new(0),
    });

    let bridge = Arc::new(HookBridge::new(Arc::clone(&store), dispatcher.clone()));
    let run = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        let _ = run.run().await;
    });

    // Touch the source post (title-only updates re-publish its FULL fields, incl.
    // block_tree) until the backlink lands on the target — the touch-loop dodges the
    // subscribe race, exactly like the serve live-bridge test. Each update dispatches
    // `post.updated` → backlink-index → `fp_set_meta` on the target.
    let target_tn = TypeName::from(POST_TYPE);
    let backlink_key = format!("from:{}", source_id.0);
    let mut landed: Option<serde_json::Value> = None;
    for i in 0..200u32 {
        let mut patch: FieldMap = FieldMap::new();
        patch.insert("title".to_owned(), Value::String(format!("Source {i}")));
        store
            .update(&TypeName::from(POST_TYPE), source_id, patch)
            .await
            .expect("touch source");
        tokio::time::sleep(Duration::from_millis(25)).await;

        let obj = store.get(&target_tn, target_id).await.expect("get target");
        if let Some(Value::Json(serde_json::Value::Object(meta))) = obj.get("meta")
            && let Some(serde_json::Value::Object(ns)) = meta.get("backlink-index")
            && let Some(v) = ns.get(&backlink_key)
        {
            landed = Some(v.clone());
            break;
        }
    }

    // Quiesce: give any (erroneous, guard-off) re-dispatch of the target's own write
    // time to arrive before we assert it did not happen.
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();

    let value = landed.expect(
        "the real backlink-index plugin must record a backlink on the target via the real bridge",
    );
    // The backlink value is the SOURCE slug (what "pages that link here" resolves to).
    assert_eq!(
        value,
        serde_json::json!("source"),
        "the backlink records the linking page's slug"
    );

    // The path was actually exercised (the source's updates dispatched the action).
    assert!(
        *dispatcher.total_dispatches.lock().unwrap() >= 1,
        "the action must have dispatched at least once"
    );
    // THE loop-break proof: the target is written ONLY by the plugin's own
    // PLUGIN_ORIGIN-stamped `set_meta`, which the bridge excludes — so the target
    // must never have been dispatched an action. Remove the guard and this write
    // re-dispatches `post.updated(target)` → this count is nonzero → test fails.
    assert_eq!(
        *dispatcher.target_dispatches.lock().unwrap(),
        0,
        "the plugin's own write to the target must not re-dispatch an action (feed-loop guard)"
    );
}
