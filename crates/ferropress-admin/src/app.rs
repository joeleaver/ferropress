//! The admin SPA: one whole-page rinch app driven by a `Signal<View>` (login → content
//! list → editor, plus Settings/Plugins) and a `Signal<EntityKind>` axis that shares the
//! list + editor between flat Posts and hierarchical Pages (nested `/parent/child`
//! permalinks + a parent/menu_order/template). Ported from the letterpress "composing
//! room" mockup (`design/mockup.html`).
//!
//! The rich-text editor is rinch's `Editor` (re-exported by `rinch-web`); content
//! crosses the wire as Ferropress `BlockTree` JSON and is converted to/from the
//! editor's `DocNode` by `ferropress-editor-bridge`. The `EditorHandle` lives in a
//! `Signal` (which is `Copy`) so every nested reactive closure can obtain a clone
//! with `.get()` — no ownership threading.

use std::collections::{HashMap, HashSet};

use gloo_timers::future::TimeoutFuture;
use rinch::prelude::*;
use rinch_editor_core::Schema;
use rinch_web::{Editor, EditorHandle, create_editor};
use wasm_bindgen_futures::spawn_local;

use ferropress_editor_bridge as bridge;
use ferropress_form_view::{FormValues, MediaChosen, OnPickMedia, SchemaForm};
use ferropress_render_form::{FormSchema, SettingRefs};

use crate::api::{self, LinkTarget, PostSummary, UserDto};

/// Which of the three views is showing. `Boot` is the transient initial state while
/// the session cookie is checked.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Boot,
    Login,
    List,
    Editor,
    Settings,
    Plugins,
    /// The nav-menu "cabinet": the menus list + the theme-location assignment panel.
    Menus,
    /// Editing ONE menu's item tree (the reorderable forest + the add-item picker).
    MenuEditor,
}

/// Which content type the shared List + Editor views are working on. Posts are flat
/// (slug-addressed); Pages are hierarchical (nested `/parent/child` permalinks, a
/// `parent`/`menu_order`/`template`). The two share the same editor machinery (the
/// rinch editor, title/slug/status/featured signals, the toolbar); Pages just add the
/// hierarchy meta controls and hit the `/admin/api/pages` endpoints. A `Signal<EntityKind>`
/// (Copy) selects which — every List/Editor branch and dispatch reads it.
#[derive(Clone, Copy, PartialEq)]
enum EntityKind {
    Post,
    Page,
}

/// Load state of a fetched list.
#[derive(Clone, Copy, PartialEq)]
enum Load {
    Loading,
    Ready,
    Error,
}

/// What the shared settings form is editing: the first-party site settings, or a
/// specific plugin's config. The `Settings` view + its load/save switch on this, so
/// one FormSchemaRenderer serves both surfaces (the plugin config is a pure addition
/// — no new form-rendering code).
#[derive(Clone, PartialEq)]
enum SettingsTarget {
    Site,
    Plugin { id: String, name: String },
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

/// The deepest a menu item may nest (ancestor count). Mirrors the server's
/// `MAX_MENU_DEPTH = ferropress_serve::MAX_NAV_DEPTH - 1` (a serve const the wasm crate
/// can't import) — so the "indent" affordance greys out at exactly the depth the server
/// would reject. The server re-validates authoritatively; this is a UX guard only. Keep
/// in lockstep with that const (a menu deeper than the theme's recursion cap 500s the site).
const MAX_MENU_DEPTH: u32 = 5;

/// What a menu row points at. The id-based kinds carry their target id; a `Custom`
/// item's URL lives in its editable per-row signal (not here), so the target is
/// reconstructed at save time. Kind never changes without remove+re-add (WP-faithful),
/// so it is a static per-row fact — reordering only moves the row, never its kind.
#[derive(Clone, PartialEq, Default)]
enum RowKind {
    Post(u64),
    Page(u64),
    Term(u64),
    // `#[default]` only to satisfy the rinch `#[component]` macro (MenuRowView takes a
    // `RowKind` prop, whose generated default props need `RowKind: Default`).
    #[default]
    Custom,
}

/// One tree row's STRUCTURE (the flat-with-depth model). The tree is a pre-order
/// sequence held in `Signal<Vec<MenuRow>>`; a row's PARENT is implicit — the nearest
/// preceding row at `depth - 1`. Only structural ops (reorder / add / remove) mutate
/// this Vec; label/URL/new-tab edits go to the per-row [`RowEdit`] signals so a keystroke
/// never re-fires the whole tree. `resolved` is the static server display sidecar.
#[derive(Clone)]
struct MenuRow {
    /// Stable within the edit session — the `for` key. Loaded items reuse the server id
    /// string; new items get `"n{k}"` (disjoint from the numeric server ids).
    cid: String,
    /// `Some` = an existing item (its id round-trips on save); `None` = a new item.
    server_id: Option<u64>,
    depth: u32,
    kind: RowKind,
    /// The server-resolved display (title + optional href) for a Post/Page/Term target;
    /// `None` for a `Custom` item (its URL is the target).
    resolved: Option<api::ResolvedTarget>,
}

/// `PartialEq` deliberately EXCLUDES `depth`: the keyed tree `for` re-runs a surviving
/// row's view (and tears down its DOM, dropping focus) only when its data compares
/// unequal. An indent/outdent changes ONLY `depth` — and that is rendered by the row's
/// reactive `class:`/`style:` closures reading the live tree, NOT by a rebuild. Excluding
/// `depth` here keeps a nest/un-nest a pure in-place attribute update: focus on the just-
/// pressed Indent/Outdent button survives, and the reactive closures do their job. (A
/// reorder — move up/down — changes order, not per-row data, so it already only moves nodes.)
impl PartialEq for MenuRow {
    fn eq(&self, other: &Self) -> bool {
        self.cid == other.cid
            && self.server_id == other.server_id
            && self.kind == other.kind
            && self.resolved == other.resolved
    }
}

/// A row's EDITABLE state, one dedicated `Signal` per field (the SchemaForm/FieldRow
/// discipline). Held in a cid-keyed map OUTSIDE the tree Vec so a keystroke touches only
/// its own input node — never the collection signal that drives the tree `for`. `Copy`
/// (all fields are `Signal`), so it threads through row closures freely.
#[derive(Clone, Copy)]
struct RowEdit {
    label: Signal<String>,
    /// Used only by a `Custom` row (the editable URL).
    url: Signal<String>,
    new_tab: Signal<bool>,
}

/// The nav-menu editing signals, bundled into one `Copy` context (the `AuthCtx`
/// discipline) so the two views + the many helpers thread them without 20-arg
/// signatures. Shared app signals (`view`/`notice`/`toast`/`auth`) ride along too.
#[derive(Clone, Copy)]
struct MenuCtx {
    // Menus list + location panel.
    list: Signal<Vec<api::MenuSummary>>,
    list_state: Signal<Load>,
    locations: Signal<Vec<api::MenuLocationRow>>,
    // The open menu editor.
    edit_id: Signal<Option<u64>>,
    name: Signal<String>,
    slug: Signal<String>,
    /// The tree STRUCTURE (drives the `for`); reorder/add/remove only.
    tree: Signal<Vec<MenuRow>>,
    /// cid -> per-row edit signals (label / URL / new-tab).
    edits: Signal<HashMap<String, RowEdit>>,
    /// Monotonic source of new-item cids (`"n{k}"`).
    next_cid: Signal<u64>,
    load: Signal<Load>,
    saving: Signal<bool>,
    /// Set on any structural or field edit; cleared on a successful save/reload. Gates
    /// the unsaved-changes confirm on every exit path (+ the `beforeunload` guard).
    dirty: Signal<bool>,
    // The add-item picker.
    picker_open: Signal<bool>,
    candidates: Signal<api::LinkCandidates>,
    candidates_state: Signal<Load>,
    /// Monotonic request generation for the search — a stale (out-of-order) response is
    /// dropped so a slow earlier query can't overwrite a newer one (the `editor_session`
    /// discipline).
    candidates_gen: Signal<u64>,
    picker_query: Signal<String>,
    /// 0 = Pages, 1 = Posts, 2 = Custom link.
    picker_tab: Signal<u8>,
    custom_url: Signal<String>,
    custom_label: Signal<String>,
    picker_err: Signal<String>,
    // Shared app signals.
    view: Signal<View>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
}

// `Default` impls exist ONLY to satisfy the rinch `#[component]` macro, which derives a
// `Default` props struct for `MenuLocationView` (a `MenuCtx` prop). The default values are
// throwaway — the macro overwrites every field with the real props at construction. Kept a
// manual impl because `Signal<T>` has no `Default` (so `#[derive(Default)]` can't be used).
impl Default for AuthCtx {
    fn default() -> Self {
        AuthCtx {
            view: Signal::new(View::Boot),
            me_user: Signal::new(None),
            login_error: Signal::new(String::new()),
        }
    }
}

impl Default for MenuCtx {
    fn default() -> Self {
        MenuCtx {
            list: Signal::new(Vec::new()),
            list_state: Signal::new(Load::Loading),
            locations: Signal::new(Vec::new()),
            edit_id: Signal::new(None),
            name: Signal::new(String::new()),
            slug: Signal::new(String::new()),
            tree: Signal::new(Vec::new()),
            edits: Signal::new(HashMap::new()),
            next_cid: Signal::new(0),
            load: Signal::new(Load::Loading),
            saving: Signal::new(false),
            dirty: Signal::new(false),
            picker_open: Signal::new(false),
            candidates: Signal::new(api::LinkCandidates::default()),
            candidates_state: Signal::new(Load::Loading),
            candidates_gen: Signal::new(0),
            picker_query: Signal::new(String::new()),
            picker_tab: Signal::new(0),
            custom_url: Signal::new(String::new()),
            custom_label: Signal::new(String::new()),
            picker_err: Signal::new(String::new()),
            view: Signal::new(View::Boot),
            notice: Signal::new(String::new()),
            toast: Signal::new(false),
            auth: AuthCtx::default(),
        }
    }
}

#[component]
pub fn app() -> NodeHandle {
    let view = Signal::new(View::Boot);
    let me_user = Signal::new(Option::<UserDto>::None);
    // Which content type the List + Editor are on (posts vs pages). Boot/login land on
    // the Post list; the "Pages"/"Posts" nav flips it.
    let kind = Signal::new(EntityKind::Post);

    // Login view.
    let username = Signal::new(String::new());
    let password = Signal::new(String::new());
    let login_error = Signal::new(String::new());
    let signing_in = Signal::new(false);

    // List view. `posts`/`page_list` back the galley for their respective `kind`;
    // `list_state` is shared (only one entity's list shows at a time). `page_list`
    // doubles as the editor's Parent-picker source.
    let posts = Signal::new(Vec::<PostSummary>::new());
    let page_list = Signal::new(Vec::<api::PageSummary>::new());
    let list_state = Signal::new(Load::Loading);
    // The theme's page templates, for the editor's Template picker. Loaded once when
    // the pages list is first opened (theme metadata, effectively static).
    let templates = Signal::new(Vec::<api::TemplateOption>::new());

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
    // The post's featured image (Media id + thumbnail url), or None. Set on open,
    // cleared on new, echoed back on save to reconcile the `featured_media` relation.
    let featured = Signal::new(Option::<api::FeaturedMedia>::None);
    // Page-only editor meta (unused while editing a Post): the parent page id (None =
    // top-level), the sibling `menu_order` (held as a String for the number input,
    // parsed on save), and the chosen theme template value ("" = default). Set on
    // open_page/new_page, read on save_page.
    let parent = Signal::new(Option::<u64>::None);
    let menu_order = Signal::new(String::new());
    let template = Signal::new(String::new());
    let saving = Signal::new(false);
    let notice = Signal::new(String::new());
    let toast = Signal::new(false);

    // Settings view (Administrator only). The loaded schema + a `FormValues` handle
    // (the live map the form writes and Save reads) live in signals so the view arm
    // mounts them and the Save handler reads the same `FormValues` back.
    let settings_load = Signal::new(Load::Loading);
    let settings_schema = Signal::new(Option::<FormSchema>::None);
    let settings_values = Signal::new(Option::<FormValues>::None);
    let settings_saving = Signal::new(false);
    // What the settings form is currently editing (site vs a plugin), so its chrome,
    // load, and save target the right endpoint.
    let settings_target = Signal::new(SettingsTarget::Site);
    // The resolved references for the form's id-valued widgets (EntityRef options +
    // MediaPicker thumbnail URLs), set alongside the schema + values on load/save.
    let settings_refs = Signal::new(SettingRefs::default());

    // The media-library picker modal (opened by a MediaPicker "Choose" button).
    // `media_pick_sink` holds the callback the active MediaPicker field handed us — it
    // is invoked with the chosen media's (id, url); `media_pick_open` toggles the
    // modal; the library list + its load-state feed the grid.
    let media_pick_sink = Signal::new(Option::<MediaChosen>::None);
    let media_pick_open = Signal::new(false);
    let media_library = Signal::new(Vec::<api::MediaSummary>::new());
    let media_library_state = Signal::new(Load::Loading);

    // Plugins view (Administrator only): the list of installed plugins.
    let plugins_list = Signal::new(Vec::<api::PluginDescriptor>::new());
    let plugins_state = Signal::new(Load::Loading);

    // The re-auth routing bundle, shared by every guarded request.
    let auth = AuthCtx {
        view,
        me_user,
        login_error,
    };

    // Nav-menu editor (Editor+). One Copy bundle threads the many signals through the
    // two menu views + their helpers (the AuthCtx discipline).
    let menu = MenuCtx {
        list: Signal::new(Vec::<api::MenuSummary>::new()),
        list_state: Signal::new(Load::Loading),
        locations: Signal::new(Vec::<api::MenuLocationRow>::new()),
        edit_id: Signal::new(Option::<u64>::None),
        name: Signal::new(String::new()),
        slug: Signal::new(String::new()),
        tree: Signal::new(Vec::<MenuRow>::new()),
        edits: Signal::new(HashMap::<String, RowEdit>::new()),
        next_cid: Signal::new(0u64),
        load: Signal::new(Load::Loading),
        saving: Signal::new(false),
        dirty: Signal::new(false),
        picker_open: Signal::new(false),
        candidates: Signal::new(api::LinkCandidates::default()),
        candidates_state: Signal::new(Load::Loading),
        candidates_gen: Signal::new(0u64),
        picker_query: Signal::new(String::new()),
        picker_tab: Signal::new(0u8),
        custom_url: Signal::new(String::new()),
        custom_label: Signal::new(String::new()),
        picker_err: Signal::new(String::new()),
        view,
        notice,
        toast,
        auth,
    };
    // Guard a browser tab-close/reload while a menu edit is unsaved (the in-app exit
    // paths are guarded by a confirm; this catches the ones the app can't intercept).
    install_beforeunload_guard(menu.dirty, menu.view);
    // Escape closes the add-item picker.
    install_escape_guard(menu.picker_open);

    // The host media picker handed to every settings form: open the library modal and,
    // on selection, invoke the field's sink with the chosen media. Held in a Signal
    // (Copy) so the render closure captures it without moving the non-Copy value; the
    // picker closure captures only Copy signals + the Copy auth bundle.
    let on_pick_media = Signal::new(OnPickMedia::new(move |sink: MediaChosen| {
        media_pick_sink.set(Some(sink));
        media_pick_open.set(true);
        load_media(media_library, media_library_state, auth);
    }));

    // Boot: pick login vs list from the session cookie (`GET /admin/api/me`).
    spawn_local(async move {
        match api::me().await {
            api::Auth::User(user) => {
                me_user.set(Some(user));
                // Boot/login always land on the Post galley (the module contract). Reset
                // `kind` explicitly so a re-login within the same app instance — after the
                // user had switched to Pages before a logout/session-expiry — doesn't render
                // the stale Pages surface while `load_posts` fills the hidden `posts` signal.
                kind.set(EntityKind::Post);
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
                                                    // Land on the Post galley; reset `kind`
                                                    // so a re-login after visiting Pages
                                                    // doesn't strand the user on a stale
                                                    // Pages surface (see the boot handler).
                                                    kind.set(EntityKind::Post);
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
                            span { class: "masthead__here",
                                {move || match kind.get() {
                                    EntityKind::Post => "Posts",
                                    EntityKind::Page => "Pages",
                                }}
                            }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary",
                                style: "width:auto",
                                onclick: move || match kind.get() {
                                    EntityKind::Post => new_post(
                                        editor, title, slug, status, featured, current_id, editor_session, notice, auth,
                                    ),
                                    EntityKind::Page => new_page(
                                        editor, kind, title, slug, status, featured, parent, menu_order, template,
                                        current_id, editor_session, notice, auth,
                                    ),
                                },
                                {move || match kind.get() {
                                    EntityKind::Post => "\u{002B} New post",
                                    EntityKind::Page => "\u{002B} New page",
                                }}
                            }
                            // Switch between the Posts and Pages galleys. Shown to every
                            // editing role (the server scopes what each may see); Pages is
                            // the hierarchical content type.
                            if matches!(kind.get(), EntityKind::Post) {
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_pages_list(
                                        kind, page_list, list_state, templates, notice, view, auth,
                                    ),
                                    "Pages"
                                }
                            }
                            if matches!(kind.get(), EntityKind::Page) {
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_posts_list(kind, posts, list_state, notice, view, auth),
                                    "Posts"
                                }
                            }
                            // Menus is Editor+ (the ManageMenus capability). Shown to
                            // Editors and Administrators; the server enforces it regardless.
                            if can_manage_menus(&me_user.get()) {
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_menus(menu),
                                    "Menus"
                                }
                            }
                            // Plugins + Settings are Administrator-only; hide the nav
                            // for lower roles (the server enforces the capability regardless).
                            if is_admin(&me_user.get()) {
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_plugins(
                                        plugins_list, plugins_state, notice, view, auth,
                                    ),
                                    "Plugins"
                                }
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_settings(
                                        settings_load, settings_schema, settings_values, settings_refs,
                                        settings_target, notice, view, auth,
                                    ),
                                    "Settings"
                                }
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
                            h2 { class: "galley__title",
                                {move || match kind.get() {
                                    EntityKind::Post => "Posts",
                                    EntityKind::Page => "Pages",
                                }}
                            }
                            span { class: "galley__count",
                                {move || match (list_state.get(), kind.get()) {
                                    (Load::Ready, EntityKind::Post) => {
                                        format!("{} in the galley", posts.get().len())
                                    }
                                    (Load::Ready, EntityKind::Page) => {
                                        format!("{} in the tree", page_list.get().len())
                                    }
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
                        // Rows for the current entity. The Post galley is flat; the Page
                        // galley is the tree — rows indented by `depth`, server-sorted so a
                        // parent always precedes its children.
                        match kind.get() {
                            EntityKind::Post => div { class: "galley__rows",
                                for row in row_vms(&posts.get()) {
                                    button {
                                        key: row.id,
                                        class: "row",
                                        onclick: {
                                            let id = row.id;
                                            move || open_post(id, editor, title, slug, status, featured, current_id, editor_session, notice, auth)
                                        },
                                        // A one-shot component (built once per row) so the
                                        // thumbnail-or-crosshair choice isn't a reactive `if`,
                                        // which can't move a non-Copy String out of the row.
                                        RowLead { url: row.featured_url.clone().unwrap_or_default() }
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
                            },
                            EntityKind::Page => div { class: "galley__rows",
                                for row in page_row_vms(&page_list.get()) {
                                    button {
                                        key: row.id,
                                        class: "row",
                                        // Indent by tree depth (inline, overriding the row's
                                        // left padding) so nesting reads at a glance.
                                        style: row.indent,
                                        onclick: {
                                            let id = row.id;
                                            move || open_page(id, editor, kind, title, slug, status, featured, parent, menu_order, template, current_id, editor_session, notice, auth)
                                        },
                                        RowLead { url: row.featured_url.clone().unwrap_or_default() }
                                        span {
                                            span { class: "row__title", {row.title} }
                                            span { class: "row__slug", {row.path_disp} }
                                        }
                                        span { class: row.stamp_class, {row.stamp_label} }
                                        span { class: "row__time", {row.time} }
                                        span { class: "row__edit", "Edit \u{2192}" }
                                    }
                                }
                                if matches!(list_state.get(), Load::Ready) && page_list.get().is_empty() {
                                    div { class: "galley__state", "No pages yet." }
                                }
                            },
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
                                    match kind.get() {
                                        EntityKind::Post => load_posts(posts, list_state, auth),
                                        EntityKind::Page => load_pages(page_list, list_state, auth),
                                    }
                                    view.set(View::List);
                                },
                                {move || match kind.get() {
                                    EntityKind::Post => "\u{2190} Posts",
                                    EntityKind::Page => "\u{2190} Pages",
                                }}
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here", {move || masthead_title(&title.get())} }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--quiet",
                                style: "width:auto",
                                title: "Save, then open this draft in the real theme (new tab)",
                                onclick: move || match kind.get() {
                                    EntityKind::Post => preview_post(
                                        editor, current_id, editor_session, title, slug, status, featured, saving, notice, auth,
                                    ),
                                    EntityKind::Page => preview_page(
                                        editor, current_id, editor_session, title, slug, status, featured,
                                        parent, menu_order, template, saving, notice, auth,
                                    ),
                                },
                                "Preview"
                            }
                            button {
                                class: "btn btn--primary",
                                style: "width:auto",
                                onclick: move || match kind.get() {
                                    EntityKind::Post => save_post(
                                        editor, current_id, editor_session, title, slug, status, featured, saving, notice, toast, auth,
                                    ),
                                    EntityKind::Page => save_page(
                                        editor, current_id, editor_session, title, slug, status, featured,
                                        parent, menu_order, template, saving, notice, toast, auth,
                                    ),
                                },
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
                                    // Controlled: a reactive `value` binding keeps the input
                                    // in sync with the signal (an `open_post`/`new_post` load
                                    // reflects here) without fighting the caret — rinch writes
                                    // the DOM *property* only when it differs (pin `2ea7625`,
                                    // upstream #100), so the type→oninput→signal echo is a no-op.
                                    input {
                                        value: {move || slug.get()},
                                        spellcheck: "false",
                                        oninput: move |v: String| slug.set(v),
                                    }
                                }
                            }
                            div { class: "metaitem",
                                label { "Status" }
                                // Controlled <select>: each option marks itself selected when
                                // it matches the signal (`selected: {|| …}`). rinch reflects the
                                // `selected` *property* and maps the stringified `false` to unset
                                // (pin `2ea7625`, upstream #100), so exactly one option is marked
                                // regardless of DOM order — no "render the current option first"
                                // trick, and `oninput` writes edits straight back.
                                select {
                                    class: "select",
                                    oninput: move |v: String| status.set(v),
                                    for entry in api::STATUSES.iter().copied() {
                                        option {
                                            key: entry.0,
                                            value: entry.0,
                                            selected: {move || status.get() == entry.0},
                                            {entry.1}
                                        }
                                    }
                                }
                            }
                            div { class: "metaitem",
                                label { "Featured image" }
                                // Native `if let`/`else` is reactive (tracks `featured`):
                                // show the thumbnail + Remove when set, else the picker
                                // button. No public display yet (no theme) — stored for it.
                                if let Some(f) = featured.get() {
                                    div { class: "featured",
                                        img { class: "featured__thumb", src: f.url, alt: "" }
                                        button {
                                            class: "btn btn--quiet", style: "width:auto",
                                            onclick: move || featured.set(None),
                                            "Remove"
                                        }
                                    }
                                } else {
                                    button {
                                        class: "btn btn--quiet", style: "width:auto",
                                        onclick: move || set_featured_via_picker(featured, editor_session, notice, auth),
                                        "Set featured image"
                                    }
                                }
                            }
                            // ── Page-only hierarchy meta (hidden while editing a Post) ──
                            // Parent picker: every page except this one and its own subtree
                            // (the server also rejects a cycle). Controlled per-option
                            // `selected`; the empty value means top-level.
                            if matches!(kind.get(), EntityKind::Page) {
                                div { class: "metaitem",
                                    label { "Parent" }
                                    select {
                                        class: "select",
                                        oninput: move |v: String| {
                                            parent.set(if v.is_empty() { None } else { v.parse::<u64>().ok() });
                                        },
                                        for opt in parent_options(&page_list.get(), current_id.get(), parent.get()) {
                                            option {
                                                key: opt.value.clone(),
                                                value: opt.value,
                                                selected: opt.selected,
                                                {opt.label}
                                            }
                                        }
                                    }
                                }
                            }
                            // Template picker: the theme's page templates (the server sends
                            // "Default" as the empty-value option, so no synthesizing here).
                            if matches!(kind.get(), EntityKind::Page) {
                                div { class: "metaitem",
                                    label { "Template" }
                                    select {
                                        class: "select",
                                        oninput: move |v: String| template.set(v),
                                        for opt in template_opts(&templates.get(), &template.get()) {
                                            option {
                                                key: opt.value.clone(),
                                                value: opt.value,
                                                selected: opt.selected,
                                                {opt.label}
                                            }
                                        }
                                    }
                                }
                            }
                            // Sibling order (WP `menu_order`). Held as a String for the
                            // number input; parsed to i32 on save.
                            if matches!(kind.get(), EntityKind::Page) {
                                div { class: "metaitem",
                                    label { "Order" }
                                    input {
                                        class: "input input--number",
                                        r#type: "number",
                                        step: "1",
                                        value: {move || menu_order.get()},
                                        oninput: move |v: String| menu_order.set(v),
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
                            button { class: "tool", title: "Insert image", onclick: move || insert_image_via_picker(editor, editor_session, notice, auth), "\u{25A6}" }
                            span { class: "toolbar__spacer" }
                            span { class: "toolbar__measure",
                                span { class: "regmark", style: "font-size:.75rem", "\u{2295}" }
                                "measure"
                            }
                        }
                        div { class: "sheet",
                            div { class: "sheet__inner",
                                // The title is the headline set on the sheet (per the
                                // mockup). Controlled like the slug field: the reactive
                                // `value` reflects an `open_post`/`new_post` load and stays
                                // caret-safe (rinch writes the property only on a real change).
                                // Typing here live-updates the masthead, which reads the
                                // same signal.
                                input {
                                    class: "sheet__title",
                                    value: {move || title.get()},
                                    placeholder: "Untitled",
                                    spellcheck: "false",
                                    oninput: move |v: String| title.set(v),
                                }
                                Editor { editor: editor.get(), content: "" }
                            }
                        }
                    }
                },

                // ── SETTINGS (THE FORMS DRAWER) — site settings OR plugin config ──
                View::Settings => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || {
                                    notice.set(String::new());
                                    // Back to wherever this form was opened from.
                                    match settings_target.get() {
                                        SettingsTarget::Site => view.set(View::List),
                                        SettingsTarget::Plugin { .. } => view.set(View::Plugins),
                                    }
                                },
                                {move || match settings_target.get() {
                                    SettingsTarget::Site => "\u{2190} Posts".to_owned(),
                                    SettingsTarget::Plugin { .. } => "\u{2190} Plugins".to_owned(),
                                }}
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here",
                                {move || match settings_target.get() {
                                    SettingsTarget::Site => "Settings".to_owned(),
                                    SettingsTarget::Plugin { name, .. } => name,
                                }}
                            }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary",
                                style: "width:auto",
                                onclick: move || save_settings(
                                    settings_values, settings_refs, settings_saving, notice, toast,
                                    settings_target.get(), auth,
                                ),
                                {move || if settings_saving.get() { "Saving\u{2026}" } else { "Save changes" }}
                            }
                        }
                    }
                    div { class: "wrap",
                        div { class: "galley__head",
                            h2 { class: "galley__title",
                                {move || match settings_target.get() {
                                    SettingsTarget::Site => "Settings".to_owned(),
                                    SettingsTarget::Plugin { name, .. } => name,
                                }}
                            }
                            span { class: "galley__count",
                                {move || match settings_target.get() {
                                    SettingsTarget::Site => "site configuration".to_owned(),
                                    SettingsTarget::Plugin { .. } => "plugin configuration".to_owned(),
                                }}
                            }
                        }
                        if !notice.get().is_empty() {
                            div { class: "galley__state err", {move || notice.get()} }
                        }
                        if matches!(settings_load.get(), Load::Loading) {
                            div { class: "galley__state", "Opening the forms drawer\u{2026}" }
                        }
                        if matches!(settings_load.get(), Load::Error) {
                            div { class: "galley__state err", "Settings are unavailable right now." }
                        }
                        // Mount the schema-driven form once loaded. A bare reactive
                        // `match` (Rule 14) so it builds when the async GET resolves; the
                        // same `FormValues` handle is read back by Save.
                        match (settings_load.get(), settings_schema.get(), settings_values.get()) {
                            (Load::Ready, Some(schema), Some(values)) => SchemaForm {
                                schema: schema,
                                values: values,
                                refs: settings_refs.get(),
                                media_picker: on_pick_media.get(),
                            },
                            _ => span {},
                        }
                    }
                },

                // ── PLUGINS (THE CABINET) — pick a plugin to configure ─────────
                View::Plugins => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || {
                                    notice.set(String::new());
                                    view.set(View::List);
                                },
                                "\u{2190} Posts"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here", "Plugins" }
                            span { class: "masthead__spacer" }
                        }
                    }
                    div { class: "wrap",
                        div { class: "galley__head",
                            h2 { class: "galley__title", "Plugins" }
                            span { class: "galley__count",
                                {move || match plugins_state.get() {
                                    Load::Ready => format!("{} installed", plugins_list.get().len()),
                                    _ => String::new(),
                                }}
                            }
                        }
                        if !notice.get().is_empty() {
                            div { class: "galley__state err", {move || notice.get()} }
                        }
                        if matches!(plugins_state.get(), Load::Loading) {
                            div { class: "galley__state", "Opening the cabinet\u{2026}" }
                        }
                        if matches!(plugins_state.get(), Load::Error) {
                            div { class: "galley__state err", "Plugins are unavailable right now." }
                        }
                        for row in plugin_row_vms(&plugins_list.get()) {
                            button {
                                key: row.id.clone(),
                                class: "row",
                                onclick: {
                                    let id = row.id.clone();
                                    let name = row.name.clone();
                                    let configurable = row.has_settings;
                                    move || {
                                        if configurable {
                                            open_plugin_config(
                                                id.clone(), name.clone(), settings_target,
                                                settings_load, settings_schema, settings_values,
                                                settings_refs, notice, view, auth,
                                            );
                                        } else {
                                            notice.set(
                                                "This plugin has no configurable settings.".to_owned(),
                                            );
                                        }
                                    }
                                },
                                span { class: "row__mark", "\u{2295}" }
                                span {
                                    span { class: "row__title", {row.name} }
                                    span { class: "row__slug", {row.id} }
                                }
                                span { class: "row__edit", {row.hint} }
                            }
                        }
                        if matches!(plugins_state.get(), Load::Ready) && plugins_list.get().is_empty() {
                            div { class: "galley__state", "No plugins installed." }
                        }
                    }
                },

                // ── MENUS (THE CABINET) — the menus list + location assignment ──
                View::Menus => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || { menu.notice.set(String::new()); menu.view.set(View::List); },
                                "\u{2190} Posts"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here", "Menus" }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary", style: "width:auto",
                                onclick: move || new_menu(menu),
                                "\u{002B} New menu"
                            }
                        }
                    }
                    div { class: "wrap",
                        // Location assignment (each theme location → a bound menu).
                        div { class: "panel",
                            h2 { class: "panel__title", "Locations" }
                            for row in menu.locations.get() {
                                MenuLocationView { key: row.location.clone(), row: row, menu: menu }
                            }
                            if matches!(menu.list_state.get(), Load::Ready) && menu.locations.get().is_empty() {
                                p { class: "panel__note", "This theme declares no menu locations." }
                            }
                            p { class: "panel__note", "A location shows one menu; a menu can fill many locations. Clearing a location falls back to the theme default." }
                        }
                        div { class: "galley__head",
                            h2 { class: "galley__title", "Menus" }
                            span { class: "galley__count",
                                {move || match menu.list_state.get() {
                                    Load::Ready => format!("{} menus", menu.list.get().len()),
                                    _ => String::new(),
                                }}
                            }
                        }
                        if !menu.notice.get().is_empty() {
                            div { class: "galley__state err", {move || menu.notice.get()} }
                        }
                        if matches!(menu.list_state.get(), Load::Loading) {
                            div { class: "galley__state", "Opening the cabinet\u{2026}" }
                        }
                        if matches!(menu.list_state.get(), Load::Error) {
                            div { class: "galley__state err", "Menus are unavailable right now." }
                        }
                        for m in menu.list.get() {
                            button {
                                key: m.id,
                                class: "row", style: "grid-template-columns: 24px 1fr auto auto",
                                onclick: { let id = m.id; move || open_menu_editor(id, menu) },
                                span { class: "row__mark", "\u{2295}" }
                                span {
                                    span { class: "row__title", {m.name.clone()} }
                                    span { class: "row__slug", {m.slug.clone()} }
                                }
                                span { class: "row__time", {format!("{} item{}", m.item_count, if m.item_count == 1 { "" } else { "s" })} }
                                span { class: "row__edit", "Edit \u{2192}" }
                            }
                        }
                        if matches!(menu.list_state.get(), Load::Ready) && menu.list.get().is_empty() {
                            div { class: "galley__state", "No menus yet. Create one to get started." }
                        }
                    }
                },

                // ── MENU EDITOR (THE ROUTING TABLE) — one menu's item tree ──────
                View::MenuEditor => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || leave_menu_editor(menu),
                                "\u{2190} Menus"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here",
                                {move || { let n = menu.name.get(); if n.is_empty() { "Menu".to_owned() } else { n } }}
                            }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary", style: "width:auto",
                                onclick: move || save_menu(menu),
                                {move || if menu.saving.get() { "Saving\u{2026}" } else { "Save menu" }}
                            }
                        }
                    }
                    div { class: "wrap",
                        div { class: "editor__meta",
                            div { class: "metaitem",
                                label { "Name" }
                                input {
                                    class: "input", style: "width:16rem",
                                    value: {move || menu.name.get()},
                                    oninput: move |v: String| { if menu.saving.get() { return; } menu.name.set(v); menu.dirty.set(true); },
                                }
                            }
                            div { class: "metaitem",
                                label { "Slug" }
                                input {
                                    class: "input input--mono", style: "width:12rem",
                                    value: {move || menu.slug.get()},
                                    spellcheck: "false",
                                    oninput: move |v: String| { if menu.saving.get() { return; } menu.slug.set(v); menu.dirty.set(true); },
                                }
                            }
                            span { class: "masthead__spacer", style: "flex:1" }
                            button {
                                class: "btn btn--danger",
                                onclick: move || delete_menu_clicked(menu),
                                "Delete menu"
                            }
                        }
                        if !menu.notice.get().is_empty() {
                            div { class: "editor__error", {move || menu.notice.get()} }
                        }
                        if matches!(menu.load.get(), Load::Loading) {
                            div { class: "galley__state", "Loading the menu\u{2026}" }
                        }
                        if matches!(menu.load.get(), Load::Error) {
                            div { class: "galley__state err", "This menu couldn't be loaded \u{2014} go back and try again." }
                        }
                        // The item tree — only when the menu loaded cleanly, so a Save can
                        // never PUT a forest derived from an errored/empty load (must-fix M1).
                        if matches!(menu.load.get(), Load::Ready) {
                            div { class: "menubar",
                                span { class: "galley__count",
                                    {move || { let n = menu.tree.get().len(); if n == 0 { String::new() } else { format!("{} item{}", n, if n == 1 { "" } else { "s" }) } }}
                                }
                                button {
                                    class: "btn btn--ghost", style: "width:auto",
                                    onclick: move || open_picker(menu),
                                    "\u{002B} Add item"
                                }
                            }
                            ul { class: "tree",
                                for row in menu.tree.get() {
                                    MenuRowView {
                                        key: row.cid.clone(),
                                        cid: row.cid.clone(),
                                        kind: row.kind.clone(),
                                        resolved: row.resolved.clone(),
                                        edits: menu.edits,
                                        tree: menu.tree,
                                        dirty: menu.dirty,
                                        saving: menu.saving,
                                    }
                                }
                            }
                            if menu.tree.get().is_empty() {
                                div { class: "emptytree", "This menu is empty. Add a page, post, or custom link." }
                            }
                            div { class: "savebar",
                                if menu.dirty.get() {
                                    span { class: "dirtydot", "\u{25CF} unsaved changes" }
                                }
                            }
                        }
                    }
                },
            }

            // The link-candidate picker (reuses the media-modal chrome). Mounted only while
            // open AND on the MenuEditor view, so a session-expiry / navigation dismisses it.
            if menu.picker_open.get() && matches!(view.get(), View::MenuEditor) {
                div { class: "media-modal",
                    div { class: "media-modal__plate", role: "dialog", aria-modal: "true", aria-label: "Add a menu item",
                        div { class: "media-modal__head",
                            h3 { class: "media-modal__title", "Add a menu item" }
                            button { class: "btn btn--quiet", onclick: move || close_picker(menu), "\u{2715}" }
                        }
                        div { class: "media-modal__body",
                            div { class: "tabs",
                                button { class: {move || tab_class(menu.picker_tab.get(), 0)}, onclick: move || menu.picker_tab.set(0), "Pages" }
                                button { class: {move || tab_class(menu.picker_tab.get(), 1)}, onclick: move || menu.picker_tab.set(1), "Posts" }
                                button { class: {move || tab_class(menu.picker_tab.get(), 2)}, onclick: move || menu.picker_tab.set(2), "Custom link" }
                            }
                            // Search box (Pages/Posts tabs).
                            if menu.picker_tab.get() != 2 {
                                div { class: "pickerfield",
                                    input {
                                        class: "input", placeholder: "Search\u{2026}",
                                        value: {move || menu.picker_query.get()},
                                        oninput: move |v: String| { menu.picker_query.set(v); reload_candidates(menu); },
                                    }
                                }
                            }
                            if matches!(menu.candidates_state.get(), Load::Loading) && menu.picker_tab.get() != 2 {
                                div { class: "media-modal__state", "Loading targets\u{2026}" }
                            }
                            match menu.picker_tab.get() {
                                0 => div {
                                    for c in menu.candidates.get().pages {
                                        button {
                                            key: c.id, r#type: "button", class: "candidate",
                                            onclick: { let cand = c.clone(); move || add_candidate_item(menu, cand.clone()) },
                                            span { class: "candidate__title", {if c.title.is_empty() { c.href.clone() } else { c.title.clone() }} }
                                            span { class: "candidate__href", {c.href.clone()} }
                                        }
                                    }
                                    if matches!(menu.candidates_state.get(), Load::Ready) && menu.candidates.get().pages.is_empty() {
                                        div { class: "truncnote", "No matching pages." }
                                    }
                                },
                                1 => div {
                                    for c in menu.candidates.get().posts {
                                        button {
                                            key: c.id, r#type: "button", class: "candidate",
                                            onclick: { let cand = c.clone(); move || add_candidate_item(menu, cand.clone()) },
                                            span { class: "candidate__title", {if c.title.is_empty() { c.href.clone() } else { c.title.clone() }} }
                                            span { class: "candidate__href", {c.href.clone()} }
                                        }
                                    }
                                    if menu.candidates.get().posts_truncated {
                                        div { class: "truncnote", "More posts exist \u{2014} refine your search." }
                                    }
                                    if matches!(menu.candidates_state.get(), Load::Ready) && menu.candidates.get().posts.is_empty() {
                                        div { class: "truncnote", "No matching posts." }
                                    }
                                },
                                _ => div {
                                    div { class: "pickerfield",
                                        label { "URL" }
                                        input {
                                            class: "input input--mono", placeholder: "/free-reads or https://\u{2026}",
                                            value: {move || menu.custom_url.get()},
                                            oninput: move |v: String| menu.custom_url.set(v),
                                        }
                                    }
                                    div { class: "pickerfield",
                                        label { "Label" }
                                        input {
                                            class: "input", placeholder: "Free Reads",
                                            value: {move || menu.custom_label.get()},
                                            oninput: move |v: String| menu.custom_label.set(v),
                                        }
                                    }
                                    if !menu.picker_err.get().is_empty() {
                                        p { class: "note", {move || menu.picker_err.get()} }
                                    }
                                    button { class: "btn btn--primary", style: "width:auto", onclick: move || add_custom_item(menu), "Add custom link" }
                                },
                            }
                        }
                        div { class: "media-modal__foot",
                            button { class: "btn btn--quiet", style: "width:auto", onclick: move || close_picker(menu), "Done" }
                        }
                    }
                }
            }

            // The media-library picker modal (a MediaPicker "Choose" button opens it).
            // A fixed overlay, mounted only while open AND on the Settings view (so a
            // session-expiry or any navigation that flips the view also dismisses it —
            // it can only ever be opened from the settings form). Inlined here (not a
            // component) so its handlers capture the Copy `auth` bundle directly.
            // Selecting an image invokes the field's sink + closes.
            if media_pick_open.get() && matches!(view.get(), View::Settings) {
                div { class: "media-modal",
                    div { class: "media-modal__plate",
                        div { class: "media-modal__head",
                            h3 { class: "media-modal__title", "Media library" }
                            span { class: "media-modal__sub", "choose an image" }
                        }
                        div { class: "media-modal__body",
                            if matches!(media_library_state.get(), Load::Loading) {
                                div { class: "media-modal__state", "Opening the drawer\u{2026}" }
                            }
                            if matches!(media_library_state.get(), Load::Error) {
                                div { class: "media-modal__state", "The media library is unavailable right now." }
                            }
                            if matches!(media_library_state.get(), Load::Ready) {
                                div { class: "media-grid",
                                    button {
                                        class: "media-cell media-cell--upload",
                                        onclick: move || upload_via_picker(media_pick_sink, media_pick_open, notice, auth),
                                        span { class: "plus", "\u{FF0B}" }
                                        "Upload new"
                                    }
                                    for item in media_library.get() {
                                        MediaCell {
                                            key: item.id,
                                            item: item,
                                            sink: media_pick_sink,
                                            open: media_pick_open,
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "media-modal__foot",
                            button {
                                class: "btn btn--quiet", style: "width:auto",
                                onclick: move || media_pick_open.set(false),
                                "Cancel"
                            }
                        }
                    }
                }
            }

            div { class: {move || if toast.get() { "toast is-shown" } else { "toast" }},
                span { class: "regmark", style: "font-size:.85rem", "\u{2295}" }
                "Saved"
            }
        }
    }
}

/// One thumbnail in the media-library grid. Its own component so its click handler can
/// own the row's `id`/`url` (a reactive `for` body can't move a non-`Copy` value into
/// a closure). Selecting it invokes the picker `sink` and closes the modal.
#[component]
fn MediaCell(
    item: api::MediaSummary,
    sink: Signal<Option<MediaChosen>>,
    open: Signal<bool>,
) -> NodeHandle {
    let id = item.id;
    let url = item.url.clone();
    let thumb = item.url.clone();
    let name = if item.filename.is_empty() {
        format!("media #{id}")
    } else {
        item.filename.clone()
    };
    let title = if item.alt.is_empty() {
        name.clone()
    } else {
        item.alt.clone()
    };
    rsx! {
        button {
            class: "media-cell",
            title: title,
            onclick: move || {
                if let Some(chosen) = sink.get() {
                    chosen(id, url.clone());
                }
                open.set(false);
            },
            img { class: "media-cell__thumb", src: thumb, alt: "" }
            span { class: "media-cell__name", {name} }
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
    /// Featured-image thumbnail URL, if the post has one.
    featured_url: Option<String>,
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
            featured_url: p.featured_media.as_ref().map(|f| f.url.clone()),
        })
        .collect()
}

/// A page-galley row's presentation fields (mirrors [`RowVm`], plus the tree
/// `indent`). `path_disp` is the full nested permalink; `indent` is an inline
/// `padding-left` derived from the page's depth so nesting reads at a glance.
#[derive(Clone, PartialEq)]
struct PageRowVm {
    id: u64,
    title: String,
    path_disp: String,
    stamp_class: &'static str,
    stamp_label: String,
    time: String,
    featured_url: Option<String>,
    indent: String,
}

fn page_row_vms(pages: &[api::PageSummary]) -> Vec<PageRowVm> {
    pages
        .iter()
        .map(|p| PageRowVm {
            id: p.id,
            title: p.title.clone(),
            path_disp: format!("/{}", p.path),
            stamp_class: api::status_stamp_class(&p.status),
            stamp_label: api::status_label(&p.status),
            time: api::fmt_relative(p.updated_at),
            featured_url: p.featured_media.as_ref().map(|f| f.url.clone()),
            indent: format!("padding-left: {:.2}rem", 0.25 + p.depth as f32 * 1.25),
        })
        .collect()
}

/// A plugin-cabinet row's presentation fields, precomputed so the reactive `for`
/// body reads each once. `hint` is the trailing affordance (configurable vs not).
#[derive(Clone, PartialEq)]
struct PluginRowVm {
    id: String,
    name: String,
    has_settings: bool,
    hint: &'static str,
}

fn plugin_row_vms(list: &[api::PluginDescriptor]) -> Vec<PluginRowVm> {
    list.iter()
        .map(|p| PluginRowVm {
            id: p.id.clone(),
            name: p.name.clone(),
            has_settings: p.has_settings,
            hint: if p.has_settings {
                "Configure \u{2192}"
            } else {
                "No settings"
            },
        })
        .collect()
}

/// The galley-row leading cell: the post's featured-image thumbnail when `url` is
/// non-empty, else the registration crosshair. A component so the choice is a
/// one-shot `if` (a `#[component]` body runs once) instead of a reactive rsx `if`,
/// which would try to move the non-`Copy` url out of the captured row.
#[component]
fn RowLead(url: String) -> NodeHandle {
    if url.is_empty() {
        rsx! { span { class: "row__mark", "\u{2295}" } }
    } else {
        rsx! { img { class: "row__thumb", src: url, alt: "" } }
    }
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

/// Fetch the page list into `pages`, tracking `state`. A 401 routes back to login (an
/// expired session), NOT a false outage. Mirrors [`load_posts`].
fn load_pages(pages: Signal<Vec<api::PageSummary>>, state: Signal<Load>, auth: AuthCtx) {
    state.set(Load::Loading);
    spawn_local(async move {
        match api::list_pages().await {
            Ok(list) => {
                pages.set(list);
                state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => state.set(Load::Error),
        }
    });
}

/// Fetch the theme's page templates into `templates` for the editor's Template picker.
/// Best-effort and idempotent: skips the fetch when already loaded (theme metadata is
/// ~static), routes a 401 to login, and on any other failure leaves the list empty (the
/// picker offers only what loaded; a save then defaults the template) rather than
/// blocking the page list.
fn load_templates(templates: Signal<Vec<api::TemplateOption>>, auth: AuthCtx) {
    if !templates.get().is_empty() {
        return;
    }
    spawn_local(async move {
        match api::list_templates().await {
            Ok(list) => templates.set(list),
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => {}
        }
    });
}

/// Whether the signed-in user is an Administrator — the only role that manages
/// settings. Decides whether to show the Settings nav; the server enforces
/// `ManageSettings` on the endpoints regardless.
fn is_admin(user: &Option<UserDto>) -> bool {
    user.as_ref()
        .map(|u| u.role == "administrator")
        .unwrap_or(false)
}

/// Switch to the Plugins view and (re)load the installed-plugin list.
fn open_plugins(
    list: Signal<Vec<api::PluginDescriptor>>,
    state: Signal<Load>,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    view.set(View::Plugins);
    load_plugins(list, state, auth);
}

/// Fetch the installed-plugin list into `list`, tracking `state`. A 401 routes back
/// to login; any other failure shows the error state.
fn load_plugins(list: Signal<Vec<api::PluginDescriptor>>, state: Signal<Load>, auth: AuthCtx) {
    state.set(Load::Loading);
    spawn_local(async move {
        match api::list_plugins().await {
            Ok(v) => {
                list.set(v);
                state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => state.set(Load::Error),
        }
    });
}

/// Switch the List view to the POSTS galley and (re)load it.
fn open_posts_list(
    kind: Signal<EntityKind>,
    posts: Signal<Vec<PostSummary>>,
    state: Signal<Load>,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    kind.set(EntityKind::Post);
    view.set(View::List);
    load_posts(posts, state, auth);
}

/// Switch the List view to the PAGES galley and (re)load it, plus the theme's page
/// templates (so the editor's Template picker is ready when a page is opened).
#[allow(clippy::too_many_arguments)]
fn open_pages_list(
    kind: Signal<EntityKind>,
    pages: Signal<Vec<api::PageSummary>>,
    state: Signal<Load>,
    templates: Signal<Vec<api::TemplateOption>>,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    kind.set(EntityKind::Page);
    view.set(View::List);
    load_pages(pages, state, auth);
    load_templates(templates, auth);
}

/// Switch to the Settings view for the SITE settings and (re)load its schema + values.
#[allow(clippy::too_many_arguments)]
fn open_settings(
    load: Signal<Load>,
    schema: Signal<Option<FormSchema>>,
    values: Signal<Option<FormValues>>,
    refs: Signal<SettingRefs>,
    target: Signal<SettingsTarget>,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    target.set(SettingsTarget::Site);
    view.set(View::Settings);
    load_settings(load, schema, values, refs, SettingsTarget::Site, auth);
}

/// Switch to the Settings view for a specific PLUGIN's config and (re)load it. Reuses
/// the exact same form pipeline as the site settings — only the endpoint differs.
#[allow(clippy::too_many_arguments)]
fn open_plugin_config(
    id: String,
    name: String,
    target: Signal<SettingsTarget>,
    load: Signal<Load>,
    schema: Signal<Option<FormSchema>>,
    values: Signal<Option<FormValues>>,
    refs: Signal<SettingRefs>,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    let t = SettingsTarget::Plugin { id, name };
    target.set(t.clone());
    view.set(View::Settings);
    load_settings(load, schema, values, refs, t, auth);
}

/// Fetch the schema + current values for `target` (site or a plugin). On success
/// stores the schema + a fresh `FormValues` handle (the live map the form writes and
/// Save reads). A 401 routes back to login; any other failure shows the error state.
#[allow(clippy::too_many_arguments)]
fn load_settings(
    load: Signal<Load>,
    schema: Signal<Option<FormSchema>>,
    values: Signal<Option<FormValues>>,
    refs: Signal<SettingRefs>,
    target: SettingsTarget,
    auth: AuthCtx,
) {
    load.set(Load::Loading);
    spawn_local(async move {
        let result = match &target {
            SettingsTarget::Site => api::get_settings().await,
            SettingsTarget::Plugin { id, .. } => api::get_plugin_settings(id).await,
        };
        match result {
            Ok(dto) => {
                // Set refs + schema BEFORE flipping to Ready so the form mounts with
                // its picker options in place (the mount reads all three).
                refs.set(dto.refs);
                schema.set(Some(dto.schema));
                values.set(Some(FormValues::new(dto.values)));
                load.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => load.set(Load::Error),
        }
    });
}

/// Persist the settings form to `target`: snapshot the live `FormValues`, PUT it to
/// the site or plugin endpoint, and on success stamp the save toast and re-seed the
/// form from the server's (validated + normalized, e.g. clamped) values. A 401 routes
/// back to login; a validation error (400) surfaces its message.
#[allow(clippy::too_many_arguments)]
fn save_settings(
    values: Signal<Option<FormValues>>,
    refs: Signal<SettingRefs>,
    saving: Signal<bool>,
    notice: Signal<String>,
    toast: Signal<bool>,
    target: SettingsTarget,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }
    let Some(form) = values.get() else {
        return;
    };
    let snapshot = form.snapshot();
    notice.set(String::new());
    saving.set(true);
    spawn_local(async move {
        let result = match &target {
            SettingsTarget::Site => api::put_settings(snapshot).await,
            SettingsTarget::Plugin { id, .. } => api::put_plugin_settings(id, snapshot).await,
        };
        saving.set(false);
        match result {
            Ok(dto) => {
                // Re-seed from the server's normalized values so the form shows the
                // authoritative result (a clamped number, a dropped unknown key). Refs
                // are re-seeded too (a newly-referenced media resolves its thumbnail);
                // set refs before values so the re-mount has them.
                refs.set(dto.refs);
                values.set(Some(FormValues::new(dto.values)));
                toast.set(true);
                TimeoutFuture::new(1600).await;
                toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(e)) => notice.set(e),
        }
    });
}

/// Fetch the media library into `library` for the picker grid. A 401 routes back to
/// login; any other failure shows the modal's error state.
fn load_media(library: Signal<Vec<api::MediaSummary>>, state: Signal<Load>, auth: AuthCtx) {
    state.set(Load::Loading);
    spawn_local(async move {
        match api::list_media().await {
            Ok(items) => {
                library.set(items);
                state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => state.set(Load::Error),
        }
    });
}

/// The media picker's "Upload new" tile: open the OS image picker, upload the file,
/// and on success hand the new media to the picker `sink` and close the modal.
///
/// Like the editor's featured-image picker, the `<input type=file>` is created and
/// `.click()`ed SYNCHRONOUSLY inside this trusted click handler (a file dialog must be
/// opened within a user gesture); the upload runs async. The `sink` is captured up
/// front, so even if the modal is later reused for another field, this upload's result
/// lands on the field it was started for.
fn upload_via_picker(
    sink: Signal<Option<MediaChosen>>,
    open: Signal<bool>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;

    // Capture the active field's sink NOW, before any async work.
    let Some(chosen) = sink.get() else {
        return;
    };
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

    let picker = input.clone();
    let on_change = Closure::<dyn FnMut()>::new(move || {
        let Some(file) = picker.files().and_then(|files| files.get(0)) else {
            return;
        };
        // A file was chosen — the picker's job is done, so close the modal NOW (not
        // after the async upload). This keeps a slow upload from later tearing down a
        // modal the user has since reopened for another field; the captured `chosen`
        // still lands the result on the right field when the upload resolves.
        open.set(false);
        // Clone the sink into the async task (the outer closure is `FnMut`).
        let chosen = chosen.clone();
        spawn_local(async move {
            let Ok(form) = web_sys::FormData::new() else {
                notice.set("Your browser blocked the upload form.".to_owned());
                return;
            };
            let _ = form.append_with_blob_and_filename("file", &file, &file.name());
            let _ = form.append_with_str("alt", "");
            match api::upload_media(form).await {
                Ok(resp) => chosen(resp.id, resp.url),
                Err(api::ApiError::Unauthorized) => auth.session_expired(),
                Err(api::ApiError::Message(e)) => notice.set(format!("Upload failed: {e}")),
            }
        });
    });
    input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    on_change.forget();
    input.click();
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
    featured: Signal<Option<api::FeaturedMedia>>,
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
                featured.set(detail.featured_media);
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

/// Open a page in the editor: fetch it, populate the meta fields (including the
/// hierarchy controls), bridge its `BlockTree` into the editor, set the entity kind to
/// Page, then switch to the editor. Mirrors [`open_post`]; a bridge failure loads an
/// EMPTY document + a warning rather than corrupting content.
#[allow(clippy::too_many_arguments)]
fn open_page(
    id: u64,
    editor: Signal<EditorHandle>,
    kind: Signal<EntityKind>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    parent: Signal<Option<u64>>,
    menu_order: Signal<String>,
    template: Signal<String>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    spawn_local(async move {
        match api::get_page(id).await {
            Ok(detail) => {
                // A new document takes over the shared editor: bump the session so an
                // in-flight save can no longer write back its id here.
                editor_session.update(|g| *g += 1);
                title.set(detail.title);
                slug.set(detail.slug);
                status.set(detail.status);
                featured.set(detail.featured_media);
                parent.set(detail.parent);
                menu_order.set(detail.menu_order.to_string());
                template.set(detail.template.unwrap_or_default());
                current_id.set(Some(detail.id));
                kind.set(EntityKind::Page);

                let schema = Schema::starter_kit();
                let handle = editor.get();
                match bridge::block_tree_json_to_node(&schema, &detail.block_tree) {
                    Ok(node) => handle.load_doc(node),
                    Err(e) => {
                        if let Ok(node) = bridge::block_tree_json_to_node(&schema, &empty_tree()) {
                            handle.load_doc(node);
                        }
                        notice.set(format!(
                            "This page uses content the editor couldn't load ({e}). Saving will overwrite it."
                        ));
                    }
                }
                auth.view.set(View::Editor);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(e)) => notice.set(format!("Couldn't open that page: {e}")),
        }
    });
}

/// Read the editor document back, bridge it to `BlockTree` JSON, and validate the
/// slug — everything a Save or Preview needs from the sheet before hitting the
/// network. Returns `(block_tree, slug)` or a ready-to-show error message. Shared by
/// [`save_post`] and [`preview_post`] so the two stay in lockstep.
fn prepared_doc(
    editor: Signal<EditorHandle>,
    slug: Signal<String>,
) -> Result<(serde_json::Value, String), String> {
    let node = editor.get().doc();
    let block_tree = bridge::node_to_block_tree_json(&node)
        .map_err(|e| format!("Couldn't prepare the document to save: {e}"))?;
    let slug_val = slug.get().trim().to_owned();
    if slug_val.is_empty() {
        return Err("The slug can't be empty.".to_owned());
    }
    Ok((block_tree, slug_val))
}

/// Persist the prepared document: CREATE (`POST`) when there is no id yet, else
/// UPDATE (`PUT`) in place. Returns the effective post id (freshly assigned on
/// create). The one create-vs-update decision, shared by [`save_post`] and
/// [`preview_post`]; the stale-session guard + `current_id` writeback stay with the
/// callers, which differ in what they do on success (toast vs. steer the tab).
async fn persist_post(
    existing: Option<u64>,
    title: String,
    slug: String,
    status: String,
    block_tree: serde_json::Value,
    featured: Option<u64>,
) -> Result<u64, api::ApiError> {
    match existing {
        Some(id) => api::save_post(
            id,
            &api::SaveRequest {
                title,
                slug,
                status,
                block_tree,
                featured_media: featured,
            },
        )
        .await
        .map(|()| id),
        None => {
            api::create_post(&api::CreateRequest {
                title,
                slug,
                status,
                block_tree,
                featured_media: featured,
            })
            .await
        }
    }
}

/// CREATE (`POST`) or UPDATE (`PUT`) a page, mirroring [`persist_post`] with the
/// hierarchy fields. Returns the effective page id (freshly assigned on create).
#[allow(clippy::too_many_arguments)]
async fn persist_page(
    existing: Option<u64>,
    title: String,
    slug: String,
    status: String,
    block_tree: serde_json::Value,
    featured: Option<u64>,
    parent: Option<u64>,
    menu_order: i32,
    template: Option<String>,
) -> Result<u64, api::ApiError> {
    match existing {
        Some(id) => api::save_page(
            id,
            &api::SavePageRequest {
                title,
                slug,
                status,
                block_tree,
                featured_media: featured,
                parent,
                menu_order,
                template,
            },
        )
        .await
        .map(|()| id),
        None => {
            api::create_page(&api::CreatePageRequest {
                title,
                slug,
                status,
                block_tree,
                featured_media: featured,
                parent,
                menu_order,
                template,
            })
            .await
        }
    }
}

/// Persist the current editor document. A post with no id yet (a fresh "New post") is
/// CREATED and its assigned id captured into `current_id` so the next save UPDATES it
/// in place. Surfaces the server's 400/409 message; stamps a "Saved" toast on success.
#[allow(clippy::too_many_arguments)]
fn save_post(
    editor: Signal<EditorHandle>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    saving: Signal<bool>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }

    let (block_tree, slug_val) = match prepared_doc(editor, slug) {
        Ok(v) => v,
        Err(msg) => {
            notice.set(msg);
            return;
        }
    };

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    let featured_val = featured.get().map(|f| f.id);
    let existing = current_id.get();
    // The document this save belongs to. If the editor loads a different document
    // before the request returns, the result is stale and must NOT write back its
    // id / toast / error into what is now a different editing context.
    let save_gen = editor_session.get();
    spawn_local(async move {
        let result = persist_post(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
        )
        .await;
        // The editor moved to another document while we were in flight: the save
        // still landed server-side, but adopting its id here would hijack the new
        // document. Free the save lock and drop the writeback. (A 401 is auth-wide,
        // so it still routes to login regardless.)
        let stale = editor_session.get() != save_gen;
        saving.set(false);
        match result {
            Ok(id) => {
                if stale {
                    return;
                }
                // A create hands back a new id; record it so the next save updates.
                if existing.is_none() {
                    current_id.set(Some(id));
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

/// Persist the current page (create-or-update). Mirrors [`save_post`] with the
/// hierarchy fields read from the Page-only signals: `menu_order` is parsed from its
/// string input and an empty `template` maps to the default (None). Surfaces the
/// server's 400/409 message (bad slug/parent/template, cycle, path clash); stamps the
/// "Saved" toast; the stale-generation guard matches [`save_post`].
#[allow(clippy::too_many_arguments)]
fn save_page(
    editor: Signal<EditorHandle>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    parent: Signal<Option<u64>>,
    menu_order: Signal<String>,
    template: Signal<String>,
    saving: Signal<bool>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }

    let (block_tree, slug_val) = match prepared_doc(editor, slug) {
        Ok(v) => v,
        Err(msg) => {
            notice.set(msg);
            return;
        }
    };

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    let featured_val = featured.get().map(|f| f.id);
    let parent_val = parent.get();
    let menu_order_val = parse_menu_order(&menu_order.get());
    let template_val = template_arg(&template.get());
    let existing = current_id.get();
    let save_gen = editor_session.get();
    spawn_local(async move {
        let result = persist_page(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
            parent_val,
            menu_order_val,
            template_val,
        )
        .await;
        let stale = editor_session.get() != save_gen;
        saving.set(false);
        match result {
            Ok(id) => {
                if stale {
                    return;
                }
                if existing.is_none() {
                    current_id.set(Some(id));
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

/// Preview the current draft in the real public theme (WordPress-style, new tab).
///
/// The preview route renders the STORED object, so the current sheet is persisted
/// first (exactly like [`save_post`]) — a preview must reflect what the author sees,
/// never a stale copy. To dodge the pop-up blocker (an `open()` from an async
/// continuation is not treated as user-initiated), the tab is opened **synchronously**
/// in this click gesture and left blank, then steered to `/admin/preview/{id}` once
/// the save lands. On a stale save (the editor moved on) or an error, the blank tab is
/// closed and the editor shows the message.
#[allow(clippy::too_many_arguments)]
fn preview_post(
    editor: Signal<EditorHandle>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    saving: Signal<bool>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }

    let (block_tree, slug_val) = match prepared_doc(editor, slug) {
        Ok(v) => v,
        Err(msg) => {
            notice.set(msg);
            return;
        }
    };

    // Open the tab NOW, in the trusted click, so it isn't blocked as a non-user
    // pop-up. It starts blank; we point it at the rendered draft after the save below.
    let Some(win) =
        web_sys::window().and_then(|w| w.open_with_url_and_target("", "_blank").ok().flatten())
    else {
        notice.set("Couldn't open a preview tab — allow pop-ups for this site.".to_owned());
        return;
    };

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    let featured_val = featured.get().map(|f| f.id);
    let existing = current_id.get();
    let save_gen = editor_session.get();
    spawn_local(async move {
        let result = persist_post(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
        )
        .await;
        let stale = editor_session.get() != save_gen;
        saving.set(false);
        match result {
            Ok(id) => {
                if stale {
                    // The editor switched documents mid-flight: the save landed, but
                    // steering the tab into what is now a different post would mislead.
                    let _ = win.close();
                    return;
                }
                if existing.is_none() {
                    current_id.set(Some(id));
                }
                if win
                    .location()
                    .set_href(&format!("/admin/preview/{id}"))
                    .is_err()
                {
                    notice.set("Saved, but couldn't open the preview.".to_owned());
                }
            }
            Err(api::ApiError::Unauthorized) => {
                let _ = win.close();
                auth.session_expired();
            }
            Err(api::ApiError::Message(e)) => {
                let _ = win.close();
                if !stale {
                    notice.set(e);
                }
            }
        }
    });
}

/// Preview the current page draft in the real public theme (new tab). Mirrors
/// [`preview_post`] — persist first (via [`persist_page`]), open the tab synchronously
/// in the click gesture to dodge the pop-up blocker — but steers the tab to the page
/// preview route `/admin/preview/page/{id}`.
#[allow(clippy::too_many_arguments)]
fn preview_page(
    editor: Signal<EditorHandle>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    parent: Signal<Option<u64>>,
    menu_order: Signal<String>,
    template: Signal<String>,
    saving: Signal<bool>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    if saving.get() {
        return;
    }

    let (block_tree, slug_val) = match prepared_doc(editor, slug) {
        Ok(v) => v,
        Err(msg) => {
            notice.set(msg);
            return;
        }
    };

    let Some(win) =
        web_sys::window().and_then(|w| w.open_with_url_and_target("", "_blank").ok().flatten())
    else {
        notice.set("Couldn't open a preview tab — allow pop-ups for this site.".to_owned());
        return;
    };

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    let featured_val = featured.get().map(|f| f.id);
    let parent_val = parent.get();
    let menu_order_val = parse_menu_order(&menu_order.get());
    let template_val = template_arg(&template.get());
    let existing = current_id.get();
    let save_gen = editor_session.get();
    spawn_local(async move {
        let result = persist_page(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
            parent_val,
            menu_order_val,
            template_val,
        )
        .await;
        let stale = editor_session.get() != save_gen;
        saving.set(false);
        match result {
            Ok(id) => {
                if stale {
                    let _ = win.close();
                    return;
                }
                if existing.is_none() {
                    current_id.set(Some(id));
                }
                if win
                    .location()
                    .set_href(&format!("/admin/preview/page/{id}"))
                    .is_err()
                {
                    notice.set("Saved, but couldn't open the preview.".to_owned());
                }
            }
            Err(api::ApiError::Unauthorized) => {
                let _ = win.close();
                auth.session_expired();
            }
            Err(api::ApiError::Message(e)) => {
                let _ = win.close();
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
    featured: Signal<Option<api::FeaturedMedia>>,
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
    featured.set(None);
    current_id.set(None);

    // Clear whatever the shared editor held from a previously opened post.
    let schema = Schema::starter_kit();
    let handle = editor.get();
    if let Ok(node) = bridge::block_tree_json_to_node(&schema, &empty_tree()) {
        handle.load_doc(node);
    }
    auth.view.set(View::Editor);
}

/// Start a brand-new page: reset the editor meta to a blank top-level Draft, clear the
/// shared editor, set the entity kind to Page, and switch to the editor. Mirrors
/// [`new_post`] plus the hierarchy defaults (no parent, order 0, default template);
/// `current_id` is `None` so the first Save creates the page.
#[allow(clippy::too_many_arguments)]
fn new_page(
    editor: Signal<EditorHandle>,
    kind: Signal<EntityKind>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    parent: Signal<Option<u64>>,
    menu_order: Signal<String>,
    template: Signal<String>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
    editor_session.update(|g| *g += 1);
    notice.set(String::new());
    title.set(String::new());
    slug.set(String::new());
    status.set("draft".to_owned());
    featured.set(None);
    parent.set(None);
    menu_order.set("0".to_owned());
    template.set(String::new());
    current_id.set(None);
    kind.set(EntityKind::Page);

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
    let Some(input) =
        web_sys::window().and_then(|w| w.prompt_with_message("Link URL:").ok().flatten())
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

/// Upload an image and set it as the post's featured image (no editor insert).
///
/// Mirrors [`insert_image_via_picker`] — a hidden `<input type=file>` `.click()`ed
/// synchronously in this trusted handler — but the chosen file becomes
/// `featured_media` (the `featured` signal, echoed back on save) rather than an
/// inline image, and there's no alt prompt (the featured control isn't inline text).
///
/// The upload runs async: if the editor has moved to a DIFFERENT document by the time
/// it resolves (tracked via `editor_session`, like [`save_post`]'s stale guard), the
/// result is dropped rather than featured on whatever post is now open.
fn set_featured_via_picker(
    featured: Signal<Option<api::FeaturedMedia>>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
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

    // The document this pick belongs to. If the editor loads another before the
    // upload resolves, its result must not land on the now-current post.
    let pick_gen = editor_session.get();
    let picker = input.clone();
    let on_change = Closure::<dyn FnMut()>::new(move || {
        let Some(file) = picker.files().and_then(|files| files.get(0)) else {
            return;
        };
        spawn_local(async move {
            let Ok(form) = web_sys::FormData::new() else {
                notice.set("Your browser blocked the upload form.".to_owned());
                return;
            };
            let _ = form.append_with_blob_and_filename("file", &file, &file.name());
            let _ = form.append_with_str("alt", "");

            let result = api::upload_media(form).await;
            // The editor moved on while we were uploading: drop the result (a 401 is
            // auth-wide, so it still routes to login).
            if editor_session.get() != pick_gen {
                if let Err(api::ApiError::Unauthorized) = result {
                    auth.session_expired();
                }
                return;
            }
            match result {
                Ok(resp) => featured.set(Some(api::FeaturedMedia {
                    id: resp.id,
                    url: resp.url,
                })),
                Err(api::ApiError::Unauthorized) => auth.session_expired(),
                Err(api::ApiError::Message(e)) => {
                    notice.set(format!("Featured image upload failed: {e}"))
                }
            }
        });
    });
    input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    on_change.forget();
    input.click();
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
///
/// The upload runs async: if the editor has loaded a DIFFERENT document by the time it
/// resolves (tracked via `editor_session`, like [`save_post`]'s stale guard), the
/// image is NOT inserted — it was picked for a post that is no longer open.
fn insert_image_via_picker(
    editor: Signal<EditorHandle>,
    editor_session: Signal<u64>,
    notice: Signal<String>,
    auth: AuthCtx,
) {
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
    let pick_gen = editor_session.get();
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

            let result = api::upload_media(form).await;
            // The editor moved to another document while we were uploading: don't
            // insert into it (a 401 is auth-wide, so it still routes to login).
            if editor_session.get() != pick_gen {
                if let Err(api::ApiError::Unauthorized) = result {
                    auth.session_expired();
                }
                return;
            }
            match result {
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

/// One controlled-`<select>` option: value + label + whether it's the current
/// selection. `selected` is computed against the signal value when the option list is
/// built (via the reactive `for`), so the right option shows on load; after that the
/// browser tracks the live selection natively and a programmatic change (open_page /
/// new_page) rebuilds the editor arm fresh — so a `Copy` bool suffices, with no
/// per-option reactive closure (which a non-`Copy` `String` value can't feed in a `for`
/// body).
#[derive(Clone, PartialEq)]
struct SelectOpt {
    value: String,
    label: String,
    selected: bool,
}

/// The option-value string for a parent id: the id as a string, or `""` for None
/// (top-level).
fn opt_value(id: Option<u64>) -> String {
    id.map(|x| x.to_string()).unwrap_or_default()
}

/// Parse the Order field (WP `menu_order`) leniently: an empty field is 0 (the default),
/// an out-of-range integer saturates to the `i32` bound, and a decimal is truncated — so a
/// mistyped `10.5` or an over-long number is honored near its intent instead of silently
/// collapsing to 0 (which would jump the page to the front of its siblings). Genuine
/// garbage still falls back to 0.
fn parse_menu_order(value: &str) -> i32 {
    let t = value.trim();
    if t.is_empty() {
        return 0;
    }
    if let Ok(n) = t.parse::<i64>() {
        return n.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
    // A float parse lets a decimal truncate (and saturate) rather than collapse to 0.
    t.parse::<f64>().map(|f| f as i32).unwrap_or(0)
}

/// Map the Template signal's string to the save request's `Option`: an empty string is
/// the default template (None); any other value is sent as-is (the server validates it
/// against the theme's registered templates).
fn template_arg(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_owned())
    }
}

/// The Template `<select>` options, marking the one matching `current` selected. The
/// server already includes the "Default" (empty value) entry, so this just annotates
/// selection.
fn template_opts(options: &[api::TemplateOption], current: &str) -> Vec<SelectOpt> {
    options
        .iter()
        .map(|o| SelectOpt {
            value: o.value.clone(),
            label: o.label.clone(),
            selected: o.value == current,
        })
        .collect()
}

/// Build the Parent `<select>` options: a top-level ("— None —") entry first, then every
/// page EXCEPT `self_id` and its own subtree (a page can't be parented under itself or a
/// descendant — the server enforces this too). `current` is the page's current parent,
/// used to mark the selected option. `self_id`'s path is looked up in `pages` to exclude
/// the subtree by path prefix; a brand-new page (`None`) excludes nothing.
fn parent_options(
    pages: &[api::PageSummary],
    self_id: Option<u64>,
    current: Option<u64>,
) -> Vec<SelectOpt> {
    let self_path = self_id
        .and_then(|sid| pages.iter().find(|p| p.id == sid).map(|p| p.path.clone()))
        .unwrap_or_default();
    let subtree_prefix = if self_path.is_empty() {
        None
    } else {
        Some(format!("{self_path}/"))
    };
    let current_str = opt_value(current);

    let mut opts = vec![SelectOpt {
        value: String::new(),
        label: "\u{2014} None (top level) \u{2014}".to_owned(),
        selected: current_str.is_empty(),
    }];
    for p in pages {
        if Some(p.id) == self_id {
            continue; // not itself
        }
        if let Some(prefix) = &subtree_prefix
            && (p.path == self_path || p.path.starts_with(prefix.as_str()))
        {
            continue; // not a descendant
        }
        let indent = "\u{00A0}\u{00A0}".repeat(p.depth as usize);
        let label = if p.title.trim().is_empty() {
            format!("{indent}/{}", p.path)
        } else {
            format!("{indent}{}", p.title)
        };
        let value = p.id.to_string();
        let selected = value == current_str;
        opts.push(SelectOpt {
            value,
            label,
            selected,
        });
    }
    // If the current parent isn't among the candidates — e.g. it's outside an
    // author-scoped page list (a Contributor/Author only sees pages they authored) or a
    // stale list — still represent it as the selected option. Otherwise NO option is
    // marked, the browser falls back to showing the first ("— None —") entry, which both
    // misreports the page as top-level AND traps the author: re-selecting the already-shown
    // "None" fires no change event, so an intended un-parent edit is silently dropped. With
    // the current parent shown as selected, "None" becomes a real change the author can pick.
    if let Some(pid) = current
        && !opts.iter().any(|o| o.selected)
    {
        opts.push(SelectOpt {
            value: pid.to_string(),
            label: format!("#{pid} (current parent)"),
            selected: true,
        });
    }
    opts
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

// ============================================================================
// Nav-menu editor — components, navigation/save helpers, and the pure tree ops.
// ============================================================================

/// Whether this user's role grants `ManageMenus` (Editor+), mirroring the server gate.
/// A role-string check like [`is_admin`] — a display hint only; the server enforces the
/// capability authoritatively, so a stale/edge role just sees a nav button that 403s.
fn can_manage_menus(user: &Option<UserDto>) -> bool {
    matches!(
        user.as_ref().map(|u| u.role.as_str()),
        Some("editor") | Some("administrator")
    )
}

// ── one location-assignment row (a <select> of menus) ─────────────────────────

/// One theme-location row in the Locations panel: a `<select>` of every menu (plus
/// "none", plus an out-of-vocab "(current)" fallback for a stranded binding). Its own
/// component so the reactive/option build is per-row. On change it assigns and RELOADS
/// the locations list as the source of truth (reverting the control on a failed assign).
#[component]
fn MenuLocationView(row: api::MenuLocationRow, menu: MenuCtx) -> NodeHandle {
    let loc = row.location.clone();
    let current_id = row.menu.as_ref().map(|m| m.id);
    let menus = menu.list.get();
    let current_present = current_id
        .map(|id| menus.iter().any(|m| m.id == id))
        .unwrap_or(true);

    let mut opts: Vec<NodeHandle> = Vec::with_capacity(menus.len() + 2);
    opts.push(
        rsx! { option { value: "", selected: current_id.is_none(), "\u{2014} none \u{2014}" } },
    );
    // Consume `menus` (owned) so each option's static text moves in — mirrors the
    // settings Select (a borrowed `m` would outlive its scope in the rsx effect).
    for m in menus {
        let selected = current_id == Some(m.id);
        opts.push(rsx! { option { value: m.id.to_string(), selected: selected, {m.name} } });
    }
    // A bound menu that isn't among the options (a stranded/undeclared binding) → append
    // it as an explicit selected "(current)" option so exactly one option stays marked and
    // "— none —" is a real, change-firing choice (mirrors the settings Select fallback).
    if let Some(id) = current_id
        && !current_present
    {
        let label = row
            .menu
            .as_ref()
            .map(|m| format!("{} (current)", m.name))
            .unwrap_or_else(|| format!("menu #{id} (current)"));
        opts.push(rsx! { option { value: id.to_string(), selected: true, {label} } });
    }

    let row_class = if row.declared {
        "setrow"
    } else {
        "setrow setrow--stranded"
    };
    let declared = row.declared;
    let label_text = if row.label.is_empty() {
        row.location.clone()
    } else {
        row.label.clone()
    };
    rsx! {
        div { class: row_class,
            div { class: "setrow__label",
                {label_text}
                if !declared {
                    small { "theme no longer declares this" }
                }
            }
            div { class: "setrow__control",
                select {
                    class: "select",
                    oninput: move |v: String| {
                        let id = v.parse::<u64>().ok();
                        assign_menu_location(menu, &loc, id);
                    },
                    {opts}
                }
            }
        }
    }
}

// ── one tree row ──────────────────────────────────────────────────────────────

/// One menu-item row in the editor tree. A `#[component]` (not inline `for`-body markup)
/// because the target descriptor is a one-shot `match` on the row's (non-Copy) kind — an
/// inline reactive branch can't move it (the reason `RowLead` exists). Its `depth`,
/// indent, and reorder-button enabled state are read REACTIVELY from the live tree signal
/// (a keyed row is not rebuilt on a tree change), and its label/URL/new-tab bind to this
/// row's own [`RowEdit`] signals so a keystroke touches only its own input.
#[component]
fn MenuRowView(
    cid: String,
    kind: RowKind,
    resolved: Option<api::ResolvedTarget>,
    edits: Signal<HashMap<String, RowEdit>>,
    tree: Signal<Vec<MenuRow>>,
    dirty: Signal<bool>,
    /// True while a save is in flight — every mutation handler no-ops so a concurrent edit
    /// can't be silently clobbered by the post-save re-seed (must-fix B2).
    saving: Signal<bool>,
) -> NodeHandle {
    // This row's per-row edit signals (created at add/load; the invariant is that every
    // cid in the tree has one). A defensive empty row if somehow absent — never hit.
    let Some(edit) = edits.get().get(&cid).copied() else {
        return rsx! { li { class: "menurow" } };
    };
    let is_custom = matches!(kind, RowKind::Custom);
    let kind_word = kind_word(&kind);
    let kind_tag = kind_tag_class(&kind);
    let title = resolved
        .as_ref()
        .map(|r| r.title.clone())
        .unwrap_or_default();
    let resolved_href = resolved.as_ref().and_then(|r| r.href.clone());
    let placeholder = if title.is_empty() {
        "Label".to_owned()
    } else {
        title.clone()
    };

    // The descriptor right of the kind tag: a live URL input for a Custom item, else the
    // static resolved title. Built ONCE (kind can't change without remove+re-add).
    let detail_node = if is_custom {
        rsx! {
            input {
                class: "menurow__urlinput",
                value: {move || edit.url.get()},
                placeholder: "/path",
                aria-label: "Custom link URL",
                oninput: move |v: String| { if saving.get() { return; } edit.url.set(v); dirty.set(true); },
            }
        }
    } else {
        let t = title.clone();
        rsx! { span { {t} } }
    };
    // Whether/where this item resolves — the "won't render" signal (href: None).
    let href_node = if is_custom {
        rsx! {
            span {
                class: {move || if edit.url.get().trim().is_empty() { "menurow__warn" } else { "menurow__href" }},
                {move || {
                    let u = edit.url.get();
                    if u.trim().is_empty() { "\u{2192} needs a URL".to_owned() }
                    else { format!("\u{2192} {}", u.trim()) }
                }}
            }
        }
    } else {
        match resolved_href {
            Some(h) => rsx! { span { class: "menurow__href", {format!("\u{2192} {h}")} } },
            None => {
                rsx! { span { class: "menurow__warn", "\u{2192} won\u{2019}t render (unpublished / unlinkable)" } }
            }
        }
    };

    // The reorder/remove button aria-labels — pre-formatted (the rsx macro wraps each
    // attribute value in its own closure, so a shared `name` would be moved five times).
    // Name the item by its current label, else its resolved title, else the cid — a Custom
    // item has no resolved title, so its label is the meaningful name.
    let name = {
        let label = edit.label.get();
        if !label.is_empty() {
            label
        } else if !title.is_empty() {
            title.clone()
        } else {
            cid.clone()
        }
    };
    let aria_up = format!("Move {name} up");
    let aria_down = format!("Move {name} down");
    let aria_out = format!("Outdent {name}");
    let aria_in = format!("Indent {name}");
    let aria_rm = format!("Remove {name}");
    // A Copy handle to this row's (constant) cid: the reactive class/style closures are
    // RE-CREATED on each effect run, so they must capture only Copy values — capturing the
    // non-Copy `cid` by move would move it out of the effect repeatedly. The onclick
    // handlers are stored once, so they keep plain per-handler clones.
    let cid_sig = Signal::new(cid.clone());
    let (c_up, c_down, c_out, c_in, c_rm) = (
        cid.clone(),
        cid.clone(),
        cid.clone(),
        cid.clone(),
        cid.clone(),
    );

    rsx! {
        li {
            class: {move || if row_depth(&tree.get(), &cid_sig.get()) > 0 { "menurow is-child" } else { "menurow" }},
            style: {move || format!("margin-left:{}rem", (row_depth(&tree.get(), &cid_sig.get()) as f32) * 1.5)},
            span { class: "menurow__reorder",
                button {
                    r#type: "button",
                    class: {move || rbtn_class(reorder_ok(tree, &cid_sig.get(), can_move_up))},
                    aria-label: aria_up, title: "Move up",
                    onclick: move || apply_op(saving, tree, dirty, &c_up, can_move_up, move_up),
                    "\u{2191}"
                }
                button {
                    r#type: "button",
                    class: {move || rbtn_class(reorder_ok(tree, &cid_sig.get(), can_move_down))},
                    aria-label: aria_down, title: "Move down",
                    onclick: move || apply_op(saving, tree, dirty, &c_down, can_move_down, move_down),
                    "\u{2193}"
                }
                button {
                    r#type: "button",
                    class: {move || rbtn_class(reorder_ok(tree, &cid_sig.get(), can_outdent))},
                    aria-label: aria_out, title: "Outdent",
                    onclick: move || apply_op(saving, tree, dirty, &c_out, can_outdent, outdent),
                    "\u{21E4}"
                }
                button {
                    r#type: "button",
                    class: {move || rbtn_class(reorder_ok(tree, &cid_sig.get(), can_indent))},
                    aria-label: aria_in, title: "Indent",
                    onclick: move || apply_op(saving, tree, dirty, &c_in, can_indent, indent),
                    "\u{21E5}"
                }
            }
            span { class: "menurow__body",
                input {
                    class: "menurow__labelinput",
                    value: {move || edit.label.get()},
                    placeholder: placeholder,
                    aria-label: "Menu item label",
                    oninput: move |v: String| { if saving.get() { return; } edit.label.set(v); dirty.set(true); },
                }
                span { class: "menurow__meta",
                    span { class: kind_tag, {kind_word} }
                    {detail_node}
                    {href_node}
                }
            }
            span { class: "menurow__actions",
                label { class: "switch", title: "Open in a new tab",
                    input {
                        r#type: "checkbox", class: "menurow__newtab",
                        checked: {move || edit.new_tab.get()},
                        oninput: move |c: String| { if saving.get() { return; } edit.new_tab.set(c == "true"); dirty.set(true); },
                    }
                    span {
                        class: {move || if edit.new_tab.get() { "switch__track is-on" } else { "switch__track" }},
                        span { class: "switch__knob" }
                    }
                    span { class: "switch__text", style: "font-size:.66rem;text-transform:uppercase;letter-spacing:.06em", "new tab" }
                }
                button {
                    r#type: "button", class: "xbtn",
                    aria-label: aria_rm, title: "Remove",
                    onclick: move || remove_row(saving, tree, edits, dirty, &c_rm),
                    "\u{2715}"
                }
            }
        }
    }
}

// ── navigation / load ─────────────────────────────────────────────────────────

/// Switch to the Menus cabinet and (re)load the menu list, THEN the location assignments.
/// The order is load-bearing: each location `<select>`'s options are the menu list, and a
/// `MenuLocationView` reads that list once when it is built (on a `locations` change). So
/// the list must be populated BEFORE `locations` is set — otherwise the selects render
/// with no menu options. Clearing `locations` first guarantees the rebuild re-fires.
fn open_menus(menu: MenuCtx) {
    menu.notice.set(String::new());
    menu.view.set(View::Menus);
    menu.list_state.set(Load::Loading);
    menu.locations.set(Vec::new());
    spawn_local(async move {
        match api::list_menus().await {
            Ok(v) => {
                menu.list.set(v);
                menu.list_state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => {
                menu.auth.session_expired();
                return;
            }
            Err(api::ApiError::Message(_)) => menu.list_state.set(Load::Error),
        }
        // List is populated (or errored) — now the selects can offer the menus.
        match api::list_menu_locations().await {
            Ok(v) => menu.locations.set(v),
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(_)) => {}
        }
    });
}

/// Fetch the location assignments (the source of truth for the Locations `<select>`s).
fn reload_menu_locations(menu: MenuCtx) {
    spawn_local(async move {
        match api::list_menu_locations().await {
            // Clear THEN set so the keyed `for` genuinely rebuilds each `MenuLocationView`
            // (its `<select>` `selected` flags are built once, non-reactively). Without the
            // clear, a reload whose data is PartialEq-equal to the current rows (e.g. a
            // FAILED assign, where the binding is unchanged) would be a no-op — leaving the
            // native `<select>` still showing the user's rejected pick (must-fix F3).
            Ok(v) => {
                menu.locations.set(Vec::new());
                menu.locations.set(v);
            }
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            // A failed locations fetch leaves the panel stale rather than blanking it.
            Err(api::ApiError::Message(_)) => {}
        }
    });
}

/// Prompt for a name and create a new (empty) menu, then open it in the editor.
fn new_menu(menu: MenuCtx) {
    let Some(name) = window_prompt("Name for the new menu", "New Menu") else {
        return;
    };
    if name.trim().is_empty() {
        return;
    }
    let name = name.trim().to_owned();
    menu.notice.set(String::new());
    spawn_local(async move {
        match api::create_menu(&name, None).await {
            Ok(m) => open_menu_editor(m.id, menu),
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(e)) => menu.notice.set(e),
        }
    });
}

/// Open one menu in the editor: reset the editing state, fetch it, and seed the tree +
/// per-row signals from the response.
fn open_menu_editor(id: u64, menu: MenuCtx) {
    menu.notice.set(String::new());
    menu.edit_id.set(Some(id));
    menu.load.set(Load::Loading);
    menu.dirty.set(false);
    menu.tree.set(Vec::new());
    menu.edits.set(HashMap::new());
    menu.name.set(String::new());
    menu.slug.set(String::new());
    menu.view.set(View::MenuEditor);
    spawn_local(async move {
        match api::get_menu(id).await {
            Ok(detail) => {
                menu.name.set(detail.name.clone());
                menu.slug.set(detail.slug.clone());
                seed_tree_from_detail(menu, detail);
                menu.dirty.set(false);
                menu.load.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                menu.notice.set(e);
                menu.load.set(Load::Error);
            }
        }
    });
}

/// Leave the editor for the Menus list — guarded by an unsaved-changes confirm.
fn leave_menu_editor(menu: MenuCtx) {
    if menu.dirty.get() && !confirm("Leave without saving? Your changes to this menu will be lost.")
    {
        return;
    }
    menu.dirty.set(false);
    open_menus(menu);
}

/// Rebuild the tree structure + per-row edit signals from a loaded/saved [`api::MenuDetail`].
fn seed_tree_from_detail(menu: MenuCtx, detail: api::MenuDetail) {
    // Keep the authoritative id from the response (a load/save always echoes it).
    menu.edit_id.set(Some(detail.id));
    // Reuse the currently-mounted rows' signals for cids that survive (a re-seed after
    // save), so a still-mounted MenuRowView's inputs keep binding to live cells (see
    // `push_subtree`). On a fresh open the current map is empty, so all are new.
    let prev = menu.edits.get();
    let (rows, edits) = build_rows(&detail.items, &prev);
    menu.edits.set(edits);
    menu.tree.set(rows);
    // New-item cids ("n{k}") restart from 0 — every loaded cid is a numeric server id,
    // so "n0" can't collide.
    menu.next_cid.set(0);
}

// ── save / delete ─────────────────────────────────────────────────────────────

/// Persist the menu: validate custom URLs, rename the menu, then reconcile its whole
/// item forest. Re-seeds the editor from the authoritative save response (new ids +
/// fresh resolved display) and clears the dirty flag. Gated on a clean load (must-fix M1).
fn save_menu(menu: MenuCtx) {
    if menu.saving.get() {
        return;
    }
    // M1: never derive a forest from anything but a cleanly-loaded menu.
    let Some(id) = menu.edit_id.get() else {
        return;
    };
    if !matches!(menu.load.get(), Load::Ready) {
        return;
    }

    let tree = menu.tree.get();
    let edits = menu.edits.get();
    // Validate every Custom URL client-side (distinct whitespace vs scheme messages).
    for r in &tree {
        if matches!(r.kind, RowKind::Custom)
            && let Some(e) = edits.get(&r.cid)
            && let Err(msg) = validate_custom_url(&e.url.get())
        {
            let label = e.label.get();
            let which = if label.is_empty() {
                "a custom link".to_owned()
            } else {
                format!("\u{201c}{label}\u{201d}")
            };
            menu.notice.set(format!("{which}: {msg}"));
            return;
        }
    }
    let forest = rows_to_forest(&tree, &edits);
    // Guard an accidental empty save (a desired-state PUT would clear every item).
    if forest.is_empty()
        && !confirm(
            "Save this menu with no items? Any location using it falls back to the theme default.",
        )
    {
        return;
    }

    menu.notice.set(String::new());
    menu.saving.set(true);
    let name = menu.name.get();
    let slug = menu.slug.get();
    spawn_local(async move {
        // Rename first (so a slug/name edit persists even if the item PUT later fails).
        match api::update_menu(id, &name, Some(&slug)).await {
            Ok(m) => {
                menu.name.set(m.name);
                menu.slug.set(m.slug);
            }
            Err(api::ApiError::Unauthorized) => {
                menu.saving.set(false);
                menu.auth.session_expired();
                return;
            }
            Err(api::ApiError::Message(e)) => {
                menu.saving.set(false);
                menu.notice.set(e);
                return;
            }
        }
        match api::save_menu_items(id, &forest).await {
            Ok(detail) => {
                menu.name.set(detail.name.clone());
                menu.slug.set(detail.slug.clone());
                seed_tree_from_detail(menu, detail);
                menu.dirty.set(false);
                menu.saving.set(false);
                menu.toast.set(true);
                TimeoutFuture::new(1600).await;
                menu.toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => {
                menu.saving.set(false);
                menu.auth.session_expired();
            }
            Err(api::ApiError::Message(e)) => {
                menu.saving.set(false);
                menu.notice.set(e);
            }
        }
    });
}

/// Delete the open menu (after a confirm that names any bound locations), then return to
/// the Menus list.
fn delete_menu_clicked(menu: MenuCtx) {
    if menu.saving.get() {
        return;
    }
    let Some(id) = menu.edit_id.get() else {
        return;
    };
    let name = menu.name.get();
    // Name the locations this menu is bound to, so the author knows the nav there breaks.
    let bound: Vec<String> = menu
        .locations
        .get()
        .iter()
        .filter(|l| l.menu.as_ref().map(|m| m.id) == Some(id))
        .map(|l| {
            if l.label.is_empty() {
                l.location.clone()
            } else {
                l.label.clone()
            }
        })
        .collect();
    let label = if name.is_empty() {
        "this menu".to_owned()
    } else {
        format!("\u{201c}{name}\u{201d}")
    };
    let msg = if bound.is_empty() {
        format!("Delete {label}? This can\u{2019}t be undone.")
    } else {
        format!(
            "Delete {label}? It is shown in {} \u{2014} the nav there will fall back to the theme default.",
            bound.join(", ")
        )
    };
    if !confirm(&msg) {
        return;
    }
    menu.notice.set(String::new());
    spawn_local(async move {
        match api::delete_menu(id).await {
            Ok(()) => {
                menu.dirty.set(false);
                open_menus(menu);
            }
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(e)) => menu.notice.set(e),
        }
    });
}

// ── the add-item picker ───────────────────────────────────────────────────────

/// Open the add-item picker and load the candidate targets.
fn open_picker(menu: MenuCtx) {
    if menu.saving.get() {
        return;
    }
    menu.picker_err.set(String::new());
    menu.picker_query.set(String::new());
    menu.picker_open.set(true);
    reload_candidates(menu);
    focus_first_modal_input();
}

/// Close the add-item picker, restoring focus to the opener (the "+ Add item" button) so a
/// keyboard user isn't stranded on `document.body` (F7).
fn close_picker(menu: MenuCtx) {
    menu.picker_open.set(false);
    focus_add_item_button();
}

/// Fetch the link candidates for the current search query. Guards against out-of-order
/// responses: each call bumps a generation and drops its writeback if a newer search has
/// started meanwhile (so a slow "a" can't overwrite a later "ab").
fn reload_candidates(menu: MenuCtx) {
    menu.candidates_state.set(Load::Loading);
    menu.candidates_gen.update(|g| *g += 1);
    let generation = menu.candidates_gen.get();
    let q = menu.picker_query.get();
    spawn_local(async move {
        let result = api::list_link_candidates(Some(&q)).await;
        // A newer search superseded this one — drop the stale response.
        if menu.candidates_gen.get() != generation {
            return;
        }
        match result {
            Ok(c) => {
                menu.candidates.set(c);
                menu.candidates_state.set(Load::Ready);
            }
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(_)) => menu.candidates_state.set(Load::Error),
        }
    });
}

/// Append a Post/Page item (from a picked candidate) at the top level.
fn add_candidate_item(menu: MenuCtx, cand: api::LinkCandidate) {
    let kind = if cand.kind == "page" {
        RowKind::Page(cand.id)
    } else {
        RowKind::Post(cand.id)
    };
    let resolved = Some(api::ResolvedTarget {
        title: cand.title,
        href: Some(cand.href),
    });
    add_row(menu, kind, resolved, String::new(), String::new());
    close_picker(menu);
}

/// Validate + append a Custom-link item (an empty label is fine — it falls back to the URL).
fn add_custom_item(menu: MenuCtx) {
    match validate_custom_url(&menu.custom_url.get()) {
        Ok(clean) => {
            add_row(menu, RowKind::Custom, None, menu.custom_label.get(), clean);
            menu.custom_url.set(String::new());
            menu.custom_label.set(String::new());
            menu.picker_err.set(String::new());
            close_picker(menu);
        }
        Err(msg) => menu.picker_err.set(msg),
    }
}

/// Append a new top-level row: create its per-row edit signals, then push the structure.
fn add_row(
    menu: MenuCtx,
    kind: RowKind,
    resolved: Option<api::ResolvedTarget>,
    label: String,
    url: String,
) {
    if menu.saving.get() {
        return;
    }
    let k = menu.next_cid.get();
    menu.next_cid.set(k + 1);
    let cid = format!("n{k}");
    let edit = RowEdit {
        label: Signal::new(label),
        url: Signal::new(url),
        new_tab: Signal::new(false),
    };
    // Insert the edit signals BEFORE pushing the row, so the row's MenuRowView (built when
    // the tree `for` re-fires) finds its entry.
    menu.edits.update(|m| {
        m.insert(cid.clone(), edit);
    });
    menu.tree.update(|t| {
        t.push(MenuRow {
            cid,
            server_id: None,
            depth: 0,
            kind,
            resolved,
        });
    });
    menu.dirty.set(true);
}

/// Assign (or clear) a location's menu, then reload the locations panel — the source of
/// truth, so a failed assign reverts the control instead of lying.
fn assign_menu_location(menu: MenuCtx, location: &str, menu_id: Option<u64>) {
    menu.notice.set(String::new());
    let loc = location.to_owned();
    spawn_local(async move {
        match api::assign_location(&loc, menu_id).await {
            Ok(()) => reload_menu_locations(menu),
            Err(api::ApiError::Unauthorized) => menu.auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                menu.notice.set(e);
                reload_menu_locations(menu);
            }
        }
    });
}

// ── flat ⇄ forest conversion ───────────────────────────────────────────────────

/// Build the flat-with-depth rows + per-row edit signals from the server's item forest.
/// Groups by `parent_client_id` (preserving Vec order for sibling order), walks pre-order
/// from the roots, and — mirroring serve's `assemble_tree` — PROMOTES any item whose
/// parent is absent from the payload to a root (never drops it, so it can't be silently
/// deleted on the next whole-tree save). A visited set breaks any cycle.
fn build_rows(
    items: &[api::MenuItemNode],
    prev: &HashMap<String, RowEdit>,
) -> (Vec<MenuRow>, HashMap<String, RowEdit>) {
    let present: HashSet<&str> = items.iter().map(|i| i.client_id.as_str()).collect();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (idx, it) in items.iter().enumerate() {
        match &it.parent_client_id {
            Some(p) if present.contains(p.as_str()) => {
                children.entry(p.clone()).or_default().push(idx);
            }
            // A root, OR an orphan whose parent isn't present → promoted to a root.
            _ => roots.push(idx),
        }
    }
    let mut rows: Vec<MenuRow> = Vec::with_capacity(items.len());
    let mut edits: HashMap<String, RowEdit> = HashMap::with_capacity(items.len());
    let mut visited = vec![false; items.len()];
    for &r in &roots {
        push_subtree(
            r,
            0,
            items,
            &children,
            prev,
            &mut rows,
            &mut edits,
            &mut visited,
        );
    }
    // Any item still unvisited is a cycle survivor (its ancestors loop) — surface it as a
    // root so it survives the round-trip rather than vanishing.
    for idx in 0..items.len() {
        if !visited[idx] {
            push_subtree(
                idx,
                0,
                items,
                &children,
                prev,
                &mut rows,
                &mut edits,
                &mut visited,
            );
        }
    }
    (rows, edits)
}

/// Recursive pre-order walk used by [`build_rows`]: emit the row + its edit signals, then
/// recurse into its children (in payload order).
///
/// For a cid that already exists in `prev` (a re-seed after save — the server echoes a kept
/// item's id AS its client_id, so the cid is stable), the EXISTING `RowEdit` signals are
/// REUSED (their values re-set to the authoritative server echo), never re-allocated. This
/// is load-bearing: the keyed tree `for` preserves the DOM of a surviving cid and does NOT
/// re-run its `MenuRowView`, so that row's inputs stay bound to the signals captured at its
/// first build. Allocating fresh signals here would strand those bindings — a subsequent
/// field edit would write the mounted (old) signal while a save reads the new one, silently
/// dropping the edit. Reusing keeps the mounted binding live (and preserves focus/caret).
#[allow(clippy::too_many_arguments)]
fn push_subtree(
    idx: usize,
    depth: u32,
    items: &[api::MenuItemNode],
    children: &HashMap<String, Vec<usize>>,
    prev: &HashMap<String, RowEdit>,
    rows: &mut Vec<MenuRow>,
    edits: &mut HashMap<String, RowEdit>,
    visited: &mut [bool],
) {
    if visited[idx] {
        return;
    }
    visited[idx] = true;
    let it = &items[idx];
    let url = match &it.target {
        LinkTarget::Custom { url } => url.clone(),
        _ => String::new(),
    };
    let edit = match prev.get(&it.client_id) {
        // Reuse the mounted row's signals, re-seeding them to the server's authoritative
        // values (idempotent right after a save; reflects any server normalization).
        Some(e) => {
            e.label.set(it.label.clone());
            e.url.set(url);
            e.new_tab.set(it.new_tab);
            *e
        }
        None => RowEdit {
            label: Signal::new(it.label.clone()),
            url: Signal::new(url),
            new_tab: Signal::new(it.new_tab),
        },
    };
    edits.insert(it.client_id.clone(), edit);
    rows.push(MenuRow {
        cid: it.client_id.clone(),
        server_id: it.id,
        depth,
        kind: kind_of(&it.target),
        resolved: it.resolved.clone(),
    });
    if let Some(kids) = children.get(&it.client_id) {
        for &k in kids {
            push_subtree(k, depth + 1, items, children, prev, rows, edits, visited);
        }
    }
}

/// Flatten the tree + per-row edits back into the whole-tree PUT forest. Each row's parent
/// is the nearest preceding row at `depth - 1` (the flat-invariant parent); item order is
/// left to the server (it renumbers from the payload order).
fn rows_to_forest(tree: &[MenuRow], edits: &HashMap<String, RowEdit>) -> Vec<api::MenuItemNode> {
    tree.iter()
        .enumerate()
        .map(|(i, r)| {
            let parent_client_id = if r.depth == 0 {
                None
            } else {
                tree[..i]
                    .iter()
                    .rev()
                    .find(|p| p.depth == r.depth - 1)
                    .map(|p| p.cid.clone())
            };
            let e = edits.get(&r.cid);
            let label = e.map(|e| e.label.get()).unwrap_or_default();
            let new_tab = e.map(|e| e.new_tab.get()).unwrap_or(false);
            let target = match &r.kind {
                RowKind::Post(id) => LinkTarget::Post { id: *id },
                RowKind::Page(id) => LinkTarget::Page { id: *id },
                RowKind::Term(id) => LinkTarget::Term { id: *id },
                RowKind::Custom => LinkTarget::Custom {
                    url: e.map(|e| e.url.get()).unwrap_or_default(),
                },
            };
            api::MenuItemNode {
                id: r.server_id,
                client_id: r.cid.clone(),
                parent_client_id,
                label,
                target,
                new_tab,
                resolved: None,
            }
        })
        .collect()
}

// ── pure tree ops (validated by the HTML-first mockup's fuzz test) ─────────────

/// The index of the row with `cid`, if present.
fn index_of(tree: &[MenuRow], cid: &str) -> Option<usize> {
    tree.iter().position(|r| r.cid == cid)
}

/// The live depth of the row with `cid` (0 if absent).
fn row_depth(tree: &[MenuRow], cid: &str) -> u32 {
    tree.iter()
        .find(|r| r.cid == cid)
        .map(|r| r.depth)
        .unwrap_or(0)
}

/// The end (exclusive) of the subtree rooted at `i`: the first later row at depth ≤ `i`'s.
fn subtree_end(tree: &[MenuRow], i: usize) -> usize {
    let d = tree[i].depth;
    let mut k = i + 1;
    while k < tree.len() && tree[k].depth > d {
        k += 1;
    }
    k
}

/// The previous SIBLING of `i` (nearest earlier row at the same depth before any shallower
/// one), or `None` when `i` is the first child / first top-level row.
fn prev_sibling(tree: &[MenuRow], i: usize) -> Option<usize> {
    let d = tree[i].depth;
    for j in (0..i).rev() {
        if tree[j].depth < d {
            return None;
        }
        if tree[j].depth == d {
            return Some(j);
        }
    }
    None
}

/// The next SIBLING of `i`, or `None` when `i` is the last in its sibling group.
fn next_sibling(tree: &[MenuRow], i: usize) -> Option<usize> {
    let d = tree[i].depth;
    let k = subtree_end(tree, i);
    if k < tree.len() && tree[k].depth == d {
        Some(k)
    } else {
        None
    }
}

/// The maximum depth within `[i, k)`.
fn max_depth_in(tree: &[MenuRow], i: usize, k: usize) -> u32 {
    tree[i..k].iter().map(|r| r.depth).max().unwrap_or(0)
}

fn can_move_up(tree: &[MenuRow], i: usize) -> bool {
    prev_sibling(tree, i).is_some()
}
fn can_move_down(tree: &[MenuRow], i: usize) -> bool {
    next_sibling(tree, i).is_some()
}
fn can_indent(tree: &[MenuRow], i: usize) -> bool {
    prev_sibling(tree, i).is_some() && max_depth_in(tree, i, subtree_end(tree, i)) < MAX_MENU_DEPTH
}
fn can_outdent(tree: &[MenuRow], i: usize) -> bool {
    tree[i].depth > 0
}

/// Move `i`'s subtree up past its previous sibling's subtree (both keep their internal depths).
fn move_up(tree: &mut Vec<MenuRow>, i: usize) {
    let Some(p) = prev_sibling(tree, i) else {
        return;
    };
    let k = subtree_end(tree, i);
    let block: Vec<MenuRow> = tree.drain(i..k).collect();
    for (off, r) in block.into_iter().enumerate() {
        tree.insert(p + off, r);
    }
}

/// Move `i`'s subtree down past its next sibling's subtree.
fn move_down(tree: &mut Vec<MenuRow>, i: usize) {
    let Some(n) = next_sibling(tree, i) else {
        return;
    };
    let k = subtree_end(tree, i); // [i, k) == [i, n)
    let m = subtree_end(tree, n); // [n, m)
    let next: Vec<MenuRow> = tree[n..m].to_vec();
    let mine: Vec<MenuRow> = tree[i..k].to_vec();
    tree.splice(i..m, next.into_iter().chain(mine));
}

/// Nest `i`'s subtree one level deeper (it becomes the previous sibling's last child).
// `&mut Vec` (not `&mut [_]`) so the signature matches the length-changing ops (move_up/
// move_down) passed alongside it to `apply_op`'s uniform `fn(&mut Vec<MenuRow>, usize)`.
#[allow(clippy::ptr_arg)]
fn indent(tree: &mut Vec<MenuRow>, i: usize) {
    if !can_indent(tree, i) {
        return;
    }
    let k = subtree_end(tree, i);
    for r in &mut tree[i..k] {
        r.depth += 1;
    }
}

/// Un-nest `i`'s subtree one level (WP-faithful: any following same-depth siblings become
/// its children, which falls out of the flat invariant).
#[allow(clippy::ptr_arg)] // uniform `fn(&mut Vec<MenuRow>, usize)` signature — see `indent`.
fn outdent(tree: &mut Vec<MenuRow>, i: usize) {
    if tree[i].depth == 0 {
        return;
    }
    let k = subtree_end(tree, i);
    for r in &mut tree[i..k] {
        r.depth -= 1;
    }
}

/// Apply a gated reorder op to the row with `cid`, marking the tree dirty iff it changed.
/// Gating lives HERE (in the handler), never on a `disabled:` attr (not reactive). A no-op
/// while a save is in flight (B2) so a reorder can't race the post-save re-seed.
fn apply_op(
    saving: Signal<bool>,
    tree: Signal<Vec<MenuRow>>,
    dirty: Signal<bool>,
    cid: &str,
    can: fn(&[MenuRow], usize) -> bool,
    op: fn(&mut Vec<MenuRow>, usize),
) {
    if saving.get() {
        return;
    }
    let mut changed = false;
    tree.update(|t| {
        if let Some(i) = index_of(t, cid)
            && can(t, i)
        {
            op(t, i);
            changed = true;
        }
    });
    if changed {
        dirty.set(true);
    }
}

/// Remove the row with `cid` AND its whole subtree (no orphans). Confirms when the subtree
/// is non-empty, and drops the removed rows' edit signals. A no-op while saving (B2).
fn remove_row(
    saving: Signal<bool>,
    tree: Signal<Vec<MenuRow>>,
    edits: Signal<HashMap<String, RowEdit>>,
    dirty: Signal<bool>,
    cid: &str,
) {
    if saving.get() {
        return;
    }
    let t = tree.get();
    let Some(i) = index_of(&t, cid) else {
        return;
    };
    let k = subtree_end(&t, i);
    let n = k - i - 1;
    if n > 0 {
        let label = edits
            .get()
            .get(cid)
            .map(|e| e.label.get())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "this item".to_owned());
        if !confirm(&format!(
            "Remove \u{201c}{}\u{201d} and its {} sub-item{}?",
            label,
            n,
            if n == 1 { "" } else { "s" }
        )) {
            return;
        }
    }
    let removed: Vec<String> = t[i..k].iter().map(|r| r.cid.clone()).collect();
    tree.update(|v| {
        v.drain(i..k);
    });
    edits.update(|m| {
        for c in &removed {
            m.remove(c);
        }
    });
    dirty.set(true);
}

/// Whether the row with `cid` may currently perform `can` (reads the live tree).
fn reorder_ok(tree: Signal<Vec<MenuRow>>, cid: &str, can: fn(&[MenuRow], usize) -> bool) -> bool {
    let t = tree.get();
    index_of(&t, cid).map(|i| can(&t, i)).unwrap_or(false)
}

// ── small helpers ─────────────────────────────────────────────────────────────

/// The reorder-button class for its enabled state (greyed via `is-off`, never `disabled`).
fn rbtn_class(ok: bool) -> &'static str {
    if ok { "rbtn" } else { "rbtn is-off" }
}

/// The picker tab's class (highlighted when current).
fn tab_class(current: u8, tab: u8) -> &'static str {
    if current == tab { "tab is-on" } else { "tab" }
}

/// The kind word shown in a row's target tag.
fn kind_word(k: &RowKind) -> &'static str {
    match k {
        RowKind::Post(_) => "Post",
        RowKind::Page(_) => "Page",
        RowKind::Term(_) => "Term",
        RowKind::Custom => "Custom",
    }
}

/// The tag CSS class for a target kind.
fn kind_tag_class(k: &RowKind) -> &'static str {
    match k {
        RowKind::Post(_) => "tag tag--post",
        RowKind::Page(_) => "tag tag--page",
        RowKind::Term(_) => "tag tag--term",
        RowKind::Custom => "tag tag--custom",
    }
}

/// The [`RowKind`] a target implies.
fn kind_of(t: &LinkTarget) -> RowKind {
    match t {
        LinkTarget::Post { id } => RowKind::Post(*id),
        LinkTarget::Page { id } => RowKind::Page(*id),
        LinkTarget::Term { id } => RowKind::Term(*id),
        LinkTarget::Custom { .. } => RowKind::Custom,
    }
}

/// Validate a Custom-link URL for the editor, returning the sanitized value or a message.
/// Distinguishes whitespace from a bad scheme (mirroring the link toolbar) and defers the
/// scheme verdict to [`ferropress_core::sanitize_href`] — the SAME allow-list the server
/// enforces (pure, wasm-safe, can't drift).
fn validate_custom_url(raw: &str) -> Result<String, String> {
    let u = raw.trim();
    if u.is_empty() {
        return Err("Enter a URL for the custom link.".to_owned());
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("A URL can\u{2019}t contain spaces.".to_owned());
    }
    match ferropress_core::sanitize_href(u) {
        Some(clean) => Ok(clean),
        None => Err("Use an http(s), mailto, tel, or site-relative (/\u{2026}) link.".to_owned()),
    }
}

/// A native `window.prompt`, or `None` if dismissed / unavailable.
fn window_prompt(message: &str, default: &str) -> Option<String> {
    web_sys::window()?
        .prompt_with_message_and_default(message, default)
        .ok()
        .flatten()
}

/// A native `window.confirm`. Defaults to `true` when unavailable (a browser always has it).
fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(message).ok())
        .unwrap_or(true)
}

/// Focus the "+ Add item" button (the picker opener), restoring focus after the modal closes.
fn focus_add_item_button() {
    use wasm_bindgen::JsCast;
    if let Some(doc) = web_sys::window().and_then(|w| w.document())
        && let Ok(Some(el)) = doc.query_selector(".menubar button")
        && let Ok(html) = el.dyn_into::<web_sys::HtmlElement>()
    {
        let _ = html.focus();
    }
}

/// Focus the first input in the (just-opened) picker modal, on the next tick.
fn focus_first_modal_input() {
    use wasm_bindgen::JsCast;
    spawn_local(async move {
        TimeoutFuture::new(0).await;
        if let Some(doc) = web_sys::window().and_then(|w| w.document())
            && let Ok(Some(el)) = doc.query_selector(".media-modal input")
            && let Ok(html) = el.dyn_into::<web_sys::HtmlElement>()
        {
            let _ = html.focus();
        }
    });
}

/// Install a `beforeunload` guard so a tab close/reload while a menu edit is unsaved
/// prompts the browser's leave-confirmation. Gated on being IN the menu editor (F8): a
/// menu action that 401s routes to Login but leaves `dirty` set, and without the view
/// gate the guard would then fire a spurious prompt on the unrelated Posts list.
fn install_beforeunload_guard(dirty: Signal<bool>, view: Signal<View>) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(
        move |e: web_sys::BeforeUnloadEvent| {
            if dirty.get() && matches!(view.get(), View::MenuEditor) {
                e.prevent_default();
                e.set_return_value("");
            }
        },
    );
    let _ = window.add_event_listener_with_callback("beforeunload", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Install a document keydown listener that closes the picker on Escape.
fn install_escape_guard(picker_open: Signal<bool>) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" && picker_open.get() {
            picker_open.set(false);
            focus_add_item_button();
        }
    });
    let _ = doc.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
    cb.forget();
}
