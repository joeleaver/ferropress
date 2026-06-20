//! # ferropress-serve
//!
//! Static-first hybrid orchestration. Pages are pre-rendered to HTML and stored
//! through the [`BlobStore`] port; a regeneration loop consumes
//! [`RhypeStore::subscribe`] and regenerates ONLY the pages affected by each
//! change — never a full rebuild. Generation is deferred / on-demand:
//! render-on-first-request -> cache -> regenerate-on-change. The same change
//! subscription doubles as the multi-instance cache-invalidation broadcast.
//!
//! This crate owns the *policy* (what a change invalidates, what to re-render);
//! it delegates the actual block-tree -> HTML to `ferropress-render` and the page
//! chrome to `ferropress-theme`, and it never talks to a concrete store/blob
//! backend — only the [`RhypeStore`] / [`BlobStore`] ports. STUB scaffold.

use std::sync::Arc;

use ferropress_core::ports::BlobStore;
use ferropress_core::query::{Change, SubscribeFilter};
use ferropress_core::store::RhypeStore;

/// Identifies one prerendered output page. The serve cache is keyed by the path
/// (URL path -> `BlobKey`); a content change maps to the set of `OutputPage`s it
/// invalidates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputPage {
    /// Site-relative URL path, e.g. `"/blog/hello-world"`.
    pub path: String,
}

/// Drives prerender + incremental regeneration over the ports.
pub struct ServeEngine {
    store: Arc<dyn RhypeStore>,
    blobs: Arc<dyn BlobStore>,
}

impl ServeEngine {
    /// Build the engine over the injected ports. The concrete adapters are chosen
    /// in `ferropress-server` (the composition root), never here.
    pub fn new(store: Arc<dyn RhypeStore>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { store, blobs }
    }

    /// Run the regeneration loop forever: subscribe to ALL changes, map each
    /// committed change to its affected output pages, and re-render + re-store
    /// exactly those.
    pub async fn regen_loop(&self) -> ferropress_core::error::Result<()> {
        let _stream = self.store.subscribe(SubscribeFilter::default()).await?;
        // while let Some(change) = stream.next().await {            // tokio_stream::StreamExt
        //     for page in self.affected_pages(&change) {
        //         let html = self.render_page(&page).await?;        // ferropress-render/-theme
        //         self.blobs.put(&page_blob_key(&page), html.into_bytes()).await?;
        //     }
        // }
        let _ = &self.blobs;
        todo!("consume the change stream -> regenerate affected pages -> BlobStore")
    }

    /// Map a committed change to the set of output pages it invalidates (the
    /// page itself, plus index/archive/feed pages that list it). Pure policy.
    pub fn affected_pages(&self, _change: &Change) -> Vec<OutputPage> {
        // TODO: derive from change.type_name + change.object_id:
        //   a Post change -> its permalink + the home/archive/term/feed pages.
        todo!("compute the invalidation set for a change")
    }

    /// Render a single output page to its final HTML (block-tree -> HTML via
    /// `ferropress-render`, then page chrome via `ferropress-theme`).
    pub async fn render_page(&self, _page: &OutputPage) -> ferropress_core::error::Result<String> {
        let _ = &self.store;
        // TODO: load the entity for `page`, ferropress_render::render(&block_tree,
        // RenderMode::Publish), then wrap in the theme template.
        todo!("render one output page to final HTML")
    }
}
