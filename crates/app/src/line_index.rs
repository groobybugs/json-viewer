//! Byte-offset line index for large JSON documents.
//!
//! `LineIndex` records the byte offset of every line start exactly once per
//! document. `TextView` then renders a window of rows without ever scanning,
//! splitting or shaping the rest of the text: a 100 MB / 2M-line file costs one
//! index build, then O(visible rows) per frame.
//!
//! [`snapshot`] memoizes the index by a cheap content fingerprint, so repeated
//! `TextView::build` calls across frames never rebuild it. A cold miss on a
//! large document does **no O(document) work on the UI thread**: it copies and
//! scans only [`PARTIAL_SCAN_BYTES`] (enough to mount the first viewport) and
//! returns a provisional snapshot. The caller's background job then calls
//! [`prewarm_off_thread`] with the owned text to build the full index off the
//! UI thread and replace the provisional entry. Small documents are indexed
//! synchronously (the cost is bounded by [`LARGE_DOCUMENT_BYTES`]).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Documents at least this big should be indexed off the UI thread.
pub const LARGE_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
/// How much of a cold large document the UI thread copies/scans for its first
/// frame. Bounded, so the first render is O(window), not O(document).
pub const PARTIAL_SCAN_BYTES: usize = 64 * 1024;

/// Cache bounds: a handful of live snapshots, at most 256 MB of source text.
const MAX_CACHED_DOCS: usize = 8;
const MAX_CACHED_BYTES: usize = 256 * 1024 * 1024;

/// O(1) content identity. Two clones of the same document at different
/// addresses share one index; a reused address with different content does not
/// (the 32-byte head/tail samples catch that).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextKey {
    len: usize,
    head: [u8; 32],
    tail: [u8; 32],
}

impl TextKey {
    pub fn of(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut head = [0u8; 32];
        let mut tail = [0u8; 32];
        let h = bytes.len().min(32);
        head[..h].copy_from_slice(&bytes[..h]);
        let t = bytes.len().min(32);
        tail[..t].copy_from_slice(&bytes[bytes.len() - t..]);
        Self {
            len: bytes.len(),
            head,
            tail,
        }
    }

    /// Stable version for `El::virtual_rows_version`.
    pub fn version(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// Byte offsets of every line start. Empty text counts as one empty line,
/// matching `str::split('\n').count()`.
#[derive(Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
    /// Bytes actually scanned. For a provisional index this is the prefix len.
    len: usize,
    /// Length of the whole document (may exceed `len` while provisional).
    total_len: usize,
    complete: bool,
}

/// Snaps `index` down to a UTF-8 boundary without scanning the text.
fn floor_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl LineIndex {
    /// One vectorizable pass for `\n` (`slice::iter().position` is lowered to a
    /// SIMD scan), with no separate counting pass and no `push` in the hot
    /// loop. No allocation proportional to line length.
    pub fn build(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut starts = Vec::with_capacity(64);
        starts.push(0);
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i..].iter().position(|&b| b == b'\n') {
                Some(offset) => {
                    i += offset + 1;
                    starts.push(i);
                }
                None => break,
            }
        }
        Self {
            starts,
            len: bytes.len(),
            total_len: bytes.len(),
            complete: true,
        }
    }

    /// A provisional index over `text` (a bounded prefix), remembering the
    /// whole document's length. O(prefix), never O(document).
    pub fn partial(text: &str, total_len: usize) -> Self {
        let mut index = Self::build(text);
        index.total_len = total_len;
        index.complete = false;
        index
    }

    /// Whether the whole document was scanned.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Length of the whole document, even when only a prefix was scanned.
    pub fn total_len(&self) -> usize {
        self.total_len
    }

    /// Number of lines (at least one).
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Only an empty document has no lines.
    pub fn is_empty(&self) -> bool {
        self.starts.len() == 1 && self.len == 0
    }

    /// Byte offset of line `line`'s first byte.
    pub fn line_start(&self, line: usize) -> Option<usize> {
        self.starts.get(line).copied()
    }

    /// Byte offset just past line `line`'s last byte (before `\n`/`\r\n`).
    pub fn line_end(&self, line: usize) -> Option<usize> {
        let start = *self.starts.get(line)?;
        if start > self.len {
            return None;
        }
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.len, |next| next.saturating_sub(1));
        Some(end.min(self.len))
    }

    /// The line's text, without its line ending. O(1) (two lookups + a slice).
    pub fn line<'a>(&self, text: &'a str, line: usize) -> Option<&'a str> {
        let start = *self.starts.get(line)?;
        let end = self.line_end(line)?;
        let mut slice = text.get(start.min(text.len())..end.min(text.len()))?;
        if slice.ends_with('\r') {
            slice = &slice[..slice.len() - 1];
        }
        Some(slice)
    }

    /// Line containing byte offset `byte` (binary search).
    pub fn line_of_byte(&self, byte: usize) -> usize {
        match self.starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }
}

/// Immutable text + index, cheap to clone into Rune's row builders. A
/// provisional snapshot holds only a bounded prefix (see
/// [`PARTIAL_SCAN_BYTES`]); `prewarm_off_thread` replaces it with a full one.
pub struct Snapshot {
    text: Arc<str>,
    index: Arc<LineIndex>,
    key: TextKey,
    partial: bool,
}

impl Snapshot {
    /// Full snapshot: owned text + complete index (off the UI thread for large
    /// documents, via `prewarm_off_thread`).
    pub fn new(text: Arc<str>) -> Self {
        let key = TextKey::of(&text);
        let index = Arc::new(LineIndex::build(&text));
        Self {
            text,
            index,
            key,
            partial: false,
        }
    }

    /// Provisional snapshot: copies and scans only the first
    /// `PARTIAL_SCAN_BYTES` of `text`, so a cold first render is O(window).
    pub fn provisional(text: &str) -> Self {
        let key = TextKey::of(text);
        let end = floor_boundary(text, PARTIAL_SCAN_BYTES.min(text.len()));
        let prefix: Arc<str> = Arc::from(&text[..end]);
        let index = Arc::new(LineIndex::partial(&prefix, text.len()));
        Self {
            text: prefix,
            index,
            key,
            partial: true,
        }
    }

    /// Whether this snapshot covers only a bounded prefix.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn index(&self) -> &LineIndex {
        &self.index
    }

    pub fn line_count(&self) -> usize {
        self.index.len()
    }

    pub fn line(&self, line: usize) -> Option<&str> {
        self.index.line(&self.text, line)
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.line(line).map_or(0, str::len)
    }

    pub fn key(&self) -> TextKey {
        self.key
    }

    /// Version handed to `El::virtual_rows_version`: rows are re-keyed only
    /// when the document itself changes, so a 2M-row list commits in time
    /// independent of its length.
    pub fn version(&self) -> u64 {
        self.key.version()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.text.len()
    }
}

struct Cache {
    entries: HashMap<TextKey, Arc<Snapshot>>,
    order: VecDeque<TextKey>,
    bytes: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Cache> {
    cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn insert(snapshot: Arc<Snapshot>) -> Arc<Snapshot> {
    let mut guard = lock();
    let key = snapshot.key();
    // A full snapshot replaces a provisional one built on the UI thread.
    if let Some(old) = guard.entries.insert(key, Arc::clone(&snapshot)) {
        guard.bytes = guard.bytes.saturating_sub(old.bytes());
        guard.order.retain(|k| *k != key);
    }
    guard.bytes += snapshot.bytes();
    guard.order.push_back(key);
    while (guard.entries.len() > MAX_CACHED_DOCS || guard.bytes > MAX_CACHED_BYTES)
        && guard.order.len() > 1
    {
        let Some(old) = guard.order.pop_front() else { break };
        if old == key {
            // Never evict the document we just added.
            continue;
        }
        if let Some(removed) = guard.entries.remove(&old) {
            guard.bytes = guard.bytes.saturating_sub(removed.bytes());
        }
    }
    snapshot
}

/// Returns the shared snapshot for `text`.
///
/// Cached lookups are O(1). A cold miss on a large document is **O(window)**:
/// a bounded prefix is copied and scanned, and a background job is expected to
/// replace it with a full index through [`prewarm_off_thread`]. A cold miss on
/// a small document is indexed synchronously (bounded by
/// [`LARGE_DOCUMENT_BYTES`]).
pub fn snapshot(text: &str) -> Arc<Snapshot> {
    let key = TextKey::of(text);
    if let Some(hit) = lock().entries.get(&key).cloned() {
        return hit;
    }
    let built = if text.len() >= LARGE_DOCUMENT_BYTES {
        Snapshot::provisional(text)
    } else {
        Snapshot::new(Arc::from(text))
    };
    insert(Arc::new(built))
}

/// Lookup without ever building. Useful for a view that wants to show a banner
/// while a background prewarm is still running.
pub fn cached_snapshot(text: &str) -> Option<Arc<Snapshot>> {
    lock().entries.get(&TextKey::of(text)).cloned()
}

/// Builds the full index and caches it on a worker thread. The caller already
/// owns `text` (the JSON worker's result), so no copy or scan happens on the
/// UI thread. Join the handle before rendering to keep the first frame O(1).
pub fn prewarm_off_thread(text: Arc<str>) -> JoinHandle<Arc<Snapshot>> {
    std::thread::spawn(move || insert(Arc::new(Snapshot::new(text))))
}

/// Test helper: number of live snapshots.
pub fn cache_entries() -> usize {
    lock().entries.len()
}

/// Test helper: drops all cached snapshots.
pub fn clear_cache() {
    let mut guard = lock();
    guard.entries.clear();
    guard.order.clear();
    guard.bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_lines() {
        let text = "a\nb\nc";
        let index = LineIndex::build(text);
        assert_eq!(index.len(), 3);
        assert_eq!(index.line_start(0), Some(0));
        assert_eq!(index.line_start(2), Some(4));
        assert_eq!(index.line_start(3), None);
        assert_eq!(index.line(text, 0), Some("a"));
        assert_eq!(index.line(text, 1), Some("b"));
        assert_eq!(index.line(text, 2), Some("c"));
        assert_eq!(index.line(text, 3), None);
    }

    #[test]
    fn trailing_newline_is_an_empty_last_line() {
        let index = LineIndex::build("a\n");
        assert_eq!(index.len(), 2);
        assert_eq!(index.line("a\n", 1), Some(""));
    }

    #[test]
    fn crlf_is_stripped() {
        let index = LineIndex::build("a\r\nb\r\n");
        assert_eq!(index.len(), 3);
        assert_eq!(index.line("a\r\nb\r\n", 0), Some("a"));
        assert_eq!(index.line("a\r\nb\r\n", 1), Some("b"));
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        let index = LineIndex::build("");
        assert_eq!(index.len(), 1);
        assert!(index.is_empty());
        assert_eq!(index.line("", 0), Some(""));
    }

    #[test]
    fn byte_to_line_is_a_binary_search() {
        let index = LineIndex::build("one\ntwo\nthree");
        assert_eq!(index.line_of_byte(0), 0);
        assert_eq!(index.line_of_byte(3), 0);
        assert_eq!(index.line_of_byte(4), 1);
        assert_eq!(index.line_of_byte(8), 2);
        assert_eq!(index.line_of_byte(999), 2);
    }

    #[test]
    fn snapshot_is_memoized_by_content_not_address() {
        // Distinct content keeps this test independent of the parallel cache.
        let owned = "memo-unique\nb\nc".to_string();
        let first = snapshot(&owned);
        let clone = owned.clone();
        let second = snapshot(&clone);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.line_count(), 3);
        assert_eq!(first.line(1), Some("b"));
    }

    #[test]
    fn prewarm_publishes_the_same_snapshot() {
        let text: Arc<str> = Arc::from("prewarm-unique\nb\nc");
        let handle = prewarm_off_thread(Arc::clone(&text));
        let warmed = handle.join().expect("prewarm thread panicked");
        assert!(Arc::ptr_eq(&warmed, &snapshot(&text)));
        assert_eq!(warmed.line_count(), 3);
    }

    #[test]
    fn large_cold_miss_is_a_bounded_provisional_snapshot() {
        let text = "x".repeat(LARGE_DOCUMENT_BYTES + 1024);
        let snap = snapshot(&text);
        assert!(snap.is_partial());
        assert_eq!(snap.text().len(), PARTIAL_SCAN_BYTES);
        assert_eq!(snap.line_count(), 1);
        assert_eq!(snap.line(0).map(str::len), Some(PARTIAL_SCAN_BYTES));
        assert_eq!(snap.index().total_len(), text.len());
        assert!(!snap.index().is_complete());
    }

    #[test]
    fn prewarm_replaces_the_provisional_snapshot() {
        let text: Arc<str> = Arc::from("y".repeat(LARGE_DOCUMENT_BYTES + 10));
        let cold = snapshot(&text);
        assert!(cold.is_partial());
        let warmed = prewarm_off_thread(Arc::clone(&text))
            .join()
            .expect("prewarm thread panicked");
        assert!(!warmed.is_partial());
        assert_eq!(warmed.line_count(), 1);
        assert_eq!(warmed.index().total_len(), text.len());
        assert!(Arc::ptr_eq(&warmed, &snapshot(&text)));
    }
}
