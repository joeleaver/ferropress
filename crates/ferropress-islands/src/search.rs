//! The semantic-search island: a query box that calls `GET /api/search` and
//! lists matching published posts (title links to the post; excerpt below).
//!
//! Search runs on the button click (Enter-to-search needs raw key handling rinch
//! does not yet expose on bare inputs — a later refinement). Real results require
//! the server's ONNX embedding model at runtime; without it the API errors and
//! the island shows a graceful "unavailable" state.

use rinch::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::api::{self, SearchHit};

/// What the results area is currently showing.
#[derive(Clone, Copy, PartialEq)]
enum State {
    /// No search run yet.
    Idle,
    Searching,
    Results,
    Empty,
    Error,
}

/// One search result (extracted so each item owns its strings — same rinch
/// per-item rendering idiom as the comment rows). The excerpt is optional.
#[component]
fn ResultItem(slug: String, title: String, excerpt: String) -> NodeHandle {
    let has_excerpt = !excerpt.is_empty();
    // `excerpt` is read inside the reactive `if`, so hold it in a `Copy` Signal
    // (Rule 4); `slug`/`title` are one-shot outside any conditional.
    let excerpt = Signal::new(excerpt);
    rsx! {
        li { class: "fp-result",
            a { href: format!("/{slug}"), {title} }
            if has_excerpt {
                span { class: "fp-excerpt", {move || excerpt.get()} }
            }
        }
    }
}

#[component]
pub fn search_island() -> NodeHandle {
    let query = Signal::new(String::new());
    let results = Signal::new(Vec::<SearchHit>::new());
    let state = Signal::new(State::Idle);

    rsx! {
        section { class: "fp-island",
            h2 { class: "fp-h2", "Search" }
            div { class: "fp-search-bar",
                input {
                    class: "fp-input",
                    r#type: "search",
                    placeholder: "Search the site…",
                    oninput: move |v: String| query.set(v),
                }
                button {
                    class: "fp-btn",
                    // No reactive `disabled` (see the comments island): the handler
                    // guards against a concurrent search and the label swaps to
                    // "Searching…" for feedback.
                    onclick: move || {
                        let q = query.get().trim().to_owned();
                        if q.is_empty() {
                            state.set(State::Idle);
                            results.set(Vec::new());
                            return;
                        }
                        if matches!(state.get(), State::Searching) {
                            return;
                        }
                        state.set(State::Searching);
                        spawn_local(async move {
                            match api::fetch_search(&q).await {
                                Ok(hits) => {
                                    let empty = hits.is_empty();
                                    results.set(hits);
                                    state.set(if empty { State::Empty } else { State::Results });
                                }
                                Err(_) => state.set(State::Error),
                            }
                        });
                    },
                    {move || if matches!(state.get(), State::Searching) { "Searching…" } else { "Search" }}
                }
            }

            if matches!(state.get(), State::Searching) {
                p { class: "fp-state", "Searching…" }
            }
            if matches!(state.get(), State::Empty) {
                p { class: "fp-state", {move || format!("No results for “{}”.", query.get().trim())} }
            }
            if matches!(state.get(), State::Error) {
                p { class: "fp-state err", "Search is unavailable right now." }
            }

            ul { class: "fp-results",
                for hit in results.get() {
                    ResultItem {
                        key: hit.id,
                        slug: hit.slug,
                        title: hit.title,
                        excerpt: hit.excerpt,
                    }
                }
            }
        }
    }
}
