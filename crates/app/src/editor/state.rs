//! Per-tab editor state and the process-wide store (rp-editor E2).
//!
//! The rope buffer, caret, selection and horizontal window live here, keyed
//! by [`TabId`] in a thread-local store — the same pattern `fold_index` uses
//! for fold sets, because the Rune view only has `&AppState`. Every access
//! goes through [`with`], which first re-syncs the buffer from `Tab::input`
//! when the tab text changed outside the editor (file load, clear, restore,
//! or a direct assignment in a test/binary). Editor edits mark the state
//! `dirty`: `Tab::input` is then a stale materialized cache until an explicit
//! consumer refreshes it (`AppState::materialize_input`), so no keystroke
//! ever copies the document.
//!
//! All offsets are byte offsets. The caret is a byte offset; a selection is
//! `anchor..caret` normalized. Vertical moves keep a goal column so arrows
//! through short lines return to the original column.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use ropey::Rope;

use crate::state::TabId;

use super::buffer::{Buffer, EditKind};

/// How a pointer drag extends the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragMode {
    /// Plain click-drag: character granularity.
    Char,
    /// Double-click drag: whole words.
    Word,
    /// Triple-click drag: whole lines.
    Line,
}

/// A caret movement command (keyboard layer maps keys onto these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    DocHome,
    DocEnd,
    WordLeft,
    WordRight,
    PageUp,
    PageDown,
}

/// A semantic editor command, carried by `Msg::EditorEdit`. The reducer
/// applies it through [`EditorState::apply`] and does the tab bookkeeping
/// (revision, error/validation invalidation, tree refresh).
#[derive(Clone, Debug, PartialEq)]
pub enum EditorAction {
    /// Insert text (typing, IME commit, paste); replaces the selection.
    Insert(String, EditKind),
    Backspace,
    DeleteForward,
    Enter,
    Tab,
    Move(Move, bool),
    SelectAll,
    SelectWord(usize),
    SelectLine(usize),
    SetCaret(usize, bool),
    Pan(isize),
    Undo,
    Redo,
}

/// Everything one tab's input editor needs beyond the document itself.
#[derive(Clone, Debug)]
pub struct EditorState {
    pub buffer: Buffer,
    /// Caret byte offset.
    pub caret: usize,
    /// Selection anchor; `None` (or equal to the caret) means no selection.
    pub anchor: Option<usize>,
    /// First byte column of the horizontal window (very long lines).
    pub first_column: usize,
    /// Goal char column kept across vertical moves.
    pub goal_column: Option<usize>,
    /// `Tab::input_revision` the buffer was last synced from.
    pub source_revision: u64,
    /// The buffer holds edits not yet materialized into `Tab::input`.
    pub dirty: bool,
    /// Open IME composition, drawn at the caret (E3).
    pub preedit: Option<String>,
    /// Line the runner should scroll into view on its next poll.
    pub reveal: Option<usize>,
    /// Geometry the view writes each build; the runner reads it to scroll.
    pub scroll_y: f32,
    pub viewport_h: f32,
    /// Active pointer-drag selection mode.
    pub drag: Option<DragMode>,
    /// Word/line start the drag extends from.
    pub drag_origin: usize,
    /// Word/line end the drag extends from (backward drags anchor here).
    pub drag_end: usize,
    /// Shaped columns per row, written by the view from the viewport width.
    pub visible_columns: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            buffer: Buffer::new(),
            caret: 0,
            anchor: None,
            first_column: 0,
            goal_column: None,
            source_revision: 0,
            dirty: false,
            preedit: None,
            reveal: None,
            scroll_y: 0.0,
            viewport_h: 0.0,
            drag: None,
            drag_origin: 0,
            drag_end: 0,
            visible_columns: super::view::VISIBLE_COLUMNS,
        }
    }
}

impl EditorState {
    /// Rebuilds the buffer when the tab text changed outside the editor.
    /// A revision mismatch always rebuilds; with a matching revision, a
    /// length mismatch rebuilds only while the buffer is not dirty (a dirty
    /// buffer is *ahead* of `Tab::input`, not behind it).
    fn sync(&mut self, input: &str, revision: u64) {
        let stale = self.source_revision != revision
            || (!self.dirty && self.buffer.len_bytes() != input.len());
        if stale {
            self.buffer.set_text(input);
            self.source_revision = revision;
            self.dirty = false;
            self.caret = 0;
            self.anchor = None;
            self.first_column = 0;
            self.goal_column = None;
            self.preedit = None;
            self.reveal = None;
            self.drag = None;
        }
    }

    /// Applies a semantic command; returns whether the *document* changed
    /// (caret/selection-only commands return false).
    pub fn apply(&mut self, action: EditorAction) -> bool {
        let revision = self.buffer.revision();
        match action {
            EditorAction::Insert(text, kind) => self.insert(&text, kind),
            EditorAction::Backspace => self.backspace(),
            EditorAction::DeleteForward => self.delete_forward(),
            EditorAction::Enter => self.enter(),
            EditorAction::Tab => self.tab(),
            EditorAction::Move(mv, extend) => {
                let page_rows =
                    (self.viewport_h / crate::ui::text_view::ROW_HEIGHT).ceil() as usize;
                self.move_caret(mv, extend, page_rows.max(1));
            }
            EditorAction::SelectAll => self.select_all(),
            EditorAction::SelectWord(offset) => self.select_word(offset),
            EditorAction::SelectLine(line) => self.select_line(line),
            EditorAction::SetCaret(offset, extend) => self.set_caret(offset, extend),
            EditorAction::Pan(delta) => self.pan_columns(delta),
            EditorAction::Undo => {
                if let Some(caret) = self.buffer.undo() {
                    self.caret = caret;
                    self.anchor = None;
                    self.goal_column = None;
                    self.dirty = true;
                    self.reveal = Some(self.buffer.byte_to_line(caret));
                    self.scroll_caret_column();
                }
            }
            EditorAction::Redo => {
                if let Some(caret) = self.buffer.redo() {
                    self.caret = caret;
                    self.anchor = None;
                    self.goal_column = None;
                    self.dirty = true;
                    self.reveal = Some(self.buffer.byte_to_line(caret));
                    self.scroll_caret_column();
                }
            }
        }
        self.buffer.revision() != revision
    }

    /// Normalized selection range, if any.
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        if anchor == self.caret {
            return None;
        }
        Some(anchor.min(self.caret)..anchor.max(self.caret))
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|range| self.buffer.slice_bytes(range))
    }

    /// Line and byte column of a byte offset.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.buffer.byte_to_line(offset.min(self.buffer.len_bytes()));
        (line, offset - self.buffer.line_to_byte(line))
    }

    /// Places the caret; `extend` keeps/starts a selection.
    pub fn set_caret(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.buffer.len_bytes());
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = offset;
        self.goal_column = None;
        self.buffer.break_group();
        self.reveal = Some(self.buffer.byte_to_line(offset));
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.buffer.len_bytes();
        self.goal_column = None;
        self.buffer.break_group();
    }

    /// Selects the word (or same-class run) around `offset`.
    pub fn select_word(&mut self, offset: usize) {
        let range = self.word_range(offset);
        self.anchor = Some(range.start);
        self.caret = range.end;
        self.goal_column = None;
        self.buffer.break_group();
        self.reveal = Some(self.buffer.byte_to_line(range.start));
    }

    /// Selects line `line`'s content (without its line break).
    pub fn select_line(&mut self, line: usize) {
        let range = self.buffer.line_byte_range(line.min(self.buffer.len_lines() - 1));
        self.anchor = Some(range.start);
        self.caret = range.end;
        self.goal_column = None;
        self.buffer.break_group();
        self.reveal = Some(line);
    }

    /// Inserts text at the caret, replacing any selection. One undo step
    /// per call for non-typing kinds; typing coalesces in the buffer.
    pub fn insert(&mut self, text: &str, kind: EditKind) {
        if text.is_empty() {
            return;
        }
        let caret_before = self.caret;
        match self.selection() {
            Some(range) => {
                let after = range.start + text.len();
                self.buffer
                    .replace(range, text, kind, caret_before, after);
                self.caret = after;
            }
            None => {
                let after = self.caret + text.len();
                self.buffer
                    .insert(self.caret, text, kind, caret_before, after);
                self.caret = after;
            }
        }
        self.anchor = None;
        self.goal_column = None;
        self.dirty = true;
        self.reveal = Some(self.buffer.byte_to_line(self.caret));
        self.scroll_caret_column();
    }

    /// Deletes the selection, or one character before the caret (a CRLF
    /// pair counts as one).
    pub fn backspace(&mut self) {
        if let Some(range) = self.selection() {
            let start = range.start;
            self.buffer
                .delete(range, EditKind::Delete, self.caret, start);
            self.caret = start;
        } else if self.caret > 0 {
            let mut start = self.buffer.char_to_byte(self.buffer.byte_to_char(self.caret) - 1);
            if self.caret - start == 1
                && self.buffer.slice_bytes(start..self.caret) == "\n"
                && start > 0
                && self.buffer.slice_bytes(start - 1..start) == "\r"
            {
                start -= 1;
            }
            self.buffer
                .delete(start..self.caret, EditKind::Delete, self.caret, start);
            self.caret = start;
        } else {
            return;
        }
        self.anchor = None;
        self.goal_column = None;
        self.dirty = true;
        self.reveal = Some(self.buffer.byte_to_line(self.caret));
        self.scroll_caret_column();
    }

    /// Deletes the selection, or one character after the caret.
    pub fn delete_forward(&mut self) {
        if let Some(range) = self.selection() {
            let start = range.start;
            self.buffer
                .delete(range, EditKind::Delete, self.caret, start);
            self.caret = start;
        } else if self.caret < self.buffer.len_bytes() {
            let mut end = self.buffer.char_to_byte(self.buffer.byte_to_char(self.caret) + 1);
            if end - self.caret == 1
                && self.buffer.slice_bytes(self.caret..end) == "\r"
                && end < self.buffer.len_bytes()
                && self.buffer.slice_bytes(end..end + 1) == "\n"
            {
                end += 1;
            }
            self.buffer
                .delete(self.caret..end, EditKind::Delete, self.caret, self.caret);
        } else {
            return;
        }
        self.anchor = None;
        self.goal_column = None;
        self.dirty = true;
        self.reveal = Some(self.buffer.byte_to_line(self.caret));
        self.scroll_caret_column();
    }

    /// Return with auto-indent: copies the current line's leading
    /// whitespace and adds two spaces after an opening `{`/`[` before the
    /// caret.
    pub fn enter(&mut self) {
        let (line, _) = self.line_col(self.caret);
        let range = self.buffer.line_byte_range(line);
        let prefix = self.buffer.slice_bytes(range.start..self.caret);
        let indent: String = prefix
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let opens = prefix.trim_end().ends_with('{') || prefix.trim_end().ends_with('[');
        let mut text = String::with_capacity(indent.len() + 3);
        text.push('\n');
        text.push_str(&indent);
        if opens {
            text.push_str("  ");
        }
        self.insert(&text, EditKind::Enter);
    }

    /// Tab inserts two spaces (the app's JSON indent).
    pub fn tab(&mut self) {
        self.insert("  ", EditKind::Typing);
    }

    /// Applies a caret movement. `page_rows` is the visible row count the
    /// view reports, for PageUp/PageDown.
    pub fn move_caret(&mut self, mv: Move, extend: bool, page_rows: usize) {
        let len = self.buffer.len_bytes();
        let caret = self.caret.min(len);
        let (line, col) = self.line_col(caret);
        let goal = self.goal_column.unwrap_or(col);
        let offset = match mv {
            Move::Left => {
                if caret == 0 {
                    0
                } else {
                    let mut start = self.buffer.char_to_byte(self.buffer.byte_to_char(caret) - 1);
                    if caret - start == 1
                        && self.buffer.slice_bytes(start..caret) == "\n"
                        && start > 0
                        && self.buffer.slice_bytes(start - 1..start) == "\r"
                    {
                        start -= 1;
                    }
                    start
                }
            }
            Move::Right => {
                if caret >= len {
                    len
                } else {
                    let mut end = self.buffer.char_to_byte(self.buffer.byte_to_char(caret) + 1);
                    if end - caret == 1
                        && self.buffer.slice_bytes(caret..end) == "\r"
                        && end < len
                        && self.buffer.slice_bytes(end..end + 1) == "\n"
                    {
                        end += 1;
                    }
                    end
                }
            }
            Move::Up | Move::Down | Move::PageUp | Move::PageDown => {
                let lines = self.buffer.len_lines();
                let target = match mv {
                    Move::Up => line.saturating_sub(1),
                    Move::Down => (line + 1).min(lines - 1),
                    Move::PageUp => line.saturating_sub(page_rows.max(1)),
                    _ => (line + page_rows.max(1)).min(lines - 1),
                };
                self.goal_column = Some(goal);
                self.column_offset(target, goal)
            }
            Move::Home => self.buffer.line_to_byte(line),
            Move::End => self.buffer.line_byte_range(line).end,
            Move::DocHome => 0,
            Move::DocEnd => len,
            Move::WordLeft => self.word_boundary(caret, false),
            Move::WordRight => self.word_boundary(caret, true),
        };
        let keep_goal = matches!(mv, Move::Up | Move::Down | Move::PageUp | Move::PageDown);
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = offset.min(len);
        if !keep_goal {
            self.goal_column = None;
        }
        self.buffer.break_group();
        self.reveal = Some(self.buffer.byte_to_line(self.caret));
        self.scroll_caret_column();
    }

    /// Pans the horizontal window by `delta` bytes (wheel / trackpad).
    pub fn pan_columns(&mut self, delta: isize) {
        self.first_column = (self.first_column as isize + delta).max(0) as usize;
    }

    /// Keeps the caret inside the horizontal window (`visible_columns` is
    /// the shaped width; a margin keeps the caret off the elided edge).
    pub fn set_horizontal_window(&mut self, visible_columns: usize) {
        let (_, col) = self.line_col(self.caret);
        let margin = 8.min(visible_columns / 4);
        if col < self.first_column {
            self.first_column = col;
        } else if col >= self.first_column + visible_columns.saturating_sub(margin) {
            self.first_column = col.saturating_sub(visible_columns - margin - 1);
        }
    }

    fn scroll_caret_column(&mut self) {
        let columns = self.visible_columns;
        self.set_horizontal_window(columns);
    }

    /// Byte offset of char column `goal` on line `line`, clamped to the
    /// line's content length.
    fn column_offset(&self, line: usize, goal: usize) -> usize {
        let range = self.buffer.line_byte_range(line);
        let line_chars = self.buffer.byte_to_char(range.end) - self.buffer.byte_to_char(range.start);
        let char_start = self.buffer.line_to_char(line);
        self.buffer
            .char_to_byte(char_start + goal.min(line_chars))
    }

    /// The same-class run around `offset`: word chars, whitespace, or a
    /// punctuation run. Scans are bounded (a 100 MB minified line never
    /// materializes more than the scan window).
    pub(crate) fn word_range(&self, offset: usize) -> Range<usize> {
        const SCAN: usize = 4096;
        let len = self.buffer.len_bytes();
        let offset = offset.min(len);
        let (line, _) = self.line_col(offset);
        let line_range = self.buffer.line_byte_range(line);
        if offset >= line_range.end {
            return offset..offset;
        }
        let class = |b: u8| -> u8 {
            if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
                0
            } else if b == b' ' || b == b'\t' {
                1
            } else {
                2
            }
        };
        let at = self.buffer.slice_bytes(offset..(offset + 1).min(len));
        let c = class(at.as_bytes()[0]);
        let lo = line_range.start.max(offset.saturating_sub(SCAN));
        let hi = line_range.end.min(offset + SCAN);
        let window = self.buffer.slice_bytes(lo..hi);
        let rel = offset - lo;
        let bytes = window.as_bytes();
        let mut start = rel;
        while start > 0 && class(bytes[start - 1]) == c {
            start -= 1;
        }
        let mut end = rel + 1;
        while end < bytes.len() && class(bytes[end]) == c {
            end += 1;
        }
        // A scan-window edge is not a real boundary; extend to it.
        (lo + start)..(lo + end)
    }

    /// Next word boundary in `forward` direction: skips the current run,
    /// then any whitespace (classic editor word motion).
    fn word_boundary(&self, offset: usize, forward: bool) -> usize {
        const SCAN: usize = 4096;
        let len = self.buffer.len_bytes();
        if forward {
            if offset >= len {
                return len;
            }
            let hi = len.min(offset + SCAN);
            let window = self.buffer.slice_bytes(offset..hi);
            let bytes = window.as_bytes();
            let mut i = 0;
            let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
            let is_space = |b: u8| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r';
            if is_word(bytes[0]) {
                while i < bytes.len() && is_word(bytes[i]) {
                    i += 1;
                }
            } else if is_space(bytes[0]) {
                while i < bytes.len() && is_space(bytes[i]) {
                    i += 1;
                }
            } else {
                while i < bytes.len() && !is_word(bytes[i]) && !is_space(bytes[i]) {
                    i += 1;
                }
            }
            while i < bytes.len() && is_space(bytes[i]) {
                i += 1;
            }
            offset + i
        } else {
            if offset == 0 {
                return 0;
            }
            let lo = offset.saturating_sub(SCAN);
            let window = self.buffer.slice_bytes(lo..offset);
            let bytes = window.as_bytes();
            let mut i = bytes.len();
            let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
            let is_space = |b: u8| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r';
            while i > 0 && is_space(bytes[i - 1]) {
                i -= 1;
            }
            if i > 0 && is_word(bytes[i - 1]) {
                while i > 0 && is_word(bytes[i - 1]) {
                    i -= 1;
                }
            } else {
                while i > 0 && !is_word(bytes[i - 1]) && !is_space(bytes[i - 1]) {
                    i -= 1;
                }
            }
            lo + i
        }
    }
}

thread_local! {
    static STORE: RefCell<HashMap<TabId, EditorState>> = RefCell::new(HashMap::new());
}

/// Runs `f` on the tab's editor state, syncing it from `Tab::input` first.
/// This is the only entry point; it keeps the buffer honest across file
/// loads, restores and direct test assignments without a poll hook.
pub fn with<R>(id: TabId, input: &str, revision: u64, f: impl FnOnce(&mut EditorState) -> R) -> R {
    STORE.with(|store| {
        let mut map = store.borrow_mut();
        let state = map.entry(id).or_default();
        state.sync(input, revision);
        f(state)
    })
}

/// O(1) rope snapshot for background jobs (after the sync check).
pub fn snapshot(id: TabId, input: &str, revision: u64) -> Rope {
    with(id, input, revision, |state| state.buffer.snapshot())
}

/// Takes a pending caret reveal from an *existing* entry, without creating
/// or syncing one: the runner poll must not build a rope for a tab no
/// editor view has ever shown.
pub fn take_reveal(id: TabId) -> Option<(usize, f32, f32)> {
    STORE.with(|store| {
        let mut map = store.borrow_mut();
        let state = map.get_mut(&id)?;
        let line = state.reveal.take()?;
        Some((line, state.scroll_y, state.viewport_h))
    })
}

/// O(1) snapshot from an *existing* entry (no create, no sync): background
/// jobs use it when the editor owns the text; `None` means the tab has no
/// live editor and `Tab::input` is the truth.
pub fn existing_snapshot(id: TabId) -> Option<Rope> {
    STORE.with(|store| {
        let map = store.borrow();
        map.get(&id).map(|state| state.buffer.snapshot())
    })
}

/// Snapshot only when the editor holds unmaterialized edits (persist).
pub fn dirty_snapshot(id: TabId) -> Option<Rope> {
    STORE.with(|store| {
        let map = store.borrow();
        map.get(&id)
            .filter(|state| state.dirty)
            .map(|state| state.buffer.snapshot())
    })
}

/// Whether the buffer holds edits `Tab::input` has not seen.
pub fn is_dirty(id: TabId, input: &str, revision: u64) -> bool {
    with(id, input, revision, |state| state.dirty)
}

/// Marks the buffer materialized (the caller copied it into `Tab::input`).
pub fn mark_clean(id: TabId, input: &str, revision: u64) {
    with(id, input, revision, |state| state.dirty = false);
}

/// Drops one tab's editor state (tab close).
pub fn remove(id: TabId) {
    STORE.with(|store| {
        store.borrow_mut().remove(&id);
    });
}

/// Drops every tab's editor state (tab restore).
pub fn clear_all() {
    STORE.with(|store| store.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> EditorState {
        let mut state = EditorState::default();
        state.sync(text, 1);
        state
    }

    #[test]
    fn sync_rebuilds_on_external_change_but_not_while_dirty() {
        let mut state = editor("abc");
        state.caret = 3;
        state.insert("X", EditKind::Typing);
        assert!(state.dirty);
        // Same revision, stale input, dirty buffer: the rope wins.
        state.sync("abc", 1);
        assert_eq!(state.buffer.to_string(), "abcX");
        // A new revision (update_input) rebuilds and resets the caret.
        state.sync("new text", 2);
        assert_eq!(state.buffer.to_string(), "new text");
        assert_eq!(state.caret, 0);
        assert!(!state.dirty);
        // Direct same-revision assignment with a different length rebuilds.
        state.sync("longer external text", 2);
        assert_eq!(state.buffer.to_string(), "longer external text");
    }

    #[test]
    fn insert_replaces_selection_and_moves_caret() {
        let mut state = editor("hello world");
        state.anchor = Some(0);
        state.caret = 5;
        state.insert("HI", EditKind::Paste);
        assert_eq!(state.buffer.to_string(), "HI world");
        assert_eq!(state.caret, 2);
        assert!(state.selection().is_none());
        assert_eq!(state.buffer.undo(), Some(5));
        assert_eq!(state.buffer.to_string(), "hello world");
    }

    #[test]
    fn backspace_and_delete_handle_crlf_and_unicode() {
        let mut state = editor("a\r\nb🌍c");
        state.caret = 3; // after \r\n
        state.backspace();
        assert_eq!(state.buffer.to_string(), "ab🌍c");
        assert_eq!(state.caret, 1);
        state.delete_forward(); // 'b'
        assert_eq!(state.buffer.to_string(), "a🌍c");
        state.delete_forward(); // 🌍 (4 bytes)
        assert_eq!(state.buffer.to_string(), "ac");
        state.caret = 1;
        state.backspace();
        assert_eq!(state.buffer.to_string(), "c");
    }

    #[test]
    fn enter_copies_indent_and_opens_brackets() {
        let mut state = editor("{\n  \"a\": 1");
        state.caret = state.buffer.len_bytes();
        state.enter();
        assert_eq!(state.buffer.to_string(), "{\n  \"a\": 1\n  ");
        let mut state = editor("  {");
        state.caret = 3;
        state.enter();
        assert_eq!(state.buffer.to_string(), "  {\n    ");
    }

    #[test]
    fn vertical_moves_keep_the_goal_column() {
        let mut state = editor("longline\nab\nlongline2");
        state.caret = 6; // column 6 on line 0
        state.move_caret(Move::Down, false, 10);
        assert_eq!(state.caret, 11); // clamped to end of "ab"
        state.move_caret(Move::Down, false, 10);
        assert_eq!(state.caret, 12 + 6); // goal column restored
        state.move_caret(Move::Up, false, 10);
        assert_eq!(state.caret, 11);
    }

    #[test]
    fn word_moves_and_selection() {
        let mut state = editor("hello  world\tfoo");
        state.caret = 0;
        state.move_caret(Move::WordRight, false, 10);
        assert_eq!(state.caret, 7); // skips word + spaces
        state.move_caret(Move::WordLeft, false, 10);
        assert_eq!(state.caret, 0);
        state.select_word(8); // inside "world"
        assert_eq!(state.selection(), Some(7..12));
        state.select_line(0);
        assert_eq!(state.selection(), Some(0..16));
    }

    #[test]
    fn shift_moves_extend_selection() {
        let mut state = editor("abcdef");
        state.caret = 2;
        state.move_caret(Move::Right, true, 10);
        state.move_caret(Move::Right, true, 10);
        assert_eq!(state.selection(), Some(2..4));
        state.move_caret(Move::Left, false, 10);
        assert!(state.selection().is_none());
    }

    #[test]
    fn horizontal_window_follows_the_caret() {
        let mut state = editor(&"x".repeat(10_000));
        state.caret = 5_000;
        state.set_horizontal_window(240);
        assert!(state.first_column <= 5_000);
        assert!(state.first_column + 240 > 5_000);
        state.caret = 10;
        state.set_horizontal_window(240);
        assert_eq!(state.first_column, 10);
    }

    #[test]
    fn store_isolates_tabs_and_resyncs() {
        clear_all();
        with(1, "one", 10, |s| {
            s.caret = 3;
            s.insert("!", EditKind::Typing);
        });
        with(2, "two", 20, |s| {
            s.caret = 3;
            s.insert("?", EditKind::Typing);
        });
        assert_eq!(with(1, "one", 10, |s| s.buffer.to_string()), "one!");
        assert_eq!(with(2, "two", 20, |s| s.buffer.to_string()), "two?");
        assert!(is_dirty(1, "one", 10));
        mark_clean(1, "one!", 10);
        assert!(!is_dirty(1, "one!", 10));
        remove(1);
        assert_eq!(with(1, "one", 10, |s| s.buffer.to_string()), "one");
        clear_all();
    }
}