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
    /// The taxonomy "cabinet": a taxonomy switcher + the active taxonomy's flat-with-
    /// depth term list (mirrors `View::Menus`).
    Terms,
    /// Creating or editing ONE term (Name/Slug/Description/Parent/Delete).
    TermEditor,
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
    /// SF17: the SAME signal handles `TermsCtx::taxonomies`/`hierarchical_terms`
    /// hold — cleared on session end so a re-login always re-fetches (never a
    /// stale vocabulary from a prior session). Not the WHOLE `TermsCtx` (a
    /// circular type — `TermsCtx` itself carries an `AuthCtx`), just the two
    /// fields this needs.
    vocab_taxonomies: Signal<Vec<api::TaxonomyDto>>,
    vocab_hierarchical_terms: Signal<HashMap<String, Vec<api::TermDto>>>,
}

impl AuthCtx {
    /// A guarded endpoint returned 401 (expired / revoked session): forget the user,
    /// explain on the login screen, and route there — never a false "outage".
    fn session_expired(self) {
        self.me_user.set(None);
        self.login_error
            .set("Your session ended \u{2014} please sign in again.".to_owned());
        self.view.set(View::Login);
        self.clear_vocab();
    }

    /// SF17: drop the shared taxonomy vocabulary — shared by `session_expired`
    /// (an implicit 401) and the explicit "Sign out" click, the two session-end
    /// paths. `load_vocab`'s own idempotency guard (`taxonomies.is_empty()`) is
    /// what makes this take effect: the NEXT sign-in's first post/posts-list
    /// visit re-fetches for real instead of silently reusing whatever the PRIOR
    /// session last saw.
    fn clear_vocab(self) {
        self.vocab_taxonomies.set(Vec::new());
        self.vocab_hierarchical_terms.set(HashMap::new());
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
    /// WordPress's "automatically add new top-level pages" flag for the open menu. Seeded from
    /// the load/save response (the single source), toggled by the editor, persisted on Save.
    auto_add: Signal<bool>,
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
    /// Drag-and-drop reorder/nest (rinch's pointer-synthesized element DnD). `drag` carries the
    /// dragged row's cid (its whole subtree moves); `drop_hint` is the hovered zone's key
    /// (`"gap:{cid}"` | `"into:{cid}"` | `"end"` | `""`) for the reactive drop-target highlight.
    drag: DragContext<String>,
    drop_hint: Signal<String>,
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
    /// The candidates checked for a BULK add (accumulates across searches within a tab; cleared on
    /// open/close/tab-switch so a Pages selection can't leak into Posts). "Add N selected" commits
    /// them all at once (dedup-guarded against items already in the tree).
    picker_selected: Signal<Vec<api::LinkCandidate>>,
    custom_url: Signal<String>,
    custom_label: Signal<String>,
    picker_err: Signal<String>,
    // Shared app signals.
    view: Signal<View>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
}

// The `Default` impls below exist ONLY to satisfy the rinch `#[component]` macro: it builds a
// component's props via `{ ..Default::default() }` (fully evaluated even when every field is
// overridden), so every prop TYPE must impl `Default`. `MenuLocationView` takes a `LocCtx` prop
// (which carries an `AuthCtx`), so both need it. Manual impls because `Signal<T>` has no `Default`
// (`#[derive(Default)]` can't be used); the default values are throwaway placeholders. `MenuCtx`
// itself is NOT a component prop (the row component `MenuRowView` takes discrete signals, and the
// location component takes `LocCtx`), so it needs no `Default` — F6.
impl Default for AuthCtx {
    fn default() -> Self {
        AuthCtx {
            view: Signal::new(View::Boot),
            me_user: Signal::new(None),
            login_error: Signal::new(String::new()),
            vocab_taxonomies: Signal::new(Vec::new()),
            vocab_hierarchical_terms: Signal::new(HashMap::new()),
        }
    }
}

/// The minimal signal bundle the Locations panel's `<select>` rows need: the menu list (the
/// options), the current assignments (the source of truth reloaded after an assign), a notice
/// sink, and the auth bundle for a 401. Passing THIS (4 signals) to `MenuLocationView` instead of
/// the whole 26-signal `MenuCtx` shrinks the throwaway signals the `#[component]`
/// `..Default::default()` allocates per render — rinch never frees signals (F6).
#[derive(Clone, Copy)]
struct LocCtx {
    list: Signal<Vec<api::MenuSummary>>,
    locations: Signal<Vec<api::MenuLocationRow>>,
    notice: Signal<String>,
    auth: AuthCtx,
}

impl Default for LocCtx {
    fn default() -> Self {
        LocCtx {
            list: Signal::new(Vec::new()),
            locations: Signal::new(Vec::new()),
            notice: Signal::new(String::new()),
            auth: AuthCtx::default(),
        }
    }
}

/// Load state for the shared taxonomy vocabulary (SF3/B4): distinguishes "still
/// fetching" from "fetch failed" from "loaded" so a slow/failed load never reads as
/// "no categories exist" — an empty Vec is genuinely ambiguous with a not-yet-loaded
/// one. Never gates the chips already assigned to the open post (SF3): those render
/// from `PostDetail.terms`/a save response regardless of this state.
#[derive(Clone, Copy, PartialEq, Default)]
enum VocabLoad {
    #[default]
    Loading,
    Ready,
    Error,
}

/// A pending new-tag chip (B9): flat taxonomies only, riding the save/preview
/// payload's `new_terms` — create-or-reuse happens server-side, atomically with the
/// rest of the save. `cid` is a client-only identity that outlives the chip's own
/// text and is what the `for` keys on (namespaced `s{cid}`, vs. an assigned term's
/// `t{id}` — SF7/B9: a tag literally named "42" can never collide with term id 42).
#[derive(Clone, PartialEq)]
struct StagedTag {
    cid: u64,
    taxonomy: String,
    name: String,
    /// Set when a save's 400 named this exact chip (its slug clashes with a term
    /// archive path — `posts.rs`'s inline-tag 409, SF13): renders `.chip--rejected`
    /// in place, not a global notice plus indistinguishable chips.
    rejected: bool,
}

/// The taxonomy vocabulary + one open post's live term-assignment state, bundled
/// (the `MenuCtx`/`AuthCtx` discipline — `Copy`, threads through helpers without
/// widening `open_post`'s signature further). Deviates from the original SF15b
/// framing ("never a `#[component]` prop"): a per-taxonomy `#[component]`
/// (`TaxonomyPanel`) turned out to be REQUIRED, not just an available option — a
/// reactive `if`/`for` nested inside the *outer* `for tax in taxonomies` loop,
/// referencing that loop's own item fields, hits a genuine rustc E0525 (the
/// generated `Fn` closure can only move a captured owned field out once, but a
/// nested reactive branch needs to do so on every re-run); the `MenuLocationView`/
/// `LocCtx` precedent — a small `Default`-bearing ctx passed as a **whole-panel**
/// prop, never a per-row one — is the established fix for exactly this shape. `Default`
/// (below) exists ONLY for the `#[component]` macro's `..Default::default()`
/// mechanism; SF15a's actual concern (no per-ROW `#[component]`, since a checklist
/// row is called tens of times in a tight loop) is untouched — `TaxonomyPanel` is
/// called once per TAXONOMY (1-2 times), not per row. Two lifetimes share this
/// struct:
///   * the VOCABULARY half (`taxonomies`/`hierarchical_terms`/`vocab_load`/
///     `vocab_gen`) is SESSION-scoped and shared app-wide (Q2) — loaded once, reused
///     by every post the editor opens. It is never reset by `open_post`/`new_post`.
///   * the ASSIGNMENT half (everything else) belongs to whichever document is
///     currently open — reseeded atomically by `open_post`/`new_post` (B5) and
///     otherwise mutated only by the panels' own interactions or a save's
///     authoritative reseed (B6).
#[derive(Clone, Copy)]
struct TermsCtx {
    // ── vocabulary (Q2, B4) ──
    taxonomies: Signal<Vec<api::TaxonomyDto>>,
    /// Full DFS term trees, HIERARCHICAL taxonomies only (keyed by `TaxonomyDto.key`)
    /// — what the checklist renders. A FLAT taxonomy's terms are never bulk-loaded
    /// here (only bounded suggestions — B2c exists precisely so the tag vocabulary
    /// is never dumped wholesale; see `suggestions`).
    hierarchical_terms: Signal<HashMap<String, Vec<api::TermDto>>>,
    vocab_load: Signal<VocabLoad>,
    /// Guards an in-flight vocab fetch against a slow/duplicate response — the
    /// `candidates_gen` idiom. Vocab has no document identity of its own (it's
    /// shared, not per-post), so it cannot reuse `editor_session`.
    vocab_gen: Signal<u64>,

    // ── this document's live assignment (B5, B6, SF1, SF3) ──
    /// Every assigned term id, pooled across taxonomies (mirrors how the server
    /// stores membership). `HashSet` so a row's `checked` closure is O(1) (SF15d).
    selected_ids: Signal<HashSet<u64>>,
    /// id -> display info for chips/checked rows, seeded from `PostDetail.terms`/a
    /// save response and NEVER derived from the vocabulary (SF3): a deleted- or
    /// renamed-away term still renders here, marked "(unavailable)".
    sidecar: Signal<HashMap<u64, api::TermRefDto>>,
    staged: Signal<Vec<StagedTag>>,
    next_staged_cid: Signal<u64>,
    /// The id set as of the last successful open/create/save — what
    /// [`api::dirty_terms`] compares the live `selected_ids` against (SF1).
    terms_snapshot: Signal<Vec<u64>>,
    /// Bumped by EVERY assignment mutation (checkbox, chip remove, tag commit) — the
    /// B6 in-flight-change detector: a save's completion merges rather than
    /// overwrites when this moved on while the request was in flight.
    terms_gen: Signal<u64>,

    // ── the tag token-input's transient UI state, keyed by taxonomy key (B7, Q5) ──
    tag_buffer: Signal<HashMap<String, String>>,
    /// Live suggestions for the currently-focused tag buffer (Q5's combobox
    /// contract) — a bounded server query (B2c's `q`+`limit`), never the whole tag
    /// vocabulary.
    suggestions: Signal<HashMap<String, Vec<api::TermDto>>>,
    /// Guards an in-flight suggestion fetch against a slow/out-of-order response.
    suggest_gen: Signal<u64>,
    /// The active (arrow-key-highlighted) suggestion index; absent = no option
    /// active (Enter then commits the typed buffer instead of accepting one).
    suggest_active: Signal<HashMap<String, usize>>,

    /// The category checklist's filter text, keyed by taxonomy key (owner call #1:
    /// ships in v1, unconditionally — not gated behind a term-count threshold).
    checklist_filter: Signal<HashMap<String, String>>,

    /// A term id a save's 400 just named as no-longer-existing (B10b) — the notice
    /// explains it and the NEXT click of the ordinary Save button already excludes
    /// it (the id is removed from `selected_ids`/`sidecar` as soon as this is set,
    /// so recovery is "read the notice, click Save again", never an auto-retry).
    missing_term_notice: Signal<Option<u64>>,

    auth: AuthCtx,
}

/// `Default` ONLY for the rinch `#[component]` macro's `..Default::default()`
/// mechanism (`TaxonomyPanel` takes this whole bundle as a prop) — every field is a
/// throwaway placeholder Signal, never actually read (the real call site always
/// supplies the live `terms` bundle explicitly).
impl Default for TermsCtx {
    fn default() -> Self {
        TermsCtx {
            taxonomies: Signal::new(Vec::new()),
            hierarchical_terms: Signal::new(HashMap::new()),
            vocab_load: Signal::new(VocabLoad::default()),
            vocab_gen: Signal::new(0),
            selected_ids: Signal::new(HashSet::new()),
            sidecar: Signal::new(HashMap::new()),
            staged: Signal::new(Vec::new()),
            next_staged_cid: Signal::new(0),
            terms_snapshot: Signal::new(Vec::new()),
            terms_gen: Signal::new(0),
            tag_buffer: Signal::new(HashMap::new()),
            suggestions: Signal::new(HashMap::new()),
            suggest_gen: Signal::new(0),
            suggest_active: Signal::new(HashMap::new()),
            checklist_filter: Signal::new(HashMap::new()),
            missing_term_notice: Signal::new(None),
            auth: AuthCtx::default(),
        }
    }
}

/// `View::Terms` (the taxonomy management list + switcher) and `View::TermEditor`
/// (the create/edit form) — a separate bundle from `TermsCtx` (which is the post
/// editor's OWN vocabulary + assignment state): this is the taxonomy MANAGEMENT
/// surface, with its own list (fetched WITH counts — `TermsCtx::hierarchical_terms`
/// is deliberately the `?counts=0` cheap mode the post editor's checklist needs, a
/// different request), its own create/edit form fields, and its own dirty/save
/// state (SF16). `terms: TermsCtx` rides along only so a term
/// create/update/delete can invalidate the shared vocabulary the post editor reads
/// (`invalidate_vocab`) — this view never touches `terms`'s OWN assignment fields.
#[derive(Clone, Copy)]
struct TermsViewCtx {
    // View::Terms — the switcher + list.
    /// The active taxonomy's KEY (which tab is showing). Never a raw index: the
    /// server-sorted taxonomy list is the source of truth, and a key survives a
    /// vocabulary reload that a positional index wouldn't.
    active_taxonomy: Signal<String>,
    list: Signal<Vec<api::TermDto>>,
    list_state: Signal<Load>,

    // View::TermEditor — the create/edit form. `edit_id: None` = create.
    edit_id: Signal<Option<u64>>,
    /// The taxonomy this term belongs to (fixed for the life of one editor visit —
    /// a term never moves taxonomies). Also determines the Parent field's presence
    /// (flat taxonomies have none).
    edit_taxonomy: Signal<String>,
    edit_name: Signal<String>,
    /// SF11: `None` on CREATE (empty field, "auto from name" placeholder, NOT
    /// controlled — matches the post/page slug idiom); `Some(current slug)` on
    /// EDIT (pre-filled, controlled — an edit's empty slug means "keep the
    /// current one" server-side, so the UI must show what that current one IS,
    /// never silently re-derive from a changed name).
    edit_slug: Signal<String>,
    edit_description: Signal<String>,
    edit_parent: Signal<Option<u64>>,
    /// SF12: `meta._rev` as loaded/last-saved, snapshotted and echoed back as
    /// `expected_rev` — the server 409s a stale save rather than silently
    /// reverting a rename/re-parent that happened elsewhere.
    edit_rev: Signal<i64>,
    saving: Signal<bool>,
    /// SF16: sibling to the menu editor's own `dirty`/beforeunload guard.
    dirty: Signal<bool>,

    terms: TermsCtx,
    view: Signal<View>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
}

/// The post editor's per-document signals, bundled so `open_post`/`new_post`/
/// `save_post`/`preview_post` take ONE parameter instead of nine-plus loose ones
/// (B5) — `terms` (the taxonomy half) rides along so every seed/save/reseed path
/// naturally includes it rather than needing a parallel, easy-to-forget parameter.
/// Not a `#[component]` prop, so (like `TermsCtx`) it needs no `Default`. Pages have
/// no taxonomies, so `open_page`/`new_page`/`save_page`/`preview_page` keep their
/// own existing individual-signal signatures.
#[derive(Clone, Copy)]
struct EditorCtx {
    editor: Signal<EditorHandle>,
    title: Signal<String>,
    slug: Signal<String>,
    status: Signal<String>,
    featured: Signal<Option<api::FeaturedMedia>>,
    current_id: Signal<Option<u64>>,
    editor_session: Signal<u64>,
    terms: TermsCtx,
    saving: Signal<bool>,
    notice: Signal<String>,
    toast: Signal<bool>,
    auth: AuthCtx,
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

    // The shared taxonomy vocabulary signals — hoisted above `auth` (SF17) so
    // `AuthCtx::session_expired` can clear them directly: a stale vocabulary
    // surviving a 401/logout would otherwise sit there non-empty, and
    // `load_vocab`'s idempotency guard (`taxonomies.is_empty()`) means it
    // would never re-fetch after the NEXT sign-in — a re-login could see
    // another era's categories/tags. `TermsCtx` below reuses these SAME
    // signal handles (never a second, competing copy).
    let vocab_taxonomies = Signal::new(Vec::<api::TaxonomyDto>::new());
    let vocab_hierarchical_terms = Signal::new(HashMap::<String, Vec<api::TermDto>>::new());

    // The re-auth routing bundle, shared by every guarded request.
    let auth = AuthCtx {
        view,
        me_user,
        login_error,
        vocab_taxonomies,
        vocab_hierarchical_terms,
    };

    // Taxonomy vocabulary + the open post's live category/tag assignment (Inc 3).
    // `terms.notice`/`terms.auth` alias the SAME `notice`/`auth` signals the rest of
    // the editor uses — one error banner, one re-auth path.
    let terms = TermsCtx {
        taxonomies: vocab_taxonomies,
        hierarchical_terms: vocab_hierarchical_terms,
        vocab_load: Signal::new(VocabLoad::Loading),
        vocab_gen: Signal::new(0u64),
        selected_ids: Signal::new(HashSet::<u64>::new()),
        sidecar: Signal::new(HashMap::<u64, api::TermRefDto>::new()),
        staged: Signal::new(Vec::<StagedTag>::new()),
        next_staged_cid: Signal::new(0u64),
        terms_snapshot: Signal::new(Vec::<u64>::new()),
        terms_gen: Signal::new(0u64),
        tag_buffer: Signal::new(HashMap::<String, String>::new()),
        suggestions: Signal::new(HashMap::<String, Vec<api::TermDto>>::new()),
        suggest_gen: Signal::new(0u64),
        suggest_active: Signal::new(HashMap::<String, usize>::new()),
        checklist_filter: Signal::new(HashMap::<String, String>::new()),
        missing_term_notice: Signal::new(Option::<u64>::None),
        auth,
    };
    let editor_ctx = EditorCtx {
        editor,
        title,
        slug,
        status,
        featured,
        current_id,
        editor_session,
        terms,
        saving,
        notice,
        toast,
        auth,
    };
    // Taxonomy MANAGEMENT (View::Terms/TermEditor) — a separate bundle from
    // `terms` (the post editor's own vocabulary + assignment state); see
    // `TermsViewCtx`'s own doc comment.
    let terms_view = TermsViewCtx {
        active_taxonomy: Signal::new(String::new()),
        list: Signal::new(Vec::<api::TermDto>::new()),
        list_state: Signal::new(Load::Loading),
        edit_id: Signal::new(Option::<u64>::None),
        edit_taxonomy: Signal::new(String::new()),
        edit_name: Signal::new(String::new()),
        edit_slug: Signal::new(String::new()),
        edit_description: Signal::new(String::new()),
        edit_parent: Signal::new(Option::<u64>::None),
        edit_rev: Signal::new(0i64),
        saving: Signal::new(false),
        dirty: Signal::new(false),
        terms,
        view,
        notice,
        toast,
        auth,
    };
    // The B7 keyboard mechanism: Enter/Backspace/Arrow/Escape for EVERY tag
    // token-input, from one document-level listener target-gated on
    // `data-fp-tagbuffer` (never `onkeydown:`/`onsubmit:` in rsx — rinch's event map
    // routes anything it doesn't recognize to the CLICK attribute, `data-rid`, so
    // those would silently become click handlers, not keyboard ones).
    install_tag_buffer_guard(terms);

    // Nav-menu editor (Editor+). One Copy bundle threads the many signals through the
    // two menu views + their helpers (the AuthCtx discipline).
    let menu = MenuCtx {
        list: Signal::new(Vec::<api::MenuSummary>::new()),
        list_state: Signal::new(Load::Loading),
        locations: Signal::new(Vec::<api::MenuLocationRow>::new()),
        edit_id: Signal::new(Option::<u64>::None),
        name: Signal::new(String::new()),
        slug: Signal::new(String::new()),
        auto_add: Signal::new(false),
        tree: Signal::new(Vec::<MenuRow>::new()),
        edits: Signal::new(HashMap::<String, RowEdit>::new()),
        next_cid: Signal::new(0u64),
        load: Signal::new(Load::Loading),
        saving: Signal::new(false),
        dirty: Signal::new(false),
        drag: DragContext::new(),
        drop_hint: Signal::new(String::new()),
        picker_open: Signal::new(false),
        candidates: Signal::new(api::LinkCandidates::default()),
        candidates_state: Signal::new(Load::Loading),
        candidates_gen: Signal::new(0u64),
        picker_query: Signal::new(String::new()),
        picker_tab: Signal::new(0u8),
        picker_selected: Signal::new(Vec::<api::LinkCandidate>::new()),
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
    // SF16 widens this to also cover an unsaved View::TermEditor.
    install_beforeunload_guard(
        menu.view,
        vec![
            (menu.dirty, View::MenuEditor),
            (terms_view.dirty, View::TermEditor),
        ],
    );
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
                                    EntityKind::Post => new_post(editor_ctx),
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
                                    onclick: move || open_posts_list(kind, posts, list_state, terms, notice, view, auth),
                                    "Posts"
                                }
                            }
                            // Menus + Categories are BOTH Editor+ (ManageMenus /
                            // ManageTerms — identical role sets), sharing one
                            // predicate (Q1). The server enforces regardless.
                            if is_editor_plus(&me_user.get()) {
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_menus(menu),
                                    "Menus"
                                }
                                button {
                                    class: "btn btn--quiet",
                                    onclick: move || open_terms_view(terms_view),
                                    "Categories"
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
                                                // SF17: the other session-end
                                                // vocab-clear path.
                                                auth.clear_vocab();
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
                                            move || open_post(id, editor_ctx)
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
                                    // B8: the panel is about to be torn down — best-
                                    // effort-commit any uncommitted tag text so it
                                    // isn't silently discarded (unlike Save, leaving
                                    // never BLOCKS on an invalid buffer: the user is
                                    // abandoning the edit, not persisting it).
                                    if matches!(kind.get(), EntityKind::Post) {
                                        let _ = commit_buffers_for_save(terms);
                                    }
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
                                    EntityKind::Post => preview_post(editor_ctx),
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
                                    EntityKind::Post => save_post(editor_ctx),
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
                        if matches!(kind.get(), EntityKind::Post) {
                            div { class: "editor__body",
                                div { class: "sheet",
                                    div { class: "sheet__inner",
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
                                div { class: "editor__below",
                                    // Zero taxonomies (migrate not run) renders no
                                    // panels at all — SF3: a genuinely empty
                                    // vocabulary is not an error state.
                                    for tax in terms.taxonomies.get() {
                                        TaxonomyPanel { key: tax.id, tax: tax, terms: terms }
                                    }
                                }
                            }
                        } else {
                            div { class: "sheet",
                                div { class: "sheet__inner",
                                    // The title is the headline set on the sheet (per the
                                    // mockup). Controlled like the slug field: the reactive
                                    // `value` reflects an `open_page`/`new_page` load and stays
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
                                MenuLocationView { key: row.location.clone(), row: row, lc: LocCtx { list: menu.list, locations: menu.locations, notice: menu.notice, auth: menu.auth } }
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
                            // WordPress's "Automatically add new top-level pages to this menu".
                            div { class: "menucfg",
                                label { class: "switch", title: "New top-level pages are added to this menu automatically",
                                    input {
                                        r#type: "checkbox", class: "menurow__newtab",
                                        checked: {move || menu.auto_add.get()},
                                        oninput: move |c: String| { if menu.saving.get() { return; } menu.auto_add.set(c == "true"); menu.dirty.set(true); },
                                    }
                                    span {
                                        class: {move || if menu.auto_add.get() { "switch__track is-on" } else { "switch__track" }},
                                        span { class: "switch__knob" }
                                    }
                                    span { class: "switch__text", "Automatically add new top-level pages to this menu" }
                                }
                            }
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
                                    // The insert-between gap is the FIRST child + carries the
                                    // for-item key (so it moves with its row); MenuRowView follows.
                                    DropGap {
                                        key: row.cid.clone(),
                                        cid: row.cid.clone(),
                                        tree: menu.tree,
                                        dirty: menu.dirty,
                                        saving: menu.saving,
                                        drag: menu.drag,
                                        drop_hint: menu.drop_hint,
                                    }
                                    MenuRowView {
                                        cid: row.cid.clone(),
                                        kind: row.kind.clone(),
                                        resolved: row.resolved.clone(),
                                        edits: menu.edits,
                                        tree: menu.tree,
                                        dirty: menu.dirty,
                                        saving: menu.saving,
                                        drag: menu.drag,
                                        drop_hint: menu.drop_hint,
                                    }
                                }
                                // The trailing drop zone: drop here to append at the top level.
                                // STATIC (after the `for`), so it needs no per-row key.
                                li {
                                    class: {move || if menu.drop_hint.get() == "end" { "dropgap dropgap--end is-hint" } else { "dropgap dropgap--end" }},
                                    ondragenter: move || menu.drop_hint.set("end".to_owned()),
                                    ondragleave: move || { if menu.drop_hint.get() == "end" { menu.drop_hint.set(String::new()); } },
                                    ondrop: move || apply_drop(menu.saving, menu.tree, menu.dirty, menu.drag, menu.drop_hint, DropDest::End),
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

                // ── TAXONOMY MANAGEMENT (THE VOCABULARY) — Inc 3 S3 ─────────────
                // Mirrors View::Menus (SF14d): "← Posts" back button, `masthead__here`
                // naming the active taxonomy, a Posts/Pages-toggle-style switcher pair,
                // and a primary "+ New {singular}" button. Every visible string comes
                // from `TaxonomyDto.label` — never the literal "Term"/"Terms" (SF14e).
                View::Terms => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || { terms_view.notice.set(String::new()); terms_view.view.set(View::List); },
                                "\u{2190} Posts"
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here",
                                {move || taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.active_taxonomy.get())}
                            }
                            span { class: "masthead__spacer" }
                            // One quiet button per OTHER taxonomy (the Posts/Pages
                            // toggle idiom, `app.rs:812-827`) — never the active one.
                            for tax in terms_view.terms.taxonomies.get() {
                                // A leading clone dedicated to the CONDITION itself —
                                // it reads a signal (`active_taxonomy`), so it is its
                                // own reactive closure site, distinct from the
                                // button's `onclick`/`key`/text sites below (the
                                // if/else-capture gotcha:
                                // `ferropress-rinch-if-else-capture-gotcha` memory).
                                let cond_key = tax.key.clone();
                                if cond_key != terms_view.active_taxonomy.get() {
                                    button {
                                        key: tax.key.clone(),
                                        class: "btn btn--quiet",
                                        onclick: { let k = tax.key.clone(); move || switch_taxonomy(terms_view, k.clone()) },
                                        {tax.label.clone()}
                                    }
                                }
                            }
                            button {
                                class: "btn btn--primary", style: "width:auto",
                                onclick: move || new_term_editor(terms_view),
                                {move || {
                                    let label = taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.active_taxonomy.get());
                                    format!("\u{002B} New {}", singularize(&label))
                                }}
                            }
                        }
                    }
                    div { class: "wrap",
                        div { class: "galley__head",
                            h2 {
                                id: "terms-heading", tabindex: "-1", class: "galley__title",
                                {move || taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.active_taxonomy.get())}
                            }
                            span { class: "galley__count",
                                {move || match terms_view.list_state.get() {
                                    Load::Ready => format!("{} total", terms_view.list.get().len()),
                                    _ => String::new(),
                                }}
                            }
                        }
                        if !terms_view.notice.get().is_empty() {
                            div { class: "galley__state err", role: "alert", {move || terms_view.notice.get()} }
                        }
                        if matches!(terms_view.list_state.get(), Load::Loading) {
                            div { class: "galley__state", "Loading\u{2026}" }
                        }
                        if matches!(terms_view.list_state.get(), Load::Error) {
                            div { class: "galley__state err", "This list is unavailable right now." }
                        }
                        // SF9: name the count column honestly — it is a DIRECT,
                        // published-only count (never "posts affected by deletion").
                        // SF14e: the taxonomy noun comes from the active tab, never
                        // a hardcoded "category" (a flat taxonomy also has no parent
                        // rollup to mention).
                        p { class: "panel__note",
                            {move || published_count_note(&terms_view.terms.taxonomies.get(), &terms_view.active_taxonomy.get())}
                        }
                        div { class: "galley__rows",
                            for row in term_row_vms(&terms_view.list.get()) {
                                button {
                                    key: row.id,
                                    class: "row", style: "grid-template-columns: 1fr auto auto",
                                    onclick: { let id = row.id; move || open_term_editor_by_id(terms_view, id) },
                                    span { style: row.indent,
                                        span { class: "row__title", {row.name} }
                                        span { class: "row__slug", {row.slug} }
                                    }
                                    span { class: "row__time", {row.count_label} }
                                    span { class: "row__edit", "Edit \u{2192}" }
                                }
                            }
                            if matches!(terms_view.list_state.get(), Load::Ready) && terms_view.list.get().is_empty() {
                                div {
                                    class: "galley__state",
                                    {move || format!(
                                        "No {} yet.",
                                        taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.active_taxonomy.get()).to_lowercase(),
                                    )}
                                }
                            }
                        }
                    }
                },

                // ── TERM EDITOR (CREATE / EDIT ONE CATEGORY OR TAG) ─────────────
                // `edit_id: None` = create (SF11: empty slug field, "auto from name"
                // placeholder); `Some` = edit (slug pre-filled + controlled — an edit's
                // empty slug means "keep the stored slug" server-side, so a cleared
                // field blocks Save rather than silently lying about what it shows).
                View::TermEditor => div {
                    header { class: "masthead",
                        div { class: "ruler" }
                        div { class: "masthead__bar",
                            button {
                                class: "btn btn--quiet",
                                onclick: move || leave_term_editor(terms_view),
                                {move || format!(
                                    "\u{2190} {}",
                                    taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.edit_taxonomy.get()),
                                )}
                            }
                            span { class: "masthead__sep", "\u{00B7}" }
                            span { class: "masthead__here",
                                {move || { let n = terms_view.edit_name.get(); if n.trim().is_empty() { "Untitled".to_owned() } else { n } }}
                            }
                            span { class: "masthead__spacer" }
                            button {
                                class: "btn btn--primary", style: "width:auto",
                                onclick: move || save_term(terms_view),
                                {move || if terms_view.saving.get() { "Saving\u{2026}" } else { "Save" }}
                            }
                        }
                    }
                    div { class: "wrap",
                        if !terms_view.notice.get().is_empty() {
                            div { class: "editor__error", role: "alert", {move || terms_view.notice.get()} }
                        }
                        div { class: "panel",
                            div { class: "setrow",
                                div { class: "setrow__label", "Name" }
                                div { class: "setrow__control",
                                    input {
                                        class: "input",
                                        value: {move || terms_view.edit_name.get()},
                                        oninput: move |v: String| { if terms_view.saving.get() { return; } terms_view.edit_name.set(v); terms_view.dirty.set(true); },
                                    }
                                }
                            }
                            div { class: "setrow",
                                div { class: "setrow__label", "Slug" }
                                div { class: "setrow__control",
                                    input {
                                        id: "term-editor-slug",
                                        class: "input input--mono",
                                        spellcheck: "false",
                                        placeholder: {move || if terms_view.edit_id.get().is_none() { "auto from name".to_owned() } else { String::new() }},
                                        value: {move || terms_view.edit_slug.get()},
                                        oninput: move |v: String| { if terms_view.saving.get() { return; } terms_view.edit_slug.set(v); terms_view.dirty.set(true); },
                                    }
                                    // Only meaningful once there IS an old URL to move
                                    // away from — a brand new term has none yet.
                                    if terms_view.edit_id.get().is_some() {
                                        p { class: "setrow__help",
                                            "Changing the slug moves this archive's URL immediately \u{2014} no redirect is recorded from the old one."
                                        }
                                    }
                                }
                            }
                            div { class: "setrow",
                                div { class: "setrow__label", "Description" }
                                div { class: "setrow__control",
                                    textarea {
                                        class: "input",
                                        value: {move || terms_view.edit_description.get()},
                                        oninput: move |v: String| { if terms_view.saving.get() { return; } terms_view.edit_description.set(v); terms_view.dirty.set(true); },
                                    }
                                }
                            }
                            // Flat taxonomies (tags) have no Parent field (SF10).
                            if term_editor_taxonomy_hierarchical(terms_view) {
                                div { class: "setrow",
                                    div { class: "setrow__label", "Parent" }
                                    div { class: "setrow__control",
                                        select {
                                            class: "select",
                                            oninput: move |v: String| {
                                                if terms_view.saving.get() { return; }
                                                terms_view.edit_parent.set(if v.is_empty() { None } else { v.parse::<u64>().ok() });
                                                terms_view.dirty.set(true);
                                            },
                                            for opt in term_parent_options(&terms_view.list.get(), terms_view.edit_id.get(), terms_view.edit_parent.get()) {
                                                option { key: opt.value.clone(), value: opt.value, selected: opt.selected, {opt.label} }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "savebar",
                            if terms_view.edit_id.get().is_some() {
                                button {
                                    class: "btn btn--danger",
                                    onclick: move || delete_term_clicked(terms_view),
                                    {move || format!(
                                        "Delete {}",
                                        singularize(&taxonomy_label(&terms_view.terms.taxonomies.get(), &terms_view.edit_taxonomy.get())),
                                    )}
                                }
                            }
                            if terms_view.dirty.get() {
                                span { class: "dirtydot", "\u{25CF} unsaved changes" }
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
                                button { class: {move || tab_class(menu.picker_tab.get(), 0)}, onclick: move || switch_picker_tab(menu, 0), "Pages" }
                                button { class: {move || tab_class(menu.picker_tab.get(), 1)}, onclick: move || switch_picker_tab(menu, 1), "Posts" }
                                button { class: {move || tab_class(menu.picker_tab.get(), 2)}, onclick: move || switch_picker_tab(menu, 2), "Custom link" }
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
                            // Pages/Posts: click a candidate to TOGGLE its checkbox (the picker
                            // stays open); "Add N selected" in the footer commits them all at once.
                            match menu.picker_tab.get() {
                                0 => div {
                                    for c in menu.candidates.get().pages {
                                        CandidateRow { key: c.id, cand: c, selected: menu.picker_selected, tree: menu.tree }
                                    }
                                    if matches!(menu.candidates_state.get(), Load::Ready) && menu.candidates.get().pages.is_empty() {
                                        div { class: "truncnote", "No matching pages." }
                                    }
                                },
                                1 => div {
                                    for c in menu.candidates.get().posts {
                                        CandidateRow { key: c.id, cand: c, selected: menu.picker_selected, tree: menu.tree }
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
                            // Bulk "Add to menu" for the Pages/Posts tabs (the Custom tab has its
                            // own "Add custom link" button in its form).
                            if menu.picker_tab.get() != 2 {
                                button {
                                    class: "btn btn--primary", style: "width:auto",
                                    onclick: move || add_selected_items(menu),
                                    {move || { let n = menu.picker_selected.get().len(); if n == 0 { "Add to menu".to_owned() } else { format!("Add {n} selected") } }}
                                }
                            }
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

            // SF16: role="status" so a save/delete outcome is announced — before this
            // the toast was a purely visual cue (a view switch also drops focus to
            // `document.body` with nothing spoken).
            div {
                class: {move || if toast.get() { "toast is-shown" } else { "toast" }},
                role: "status",
                span { class: "regmark", style: "font-size:.85rem", "\u{2295}" }
                "Saved"
            }
        }
    }
}

/// One taxonomy's assignment panel — a Categories checklist when `tax.hierarchical`,
/// else a Tags token-input. A `#[component]` (not inline in `app()`'s own rsx!)
/// despite SF15a's general preference against extra components: a reactive `if`/`for`
/// nested inside the OUTER `for tax in taxonomies` loop, referencing that loop's own
/// item fields, hits a genuine rustc E0525 (`Fn` vs `FnOnce` — the generated closure
/// can move an owned field out of its environment only once, but a nested reactive
/// branch needs to on every re-run). `tax`/`terms` becoming THIS function's own
/// parameters — available for its whole lifetime, freely re-borrowable by any nested
/// reactive closure — is the fix, exactly the `MenuLocationView`/`LocCtx` precedent:
/// a small `Default`-bearing ctx passed as a WHOLE-PANEL prop, never a per-row one.
/// Called once per TAXONOMY (1-2 times) — SF15a's actual concern (no per-ROW
/// component; a checklist row, rendered tens of times in the `for` below, stays
/// inline) is untouched.
#[component]
fn TaxonomyPanel(tax: api::TaxonomyDto, terms: TermsCtx) -> NodeHandle {
    // Split BEFORE `rsx!` even starts (plain Rust here, no macro-child
    // restrictions) into two INDEPENDENT clones, one per outer branch: both
    // the `if` and `else` branches below are constructed as VALUES (passed to
    // `show_dom` regardless of which condition holds), so a SHARED source
    // `tax` referenced from both — even just to shadow-clone it AGAIN inside
    // each branch — has them compete over moving the same struct (verified:
    // that's exactly what a same-named `let tax = tax.clone();` inside each
    // branch still did). `hier_tax`/`flat_tax` are each touched by exactly
    // ONE branch.
    let hier_tax = tax.clone();
    let flat_tax = tax.clone();
    rsx! {
        div { class: "panel",
            if tax.hierarchical {
                // `hier_tax` (this branch's own pre-split clone, see the
                // function's own doc comment above) shadowed to `tax` so
                // every leading-let below reads unchanged. Leading statements
                // of THIS if-branch's own body (rinch's rsx! only accepts a
                // leading `let` as a child of an if/for/match BODY, not of a
                // plain element).
                let tax = hier_tax.clone();
                let legend_text = tax.label.clone();
                let chip_label = panel_chipline_label(&tax);
                let filter_label_text = panel_filter_label(&tax);
                let filter_placeholder_text = panel_filter_placeholder(&tax);
                let key_for_chips = tax.key.clone();
                let key_for_filter_value = tax.key.clone();
                let key_for_filter_input = tax.key.clone();
                let loading_msg = panel_loading_text(&tax);
                let error_msg = panel_error_text(&tax);
                let key_for_empty_check = tax.key.clone();
                let empty_msg = panel_empty_text(&tax);
                let key_for_scroll_check = tax.key.clone();
                let key_for_scroll_iter = tax.key.clone();
                fieldset { class: "termcheck",
                    legend { class: "termcheck__legend", {{ let v = legend_text.clone(); move || v }} }
                    div { class: "chipline", aria-label: { let v = chip_label.clone(); move || v },
                        for chip in chip_vms(&terms.selected_ids.get(), &terms.hierarchical_terms.get(), &terms.sidecar.get(), &key_for_chips, true) {
                            span { key: chip.id, class: {if chip.oov { "chip chip--oov" } else { "chip" }},
                                {chip.name.clone()}
                                if chip.oov {
                                    small { "(unavailable)" }
                                }
                                button {
                                    class: "chip__x",
                                    aria-label: format!("Remove {}", chip.name),
                                    onclick: {
                                        let id = chip.id;
                                        move || {
                                            terms.selected_ids.update(|s| { s.remove(&id); });
                                            terms.terms_gen.update(|g| *g += 1);
                                        }
                                    },
                                    "\u{2715}"
                                }
                            }
                        }
                    }
                    div { class: "termcheck__search",
                        input {
                            class: "input", r#type: "search",
                            aria-label: { let v = filter_label_text.clone(); move || v },
                            placeholder: { let v = filter_placeholder_text.clone(); move || v },
                            value: {
                                let tax_key = key_for_filter_value.clone();
                                move || filter_text(terms, &tax_key)
                            },
                            oninput: {
                                let tax_key = key_for_filter_input.clone();
                                move |v: String| {
                                    terms.checklist_filter.update(|m| { m.insert(tax_key.clone(), v); });
                                }
                            },
                        }
                    }
                    // FOUR INDEPENDENT SIBLING `if`s (mutually exclusive via their
                    // own conditions), not an `if`/`else if` chain: rinch's `else
                    // if` codegen nests each subsequent branch inside an ADDITIONAL
                    // wrapper closure carrying the ENTIRE rest of the chain — so
                    // that wrapper alone would need `error_msg` AND
                    // `key_for_empty_check` AND `empty_msg` AND `key_for_scroll`
                    // ALL captured together, hitting the same rustc E0525 (`Fn` vs
                    // `FnOnce`) one level up. Four independent `if`s are each their
                    // OWN top-level `show_dom` call — no shared wrapper, so each
                    // needs only its own leading-let value (verified against
                    // rinch-macros/src/dom_codegen/control_flow.rs at the pinned
                    // rev — only `for`'s iter_expr gets auto-clone protection, and
                    // an `else if` wrapper gets none at all).
                    // Each branch ALSO re-clones its own leading-let value as
                    // ITS OWN first statement (`msg` below): the text
                    // interpolation's `create_effect` reconstructs its inner
                    // closure on every reactive fire, and THAT reconstruction
                    // needs a value it can freely move — not `loading_msg`
                    // itself, which this whole `if` branch only holds by
                    // `&self` (`show_dom`'s `Fn` bound) and can never move out
                    // of, even to itself, more than once. Same rule, one level
                    // deeper than the branch-level leading-lets above.
                    if matches!(terms.vocab_load.get(), VocabLoad::Loading) {
                        let msg = loading_msg.clone();
                        p { class: "panelstate", {{ let v = msg.clone(); move || v }} }
                    }
                    if matches!(terms.vocab_load.get(), VocabLoad::Error) {
                        let msg = error_msg.clone();
                        p { class: "panelerror", role: "alert", {{ let v = msg.clone(); move || v }} }
                    }
                    if matches!(terms.vocab_load.get(), VocabLoad::Ready) && checklist_is_empty(terms, &key_for_empty_check) {
                        let msg = empty_msg.clone();
                        p { class: "panelstate", {{ let v = msg.clone(); move || v }} }
                    }
                    if matches!(terms.vocab_load.get(), VocabLoad::Ready) && !checklist_is_empty(terms, &key_for_scroll_check) {
                        div { class: "termcheck__scroll",
                            for row in checklist_row_vms(&hier_vocab(terms, &key_for_scroll_iter), &filter_text(terms, &key_for_scroll_iter), &key_for_scroll_iter) {
                                label {
                                    key: row.id,
                                    class: {if row.is_match { "termcheck__row termcheck__row--match" } else { "termcheck__row termcheck__row--ctx" }},
                                    style: format!("padding-left:{}rem", 0.5 + row.depth as f32 * 1.15),
                                    input {
                                        r#type: "checkbox",
                                        checked: {
                                            let id = row.id;
                                            move || terms.selected_ids.get().contains(&id)
                                        },
                                        oninput: {
                                            let id = row.id;
                                            let name = row.name.clone();
                                            // `row.taxonomy` (the row's OWN
                                            // field, not a shared outer
                                            // clone): each row is its own
                                            // independently-owned `for`-item,
                                            // so this never competes with a
                                            // SIBLING row's own oninput
                                            // closure the way one shared
                                            // outer `String` would (verified
                                            // — that's exactly what happened
                                            // before this field existed).
                                            let tax_key = row.taxonomy.clone();
                                            move |_: String| {
                                                terms.selected_ids.update(|s| {
                                                    if !s.remove(&id) { s.insert(id); }
                                                });
                                                terms.sidecar.update(|m| {
                                                    m.insert(id, api::TermRefDto {
                                                        id, name: name.clone(), slug: String::new(), taxonomy: tax_key.clone(),
                                                    });
                                                });
                                                terms.terms_gen.update(|g| *g += 1);
                                            }
                                        },
                                    }
                                    span { class: "termcheck__name", {row.name.clone()} }
                                }
                            }
                        }
                    }
                    p { class: "panel__note",
                        "Ancestors of a match stay visible so the hierarchy still reads."
                    }
                }
            } else {
                // `flat_tax` (this branch's own pre-split clone) — see the
                // identical note on the Categories branch above.
                let tax = flat_tax.clone();
                let legend_text = tax.label.clone();
                let chip_label = panel_chipline_label(&tax);
                let tag_label_text = panel_tag_label(&tax);
                let tag_placeholder_text = panel_tag_placeholder(&tax);
                let key_for_databuffer = tax.key.clone();
                let key_for_buffer_value = tax.key.clone();
                let key_for_buffer_input = tax.key.clone();
                let key_for_suggest_check = tax.key.clone();
                let key_for_suggest_list = tax.key.clone();
                // SF16/#11 a11y: the combobox pattern's own three ARIA
                // attributes — each its own leading clone (the same rule as
                // every other reactive site on this element).
                let key_for_expanded = tax.key.clone();
                let key_for_controls = tax.key.clone();
                let key_for_activedescendant = tax.key.clone();
                fieldset { class: "termcheck",
                    legend { class: "termcheck__legend", {{ let v = legend_text.clone(); move || v }} }
                    div { class: "tokenfield",
                        div { class: "chipline", aria-label: { let v = chip_label.clone(); move || v },
                            for chip in chip_vms(&terms.selected_ids.get(), &terms.hierarchical_terms.get(), &terms.sidecar.get(), &key_for_databuffer, false) {
                                span { key: chip.id, class: "chip",
                                    {chip.name.clone()}
                                    button {
                                        class: "chip__x",
                                        aria-label: format!("Remove {}", chip.name),
                                        onclick: {
                                            let id = chip.id;
                                            move || {
                                                terms.selected_ids.update(|s| { s.remove(&id); });
                                                terms.terms_gen.update(|g| *g += 1);
                                            }
                                        },
                                        "\u{2715}"
                                    }
                                }
                            }
                            for staged in staged_for_taxonomy(&terms.staged.get(), &key_for_databuffer) {
                                span {
                                    key: format!("s{}", staged.cid),
                                    class: {if staged.rejected { "chip chip--rejected" } else { "chip chip--new" }},
                                    {staged.name.clone()}
                                    small { {if staged.rejected { "couldn't add \u{2014} try again" } else { "will be created" }} }
                                    button {
                                        class: "chip__x",
                                        aria-label: format!("Remove {}", staged.name),
                                        onclick: {
                                            let cid = staged.cid;
                                            move || {
                                                terms.staged.update(|v| v.retain(|s| s.cid != cid));
                                                terms.terms_gen.update(|g| *g += 1);
                                            }
                                        },
                                        "\u{2715}"
                                    }
                                }
                            }
                        }
                        input {
                            class: "input input--mono tokenfield__input",
                            role: "combobox",
                            data-fp-tagbuffer: { let v = key_for_databuffer.clone(); move || v },
                            aria-label: { let v = tag_label_text.clone(); move || v },
                            placeholder: { let v = tag_placeholder_text.clone(); move || v },
                            aria-expanded: {
                                let key = key_for_expanded.clone();
                                move || {
                                    let open = terms.suggestions.get().get(&key).is_some_and(|v| !v.is_empty());
                                    if open { "true".to_owned() } else { "false".to_owned() }
                                }
                            },
                            aria-controls: { let key = key_for_controls.clone(); move || suggest_listbox_id(&key) },
                            aria-activedescendant: {
                                let key = key_for_activedescendant.clone();
                                move || {
                                    let active = terms.suggest_active.get().get(&key).copied();
                                    active
                                        .and_then(|i| terms.suggestions.get().get(&key).and_then(|list| list.get(i).map(|t| t.id)))
                                        .map(suggest_option_id)
                                        .unwrap_or_default()
                                }
                            },
                            // Its own uniquely-named leading clone — `value:`'s
                            // `create_effect` directly captures whatever this
                            // bare closure references, so REUSING another
                            // site's clone here would recreate the exact
                            // multi-closure conflict this branch's doc comment
                            // describes (verified: it did, empirically).
                            value: { let key = key_for_buffer_value.clone(); move || buffer_text(terms, &key) },
                            oninput: {
                                let tax_key = key_for_buffer_input.clone();
                                move |v: String| {
                                    if let Some(comma_pos) = v.rfind(',') {
                                        let to_commit = v[..comma_pos].to_owned();
                                        let remainder = v[comma_pos + 1..].to_owned();
                                        commit_tag_text(terms, &tax_key, &to_commit);
                                        terms.tag_buffer.update(|m| { m.insert(tax_key.clone(), remainder.clone()); });
                                        load_suggestions(terms, tax_key.clone(), remainder, terms.auth);
                                    } else {
                                        terms.tag_buffer.update(|m| { m.insert(tax_key.clone(), v.clone()); });
                                        load_suggestions(terms, tax_key.clone(), v, terms.auth);
                                    }
                                }
                            },
                        }
                        // Two closures (the `if` condition + its body) each need
                        // their own clone — `key_for_suggest_check`/
                        // `key_for_suggest_list` (this branch's own leading
                        // statements, above) are exactly that; see the identical
                        // note on the Categories panel's vocab_load if/else chain.
                        if !terms.suggestions.get().get(&key_for_suggest_check).cloned().unwrap_or_default().is_empty() {
                            // This `if`'s own leading clones — separate from
                            // the condition above, and separate from EACH
                            // OTHER (the `id:` attribute's `create_effect` and
                            // the `for`'s own construction are two more
                            // distinct closures).
                            let key_for_suggest_id = key_for_suggest_list.clone();
                            let key_for_suggest_iter = key_for_suggest_list.clone();
                            ul { class: "combobox__list", role: "listbox", id: { let v = key_for_suggest_id.clone(); move || suggest_listbox_id(&v) },
                                for s in suggest_vms(&terms.suggestions.get().get(&key_for_suggest_iter).cloned().unwrap_or_default(), terms.suggest_active.get().get(&key_for_suggest_iter).copied(), &key_for_suggest_iter) {
                                    li {
                                        key: s.id,
                                        id: suggest_option_id(s.id),
                                        class: {if s.is_active { "combobox__opt is-active" } else { "combobox__opt" }},
                                        role: "option",
                                        aria-selected: {if s.is_active { "true" } else { "false" }},
                                        onclick: {
                                            let s_id = s.id;
                                            let s_name = s.name.clone();
                                            let s_slug = s.slug.clone();
                                            // `s.taxonomy` (the suggestion's
                                            // OWN field) — see the checklist
                                            // row's identical fix above.
                                            let tax_key = s.taxonomy.clone();
                                            move || {
                                                terms.selected_ids.update(|sel| { sel.insert(s_id); });
                                                terms.sidecar.update(|m| {
                                                    m.insert(s_id, api::TermRefDto { id: s_id, name: s_name.clone(), slug: s_slug.clone(), taxonomy: tax_key.clone() });
                                                });
                                                terms.tag_buffer.update(|m| { m.remove(&tax_key); });
                                                terms.suggestions.update(|m| { m.remove(&tax_key); });
                                                terms.terms_gen.update(|g| *g += 1);
                                            }
                                        },
                                        {s.name.clone()}
                                        span { class: "combobox__hint", "Enter to add" }
                                    }
                                }
                            }
                        }
                    }
                    p { class: "panel__note",
                        "Comma or Enter commits the buffer; unmatched text stages a chip that's created when you save."
                    }
                }
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

/// Fetch the shared taxonomy vocabulary into `terms`: every taxonomy, plus each
/// HIERARCHICAL one's full term tree (`?counts=0` — the panels never show the live
/// count). Idempotent (skips when already loaded OR already loading — the
/// `load_templates` discipline, widened with a Loading check since this fires from
/// three independent entry points) and `vocab_gen`-guarded against a slow response
/// racing a later call. Fired from `open_posts_list` and idempotently from
/// `open_post`/`new_post` (Q2) — eager, not lazy-on-panel-expand, so the
/// always-visible checklist never races its own seed. A FLAT taxonomy's terms are
/// deliberately never bulk-loaded here (see [`TermsCtx::hierarchical_terms`]).
/// SF17: after ANY failed save, invalidate the shared vocabulary so a staged chip
/// that turns out to have already become real (a partial `apply_terms` failure can
/// leave an earlier tag created-but-unassigned) re-renders solid on the next load,
/// and View::Terms (S3) shows the truth. Resets `taxonomies` to empty — the SAME
/// signal `load_vocab`'s own idempotency check reads — so the very next call
/// (`open_post`/`new_post`/`open_posts_list`) re-fetches for real, not a no-op.
fn invalidate_vocab(terms: TermsCtx) {
    terms.taxonomies.set(Vec::new());
    terms.hierarchical_terms.set(HashMap::new());
}

fn load_vocab(terms: TermsCtx, auth: AuthCtx) {
    // `VocabLoad::Loading` is ALSO the enum's `#[default]` — i.e. the signal's
    // own INITIAL value, not solely an "already fetching" flag — so gating on
    // it here would make this fn's very FIRST-EVER call see "Loading" (from
    // nothing having run yet, not from an in-flight fetch) and return without
    // ever fetching (a real bug this once was). `taxonomies.get().is_empty()`
    // alone is the correct idempotency guard: a rare, harmless duplicate
    // concurrent fetch (e.g. `open_posts_list` and `new_post` firing in the
    // same tick) just costs one extra request — `vocab_gen` (below) already
    // ensures only the LATEST response is ever applied.
    if !terms.taxonomies.get().is_empty() {
        return;
    }
    terms.vocab_load.set(VocabLoad::Loading);
    let load_gen = terms.vocab_gen.get() + 1;
    terms.vocab_gen.set(load_gen);
    spawn_local(async move {
        let taxonomies = match api::list_taxonomies().await {
            Ok(v) => v,
            Err(api::ApiError::Unauthorized) => {
                auth.session_expired();
                return;
            }
            Err(api::ApiError::Message(_)) => {
                if terms.vocab_gen.get() == load_gen {
                    terms.vocab_load.set(VocabLoad::Error);
                }
                return;
            }
        };
        let mut trees: HashMap<String, Vec<api::TermDto>> = HashMap::new();
        for tax in taxonomies.iter().filter(|t| t.hierarchical) {
            match api::list_terms(&tax.key, false, None, None).await {
                Ok(resp) => {
                    trees.insert(tax.key.clone(), resp.terms);
                }
                Err(api::ApiError::Unauthorized) => {
                    auth.session_expired();
                    return;
                }
                Err(api::ApiError::Message(_)) => {
                    if terms.vocab_gen.get() == load_gen {
                        terms.vocab_load.set(VocabLoad::Error);
                    }
                    return;
                }
            }
        }
        // A later `load_vocab` call (e.g. a fast Posts-list -> new-post sequence)
        // already superseded this one — never let a slow response win.
        if terms.vocab_gen.get() != load_gen {
            return;
        }
        terms.taxonomies.set(taxonomies);
        terms.hierarchical_terms.set(trees);
        terms.vocab_load.set(VocabLoad::Ready);
    });
}

/// Fetch bounded tag suggestions for `taxonomy`'s current buffer text (Q5's combobox
/// contract; B2c's `q`+`limit` — never the whole tag vocabulary). An empty buffer
/// clears the suggestion list outright rather than fetching (matches the mockup's
/// "no matching entry -> no list at all" rule at the boundary). Guarded by
/// `suggest_gen` against a slow/out-of-order response, not a timer — a fast typist's
/// intermediate keystrokes each fire a request, but only the LAST one's response is
/// ever applied.
fn load_suggestions(terms: TermsCtx, taxonomy: String, buffer: String, auth: AuthCtx) {
    let needle = buffer.trim().to_owned();
    if needle.is_empty() {
        terms.suggestions.update(|m| {
            m.remove(&taxonomy);
        });
        terms.suggest_active.update(|m| {
            m.remove(&taxonomy);
        });
        return;
    }
    let fetch_gen = terms.suggest_gen.get() + 1;
    terms.suggest_gen.set(fetch_gen);
    let tax_key = taxonomy;
    spawn_local(async move {
        match api::list_terms(&tax_key, false, Some(&needle), Some(8)).await {
            Ok(resp) => {
                if terms.suggest_gen.get() != fetch_gen {
                    return;
                }
                // A comma-commit can race this SAME fetch and stage a chip
                // for a term that, per THIS response, already exists — the
                // server's own reuse-vs-create decision (`apply_terms`'s
                // DECIDE step, re-read fresh under `taxonomy_lock` at save
                // time) already reuses it regardless, so this is cosmetic,
                // never data-lossy; but a chip lying "will be created" about
                // a term that already exists is worth correcting the
                // instant we learn better, not just at reseed-after-save.
                // Mirrors `commit_one_tag`'s own match rule exactly: BOTH
                // sides re-slugified from their name, never `t.slug` read
                // directly.
                let staged_now = terms.staged.get();
                for t in &resp.terms {
                    let Some(t_slug) = ferropress_core::slugify(&t.name) else {
                        continue;
                    };
                    let matched = staged_now.iter().any(|s| {
                        s.taxonomy == tax_key
                            && ferropress_core::slugify(&s.name).as_deref() == Some(t_slug.as_str())
                    });
                    if !matched {
                        continue;
                    }
                    let promoted_id = t.id;
                    let promoted_name = t.name.clone();
                    let promoted_slug = t.slug.clone();
                    let tax_for_sidecar = tax_key.clone();
                    let t_slug_for_retain = t_slug.clone();
                    terms.staged.update(|v| {
                        v.retain(|s| {
                            !(s.taxonomy == tax_key
                                && ferropress_core::slugify(&s.name).as_deref()
                                    == Some(t_slug_for_retain.as_str()))
                        });
                    });
                    terms.selected_ids.update(|s| {
                        s.insert(promoted_id);
                    });
                    terms.sidecar.update(|m| {
                        m.insert(
                            promoted_id,
                            api::TermRefDto {
                                id: promoted_id,
                                name: promoted_name,
                                slug: promoted_slug,
                                taxonomy: tax_for_sidecar,
                            },
                        );
                    });
                    terms.terms_gen.update(|g| *g += 1);
                }
                terms.suggestions.update(|m| {
                    m.insert(tax_key.clone(), resp.terms);
                });
                terms.suggest_active.update(|m| {
                    m.remove(&tax_key);
                });
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(_)) => {
                if terms.suggest_gen.get() == fetch_gen {
                    terms.suggestions.update(|m| {
                        m.remove(&tax_key);
                    });
                }
            }
        }
    });
}

/// The checklist rows to render for a hierarchical taxonomy: every term matching
/// `filter` (case-insensitive substring on the name), PLUS every ancestor of a match
/// (so the hierarchy still reads — an orphaned child with no visible parent would be
/// confusing) — same tree DFS order as `vocab` itself, just narrowed. An empty
/// filter matches everything. A PURE function of the vocabulary + filter text ONLY
/// (SF15c) — never `selected_ids`, so a checkbox toggle never touches this list or
/// the keyed `for` it drives.
fn checklist_rows(vocab: &[api::TermDto], filter: &str) -> Vec<api::TermDto> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return vocab.to_vec();
    }
    let by_id: HashMap<u64, &api::TermDto> = vocab.iter().map(|t| (t.id, t)).collect();
    let matched: Vec<u64> = vocab
        .iter()
        .filter(|t| t.name.to_lowercase().contains(&needle))
        .map(|t| t.id)
        .collect();
    let mut keep: HashSet<u64> = matched.iter().copied().collect();
    for id in &matched {
        let mut cur = by_id.get(id).and_then(|t| t.parent);
        while let Some(pid) = cur {
            if !keep.insert(pid) {
                break; // already kept (a shared ancestor chain) — no need to re-walk
            }
            cur = by_id.get(&pid).and_then(|t| t.parent);
        }
    }
    vocab
        .iter()
        .filter(|t| keep.contains(&t.id))
        .cloned()
        .collect()
}

/// Every selected id that belongs to `taxonomy`, sorted for a stable chip order:
/// found either in that taxonomy's loaded vocabulary (hierarchical taxonomies only —
/// a flat taxonomy's vocabulary is never bulk-loaded, see
/// [`TermsCtx::hierarchical_terms`]) or, failing that, in the sidecar (SF3 — an
/// out-of-vocab assignment still renders, attributed by the sidecar's OWN `taxonomy`
/// field, always set correctly at seed/commit time regardless of whether the
/// vocabulary happens to be loaded).
fn chip_ids_for_taxonomy(
    selected: &HashSet<u64>,
    hierarchical_terms: &HashMap<String, Vec<api::TermDto>>,
    sidecar: &HashMap<u64, api::TermRefDto>,
    taxonomy: &str,
) -> Vec<u64> {
    let in_vocab: HashSet<u64> = hierarchical_terms
        .get(taxonomy)
        .map(|v| v.iter().map(|t| t.id).collect())
        .unwrap_or_default();
    let mut ids: Vec<u64> = selected
        .iter()
        .copied()
        .filter(|id| {
            in_vocab.contains(id) || sidecar.get(id).map(|t| t.taxonomy.as_str()) == Some(taxonomy)
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Panel copy that reads `tax.label` (`format!` directly against `tax.label` inside
/// a taxonomy-loop body that ALSO has reactive `if`/`for` siblings makes rustc
/// capture the field itself into the generated `Fn` closure by value — fine on the
/// first call, `E0525` on the second. Routing every such string through a helper
/// that takes `&TaxonomyDto` makes the closure capture (and re-borrow) the WHOLE
/// struct instead, which is `Fn`-safe — this file's only reason these exist.
fn panel_chipline_label(tax: &api::TaxonomyDto) -> String {
    format!("Assigned {}", tax.label.to_lowercase())
}
fn panel_filter_label(tax: &api::TaxonomyDto) -> String {
    format!("Filter {}", tax.label.to_lowercase())
}
fn panel_filter_placeholder(tax: &api::TaxonomyDto) -> String {
    format!("Filter {}\u{2026}", tax.label.to_lowercase())
}
fn panel_loading_text(tax: &api::TaxonomyDto) -> String {
    format!("Loading {}\u{2026}", tax.label.to_lowercase())
}
fn panel_error_text(tax: &api::TaxonomyDto) -> String {
    format!(
        "Couldn't load {} \u{2014} your existing assignments are preserved.",
        tax.label.to_lowercase()
    )
}
fn panel_empty_text(tax: &api::TaxonomyDto) -> String {
    format!("No {} exist yet.", tax.label.to_lowercase())
}
fn panel_tag_label(tax: &api::TaxonomyDto) -> String {
    format!("Add a {}", singularize(&tax.label))
}
fn panel_tag_placeholder(tax: &api::TaxonomyDto) -> String {
    format!("Add a {}\u{2026}", singularize(&tax.label))
}

/// The tag-suggestion listbox's id, keyed by taxonomy — shared by the
/// listbox's own `id:` and the combobox `<input>`'s `aria-controls` (#11).
fn suggest_listbox_id(taxonomy_key: &str) -> String {
    format!("suggest-{taxonomy_key}")
}

/// One suggestion option's id — shared by the `<li role="option">`'s own
/// `id:` and the combobox `<input>`'s `aria-activedescendant` (#11), so a
/// screen reader announces which option is virtually focused as arrow keys
/// move `suggest_active` without moving the DOM focus off the input.
fn suggest_option_id(term_id: u64) -> String {
    format!("suggest-opt-{term_id}")
}

/// `terms.hierarchical_terms.get().get(key)`, owned and defaulted — a small direct
/// accessor to keep the rsx call sites (`for row in hier_vocab(terms, &tax.key)`,
/// the `parent_options`-style precedent) free of intermediate `let`s.
fn hier_vocab(terms: TermsCtx, key: &str) -> Vec<api::TermDto> {
    terms
        .hierarchical_terms
        .get()
        .get(key)
        .cloned()
        .unwrap_or_default()
}

fn filter_text(terms: TermsCtx, key: &str) -> String {
    terms
        .checklist_filter
        .get()
        .get(key)
        .cloned()
        .unwrap_or_default()
}

fn buffer_text(terms: TermsCtx, key: &str) -> String {
    terms.tag_buffer.get().get(key).cloned().unwrap_or_default()
}

/// One rendered chip (SF15c's "view-model precompute" idiom, mirroring `row_vms`):
/// the `for`'s item is a plain data struct, never a bare block, so its body needs no
/// intermediate `let`s. `oov` is only ever true for a hierarchical taxonomy's own
/// chip line (a flat taxonomy's assigned chips are never marked out-of-vocab — its
/// vocabulary is never bulk-loaded, so there is nothing to check against; see
/// [`chip_ids_for_taxonomy`]).
#[derive(Clone, PartialEq)]
struct ChipVm {
    id: u64,
    name: String,
    oov: bool,
}

fn chip_vms(
    selected: &HashSet<u64>,
    hierarchical_terms: &HashMap<String, Vec<api::TermDto>>,
    sidecar: &HashMap<u64, api::TermRefDto>,
    taxonomy: &str,
    hierarchical: bool,
) -> Vec<ChipVm> {
    let in_vocab: HashSet<u64> = hierarchical_terms
        .get(taxonomy)
        .map(|v| v.iter().map(|t| t.id).collect())
        .unwrap_or_default();
    chip_ids_for_taxonomy(selected, hierarchical_terms, sidecar, taxonomy)
        .into_iter()
        .map(|id| {
            let name = sidecar
                .get(&id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| format!("Term #{id}"));
            ChipVm {
                id,
                name,
                oov: hierarchical && !in_vocab.contains(&id),
            }
        })
        .collect()
}

/// Whether the (filtered) checklist for `taxonomy` has no rows to show — a single
/// function call so a reactive `else if` condition referencing it only touches
/// `taxonomy` once (rinch's `if`/`else if` codegen has no auto-clone protection for
/// a captured field referenced from multiple places within one condition/branch —
/// see the `TaxonomyPanel` doc comment).
fn checklist_is_empty(terms: TermsCtx, taxonomy: &str) -> bool {
    checklist_row_vms(
        &hier_vocab(terms, taxonomy),
        &filter_text(terms, taxonomy),
        taxonomy,
    )
    .is_empty()
}

/// One checklist row, ready to render (SF15c: still just `TermDto`'s own fields plus
/// one filter-derived `bool` — never anything interaction-derived). Carries its OWN
/// `taxonomy` copy (not read from an outer capture): each `for`-item is its own
/// independently-owned value, so a per-row closure (`oninput`) reading `row.taxonomy`
/// never competes with a SIBLING row's own closure the way a shared outer `String`
/// would (verified — that's exactly what happened before this field existed; rinch's
/// `for` only auto-clone-protects its OWN iter_expr, not values a nested per-item
/// closure references some other way).
#[derive(Clone, PartialEq)]
struct ChecklistRowVm {
    id: u64,
    name: String,
    is_match: bool,
    depth: usize,
    taxonomy: String,
}

fn checklist_row_vms(vocab: &[api::TermDto], filter: &str, taxonomy: &str) -> Vec<ChecklistRowVm> {
    checklist_rows(vocab, filter)
        .into_iter()
        .map(|t| ChecklistRowVm {
            id: t.id,
            is_match: checklist_row_is_match(&t, filter),
            depth: t.depth,
            name: t.name,
            taxonomy: taxonomy.to_owned(),
        })
        .collect()
}

/// One tag suggestion, ready to render. Carries its own `taxonomy` copy — see
/// `ChecklistRowVm`'s identical doc comment.
#[derive(Clone, PartialEq)]
struct SuggestVm {
    id: u64,
    name: String,
    slug: String,
    is_active: bool,
    taxonomy: String,
}

fn suggest_vms(list: &[api::TermDto], active: Option<usize>, taxonomy: &str) -> Vec<SuggestVm> {
    list.iter()
        .enumerate()
        .map(|(i, t)| SuggestVm {
            id: t.id,
            name: t.name.clone(),
            slug: t.slug.clone(),
            is_active: active == Some(i),
            taxonomy: taxonomy.to_owned(),
        })
        .collect()
}

/// Every staged (pending-create) chip belonging to `taxonomy`, in insertion order.
fn staged_for_taxonomy(staged: &[StagedTag], taxonomy: &str) -> Vec<StagedTag> {
    staged
        .iter()
        .filter(|s| s.taxonomy == taxonomy)
        .cloned()
        .collect()
}

/// Whether `term`'s name/slug itself matches `filter` (as opposed to being kept
/// in-view only as an ancestor of a match) — recomputed per-row from the SAME
/// predicate [`checklist_rows`] used to build the list, rather than threading a
/// derived flag through the `for` item (SF15c: the item stays `TermDto` alone).
fn checklist_row_is_match(term: &api::TermDto, filter: &str) -> bool {
    let needle = filter.trim().to_lowercase();
    needle.is_empty() || term.name.to_lowercase().contains(&needle)
}

/// A crude English singular for a taxonomy label ("Tags" -> "tag") — used only for
/// the token-input's "Add a {…}" placeholder/aria-label. Strips a trailing "es" or
/// "s" and lowercases; falls back to the label verbatim (lowercased) when neither
/// suffix is present. Good enough for the pinned v1 vocabulary ("Tags"); a taxonomy
/// whose plural this mangles just gets a slightly odd placeholder, never a wrong or
/// unsafe one.
fn singularize(label: &str) -> String {
    let lower = label.to_lowercase();
    if let Some(stem) = lower.strip_suffix("ies") {
        // "Categories" -> "category" (this taxonomy's OWN label, not just an
        // edge case) — stripping only "es" left a bare, wrong "categori".
        format!("{stem}y")
    } else if let Some(stem) = lower.strip_suffix("es") {
        stem.to_owned()
    } else if let Some(stem) = lower.strip_suffix('s') {
        stem.to_owned()
    } else {
        lower
    }
}

/// Mirrors `ferropress-http::admin::terms::RESERVED_TERM_SLUGS`. Kept as a small,
/// manually-synced duplicate rather than a core promotion (unlike `slugify`): this
/// is a client-side FAIL-FAST hint, not a decision the two sides must ever agree on
/// bit-for-bit — the server remains the sole authority (SF13's error taxonomy covers
/// the 400 this is purely trying to avoid round-tripping for).
fn is_reserved_term_slug(slug: &str) -> bool {
    matches!(slug, "page" | "feed")
}

/// Commit one tag-buffer SEGMENT (already comma-split) as a chip: a slug match
/// against the taxonomy's current (already server-fetched) suggestion list reuses
/// that term's REAL id (SF2 — never a staged/dashed chip for something that already
/// exists); anything else stages a new chip, deduped case-insensitively by slug
/// against what's already staged (B9). Silently no-ops when `raw` slugifies to
/// nothing (a stray comma, or an all-symbol scrap) — that is normal typing noise
/// here, not the save-blocking condition (see [`commit_buffers_for_save`]).
fn commit_one_tag(terms: TermsCtx, taxonomy: &str, raw: &str) {
    let Some(slug) = ferropress_core::slugify(raw) else {
        return;
    };
    let reuse = terms
        .suggestions
        .get()
        .get(taxonomy)
        .and_then(|list| {
            list.iter()
                .find(|t| ferropress_core::slugify(&t.name).as_deref() == Some(slug.as_str()))
        })
        .cloned();
    match reuse {
        Some(t) => {
            terms.selected_ids.update(|s| {
                s.insert(t.id);
            });
            terms.sidecar.update(|m| {
                m.insert(
                    t.id,
                    api::TermRefDto {
                        id: t.id,
                        name: t.name,
                        slug: t.slug,
                        taxonomy: taxonomy.to_owned(),
                    },
                );
            });
        }
        None => {
            let already_staged = terms.staged.get().iter().any(|s| {
                s.taxonomy == taxonomy
                    && ferropress_core::slugify(&s.name).as_deref() == Some(slug.as_str())
            });
            if !already_staged {
                let cid = terms.next_staged_cid.get();
                terms.next_staged_cid.set(cid + 1);
                terms.staged.update(|v| {
                    v.push(StagedTag {
                        cid,
                        taxonomy: taxonomy.to_owned(),
                        name: raw.trim().to_owned(),
                        rejected: false,
                    })
                });
            }
        }
    }
    terms.terms_gen.update(|g| *g += 1);
}

/// Comma-splits `text` and commits each segment through [`commit_one_tag`] — the
/// B7(i) live-typing path (`oninput` scans the buffer for `,`) and the shared tail
/// of an Enter/blur/save commit alike.
fn commit_tag_text(terms: TermsCtx, taxonomy: &str, text: &str) {
    for segment in text.split(',') {
        commit_one_tag(terms, taxonomy, segment.trim());
    }
}

/// B8: commit EVERY taxonomy's uncommitted tag buffer before a save/preview
/// snapshot (or before a view switch tears the panel down) — the same path an
/// in-progress Enter would take, so unsent text is never silently discarded. First
/// validates every non-empty buffer (must slugify to something, must not land on a
/// reserved term slug); on the FIRST failure, returns its message and commits
/// NOTHING (so a later-failing buffer can't have already mutated `staged` by the
/// time the caller aborts the save). Only once every buffer passes does it actually
/// commit them all and clear.
fn commit_buffers_for_save(terms: TermsCtx) -> Result<(), String> {
    let buffers = terms.tag_buffer.get();
    for text in buffers.values() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        match ferropress_core::slugify(trimmed) {
            None => {
                return Err(format!(
                    "\u{201C}{trimmed}\u{201D} isn't a usable tag \u{2014} clear it or finish typing before saving."
                ));
            }
            Some(slug) if is_reserved_term_slug(&slug) => {
                return Err(format!(
                    "\u{201C}{trimmed}\u{201D} can't be used as a tag name \u{2014} clear it or finish typing before saving."
                ));
            }
            Some(_) => {}
        }
    }
    for (taxonomy, text) in buffers {
        if !text.trim().is_empty() {
            commit_tag_text(terms, &taxonomy, &text);
        }
    }
    terms.tag_buffer.set(HashMap::new());
    Ok(())
}

/// The B6 post-save reseed: applies the server's AUTHORITATIVE term list after a
/// save/preview/create completes (the caller has already dropped a STALE response —
/// a different document now open — before calling this). When `terms_gen_moved` is
/// true (the user ticked another box or committed another chip while the request was
/// in flight): MERGE — union the response's real ids into the live selection (a
/// concurrent uncheck during the flight is preserved, since this only ADDS) and drop
/// only the staged entries that were PART OF THIS SAVE (`sent_staged`) and whose slug
/// the response confirms is now real, leaving anything typed during the flight
/// alone. Otherwise (the common case, no interleaved edit): reseed wholesale from the
/// response. Either way, `sidecar`/`terms_snapshot` always become the response's own
/// (they describe "what IS assigned now", never a merge target).
fn reseed_terms(
    terms: TermsCtx,
    terms_gen_moved: bool,
    sent_staged: &[StagedTag],
    resp: &[api::TermRefDto],
) {
    let resp_ids: HashSet<u64> = resp.iter().map(|t| t.id).collect();
    let resp_slugs: HashSet<String> = resp
        .iter()
        .filter_map(|t| ferropress_core::slugify(&t.name))
        .collect();
    if terms_gen_moved {
        terms.selected_ids.update(|s| {
            s.extend(resp_ids.iter().copied());
        });
        let sent_cids: HashSet<u64> = sent_staged.iter().map(|s| s.cid).collect();
        terms.staged.update(|v| {
            v.retain(|t| {
                !sent_cids.contains(&t.cid)
                    || !ferropress_core::slugify(&t.name)
                        .is_some_and(|slug| resp_slugs.contains(&slug))
            });
        });
    } else {
        terms.selected_ids.set(resp_ids.clone());
        let sent_cids: HashSet<u64> = sent_staged.iter().map(|s| s.cid).collect();
        terms
            .staged
            .update(|v| v.retain(|t| !sent_cids.contains(&t.cid)));
    }
    terms.sidecar.update(|m| {
        for t in resp {
            m.insert(t.id, t.clone());
        }
    });
    terms.terms_snapshot.set(resp_ids.into_iter().collect());
    terms.missing_term_notice.set(None);
}

/// Extracts the term id from the server's `"term {id} does not exist"` 400 (B10b) —
/// the ONE error shape this needs to recognize to offer the removed-it-for-you
/// recovery. Any other message (including the sibling "term {id} has no taxonomy" —
/// genuinely corrupt data with no client-side fix) surfaces verbatim, no special
/// handling.
fn missing_term_id(message: &str) -> Option<u64> {
    message
        .strip_prefix("term ")
        .and_then(|rest| rest.strip_suffix(" does not exist"))
        .and_then(|id_str| id_str.parse().ok())
}

/// SF13: the inline-tag archive-path 409 (`ferropress-http::admin::posts::plan_terms`,
/// `the archive path "{taxonomy}/{slug}" is already used by a post or page`) names
/// the offending SLUG — extract it so the failure can be mapped back to ONE staged
/// chip and marked rejected in place, distinct from `missing_term_id`'s "term N does
/// not exist" shape (a different failure entirely: this one names a slug, that one
/// an id).
fn rejected_tag_slug(message: &str) -> Option<String> {
    let rest = message.strip_prefix("the archive path \"")?;
    let path = rest.split('"').next()?;
    path.rsplit_once('/').map(|(_, slug)| slug.to_owned())
}

// SF13/#12: this 409's live trigger needs a page WHOSE OWN materialized path
// happens to collide with a NOT-YET-EXISTING tag's archive path — real but
// awkward to stage end-to-end (a nested page under a "tag"-slugged parent,
// then an inline-tag save naming exactly that child's slug). The parser
// itself is a pure function, covered directly here per the design ruling's
// stated fallback ("cover the code path with a unit test... say so
// explicitly") — `mark_rejected_staged_tag`'s own body is a single `Vec`
// iteration + slug compare, verified by reading rather than by a live 409.
#[cfg(test)]
mod tests {
    use super::rejected_tag_slug;

    #[test]
    fn rejected_tag_slug_extracts_the_slug_from_the_real_server_message() {
        let msg = "the archive path \"tag/sea-glass\" is already used by a post or page";
        assert_eq!(rejected_tag_slug(msg).as_deref(), Some("sea-glass"));
    }

    #[test]
    fn rejected_tag_slug_handles_a_hierarchical_taxonomy_key_too() {
        // Not the tags-only inline path in practice, but the parser itself
        // doesn't assume a specific taxonomy key — only the trailing
        // slash-segment.
        let msg = "the archive path \"category/sea-stories\" is already used by a post or page";
        assert_eq!(rejected_tag_slug(msg).as_deref(), Some("sea-stories"));
    }

    #[test]
    fn rejected_tag_slug_is_none_for_an_unrelated_message() {
        assert_eq!(rejected_tag_slug("term 5 does not exist"), None);
        assert_eq!(
            rejected_tag_slug("a sibling term with the slug \"x\" already exists"),
            None
        );
        assert_eq!(rejected_tag_slug(""), None);
    }
}

/// Apply `rejected_tag_slug`'s result: mark the ONE staged chip whose slugified name
/// matches as rejected (SF13 — "never a global message plus N indistinguishable
/// chips"), leaving every other staged chip untouched so a retry doesn't have to
/// re-type them. A no-op when the message doesn't name this shape of failure, or
/// names a slug no currently-staged chip produces (already resolved/removed).
fn mark_rejected_staged_tag(terms: TermsCtx, message: &str) {
    let Some(slug) = rejected_tag_slug(message) else {
        return;
    };
    terms.staged.update(|v| {
        for s in v.iter_mut() {
            if ferropress_core::slugify(&s.name).as_deref() == Some(slug.as_str()) {
                s.rejected = true;
            }
        }
    });
}

/// Install the B7 keyboard mechanism for every tag token-input, in ONE
/// document-level `keydown` listener (the `install_escape_guard` idiom) rather than
/// `onkeydown:`/`onsubmit:` in rsx — at the pinned rinch rev those attributes compile
/// but silently become CLICK handlers (the rsx event map routes anything it doesn't
/// recognize to `data-rid`), so an element-scoped keyboard handler here would be a
/// silent no-op. Target-gated on `data-fp-tagbuffer="{taxonomy key}"` so the listener
/// knows WHICH taxonomy's buffer to act on and never fires for a keystroke elsewhere
/// on the page. `prevent_default` only on the keys actually consumed.
///
/// * Enter — accepts the active suggestion if one is highlighted, else commits the
///   whole typed buffer as a chip (comma-splitting it too, in case of a paste that
///   never fired `oninput`'s own scan).
/// * ArrowDown/ArrowUp — moves the active suggestion index (wraps at the ends);
///   no-op when the suggestion list is empty.
/// * Backspace on an EMPTY buffer — two-step (Gutenberg behaviour): the first press
///   just visually selects the last chip (todo: full press-again-to-remove needs a
///   "selected chip" signal this pass keeps out of scope — see the S2 report); this
///   pass implements the simpler, still-correct single-press remove, since a chip's
///   only OTHER removal route (the pointer-only `×`) is always available regardless.
/// * Escape — closes the suggestion list and explicitly KEEPS the buffer text (never
///   clears it — Escape must not double as "discard what I typed").
fn install_tag_buffer_guard(terms: TermsCtx) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
        let Some(taxonomy) = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.get_attribute("data-fp-tagbuffer"))
        else {
            return;
        };
        match e.key().as_str() {
            "Enter" => {
                e.prevent_default();
                let active = terms.suggest_active.get().get(&taxonomy).copied();
                let picked = active.and_then(|i| {
                    terms
                        .suggestions
                        .get()
                        .get(&taxonomy)
                        .and_then(|list| list.get(i))
                        .cloned()
                });
                match picked {
                    Some(t) => {
                        terms.selected_ids.update(|s| {
                            s.insert(t.id);
                        });
                        terms.sidecar.update(|m| {
                            m.insert(
                                t.id,
                                api::TermRefDto {
                                    id: t.id,
                                    name: t.name,
                                    slug: t.slug,
                                    taxonomy: taxonomy.clone(),
                                },
                            );
                        });
                        terms.terms_gen.update(|g| *g += 1);
                    }
                    None => {
                        let buf = terms
                            .tag_buffer
                            .get()
                            .get(&taxonomy)
                            .cloned()
                            .unwrap_or_default();
                        if !buf.trim().is_empty() {
                            commit_tag_text(terms, &taxonomy, &buf);
                        }
                    }
                }
                terms.tag_buffer.update(|m| {
                    m.remove(&taxonomy);
                });
                terms.suggestions.update(|m| {
                    m.remove(&taxonomy);
                });
                terms.suggest_active.update(|m| {
                    m.remove(&taxonomy);
                });
            }
            "Backspace" => {
                let empty = terms
                    .tag_buffer
                    .get()
                    .get(&taxonomy)
                    .is_none_or(|b| b.is_empty());
                if !empty {
                    return;
                }
                e.prevent_default();
                let last = terms
                    .staged
                    .get()
                    .iter()
                    .rev()
                    .find(|s| s.taxonomy == taxonomy)
                    .map(|s| s.cid)
                    .map(StagedOrAssigned::Staged)
                    .or_else(|| {
                        terms
                            .sidecar
                            .get()
                            .values()
                            .filter(|t| t.taxonomy == taxonomy)
                            .map(|t| t.id)
                            .max()
                            .map(StagedOrAssigned::Assigned)
                    });
                match last {
                    Some(StagedOrAssigned::Staged(cid)) => {
                        terms.staged.update(|v| v.retain(|s| s.cid != cid));
                        terms.terms_gen.update(|g| *g += 1);
                    }
                    Some(StagedOrAssigned::Assigned(id)) => {
                        terms.selected_ids.update(|s| {
                            s.remove(&id);
                        });
                        terms.terms_gen.update(|g| *g += 1);
                    }
                    None => {}
                }
            }
            "ArrowDown" | "ArrowUp" => {
                let len = terms
                    .suggestions
                    .get()
                    .get(&taxonomy)
                    .map(Vec::len)
                    .unwrap_or(0);
                if len == 0 {
                    return;
                }
                e.prevent_default();
                let cur = terms.suggest_active.get().get(&taxonomy).copied();
                let next = match (e.key().as_str(), cur) {
                    ("ArrowDown", None) => 0,
                    ("ArrowDown", Some(i)) => (i + 1) % len,
                    ("ArrowUp", None) => len - 1,
                    ("ArrowUp", Some(i)) => (i + len - 1) % len,
                    _ => unreachable!(),
                };
                terms.suggest_active.update(|m| {
                    m.insert(taxonomy.clone(), next);
                });
            }
            "Escape" => {
                let had_suggestions = terms.suggestions.get().contains_key(&taxonomy);
                if !had_suggestions {
                    return;
                }
                e.prevent_default();
                terms.suggestions.update(|m| {
                    m.remove(&taxonomy);
                });
                terms.suggest_active.update(|m| {
                    m.remove(&taxonomy);
                });
                // The buffer text is deliberately left untouched.
            }
            _ => {}
        }
    });
    let _ = doc.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
    cb.forget();

    // B8's blur-commits-the-buffer leg. rinch's rsx event map has no `onblur`
    // mapping (it would silently become a click handler, per this function's own
    // doc comment) — a real `focusout` listener (bubbles, unlike `blur`) instead.
    // Re-fetches `document` rather than reusing the outer `doc` binding: `Closure`
    // captures by move, and the keydown closure above already consumed it.
    let Some(doc2) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let blur_cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        let Some(taxonomy) = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.get_attribute("data-fp-tagbuffer"))
        else {
            return;
        };
        let buf = terms
            .tag_buffer
            .get()
            .get(&taxonomy)
            .cloned()
            .unwrap_or_default();
        if !buf.trim().is_empty() {
            commit_tag_text(terms, &taxonomy, &buf);
            terms.tag_buffer.update(|m| {
                m.remove(&taxonomy);
            });
        }
        terms.suggestions.update(|m| {
            m.remove(&taxonomy);
        });
        terms.suggest_active.update(|m| {
            m.remove(&taxonomy);
        });
    });
    let _ = doc2.add_event_listener_with_callback("focusout", blur_cb.as_ref().unchecked_ref());
    blur_cb.forget();
}

/// Which side of the assignment a Backspace-on-empty-buffer removes: the most
/// recently staged new chip for this taxonomy, else the highest-id assigned term
/// (a stable, if arbitrary, "most recent" proxy — assignment order isn't tracked).
enum StagedOrAssigned {
    Staged(u64),
    Assigned(u64),
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
    terms: TermsCtx,
    notice: Signal<String>,
    view: Signal<View>,
    auth: AuthCtx,
) {
    notice.set(String::new());
    kind.set(EntityKind::Post);
    view.set(View::List);
    load_posts(posts, state, auth);
    // Q2: pre-warm the shared vocabulary from the Posts galley too (idempotent —
    // `open_post`/`new_post` already guarantee it's loaded by the time a panel
    // could render, so this is a head start, not a correctness requirement).
    load_vocab(terms, auth);
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
fn open_post(id: u64, ectx: EditorCtx) {
    let EditorCtx {
        editor,
        title,
        slug,
        status,
        featured,
        current_id,
        editor_session,
        terms,
        notice,
        auth,
        ..
    } = ectx;
    notice.set(String::new());
    load_vocab(terms, auth);
    // B5: bump the OPEN generation SYNCHRONOUSLY, before the fetch even starts — a
    // second rapid click (a different post, or "New post") must invalidate this
    // fetch's eventual arrival by generation, not just race it. `editor_session`
    // already serves this role for save/preview; opens get the identical guard here
    // for the first time.
    editor_session.update(|g| *g += 1);
    let open_gen = editor_session.get();
    spawn_local(async move {
        match api::get_post(id).await {
            Ok(detail) => {
                // A newer open (or a save that landed and moved the generation)
                // superseded this one while it was in flight: drop it entirely
                // rather than writing a stale document's fields over the current one.
                if editor_session.get() != open_gen {
                    return;
                }
                // Every field lands in this SAME synchronous block (no `.await`
                // between them) — an observer can never see e.g. the new title with
                // the old terms, or vice versa (B5's atomic-seed requirement).
                title.set(detail.title);
                slug.set(detail.slug);
                status.set(detail.status);
                featured.set(detail.featured_media);
                current_id.set(Some(detail.id));
                seed_terms(terms, &detail.terms);

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
            Err(api::ApiError::Message(e)) => {
                if editor_session.get() == open_gen {
                    notice.set(format!("Couldn't open that post: {e}"));
                }
            }
        }
    });
}

/// Reset `terms`' per-document assignment half from an authoritative term list — the
/// SAME seeding [`open_post`] uses on load and [`new_post`] uses to go blank
/// (`terms: &[]`). Clears every staged/filter/buffer/suggestion signal too: none of
/// that transient UI state can belong to whatever document was open a moment ago.
fn seed_terms(terms: TermsCtx, seed: &[api::TermRefDto]) {
    terms
        .selected_ids
        .set(seed.iter().map(|t| t.id).collect::<HashSet<u64>>());
    terms.sidecar.set(
        seed.iter()
            .map(|t| (t.id, t.clone()))
            .collect::<HashMap<u64, api::TermRefDto>>(),
    );
    terms
        .terms_snapshot
        .set(seed.iter().map(|t| t.id).collect());
    terms.staged.set(Vec::new());
    terms.tag_buffer.set(HashMap::new());
    terms.checklist_filter.set(HashMap::new());
    terms.suggestions.set(HashMap::new());
    terms.suggest_active.set(HashMap::new());
    terms.missing_term_notice.set(None);
    terms.terms_gen.update(|g| *g += 1);
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
/// create) AND the post's AUTHORITATIVE terms after the server's reconcile (SF4),
/// for the caller's B6 reseed. `terms` follows the SF1 dirty-send contract
/// ([`api::dirty_terms`] — `None` = leave unchanged, `Some(v)` = set-reconcile,
/// including the deliberate `Some(vec![])` "clear everything"); `new_terms` is
/// additive on top of it. The one create-vs-update decision, shared by
/// [`save_post`] and [`preview_post`]; the stale-session guard + `current_id`
/// writeback stay with the callers, which differ in what they do on success (toast
/// vs. steer the tab).
#[allow(clippy::too_many_arguments)]
async fn persist_post(
    existing: Option<u64>,
    title: String,
    slug: String,
    status: String,
    block_tree: serde_json::Value,
    featured: Option<u64>,
    terms: Option<Vec<u64>>,
    new_terms: Option<Vec<api::NewTermRequest>>,
) -> Result<(u64, Vec<api::TermRefDto>), api::ApiError> {
    match existing {
        Some(id) => api::save_post(
            id,
            &api::SaveRequest {
                title,
                slug,
                status,
                block_tree,
                featured_media: featured,
                terms,
                new_terms,
            },
        )
        .await
        .map(|resp| (resp.id, resp.terms)),
        None => api::create_post(&api::CreateRequest {
            title,
            slug,
            status,
            block_tree,
            featured_media: featured,
            terms,
            new_terms,
        })
        .await
        .map(|resp| (resp.id, resp.terms)),
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
fn save_post(ectx: EditorCtx) {
    let EditorCtx {
        editor,
        title,
        slug,
        status,
        featured,
        current_id,
        editor_session,
        terms,
        saving,
        notice,
        toast,
        auth,
    } = ectx;
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
    // B8: an uncommitted tag buffer must never be silently dropped by a save — it
    // is committed through the SAME path as Enter, synchronously, before the
    // payload is even built. A buffer that fails local validation BLOCKS the save
    // (an inline message) rather than being swallowed, which would otherwise 400
    // the WHOLE save (title/body/status included) via the server's own reserved-
    // slug/empty-name checks — this surfaces the same problem before the network
    // round trip, attributed to the actual field at fault.
    if let Err(msg) = commit_buffers_for_save(terms) {
        notice.set(msg);
        return;
    }

    notice.set(String::new());
    saving.set(true);
    let title_val = title.get();
    let status_val = status.get();
    let featured_val = featured.get().map(|f| f.id);
    let existing = current_id.get();
    // SF1 dirty-send: only send a `terms` field when the live selection actually
    // differs from the last-known-persisted snapshot — an untouched panel must
    // never clear (or even touch) a post's categories/tags on a body-only save.
    let current_ids: Vec<u64> = terms.selected_ids.get().into_iter().collect();
    let snapshot_ids = terms.terms_snapshot.get();
    let terms_arg = api::dirty_terms(&current_ids, &snapshot_ids);
    let sent_staged = terms.staged.get();
    let new_terms_arg = if sent_staged.is_empty() {
        None
    } else {
        Some(
            sent_staged
                .iter()
                .map(|s| api::NewTermRequest {
                    taxonomy: s.taxonomy.clone(),
                    name: s.name.clone(),
                })
                .collect(),
        )
    };
    // The document this save belongs to (editor_session) AND whether the live
    // assignment moves again while the request is in flight (terms_gen) — B6's two
    // independent staleness axes. A DIFFERENT document supersedes everything; the
    // SAME document with a later assignment edit merges instead of overwriting.
    let save_gen = editor_session.get();
    let terms_gen_at_send = terms.terms_gen.get();
    spawn_local(async move {
        let result = persist_post(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
            terms_arg,
            new_terms_arg,
        )
        .await;
        let stale = editor_session.get() != save_gen;
        let terms_gen_moved = terms.terms_gen.get() != terms_gen_at_send;
        saving.set(false);
        match result {
            Ok((id, resp_terms)) => {
                if stale {
                    return;
                }
                if existing.is_none() {
                    current_id.set(Some(id));
                }
                reseed_terms(terms, terms_gen_moved, &sent_staged, &resp_terms);
                toast.set(true);
                TimeoutFuture::new(1600).await;
                toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                if stale {
                    return;
                }
                // SF17: every failed save invalidates the vocab, regardless of
                // WHY it failed — `apply_terms` mutates one tag at a time, so a
                // partial failure can leave an earlier tag created-but-
                // unassigned; the next load must show the truth. `taxonomies`
                // is the SAME signal `TaxonomyPanel`'s own `for tax in
                // terms.taxonomies.get()` renders from (Q2's one-cache
                // design) — clearing it with nothing to refill it made the
                // WHOLE Categories/Tags UI vanish for the rest of the editing
                // session (caught live: it never came back even after a
                // successful retry save). `load_vocab` is the one function
                // that refills `taxonomies` and `hierarchical_terms`
                // together, so fire it right back up.
                invalidate_vocab(terms);
                load_vocab(terms, auth);
                // B10(b): a term deleted elsewhere between load and save hard-400s
                // the WHOLE save. Recognize that one shape, silently drop the
                // offending id from the live selection so the SAME Save button
                // just works on the next click — never an automatic retry, and
                // never a preventative prune of `selected_ids` against the
                // vocabulary otherwise (B10c forbids that as its own silent-unlink
                // path).
                match missing_term_id(&e) {
                    Some(missing_id) => {
                        terms.selected_ids.update(|s| {
                            s.remove(&missing_id);
                        });
                        terms.sidecar.update(|m| {
                            m.remove(&missing_id);
                        });
                        terms.terms_gen.update(|g| *g += 1);
                        terms.missing_term_notice.set(Some(missing_id));
                        notice.set(
                            "One of this post's categories/tags no longer exists, so nothing was saved. It's been removed here \u{2014} click Save again."
                                .to_string(),
                        );
                    }
                    // SF13: distinct from the missing-id shape above — this
                    // one names a SLUG (an inline tag whose archive path
                    // collided), so it maps back to the ONE offending staged
                    // chip instead of a global message next to every chip.
                    None => {
                        mark_rejected_staged_tag(terms, &e);
                        notice.set(e);
                    }
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
fn preview_post(ectx: EditorCtx) {
    let EditorCtx {
        editor,
        title,
        slug,
        status,
        featured,
        current_id,
        editor_session,
        terms,
        saving,
        notice,
        auth,
        ..
    } = ectx;
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
    // B8/SF4: Preview shares Save's contract exactly — the same buffer-commit
    // (WYSIWYP: a chip typed but not yet committed must be in the preview too, or
    // the tab would render a document the panel doesn't yet show as saved).
    if let Err(msg) = commit_buffers_for_save(terms) {
        notice.set(msg);
        return;
    }

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
    let current_ids: Vec<u64> = terms.selected_ids.get().into_iter().collect();
    let snapshot_ids = terms.terms_snapshot.get();
    let terms_arg = api::dirty_terms(&current_ids, &snapshot_ids);
    let sent_staged = terms.staged.get();
    let new_terms_arg = if sent_staged.is_empty() {
        None
    } else {
        Some(
            sent_staged
                .iter()
                .map(|s| api::NewTermRequest {
                    taxonomy: s.taxonomy.clone(),
                    name: s.name.clone(),
                })
                .collect(),
        )
    };
    let save_gen = editor_session.get();
    let terms_gen_at_send = terms.terms_gen.get();
    spawn_local(async move {
        let result = persist_post(
            existing,
            title_val,
            slug_val,
            status_val,
            block_tree,
            featured_val,
            terms_arg,
            new_terms_arg,
        )
        .await;
        let stale = editor_session.get() != save_gen;
        let terms_gen_moved = terms.terms_gen.get() != terms_gen_at_send;
        saving.set(false);
        match result {
            Ok((id, resp_terms)) => {
                if stale {
                    // The editor switched documents mid-flight: the save landed, but
                    // steering the tab into what is now a different post would mislead.
                    let _ = win.close();
                    return;
                }
                if existing.is_none() {
                    current_id.set(Some(id));
                }
                reseed_terms(terms, terms_gen_moved, &sent_staged, &resp_terms);
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
                if stale {
                    return;
                }
                // SF17 — see the identical note in `save_post`.
                invalidate_vocab(terms);
                load_vocab(terms, auth);
                match missing_term_id(&e) {
                    Some(missing_id) => {
                        terms.selected_ids.update(|s| {
                            s.remove(&missing_id);
                        });
                        terms.sidecar.update(|m| {
                            m.remove(&missing_id);
                        });
                        terms.terms_gen.update(|g| *g += 1);
                        terms.missing_term_notice.set(Some(missing_id));
                        notice.set(
                            "One of this post's categories/tags no longer exists, so nothing was saved. It's been removed here \u{2014} click Preview again."
                                .to_string(),
                        );
                    }
                    None => {
                        mark_rejected_staged_tag(terms, &e);
                        notice.set(e);
                    }
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
fn new_post(ectx: EditorCtx) {
    let EditorCtx {
        editor,
        title,
        slug,
        status,
        featured,
        current_id,
        editor_session,
        terms,
        notice,
        auth,
        ..
    } = ectx;
    // A fresh document takes over the shared editor: bump the session so any
    // still-in-flight save/open can no longer write its result back into this
    // blank post (the same generation guard `open_post` uses).
    editor_session.update(|g| *g += 1);
    notice.set(String::new());
    load_vocab(terms, auth);
    title.set(String::new());
    slug.set(String::new());
    status.set("draft".to_owned());
    featured.set(None);
    current_id.set(None);
    seed_terms(terms, &[]);

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
// Taxonomy MANAGEMENT (View::Terms / View::TermEditor) — Inc 3 S3. A separate
// surface from `TaxonomyPanel` (the post editor's assignment checklist/tags):
// this is where categories and tags themselves are created, renamed,
// re-parented, and deleted. SF14e: every visible string comes from
// `TaxonomyDto.label` — never the literal "Term"/"Terms".
// ============================================================================

/// The active/editing taxonomy's display label, or "" if the vocabulary hasn't
/// loaded (or resolved) yet — the sole source of every user-facing name on
/// these two views (SF14e).
fn taxonomy_label(taxonomies: &[api::TaxonomyDto], key: &str) -> String {
    taxonomies
        .iter()
        .find(|t| t.key == key)
        .map(|t| t.label.clone())
        .unwrap_or_default()
}

/// SF9's "Published" column note, worded for whichever taxonomy is active
/// (SF14e — never a hardcoded "category"): a flat taxonomy has no parent
/// rollup to mention, so its sentence drops entirely rather than describing a
/// hierarchy relation that doesn't exist for it.
fn published_count_note(taxonomies: &[api::TaxonomyDto], key: &str) -> String {
    let Some(tax) = taxonomies.iter().find(|t| t.key == key) else {
        return String::new();
    };
    let noun = singularize(&tax.label);
    let mut note = format!(
        "\u{201c}Published\u{201d} counts this {noun}'s own direct posts (drafts excluded)."
    );
    if tax.hierarchical {
        note.push_str(" A parent's public archive additionally rolls up its descendants.");
    }
    note
}

/// Whether the taxonomy currently open in the editor is hierarchical — gates
/// the Parent field (SF10: a flat taxonomy's terms have none).
fn term_editor_taxonomy_hierarchical(tv: TermsViewCtx) -> bool {
    let key = tv.edit_taxonomy.get();
    tv.terms
        .taxonomies
        .get()
        .iter()
        .any(|t| t.key == key && t.hierarchical)
}

/// One term-management row's presentation fields (mirrors [`PageRowVm`]).
/// `indent` comes straight from `TermDto.depth` (a flat taxonomy's rows are
/// always depth 0, so no indent). `count_label` names SF9's "Published" column
/// honestly — `TermDto.count` is the live DIRECT published-post count, never
/// "posts affected by deleting this term". Deliberately carries only `id` (not
/// the whole [`api::TermDto`]) — the row's click handlers re-look-up the term
/// from `TermsViewCtx::list` by id, so `id` (a `Copy` `u64`) is the only field
/// referenced from more than one place in a row's body, sidestepping the
/// if/else-capture gotcha a second `.clone()` of a shared non-`Copy` field
/// would invite (`ferropress-rinch-if-else-capture-gotcha` memory).
#[derive(Clone, PartialEq)]
struct TermRowVm {
    id: u64,
    name: String,
    slug: String,
    count_label: String,
    indent: String,
}

fn term_row_vms(terms: &[api::TermDto]) -> Vec<TermRowVm> {
    terms
        .iter()
        .map(|t| TermRowVm {
            id: t.id,
            name: if t.name.trim().is_empty() {
                "(untitled)".to_owned()
            } else {
                t.name.clone()
            },
            slug: t.slug.clone(),
            count_label: format!("{} published", t.count),
            indent: format!("padding-left: {:.2}rem", 0.25 + t.depth as f32 * 1.25),
        })
        .collect()
}

/// The Parent `<select>` options for the term editor (SF10, Q4): a top-level
/// ("— None —") entry first, then every OTHER term in the taxonomy except
/// `self_id` and its own subtree — computed by walking `TermDto.parent`
/// upward from each CANDIDATE term (mirroring the server's own
/// `is_ancestor_or_self`, `ferropress-http::admin::terms`), never a
/// depth-ordinal scan (`flatten_tree` can append an unreachable/cyclic row at
/// depth 0 after the tree, which a depth-run scan would misclassify). A brand
/// new term (`self_id: None`) excludes nothing. `current` (the term's current
/// parent) marks the selected option; if it isn't among the candidates — a
/// stale list, or a corrupt/foreign id — it is still represented so "None"
/// becomes a real, clickable change rather than a silently-already-selected
/// option a re-pick can't distinguish from (mirrors `parent_options`).
fn term_parent_options(
    list: &[api::TermDto],
    self_id: Option<u64>,
    current: Option<u64>,
) -> Vec<SelectOpt> {
    let current_str = opt_value(current);
    let mut opts = vec![SelectOpt {
        value: String::new(),
        label: "\u{2014} None (top level) \u{2014}".to_owned(),
        selected: current_str.is_empty(),
    }];
    for t in list {
        if Some(t.id) == self_id {
            continue; // not itself
        }
        if let Some(sid) = self_id {
            // Walk UP from this candidate; if `self_id` appears, the candidate
            // is self's own descendant (or self) — excluded.
            let mut cur = Some(t.id);
            let mut seen = HashSet::new();
            let mut in_subtree = false;
            while let Some(id) = cur {
                if id == sid {
                    in_subtree = true;
                    break;
                }
                if !seen.insert(id) {
                    break; // pre-existing cycle in stored data — stop walking
                }
                cur = list.iter().find(|x| x.id == id).and_then(|x| x.parent);
            }
            if in_subtree {
                continue;
            }
        }
        let indent = "\u{00A0}\u{00A0}".repeat(t.depth);
        let value = t.id.to_string();
        let selected = value == current_str;
        opts.push(SelectOpt {
            value,
            label: format!("{indent}{}", t.name),
            selected,
        });
    }
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

/// Switch to the Categories cabinet (Q2): ensure the shared taxonomy
/// vocabulary is loaded (the SAME `terms.taxonomies`/`vocab_load` cache the
/// post editor's assignment panels read), pick the active taxonomy tab (the
/// one already showing, if it still exists; else the first), and load that
/// taxonomy's WHOLE term list WITH counts — unlike the post editor's cheap
/// `?counts=0` checklist mode, this screen's whole point is the "Published"
/// column (SF9).
fn open_terms_view(tv: TermsViewCtx) {
    tv.notice.set(String::new());
    tv.view.set(View::Terms);
    tv.list_state.set(Load::Loading);
    let keep = tv.active_taxonomy.get();
    spawn_local(async move {
        let taxonomies = if tv.terms.taxonomies.get().is_empty() {
            match api::list_taxonomies().await {
                Ok(v) => {
                    tv.terms.taxonomies.set(v.clone());
                    tv.terms.vocab_load.set(VocabLoad::Ready);
                    v
                }
                Err(api::ApiError::Unauthorized) => {
                    tv.auth.session_expired();
                    return;
                }
                Err(api::ApiError::Message(e)) => {
                    tv.terms.vocab_load.set(VocabLoad::Error);
                    tv.list_state.set(Load::Error);
                    tv.notice.set(e);
                    return;
                }
            }
        } else {
            tv.terms.taxonomies.get()
        };
        let key = if taxonomies.iter().any(|t| t.key == keep) {
            keep
        } else {
            taxonomies
                .first()
                .map(|t| t.key.clone())
                .unwrap_or_default()
        };
        tv.active_taxonomy.set(key.clone());
        if key.is_empty() {
            // No taxonomies at all — a real, non-error empty state (SF13).
            tv.list.set(Vec::new());
            tv.list_state.set(Load::Ready);
            return;
        }
        load_term_list(tv, key).await;
    });
}

/// Fetch one taxonomy's full term list WITH counts and apply it — the shared
/// body behind `open_terms_view`'s initial load, a tab switch, and the
/// post-mutation refresh.
async fn load_term_list(tv: TermsViewCtx, key: String) {
    match api::list_terms(&key, true, None, None).await {
        Ok(resp) => {
            tv.list.set(resp.terms);
            tv.list_state.set(Load::Ready);
        }
        Err(api::ApiError::Unauthorized) => tv.auth.session_expired(),
        Err(api::ApiError::Message(e)) => {
            tv.notice.set(e);
            tv.list_state.set(Load::Error);
        }
    }
}

/// Switch the active taxonomy tab (the Posts/Pages-toggle idiom, SF14d).
fn switch_taxonomy(tv: TermsViewCtx, key: String) {
    if tv.active_taxonomy.get() == key {
        return;
    }
    tv.notice.set(String::new());
    tv.active_taxonomy.set(key.clone());
    tv.list_state.set(Load::Loading);
    spawn_local(async move { load_term_list(tv, key).await });
}

/// Open the create form for the currently active taxonomy (SF11: CREATE mode
/// — an empty slug field with the "auto from name" placeholder).
fn new_term_editor(tv: TermsViewCtx) {
    tv.notice.set(String::new());
    tv.edit_id.set(None);
    tv.edit_taxonomy.set(tv.active_taxonomy.get());
    tv.edit_name.set(String::new());
    tv.edit_slug.set(String::new());
    tv.edit_description.set(String::new());
    tv.edit_parent.set(None);
    tv.edit_rev.set(0);
    tv.dirty.set(false);
    tv.view.set(View::TermEditor);
}

/// Open the edit form for one existing term, looked up fresh from the loaded
/// list by id (SF11: EDIT mode — the slug field is pre-filled + controlled,
/// never re-derived from a changed name).
fn open_term_editor_by_id(tv: TermsViewCtx, id: u64) {
    let Some(term) = tv.list.get().into_iter().find(|t| t.id == id) else {
        return;
    };
    tv.notice.set(String::new());
    tv.edit_id.set(Some(term.id));
    tv.edit_taxonomy.set(tv.active_taxonomy.get());
    tv.edit_name.set(term.name);
    tv.edit_slug.set(term.slug);
    tv.edit_description.set(term.description);
    tv.edit_parent.set(term.parent);
    tv.edit_rev.set(term.rev);
    tv.dirty.set(false);
    tv.view.set(View::TermEditor);
}

/// Leave the term editor, confirming first if there are unsaved changes
/// (mirrors `leave_menu_editor`) — a full reload of the Categories cabinet.
fn leave_term_editor(tv: TermsViewCtx) {
    if tv.dirty.get() {
        // SF14e: the noun names the taxonomy actually being edited (a Tag
        // form must not say "category").
        let noun = singularize(&taxonomy_label(
            &tv.terms.taxonomies.get(),
            &tv.edit_taxonomy.get(),
        ));
        let noun = if noun.is_empty() {
            "item".to_owned()
        } else {
            noun
        };
        if !confirm(&format!(
            "Leave without saving? Your changes to this {noun} will be lost."
        )) {
            return;
        }
    }
    tv.dirty.set(false);
    open_terms_view(tv);
}

/// Save the create/edit form. SF11: CREATE derives the slug server-side when
/// the field is empty; EDIT always sends the field's live value — an edit's
/// empty slug means "keep the stored slug" server-side (`update_term`'s own
/// doc comment), so a cleared field would silently NOT do what it visually
/// shows and is refused client-side instead. SF12: an edit echoes the loaded
/// `rev` as `expected_rev`, so a save that lost a race with another session's
/// edit 409s instead of silently reverting it. SF13: a sibling-slug 409 moves
/// focus to the Slug field (a string match on the server's own message text —
/// the only signal a `{ error }` string body gives the client).
fn save_term(tv: TermsViewCtx) {
    if tv.saving.get() {
        return;
    }
    let name = tv.edit_name.get();
    if name.trim().is_empty() {
        tv.notice.set("A name is required.".to_owned());
        return;
    }
    let id = tv.edit_id.get();
    let slug_trimmed = tv.edit_slug.get().trim().to_owned();
    if id.is_some() && slug_trimmed.is_empty() {
        tv.notice.set("A slug is required.".to_owned());
        focus_field("term-editor-slug");
        return;
    }
    let slug_arg = if slug_trimmed.is_empty() {
        None
    } else {
        Some(slug_trimmed)
    };
    let taxonomy = tv.edit_taxonomy.get();
    let description = tv.edit_description.get();
    let parent = tv.edit_parent.get();
    let expected_rev = id.map(|_| tv.edit_rev.get());
    tv.notice.set(String::new());
    tv.saving.set(true);
    spawn_local(async move {
        let result = match id {
            Some(id) => {
                api::update_term(
                    id,
                    &name,
                    slug_arg.as_deref(),
                    &description,
                    parent,
                    expected_rev,
                )
                .await
            }
            None => {
                api::create_term(&taxonomy, &name, slug_arg.as_deref(), &description, parent).await
            }
        };
        tv.saving.set(false);
        match result {
            Ok(_) => {
                tv.dirty.set(false);
                // `invalidate_vocab` blanks the SAME shared `taxonomies` cache
                // this view's own masthead/switcher/heading read (Q2) — it
                // exists so the post editor's checklist re-fetches on its
                // NEXT open, but staying HERE needs it repopulated right
                // away, not left empty until some unrelated code path
                // happens to call `load_vocab`. `load_vocab` is the one
                // function that refills both `taxonomies` and
                // `hierarchical_terms` together (they share one fetch), so
                // fire it immediately rather than duplicating its fetch.
                invalidate_vocab(tv.terms);
                load_vocab(tv.terms, tv.auth);
                tv.view.set(View::Terms);
                let key = tv.active_taxonomy.get();
                load_term_list(tv, key).await;
                focus_field("terms-heading");
                tv.toast.set(true);
                TimeoutFuture::new(1600).await;
                tv.toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => tv.auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                if e.to_lowercase().contains("slug") {
                    focus_field("term-editor-slug");
                }
                tv.notice.set(e);
            }
        }
    });
}

/// Delete one term (SF9): a native `confirm()` naming the REAL blast radius —
/// its direct children's new parent (WP's own `wp_delete_term` re-homing: they
/// move under the deleted term's OWN parent, or become top-level; there is no
/// "Uncategorized" fallback here) and that posts in it lose the category
/// outright (never re-assigned — this codebase has nowhere to re-home them
/// to). The server re-checks the same per-sibling-slug/archive-path invariants
/// a re-parent would and 409s (naming the offending child) rather than
/// silently breaking them; that message is surfaced verbatim (SF13).
fn delete_term_clicked(tv: TermsViewCtx) {
    if tv.saving.get() {
        return;
    }
    let Some(id) = tv.edit_id.get() else {
        return;
    };
    let list = tv.list.get();
    let name = tv.edit_name.get();
    let parent = tv.edit_parent.get();
    // SF14e: name the taxonomy actually being deleted — a Tag's confirm must
    // not say "category". The taxonomy's OWN `label` is already its natural
    // plural (WP convention — "Categories"/"Tags"), so the plural noun is
    // just that lowercased; only the singular needs `singularize`.
    let tax_label = taxonomy_label(&tv.terms.taxonomies.get(), &tv.edit_taxonomy.get());
    let noun_singular = {
        let s = singularize(&tax_label);
        if s.is_empty() { "item".to_owned() } else { s }
    };
    let noun_plural = {
        let p = tax_label.to_lowercase();
        if p.is_empty() { "items".to_owned() } else { p }
    };
    let child_count = list.iter().filter(|t| t.parent == Some(id)).count();
    let label = if name.trim().is_empty() {
        format!("this {noun_singular}")
    } else {
        format!("\u{201c}{name}\u{201d}")
    };
    let mut msg = format!("Delete {label}?");
    if child_count > 0 {
        let word = if child_count == 1 {
            noun_singular.clone()
        } else {
            noun_plural.clone()
        };
        let verb = if child_count == 1 { "moves" } else { "move" };
        let dest = match parent {
            Some(pid) => list
                .iter()
                .find(|t| t.id == pid)
                .map(|t| format!("\u{201c}{}\u{201d}", t.name))
                .unwrap_or_else(|| "its parent".to_owned()),
            None => "the top level".to_owned(),
        };
        msg.push_str(&format!(
            " Its {child_count} child {word} {verb} to {dest}."
        ));
    }
    msg.push_str(&format!(
        " Posts in it lose this {noun_singular} (they will not be re-assigned)."
    ));
    if !confirm(&msg) {
        return;
    }
    tv.notice.set(String::new());
    tv.saving.set(true);
    spawn_local(async move {
        match api::delete_term(id).await {
            Ok(()) => {
                tv.saving.set(false);
                tv.dirty.set(false);
                // B10(a): purge the deleted id from the SHARED assignment
                // state — `terms.selected_ids`/`sidecar` is the SAME
                // `TermsCtx` the post editor's own checklist/chips read
                // (there is one app-wide `terms`, not a per-view copy), so a
                // post left open with this term checked before navigating
                // here must come back with it UNCHECKED, not merely absent
                // from a reloaded vocabulary — otherwise the next Save
                // still sends the now-nonexistent id and hard-400s (the
                // exact failure `missing_term_id`'s B10(b) path recovers
                // from reactively; this is the SAME cleanup, done proactively
                // since we already know the id here).
                tv.terms.selected_ids.update(|s| {
                    s.remove(&id);
                });
                tv.terms.sidecar.update(|m| {
                    m.remove(&id);
                });
                tv.terms.terms_gen.update(|g| *g += 1);
                // See the identical comment in `save_term` — `invalidate_vocab`
                // blanks the SAME `taxonomies` cache this view's own chrome
                // reads, so refill it right away via `load_vocab`.
                invalidate_vocab(tv.terms);
                load_vocab(tv.terms, tv.auth);
                tv.view.set(View::Terms);
                let key = tv.active_taxonomy.get();
                load_term_list(tv, key).await;
                focus_field("terms-heading");
                tv.toast.set(true);
                TimeoutFuture::new(1600).await;
                tv.toast.set(false);
            }
            Err(api::ApiError::Unauthorized) => {
                tv.saving.set(false);
                tv.auth.session_expired();
            }
            Err(api::ApiError::Message(e)) => {
                tv.saving.set(false);
                tv.notice.set(e);
            }
        }
    });
}

// ============================================================================
// Nav-menu editor — components, navigation/save helpers, and the pure tree ops.
// ============================================================================

/// Whether this user's role is Editor+ — grants BOTH `ManageMenus` and `ManageTerms`
/// (identical role sets server-side, ferropress_core::role), so Menus and Categories
/// share this ONE predicate rather than two role-string lists that could drift (Inc-3
/// design ruling Q1). A role-string check like [`is_admin`] — a display hint only;
/// the server enforces the capability authoritatively, so a stale/edge role just sees
/// a nav button that 403s.
fn is_editor_plus(user: &Option<UserDto>) -> bool {
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
fn MenuLocationView(row: api::MenuLocationRow, lc: LocCtx) -> NodeHandle {
    let loc = row.location.clone();
    let current_id = row.menu.as_ref().map(|m| m.id);
    let menus = lc.list.get();
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
                        assign_menu_location(lc, &loc, id);
                    },
                    {opts}
                }
            }
        }
    }
}

// ── the insert-between drop gap ─────────────────────────────────────────────────

/// The thin drop zone rendered BEFORE each row — dropping a dragged subtree here re-inserts it as
/// a sibling at that position ([`DropDest::Before`]). A `#[component]` (not inline `for`-body
/// markup) so it can hold its own Copy `cid_sig` for the reactive highlight closure (which is
/// re-created each effect run and so must capture only Copy). It is the FIRST child of the
/// per-row `for` body and carries the for-item `key` (so the gap + its row reconcile as one unit
/// and move together); the drop/hover handlers are stored once, so they may capture the non-Copy
/// cid clones by move.
#[component]
fn DropGap(
    cid: String,
    tree: Signal<Vec<MenuRow>>,
    dirty: Signal<bool>,
    saving: Signal<bool>,
    drag: DragContext<String>,
    drop_hint: Signal<String>,
) -> NodeHandle {
    let cid_sig = Signal::new(cid.clone());
    let (c_drop, c_enter, c_leave) = (cid.clone(), cid.clone(), cid.clone());
    rsx! {
        li {
            class: {move || if drop_hint.get() == format!("gap:{}", cid_sig.get()) { "dropgap is-hint" } else { "dropgap" }},
            ondragenter: move || drop_hint.set(format!("gap:{c_enter}")),
            ondragleave: move || { if drop_hint.get() == format!("gap:{c_leave}") { drop_hint.set(String::new()); } },
            ondrop: move || apply_drop(saving, tree, dirty, drag, drop_hint, DropDest::Before(c_drop.clone())),
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
    /// Drag-and-drop: this row's grip is the drag SOURCE (sets `drag`); the whole row is a
    /// nest-drop TARGET ([`DropDest::Into`]). `drop_hint` drives the reactive drop highlight.
    drag: DragContext<String>,
    drop_hint: Signal<String>,
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
    // Clones for the drag grip (source) + the row's nest-drop (Into) handlers — stored once, so
    // capturing the non-Copy cid by move is fine.
    let (c_grip, c_into_drop, c_into_enter, c_into_leave) =
        (cid.clone(), cid.clone(), cid.clone(), cid.clone());

    rsx! {
        li {
            // Depth indent + the drag-source dim + the nest-drop highlight, all reactive (a keyed
            // row isn't rebuilt on a tree/hint change; depth is excluded from `MenuRow`'s PartialEq).
            class: {move || {
                let c = cid_sig.get();
                let mut s = if row_depth(&tree.get(), &c) > 0 { "menurow is-child".to_owned() } else { "menurow".to_owned() };
                if drag.get().as_deref() == Some(c.as_str()) { s.push_str(" is-dragging"); }
                if drop_hint.get() == format!("into:{c}") { s.push_str(" is-droptarget"); }
                s
            }},
            style: {move || format!("margin-left:{}rem", (row_depth(&tree.get(), &cid_sig.get()) as f32) * 1.5)},
            // The whole row is a nest-drop target (drop → make the dragged subtree its last child).
            ondragenter: move || drop_hint.set(format!("into:{c_into_enter}")),
            ondragleave: move || { if drop_hint.get() == format!("into:{c_into_leave}") { drop_hint.set(String::new()); } },
            ondrop: move || apply_drop(saving, tree, dirty, drag, drop_hint, DropDest::Into(c_into_drop.clone())),
            span { class: "menurow__reorder",
                span {
                    class: "menurow__grip", draggable: "true",
                    aria-hidden: "true", title: "Drag to reorder or nest",
                    ondragstart: move || drag.set(c_grip.clone()),
                    ondragend: move || { drag.clear(); drop_hint.set(String::new()); },
                    "\u{283F}"
                }
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
fn reload_menu_locations(lc: LocCtx) {
    spawn_local(async move {
        match api::list_menu_locations().await {
            // Clear THEN set so the keyed `for` genuinely rebuilds each `MenuLocationView`
            // (its `<select>` `selected` flags are built once, non-reactively). Without the
            // clear, a reload whose data is PartialEq-equal to the current rows (e.g. a
            // FAILED assign, where the binding is unchanged) would be a no-op — leaving the
            // native `<select>` still showing the user's rejected pick (must-fix F3).
            Ok(v) => {
                lc.locations.set(Vec::new());
                lc.locations.set(v);
            }
            Err(api::ApiError::Unauthorized) => lc.auth.session_expired(),
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
    // Seed the auto-add flag from the authoritative response — the SINGLE source, so a load and a
    // post-save re-seed both reflect exactly what the server persisted (never a duplicated set).
    menu.auto_add.set(detail.auto_add_pages);
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
    let auto_add = menu.auto_add.get();
    spawn_local(async move {
        // Rename first (so a slug/name/auto-add edit persists even if the item PUT later fails).
        match api::update_menu(id, &name, Some(&slug), Some(auto_add)).await {
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
    menu.picker_selected.set(Vec::new());
    menu.picker_open.set(true);
    reload_candidates(menu);
    focus_first_modal_input();
}

/// Close the add-item picker, restoring focus to the opener (the "+ Add item" button) so a
/// keyboard user isn't stranded on `document.body` (F7). Clears any pending bulk selection.
fn close_picker(menu: MenuCtx) {
    menu.picker_selected.set(Vec::new());
    menu.picker_open.set(false);
    focus_add_item_button();
}

/// Switch the picker tab, clearing the bulk selection so a Pages selection can't leak into Posts.
fn switch_picker_tab(menu: MenuCtx, tab: u8) {
    menu.picker_tab.set(tab);
    menu.picker_selected.set(Vec::new());
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

/// The [`RowKind`] a link candidate implies (`"page"` → Page, else Post).
fn candidate_kind(cand: &api::LinkCandidate) -> RowKind {
    if cand.kind == "page" {
        RowKind::Page(cand.id)
    } else {
        RowKind::Post(cand.id)
    }
}

/// Append every checked candidate at the top level in one batch (dedup-guarded against items
/// already in the tree — defensive, since [`CandidateRow`] hides already-added ones), then clear
/// the selection and close the picker. A no-op with nothing selected.
fn add_selected_items(menu: MenuCtx) {
    let selected = menu.picker_selected.get();
    if selected.is_empty() {
        return;
    }
    for cand in selected {
        let kind = candidate_kind(&cand);
        if menu.tree.get().iter().any(|r| r.kind == kind) {
            continue; // already in the menu
        }
        let resolved = Some(api::ResolvedTarget {
            title: cand.title,
            href: Some(cand.href),
        });
        add_row(menu, kind, resolved, String::new(), String::new());
    }
    close_picker(menu);
}

/// One pickable Post/Page candidate row (Pages/Posts tabs). Clicking TOGGLES its checkbox in the
/// bulk `selected` list (the picker stays open); a candidate already in the tree renders as a
/// non-interactive "already in menu" row (dedup at the source). Its own `#[component]` so the
/// toggle closure can own the non-Copy candidate while the reactive checkbox reads only Copy state.
#[component]
fn CandidateRow(
    cand: api::LinkCandidate,
    selected: Signal<Vec<api::LinkCandidate>>,
    tree: Signal<Vec<MenuRow>>,
) -> NodeHandle {
    let id = cand.id;
    let kind = candidate_kind(&cand);
    let title = if cand.title.is_empty() {
        cand.href.clone()
    } else {
        cand.title.clone()
    };
    let href = cand.href.clone();
    // Already in the tree? The tree can't change while the picker is open, so a one-shot read.
    if tree.get().iter().any(|r| r.kind == kind) {
        return rsx! {
            div { class: "candidate is-added",
                span { class: "candidate__check", "\u{2713}" }
                span { class: "candidate__title", {title} }
                span { class: "candidate__href", "already in menu" }
            }
        };
    }
    let cand_toggle = cand.clone();
    rsx! {
        button {
            r#type: "button",
            class: {move || if selected.get().iter().any(|c| c.id == id) { "candidate is-selected" } else { "candidate" }},
            onclick: move || selected.update(|v| {
                if let Some(pos) = v.iter().position(|c| c.id == id) {
                    v.remove(pos);
                } else {
                    v.push(cand_toggle.clone());
                }
            }),
            span { class: "candidate__check", {move || if selected.get().iter().any(|c| c.id == id) { "\u{2611}" } else { "\u{2610}" }} }
            span { class: "candidate__title", {title} }
            span { class: "candidate__href", {href} }
        }
    }
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
fn assign_menu_location(lc: LocCtx, location: &str, menu_id: Option<u64>) {
    lc.notice.set(String::new());
    let loc = location.to_owned();
    spawn_local(async move {
        match api::assign_location(&loc, menu_id).await {
            Ok(()) => reload_menu_locations(lc),
            Err(api::ApiError::Unauthorized) => lc.auth.session_expired(),
            Err(api::ApiError::Message(e)) => {
                lc.notice.set(e);
                reload_menu_locations(lc);
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

/// Where a dragged subtree is dropped. `Before` = as the anchor row's PREVIOUS SIBLING (at the
/// anchor's own depth); `Into` = as the target's LAST child (its depth + 1); `End` = appended at
/// the top level. The `String` is the anchor/target row's cid.
enum DropDest {
    Before(String),
    Into(String),
    End,
}

/// Move the subtree rooted at `src` to `dest`, re-basing every moved row's depth uniformly so the
/// only row that changes parent is the moved root (a `Before` drop places it at the anchor's own
/// depth, a `Into` under the target, an `End` at the top). Returns whether the tree changed.
/// REJECTS (no-op) a drop whose anchor/target lies inside the
/// moving subtree (the is-descendant guard — can't nest a node into its own subtree) and one that
/// would push any moved row past [`MAX_MENU_DEPTH`] (bounce, never silently clamp — matching the
/// keyboard indent's disable and the server's reject). Because it only reorders + re-depths
/// EXISTING rows (unchanged cid/kind/resolved), the keyed `for` relocates their DOM without a
/// rebuild (depth is excluded from `MenuRow`'s `PartialEq`, re-rendered by the reactive class).
/// Fuzz-proven (invariant + cid-multiset preserved) in `design/move-subtree.fuzz.mjs`.
fn move_subtree(tree: &mut Vec<MenuRow>, src: &str, dest: &DropDest) -> bool {
    let Some(i) = index_of(tree, src) else {
        return false;
    };
    let k = subtree_end(tree, i);
    // is-descendant / self guard on the ORIGINAL tree: the anchor/target must survive the cut.
    if let DropDest::Before(cid) | DropDest::Into(cid) = dest {
        let Some(a) = index_of(tree, cid) else {
            return false;
        };
        if a >= i && a < k {
            return false;
        }
    }
    let block: Vec<MenuRow> = tree[i..k].to_vec();
    let mut post: Vec<MenuRow> = Vec::with_capacity(tree.len() - block.len());
    post.extend_from_slice(&tree[..i]);
    post.extend_from_slice(&tree[k..]);

    // Insertion index + the moved root's new depth, computed in the POST-cut Vec so a gap's depth
    // never reads a row that was inside the cut block (must-fix).
    let (ins_at, new_root) = match dest {
        DropDest::End => (post.len(), 0u32),
        DropDest::Before(cid) => {
            let a = index_of(&post, cid).expect("anchor survived the cut");
            // Insert as the anchor's PREVIOUS SIBLING → the anchor's OWN depth. (Using the row
            // ABOVE the gap would land a first-child anchor's block at that shallower depth and
            // silently REPARENT the anchor's whole subtree under the dropped item — a valid,
            // cid-preserving tree the fuzz can't flag, so it's asserted in the fuzz's
            // parent-preservation check instead.)
            (a, post[a].depth)
        }
        DropDest::Into(cid) => {
            let t = index_of(&post, cid).expect("target survived the cut");
            (subtree_end(&post, t), post[t].depth + 1)
        }
    };
    // Re-base by delta (may be negative); reject if the deepest moved row would exceed the cap.
    let block_root = block[0].depth as i64;
    let block_max = block.iter().map(|r| r.depth).max().unwrap_or(0) as i64;
    let delta = new_root as i64 - block_root;
    if block_max + delta > MAX_MENU_DEPTH as i64 {
        return false;
    }
    let mut rebased = block;
    for r in &mut rebased {
        r.depth = (r.depth as i64 + delta) as u32;
    }
    post.splice(ins_at..ins_at, rebased);
    // A genuine no-op (same order + depths) must not dirty the menu.
    if post.len() == tree.len()
        && post
            .iter()
            .zip(tree.iter())
            .all(|(a, b)| a.cid == b.cid && a.depth == b.depth)
    {
        return false;
    }
    *tree = post;
    true
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

/// Apply a completed drag-drop: move the dragged subtree (from the [`DragContext`]) to `dest`,
/// clearing the hover hint and marking the tree dirty iff it changed. A no-op while a save is in
/// flight (B2, mirroring `apply_op`), and `drag.take()` always empties the payload so a bounced or
/// mid-save drop can't leave a stale drag armed.
fn apply_drop(
    saving: Signal<bool>,
    tree: Signal<Vec<MenuRow>>,
    dirty: Signal<bool>,
    drag: DragContext<String>,
    drop_hint: Signal<String>,
    dest: DropDest,
) {
    drop_hint.set(String::new());
    let Some(src) = drag.take() else {
        return;
    };
    if saving.get() {
        return;
    }
    let mut changed = false;
    tree.update(|t| {
        changed = move_subtree(t, &src, &dest);
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

/// Focus one element by id, on the next tick (so a just-mounted/reactively-swapped
/// element exists in the DOM first) — the Terms/TermEditor SF16 focus-management
/// idiom: a returned list's heading (`tabindex="-1"`, a landmark rather than a
/// control) or a form field a server error names (`#term-editor-slug`).
fn focus_field(id: &str) {
    use wasm_bindgen::JsCast;
    let id = id.to_owned();
    spawn_local(async move {
        TimeoutFuture::new(0).await;
        if let Some(doc) = web_sys::window().and_then(|w| w.document())
            && let Some(el) = doc.get_element_by_id(&id)
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
/// `guards`: one `(dirty, expected_view)` pair per unsaved-changes editor — SF16
/// widens this from the menu editor alone to also cover `View::TermEditor`, sharing
/// the ONE app-wide `view` signal (there is only ever one `Signal<View>`) rather
/// than installing a second, independent `beforeunload` listener.
fn install_beforeunload_guard(view: Signal<View>, guards: Vec<(Signal<bool>, View)>) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(
        move |e: web_sys::BeforeUnloadEvent| {
            let current = view.get();
            if guards
                .iter()
                .any(|(dirty, expected)| dirty.get() && current == *expected)
            {
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
