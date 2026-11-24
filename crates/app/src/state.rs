//! Pure application model: tabs, search results, messages and effects.
//!
//! Everything in this module is UI-framework agnostic and synchronous. The
//! Rune view only *emits* [`Msg`] values (through [`AppState::dispatch`]); the
//! runner in `runner.rs` calls [`AppState::update`] and executes the returned
//! [`Effect`]s. That keeps reducers deterministic and unit-testable while
//! still allowing background JSON and search work.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use rune::TextEdit;
use serde::{Deserialize, Serialize};

use crate::json_ops::{JsonAction, JsonError, ValidationInfo};
use crate::search::SearchHit;
use crate::tree_index::{self, IndexOutcome, RowsDispatch, TreeJob, TreeUi};

/// Stable identifier for a tab. Ids are never reused within a session.
pub type TabId = u64;

/// Which document of the active tab is shown in the editor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabView {
    /// The editable source text.
    #[default]
    Input,
    /// The formatted/minified result.
    Output,
}

/// Result of the latest explicit validation or failed JSON action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// The input is valid; these statistics came from the streaming parser.
    Valid(ValidationInfo),
    /// The input is invalid at the contained source location.
    Invalid(JsonError),
}

/// One JSON document.
///
/// `input` is the source of truth for the editor. `output` is the last
/// successful [`crate::json_ops`] result. `error` is the last JSON failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    /// Stable id, also used as the persistence key.
    pub id: TabId,
    /// Human-readable title shown in the tab strip (for example `JSON 1`).
    pub title: String,
    /// Source text.
    pub input: String,
    /// Last successfully formatted or minified text.
    #[serde(default)]
    pub output: String,
    /// Last error message, or `None`.
    #[serde(default)]
    pub error: Option<String>,
    /// A background JSON or search job is running for this tab.
    #[serde(default)]
    pub is_processing: bool,
    /// Which document the editor shows.
    #[serde(default)]
    pub view: TabView,
    /// Current search query for this tab.
    #[serde(default)]
    pub search_query: String,
    /// Hits from the last completed search.
    #[serde(default)]
    pub search_results: Vec<SearchHit>,
    /// Revision of `input`, bumped whenever the source text changes.
    #[serde(default)]
    pub input_revision: u64,
    /// Revision of `output`, assigned by the JSON worker on every success.
    #[serde(default)]
    pub output_revision: u64,
    /// Latest validation display. Transient because an edit invalidates it.
    #[serde(skip)]
    pub validation: Option<ValidationResult>,
}

impl Tab {
    /// Creates an empty tab with the given id and title.
    pub fn new(id: TabId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            input: String::new(),
            output: String::new(),
            error: None,
            is_processing: false,
            view: TabView::Input,
            search_query: String::new(),
            search_results: Vec::new(),
            input_revision: 0,
            output_revision: 0,
            validation: None,
        }
    }

    /// Returns the structured validation failure, if the latest result failed.
    pub fn validation_error(&self) -> Option<&JsonError> {
        match &self.validation {
            Some(ValidationResult::Invalid(error)) => Some(error),
            _ => None,
        }
    }
}

/// Everything the app needs to draw a frame.
///
/// The `editors` map holds Rune's [`TextEdit`] values. They are UI state, not
/// domain state: `input` on [`Tab`] remains the source of truth, and
/// [`AppState::update_input`] keeps the editor in sync when something other
/// than typing changes the text (drop, import, clear).
#[derive(Debug, Clone, PartialEq)]
pub struct AppState {
    /// Open tabs, left to right.
    pub tabs: Vec<Tab>,
    /// Id of the visible tab. Always one of `tabs`.
    pub active: TabId,
    /// Next tab id to hand out.
    pub next_id: TabId,
    /// Counter used for default titles (`JSON 1`, `JSON 2`, ...).
    pub tab_counter: u32,
    /// Transient status line text.
    pub status: Option<String>,
    /// Rune text fields, one per tab.
    pub editors: HashMap<TabId, TextEdit>,
    /// Rune text field backing the search box.
    pub search_edit: TextEdit,
    /// Rune text field backing the tree filter box.
    pub tree_filter_edit: TextEdit,
    /// Rune text field holding the selected tree value's raw JSON for editing.
    pub tree_value_edit: TextEdit,
    /// Per-tab tree explorer state (see `tree_index::TreeUi`). A tab is in
    /// tree mode when its entry exists and `open` is set; the entry, index
    /// and rows are runtime-only and never persisted with `Tab`.
    pub tree: HashMap<TabId, TreeUi>,
    /// Messages queued by view handlers, drained by the runner.
    pub pending: Vec<Msg>,
    /// Invalidation counter for code folding. The fold *set* lives in the
    /// revision-keyed store in `crate::fold_index` (the view only has
    /// `&AppState`, but a reveal must be able to unfold a line before
    /// `scroll_into_view`). Fold handlers bump this so Rune re-runs the view
    /// after a fold/unfold; the value is not otherwise read by the model.
    pub fold_epoch: u64,
    /// First half of the `Ctrl+K` fold chord is pending. Rune key handlers
    /// receive one `KeyEvent` at a time (no built-in two-key chords), so the
    /// keyboard layer records the prefix here and completes it on the next
    /// key; any other key clears it. Presentation-only, like `fold_epoch`.
    pub fold_chord: bool,
    /// Last drawn vertical offset for each `(tab, view)` document viewport.
    /// Runtime-only: scroll positions are intentionally not persisted.
    pub(crate) scroll_offsets: Rc<RefCell<HashMap<String, f32>>>,
}

impl Default for AppState {
    fn default() -> Self {
        let mut state = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            tab_counter: 0,
            status: None,
            editors: HashMap::new(),
            search_edit: TextEdit::default(),
            tree_filter_edit: TextEdit::default(),
            tree_value_edit: TextEdit::default(),
            tree: HashMap::new(),
            pending: Vec::new(),
            fold_epoch: 0,
            fold_chord: false,
            scroll_offsets: Rc::new(RefCell::new(HashMap::new())),
        };
        state.add_tab();
        state
    }
}

impl AppState {
    /// Starts with one empty `JSON 1` tab, mirroring the original Redux state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the tab with `id`, if it exists.
    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    /// Returns the tab with `id`, if it exists.
    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Returns the active tab. The invariant is that this is always `Some` for
    /// a state built through the reducers.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tab(self.active)
    }

    /// Returns the active tab mutably.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tab_mut(self.active)
    }

    /// Returns the Rune editor for `id`.
    pub fn editor(&self, id: TabId) -> Option<&TextEdit> {
        self.editors.get(&id)
    }

    /// Returns the Rune editor for the active tab.
    pub fn active_editor(&self) -> Option<&TextEdit> {
        self.editor(self.active)
    }

    /// Queues a message. View handlers call this; the message is applied on
    /// the next runner poll.
    pub fn dispatch(&mut self, msg: Msg) {
        self.pending.push(msg);
    }

    /// Takes every queued message, leaving the queue empty.
    pub fn take_pending(&mut self) -> Vec<Msg> {
        std::mem::take(&mut self.pending)
    }

    /// Copies the active editor's text into the model. Call from
    /// `on_input_change`; the editor owns the caret and selection.
    pub fn sync_active_editor(&mut self) {
        let id = self.active;
        let Some(edit) = self.editors.get(&id) else {
            return;
        };
        let text = edit.text.clone();
        if let Some(tab) = self.tab_mut(id) {
            if tab.input != text {
                tab.input = text;
                tab.input_revision = crate::line_index::next_revision();
                tab.error = None;
                tab.validation = None;
            }
        }
    }

    /// Adds a tab, makes it active and returns its id.
    pub fn add_tab(&mut self) -> TabId {
        self.tab_counter += 1;
        let id = self.next_id;
        self.next_id += 1;
        let title = format!("JSON {}", self.tab_counter);
        self.tabs.push(Tab::new(id, title));
        self.editors.insert(id, TextEdit::default());
        self.active = id;
        id
    }

    /// Removes a tab. The last remaining tab is never removed (the original UI
    /// hides the close button in that case). Selecting the neighbour of the
    /// removed active tab matches the React reducer's behaviour.
    pub fn remove_tab(&mut self, id: TabId) -> Option<Tab> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let index = self.tabs.iter().position(|t| t.id == id)?;
        let tab = self.tabs.remove(index);
        self.editors.remove(&id);
        self.tree.remove(&id);
        self.scroll_offsets
            .borrow_mut()
            .retain(|key, _| !key.starts_with(&format!("document-scroll-{id}-")));
        crate::editor::state::remove(id);
        if self.active == id {
            let next = index.min(self.tabs.len() - 1);
            self.active = self.tabs[next].id;
        }
        Some(tab)
    }

    /// Selects `id`. Returns `false` when the tab does not exist.
    pub fn set_active(&mut self, id: TabId) -> bool {
        if self.tab(id).is_some() {
            self.active = id;
            true
        } else {
            false
        }
    }

    /// Replaces the source text of `id` and clears its error. The Rune editor
    /// is updated too, unless the change came from that editor.
    pub fn update_input(&mut self, id: TabId, input: String) {
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        tab.input = input.clone();
        tab.input_revision = crate::line_index::next_revision();
        tab.error = None;
        tab.validation = None;
        // H2: drop any rope entry now. `sync()` repairs a stale buffer only
        // on the next Input-view access; while the tab shows Output/Tree a
        // dirty entry would keep the *old* document, and the no-sync
        // accessors (`existing_snapshot` for jobs, `dirty_snapshot` for
        // persist) would format/save it. The next view build recreates the
        // entry from the new text via `with()`.
        crate::editor::state::remove(id);
        if let Some(edit) = self.editors.get_mut(&id) {
            if edit.text != input {
                edit.replace_text(input);
            }
        }
        // New text means a new tree: the old index is dropped and rebuilt
        // only if the tab is showing the tree.
        self.refresh_tree(id);
    }

    /// Copies the rope into `Tab::input` when editor edits are pending.
    /// Explicit consumers call this (paste, copy/export, tree builds,
    /// validation display, persist); the keystroke path never does.
    pub fn materialize_input(&mut self, id: TabId) {
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        let dirty = crate::editor::state::is_dirty(id, &tab.input, tab.input_revision);
        if !dirty {
            return;
        }
        let text =
            crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.to_string());
        tab.input = text;
        crate::editor::state::mark_clean(id, &tab.input, tab.input_revision);
    }

    /// Applies one editor command to the tab's rope and keeps the tab
    /// bookkeeping honest: a document change bumps `input_revision` (which
    /// keys the line index, tree and validation staleness), clears the last
    /// error/validation, and refreshes an open tree. Caret/selection-only
    /// commands change no revision. When the document crosses the
    /// empty/non-empty boundary the input is materialized at once so the
    /// toolbar's enabled state stays correct without a per-keystroke copy.
    fn apply_editor_action(&mut self, id: TabId, action: crate::editor::state::EditorAction) {
        let revision = crate::line_index::next_revision();
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        let (changed, empty_changed) =
            crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
                let was_empty = ed.buffer.is_empty();
                let changed = ed.apply(action);
                if changed {
                    // Keep the store's sync marker ahead of the revision
                    // bump so the next access does not rebuild the rope.
                    ed.source_revision = revision;
                }
                (changed, was_empty != ed.buffer.is_empty())
            });
        if changed {
            tab.input_revision = revision;
            tab.error = None;
            tab.validation = None;
        }
        if empty_changed {
            self.materialize_input(id);
        }
        if changed {
            self.refresh_tree(id);
        }
    }

    /// Clears input, output, error and search results, and switches back to
    /// the input view, mirroring the original `clearTab` action.
    pub fn clear_tab(&mut self, id: TabId) {
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        tab.input.clear();
        tab.output.clear();
        tab.input_revision = crate::line_index::next_revision();
        tab.output_revision = crate::line_index::next_revision();
        tab.error = None;
        tab.validation = None;
        tab.is_processing = false;
        tab.view = TabView::Input;
        tab.search_query.clear();
        tab.search_results.clear();
        // H2 (same class as update_input): Clear persists at once; a dirty
        // stale rope must not save the previous document over the clear.
        crate::editor::state::remove(id);
        if let Some(edit) = self.editors.get_mut(&id) {
            edit.replace_text("");
        }
        // Clearing is the start of a paste-new-JSON flow: leave the tree
        // (the input view is where pasting happens) and drop its index.
        if let Some(tree) = self.tree.get_mut(&id) {
            tree.open = false;
            tree.invalidate();
        }
    }

    /// Applies a finished formatting job. A success replaces `output`; a
    /// failure replaces `error` and leaves the previous output untouched.
    /// Returns `true` on success.
    pub fn apply_json_result(&mut self, id: TabId, result: Result<String, JsonError>) -> bool {
        let revision = crate::line_index::next_revision();
        // Synchronous callers apply to the tab's current document.
        let input_revision = self.tab(id).map_or(0, |tab| tab.input_revision);
        self.apply_json_result_with_revision(id, revision, input_revision, result)
    }

    /// Applies a finished job whose output revision was assigned by the JSON
    /// worker, so the prewarmed line index matches the one the view requests.
    /// `input_revision` is the revision the job was started for; a result
    /// for an edited document is dropped (H3, mirrors `apply_validation_result`).
    fn apply_json_result_with_revision(
        &mut self,
        id: TabId,
        revision: u64,
        input_revision: u64,
        result: Result<String, JsonError>,
    ) -> bool {
        // rp-format-inplace: the worker prepared the Input replacement rope
        // off the UI thread. Take it even for stale/failed/closed-tab results
        // so a prepared rope never leaks.
        let prepared = crate::editor::replace::take(revision);
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        if tab.input_revision != input_revision {
            // The document moved on while the job ran; never publish output
            // for text the tab no longer holds, and never leave the toolbar
            // stuck in the busy state.
            tab.is_processing = false;
            return false;
        }
        tab.is_processing = false;
        match result {
            Ok(output) => {
                let replacement = match prepared {
                    Some(crate::editor::replace::Prepared::Rope(rope)) => Some(rope),
                    Some(crate::editor::replace::Prepared::Unchanged) => None,
                    // Synchronous caller (tests, bins): no worker prepared a
                    // rope; build one inline unless the text is unchanged.
                    None => (tab.input != output).then(|| ropey::Rope::from_str(&output)),
                };
                tab.output = output;
                tab.output_revision = revision;
                tab.error = None;
                tab.validation = None;
                // The original `processJsonContent.fulfilled` reducer switched
                // the active content to Output, so Format/Minify visibly
                // produced a result instead of leaving the user on Input.
                tab.view = TabView::Output;
                if let Some(rope) = replacement {
                    self.replace_input_in_place(id, rope);
                }
                true
            }
            Err(err) => {
                tab.error = Some(err.to_string());
                tab.validation = Some(ValidationResult::Invalid(err));
                false
            }
        }
    }

    /// rp-format-inplace: a successful Format/Minify rewrites the Input
    /// document itself as ONE undo transaction in the rope editor (Ctrl+Z
    /// restores the previous text and caret). The revision bump keeps
    /// search/validate/tree staleness honest; no second job is spawned, and
    /// Output receives the same text through the caller.
    fn replace_input_in_place(&mut self, id: TabId, new_rope: ropey::Rope) {
        let revision = crate::line_index::next_revision();
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        if crate::editor::state::existing_snapshot(id).is_some() {
            // Live editor: swap the rope in place. The old rope is an O(1)
            // clone in the undo step, and `Tab::input` stays a stale cache
            // until an explicit consumer materializes it (the typing
            // pattern); the dirty flag routes jobs/persist to the rope.
            crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
                ed.replace_document(new_rope);
                // Keep the store's sync marker ahead of the revision bump
                // so the next access does not rebuild the rope.
                ed.source_revision = revision;
            });
        } else {
            // No live editor (a restored tab never opened): seed one. The
            // old cache moves into the undo step (no copy, no old-rope build
            // on the UI thread); the new cache is one copy so the toolbar
            // and job gates keep reading a truthful `Tab::input`.
            let previous = std::mem::take(&mut tab.input);
            tab.input = tab.output.clone();
            crate::editor::state::insert_replaced(id, previous, new_rope, revision);
        }
        tab.input_revision = revision;
        tab.error = None;
        tab.validation = None;
        self.refresh_tree(id);
    }

    /// Applies validation if the tab still contains the revision that was
    /// checked. Returns `false` for a closed tab or a stale worker result.
    pub fn apply_validation_result(
        &mut self,
        id: TabId,
        input_revision: u64,
        result: Result<ValidationInfo, JsonError>,
    ) -> bool {
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        if tab.input_revision != input_revision {
            // The edited document no longer has a useful validation job to
            // wait for; never leave its toolbar stuck in the busy state.
            tab.is_processing = false;
            return false;
        }
        tab.is_processing = false;
        match result {
            Ok(info) => {
                tab.error = None;
                tab.validation = Some(ValidationResult::Valid(info));
            }
            Err(error) => {
                tab.error = Some(error.to_string());
                tab.validation = Some(ValidationResult::Invalid(error));
            }
        }
        true
    }

    /// Marks a tab as running (or no longer running) background work.
    pub fn set_processing(&mut self, id: TabId, processing: bool) {
        if let Some(tab) = self.tab_mut(id) {
            tab.is_processing = processing;
        }
    }

    /// Switches which document the active tab shows.
    pub fn set_view(&mut self, id: TabId, view: TabView) -> bool {
        match self.tab_mut(id) {
            Some(tab) => {
                tab.view = view;
                true
            }
            None => false,
        }
    }

    /// Stores the query for `id` and clears stale results.
    pub fn set_search_query(&mut self, id: TabId, query: String) {
        if let Some(tab) = self.tab_mut(id) {
            tab.search_query = query;
            tab.search_results.clear();
        }
    }

    /// Applies completed search hits.
    pub fn apply_search_results(&mut self, id: TabId, hits: Vec<SearchHit>) {
        if let Some(tab) = self.tab_mut(id) {
            tab.search_results = hits;
            tab.is_processing = false;
        }
    }

    // ── Tree explorer ────────────────────────────────────────────────────
    //
    // The tree's compact index is built by `tree_index` off the UI thread and
    // delivered through `AppHandle::update` (or synchronously in tests and
    // headless runs, where no handle is registered). No new `Effect` is
    // involved, so `runner.rs` stays untouched; copy actions reuse
    // `Effect::Copy`.

    /// Marks `id`'s tree state stale (input changed) and rebuilds the index
    /// if the tab is showing the tree.
    fn refresh_tree(&mut self, id: TabId) {
        let open = self.tree.get(&id).is_some_and(|tree| tree.open);
        if let Some(tree) = self.tree.get_mut(&id) {
            tree.invalidate();
        }
        if open {
            self.ensure_tree_index(id);
        }
    }

    /// Shows `query` in the shared tree filter field. The field is one
    /// `TextEdit` across tabs, so it is re-synced whenever the active tab
    /// changes, the tree is (re)entered, or a filter arrives from anywhere
    /// other than typing (restored sessions, scripts, other tabs). A field
    /// that already holds the text is left alone: `replace_text` would
    /// reset the caret and selection.
    fn sync_tree_filter_field(&mut self, query: &str) {
        if self.tree_filter_edit.text != query {
            self.tree_filter_edit.replace_text(query.to_owned());
        }
    }

    /// Makes sure the tab showing the tree has a fresh index for its current
    /// input. Idempotent: skips when the index covers `input_revision` or a
    /// build for it is already in flight.
    fn ensure_tree_index(&mut self, id: TabId) {
        // The tree index reads `Tab::input`; refresh the editor cache first
        // (one O(n) copy per tree build, never per keystroke).
        self.materialize_input(id);
        let Some(tab) = self.tab(id) else {
            return;
        };
        if tab.input.is_empty() {
            if let Some(tree) = self.tree.get_mut(&id) {
                tree.building = false;
                tree.error = None;
            }
            return;
        }
        let revision = tab.input_revision;
        if let Some(tree) = self.tree.get(&id) {
            if tree.index.is_some() && tree.index_revision == revision {
                return;
            }
            if tree.build_pending_revision == Some(revision) {
                return;
            }
        }
        let text: Arc<str> = Arc::from(tab.input.as_str());
        let tree = self.tree.entry(id).or_insert_with(|| TreeUi::new(false));
        tree.building = true;
        tree.build_pending_revision = Some(revision);
        match tree_index::ensure_index(text, move |outcome| {
            Msg::TreeIndexDone {
                tab: id,
                revision,
                outcome,
            }
        }) {
            tree_index::IndexSync::Spawned => {}
            tree_index::IndexSync::Sync(outcome) => {
                self.apply_tree_index(id, revision, outcome);
            }
        }
    }

    /// Applies a finished index build. Stale deliveries (input changed, a
    /// newer build superseded, tab gone) are dropped.
    fn apply_tree_index(&mut self, id: TabId, revision: u64, outcome: IndexOutcome) {
        let current = self.tab(id).map_or(u64::MAX, |tab| tab.input_revision);
        let Some(tree) = self.tree.get_mut(&id) else {
            return;
        };
        if tree.build_pending_revision != Some(revision) {
            return;
        }
        tree.build_pending_revision = None;
        tree.building = false;
        if current != revision {
            // The text moved on; the change already queued a rebuild.
            return;
        }
        match outcome {
            IndexOutcome::Ready(index) => {
                let filter = tree.filter.clone();
                tree.install_index(index, revision);
                if !filter.is_empty() {
                    // A new document re-runs the active filter; the fresh
                    // rows are the base that clearing the filter restores.
                    tree.save_filter_base();
                    self.start_tree_job(id, TreeJob::Filter(filter));
                }
                let pending = self.tree.get_mut(&id).and_then(|tree| tree.pending_reveal.take());
                if let Some(path) = pending {
                    self.tree_reveal_path(id, &path);
                }
            }
            IndexOutcome::Failed(error) => {
                tree.error = Some(error.to_string());
            }
        }
    }

    /// Expands every container along `path` so its target row exists, then
    /// selects that row. Used by the search-results hook in tree mode.
    fn tree_reveal_path(&mut self, id: TabId, path: &str) {
        let Some(index) = self.tree.get(&id).and_then(|tree| tree.index.clone()) else {
            return;
        };
        // `self.tabs` (read) and `self.tree` (mutated) are disjoint fields.
        let input: &str = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.input.as_str())
            .unwrap_or("");
        let mut node = 0u32;
        for segment in path.split(" → ") {
            match index.child_by_segment(input, node, segment) {
                Some(next) => {
                    if index.is_container(node) {
                        if let Some(tree) = self.tree.get_mut(&id) {
                            tree.expand(&index, input, node);
                        }
                    }
                    node = next;
                }
                None => return, // stale path: nothing to reveal
            }
        }
        if let Some(tree) = self.tree.get_mut(&id) {
            tree.select(&index, input, node);
        }
        self.sync_tree_value_field(id);
    }

    /// Shows the selected scalar's raw JSON in the tree value field (empty
    /// for containers or no selection).
    fn sync_tree_value_field(&mut self, id: TabId) {
        let text = self
            .tree
            .get(&id)
            .and_then(|tree| tree.selected.as_ref())
            .filter(|selection| !selection.is_container)
            .and_then(|selection| {
                let start = selection.value_off as usize;
                let end = start + selection.value_len as usize;
                self.tab(id)?.input.get(start..end).map(str::to_owned)
            })
            .unwrap_or_default();
        if self.tree_value_edit.text != text {
            self.tree_value_edit.replace_text(text);
        }
    }

    /// Writes `tree_value_edit` over the selected scalar in the document as
    /// one undo step (Ctrl+Z in Input/Output restores it). The text must be
    /// one JSON value; the tree rebuilds and re-selects the edited path.
    fn tree_edit_value(&mut self, id: TabId) {
        let new_text = self.tree_value_edit.text.trim().to_owned();
        let selection = self.tree.get(&id).and_then(|tree| tree.selected.clone());
        let fresh = self.tree.get(&id).is_some_and(|tree| {
            tree.index_revision == self.tab(id).map_or(0, |tab| tab.input_revision)
        });
        let Some(selection) = selection.filter(|s| fresh && !s.is_container) else {
            self.status = Some("Select a value to edit".into());
            return;
        };
        if let Err(error) = tree_index::TreeIndex::build(&new_text) {
            self.status = Some(format!("Invalid JSON value: {}", error.message));
            return;
        }
        let start = selection.value_off as usize;
        let end = start + selection.value_len as usize;
        let Some(old) = self.tab(id).and_then(|tab| tab.input.get(start..end)) else {
            self.status = Some("Value no longer available".into());
            return;
        };
        if old == new_text {
            return;
        }
        use crate::editor::state::EditorAction;
        if let Some(tree) = self.tree.get_mut(&id) {
            tree.pending_reveal = Some(selection.path.clone());
        }
        self.apply_editor_action(id, EditorAction::SetCaret(start, false));
        self.apply_editor_action(id, EditorAction::SetCaret(end, true));
        self.apply_editor_action(
            id,
            EditorAction::Insert(new_text, crate::editor::EditKind::Paste),
        );
        self.materialize_input(id);
        // A synchronous rebuild already ran inside the edit; make sure the
        // index exists for the new revision either way.
        self.ensure_tree_index(id);
        self.status = Some(format!("Updated {}", selection.path));
    }

    /// Starts an off-thread rows job (expand-all on a big doc, sort rebuild,
    /// filter), or runs it inline without an app handle.
    fn start_tree_job(&mut self, id: TabId, job: TreeJob) {
        // Gather everything first: `tab` reads and the `tree` mutation below
        // borrow different fields of `self`, so no borrow may be held across.
        let Some(index) = self.tree.get(&id).and_then(|tree| tree.index.clone()) else {
            return;
        };
        let sort = self.tree.get(&id).is_some_and(|tree| tree.sort_keys);
        let text: Arc<str> =
            Arc::from(self.tabs.iter().find(|tab| tab.id == id).map(|tab| tab.input.as_str()).unwrap_or(""));
        let gen = {
            let Some(tree) = self.tree.get_mut(&id) else {
                return;
            };
            tree.jobs_gen += 1;
            tree.job_pending = true;
            tree.job_kind = job.label();
            tree.jobs_gen
        };
        let cancel = tree_index::register_job_cancel(id);
        match tree_index::dispatch_rows_job(id, gen, index, text, sort, job, cancel) {
            RowsDispatch::Spawned => {}
            RowsDispatch::Done(outcome) => {
                if let Some(outcome) = outcome {
                    self.apply_tree_rows(id, gen, outcome);
                } else {
                    self.clear_tree_job(id, gen);
                }
            }
        }
    }

    /// Applies finished rows from a job, generation-gated.
    fn apply_tree_rows(
        &mut self,
        id: TabId,
        gen: u64,
        outcome: tree_index::RowsOutcome,
    ) {
        let Some(tree) = self.tree.get_mut(&id) else {
            return;
        };
        if tree.jobs_gen != gen {
            return;
        }
        tree.job_pending = false;
        tree.job_kind = "";
        tree.apply_rows(outcome.rows, outcome.matched);
    }

    /// Clears a cancelled/failed job's pending flag, generation-gated.
    fn clear_tree_job(&mut self, id: TabId, gen: u64) {
        let Some(tree) = self.tree.get_mut(&id) else {
            return;
        };
        if tree.jobs_gen == gen {
            tree.job_pending = false;
            tree.job_kind = "";
        }
    }

    /// Toggles sort A→Z. Small docs re-flatten inline; big ones (and any
    /// active filter, which re-runs with the new order) go through a job.
    fn tree_sort_toggled(&mut self, id: TabId) {
        let Some(tree) = self.tree.get_mut(&id) else {
            return;
        };
        tree.sort_keys = !tree.sort_keys;
        let filter = tree.filter.clone();
        if !filter.is_empty() {
            self.start_tree_job(id, TreeJob::Filter(filter));
            return;
        }
        let index = tree.index.clone();
        let node_count = index.as_ref().map_or(0, |index| index.node_count());
        if node_count > tree_index::SYNC_ROW_LIMIT {
            let expanded: Vec<u32> = tree
                .rows
                .iter()
                .filter(|row| row.expanded)
                .map(|row| row.node)
                .collect();
            self.start_tree_job(id, TreeJob::RebuildSorted(expanded));
        } else if let Some(index) = index {
            let expanded: std::collections::HashSet<u32> =
                tree.rows.iter().filter(|row| row.expanded).map(|row| row.node).collect();
            // `self.tabs` (read) and `self.tree` (mutated) are disjoint fields.
            let input: &str = self
                .tabs
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| tab.input.as_str())
                .unwrap_or("");
            if let Some(tree) = self.tree.get_mut(&id) {
                tree.rebuild_all(&index, input, &expanded);
            }
        }
    }

    /// Replaces the whole tab set after loading from disk. Empty input is
    /// ignored so the app always has a tab.
    pub fn replace_tabs(&mut self, tabs: Vec<Tab>) {
        if tabs.is_empty() {
            return;
        }
        self.tab_counter = tabs
            .iter()
            .filter_map(|t| t.title.strip_prefix("JSON "))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(tabs.len() as u32)
            .max(tabs.len() as u32);
        self.next_id = tabs.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        self.active = tabs[0].id;
        self.editors = tabs
            .iter()
            .map(|t| (t.id, TextEdit::new(t.input.clone())))
            .collect();
        // Restored tabs get fresh tree state (indexes are never persisted).
        self.tree.clear();
        self.scroll_offsets.borrow_mut().clear();
        crate::editor::state::clear_all();
        self.tabs = tabs;
        self.status = None;
    }

    /// Applies one message and returns the side effects the runner must
    /// perform. This function performs no I/O and never blocks.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::AddTab => {
                self.add_tab();
                Vec::new()
            }
            Msg::CloseTab(id) => {
                if self.remove_tab(id).is_some() {
                    vec![Effect::Persist]
                } else {
                    Vec::new()
                }
            }
            Msg::SelectTab(id) => {
                self.set_active(id);
                // A restored or long-idle tree tab may need a fresh index.
                if self.tree.get(&id).is_some_and(|tree| tree.open) {
                    self.ensure_tree_index(id);
                }
                // The tree filter field is shared: it always shows the
                // active tab's filter.
                if let Some(filter) = self.tree.get(&id).map(|tree| tree.filter.clone()) {
                    self.sync_tree_filter_field(&filter);
                }
                Vec::new()
            }
            Msg::SwitchView(view) => {
                // Choosing a code view explicitly leaves the tree explorer,
                // so the Input/Output buttons (and search-result jumps, which
                // dispatch `SwitchView(Output)`) behave as expected.
                let id = self.active;
                if let Some(tree) = self.tree.get_mut(&id) {
                    tree.open = false;
                }
                self.set_view(id, view);
                Vec::new()
            }
            Msg::InputChanged(text) => {
                self.update_input(self.active, text);
                Vec::new()
            }
            Msg::EditorEdit { tab, action } => {
                self.apply_editor_action(tab, action);
                Vec::new()
            }
            Msg::EditorPaste { tab, text } => {
                self.apply_editor_action(
                    tab,
                    crate::editor::state::EditorAction::Insert(
                        text,
                        crate::editor::EditKind::Paste,
                    ),
                );
                // Paste is a bulk action: keep `Tab::input` fresh for the
                // toolbar, status line and any immediate consumer.
                self.materialize_input(tab);
                Vec::new()
            }
            Msg::EditorCopy { tab, cut } => {
                let Some(t) = self.tab(tab) else {
                    return Vec::new();
                };
                let selection = crate::editor::state::with(tab, &t.input, t.input_revision, |ed| {
                    ed.selected_text()
                });
                match selection {
                    Some(text) if !text.is_empty() => {
                        if cut {
                            self.apply_editor_action(
                                tab,
                                crate::editor::state::EditorAction::Backspace,
                            );
                        }
                        vec![Effect::Copy(text)]
                    }
                    _ => {
                        self.status = Some("Nothing to copy".into());
                        Vec::new()
                    }
                }
            }
            Msg::RopeMaterialized { tab, input, job } => {
                // H3: the input may have changed while the rope was being
                // converted off-thread. Starting the job anyway would cancel
                // a newer per-tab job and publish a stale result; drop it.
                if self
                    .tab(tab)
                    .is_some_and(|t| t.input_revision != job.input_revision())
                {
                    if let Some(t) = self.tab_mut(tab) {
                        t.is_processing = false;
                    }
                    return Vec::new();
                }
                match job {
                    RopeJob::Json { action, input_revision } => vec![Effect::SpawnJson {
                        tab,
                        input,
                        action,
                        input_revision,
                    }],
                    RopeJob::Validate { input_revision } => vec![Effect::SpawnValidation {
                        tab,
                        input,
                        input_revision,
                    }],
                    RopeJob::Search { query, input_revision } => vec![Effect::Search {
                        tab,
                        input,
                        query,
                        input_revision,
                    }],
                }
            }
            Msg::Clear => {
                self.clear_tab(self.active);
                vec![Effect::Persist]
            }
            Msg::Format => self.start_json(JsonAction::Format),
            Msg::Minify => self.start_json(JsonAction::Minify),
            Msg::Validate => self.start_validation(),
            Msg::Copy => {
                self.materialize_input(self.active);
                let Some(tab) = self.active_tab() else {
                    return Vec::new();
                };
                // Input and Output both show the live document (Format and
                // Minify rewrite it in place), so copy what the user sees.
                let text = tab.input.clone();
                if text.is_empty() {
                    self.status = Some("Nothing to copy".into());
                    Vec::new()
                } else {
                    vec![Effect::Copy(text)]
                }
            }
            Msg::Export => {
                self.materialize_input(self.active);
                let Some(tab) = self.active_tab() else {
                    return Vec::new();
                };
                let content = tab.input.clone();
                if content.is_empty() {
                    self.status = Some("Nothing to export".into());
                    return Vec::new();
                }
                vec![Effect::Export {
                    default_name: default_export_name(tab),
                    content,
                }]
            }
            Msg::OpenFile => vec![Effect::Open],
            Msg::FileDropped(path) => vec![Effect::ReadDroppedFile(path)],
            Msg::FileLoaded { path: _, content } => {
                self.update_input(self.active, content);
                Vec::new()
            }
            Msg::FileLoadedFor { tab, path: _, content } => {
                // M3: land in the tab that requested the read, even if the
                // user switched tabs while the file was being read.
                self.update_input(tab, content);
                Vec::new()
            }
            Msg::SearchChanged(query) => {
                let id = self.active;
                let Some(tab) = self.tab_mut(id) else {
                    return Vec::new();
                };
                tab.search_query = query.clone();
                tab.search_results.clear();
                if query.len() < 2 {
                    tab.is_processing = false;
                    return Vec::new();
                }
                tab.is_processing = true;
                let input_revision = tab.input_revision;
                vec![self.spawn_input_job(id, RopeJob::Search { query, input_revision })]
            }
            Msg::JsonDone {
                tab,
                revision,
                input_revision,
                result,
            } => {
                let ok =
                    self.apply_json_result_with_revision(tab, revision, input_revision, result);
                if ok {
                    vec![Effect::Persist]
                } else {
                    Vec::new()
                }
            }
            Msg::ValidationDone {
                tab,
                input_revision,
                result,
            } => {
                // The error panel and Go-to-error read `Tab::input` for
                // source context; refresh the cache off the keystroke path.
                self.materialize_input(tab);
                self.apply_validation_result(tab, input_revision, result);
                Vec::new()
            }
            Msg::SearchDone { tab, hits } => {
                self.apply_search_results(tab, hits);
                Vec::new()
            }
            Msg::SearchDoneFor { tab, input_revision, query, hits } => {
                // H3 (mirrors apply_validation_result): drop hits for a
                // document or query that moved on while the walk ran, and
                // never leave the toolbar stuck in the busy state.
                let fresh = self
                    .tab(tab)
                    .is_some_and(|t| t.input_revision == input_revision && t.search_query == query);
                if fresh {
                    self.apply_search_results(tab, hits);
                } else if let Some(t) = self.tab_mut(tab) {
                    t.is_processing = false;
                }
                Vec::new()
            }
            Msg::TabsLoaded(tabs) => {
                self.replace_tabs(tabs);
                Vec::new()
            }
            Msg::TreeOpen => {
                let id = self.active;
                if let Some(tree) = self.tree.get_mut(&id) {
                    tree.open = true;
                } else {
                    self.tree.insert(id, TreeUi::new(true));
                }
                // Entering (or re-entering) the tree shows this tab's filter
                // in the shared field.
                let filter = self
                    .tree
                    .get(&id)
                    .map(|tree| tree.filter.clone())
                    .unwrap_or_default();
                self.sync_tree_filter_field(&filter);
                self.ensure_tree_index(id);
                Vec::new()
            }
            Msg::TreeClose => {
                let id = self.active;
                if let Some(tree) = self.tree.get_mut(&id) {
                    tree.open = false;
                }
                Vec::new()
            }
            Msg::TreeExpand { tab, node } => {
                // The filtered view is read-only for expansion.
                if !self.tree.get(&tab).is_some_and(|tree| tree.filter_active()) {
                    let index = self.tree.get(&tab).and_then(|tree| tree.index.clone());
                    if let Some(index) = index {
                        // `self.tabs` (read) and `self.tree` (mutated) are
                        // disjoint field borrows.
                        let input: &str = self
                            .tabs
                            .iter()
                            .find(|t| t.id == tab)
                            .map(|t| t.input.as_str())
                            .unwrap_or("");
                        if let Some(tree) = self.tree.get_mut(&tab) {
                            tree.expand(&index, input, node);
                        }
                    }
                }
                Vec::new()
            }
            Msg::TreeCollapse { tab, node } => {
                if !self.tree.get(&tab).is_some_and(|tree| tree.filter_active()) {
                    if let Some(tree) = self.tree.get_mut(&tab) {
                        tree.collapse(node);
                    }
                }
                Vec::new()
            }
            Msg::TreeSelect { tab, node } => {
                let index = self.tree.get(&tab).and_then(|tree| tree.index.clone());
                if let Some(index) = index {
                    let input: &str = self
                        .tabs
                        .iter()
                        .find(|t| t.id == tab)
                        .map(|t| t.input.as_str())
                        .unwrap_or("");
                    if let Some(tree) = self.tree.get_mut(&tab) {
                        tree.select(&index, input, node);
                    }
                }
                self.sync_tree_value_field(tab);
                Vec::new()
            }
            Msg::TreeEditValue { tab } => {
                self.tree_edit_value(tab);
                Vec::new()
            }
            Msg::TreeRevealPath { tab, path } => {
                self.tree_reveal_path(tab, &path);
                Vec::new()
            }
            Msg::TreeExpandAll { tab } => {
                let node_count = self
                    .tree
                    .get(&tab)
                    .and_then(|tree| tree.index.as_ref())
                    .map(|index| index.node_count())
                    .unwrap_or(0);
                if node_count == 0 || self.tree.get(&tab).is_some_and(|tree| tree.filter_active())
                {
                    Vec::new()
                } else if node_count <= tree_index::SYNC_ROW_LIMIT {
                    let index = self.tree.get(&tab).and_then(|tree| tree.index.clone());
                    if let Some(index) = index {
                        let mut expanded = std::collections::HashSet::with_capacity(node_count);
                        for node in 0..index.node_count() as u32 {
                            if index.is_container(node) {
                                expanded.insert(node);
                            }
                        }
                        let input: &str = self
                            .tabs
                            .iter()
                            .find(|t| t.id == tab)
                            .map(|t| t.input.as_str())
                            .unwrap_or("");
                        if let Some(tree) = self.tree.get_mut(&tab) {
                            tree.rebuild_all(&index, input, &expanded);
                        }
                    }
                    Vec::new()
                } else {
                    // Expanding a 100 MB document produces millions of rows;
                    // that never happens on the UI thread (see AGENTS.md).
                    self.start_tree_job(tab, TreeJob::ExpandAll);
                    Vec::new()
                }
            }
            Msg::TreeCollapseAll { tab } => {
                let index = self.tree.get(&tab).and_then(|tree| tree.index.clone());
                if let Some(index) = index {
                    let input: &str = self
                        .tabs
                        .iter()
                        .find(|t| t.id == tab)
                        .map(|t| t.input.as_str())
                        .unwrap_or("");
                    if let Some(tree) = self.tree.get_mut(&tab) {
                        tree.collapse_all(&index, input);
                    }
                }
                Vec::new()
            }
            Msg::TreeSortToggled { tab } => {
                self.tree_sort_toggled(tab);
                Vec::new()
            }
            Msg::TreeFilterChanged { tab, query } => {
                let changed = query
                    != self
                        .tree
                        .get(&tab)
                        .map(|tree| tree.filter.clone())
                        .unwrap_or_default();
                if !changed {
                    Vec::new()
                } else if query.is_empty() {
                    if let Some(tree) = self.tree.get_mut(&tab) {
                        tree.clear_filter();
                    }
                    // The field follows here too: a cleared filter must not
                    // leave stale text behind.
                    if tab == self.active {
                        self.sync_tree_filter_field(&query);
                    }
                    Vec::new()
                } else {
                    let node_count = self
                        .tree
                        .get(&tab)
                        .and_then(|tree| tree.index.as_ref())
                        .map(|index| index.node_count())
                        .unwrap_or(0);
                    if node_count > 0 {
                        if let Some(tree) = self.tree.get_mut(&tab) {
                            tree.filter = query.clone();
                            tree.save_filter_base();
                        }
                        // The field follows the active tab's filter, whether
                        // the query came from typing (already equal, a
                        // no-op) or from anywhere else.
                        if tab == self.active {
                            self.sync_tree_filter_field(&query);
                        }
                        // Filtering runs through the job machinery: inline
                        // without an app handle, off-thread (canceling any
                        // previous filter) with one.
                        self.start_tree_job(tab, TreeJob::Filter(query));
                    }
                    Vec::new()
                }
            }
            Msg::TreeCopyPath => {
                let id = self.active;
                let path = self.tree.get(&id).and_then(|tree| tree.selected.clone()).map(|sel| sel.path);
                match path {
                    Some(path) if !path.is_empty() => {
                        self.status = Some("Copied path".into());
                        vec![Effect::Copy(path)]
                    }
                    _ => {
                        self.status = Some("No node selected".into());
                        Vec::new()
                    }
                }
            }
            Msg::TreeCopyValue => {
                let id = self.active;
                let selected =
                    self.tree.get(&id).and_then(|tree| tree.selected.clone());
                let fresh = self
                    .tree
                    .get(&id)
                    .is_some_and(|tree| tree.index_revision == self.tab(id).map_or(0, |tab| tab.input_revision));
                match (selected, fresh) {
                    (Some(selection), true) => {
                        let range = selection.value_off as usize
                            ..(selection.value_off + selection.value_len) as usize;
                        let value = self
                            .tab(id)
                            .and_then(|tab| tab.input.get(range.clone()))
                            .map(str::to_owned);
                        match value {
                            Some(value) => {
                                self.status = Some("Copied value".into());
                                vec![Effect::Copy(value)]
                            }
                            None => {
                                self.status = Some("Value no longer available".into());
                                Vec::new()
                            }
                        }
                    }
                    _ => {
                        self.status = Some("No node selected".into());
                        Vec::new()
                    }
                }
            }
            Msg::TreeIndexDone {
                tab,
                revision,
                outcome,
            } => {
                self.apply_tree_index(tab, revision, outcome);
                Vec::new()
            }
            Msg::TreeRowsReady {
                tab,
                gen,
                rows,
                matched,
            } => {
                let outcome = tree_index::RowsOutcome { rows, matched };
                self.apply_tree_rows(tab, gen, outcome);
                Vec::new()
            }
            Msg::Status(text) => {
                self.status = Some(text);
                Vec::new()
            }
            Msg::Error(text) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.error = Some(text);
                    tab.validation = None;
                    tab.is_processing = false;
                }
                Vec::new()
            }
        }
    }

    fn start_json(&mut self, action: JsonAction) -> Vec<Effect> {
        let id = self.active;
        let Some(tab) = self.tab_mut(id) else {
            return Vec::new();
        };
        if tab.input.trim().is_empty() {
            self.status = Some("Nothing to process".into());
            return Vec::new();
        }
        tab.is_processing = true;
        tab.error = None;
        tab.validation = None;
        let input_revision = tab.input_revision;
        vec![self.spawn_input_job(id, RopeJob::Json { action, input_revision })]
    }

    fn start_validation(&mut self) -> Vec<Effect> {
        let id = self.active;
        let Some(tab) = self.tab_mut(id) else {
            return Vec::new();
        };
        if tab.input.trim().is_empty() {
            self.status = Some("Nothing to validate".into());
            return Vec::new();
        }
        tab.is_processing = true;
        tab.error = None;
        tab.validation = None;
        let input_revision = tab.input_revision;
        vec![self.spawn_input_job(id, RopeJob::Validate { input_revision })]
    }

    /// Starts a background job over the tab's input. When the rope editor
    /// owns the text, the effect carries an O(1) rope snapshot and the
    /// runner materializes it on a worker thread; otherwise (no editor
    /// entry, e.g. a restored tab never opened) the classic `Arc<str>` copy
    /// path is used.
    fn spawn_input_job(&self, id: TabId, job: RopeJob) -> Effect {
        let tab = self.tab(id).expect("caller checked the tab");
        match crate::editor::state::existing_snapshot(id) {
            Some(input) => Effect::MaterializeRope { tab: id, input, job },
            None => {
                let input: Arc<str> = Arc::from(tab.input.as_str());
                match job {
                    RopeJob::Json { action, input_revision } => Effect::SpawnJson {
                        tab: id,
                        input,
                        action,
                        input_revision,
                    },
                    RopeJob::Validate { input_revision } => Effect::SpawnValidation {
                        tab: id,
                        input,
                        input_revision,
                    },
                    RopeJob::Search { query, input_revision } => Effect::Search {
                        tab: id,
                        input,
                        query,
                        input_revision,
                    },
                }
            }
        }
    }
}

/// Messages emitted by the view. They are intentionally small and owned so
/// that callbacks can queue them without borrowing `AppState`.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Add a new tab and make it active.
    AddTab,
    /// Close `TabId` unless it is the last tab.
    CloseTab(TabId),
    /// Make `TabId` active.
    SelectTab(TabId),
    /// Show input or output for the active tab.
    SwitchView(TabView),
    /// Replace the active tab's source text.
    InputChanged(String),
    /// Apply a semantic editor command (rp-editor): typing, motion,
    /// selection, undo/redo. The reducer applies it to the tab's rope and
    /// does the revision/error/tree bookkeeping.
    EditorEdit {
        tab: TabId,
        action: crate::editor::state::EditorAction,
    },
    /// Clipboard paste into the editor (root `on_paste`); one undo step,
    /// materialized into `Tab::input` immediately (bulk action).
    EditorPaste { tab: TabId, text: String },
    /// Copy (or cut) the editor selection.
    EditorCopy { tab: TabId, cut: bool },
    /// A rope snapshot was converted to text off-thread; start the job.
    RopeMaterialized {
        tab: TabId,
        input: Arc<str>,
        job: RopeJob,
    },
    /// Format the active tab.
    Format,
    /// Minify the active tab.
    Minify,
    /// Validate the active tab without changing its output.
    Validate,
    /// Copy output (or input when there is no output).
    Copy,
    /// Export output (or input) through a save dialog.
    Export,
    /// Open a file through the native open dialog.
    OpenFile,
    /// A file was dropped on the window.
    FileDropped(PathBuf),
    /// A file's content is ready to be placed in the active tab. Legacy
    /// shape kept for direct callers (tests/bins); the runner delivers
    /// [`Msg::FileLoadedFor`] so the destination is the tab that requested
    /// the read, not whichever tab is active at completion (M3).
    FileLoaded { path: PathBuf, content: String },
    /// A file's content is ready for the tab that requested the read (M3).
    FileLoadedFor {
        tab: TabId,
        path: PathBuf,
        content: String,
    },
    /// Clear the active tab.
    Clear,
    /// Search query changed; searches start at two characters.
    SearchChanged(String),
    /// Background JSON work finished.
    JsonDone {
        tab: TabId,
        /// Revision the worker used to prewarm the line index.
        revision: u64,
        /// Input revision the job was started for; a result whose input
        /// revision no longer matches the tab is dropped (H3).
        input_revision: u64,
        result: Result<String, JsonError>,
    },
    /// Background validation finished for a specific input revision.
    ValidationDone {
        tab: TabId,
        input_revision: u64,
        result: Result<ValidationInfo, JsonError>,
    },
    /// Background search finished (legacy shape kept for direct callers in
    /// tests/bins; applies unconditionally like [`AppState::apply_search_results`]).
    /// The runner delivers [`Msg::SearchDoneFor`], which is revision- and
    /// query-gated (H3).
    SearchDone { tab: TabId, hits: Vec<SearchHit> },
    /// Background search finished for a specific input revision and query;
    /// a result whose document or query moved on is dropped (H3).
    SearchDoneFor {
        tab: TabId,
        input_revision: u64,
        query: String,
        hits: Vec<SearchHit>,
    },
    /// Persisted tabs were loaded at startup.
    TabsLoaded(Vec<Tab>),
    /// Set the status line.
    Status(String),
    /// Show an error on the active tab.
    Error(String),
    /// Show the tree explorer for the active tab (builds its index).
    TreeOpen,
    /// Leave the tree explorer, back to the code view.
    TreeClose,
    /// Toggle tree rows for `node` (expand when collapsed, collapse when
    /// expanded). Carried by row/arrow handlers with the node id, never a row
    /// index: rows move while the message sits in the queue.
    TreeExpand { tab: TabId, node: u32 },
    /// Remove `node`'s subtree from the rows.
    TreeCollapse { tab: TabId, node: u32 },
    /// Select `node` (shows its path in the tree footer).
    TreeSelect { tab: TabId, node: u32 },
    /// Expand the containers along a ` → `-joined path and select its node
    /// (the search-results hook's tree-mode behaviour).
    TreeRevealPath { tab: TabId, path: String },
    /// Expand every container (off-thread above `tree_index::SYNC_ROW_LIMIT`
    /// nodes).
    TreeExpandAll { tab: TabId },
    /// Collapse everything below the root.
    TreeCollapseAll { tab: TabId },
    /// Toggle sort A→Z (view order only; data is never modified).
    TreeSortToggled { tab: TabId },
    /// The tree filter query changed.
    TreeFilterChanged { tab: TabId, query: String },
    /// Copy the selected node's path to the clipboard.
    TreeCopyPath,
    /// Copy the selected node's value (its whole subtree for containers).
    TreeCopyValue,
    /// Replace the selected scalar's value with `tree_value_edit`'s text
    /// (one undo step in the document editor).
    TreeEditValue { tab: TabId },
    /// A tree index build finished (revision-gated).
    TreeIndexDone {
        tab: TabId,
        revision: u64,
        outcome: IndexOutcome,
    },
    /// An off-thread rows job finished (generation-gated).
    TreeRowsReady {
        tab: TabId,
        gen: u64,
        rows: Vec<tree_index::TreeRow>,
        matched: Option<usize>,
    },
}

/// Which background job a rope snapshot is materialized for. The reducer
/// hands `Effect::MaterializeRope` to the runner, which converts the rope
/// to an `Arc<str>` on a worker thread and returns it as
/// `Msg::RopeMaterialized`; the reducer then emits the classic job effect.
#[derive(Debug, Clone, PartialEq)]
pub enum RopeJob {
    Json {
        action: JsonAction,
        /// Input revision the job was started for (H3 staleness gate).
        input_revision: u64,
    },
    Validate { input_revision: u64 },
    Search {
        query: String,
        /// Input revision the job was started for (H3 staleness gate).
        input_revision: u64,
    },
}

impl RopeJob {
    /// The input revision this job was started for.
    fn input_revision(&self) -> u64 {
        match self {
            RopeJob::Json { input_revision, .. }
            | RopeJob::Validate { input_revision }
            | RopeJob::Search { input_revision, .. } => *input_revision,
        }
    }
}

/// Side effects returned by [`AppState::update`]. The runner owns execution;
/// the model never performs I/O itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Run [`crate::json_ops::process`] on a background thread.
    SpawnJson {
        tab: TabId,
        input: Arc<str>,
        action: JsonAction,
        /// Input revision the job was started for; `JsonDone` carries it
        /// back so a result for an edited document is dropped (H3).
        input_revision: u64,
    },
    /// Run [`crate::json_ops::validate`] on a background thread.
    SpawnValidation {
        tab: TabId,
        input: Arc<str>,
        input_revision: u64,
    },
    /// Run [`crate::search::search`] on a background thread.
    Search {
        tab: TabId,
        input: Arc<str>,
        query: String,
        /// Input revision the job was started for; `SearchDoneFor` carries
        /// it back so a result for an edited document is dropped (H3).
        input_revision: u64,
    },
    /// Write the tab set to disk.
    Persist,
    /// Put text on the system clipboard.
    Copy(String),
    /// Ask for a path and write the content there.
    Export {
        default_name: String,
        content: String,
    },
    /// Ask for a JSON file and read it.
    Open,
    /// Read a dropped file and deliver [`Msg::FileLoaded`].
    ReadDroppedFile(PathBuf),
    /// Convert an O(1) editor rope snapshot to text on a worker thread and
    /// deliver [`Msg::RopeMaterialized`] (rp-editor: Format/Minify/Validate/
    /// Search never copy the document on the UI thread).
    MaterializeRope {
        tab: TabId,
        input: ropey::Rope,
        job: RopeJob,
    },
}

fn default_export_name(tab: &Tab) -> String {
    let cleaned: String = tab
        .title
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() => c,
            '-' | '_' => c,
            ' ' => '-',
            _ => '-',
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "export.json".into()
    } else {
        format!("{cleaned}.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_ops::JsonError;

    /// Simulates the runner's off-thread rope conversion: a
    /// `MaterializeRope` effect becomes the classic job effect after the
    /// (here synchronous) `RopeMaterialized` round trip.
    fn finish_materialize(state: &mut AppState, effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .flat_map(|effect| match effect {
                Effect::MaterializeRope { tab, input, job } => {
                    let input: Arc<str> = Arc::from(input.to_string());
                    state.update(Msg::RopeMaterialized { tab, input, job })
                }
                other => vec![other],
            })
            .collect()
    }

    #[test]
    fn default_state_has_one_empty_tab() {
        let state = AppState::default();
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].title, "JSON 1");
        assert_eq!(state.active, state.tabs[0].id);
        assert!(state.tabs[0].input.is_empty());
        assert_eq!(state.tabs[0].view, TabView::Input);
    }

    #[test]
    fn add_tab_numbers_and_activates() {
        let mut state = AppState::default();
        let id = state.add_tab();
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, id);
        assert_eq!(state.tab(id).unwrap().title, "JSON 2");
        assert_ne!(id, state.tabs[0].id);
        assert!(state.editor(id).is_some());
    }

    #[test]
    fn remove_active_tab_selects_neighbour_and_keeps_last() {
        let mut state = AppState::default();
        let first = state.active;
        let second = state.add_tab();
        let third = state.add_tab();

        state.set_active(second);
        assert_eq!(state.remove_tab(second).unwrap().id, second);
        assert_eq!(state.tabs.len(), 2);
        // index 1 now holds the third tab, which is the neighbour to the right.
        assert_eq!(state.active, third);
        assert!(state.editor(second).is_none());

        state.set_active(third);
        state.remove_tab(third);
        assert_eq!(state.active, first);

        assert!(state.remove_tab(first).is_none());
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn set_active_rejects_unknown_ids() {
        let mut state = AppState::default();
        let active = state.active;
        assert!(!state.set_active(999));
        assert_eq!(state.active, active);
        assert!(state.set_active(active));
    }

    #[test]
    fn update_input_clears_error_and_syncs_editor() {
        let mut state = AppState::default();
        let id = state.active;
        state.tab_mut(id).unwrap().error = Some("old".into());
        state.dispatch(Msg::Error("boom".into()));
        state.update(Msg::InputChanged(r#"{"a":1}"#.into()));
        assert_eq!(state.tab(id).unwrap().input, r#"{"a":1}"#);
        assert_eq!(state.tab(id).unwrap().error, None);
        assert_eq!(state.editor(id).unwrap().text, r#"{"a":1}"#);
        assert_eq!(state.take_pending().len(), 1);
    }

    #[test]
    fn clear_tab_resets_everything_and_shows_input() {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "{\"a\":1}".into());
        {
            let tab = state.tab_mut(id).unwrap();
            tab.output = "{}".into();
            tab.error = Some("bad".into());
            tab.view = TabView::Output;
            tab.search_query = "a".into();
            tab.search_results.push(SearchHit {
                path: "a".into(),
                key: Some("a".into()),
                value_preview: "1".into(),
                line: None,
            });
            tab.is_processing = true;
        }
        assert_eq!(state.update(Msg::Clear), vec![Effect::Persist]);
        let tab = state.tab(id).unwrap();
        assert!(tab.input.is_empty());
        assert!(tab.output.is_empty());
        assert_eq!(tab.error, None);
        assert!(!tab.is_processing);
        assert_eq!(tab.view, TabView::Input);
        assert!(tab.search_query.is_empty());
        assert!(tab.search_results.is_empty());
        assert!(state.editor(id).unwrap().text.is_empty());
    }

    #[test]
    fn successful_json_result_replaces_output() {
        let mut state = AppState::default();
        let id = state.active;
        state.tab_mut(id).unwrap().is_processing = true;
        assert!(state.apply_json_result(id, Ok("{\n  \"a\": 1\n}".into())));
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.output, "{\n  \"a\": 1\n}");
        assert_eq!(tab.error, None);
        assert!(!tab.is_processing);
        assert_eq!(tab.view, TabView::Output);
    }

    #[test]
    fn failed_json_result_keeps_old_output() {
        let mut state = AppState::default();
        let id = state.active;
        state.tab_mut(id).unwrap().output = "old".into();
        let err = JsonError::new("expected value", 3, 7);
        assert!(!state.apply_json_result(id, Err(err)));
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.output, "old");
        assert_eq!(
            tab.error.as_deref(),
            Some("expected value at line 3, column 7")
        );
        assert!(!tab.is_processing);
    }

    #[test]
    fn format_needs_input_and_marks_processing() {
        let mut state = AppState::default();
        let id = state.active;
        assert!(state.update(Msg::Format).is_empty());
        assert_eq!(state.status.as_deref(), Some("Nothing to process"));

        state.update_input(id, "{\"a\":1}".into());
        let effects = state.update(Msg::Format);
        assert_eq!(effects.len(), 1);
        // No editor entry yet (reducer-only): the classic copy path.
        assert!(matches!(
            &effects[0],
            Effect::SpawnJson { tab, action: JsonAction::Format, .. } if *tab == id
        ));
        assert!(state.tab(id).unwrap().is_processing);
    }

    #[test]
    fn format_uses_the_rope_snapshot_when_the_editor_owns_the_text() {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "{\"a\":1}".into());
        // Mount-equivalent: the editor view/handlers create the entry.
        crate::editor::state::with(id, &state.tab(id).unwrap().input, state.tab(id).unwrap().input_revision, |_| {});
        let effects = state.update(Msg::Format);
        assert!(matches!(
            &effects[0],
            Effect::MaterializeRope { tab, job: RopeJob::Json { action: JsonAction::Format, .. }, .. } if *tab == id
        ));
        let effects = finish_materialize(&mut state, effects);
        assert!(matches!(
            &effects[0],
            Effect::SpawnJson { tab, action: JsonAction::Format, .. } if *tab == id
        ));
    }

    #[test]
    fn minify_returns_minify_effect() {
        let mut state = AppState::default();
        state.update_input(state.active, "{}".into());
        let effects = state.update(Msg::Minify);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SpawnJson {
                action: JsonAction::Minify,
                ..
            }]
        ));
    }

    #[test]
    fn search_starts_at_two_characters() {
        let mut state = AppState::default();
        state.update_input(state.active, r#"{"alpha":1}"#.into());
        assert!(state.update(Msg::SearchChanged("a".into())).is_empty());
        assert!(!state.tab(state.active).unwrap().is_processing);

        let effects = state.update(Msg::SearchChanged("al".into()));
        assert!(matches!(effects.as_slice(), [Effect::Search { query, .. }] if query == "al"));
        assert!(state.tab(state.active).unwrap().is_processing);
    }

    #[test]
    fn search_results_are_per_tab() {
        let mut state = AppState::default();
        let first = state.active;
        let second = state.add_tab();
        let hit = SearchHit {
            path: "a".into(),
            key: Some("a".into()),
            value_preview: "1".into(),
            line: Some(2),
        };
        state.update(Msg::SearchDone {
            tab: first,
            hits: vec![hit],
        });
        assert_eq!(state.tab(first).unwrap().search_results.len(), 1);
        assert!(state.tab(second).unwrap().search_results.is_empty());
    }

    #[test]
    fn update_input_drops_a_dirty_rope_snapshot() {
        // H2: edit (dirty rope) → external replace while the Input view is
        // not mounted. The no-sync accessors must never return the old text.
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "[1]".into());
        crate::editor::state::with(
            id,
            &state.tab(id).unwrap().input,
            state.tab(id).unwrap().input_revision,
            |ed| {
                ed.insert(" ", crate::editor::EditKind::Typing); // dirty: " [1]"
            },
        );
        assert!(crate::editor::state::dirty_snapshot(id).is_some());
        state.update_input(id, "[2]".into());
        assert!(
            crate::editor::state::existing_snapshot(id).is_none(),
            "stale rope survived update_input"
        );
        assert!(crate::editor::state::dirty_snapshot(id).is_none());
        // Format takes the model text, not the previous document's rope.
        let effects = state.update(Msg::Format);
        assert!(matches!(&effects[0], Effect::SpawnJson { input, .. } if &**input == "[2]"));
    }

    #[test]
    fn clear_tab_drops_a_dirty_rope_snapshot() {
        // H2 (same class): Clear persists immediately; a dirty stale rope
        // would save the previous document over the cleared tab.
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "[1]".into());
        crate::editor::state::with(
            id,
            &state.tab(id).unwrap().input,
            state.tab(id).unwrap().input_revision,
            |ed| {
                ed.insert("!", crate::editor::EditKind::Typing);
            },
        );
        state.update(Msg::Clear);
        assert!(crate::editor::state::dirty_snapshot(id).is_none());
        assert!(crate::editor::state::existing_snapshot(id).is_none());
    }

    fn stale_hit() -> SearchHit {
        SearchHit {
            path: "old".into(),
            key: Some("old".into()),
            value_preview: "1".into(),
            line: Some(2),
        }
    }

    #[test]
    fn stale_search_results_are_dropped() {
        // H3: a search started for an older query/document must not install
        // its hits after the query or the input moved on.
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, r#"{"old":1}"#.into());
        let old_revision = state.tab(id).unwrap().input_revision;
        state.update(Msg::SearchChanged("old".into()));
        state.update(Msg::SearchChanged("new".into()));
        state.update_input(id, r#"{"new":2}"#.into());
        state.update(Msg::SearchDoneFor {
            tab: id,
            input_revision: old_revision,
            query: "old".into(),
            hits: vec![stale_hit()],
        });
        let tab = state.tab(id).unwrap();
        assert!(
            tab.search_results.is_empty(),
            "stale hits installed: {:?}",
            tab.search_results
        );
        assert!(!tab.is_processing);
    }

    #[test]
    fn stale_query_search_results_are_dropped_without_an_edit() {
        // H3: the input revision alone is not enough — typing a new query
        // supersedes the running walk without touching the document.
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, r#"{"old":1,"new":2}"#.into());
        let revision = state.tab(id).unwrap().input_revision;
        state.update(Msg::SearchChanged("old".into()));
        state.update(Msg::SearchChanged("new".into()));
        state.update(Msg::SearchDoneFor {
            tab: id,
            input_revision: revision,
            query: "old".into(),
            hits: vec![stale_hit()],
        });
        assert!(state.tab(id).unwrap().search_results.is_empty());
        assert!(!state.tab(id).unwrap().is_processing);
        // The current query's result still applies.
        state.update(Msg::SearchDoneFor {
            tab: id,
            input_revision: revision,
            query: "new".into(),
            hits: vec![SearchHit {
                path: "new".into(),
                key: Some("new".into()),
                value_preview: "2".into(),
                line: Some(3),
            }],
        });
        assert_eq!(state.tab(id).unwrap().search_results.len(), 1);
    }

    #[test]
    fn stale_json_results_are_dropped() {
        // H3: a format started for an older input revision must not publish
        // its output after the user edited the document.
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, r#"{"old":1}"#.into());
        state.update(Msg::Format);
        let old_revision = state.tab(id).unwrap().input_revision;
        state.update_input(id, r#"{"new":2}"#.into());
        state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: old_revision,
            result: Ok("{\n  \"old\": 1\n}".into()),
        });
        let tab = state.tab(id).unwrap();
        assert!(tab.output.is_empty(), "stale output published: {:?}", tab.output);
        assert!(!tab.is_processing);
        assert_eq!(tab.view, TabView::Input);
        // A result for the current revision still applies and persists.
        let current = state.tab(id).unwrap().input_revision;
        let effects = state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: current,
            result: Ok("{\n  \"new\": 2\n}".into()),
        });
        assert_eq!(effects, vec![Effect::Persist]);
        assert_eq!(state.tab(id).unwrap().output, "{\n  \"new\": 2\n}");
        assert_eq!(state.tab(id).unwrap().view, TabView::Output);
    }

    #[test]
    fn successful_json_result_rewrites_input_as_one_undo_step() {
        // rp-format-inplace: Format/Minify replace the Input document itself;
        // Ctrl+Z restores the exact previous text and caret.
        let mut state = AppState::default();
        let id = state.active;
        let original = "{\"a\":1,\"b\":[1,2,3]}";
        state.update_input(id, original.into());
        let tab = state.tab(id).unwrap();
        crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.set_caret(10, false);
        });
        let old_revision = state.tab(id).unwrap().input_revision;
        let formatted = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
        let effects = state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: old_revision,
            result: Ok(formatted.into()),
        });
        assert_eq!(effects, vec![Effect::Persist], "no second job may spawn");
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.output, formatted);
        assert_eq!(tab.view, TabView::Output);
        assert!(tab.input_revision > old_revision, "staleness keys must move");
        assert!(tab.validation.is_none());
        let (text, caret, can_undo) =
            crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
                (ed.buffer.to_string(), ed.caret, ed.buffer.can_undo())
            });
        assert_eq!(text, formatted);
        assert!(can_undo);
        let expected_caret =
            (10u128 * formatted.len() as u128 / original.len() as u128) as usize;
        assert_eq!(caret, expected_caret);
        // One undo step restores the minified original and the old caret.
        state.update(Msg::EditorEdit {
            tab: id,
            action: crate::editor::state::EditorAction::Undo,
        });
        let tab = state.tab(id).unwrap();
        let (text, caret) =
            crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
                (ed.buffer.to_string(), ed.caret)
            });
        assert_eq!(text, original);
        assert_eq!(caret, 10);
        // Redo re-applies the result.
        state.update(Msg::EditorEdit {
            tab: id,
            action: crate::editor::state::EditorAction::Redo,
        });
        let tab = state.tab(id).unwrap();
        let text = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.buffer.to_string()
        });
        assert_eq!(text, formatted);
    }

    #[test]
    fn unchanged_output_leaves_input_and_history_alone() {
        let mut state = AppState::default();
        let id = state.active;
        let formatted = "{\n  \"a\": 1\n}";
        state.update_input(id, formatted.into());
        let tab = state.tab(id).unwrap();
        crate::editor::state::with(id, &tab.input, tab.input_revision, |_| {});
        let old_revision = state.tab(id).unwrap().input_revision;
        state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: old_revision,
            result: Ok(formatted.into()),
        });
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.input_revision, old_revision, "no-op must not bump the revision");
        let can_undo = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.buffer.can_undo()
        });
        assert!(!can_undo, "no-op must not add an undo step");
    }

    #[test]
    fn failed_json_result_leaves_input_untouched() {
        let mut state = AppState::default();
        let id = state.active;
        let original = "{\"a\":1}";
        state.update_input(id, original.into());
        let tab = state.tab(id).unwrap();
        crate::editor::state::with(id, &tab.input, tab.input_revision, |_| {});
        let old_revision = state.tab(id).unwrap().input_revision;
        state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: old_revision,
            result: Err(JsonError::new("expected value", 1, 2)),
        });
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.input_revision, old_revision);
        assert_eq!(tab.view, TabView::Input);
        let text = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.buffer.to_string()
        });
        assert_eq!(text, original);
    }

    #[test]
    fn json_done_without_a_live_editor_seeds_one_undoable_entry() {
        // A restored tab never opened has no rope entry: the old cache moves
        // into the undo step and the new cache stays truthful for the toolbar.
        let mut state = AppState::default();
        let id = state.active;
        let original = "{\"a\":1}";
        state.update_input(id, original.into());
        assert!(crate::editor::state::existing_snapshot(id).is_none());
        let old_revision = state.tab(id).unwrap().input_revision;
        let formatted = "{\n  \"a\": 1\n}";
        state.update(Msg::JsonDone {
            tab: id,
            revision: crate::line_index::next_revision(),
            input_revision: old_revision,
            result: Ok(formatted.into()),
        });
        let tab = state.tab(id).unwrap();
        assert_eq!(tab.input, formatted, "cache must stay truthful without an editor");
        assert!(tab.input_revision > old_revision);
        let text = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.buffer.to_string()
        });
        assert_eq!(text, formatted);
        state.update(Msg::EditorEdit {
            tab: id,
            action: crate::editor::state::EditorAction::Undo,
        });
        let tab = state.tab(id).unwrap();
        let text = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.buffer.to_string()
        });
        assert_eq!(text, original);
    }

    #[test]
    fn file_loaded_for_targets_the_requesting_tab() {
        // M3: the reducer applies a read result to the tab it names, not
        // the active tab.
        let mut state = AppState::default();
        let a = state.active;
        let b = state.add_tab();
        state.set_active(b);
        state.update(Msg::FileLoadedFor {
            tab: a,
            path: PathBuf::from("/tmp/file.json"),
            content: "[1]".into(),
        });
        assert_eq!(state.tab(a).unwrap().input, "[1]");
        assert!(state.tab(b).unwrap().input.is_empty());
        // A closed tab's late read is a no-op.
        state.update(Msg::FileLoadedFor {
            tab: 999,
            path: PathBuf::from("/tmp/file.json"),
            content: "[2]".into(),
        });
        assert_eq!(state.tab(a).unwrap().input, "[1]");
    }

    #[test]
    fn rope_materialized_for_an_edited_input_is_dropped() {
        // H3: input changed while the rope was converted off-thread; the
        // job must not start (it would cancel a newer per-tab job).
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "[1]".into());
        let revision = state.tab(id).unwrap().input_revision;
        state.tab_mut(id).unwrap().is_processing = true;
        state.update_input(id, "[2]".into()); // edits while materializing
        let effects = state.update(Msg::RopeMaterialized {
            tab: id,
            input: Arc::from("[1]"),
            job: RopeJob::Json { action: JsonAction::Format, input_revision: revision },
        });
        assert!(effects.is_empty(), "stale job started: {effects:?}");
        assert!(!state.tab(id).unwrap().is_processing);
        // The current revision's materialization still starts the job.
        let current = state.tab(id).unwrap().input_revision;
        let effects = state.update(Msg::RopeMaterialized {
            tab: id,
            input: Arc::from("[2]"),
            job: RopeJob::Search { query: "x".into(), input_revision: current },
        });
        assert!(matches!(
            effects.as_slice(),
            [Effect::Search { input_revision, .. }] if *input_revision == current
        ));
    }

    #[test]
    fn close_emits_persist_only_when_a_tab_was_removed() {
        let mut state = AppState::default();
        let first = state.active;
        assert!(state.update(Msg::CloseTab(first)).is_empty());
        let second = state.add_tab();
        assert_eq!(state.update(Msg::CloseTab(second)), vec![Effect::Persist]);
    }

    #[test]
    fn export_uses_the_live_document() {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "in".into());
        let effects = state.update(Msg::Export);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Export { default_name, content }]
                if default_name == "JSON-1.json" && content == "in"
        ));
        // A stale Output (edited after Format) must not win over the document.
        state.tab_mut(id).unwrap().output = "out".into();
        let effects = state.update(Msg::Export);
        assert!(matches!(effects.as_slice(), [Effect::Export { content, .. }] if content == "in"));
    }

    #[test]
    fn replace_tabs_rebuilds_editors_and_counters() {
        let mut state = AppState::default();
        let mut a = Tab::new(7, "JSON 9");
        a.input = "one".into();
        let mut b = Tab::new(12, "Notes");
        b.input = "two".into();
        state.replace_tabs(vec![a, b]);
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, 7);
        assert_eq!(state.next_id, 13);
        assert_eq!(state.tab_counter, 9);
        assert_eq!(state.editor(7).unwrap().text, "one");
        assert_eq!(state.editor(12).unwrap().text, "two");
    }

    #[test]
    fn replace_tabs_ignores_empty_input() {
        let mut state = AppState::default();
        let active = state.active;
        state.replace_tabs(Vec::new());
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.active, active);
    }

    // ── Tree explorer reducers ───────────────────────────────────────────

    const TREE_DOC: &str =
        r#"{"scripts":{"name":"simple-json-viewer","items":[12,"x"]},"n":-4.5}"#;

    fn tree_ready_state() -> (AppState, TabId) {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, TREE_DOC.into());
        state.update(Msg::TreeOpen);
        let tree = state.tree.get(&id).expect("tree state exists");
        assert!(tree.open);
        assert!(tree.is_ready(), "sync path built the index inline");
        assert_eq!(tree.rows.len(), 3); // root + 2 members
        (state, id)
    }

    #[test]
    fn tree_edit_value_rewrites_the_document_and_undo_restores_it() {
        let (mut state, id) = tree_ready_state();
        let path = "scripts → name";
        state.update(Msg::TreeRevealPath { tab: id, path: path.into() });
        assert_eq!(state.tree_value_edit.text, r#""simple-json-viewer""#);

        state.tree_value_edit.replace_text(r#""renamed""#);
        state.update(Msg::TreeEditValue { tab: id });
        state.materialize_input(id);
        let expected = r#"{"scripts":{"name":"renamed","items":[12,"x"]},"n":-4.5}"#;
        assert_eq!(state.tab(id).unwrap().input, expected);
        // The rebuilt tree re-selects the edited path.
        let tree = &state.tree[&id];
        assert_eq!(tree.index_revision, state.tab(id).unwrap().input_revision);
        assert_eq!(tree.selected.as_ref().map(|s| s.path.as_str()), Some(path));
        assert_eq!(state.tree_value_edit.text, r#""renamed""#);

        // One undo step in the document editor restores the old value.
        state.update(Msg::EditorEdit {
            tab: id,
            action: crate::editor::state::EditorAction::Undo,
        });
        state.materialize_input(id);
        assert_eq!(state.tab(id).unwrap().input, TREE_DOC);
    }

    #[test]
    fn tree_edit_value_rejects_invalid_json_and_containers() {
        let (mut state, id) = tree_ready_state();
        state.update(Msg::TreeRevealPath { tab: id, path: "n".into() });
        state.tree_value_edit.replace_text("not json");
        state.update(Msg::TreeEditValue { tab: id });
        state.materialize_input(id);
        assert_eq!(state.tab(id).unwrap().input, TREE_DOC);
        assert!(state.status.as_deref().unwrap_or("").starts_with("Invalid JSON value"));

        state.update(Msg::TreeRevealPath { tab: id, path: "scripts".into() });
        assert_eq!(state.tree_value_edit.text, "");
        state.tree_value_edit.replace_text("1");
        state.update(Msg::TreeEditValue { tab: id });
        state.materialize_input(id);
        assert_eq!(state.tab(id).unwrap().input, TREE_DOC);
        assert_eq!(state.status.as_deref(), Some("Select a value to edit"));
    }

    #[test]
    fn tree_open_builds_the_index_synchronously_without_a_handle() {
        let (state, id) = tree_ready_state();
        let tree = state.tree.get(&id).unwrap();
        assert_eq!(tree.index_revision, state.tab(id).unwrap().input_revision);
        assert!(!tree.building);
        assert!(tree.error.is_none());
    }

    #[test]
    fn tree_expand_and_collapse_splice_rows() {
        let (mut state, id) = tree_ready_state();
        let scripts = state.tree[&id].rows[1].node;
        state.update(Msg::TreeExpand { tab: id, node: scripts });
        assert_eq!(state.tree[&id].rows.len(), 5); // + name + items
        assert!(state.tree[&id].rows[1].expanded);
        // Expanding again is a no-op.
        state.update(Msg::TreeExpand { tab: id, node: scripts });
        assert_eq!(state.tree[&id].rows.len(), 5);
        state.update(Msg::TreeCollapse { tab: id, node: scripts });
        assert_eq!(state.tree[&id].rows.len(), 3);
        assert!(!state.tree[&id].rows[1].expanded);
    }

    #[test]
    fn tree_select_computes_the_path_and_copy_effects() {
        let (mut state, id) = tree_ready_state();
        let scripts = state.tree[&id].rows[1].node;
        state.update(Msg::TreeExpand { tab: id, node: scripts });
        let name = state.tree[&id].rows[2].node;
        state.update(Msg::TreeSelect { tab: id, node: name });
        let selection = state.tree[&id].selected.clone().unwrap();
        assert_eq!(selection.path, "scripts → name");
        assert!(!selection.is_container);

        let effects = state.update(Msg::TreeCopyPath);
        assert!(matches!(effects.as_slice(), [Effect::Copy(text)] if text == "scripts → name"));
        assert_eq!(state.status.as_deref(), Some("Copied path"));

        let effects = state.update(Msg::TreeCopyValue);
        assert!(matches!(effects.as_slice(), [Effect::Copy(text)] if text == r#""simple-json-viewer""#));
        assert_eq!(state.status.as_deref(), Some("Copied value"));
    }

    #[test]
    fn tree_copy_value_of_a_container_copies_the_subtree() {
        let (mut state, id) = tree_ready_state();
        let scripts = state.tree[&id].rows[1].node;
        state.update(Msg::TreeSelect { tab: id, node: scripts });
        let effects = state.update(Msg::TreeCopyValue);
        assert!(matches!(effects.as_slice(), [Effect::Copy(text)] if text == r#"{"name":"simple-json-viewer","items":[12,"x"]}"#));
    }

    #[test]
    fn tree_copy_without_selection_sets_a_status() {
        let (mut state, id) = tree_ready_state();
        let effects = state.update(Msg::TreeCopyValue);
        assert!(effects.is_empty());
        assert_eq!(state.status.as_deref(), Some("No node selected"));
        state.update(Msg::TreeSelect { tab: id, node: 999 });
        assert!(state.tree[&id].selected.is_none());
    }

    #[test]
    fn tree_expand_all_and_collapse_all() {
        let (mut state, id) = tree_ready_state();
        state.update(Msg::TreeExpandAll { tab: id });
        let tree = &state.tree[&id];
        assert_eq!(tree.rows.len(), 7); // every node of TREE_DOC
        assert!(tree.rows.iter().filter(|r| r.expanded).count() >= 2);
        state.update(Msg::TreeCollapseAll { tab: id });
        let tree = &state.tree[&id];
        assert_eq!(tree.rows.len(), 3);
        assert!(tree.rows[0].expanded);
        assert!(tree.rows.iter().skip(1).all(|r| !r.expanded));
    }

    #[test]
    fn tree_sort_is_a_view_order_permutation() {
        let (mut state, id) = tree_ready_state();
        let before: Vec<u32> = state.tree[&id].rows.iter().map(|r| r.node).collect();
        state.update(Msg::TreeSortToggled { tab: id });
        let after: Vec<u32> = state.tree[&id].rows.iter().map(|r| r.node).collect();
        assert_eq!(after.len(), before.len());
        // Same nodes, display order only.
        assert!(before.iter().all(|node| after.contains(node)));
        // `n` (node with key "n") sorts before `scripts`.
        let input = state.tab(id).unwrap().input.clone();
        let keys: Vec<String> = state.tree[&id]
            .rows
            .iter()
            .skip(1)
            .filter_map(|r| {
                state.tree[&id]
                    .index
                    .as_ref()
                    .and_then(|index| index.key_text(&input, r.node))
                    .map(|key| key.into_owned())
            })
            .collect();
        assert_eq!(keys, ["n", "scripts"]);
        // Toggling back restores document order.
        state.update(Msg::TreeSortToggled { tab: id });
        let restored: Vec<u32> = state.tree[&id].rows.iter().map(|r| r.node).collect();
        assert_eq!(restored, before);
    }

    #[test]
    fn tree_filter_keeps_ancestors_and_clearing_restores() {
        let (mut state, id) = tree_ready_state();
        // Expand scripts so the filter has expansion state to restore.
        let scripts = state.tree[&id].rows[1].node;
        state.update(Msg::TreeExpand { tab: id, node: scripts });
        let unfiltered_rows = state.tree[&id].rows.len();

        state.update(Msg::TreeFilterChanged { tab: id, query: "simple".into() });
        let tree = &state.tree[&id];
        assert_eq!(tree.filter, "simple");
        assert_eq!(tree.filter_matched, Some(1));
        assert_eq!(tree.rows.len(), 3); // root + scripts + name
        // Expansion is blocked while filtering.
        state.update(Msg::TreeExpand { tab: id, node: 0 });
        assert_eq!(state.tree[&id].rows.len(), 3);

        state.update(Msg::TreeFilterChanged { tab: id, query: String::new() });
        let tree = &state.tree[&id];
        assert!(tree.filter.is_empty());
        assert_eq!(tree.rows.len(), unfiltered_rows);
        assert!(tree.rows.iter().any(|r| r.node == scripts && r.expanded));
    }

    #[test]
    fn tree_filter_with_no_matches_leaves_an_empty_view() {
        let (mut state, id) = tree_ready_state();
        state.update(Msg::TreeFilterChanged { tab: id, query: "zzz-unknown".into() });
        let tree = &state.tree[&id];
        assert_eq!(tree.filter_matched, Some(0));
        assert!(tree.rows.is_empty());
    }

    #[test]
    fn tree_index_rebuilds_when_input_changes() {
        let (mut state, id) = tree_ready_state();
        let old_revision = state.tree[&id].index_revision;
        state.update_input(id, r#"{"fresh":true}"#.into());
        let tree = state.tree.get(&id).unwrap();
        assert!(tree.is_ready());
        assert_ne!(tree.index_revision, old_revision);
        assert_eq!(tree.index_revision, state.tab(id).unwrap().input_revision);
        assert_eq!(tree.rows.len(), 2); // root + "fresh"
        // A second update with the same content still rebuilds (new revision).
        state.update_input(id, r#"{"fresh":true}"#.into());
        assert_eq!(
            state.tree[&id].index_revision,
            state.tab(id).unwrap().input_revision
        );
    }

    #[test]
    fn tree_invalid_json_reports_an_error() {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "{\"broken\": tru".into());
        state.update(Msg::TreeOpen);
        let tree = state.tree.get(&id).unwrap();
        assert!(tree.error.is_some(), "parse failure recorded");
        assert!(!tree.is_ready());
    }

    #[test]
    fn tree_empty_input_shows_nothing_to_explore() {
        let mut state = AppState::default();
        let id = state.active;
        state.update(Msg::TreeOpen);
        let tree = state.tree.get(&id).unwrap();
        assert!(tree.open);
        assert!(!tree.building);
        assert!(!tree.is_ready());
        assert!(tree.error.is_none());
    }

    #[test]
    fn tree_close_and_switch_view_leave_the_tree() {
        let (mut state, id) = tree_ready_state();
        state.update(Msg::SwitchView(TabView::Output));
        assert!(!state.tree[&id].open, "choosing a code view leaves the tree");
        // The index is kept for a quick return.
        assert!(state.tree[&id].is_ready());
        state.update(Msg::TreeOpen);
        assert!(state.tree[&id].open);
        state.update(Msg::TreeClose);
        assert!(!state.tree[&id].open);
        // Tab switching into a tree tab keeps it open.
        state.update(Msg::TreeOpen);
        let second = state.add_tab();
        state.set_active(second);
        assert!(!state.tree.contains_key(&second));
        state.set_active(id);
        assert!(state.tree[&id].open);
    }

    #[test]
    fn tree_clear_tab_closes_the_tree() {
        let (mut state, id) = tree_ready_state();
        state.update(Msg::Clear);
        let tree = state.tree.get(&id).unwrap();
        assert!(!tree.open);
        assert!(!tree.is_ready());
    }

    #[test]
    fn tree_remove_tab_drops_its_state() {
        let (mut state, id) = tree_ready_state();
        let second = state.add_tab();
        state.update(Msg::TreeOpen);
        assert!(state.tree.contains_key(&second));
        state.update(Msg::CloseTab(second));
        assert!(!state.tree.contains_key(&second));
        assert!(state.tree.contains_key(&id));
    }

    #[test]
    fn tree_replace_tabs_clears_tree_state() {
        let (mut state, id) = tree_ready_state();
        assert!(state.tree.contains_key(&id));
        let mut restored = Tab::new(id, "JSON 1");
        restored.input = TREE_DOC.into();
        restored.input_revision = 99;
        state.update(Msg::TabsLoaded(vec![restored]));
        assert!(!state.tree.contains_key(&id), "restored tabs get fresh tree state");
    }

    #[test]
    fn tree_rows_ready_is_generation_gated() {
        let (mut state, id) = tree_ready_state();
        let gen = state.tree[&id].jobs_gen;
        // A delivery for a stale generation is dropped.
        state.update(Msg::TreeRowsReady {
            tab: id,
            gen: gen.wrapping_sub(1),
            rows: Vec::new(),
            matched: None,
        });
        assert_eq!(state.tree[&id].rows.len(), 3);
        // A job bumps the generation; its delivery applies.
        state.tree.get_mut(&id).unwrap().jobs_gen += 1;
        let gen = state.tree[&id].jobs_gen;
        let rows = vec![crate::tree_index::TreeRow {
            node: 0,
            depth: 0,
            parent_row: crate::tree_index::NONE,
            child_index: 0,
            expanded: false,
        }];
        state.update(Msg::TreeRowsReady { tab: id, gen, rows, matched: None });
        assert_eq!(state.tree[&id].rows.len(), 1);
    }

    #[test]
    fn tree_index_done_for_a_stale_revision_is_dropped() {
        let (mut state, id) = tree_ready_state();
        let outcome = crate::tree_index::IndexOutcome::Ready(std::sync::Arc::new(
            crate::tree_index::TreeIndex::build("{}").unwrap(),
        ));
        // The pending request was superseded (input changed): dropped.
        state.update(Msg::TreeIndexDone {
            tab: id,
            revision: state.tree[&id].index_revision.wrapping_add(1),
            outcome,
        });
        assert_eq!(state.tree[&id].rows.len(), 3, "stale delivery ignored");
    }

    #[test]
    fn tree_filter_field_shows_the_active_tab_filter() {
        let (mut state, id) = tree_ready_state();
        // Set programmatically (scripts, restored runs): the field follows.
        state.update(Msg::TreeFilterChanged {
            tab: id,
            query: "simple".into(),
        });
        assert_eq!(state.tree_filter_edit.text, "simple");
        // The same query again must not disturb the field.
        state.update(Msg::TreeFilterChanged {
            tab: id,
            query: "simple".into(),
        });
        assert_eq!(state.tree_filter_edit.text, "simple");

        // A second tab with its own filter: the shared field follows the
        // active tab.
        let second = state.add_tab();
        state.update_input(second, r#"{"x":1}"#.into());
        state.update(Msg::TreeOpen);
        state.update(Msg::TreeFilterChanged {
            tab: second,
            query: "other".into(),
        });
        assert_eq!(state.tree_filter_edit.text, "other");
        state.update(Msg::SelectTab(id));
        assert_eq!(state.tree_filter_edit.text, "simple");
        state.update(Msg::SelectTab(second));
        assert_eq!(state.tree_filter_edit.text, "other");

        // Leaving for a code view and re-entering the tree keeps the text.
        state.update(Msg::SwitchView(TabView::Output));
        assert!(!state.tree[&second].open);
        state.update(Msg::TreeOpen);
        assert_eq!(state.tree_filter_edit.text, "other");

        // Clearing the filter clears the field (the view's clear button
        // replaces the buffer text itself; the reducer path stays in step).
        state.update(Msg::TreeFilterChanged {
            tab: second,
            query: String::new(),
        });
        assert_eq!(state.tree_filter_edit.text, "");
    }

    #[test]
    fn tree_reveal_path_expands_ancestors_and_selects() {
        let (mut state, id) = tree_ready_state();
        // The target (`items`) sits under a collapsed `scripts`; the reveal
        // must expand the containers along the path, then select.
        let scripts = state.tree[&id].rows[1].node;
        assert!(!state.tree[&id].rows[1].expanded);
        state.update(Msg::TreeRevealPath {
            tab: id,
            path: "scripts → items".into(),
        });
        let tree = &state.tree[&id];
        assert!(
            tree.row_of_node(scripts)
                .is_some_and(|row| tree.rows[row].expanded)
        );
        let selection = tree.selected.as_ref().expect("node selected");
        assert_eq!(selection.path, "scripts → items");
        assert_eq!(
            &state.tab(id).unwrap().input
                [selection.value_off as usize..(selection.value_off + selection.value_len) as usize],
            r#"[12,"x"]"#
        );
        // A path that no longer resolves changes nothing.
        state.update(Msg::TreeRevealPath {
            tab: id,
            path: "scripts → nope → deeper".into(),
        });
        assert_eq!(
            state.tree[&id].selected.as_ref().unwrap().path,
            "scripts → items"
        );
    }

    #[test]
    fn tree_filter_survives_an_input_change_and_clears_back() {
        let (mut state, id) = tree_ready_state();
        // "a" matches `name` in the old doc and cat/canary in the new one.
        state.update(Msg::TreeFilterChanged {
            tab: id,
            query: "a".into(),
        });
        assert!(state.tree[&id].filter_matched.unwrap_or(0) > 0);
        // New document with the filter still active: the re-run must keep a
        // saved base so clearing the filter restores the fresh rows.
        state.update_input(id, r#"{"cat":1,"canary":2,"dog":3}"#.into());
        assert!(state.tree[&id].filter_matched.unwrap_or(0) >= 2);
        state.update(Msg::TreeFilterChanged {
            tab: id,
            query: String::new(),
        });
        let tree = &state.tree[&id];
        assert!(tree.filter.is_empty());
        assert_eq!(
            tree.rows.len(),
            4,
            "unfiltered rows restored: {}",
            tree.rows.len()
        );
    }
}
