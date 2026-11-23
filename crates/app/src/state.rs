//! Pure application model: tabs, search results, messages and effects.
//!
//! Everything in this module is UI-framework agnostic and synchronous. The
//! Rune view only *emits* [`Msg`] values (through [`AppState::dispatch`]); the
//! runner in `runner.rs` calls [`AppState::update`] and executes the returned
//! [`Effect`]s. That keeps reducers deterministic and unit-testable while
//! still allowing background JSON and search work.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rune::TextEdit;
use serde::{Deserialize, Serialize};

use crate::json_ops::{JsonAction, JsonError};
use crate::search::SearchHit;

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
    /// Messages queued by view handlers, drained by the runner.
    pub pending: Vec<Msg>,
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
            pending: Vec::new(),
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
                tab.error = None;
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
        tab.error = None;
        if let Some(edit) = self.editors.get_mut(&id) {
            if edit.text != input {
                edit.replace_text(input);
            }
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
        tab.error = None;
        tab.is_processing = false;
        tab.view = TabView::Input;
        tab.search_query.clear();
        tab.search_results.clear();
        if let Some(edit) = self.editors.get_mut(&id) {
            edit.replace_text("");
        }
    }

    /// Applies a finished formatting job. A success replaces `output`; a
    /// failure replaces `error` and leaves the previous output untouched.
    /// Returns `true` on success.
    pub fn apply_json_result(&mut self, id: TabId, result: Result<String, JsonError>) -> bool {
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        tab.is_processing = false;
        match result {
            Ok(output) => {
                tab.output = output;
                tab.error = None;
                true
            }
            Err(err) => {
                tab.error = Some(err.to_string());
                false
            }
        }
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
                Vec::new()
            }
            Msg::SwitchView(view) => {
                self.set_view(self.active, view);
                Vec::new()
            }
            Msg::InputChanged(text) => {
                self.update_input(self.active, text);
                Vec::new()
            }
            Msg::Clear => {
                self.clear_tab(self.active);
                vec![Effect::Persist]
            }
            Msg::Format => self.start_json(JsonAction::Format),
            Msg::Minify => self.start_json(JsonAction::Minify),
            Msg::Copy => {
                let Some(tab) = self.active_tab() else {
                    return Vec::new();
                };
                let text = if tab.output.is_empty() {
                    tab.input.clone()
                } else {
                    tab.output.clone()
                };
                if text.is_empty() {
                    self.status = Some("Nothing to copy".into());
                    Vec::new()
                } else {
                    vec![Effect::Copy(text)]
                }
            }
            Msg::Export => {
                let Some(tab) = self.active_tab() else {
                    return Vec::new();
                };
                let content = if tab.output.is_empty() {
                    tab.input.clone()
                } else {
                    tab.output.clone()
                };
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
                let input: Arc<str> = Arc::from(tab.input.as_str());
                vec![Effect::Search {
                    tab: id,
                    input,
                    query,
                }]
            }
            Msg::JsonDone { tab, result } => {
                let ok = self.apply_json_result(tab, result);
                if ok {
                    vec![Effect::Persist]
                } else {
                    Vec::new()
                }
            }
            Msg::SearchDone { tab, hits } => {
                self.apply_search_results(tab, hits);
                Vec::new()
            }
            Msg::TabsLoaded(tabs) => {
                self.replace_tabs(tabs);
                Vec::new()
            }
            Msg::Status(text) => {
                self.status = Some(text);
                Vec::new()
            }
            Msg::Error(text) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.error = Some(text);
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
        let input: Arc<str> = Arc::from(tab.input.as_str());
        vec![Effect::SpawnJson {
            tab: id,
            input,
            action,
        }]
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
    /// Format the active tab.
    Format,
    /// Minify the active tab.
    Minify,
    /// Copy output (or input when there is no output).
    Copy,
    /// Export output (or input) through a save dialog.
    Export,
    /// Open a file through the native open dialog.
    OpenFile,
    /// A file was dropped on the window.
    FileDropped(PathBuf),
    /// A file's content is ready to be placed in the active tab.
    FileLoaded { path: PathBuf, content: String },
    /// Clear the active tab.
    Clear,
    /// Search query changed; searches start at two characters.
    SearchChanged(String),
    /// Background JSON work finished.
    JsonDone {
        tab: TabId,
        result: Result<String, JsonError>,
    },
    /// Background search finished.
    SearchDone { tab: TabId, hits: Vec<SearchHit> },
    /// Persisted tabs were loaded at startup.
    TabsLoaded(Vec<Tab>),
    /// Set the status line.
    Status(String),
    /// Show an error on the active tab.
    Error(String),
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
    },
    /// Run [`crate::search::search`] on a background thread.
    Search {
        tab: TabId,
        input: Arc<str>,
        query: String,
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
        assert!(matches!(
            &effects[0],
            Effect::SpawnJson { tab, action: JsonAction::Format, .. } if *tab == id
        ));
        assert!(state.tab(id).unwrap().is_processing);
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
    fn close_emits_persist_only_when_a_tab_was_removed() {
        let mut state = AppState::default();
        let first = state.active;
        assert!(state.update(Msg::CloseTab(first)).is_empty());
        let second = state.add_tab();
        assert_eq!(state.update(Msg::CloseTab(second)), vec![Effect::Persist]);
    }

    #[test]
    fn export_uses_output_then_input() {
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, "in".into());
        let effects = state.update(Msg::Export);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Export { default_name, content }]
                if default_name == "JSON-1.json" && content == "in"
        ));
        state.tab_mut(id).unwrap().output = "out".into();
        let effects = state.update(Msg::Export);
        assert!(matches!(effects.as_slice(), [Effect::Export { content, .. }] if content == "out"));
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
}
