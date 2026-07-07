//! The admin SPA: one whole-page rinch app with three views (login → post list →
//! editor) driven by a `Signal<View>`. Ported from the letterpress "composing room"
//! mockup (`design/mockup.html`).
//!
//! The rich-text editor is rinch's `Editor` (re-exported by `rinch-web`); content
//! crosses the wire as Ferropress `BlockTree` JSON and is converted to/from the
//! editor's `DocNode` by `ferropress-editor-bridge`. The `EditorHandle` lives in a
//! `Signal` (which is `Copy`) so every nested reactive closure can obtain a clone
//! with `.get()` — no ownership threading.

use gloo_timers::future::TimeoutFuture;
use rinch::prelude::*;
use rinch_editor_core::Schema;
use rinch_web::{Editor, EditorHandle, create_editor};
use wasm_bindgen_futures::spawn_local;

use ferropress_editor_bridge as bridge;

use crate::api::{self, PostSummary, UserDto};

/// Which of the three views is showing. `Boot` is the transient initial state while
/// the session cookie is checked.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Boot,
    Login,
    List,
    Editor,
}

/// Load state of the post list.
#[derive(Clone, Copy, PartialEq)]
enum Load {
    Loading,
    Ready,
    Error,
}

/// The signals a guarded request needs to route back to login on a 401. `Signal` is
/// `Copy`, so this bundle is `Copy` too and threads through the async closures freely.
#[derive(Clone, Copy)]
struct AuthCtx {
    view: Signal<View>,
    me_user: Signal<Option<UserDto>>,
    login_error: Signal<String>,
}

impl AuthCtx {
    /// A guarded endpoint returned 401 (expired / revoked session): forget the user,
    /// explain on the login screen, and route there — never a false "outage".
    fn session_expired(self) {
        self.me_user.set(None);
        self.login_error
            .set("Your session ended \u{2014} please sign in again.".to_owned());
        self.view.set(View::Login);
    }
}

#[component]
pub fn app() -> NodeHandle {
    let view = Signal::new(View::Boot);
    let me_user = Signal::new(Option::<UserDto>::None);

    // Login view.
    let username = Signal::new(String::new());
    let password = Signal::new(String::new());
    let login_error = Signal::new(String::new());
    let signing_in = Signal::new(false);

    // List view.
    let posts = Signal::new(Vec::<PostSummary>::new());
    let list_state = Signal::new(Load::Loading);

    // Editor view. The handle lives in a Signal (Copy) so it flows into every
    // closure freely; `.get()` returns a clone of the underlying `Rc` editor.
    let editor = Signal::new(create_editor());
    let current_id = Signal::new(Option::<u64>::None);
    // A monotonic "editing session" generation, bumped whenever a different document
    // is loaded into the shared editor (open a post / start a new one). A save
    // captures it and, on completion, ignores its own writeback if the generation
    // has moved on — so a slow save that finishes after the user navigated away can
    // never clobber the now-current post's id (see `save_post`).
    let editor_session = Signal::new(0u64);
    let title = Signal::new(String::new());
    let slug = Signal::new(String::new());
    let status = Signal::new(String::new());
    let saving = Signal::new(false);
    let notice = Signal::new(String::new());
    let toast = Signal::new(false);

    // The re-auth routing bundle, shared by every guarded request.
    let auth = AuthCtx {
        view,
        me_user,
        login_error,
    };

    // Boot: pick login vs list from the session cookie (`GET /admin/api/me`).
    spawn_local(async move {
        match api::me().await {
            api::Auth::User(user) => {
                me_user.set(Some(user));
                load_posts(posts, list_state, auth);
                view.set(View::List);
            }
            // 401 or any error → show the login view (login surfaces real errors).
            _ => view.set(View::Login),
        }
    });

    rsx! {
        div { class: "fp-admin",
            match view.get() {
                View::Boot => div { class: "login",
                    div { class: "ruler" }
                    div { class: "login__stage", p { class: "brand__sub", "loading the composing room" } }
                },

                // ── LOGIN ──────────────────────────────────────────────────────
                View::Login => div { class: "login",
                    div { class: "ruler" }
                    div { class: "login__stage",
                        div { class: "login__plate",
                            div { class: "brand",
                                span { class: "regmark", style: "font-size:1.7rem", "\u{2295}" }
                                h1 { class: "brand__word", "Ferropress" }
                            }
                            p { class: "brand__sub", "composing room" }
                            div { class: "card",
                                div { class: "field",
                                    label { "Username" }
                                    input {
                                        class: "input input--mono",
                                        placeholder: "jane",
                                        oninput: move |v: String| username.set(v),
                                    }
                                }
                                div { class: "field",
                                    label { "Password" }
                                    input {
                                        class: "input",
                                        r#type: "password",
                                        placeholder: "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}",
                                        oninput: move |v: String| password.set(v),
                                    }
                                }
                                if !login_error.get().is_empty() {
                                    p { class: "note", {move || login_error.get()} }
                                }
                                button {
                                    class: "btn btn--primary",
                                    onclick: move || {
                                        if signing_in.get() { return; }
                                        let u = username.get();
                                        let p = password.get();
                                        if u.trim().is_empty() || p.is_empty() {
                                            login_error.set("Enter a username and password.".to_owned());
                                            return;
                                        }
                                        login_error.set(String::new());
                                        signing_in.set(true);
                                        spawn_local(async move {
                                            match api::login(&u, &p).await {
                                                api::Auth::User(user) => {
                                                    me_user.set(Some(user));
                                                    load_posts(posts, list_state, auth);
                                                    view.set(View::List);
                                                }
                                                api::Auth::Anonymous => login_error
                                                    .set("That username and password don't match.".to_owned()),
                                                api::Auth::Error(e) => login_error.set(format!("Sign-in failed: {e}")),
                                            }
                                            signing_in.set(false);
                                        });
                                    },
                                    {move || if signing_in.get() { "Signing in\u{2026}" } else { "Sign in \u{2192}" }}
                                }
                            }
                        }
                    }
                    p { class: "login__footer", "build proofs \u{00B7} press live" }
                },

                // ── POST LIST (THE GALLEY) ─────────────────────────────────────
                View::List => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            span { class: "masthead__brand",
                                span { class: "regmark", style: "font-size:1.05rem", "\u{2295}" }
                                "Ferropress"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here", "Posts" }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary",
                                style: "width:auto",
                                onclick: move || new_post(
                                    editor, title, slug, status, current_id, editor_session, notice, auth,
                                ),
                                "\u{002B} New post"
                            }
                            span { class: "masthead__user",
                                span { class: "masthead__avatar", {move || avatar_initial(&me_user.get())} }
                                {move || display_name(&me_user.get())}
                            }
                            button {
                                class: "btn btn--quiet",
                                onclick: move || {
                                    spawn_local(async move {
                                        // Only claim signed-out if the server actually
                                        // cleared the (HttpOnly) cookie; JS can't clear
                                        // it, so a failed logout must NOT fake it.
                                        match api::logout().await {
                                            Ok(()) => {
                                                me_user.set(None);
                                                view.set(View::Login);
                                            }
                                            Err(_) => notice.set(
                                                "Couldn't sign out \u{2014} check your connection and try again."
                                                    .to_owned(),
                                            ),
                                        }
                                    });
                                },
                                "Sign out"
                            }
                        }
                    }
                    div { class: "wrap",
                        div { class: "galley__head",
                            h2 { class: "galley__title", "Posts" }
                            span { class: "galley__count",
                                {move || match list_state.get() {
                                    Load::Ready => format!("{} in the galley", posts.get().len()),
                                    _ => String::new(),
                                }}
                            }
                        }
                        if !notice.get().is_empty() {
                            div { class: "galley__state err", {move || notice.get()} }
                        }
                        if matches!(list_state.get(), Load::Loading) {
                            div { class: "galley__state", "Setting the galley\u{2026}" }
                        }
                        if matches!(list_state.get(), Load::Error) {
                            div { class: "galley__state err", "The galley is unavailable right now." }
                        }
                        for row in row_vms(&posts.get()) {
                            button {
                                key: row.id,
                                class: "row",
                                onclick: {
                                    let id = row.id;
                                    move || open_post(id, editor, title, slug, status, current_id, editor_session, notice, auth)
                                },
                                span { class: "row__mark", "\u{2295}" }
                                span {
                                    span { class: "row__title", {row.title} }
                                    span { class: "row__slug", {row.slug_path} }
                                }
                                span { class: row.stamp_class, {row.stamp_label} }
                                span { class: "row__time", {row.time} }
                                span { class: "row__edit", "Edit \u{2192}" }
                            }
                        }
                        if matches!(list_state.get(), Load::Ready) && posts.get().is_empty() {
                            div { class: "galley__state", "No posts yet." }
                        }
                    }
                },

                // ── EDITOR (THE SHEET) ─────────────────────────────────────────
                View::Editor => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || {
                                    notice.set(String::new());
                                    load_posts(posts, list_state, auth);
                                    view.set(View::List);
                                },
                                "\u{2190} Posts"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here", {move || masthead_title(&title.get())} }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary",
                                style: "width:auto",
                                onclick: move || save_post(
                                    editor, current_id, editor_session, title, slug, status, saving, notice, toast, auth,
                                ),
                                {move || if saving.get() { "Saving\u{2026}" } else { "Save" }}
                            }
                        }
                    }
                    div { class: "wrap",
                        div { class: "editor__meta",
                            div { class: "metaitem",
                                label { "Slug" }
                                span { class: "slugbox",
                                    span { class: "slugbox__host", "/" }
                                    input {
                                        value: slug.get(),
                                        spellcheck: "false",
                                        oninput: move |v: String| slug.set(v),
                                    }
                                }
                            }
                            div { class: "metaitem",
                                label { "Status" }
                                // Native <select> (rinch#95 now delivers its change via
                                // `oninput`). Uncontrolled: the current status is rendered
                                // first so the browser shows it as the default — rinch emits
                                // a boolean `selected` attr even when false, so a per-option
                                // `selected` can't mark just one.
                                select {
                                    class: "select",
                                    oninput: move |v: String| status.set(v),
                                    for entry in status_options(&status.get()) {
                                        option { key: entry.0.clone(), value: entry.0, {entry.1} }
                                    }
                                }
                            }
                        }
                        if !notice.get().is_empty() {
                            div { class: "editor__error", {move || notice.get()} }
                        }
                        div { class: "toolbar", role: "toolbar",
                            button { class: "tool tool--i", title: "Bold", onclick: move || { editor.get().command("toggleBold"); }, b { "B" } }
                            button { class: "tool tool--i", title: "Italic", onclick: move || { editor.get().command("toggleItalic"); }, i { "I" } }
                            button { class: "tool tool--label", title: "Underline", style: "text-decoration:underline", onclick: move || { editor.get().command("toggleUnderline"); }, "U" }
                            button { class: "tool tool--label", title: "Strikethrough", style: "text-decoration:line-through", onclick: move || { editor.get().command("toggleStrike"); }, "S" }
                            button { class: "tool tool--label", title: "Inline code", onclick: move || { editor.get().command("toggleCode"); }, "</>" }
                            button { class: "tool", title: "Link", onclick: move || edit_link_via_prompt(editor, notice), "\u{1F517}" }
                            span { class: "toolbar__sep" }
                            button { class: "tool tool--label", title: "Heading 2", onclick: move || { editor.get().command("setHeading2"); }, "H2" }
                            button { class: "tool tool--label", title: "Heading 3", onclick: move || { editor.get().command("setHeading3"); }, "H3" }
                            span { class: "toolbar__sep" }
                            button { class: "tool", title: "Quote", onclick: move || { editor.get().command("wrapInBlockquote"); }, "\u{201C}" }
                            button { class: "tool", title: "Bulleted list", onclick: move || { editor.get().command("toggleBulletList"); }, "\u{2022}" }
                            button { class: "tool tool--label", title: "Code block", onclick: move || { editor.get().command("setCodeBlock"); }, "{ }" }
                            span { class: "toolbar__sep" }
                            button { class: "tool", title: "Insert image", onclick: move || insert_image_via_picker(editor, notice, auth), "\u{25A6}" }
                            span { class: "toolbar__spacer" }
                            span { class: "toolbar__measure",
                                span { class: "regmark", style: "font-size:.75rem", "\u{2295}" }
                                "measure"
                            }
                        }
                        div { class: "sheet",
                            div { class: "sheet__inner",
                                // The title is the headline set on the sheet (per the
                                // mockup). Uncontrolled like the slug field: `value` is
                                // read once at arm-build — after `open_post`/`new_post`
                                // set the signal — and `oninput` feeds edits back (a
                                // reactive `value` would fight the caret). Typing here
                                // live-updates the masthead, which reads the same signal.
                                input {
                                    class: "sheet__title",
                                    value: title.get(),
                                    placeholder: "Untitled",
                                    spellcheck: "false",
                                    oninput: move |v: String| title.set(v),
                                }
                                Editor { editor: editor.get(), content: "" }
                            }
                        }
                    }
                },
            }

            div { class: {move || if toast.get() { "toast is-shown" } else { "toast" }},
                span { class: "regmark", style: "font-size:.85rem", "\u{2295}" }
                "Saved"
            }
        }
    }
}

/// A galley row's presentation fields, precomputed so the reactive `for` body reads
/// each one exactly once (each `{…}` child moves its value; a shared `post.status`
/// used for both the stamp class and label would double-move).
#[derive(Clone, PartialEq)]
struct RowVm {
    id: u64,
    title: String,
    slug_path: String,
    stamp_class: &'static str,
    stamp_label: String,
    time: String,
}

fn row_vms(posts: &[PostSummary]) -> Vec<RowVm> {
    posts
        .iter()
        .map(|p| RowVm {
            id: p.id,
            title: p.title.clone(),
            slug_path: format!("/{}", p.slug),
            stamp_class: api::status_stamp_class(&p.status),
            stamp_label: api::status_label(&p.status),
            time: api::fmt_relative(p.updated_at),
        })
        .collect()
}

/// Fetch the post list into `posts`, tracking `state`. A 401 routes back to login
/// (an expired session), NOT a false "galley unavailable" outage.
fn load_posts(posts: Signal<Vec<PostSummary>>, state: Signal<Load>, auth: AuthCtx) {
    state.set(Load::Loading);
    spawn_local(async move {
        match api::list_posts().await {
            Ok(list) => {
                posts.set(list);
                state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => state.set(Load::Error),
        }
    });
}

/// Open a post in the editor: fetch it, populate the meta fields, bridge its
/// `BlockTree` into the editor, then switch to the editor view. A bridge failure
/// loads an EMPTY document and surfaces a warning rather than corrupting content.
#[allow(clippy::too_many_arguments)]
fn open_post(
    id: u64,
    editor: Signal<EditorHandle>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    spawn_local(async move {
        match api::get_post(id).await {
            Ok(detail) => {
                // A new document is taking over the shared editor: bump the session
                // so any still-in-flight save can no longer write back its id here.
                editor_session.update(|g| *g += 1);
                title.set(detail.title);
                slug.set(detail.slug);
                status.set(detail.status);
                current_id.set(Some(detail.id));

                let schema = Schema::starter_kit();
                let handle = editor.get();
                match bridge::block_tree_json_to_node(&schema, &detail.block_tree) {
                    Ok(node) => handle.load_doc(node),
                    Err(e) => {
                        if let Ok(node) = bridge::block_tree_json_to_node(&schema, &empty_tree()) {
                            handle.load_doc(node);
                        }
                        notice.set(format!(
                            "This post uses content the editor couldn't load ({e}). Saving will overwrite it."
                        ));
                    }
                }
                auth.view.set(View::Editor);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(e)) => notice.set(format!("Couldn't open that post: {e}")),
        }
    });
}

/// Persist the current editor document: read it back, bridge to `BlockTree`, and
/// send it. A post with no id yet (a fresh "New post") is CREATED (`POST`) and its
/// assigned id captured into `current_id` so the next save UPDATES (`PUT`) it in
/// place. Surfaces the server's 400/409 message; stamps a "Saved" toast on success.
#[allow(clippy::too_many_arguments)]
fn save_post(
    editor: Signal<EditorHandle>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    saving: Signal<bool>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }

    let node = editor.get().doc();
    let block_tree = match bridge::node_to_block_tree_json(&node) {
        Ok(v) => v,
        Err(e) => {
            notice.set(format!("Couldn't prepare the document to save: {e}"));
            return;
        }
    };
    let slug_val = slug.get().trim().to_owned();
    if slug_val.is_empty() {
        notice.set("The slug can't be empty.".to_owned());
        return;
    }

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    // The document this save belongs to. If the editor loads a different document
    // before the request returns, the result is stale and must NOT write back its
    // id / toast / error into what is now a different editing context.
    let save_gen = editor_session.get();
    spawn_local(async move {
        // Create when there's no id yet, else update in place. Both map to
        // `Result<Option<u64>, _>` where `Some(id)` is a freshly assigned id.
        let result = match current_id.get() {
            Some(id) => api::save_post(
                id,
                &api::SaveRequest {
                    title: title_val,
                    slug: slug_val,
                    status: status_val,
                    block_tree,
                },
            )
            .await
            .map(|()| None),
            None => api::create_post(&api::CreateRequest {
                title: title_val,
                slug: slug_val,
                status: status_val,
                block_tree,
            })
            .await
            .map(Some),
        };
        // The editor moved to another document while we were in flight: the save
        // still landed server-side, but adopting its id here would hijack the new
        // document. Free the save lock and drop the writeback. (A 401 is auth-wide,
        // so it still routes to login regardless.)
        let stale = editor_session.get() != save_gen;
        saving.set(false);
        match result {
            Ok(new_id) => {
                if stale {
                    return;
                }
                // A create hands back the new id; record it so the next save updates.
                if new_id.is_some() {
                    current_id.set(new_id);
                }
                toast.set(true);
                TimeoutFuture::new(1600).await;
                toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                if !stale {
                    notice.set(e);
                }
            }
        }
    });
}

/// Start a brand-new post: reset the editor meta to a blank Draft, clear the shared
/// editor to an empty document, and switch to the editor view. `current_id` is set
/// to `None` so the first Save creates the post (see [`save_post`]). The signals are
/// set BEFORE the view switch so the uncontrolled title/slug inputs read the blank
/// values at arm-build.
#[allow(clippy::too_many_arguments)]
fn new_post(
    editor: Signal<EditorHandle>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    // A fresh document takes over the shared editor: bump the session so any
    // still-in-flight save can no longer write its id back into this blank post.
    editor_session.update(|g| *g += 1);
    notice.set(String::new());
    title.set(String::new());
    slug.set(String::new());
    status.set("draft".to_owned());
    current_id.set(None);

    // Clear whatever the shared editor held from a previously opened post.
    let schema = Schema::starter_kit();
    let handle = editor.get();
    if let Ok(node) = bridge::block_tree_json_to_node(&schema, &empty_tree()) {
        handle.load_doc(node);
    }
    auth.view.set(View::Editor);
}

/// Add, replace, or remove a hyperlink on the current selection.
///
/// A link spans a text RANGE, so this needs a non-empty selection (unlike image
/// insert, which drops an atom at the caret). rinch registers no string command to
/// *add* a link — only `removeLink` — because a link carries an `href` and the
/// arg-less `command(&str)` facade can't pass one; so the mark is built and
/// dispatched through the general `update` path using the upstream
/// `commands::toggle_link(href)` builder.
///
/// Flow: prompt for the URL, clear any link already on the selection (so re-linking
/// *replaces* the target rather than layering a second `link` mark), and — for a
/// non-empty, script-safe URL — apply the new link. Clearing the field and
/// confirming removes the link; cancelling (Esc) changes nothing. The URL is checked
/// with the SAME `ferropress_core::is_safe_href` policy the renderer enforces, so a
/// rejected scheme (e.g. `javascript:`) is never stored — and the author is told why.
fn edit_link_via_prompt(editor: Signal<EditorHandle>, notice: Signal<String>) {
    let handle = editor.get();

    if handle.selection().is_empty() {
        notice.set("Select the text you want to link first.".to_owned());
        return;
    }

    // Cancelling the prompt (Esc / no window) leaves the selection untouched.
    let Some(input) = web_sys::window().and_then(|w| w.prompt_with_message("Link URL:").ok().flatten())
    else {
        return;
    };
    let href = input.trim().to_owned();

    // Validate BEFORE mutating anything, so a rejected URL never disturbs a link
    // already on the selection. An empty URL is allowed here — it means "unlink".
    // Report a whitespace/control fault distinctly from a bad scheme, so the author
    // isn't told an otherwise-valid http(s) URL has the "wrong scheme" when the real
    // problem is an embedded space.
    if !href.is_empty() {
        if href.chars().any(|c| c.is_control() || c.is_whitespace()) {
            notice.set("Link URL can't contain spaces or control characters.".to_owned());
            return;
        }
        if !ferropress_core::is_safe_href(&href) {
            notice.set("Links must be http(s), mailto, or a site-relative path.".to_owned());
            return;
        }
    }

    // Clear any existing link on the range so a new URL replaces it cleanly (rather
    // than stacking marks); an empty URL then simply leaves the text unlinked.
    if handle.is_mark_active("link") {
        handle.command("removeLink");
    }
    if href.is_empty() {
        return;
    }

    // No named command carries an href, so run the `toggle_link` builder through the
    // general dispatch path (which re-projects the DOM). The selection now has no
    // link (cleared above), so `toggle_link` adds one.
    let applied = handle.update(move |state| {
        let command = rinch_editor_core::commands::toggle_link(href);
        let mut tx = None;
        command(state, Some(&mut |t| tx = Some(t)));
        tx
    });
    if !applied {
        notice.set("Couldn't add the link \u{2014} try selecting the text again.".to_owned());
    }
}

/// Open the OS image picker, upload the chosen file, and insert it at the caret.
///
/// The `<input type=file>` is created and `.click()`ed SYNCHRONOUSLY inside this
/// (trusted) toolbar-button handler, so the browser opens the dialog — a file dialog
/// must be initiated within a user gesture. The upload + insert then run async: we
/// prompt for alt text, POST the bytes as multipart, and on success insert an `image`
/// node whose `src` is the server-issued `/media/{id}` URL — the SAME URL the bridge
/// reverses to a `media_id` on save and the public page renders, so the in-editor
/// image matches the published page.
fn insert_image_via_picker(editor: Signal<EditorHandle>, notice: Signal<String>, auth: AuthCtx) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(input) = document
        .create_element("input")
        .ok()
        .and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok())
    else {
        return;
    };
    input.set_type("file");
    let _ = input.set_attribute("accept", "image/*");

    // Fires when the user picks a file. `Closure::forget` intentionally leaks this
    // one-shot handler (a few hundred bytes) so it outlives this function and is alive
    // when the dialog resolves — the standard wasm-bindgen idiom for a detached DOM
    // callback; image inserts are infrequent, so the leak is immaterial.
    let picker = input.clone();
    let on_change = Closure::<dyn FnMut()>::new(move || {
        let Some(file) = picker.files().and_then(|files| files.get(0)) else {
            return;
        };
        spawn_local(async move {
            // Alt text for accessibility. Cancelling the prompt yields an empty alt
            // (the server's default too) rather than aborting the upload.
            let alt = web_sys::window()
                .and_then(|w| {
                    w.prompt_with_message("Describe this image (alt text):")
                        .ok()
                        .flatten()
                })
                .unwrap_or_default();

            let Ok(form) = web_sys::FormData::new() else {
                notice.set("Your browser blocked the upload form.".to_owned());
                return;
            };
            let _ = form.append_with_blob_and_filename("file", &file, &file.name());
            let _ = form.append_with_str("alt", &alt);

            match api::upload_media(form).await {
                Ok(resp) => {
                    if !editor.get().insert_image(&resp.url, &alt) {
                        notice.set(
                            "Couldn't place the image \u{2014} click into the sheet, then insert again."
                                .to_owned(),
                        );
                    }
                }
                Err(api::ApiError::Unauthorized) => auth.session_expired(),
                Err(api::ApiError::Message(e)) => notice.set(format!("Image upload failed: {e}")),
            }
        });
    });
    input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    on_change.forget();
    input.click();
}

/// The editor masthead's "you are here" label — the post title, or a neutral
/// placeholder while it's still empty (a fresh post, or one whose title was cleared).
fn masthead_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Untitled".to_owned()
    } else {
        title.to_owned()
    }
}

fn empty_tree() -> serde_json::Value {
    serde_json::json!({ "schema_version": 1, "blocks": [] })
}

/// The masthead avatar letter — the display name's (or username's) first char.
fn avatar_initial(user: &Option<UserDto>) -> String {
    user.as_ref()
        .and_then(|u| {
            u.display_name
                .chars()
                .next()
                .or_else(|| u.username.chars().next())
        })
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

/// The status options with the current one FIRST — an uncontrolled `<select>` shows
/// its first option, so this makes the dropdown default to the post's current status
/// (a per-option `selected` attr can't work: rinch emits it even when false). Owned
/// `String`s so a status outside [`api::STATUSES`] is still shown + preserved.
fn status_options(current: &str) -> Vec<(String, String)> {
    let mut opts = Vec::with_capacity(api::STATUSES.len() + 1);
    opts.push((current.to_owned(), api::status_label(current)));
    for (value, label) in api::STATUSES {
        if *value != current {
            opts.push(((*value).to_owned(), (*label).to_owned()));
        }
    }
    opts
}

/// The signed-in user's display name (falls back to the username).
fn display_name(user: &Option<UserDto>) -> String {
    user.as_ref()
        .map(|u| {
            if u.display_name.is_empty() {
                u.username.clone()
            } else {
                u.display_name.clone()
            }
        })
        .unwrap_or_default()
}
