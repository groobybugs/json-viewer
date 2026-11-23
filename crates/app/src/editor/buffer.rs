//! Rope-backed text buffer with grouped undo/redo (rp-editor E1).
//!
//! The buffer is the source of truth for the input editor at any document
//! size. Edits are O(log n) rope operations, snapshots are O(1) `Rope` clones
//! for background jobs (Format/Minify/Validate/Search/persist convert to a
//! `String` on the worker thread, never here), and undo steps coalesce
//! consecutive typing into words while a paste stays one step.
//!
//! All offsets are **byte** offsets into the UTF-8 text unless a name says
//! `chars`. Line semantics follow ropey: `\n`, `\r\n` and lone `\r` each end
//! one line, and a trailing line break starts a final empty line.

use std::collections::VecDeque;
use std::ops::Range;

use ropey::Rope;

/// What produced an edit; decides undo coalescing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// A typed character run. Consecutive inserts at the caret coalesce while
    /// they stay inside one word (or one whitespace run).
    Typing,
    /// An input-method commit; one undo step per composition.
    Ime,
    /// A paste (any size); always one undo step.
    Paste,
    /// Return / auto-indent; always one undo step.
    Enter,
    /// Backspace / Delete / selection delete.
    Delete,
    /// A bulk replacement (replace-range, set-text flows).
    Replace,
}

/// Text held by one undo step. Incremental edits carry `Text`; a
/// whole-document replacement (Format/Minify in-place, rp-format-inplace)
/// carries `Rope` handles — an O(1) clone of the live buffer — or a moved
/// materialized cache (`Text`), so the step never copies the document.
#[derive(Clone, Debug, PartialEq)]
pub enum EditText {
    Text(String),
    Rope(Rope),
}

impl EditText {
    pub fn len_bytes(&self) -> usize {
        match self {
            EditText::Text(text) => text.len(),
            EditText::Rope(rope) => rope.len_bytes(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// The text of an incremental payload; `None` for a rope handle.
    fn as_str(&self) -> Option<&str> {
        match self {
            EditText::Text(text) => Some(text),
            EditText::Rope(_) => None,
        }
    }

    /// A rope holding this text: O(1) for a rope handle, one O(n) build for
    /// a moved cache string (the undo path of a tab that had no live editor).
    fn to_rope(&self) -> Rope {
        match self {
            EditText::Text(text) => Rope::from_str(text),
            EditText::Rope(rope) => rope.clone(),
        }
    }
}

/// One undo step. `removed`/`inserted` are the text the edit took out and put
/// in at `offset`; coalesced typing merges into a single step. Caret positions
/// are byte offsets the editor restores on undo/redo. A whole-document step
/// (either payload a `Rope`) swaps the buffer instead of editing at `offset`.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub offset: usize,
    pub removed: EditText,
    pub inserted: EditText,
    pub caret_before: usize,
    pub caret_after: usize,
}

impl Edit {
    /// Bytes this step holds against the undo/redo history cap.
    fn byte_cost(&self) -> usize {
        self.removed.len_bytes() + self.inserted.len_bytes()
    }

    /// Whole-document replacement: undo/redo swap ropes in O(1).
    fn is_whole_document(&self) -> bool {
        matches!(self.removed, EditText::Rope(_)) || matches!(self.inserted, EditText::Rope(_))
    }
}

/// Undo history cap (removed + inserted bytes). Large pastes stay undoable
/// once; the oldest steps fall off beyond this so RSS stays bounded.
const UNDO_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// A rope plus its edit history. `Clone` is cheap (the rope clone is O(1);
/// the history vectors clone their text, so snapshots for workers use
/// [`Buffer::snapshot`], not `Clone`).
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    rope: Rope,
    revision: u64,
    undo: VecDeque<Edit>,
    redo: VecDeque<Edit>,
    undo_bytes: usize,
    redo_bytes: usize,
    /// Set by [`Buffer::break_group`]; the next edit starts a fresh step even
    /// if it is contiguous with the previous typing.
    break_pending: bool,
    /// Bounded log of (revision, touched byte range) for every mutation, in
    /// revision order. Lets cached per-line analysis (lexer checkpoints)
    /// prove an edit did not touch a line without rescanning it.
    recent: VecDeque<(u64, Range<usize>)>,
}

impl Buffer {
    /// An empty buffer (one empty line).
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a buffer from text, with empty history.
    pub fn from_text(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Bumped on every mutation (edit, undo, redo, set-text). The view keys
    /// its shaped-line cache and row version by it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// O(1) snapshot for background jobs; later edits never touch it.
    pub fn snapshot(&self) -> Rope {
        self.rope.clone()
    }

    /// Byte ranges touched by mutations newer than `revision`, or `None`
    /// when the log cannot prove coverage back to that revision (too old,
    /// or cleared). `Some(empty)` means nothing changed since.
    pub fn edits_since(&self, revision: u64) -> Option<Vec<Range<usize>>> {
        if revision == self.revision {
            return Some(Vec::new());
        }
        let front = self.recent.front()?;
        if front.0 > revision + 1 {
            return None;
        }
        Some(
            self.recent
                .iter()
                .filter(|(rev, _)| *rev > revision)
                .map(|(_, range)| range.clone())
                .collect(),
        )
    }

    fn note_edit(&mut self, range: Range<usize>) {
        self.recent.push_back((self.revision, range));
        while self.recent.len() > 1024 {
            self.recent.pop_front();
        }
    }

    // ── Offset mapping ───────────────────────────────────────────────────

    pub fn byte_to_char(&self, byte: usize) -> usize {
        self.rope.byte_to_char(byte)
    }

    pub fn char_to_byte(&self, char_offset: usize) -> usize {
        self.rope.char_to_byte(char_offset)
    }

    pub fn byte_to_line(&self, byte: usize) -> usize {
        self.rope.byte_to_line(byte)
    }

    pub fn line_to_byte(&self, line: usize) -> usize {
        self.rope.line_to_byte(line)
    }

    pub fn char_to_line(&self, char_offset: usize) -> usize {
        self.rope.char_to_line(char_offset)
    }

    pub fn line_to_char(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    /// Byte range of line `line`'s content, excluding its line break.
    pub fn line_byte_range(&self, line: usize) -> Range<usize> {
        let start = self.line_to_byte(line);
        let mut end = start + self.rope.line(line).len_bytes();
        if end > start && self.rope.byte(end - 1) == b'\n' {
            end -= 1;
            if end > start && self.rope.byte(end - 1) == b'\r' {
                end -= 1;
            }
        } else if end > start && self.rope.byte(end - 1) == b'\r' {
            end -= 1;
        }
        start..end
    }

    /// Copies `range` (bytes) out of the rope. O(range + log n); the view
    /// slices only the visible column window of very long lines.
    pub fn slice_bytes(&self, range: Range<usize>) -> String {
        let chars = self.rope.byte_to_char(range.start)..self.rope.byte_to_char(range.end);
        self.rope.slice(chars).to_string()
    }

    /// The whole line `line` without its line break. Prefer
    /// [`Buffer::slice_bytes`] over a column window for very long lines.
    pub fn line_text(&self, line: usize) -> String {
        self.slice_bytes(self.line_byte_range(line))
    }

    // ── Edits ────────────────────────────────────────────────────────────

    /// Inserts `text` at byte `offset`. Caret positions are recorded for
    /// undo; the caller owns the caret.
    pub fn insert(
        &mut self,
        offset: usize,
        text: &str,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        if text.is_empty() {
            return;
        }
        self.rope.insert(self.rope.byte_to_char(offset), text);
        self.revision += 1;
        self.note_edit(offset..offset + text.len());
        self.push_edit(
            Edit {
                offset,
                removed: EditText::Text(String::new()),
                inserted: EditText::Text(text.to_owned()),
                caret_before,
                caret_after,
            },
            kind,
        );
    }

    /// Deletes byte `range`.
    pub fn delete(
        &mut self,
        range: Range<usize>,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        if range.start >= range.end {
            return;
        }
        let removed = self.slice_bytes(range.clone());
        let chars = self.rope.byte_to_char(range.start)..self.rope.byte_to_char(range.end);
        self.rope.remove(chars);
        self.revision += 1;
        self.note_edit(range.clone());
        self.push_edit(
            Edit {
                offset: range.start,
                removed: EditText::Text(removed),
                inserted: EditText::Text(String::new()),
                caret_before,
                caret_after,
            },
            kind,
        );
    }

    /// Replaces byte `range` with `text` (one undo step).
    pub fn replace(
        &mut self,
        range: Range<usize>,
        text: &str,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        if range.start >= range.end && text.is_empty() {
            return;
        }
        let removed = self.slice_bytes(range.clone());
        let chars = self.rope.byte_to_char(range.start)..self.rope.byte_to_char(range.end);
        self.rope.remove(chars);
        self.rope.insert(self.rope.byte_to_char(range.start), text);
        self.revision += 1;
        self.note_edit(range.start..range.start + removed.len().max(text.len()));
        self.push_edit(
            Edit {
                offset: range.start,
                removed: EditText::Text(removed),
                inserted: EditText::Text(text.to_owned()),
                caret_before,
                caret_after,
            },
            kind,
        );
    }

    /// Replaces the whole document as one undo step (Format/Minify in-place,
    /// rp-format-inplace). The previous rope is kept as an O(1) clone and
    /// `new_rope` is moved in, so neither side copies text; undo/redo swap
    /// ropes in O(1). The caller decides whether a replacement is needed
    /// (identical output is detected off the UI thread by the JSON worker).
    pub fn replace_all(&mut self, new_rope: Rope, caret_before: usize, caret_after: usize) {
        let old = std::mem::replace(&mut self.rope, new_rope);
        self.revision += 1;
        let span = self.rope.len_bytes().max(old.len_bytes());
        self.note_edit(0..span);
        self.push_edit(
            Edit {
                offset: 0,
                removed: EditText::Rope(old),
                inserted: EditText::Rope(self.rope.clone()),
                caret_before,
                caret_after,
            },
            EditKind::Replace,
        );
    }

    /// A buffer already holding a Format/Minify result, with the previous
    /// document as one undo step. `previous` is moved (a materialized cache),
    /// never copied; used when the tab had no live editor entry, so no old
    /// rope is built on the UI thread.
    pub fn from_replaced(
        previous: EditText,
        new_rope: Rope,
        caret_before: usize,
        caret_after: usize,
    ) -> Self {
        let mut buf = Self {
            rope: new_rope.clone(),
            ..Self::default()
        };
        buf.revision = 1;
        let len = buf.rope.len_bytes();
        buf.note_edit(0..len);
        let edit = Edit {
            offset: 0,
            removed: previous,
            inserted: EditText::Rope(new_rope),
            caret_before,
            caret_after,
        };
        buf.undo_bytes = edit.byte_cost();
        buf.undo.push_back(edit);
        buf
    }

    /// Char-offset convenience over [`Buffer::insert`].
    pub fn insert_chars(
        &mut self,
        char_offset: usize,
        text: &str,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        let offset = self.rope.char_to_byte(char_offset);
        self.insert(offset, text, kind, caret_before, caret_after);
    }

    /// Char-offset convenience over [`Buffer::delete`].
    pub fn delete_chars(
        &mut self,
        range: Range<usize>,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        let bytes = self.rope.char_to_byte(range.start)..self.rope.char_to_byte(range.end);
        self.delete(bytes, kind, caret_before, caret_after);
    }

    /// Char-offset convenience over [`Buffer::replace`].
    pub fn replace_chars(
        &mut self,
        range: Range<usize>,
        text: &str,
        kind: EditKind,
        caret_before: usize,
        caret_after: usize,
    ) {
        let bytes = self.rope.char_to_byte(range.start)..self.rope.char_to_byte(range.end);
        self.replace(bytes, text, kind, caret_before, caret_after);
    }

    /// Replaces the whole text and clears history (file load, clear, restore).
    pub fn set_text(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.revision += 1;
        self.recent.clear();
        self.note_edit(0..text.len());
        self.undo.clear();
        self.redo.clear();
        self.undo_bytes = 0;
        self.redo_bytes = 0;
        self.break_pending = false;
    }

    /// Ends the open typing run (caret moves, focus changes, mode switches).
    pub fn break_group(&mut self) {
        self.break_pending = true;
    }

    // ── Undo / redo ──────────────────────────────────────────────────────

    /// Undoes one step; returns the caret to restore, or `None` when history
    /// is empty.
    pub fn undo(&mut self) -> Option<usize> {
        let mut edit = self.undo.pop_back()?;
        self.undo_bytes -= edit.byte_cost();
        if edit.is_whole_document() {
            self.rope = edit.removed.to_rope();
            // Normalize the consumed side to an O(1) handle so later
            // undo/redo cycles never rebuild a rope from the moved cache.
            edit.removed = EditText::Rope(self.rope.clone());
        } else {
            if let Some(inserted) = edit.inserted.as_str().filter(|text| !text.is_empty()) {
                let chars = self.rope.byte_to_char(edit.offset)
                    ..self.rope.byte_to_char(edit.offset + inserted.len());
                self.rope.remove(chars);
            }
            if let Some(removed) = edit.removed.as_str().filter(|text| !text.is_empty()) {
                self.rope.insert(self.rope.byte_to_char(edit.offset), removed);
            }
        }
        self.revision += 1;
        self.break_pending = true;
        let caret = edit.caret_before;
        let span = edit.removed.len_bytes().max(edit.inserted.len_bytes());
        self.note_edit(edit.offset..edit.offset + span);
        self.redo_bytes += edit.byte_cost();
        self.redo.push_back(edit);
        cap_history(&mut self.redo, &mut self.redo_bytes);
        Some(caret)
    }

    /// Redoes one step; returns the caret to restore.
    pub fn redo(&mut self) -> Option<usize> {
        let mut edit = self.redo.pop_back()?;
        self.redo_bytes -= edit.byte_cost();
        if edit.is_whole_document() {
            self.rope = edit.inserted.to_rope();
            edit.inserted = EditText::Rope(self.rope.clone());
        } else {
            if let Some(removed) = edit.removed.as_str().filter(|text| !text.is_empty()) {
                let chars = self.rope.byte_to_char(edit.offset)
                    ..self.rope.byte_to_char(edit.offset + removed.len());
                self.rope.remove(chars);
            }
            if let Some(inserted) = edit.inserted.as_str().filter(|text| !text.is_empty()) {
                self.rope.insert(self.rope.byte_to_char(edit.offset), inserted);
            }
        }
        self.revision += 1;
        self.break_pending = true;
        let caret = edit.caret_after;
        let span = edit.removed.len_bytes().max(edit.inserted.len_bytes());
        self.note_edit(edit.offset..edit.offset + span);
        self.undo_bytes += edit.byte_cost();
        self.undo.push_back(edit);
        cap_history(&mut self.undo, &mut self.undo_bytes);
        Some(caret)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    // ── Internals ────────────────────────────────────────────────────────

    fn push_edit(&mut self, edit: Edit, kind: EditKind) {
        self.redo.clear();
        self.redo_bytes = 0;
        let coalesce = kind == EditKind::Typing
            && !self.break_pending
            && matches!(edit.inserted, EditText::Text(_))
            && !edit.inserted.is_empty()
            && edit.removed.is_empty()
            && self.undo.back().is_some_and(|last| {
                last.removed.is_empty()
                    && matches!(last.inserted, EditText::Text(_))
                    && last.offset + last.inserted.len_bytes() == edit.offset
                    && last.inserted.as_str().and_then(|text| text.chars().next_back()).is_some_and(|a| {
                        edit.inserted.as_str().and_then(|text| text.chars().next()).is_some_and(|b| {
                            word_class(a) == word_class(b) && word_class(a) != 2
                        })
                    })
            });
        if coalesce {
            let last = self.undo.back_mut().expect("checked above");
            let EditText::Text(last_text) = &mut last.inserted else {
                unreachable!("coalescing checked for a Text payload");
            };
            let EditText::Text(new_text) = &edit.inserted else {
                unreachable!("coalescing checked for a Text payload");
            };
            self.undo_bytes -= last_text.len();
            last_text.push_str(new_text);
            last.caret_after = edit.caret_after;
            self.undo_bytes += last_text.len();
        } else {
            self.undo_bytes += edit.byte_cost();
            self.undo.push_back(edit);
        }
        self.break_pending = false;
        cap_history(&mut self.undo, &mut self.undo_bytes);
    }
}

/// Drops oldest steps beyond the byte cap, always keeping at least one.
fn cap_history(queue: &mut VecDeque<Edit>, bytes: &mut usize) {
    while *bytes > UNDO_LIMIT_BYTES && queue.len() > 1 {
        if let Some(old) = queue.pop_front() {
            *bytes -= old.byte_cost();
        }
    }
}

/// Word class for typing coalescing: 0 word chars, 1 whitespace, 2 other.
/// Punctuation never coalesces; a word and the space after it are separate
/// undo steps.
fn word_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() && c != '\n' && c != '\r' {
        1
    } else {
        2
    }
}

/// O(n) materialization through `to_string()`. Worker threads and explicit
/// bulk actions only — never the keystroke path.
impl std::fmt::Display for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Buffer {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(buf: &mut Buffer, at: usize, text: &str) {
        buf.insert(at, text, EditKind::Typing, at, at + text.len());
    }

    #[test]
    fn insert_delete_replace_and_revision() {
        let mut buf = Buffer::from_text("hello");
        let r0 = buf.revision();
        buf.insert(5, " world", EditKind::Typing, 5, 11);
        assert_eq!(buf.to_string(), "hello world");
        assert!(buf.revision() > r0);
        buf.delete(0..6, EditKind::Delete, 6, 0);
        assert_eq!(buf.to_string(), "world");
        buf.replace(0..5, "WORLD", EditKind::Replace, 0, 5);
        assert_eq!(buf.to_string(), "WORLD");
        assert_eq!(buf.len_bytes(), 5);
        assert_eq!(buf.len_chars(), 5);
        assert_eq!(buf.len_lines(), 1);
    }

    #[test]
    fn char_and_byte_offsets_agree_on_ascii_and_diverge_on_unicode() {
        let mut buf = Buffer::from_text("héllo🌍");
        // h=1, é=2, l l o=3, 🌍=4 bytes.
        assert_eq!(buf.len_bytes(), 10);
        assert_eq!(buf.len_chars(), 6);
        assert_eq!(buf.char_to_byte(2), 3); // after "hé"
        assert_eq!(buf.byte_to_char(3), 2);
        buf.insert_chars(2, "X", EditKind::Typing, 3, 4);
        assert_eq!(buf.to_string(), "héXllo🌍");
        buf.delete_chars(2..3, EditKind::Delete, 4, 3);
        assert_eq!(buf.to_string(), "héllo🌍");
        buf.replace_chars(5..6, "!", EditKind::Replace, 9, 10);
        assert_eq!(buf.to_string(), "héllo!");
    }

    #[test]
    fn line_mapping_counts_crlf_as_one_break() {
        let buf = Buffer::from_text("a\r\nbb\nccc\r");
        assert_eq!(buf.len_lines(), 4); // a / bb / ccc / (empty after \r)
        assert_eq!(buf.line_byte_range(0), 0..1);
        assert_eq!(buf.line_byte_range(1), 3..5);
        assert_eq!(buf.line_byte_range(2), 6..9);
        assert_eq!(buf.byte_to_line(4), 1);
        assert_eq!(buf.line_to_byte(2), 6);
        assert_eq!(buf.line_text(1), "bb");
        let trailing = Buffer::from_text("x\n");
        assert_eq!(trailing.len_lines(), 2);
        assert_eq!(trailing.line_byte_range(1), 2..2);
    }

    #[test]
    fn slice_bytes_bounds_a_huge_line() {
        let mut text = String::from("[");
        text.push_str(&"1,".repeat(100_000));
        text.push(']');
        let buf = Buffer::from_text(&text);
        assert_eq!(buf.len_lines(), 1);
        let window = buf.slice_bytes(100_000..100_020);
        assert_eq!(window.len(), 20);
        assert_eq!(window, &text[100_000..100_020]);
    }

    #[test]
    fn typing_coalesces_into_words_and_breaks_on_caret_moves() {
        let mut buf = Buffer::new();
        type_str(&mut buf, 0, "h");
        type_str(&mut buf, 1, "e");
        type_str(&mut buf, 2, "l");
        type_str(&mut buf, 3, "l");
        type_str(&mut buf, 4, "o");
        type_str(&mut buf, 5, " ");
        type_str(&mut buf, 6, "w");
        type_str(&mut buf, 7, "o");
        assert_eq!(buf.to_string(), "hello wo");
        assert_eq!(buf.undo.len(), 3); // "hello" + " " + "wo"
        // Caret move breaks the run even at a contiguous offset.
        buf.break_group();
        type_str(&mut buf, 8, "r");
        assert_eq!(buf.undo.len(), 4);
        // Undo restores text and caret step by step.
        assert_eq!(buf.undo(), Some(8));
        assert_eq!(buf.to_string(), "hello wo");
        assert_eq!(buf.undo(), Some(6));
        assert_eq!(buf.to_string(), "hello ");
        assert_eq!(buf.undo(), Some(5));
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "");
        assert_eq!(buf.undo(), None);
        // Redo replays forward.
        assert_eq!(buf.redo(), Some(5));
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(buf.redo(), Some(6));
        assert_eq!(buf.redo(), Some(8));
        assert_eq!(buf.redo(), Some(9));
        assert_eq!(buf.to_string(), "hello wor");
        assert_eq!(buf.redo(), None);
    }

    #[test]
    fn paste_is_one_step_and_undo_restores_the_selection_replacement() {
        let mut buf = Buffer::from_text("abc");
        buf.replace(1..2, "PASTED", EditKind::Paste, 2, 7);
        assert_eq!(buf.to_string(), "aPASTEDc");
        assert_eq!(buf.undo.len(), 1);
        assert_eq!(buf.undo(), Some(2));
        assert_eq!(buf.to_string(), "abc");
        assert_eq!(buf.redo(), Some(7));
        assert_eq!(buf.to_string(), "aPASTEDc");
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut buf = Buffer::from_text("abc");
        type_str(&mut buf, 3, "d");
        buf.undo();
        assert!(buf.can_redo());
        type_str(&mut buf, 3, "e");
        assert!(!buf.can_redo());
        assert_eq!(buf.to_string(), "abce");
    }

    #[test]
    fn delete_undo_restores_text() {
        let mut buf = Buffer::from_text("hello world");
        buf.delete(5..11, EditKind::Delete, 11, 5);
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(buf.undo(), Some(11));
        assert_eq!(buf.to_string(), "hello world");
    }

    #[test]
    fn snapshot_is_isolated_from_later_edits() {
        let mut buf = Buffer::from_text("original");
        let snap = buf.snapshot();
        buf.insert(8, " edited", EditKind::Typing, 8, 15);
        assert_eq!(snap.to_string(), "original");
        assert_eq!(buf.to_string(), "original edited");
    }

    #[test]
    fn set_text_clears_history() {
        let mut buf = Buffer::from_text("abc");
        type_str(&mut buf, 3, "d");
        buf.set_text("new");
        assert!(!buf.can_undo());
        assert!(!buf.can_redo());
        assert_eq!(buf.to_string(), "new");
    }

    #[test]
    fn undo_history_is_capped() {
        let mut buf = Buffer::new();
        // 80 MB of distinct paste steps: the cap drops the oldest, keeps the
        // newest, and never drops the last remaining step.
        let chunk = "x".repeat(1024 * 1024);
        for i in 0..80 {
            buf.insert(i * chunk.len(), &chunk, EditKind::Paste, 0, 0);
        }
        assert!(buf.undo_bytes <= UNDO_LIMIT_BYTES + chunk.len());
        assert!(buf.can_undo());
        while buf.undo().is_some() {}
        assert!(buf.is_empty() || buf.len_bytes() < 80 * chunk.len());
    }

    #[test]
    fn replace_all_is_one_undo_step_and_restores_text_and_caret() {
        let mut buf = Buffer::from_text("{\"a\":1}");
        let formatted = Rope::from_str("{\n  \"a\": 1\n}");
        buf.replace_all(formatted, 4, 6);
        assert_eq!(buf.to_string(), "{\n  \"a\": 1\n}");
        assert_eq!(buf.undo.len(), 1, "format must be ONE undo step");
        assert_eq!(buf.undo(), Some(4));
        assert_eq!(buf.to_string(), "{\"a\":1}");
        assert_eq!(buf.redo(), Some(6));
        assert_eq!(buf.to_string(), "{\n  \"a\": 1\n}");
        // A second replacement after undo clears the redo branch.
        buf.undo();
        buf.replace_all(Rope::from_str("[1]"), 0, 3);
        assert!(!buf.can_redo());
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "{\"a\":1}");
    }

    #[test]
    fn replace_all_round_trips_unicode_and_keeps_the_edit_log_honest() {
        let original = "{\"e\":\"🚀\",\"k\":\"café\"}";
        let mut buf = Buffer::from_text(original);
        let revision = buf.revision();
        let formatted = Rope::from_str("{\n  \"e\": \"🚀\",\n  \"k\": \"café\"\n}");
        buf.replace_all(formatted, 6, 10);
        assert!(buf.revision() > revision);
        // The whole-document edit invalidates every cached line analysis.
        let edits = buf.edits_since(revision).expect("log covers the replace");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].start, 0);
        assert!(edits[0].end >= buf.len_bytes());
        assert_eq!(buf.undo(), Some(6));
        assert_eq!(buf.to_string(), original);
        assert_eq!(buf.redo(), Some(10));
        assert_eq!(buf.to_string(), "{\n  \"e\": \"🚀\",\n  \"k\": \"café\"\n}");
    }

    #[test]
    fn typing_after_replace_all_starts_a_fresh_step() {
        let mut buf = Buffer::from_text("old");
        buf.replace_all(Rope::from_str("new text"), 0, 0);
        buf.insert(8, "!", EditKind::Typing, 8, 9);
        assert_eq!(buf.undo.len(), 2, "typing must not coalesce into the bulk step");
        assert_eq!(buf.undo(), Some(8));
        assert_eq!(buf.to_string(), "new text");
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "old");
    }

    #[test]
    fn from_replaced_moves_the_previous_cache_into_one_undo_step() {
        // The no-live-editor case: the old document arrives as a moved
        // String, the result as a prepared rope; neither is copied.
        let previous = "{\"a\":1}".to_owned();
        let mut buf = Buffer::from_replaced(
            EditText::Text(previous),
            Rope::from_str("{\n  \"a\": 1\n}"),
            0,
            0,
        );
        assert_eq!(buf.to_string(), "{\n  \"a\": 1\n}");
        assert_eq!(buf.undo.len(), 1);
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "{\"a\":1}");
        // The consumed Text side is normalized to a rope: redo is O(1).
        assert_eq!(buf.redo(), Some(0));
        assert_eq!(buf.to_string(), "{\n  \"a\": 1\n}");
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "{\"a\":1}");
    }

    #[test]
    fn bulk_step_respects_the_undo_cap() {
        let mut buf = Buffer::new();
        let chunk = "x".repeat(1024 * 1024);
        // 60 MB of pastes, then a 10 MB whole-document replacement: the cap
        // drops the oldest pastes but keeps the newest step undoable.
        for i in 0..60 {
            buf.insert(i * chunk.len(), &chunk, EditKind::Paste, 0, 0);
        }
        let big = Rope::from_str(&"y".repeat(10 * 1024 * 1024));
        buf.replace_all(big, 0, 0);
        assert!(buf.undo_bytes <= UNDO_LIMIT_BYTES + 10 * chunk.len());
        assert!(buf.can_undo());
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.len_bytes(), 60 * chunk.len());
        // A bulk step alone beyond the cap is still kept (at least one step).
        let mut buf = Buffer::from_text("small");
        buf.replace_all(Rope::from_str(&"z".repeat(80 * 1024 * 1024)), 0, 0);
        assert!(buf.undo_bytes > UNDO_LIMIT_BYTES);
        assert_eq!(buf.undo.len(), 1);
        assert_eq!(buf.undo(), Some(0));
        assert_eq!(buf.to_string(), "small");
    }

    #[test]
    fn slice_bytes_at_mid_char_offsets_snaps_to_whole_chars() {
        // Audit: slice_bytes maps both ends through byte_to_char, which
        // floors mid-char offsets to the char start — never a panic, never
        // a partial char.
        let buf = Buffer::from_text("a🚀bé");
        // a=0..1, 🚀=1..5, b=5..6, é=6..8.
        assert_eq!(buf.slice_bytes(2..4), ""); // both ends inside the emoji
        assert_eq!(buf.slice_bytes(0..2), "a"); // end floors to the emoji start
        assert_eq!(buf.slice_bytes(1..5), "🚀"); // exact boundaries
        assert_eq!(buf.slice_bytes(7..8), "é"); // start inside é floors to 6
        assert_eq!(buf.slice_bytes(3..3), ""); // empty mid-char window
    }

    #[test]
    fn line_byte_range_with_multibyte_and_crlf_stays_on_boundaries() {
        // Audit: the line-break trim compares raw bytes; continuation bytes
        // (>= 0x80) never equal \r or \n, so multibyte line ends are safe.
        let buf = Buffer::from_text("a🚀\r\n中文\né\r");
        assert_eq!(buf.line_byte_range(0), 0..5); // a + 🚀, CRLF stripped
        assert_eq!(buf.line_byte_range(1), 7..13); // 中文 (2x3 bytes)
        assert_eq!(buf.line_byte_range(2), 14..16); // é, lone \r stripped
        for line in 0..buf.len_lines() {
            let range = buf.line_byte_range(line);
            for off in [range.start, range.end] {
                assert_eq!(buf.char_to_byte(buf.byte_to_char(off)), off);
            }
        }
    }

    #[test]
    fn edits_at_mid_char_offsets_floor_to_char_starts() {
        // Audit: insert/delete/replace map through byte_to_char, so a
        // mid-char offset edits at the char start instead of splitting it.
        let mut buf = Buffer::from_text("a🚀b");
        buf.insert(2, "X", EditKind::Typing, 2, 3); // inside the emoji → char start 1
        assert_eq!(buf.to_string(), "aX🚀b");
        // Now a=0, X=1, 🚀=2..6, b=6..7.
        buf.delete(6..7, EditKind::Delete, 7, 6);
        assert_eq!(buf.to_string(), "aX🚀");
        // Undo/redo restore whole chars and boundary carets.
        let mut buf = Buffer::from_text("中文");
        buf.insert(3, "🚀", EditKind::Paste, 3, 7); // after 中 (byte 3 is a boundary)
        assert_eq!(buf.to_string(), "中🚀文");
        assert_eq!(buf.undo(), Some(3));
        assert_eq!(buf.to_string(), "中文");
        assert_eq!(buf.redo(), Some(7));
        assert_eq!(buf.to_string(), "中🚀文");
    }

    #[test]
    fn hundred_mb_insert_at_middle_is_fast() {
        let chunk = "0123456789abcdef".repeat(64 * 1024); // 1 MiB
        let mut big = String::with_capacity(100 * 1024 * 1024 + 16);
        for _ in 0..100 {
            big.push_str(&chunk);
        }
        let mut buf = Buffer::from_text(&big);
        let mid = buf.len_bytes() / 2;
        let started = std::time::Instant::now();
        buf.insert(mid, "INSERTED", EditKind::Typing, mid, mid + 8);
        let elapsed = started.elapsed();
        assert_eq!(buf.len_bytes(), 100 * 1024 * 1024 + 8);
        assert_eq!(buf.slice_bytes(mid..mid + 8), "INSERTED");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "100 MB middle insert took {elapsed:?}"
        );
        let started = std::time::Instant::now();
        let snap = buf.snapshot();
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(snap.len_bytes(), buf.len_bytes());
    }
}