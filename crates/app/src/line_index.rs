//! Byte-offset line index for large JSON documents.
//!
//! `LineIndex` records the byte offset of every line start exactly once per
//! document. `TextView` then renders a window of rows without ever scanning,
//! splitting or shaping the rest of the text: a 100 MB / 2M-line file costs one
//! index build, then O(visible rows) per frame.
//!
//! [`snapshot`] memoizes the index by a cheap content fingerprint, so repeated
//! `TextView::build` calls across frames never rebuild it. Large documents
//! should be indexed off the UI thread with [`prewarm_off_thread`] (call it
//! from a background job, e.g. right after a JSON format/minify result arrives,
//! and join the handle before switching the view to the output). If a caller
//! does not prewarm, the first `snapshot` builds the index once, synchronously;
//! every later frame hits the cache.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Documents at least this big should be indexed off the UI thread.
pub const LARGE_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

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
    len: usize,
}

impl LineIndex {
    /// One linear scan for `\n`. No allocation proportional to line length.
    pub fn build(text: &str) -> Self {
        let bytes = text.as_bytes();
        let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
        let mut starts = Vec::with_capacity(newlines + 1);
        starts.push(0);
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                starts.push(i + 1);
            }
            i += 1;
        }
        Self {
            starts,
            len: bytes.len(),
        }
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

/// Immutable text + index, cheap to clone into Rune's row builders.
pub struct Snapshot {
    text: Arc<str>,
    index: Arc<LineIndex>,
    key: TextKey,
}

impl Snapshot {
    pub fn new(text: Arc<str>) -> Self {
        let key = TextKey::of(&text);
        let index = Arc::new(LineIndex::build(&text));
        Self { text, index, key }
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
    guard.bytes += snapshot.bytes();
    guard.entries.insert(key, Arc::clone(&snapshot));
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

/// Returns the shared snapshot for `text`, building the index at most once per
/// distinct document. Cached lookups are O(1) (fingerprint + hash).
pub fn snapshot(text: &str) -> Arc<Snapshot> {
    let key = TextKey::of(text);
    if let Some(hit) = lock().entries.get(&key).cloned() {
        return hit;
    }
    insert(Arc::new(Snapshot::new(Arc::from(text))))
}

/// Lookup without ever building. Useful for a view that wants to show a banner
/// while a background prewarm is still running.
pub fn cached_snapshot(text: &str) -> Option<Arc<Snapshot>> {
    lock().entries.get(&TextKey::of(text)).cloned()
}

/// Builds and caches a snapshot on a worker thread. Join the handle before
/// rendering the document to keep the index build off the UI thread.
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
}
