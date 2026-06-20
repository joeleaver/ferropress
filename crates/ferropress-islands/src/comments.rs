//! The live-comments island: lists the approved comments for the current page
//! and accepts a new (pending) comment.
//!
//! Threading is rendered by flattening the tree into one keyed list with a
//! per-row depth, so a single reactive `for` (not nested loops) drives the DOM;
//! replies carry the `.fp-reply` rail. The compose form posts a top-level comment
//! (per-reply forms are a later refinement) and, on success, swaps itself for the
//! "awaiting moderation" confirmation.

use rinch::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::api::{self, CommentDto, NewComment};

/// Load state of the comment thread.
#[derive(Clone, Copy, PartialEq)]
enum Load {
    Loading,
    Ready,
    Error,
}

/// A display-ready comment row (presentation fields precomputed so the `rsx!`
/// `for` body stays purely declarative). `depth > 0` is a reply.
#[derive(Clone, PartialEq)]
struct Row {
    id: u64,
    depth: u32,
    name: String,
    /// Author link, or empty when the commenter gave no URL.
    url: String,
    date: String,
    body: String,
}

/// Flatten the comment list into render rows: each top-level comment followed by
/// its (one level of) replies, preserving the server's chronological order.
fn rows(all: &[CommentDto]) -> Vec<Row> {
    let mut out = Vec::with_capacity(all.len());
    let mut push = |c: &CommentDto, depth: u32| {
        out.push(Row {
            id: c.id,
            depth,
            name: if c.author_name.is_empty() {
                "Anonymous".to_owned()
            } else {
                c.author_name.clone()
            },
            url: c.author_url.clone().unwrap_or_default(),
            date: api::fmt_date(&c.created_at),
            body: c.body.clone(),
        });
    };
    for top in all.iter().filter(|c| c.parent_id.is_none()) {
        push(top, 0);
        for reply in all.iter().filter(|c| c.parent_id == Some(top.id)) {
            push(reply, 1);
        }
    }
    out
}

/// CSS class for a row at the given depth (replies get the threading rail).
fn row_class(depth: u32) -> &'static str {
    if depth > 0 {
        "fp-comment fp-reply"
    } else {
        "fp-comment"
    }
}

/// One rendered comment. Extracted as a component so each row owns its strings
/// (the rinch idiom for per-item rendering — see the `TodoItem` pattern): the
/// reactive `if` for the optional author link needs a `Copy` condition and
/// distinct owned clones per branch.
#[component]
fn CommentRow(depth: u32, name: String, url: String, date: String, body: String) -> NodeHandle {
    let has_url = !url.is_empty();
    // `name`/`url` are read inside the reactive `if` (the optional author link),
    // so they must be `Copy` to be captured by the branch closures rinch requires
    // to be `Fn` — hold them in Signals (Rule 4). `date`/`body` are one-shot text
    // outside any conditional, so they stay plain owned Strings.
    let name = Signal::new(name);
    let url = Signal::new(url);
    rsx! {
        div { class: row_class(depth),
            div { class: "fp-comment-head",
                span { class: "fp-author",
                    if has_url {
                        a { href: {move || url.get()}, {move || name.get()} }
                    } else {
                        {move || name.get()}
                    }
                }
                span { class: "fp-time", {date} }
            }
            p { class: "fp-body", {body} }
        }
    }
}

#[component]
pub fn comments_island() -> NodeHandle {
    // Slug lives in a Signal so it is `Copy` and can be read freely from the
    // nested reactive closures (form `onclick`, fetch) without moving an owned
    // String through an `Fn` boundary.
    let slug = Signal::new(api::current_slug());
    let has_slug = !slug.get().is_empty();

    let comments = Signal::new(Vec::<CommentDto>::new());
    let load = Signal::new(if has_slug {
        Load::Loading
    } else {
        Load::Ready
    });

    // Compose-form state.
    let name = Signal::new(String::new());
    let email = Signal::new(String::new());
    let body = Signal::new(String::new());
    let submitting = Signal::new(false);
    let submitted = Signal::new(false);
    let form_error = Signal::new(String::new());

    // Initial load (skipped at the site root, where there is no post slug).
    if has_slug {
        spawn_local(async move {
            match api::fetch_comments(&slug.get()).await {
                Ok(list) => {
                    comments.set(list);
                    load.set(Load::Ready);
                }
                Err(_) => load.set(Load::Error),
            }
        });
    }

    rsx! {
        section { class: "fp-island",
            h2 { class: "fp-h2",
                "Comments"
                span { class: "fp-count",
                    {move || match load.get() {
                        Load::Ready => comments.get().len().to_string(),
                        _ => String::new(),
                    }}
                }
            }

            if matches!(load.get(), Load::Loading) {
                p { class: "fp-state", "Loading comments…" }
            }
            if matches!(load.get(), Load::Error) {
                p { class: "fp-state err", "Comments are unavailable right now." }
            }

            div { class: "fp-thread",
                for row in rows(&comments.get()) {
                    CommentRow {
                        key: row.id,
                        depth: row.depth,
                        name: row.name,
                        url: row.url,
                        date: row.date,
                        body: row.body,
                    }
                }
            }

            if matches!(load.get(), Load::Ready) && comments.get().is_empty() {
                p { class: "fp-state", "No comments yet — start the conversation." }
            }

            if submitted.get() {
                div { class: "fp-confirm",
                    strong { "Thanks for your comment." }
                    "It's awaiting moderation and will appear once approved."
                }
            } else {
                div { class: "fp-form",
                    div { class: "fp-form-title", "Leave a comment" }
                    div { class: "fp-row",
                        div { class: "fp-field",
                            label { "Name" }
                            input {
                                class: "fp-input",
                                placeholder: "Your name",
                                oninput: move |v: String| name.set(v),
                            }
                        }
                        div { class: "fp-field",
                            label { "Email (optional)" }
                            input {
                                class: "fp-input",
                                placeholder: "you@example.com",
                                oninput: move |v: String| email.set(v),
                            }
                        }
                    }
                    div { class: "fp-field",
                        label { "Comment" }
                        textarea {
                            class: "fp-textarea",
                            placeholder: "Add to the discussion…",
                            oninput: move |v: String| body.set(v),
                        }
                    }
                    if !form_error.get().is_empty() {
                        p { class: "fp-state err", {move || form_error.get()} }
                    }
                    button {
                        class: "fp-btn",
                        // No reactive `disabled` attribute: an HTML boolean attr is
                        // "present = disabled" regardless of value, and rinch sets
                        // it from the stringified bool — so a re-entrancy guard in
                        // the handler + the label swap give the feedback instead.
                        onclick: move || {
                            if submitting.get() {
                                return;
                            }
                            let author = name.get().trim().to_owned();
                            let text = body.get().trim().to_owned();
                            if author.is_empty() || text.is_empty() {
                                form_error.set("Please add your name and a comment.".to_owned());
                                return;
                            }
                            let mail = email.get().trim().to_owned();
                            form_error.set(String::new());
                            submitting.set(true);
                            let post_slug = slug.get();
                            spawn_local(async move {
                                let payload = NewComment {
                                    slug: &post_slug,
                                    author_name: &author,
                                    author_email: if mail.is_empty() { None } else { Some(mail.as_str()) },
                                    body: &text,
                                };
                                match api::post_comment(&payload).await {
                                    Ok(()) => submitted.set(true),
                                    Err(_) => form_error.set(
                                        "Sorry — your comment couldn't be posted. Please try again."
                                            .to_owned(),
                                    ),
                                }
                                submitting.set(false);
                            });
                        },
                        {move || if submitting.get() { "Posting…" } else { "Post comment" }}
                    }
                }
            }
        }
    }
}
