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

/// One undo step. `removed`/`inserted` are the text the edit took out and put
/// in at `offset`; coalesced typing merges into a single step. Caret positions
/// are byte offsets the editor restores on undo/redo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub offset: usize,
    pub removed: String,
    pub inserted: String,
    pub caret_before: usize,
    pub caret_after: usize,
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
        self.push_edit(
            Edit {
                offset,
                removed: String::new(),
                inserted: text.to_owned(),
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
        self.push_edit(
            Edit {
                offset: range.start,
                removed,
                inserted: String::new(),
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
        self.push_edit(
            Edit {
                offset: range.start,
                removed,
                inserted: text.to_owned(),
                caret_before,
                caret_after,
            },
            kind,
        );
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
        let edit = self.undo.pop_back()?;
        self.undo_bytes -= edit.removed.len() + edit.inserted.len();
        if !edit.inserted.is_empty() {
            let chars = self.rope.byte_to_char(edit.offset)
                ..self.rope.byte_to_char(edit.offset + edit.inserted.len());
            self.rope.remove(chars);
        }
        if !edit.removed.is_empty() {
            self.rope.insert(self.rope.byte_to_char(edit.offset), &edit.removed);
        }
        self.revision += 1;
        self.break_pending = true;
        let caret = edit.caret_before;
        self.redo_bytes += edit.removed.len() + edit.inserted.len();
        self.redo.push_back(edit);
        cap_history(&mut self.redo, &mut self.redo_bytes);
        Some(caret)
    }

    /// Redoes one step; returns the caret to restore.
    pub fn redo(&mut self) -> Option<usize> {
        let edit = self.redo.pop_back()?;
        self.redo_bytes -= edit.removed.len() + edit.inserted.len();
        if !edit.removed.is_empty() {
            let chars = self.rope.byte_to_char(edit.offset)
                ..self.rope.byte_to_char(edit.offset + edit.removed.len());
            self.rope.remove(chars);
        }
        if !edit.inserted.is_empty() {
            self.rope.insert(self.rope.byte_to_char(edit.offset), &edit.inserted);
        }
        self.revision += 1;
        self.break_pending = true;
        let caret = edit.caret_after;
        self.undo_bytes += edit.removed.len() + edit.inserted.len();
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
            && !edit.inserted.is_empty()
            && edit.removed.is_empty()
            && self.undo.back().is_some_and(|last| {
                last.removed.is_empty()
                    && last.offset + last.inserted.len() == edit.offset
                    && last.inserted.chars().next_back().is_some_and(|a| {
                        edit.inserted.chars().next().is_some_and(|b| {
                            word_class(a) == word_class(b) && word_class(a) != 2
                        })
                    })
            });
        if coalesce {
            let last = self.undo.back_mut().expect("checked above");
            self.undo_bytes -= last.inserted.len();
            last.inserted.push_str(&edit.inserted);
            last.caret_after = edit.caret_after;
            self.undo_bytes += last.inserted.len();
        } else {
            self.undo_bytes += edit.removed.len() + edit.inserted.len();
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
            *bytes -= old.removed.len() + old.inserted.len();
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