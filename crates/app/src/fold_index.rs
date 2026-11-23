//! Fold index and fold state for VS Code-style code folding in the Output view.
//!
//! [`FoldIndex`] is a struct-of-arrays description of every container that
//! opens a `{` or `[` on one line and closes it on a later line. It is built in
//! a single O(bytes) pass, off the UI thread: [`crate::line_index::prewarm_off_thread`]
//! calls [`prewarm`] next to the line-index build, so a formatted document's
//! fold index and line index land together. Small documents
//! (under [`LARGE_FOLD_BYTES`]) build lazily on first view, like the line
//! index; larger cold ones get a bounded [`FoldIndex::provisional`] placeholder
//! that never scans the document on the UI thread.
//!
//! Folding keeps a fixed-pitch bitset of folded *start* lines ([`FoldSet`]).
//! Turning that into the visual-row <-> document-line map is a sorted list of
//! disjoint visible runs plus prefix sums ([`FoldView`]): `virtual_rows` counts
//! visible lines, each visual row maps back to a document line in
//! O(log segments), and a fold/unfold is O(1) on the bitset plus one map
//! rebuild (only on a fold action, never per frame). Nested folds stay marked
//! in the bitset while the map counts only the outermost hidden ranges, so
//! unfolding a parent restores the children's own fold marks.
//!
//! The fold *state* lives here in a process-wide, revision-keyed store (interior
//! mutability) rather than in `AppState`: the view only has `&AppState`, but a
//! search-hit/Validate reveal must unfold a target line before
//! `Tx::scroll_into_view` fires. [`note_reveal`] records that target; the next
//! [`view_for`] (i.e. the next `TextView::build`) unfolds its ancestor folds.
//! [`note_hover`] records the row under the pointer so the keyboard fold
//! commands have a cursor-less target. Handlers bump `AppState::fold_epoch` to
//! invalidate the frame.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A container that opens on `start` and closes on `end` (both 0-based doc
/// lines, `end > start`). `child_count` is the number of direct children/items;
/// `depth` is the number of ancestor containers (0 = top level).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u32,
    pub end: u32,
    pub child_count: u32,
    pub depth: u32,
    pub is_array: bool,
}

/// Struct-of-arrays fold index, sorted by `starts`.
#[derive(Debug)]
pub struct FoldIndex {
    starts: Vec<u32>,
    ends: Vec<u32>,
    child_counts: Vec<u32>,
    depths: Vec<u32>,
    is_array: Vec<u8>,
    line_count: usize,
    complete: bool,
}

struct Frame {
    start: u32,
    is_array: bool,
    commas: u32,
    has_content: bool,
    depth: u32,
}

impl FoldIndex {
    /// One pass over the bytes: brace/string state per line, no allocation
    /// proportional to line length. Containers are recorded only on close, in
    /// LIFO order, then sorted by start line so [`FoldIndex::region_at`] can
    /// binary-search. Braces and commas inside strings are ignored; only
    /// multi-line containers are foldable.
    pub fn build(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut stack: Vec<Frame> = Vec::with_capacity(16);
        let mut regions: Vec<(u32, u32, u32, u32, u8)> = Vec::new();
        let mut line: u32 = 0;
        let mut in_string = false;
        let mut escaped = false;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            if in_string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = false;
                }
                i += 1;
                continue;
            }
            match b {
                b'\n' => {
                    line += 1;
                    i += 1;
                }
                b'"' => {
                    // A key or a string value begins a direct child.
                    if let Some(top) = stack.last_mut() {
                        top.has_content = true;
                    }
                    in_string = true;
                    i += 1;
                }
                b'{' | b'[' => {
                    // This opening is itself a child value of the parent.
                    if let Some(top) = stack.last_mut() {
                        top.has_content = true;
                    }
                    let depth = stack.len() as u32;
                    stack.push(Frame {
                        start: line,
                        is_array: b == b'[',
                        commas: 0,
                        has_content: false,
                        depth,
                    });
                    i += 1;
                }
                b'}' | b']' => {
                    if let Some(frame) = stack.pop() {
                        // Only multi-line containers are foldable. Children =
                        // top-level separators + 1 when the container has any.
                        if line > frame.start {
                            let children = if frame.has_content {
                                frame.commas + 1
                            } else {
                                0
                            };
                            regions.push((
                                frame.start,
                                line,
                                children,
                                frame.depth,
                                frame.is_array as u8,
                            ));
                        }
                    }
                    i += 1;
                }
                b',' => {
                    if let Some(top) = stack.last_mut() {
                        top.commas += 1;
                    }
                    i += 1;
                }
                b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {
                    // A scalar value begins a direct child.
                    if let Some(top) = stack.last_mut() {
                        top.has_content = true;
                    }
                    i += 1;
                }
                _ => i += 1, // whitespace, ':', number tails, etc.
            }
        }
        // A line can open more than one multi-line container (`{"a": [` on one
        // line). Folding is addressed by line, so keep the outermost region
        // (the smallest depth) for each start line; the others are unreachable
        // by a line-keyed fold and would make `region_at` ambiguous.
        regions.sort_unstable_by_key(|r| (r.0, r.3));
        let mut index = Self {
            starts: Vec::with_capacity(regions.len()),
            ends: Vec::with_capacity(regions.len()),
            child_counts: Vec::with_capacity(regions.len()),
            depths: Vec::with_capacity(regions.len()),
            is_array: Vec::with_capacity(regions.len()),
            line_count: line as usize + 1,
            complete: true,
        };
        let mut previous_start = u32::MAX;
        for (start, end, children, depth, arr) in regions {
            if start == previous_start {
                continue;
            }
            previous_start = start;
            index.starts.push(start);
            index.ends.push(end);
            index.child_counts.push(children);
            index.depths.push(depth);
            index.is_array.push(arr);
        }
        index
    }

    /// Cold placeholder: correct line count, no regions. Never O(document).
    pub fn provisional(line_count: usize) -> Self {
        Self {
            starts: Vec::new(),
            ends: Vec::new(),
            child_counts: Vec::new(),
            depths: Vec::new(),
            is_array: Vec::new(),
            line_count,
            complete: false,
        }
    }

    /// Whether this index scanned the whole document (false for a cold
    /// placeholder that [`prewarm`] will replace).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn line_count(&self) -> usize {
        self.line_count
    }

    /// Number of foldable containers.
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Region for a foldable start line (binary search).
    pub fn region_at(&self, start: u32) -> Option<Region> {
        let i = self.starts.binary_search(&start).ok()?;
        Some(self.region(i))
    }

    pub fn region(&self, i: usize) -> Region {
        Region {
            start: self.starts[i],
            end: self.ends[i],
            child_count: self.child_counts[i],
            depth: self.depths[i],
            is_array: self.is_array[i] != 0,
        }
    }

    /// Every foldable start line (sorted).
    pub fn starts(&self) -> &[u32] {
        &self.starts
    }

    /// Start lines at exactly `depth` (1-based level `depth + 1`).
    pub fn starts_at_depth(&self, depth: u32) -> Vec<u32> {
        self.starts
            .iter()
            .zip(self.depths.iter())
            .filter(|(_, d)| **d == depth)
            .map(|(s, _)| *s)
            .collect()
    }

    /// Start lines at `depth` or deeper, i.e. every region a "Fold to level
    /// N" (`depth = N - 1`) should collapse. Folding an outer region hides its
    /// children anyway, so this is equivalent to folding only `depth`, but it
    /// also keeps the deeper marks for when the user unfolds the parent.
    pub fn starts_at_or_below_depth(&self, depth: u32) -> Vec<u32> {
        self.starts
            .iter()
            .zip(self.depths.iter())
            .filter(|(_, d)| **d >= depth)
            .map(|(s, _)| *s)
            .collect()
    }

    /// Start lines of every foldable region enclosing `doc_line`, innermost
    /// first. Containers are properly nested, so this walks back from the last
    /// region that starts before the line, following decreasing depths: it
    /// stops at the outermost container instead of scanning every region.
    pub fn ancestors_of(&self, doc_line: u32) -> Vec<u32> {
        let i = self.starts.partition_point(|&s| s < doc_line);
        let mut out = Vec::new();
        let mut want = u32::MAX;
        for j in (0..i).rev() {
            if self.ends[j] < doc_line {
                continue;
            }
            let depth = self.depths[j];
            if want == u32::MAX || depth + 1 == want {
                out.push(self.starts[j]);
                if depth == 0 {
                    break;
                }
                want = depth;
            }
        }
        out
    }

    /// The region a fold command acts on for `doc_line`: the container opening
    /// on that exact line, else the innermost container enclosing it. `None`
    /// when the line is not inside any foldable container.
    pub fn target_for_line(&self, doc_line: u32) -> Option<Region> {
        if let Some(region) = self.region_at(doc_line) {
            return Some(region);
        }
        let first = *self.ancestors_of(doc_line).first()?;
        self.region_at(first)
    }
}

/// Fixed-pitch bitset of folded fold-start lines. O(1) fold/unfold/membership,
/// word-at-a-time iteration (2M lines = 250 KB and 31k word reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldSet {
    bits: Vec<u64>,
    len: usize,
}

impl FoldSet {
    pub fn new(line_count: usize) -> Self {
        Self {
            bits: vec![0; line_count.div_ceil(64)],
            len: line_count,
        }
    }

    fn word(line: usize) -> (usize, u64) {
        (line / 64, 1u64 << (line % 64))
    }

    pub fn set(&mut self, line: usize) {
        if line >= self.len {
            return;
        }
        let (w, m) = Self::word(line);
        self.bits[w] |= m;
    }

    pub fn clear(&mut self, line: usize) {
        if line >= self.len {
            return;
        }
        let (w, m) = Self::word(line);
        self.bits[w] &= !m;
    }

    pub fn get(&self, line: usize) -> bool {
        if line >= self.len {
            return false;
        }
        let (w, m) = Self::word(line);
        self.bits[w] & m != 0
    }

    pub fn toggle(&mut self, line: usize) {
        if line >= self.len {
            return;
        }
        let (w, m) = Self::word(line);
        self.bits[w] ^= m;
    }

    pub fn clear_all(&mut self) {
        self.bits.fill(0);
    }

    /// Replaces the whole set with `starts` (already sorted, no duplicates).
    pub fn set_all(&mut self, starts: &[u32]) {
        self.clear_all();
        for &s in starts {
            self.set(s as usize);
        }
    }

    /// Every folded start line, ascending. Skips zero words, so the cost is
    /// proportional to the number of folded regions, not the line count.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits.iter().enumerate().flat_map(|(w, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros();
                word &= word - 1;
                Some((w * 64) as u32 + bit)
            })
        })
    }

    pub fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|w| *w == 0)
    }
}

/// One visible run of document lines between hidden (folded) ranges.
#[derive(Clone, Copy, Debug)]
struct Segment {
    visual_start: u32,
    doc_start: u32,
    len: u32,
}

/// Immutable visual-row <-> document-line map, rebuilt only when folds change.
#[derive(Clone)]
pub struct FoldView {
    index: Arc<FoldIndex>,
    folded: Arc<FoldSet>,
    segments: Vec<Segment>,
    visible_count: usize,
    hidden_count: usize,
    epoch: u64,
}

impl FoldView {
    /// Builds the prefix-sum segment map from a fold index and folded set.
    /// Nested folds are collapsed into their outermost hidden range, so hidden
    /// ranges are disjoint and a fold/unfold never double-counts lines.
    pub fn new(index: Arc<FoldIndex>, folded: FoldSet, epoch: u64) -> Self {
        let line_count = index.line_count();
        let mut segments = Vec::new();
        let mut covered_until: i64 = -1;
        let mut hidden: u64 = 0;
        let mut doc_pos: u32 = 0;
        let mut vis_pos: u32 = 0;

        for s in folded.iter() {
            let s_i = s as i64;
            if s_i <= covered_until {
                continue; // nested inside an already-hidden range
            }
            let Some(region) = index.region_at(s) else {
                continue;
            };
            let a = (s + 1).min(line_count as u32); // first hidden doc line
            let b = (region.end + 1).min(line_count as u32); // exclusive
            if a >= b {
                continue;
            }
            if a > doc_pos {
                segments.push(Segment {
                    visual_start: vis_pos,
                    doc_start: doc_pos,
                    len: a - doc_pos,
                });
                vis_pos += a - doc_pos;
            }
            hidden += (b - a) as u64;
            doc_pos = b;
            covered_until = region.end as i64;
        }
        if (doc_pos as usize) < line_count {
            segments.push(Segment {
                visual_start: vis_pos,
                doc_start: doc_pos,
                len: line_count as u32 - doc_pos,
            });
            vis_pos += line_count as u32 - doc_pos;
        }
        Self {
            index,
            folded: Arc::new(folded),
            segments,
            visible_count: vis_pos as usize,
            hidden_count: hidden as usize,
            epoch,
        }
    }

    pub fn visible_count(&self) -> usize {
        self.visible_count
    }

    pub fn hidden_count(&self) -> usize {
        self.hidden_count
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn index(&self) -> &Arc<FoldIndex> {
        &self.index
    }

    pub fn is_folded(&self, doc_line: usize) -> bool {
        self.folded.get(doc_line)
    }

    pub fn is_foldable(&self, doc_line: usize) -> bool {
        self.index.region_at(doc_line as u32).is_some()
    }

    pub fn region(&self, doc_line: usize) -> Option<Region> {
        self.index.region_at(doc_line as u32)
    }

    /// Visual row (0-based) -> document line (0-based). O(log segments).
    pub fn visual_to_doc(&self, visual_row: usize) -> usize {
        if self.visible_count == 0 {
            return 0;
        }
        let v = visual_row.min(self.visible_count - 1) as u32;
        let i = match self.segments.binary_search_by_key(&v, |s| s.visual_start) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let seg = self.segments[i];
        (seg.doc_start + (v - seg.visual_start).min(seg.len.saturating_sub(1))) as usize
    }

    /// Document line -> visual row, or `None` when the line is hidden by a
    /// fold. Used by tests and to keep line-number mapping honest.
    pub fn doc_to_visual(&self, doc_line: usize) -> Option<usize> {
        let d = doc_line as u32;
        let i = match self.segments.binary_search_by(|s| s.doc_start.cmp(&d)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let seg = self.segments.get(i)?;
        if d < seg.doc_start || d >= seg.doc_start + seg.len {
            return None;
        }
        Some((seg.visual_start + (d - seg.doc_start)) as usize)
    }
}

// ── Process-wide store, keyed by the document fold key ────────────────────

struct State {
    index: Arc<FoldIndex>,
    folded: FoldSet,
    epoch: u64,
    view: Option<Arc<FoldView>>,
}

struct Store {
    states: HashMap<u64, State>,
    order: Vec<u64>,
    /// Pending `scroll_into_view` targets, one per view base key: the next
    /// build of that view unfolds the ancestors of the line before the reveal
    /// scrolls. Keyed by base so a pending click in one view never unfolds a
    /// line in another.
    reveal: HashMap<String, u32>,
    hover: Option<(u64, u32)>,
}

const MAX_STATES: usize = 32;
/// Above this, a cold fold index is a bounded placeholder (built off-thread by
/// [`prewarm`]) instead of an O(document) scan on the UI thread. Matches
/// [`crate::line_index::LARGE_DOCUMENT_BYTES`], so both indices warm together.
const LARGE_FOLD_BYTES: usize = crate::line_index::LARGE_DOCUMENT_BYTES;
static GLOBAL_EPOCH: AtomicU64 = AtomicU64::new(1);

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            states: HashMap::new(),
            order: Vec::new(),
            reveal: HashMap::new(),
            hover: None,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|p| p.into_inner())
}

fn bump_epoch() -> u64 {
    GLOBAL_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Stable fold key for a one-off view with no reducer revision: buffer address,
/// length and a head/tail sample, so a freed-and-reused allocation does not
/// silently inherit another document's folds.
pub fn key_for_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    (text.as_ptr() as usize, text.len()).hash(&mut h);
    let bytes = text.as_bytes();
    let head = &bytes[..bytes.len().min(64)];
    let tail = &bytes[bytes.len().saturating_sub(64)..];
    (head, tail).hash(&mut h);
    h.finish() | 1 // never 0, distinct from the "no revision" sentinel
}

/// The fold-store key shared by the view, the toolbar and the keyboard
/// handlers: a reducer-assigned revision when present, otherwise a stable
/// fingerprint of the buffer.
pub fn key(revision: u64, text: &str) -> u64 {
    if revision != 0 {
        revision
    } else {
        key_for_text(text)
    }
}

/// `line_count` comes from the caller's cached line index, so a cold large
/// document costs O(1) here (no O(document) newline scan on the UI thread).
fn ensure_state_locked<'a>(
    store: &'a mut Store,
    text: &str,
    key: u64,
    line_count: usize,
) -> &'a mut State {
    let mut inserted = false;
    match store.states.entry(key) {
        std::collections::hash_map::Entry::Occupied(_) => {}
        std::collections::hash_map::Entry::Vacant(slot) => {
            let index = if text.len() >= LARGE_FOLD_BYTES {
                Arc::new(FoldIndex::provisional(line_count))
            } else {
                Arc::new(FoldIndex::build(text))
            };
            let folded = FoldSet::new(index.line_count());
            slot.insert(State {
                index,
                folded,
                epoch: bump_epoch(),
                view: None,
            });
            inserted = true;
        }
    }
    if inserted {
        store.order.push(key);
        prune(store);
    }
    store.states.get_mut(&key).expect("just inserted")
}

fn prune(store: &mut Store) {
    while store.states.len() > MAX_STATES {
        let Some(old) = store.order.first().copied() else {
            break;
        };
        store.order.remove(0);
        store.states.remove(&old);
    }
}

/// Builds the full fold index off the UI thread and publishes it under `key`.
/// Called by [`crate::line_index::prewarm_off_thread`] next to the line-index
/// build, so the Output view finds both already warmed.
pub fn prewarm(text: Arc<str>, key: u64) -> Arc<FoldIndex> {
    let index = Arc::new(FoldIndex::build(&text));
    let mut g = lock();
    if let Some(state) = g.states.get_mut(&key) {
        if state.index.is_complete() {
            // Already warmed (two jobs for the same revision): keep the
            // existing index *and* the user's folds.
            return Arc::clone(&state.index);
        }
        // Keep any folds the user already made; the revision/document is the
        // same, so line numbers still line up. Invalidate the cached map.
        if state.folded.len() != index.line_count() {
            state.folded = FoldSet::new(index.line_count());
        }
        state.index = Arc::clone(&index);
        state.epoch = bump_epoch();
        state.view = None;
    } else {
        let folded = FoldSet::new(index.line_count());
        g.states.insert(
            key,
            State {
                index: Arc::clone(&index),
                folded,
                epoch: bump_epoch(),
                view: None,
            },
        );
        g.order.push(key);
        prune(&mut g);
    }
    index
}

/// Ensures a fold index exists for `key` without doing O(document) work on the
/// UI thread for a large cold document, and returns the current fold view.
/// Applies any pending reveal (unfolding its ancestor folds) first.
pub fn view_for(text: &str, key: u64, line_count: usize, view_key: &str) -> Arc<FoldView> {
    let mut g = lock();
    if let Some(line) = g.reveal.remove(view_key) {
        let state = ensure_state_locked(&mut g, text, key, line_count);
        let mut changed = false;
        for s in state.index.ancestors_of(line) {
            if state.folded.get(s as usize) {
                state.folded.clear(s as usize);
                changed = true;
            }
        }
        if changed {
            state.epoch = bump_epoch();
            state.view = None;
        }
    }
    let state = ensure_state_locked(&mut g, text, key, line_count);
    if state.view.is_none() {
        let view = FoldView::new(
            Arc::clone(&state.index),
            state.folded.clone(),
            state.epoch,
        );
        state.view = Some(Arc::new(view));
    }
    Arc::clone(state.view.as_ref().expect("view just built"))
}

/// Creates the fold state for `key` if a caller acts before the view ran
/// (toolbar/keyboard on a document that was never rendered). Small documents
/// build eagerly; a large cold document is left to [`prewarm`], so this never
/// scans O(document) bytes on the UI thread.
pub fn ensure_for(text: &str, key: u64) {
    if text.len() >= LARGE_FOLD_BYTES {
        return;
    }
    let mut g = lock();
    let mut inserted = false;
    match g.states.entry(key) {
        std::collections::hash_map::Entry::Occupied(_) => {}
        std::collections::hash_map::Entry::Vacant(slot) => {
            let index = Arc::new(FoldIndex::build(text));
            let folded = FoldSet::new(index.line_count());
            slot.insert(State {
                index,
                folded,
                epoch: bump_epoch(),
                view: None,
            });
            inserted = true;
        }
    }
    if inserted {
        g.order.push(key);
        prune(&mut g);
    }
}

/// Records a document line that must become visible before a
/// `scroll_into_view` reveal (search hit / Validate error). The next
/// [`view_for`] unfolds all ancestor folds covering it.
pub fn note_reveal(view_key: &str, doc_line: usize) {
    let mut g = lock();
    if g.reveal.len() >= 8 {
        g.reveal.clear();
    }
    g.reveal.insert(view_key.to_string(), doc_line as u32);
}

/// Records the document row under the pointer, the target of the keyboard
/// fold/unfold commands (there is no text cursor in the read-only view). The
/// fold key travels with it, so a hover in another tab never becomes the
/// target of this document.
pub fn note_hover(key: u64, doc_line: usize) {
    lock().hover = Some((key, doc_line as u32));
}

/// Last hovered document line for `key`, if any.
pub fn hovered(key: u64) -> Option<usize> {
    lock()
        .hover
        .filter(|(hover_key, _)| *hover_key == key)
        .map(|(_, line)| line as usize)
}

fn mutate(key: u64, f: impl FnOnce(&mut State)) -> bool {
    let mut g = lock();
    if let Some(state) = g.states.get_mut(&key) {
        f(state);
        state.epoch = bump_epoch();
        state.view = None;
        true
    } else {
        false
    }
}

/// Toggles one foldable start line.
pub fn toggle(key: u64, start: u32) -> bool {
    let mut toggled = false;
    mutate(key, |state| {
        if state.index.region_at(start).is_some() {
            state.folded.toggle(start as usize);
            toggled = true;
        }
    });
    toggled
}

/// Toggles the region a keyboard fold command acts on: the container opening
/// on `doc_line`, else the innermost one enclosing it. Returns the region and
/// whether it is now folded.
pub fn toggle_near(key: u64, doc_line: u32) -> Option<(Region, bool)> {
    let mut g = lock();
    let state = g.states.get_mut(&key)?;
    let region = state.index.target_for_line(doc_line)?;
    state.folded.toggle(region.start as usize);
    let now = state.folded.get(region.start as usize);
    state.epoch = bump_epoch();
    state.view = None;
    Some((region, now))
}

/// Unfolds the region enclosing `doc_line` (or the one opening on it) if it is
/// folded. Returns the region that was opened. Unlike [`toggle_near`] this
/// never folds an open region, so "unfold at the pointer" is a no-op when
/// nothing is collapsed there.
pub fn unfold_near(key: u64, doc_line: u32) -> Option<Region> {
    let mut g = lock();
    let state = g.states.get_mut(&key)?;
    let mut candidates = Vec::with_capacity(4);
    if state.index.region_at(doc_line).is_some() {
        candidates.push(doc_line);
    }
    candidates.extend(state.index.ancestors_of(doc_line));
    for start in candidates {
        if state.folded.get(start as usize) {
            state.folded.clear(start as usize);
            let region = state.index.region_at(start)?;
            state.epoch = bump_epoch();
            state.view = None;
            return Some(region);
        }
    }
    None
}

/// Folds every foldable container (the map collapses nested folds to the
/// outermost ranges). Returns the number of fold marks set.
pub fn fold_all(key: u64) -> usize {
    let mut count = 0;
    mutate(key, |state| {
        state.folded.set_all(state.index.starts());
        count = state.folded.count();
    });
    count
}

/// Clears every fold mark. Returns how many were cleared.
pub fn unfold_all(key: u64) -> usize {
    let mut count = 0;
    mutate(key, |state| {
        count = state.folded.count();
        state.folded.clear_all();
    });
    count
}

/// "Fold to level N" (1..=3): collapse every container at nesting depth
/// `N - 1` or deeper, leaving shallower levels open. Returns the number of
/// fold marks set.
pub fn fold_level(key: u64, level: u32) -> usize {
    let mut count = 0;
    mutate(key, |state| {
        let starts = state
            .index
            .starts_at_or_below_depth(level.saturating_sub(1));
        state.folded.set_all(&starts);
        count = state.folded.count();
    });
    count
}

/// Number of fold marks for `key` (0 when unknown).
pub fn folded_count(key: u64) -> usize {
    lock()
        .states
        .get(&key)
        .map_or(0, |state| state.folded.count())
}

/// Every folded start line for `key`, ascending.
pub fn folded_starts(key: u64) -> Vec<u32> {
    lock()
        .states
        .get(&key)
        .map(|state| state.folded.iter().collect())
        .unwrap_or_default()
}

/// Test/diagnostic helper: drops every fold state, reveal and hover.
pub fn clear() {
    let mut g = lock();
    g.states.clear();
    g.order.clear();
    g.reveal.clear();
    g.hover = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(text: &str) -> Arc<FoldIndex> {
        Arc::new(FoldIndex::build(text))
    }

    fn view_count_lines(text: &str) -> usize {
        text.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1
    }

    #[test]
    fn builds_nested_regions_with_child_counts() {
        let text = "{\n  \"a\": {\n    \"x\": 1,\n    \"y\": 2\n  },\n  \"b\": [1, 2, 3],\n  \"c\": true\n}\n";
        let index = idx(text);
        // Root object at line 0 (depth 0), nested "a" at line 1 (depth 1).
        let root = index.region_at(0).expect("root fold");
        assert_eq!(root.depth, 0);
        assert_eq!(root.end, 7);
        assert_eq!(root.child_count, 3, "a, b, c");
        let a = index.region_at(1).expect("nested object fold");
        assert_eq!(a.depth, 1);
        assert_eq!(a.end, 4);
        assert_eq!(a.child_count, 2, "x, y");
    }

    #[test]
    fn single_line_containers_are_not_foldable() {
        let index = idx("{\"a\": {\"x\": 1}, \"b\": [1, 2]}\n");
        assert!(index.is_empty());
    }

    #[test]
    fn containers_opening_on_one_line_collapse_to_the_outermost() {
        let text = "{\"a\": [\n  1,\n  2\n],\n \"b\": {\n  \"x\": 1\n}}\n";
        let index = idx(text);
        // Root and array both open on line 0; only the root is addressable.
        assert_eq!(index.len(), 2);
        assert_eq!(index.region_at(0).unwrap().end, 6);
        assert_eq!(index.region_at(0).unwrap().depth, 0);
        assert_eq!(index.region_at(4).unwrap().end, 6);
    }

    #[test]
    fn commas_and_braces_in_strings_are_ignored() {
        let text = "{\n  \"s\": \"a,b{c}d\",\n  \"n\": 2\n}\n";
        let index = idx(text);
        assert_eq!(index.region_at(0).unwrap().child_count, 2);
        assert_eq!(index.len(), 1, "string braces must not open containers");
    }

    #[test]
    fn child_counts_cover_arrays_and_empty_containers() {
        let text = "{\n  \"list\": [\n    1,\n    2,\n    3\n  ],\n  \"empty\": {\n  }\n}\n";
        let index = idx(text);
        let list = index.region_at(1).expect("array fold");
        assert!(list.is_array);
        assert_eq!(list.child_count, 3);
        let root = index.region_at(0).unwrap();
        assert_eq!(root.child_count, 2);
        // The empty object spans two lines, but has no children.
        let empty = index.region_at(6).expect("empty object fold");
        assert_eq!(empty.child_count, 0);
    }

    #[test]
    fn ancestors_of_a_line_are_the_enclosing_folds() {
        let text = "{\n  \"a\": {\n    \"x\": 1\n  }\n}\n";
        let index = idx(text);
        // line 2 ("x": 1) is inside root(0) and nested(1).
        assert_eq!(index.ancestors_of(2), vec![1, 0]);
        assert_eq!(index.target_for_line(2).unwrap().start, 1);
        assert_eq!(index.target_for_line(1).unwrap().start, 1);
        assert!(index.ancestors_of(0).is_empty());
    }

    #[test]
    fn ancestors_of_a_late_line_stop_at_the_outermost_container() {
        // A flat top-level object with many members, last one nested.
        let mut text = String::from("{\n");
        for i in 0..500 {
            text.push_str(&format!("  \"k{i}\": {i},\n"));
        }
        text.push_str("  \"nested\": {\n    \"deep\": 1\n  }\n}\n");
        let index = idx(&text);
        let line = (text[..text.find("\"deep\"").unwrap()].matches('\n').count()) as u32;
        assert_eq!(index.ancestors_of(line), vec![501, 0]);
    }

    #[test]
    fn visual_rows_map_to_document_lines_skipping_folded() {
        let text = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
        let index = idx(text);
        let mut folded = FoldSet::new(index.line_count());
        folded.set(0); // fold the whole root object, hides lines 1..=4
        let view = FoldView::new(index, folded, 1);
        assert_eq!(view.visible_count(), 2); // line 0 (placeholder) + line 5 (empty)
        assert_eq!(view.visual_to_doc(0), 0);
        assert_eq!(view.visual_to_doc(1), 5);
        assert_eq!(view.hidden_count(), 4);
        assert_eq!(view.doc_to_visual(0), Some(0));
        assert_eq!(view.doc_to_visual(2), None, "line 2 is hidden");
        assert_eq!(view.doc_to_visual(5), Some(1));
    }

    #[test]
    fn nested_folds_are_collapsed_to_outermost_ranges() {
        let text = "{\n  \"a\": {\n    \"x\": 1\n  },\n  \"b\": 2\n}\n";
        let index = idx(text);
        let mut folded = FoldSet::new(index.line_count());
        folded.set(0);
        folded.set(1);
        folded.set(2);
        let view = FoldView::new(index, folded, 1);
        // Folding root hides 1..=5; the nested folds are inside that range.
        assert_eq!(view.visible_count(), 2);
        assert_eq!(view.hidden_count(), 5);
        assert!(view.is_folded(1), "child fold mark is preserved");
    }

    #[test]
    fn fold_set_iterates_set_bits_in_order() {
        let mut set = FoldSet::new(1000);
        set.set(3);
        set.set(64);
        set.set(999);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![3, 64, 999]);
        assert_eq!(set.count(), 3);
        set.toggle(64);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![3, 999]);
    }

    #[test]
    fn fold_level_collapses_from_that_depth_down() {
        let text = "{\n  \"a\": {\n    \"b\": {\n      \"x\": 1\n    }\n  }\n}\n";
        let index = idx(text);
        // depths: root 0, a 1, b 2.
        assert_eq!(index.starts_at_or_below_depth(0), vec![0, 1, 2]);
        assert_eq!(index.starts_at_or_below_depth(1), vec![1, 2]);
        assert_eq!(index.starts_at_or_below_depth(2), vec![2]);
    }

    #[test]
    fn unfold_on_reveal_clears_enclosing_folds() {
        let text = "{\n  \"a\": {\n    \"x\": 1\n  },\n  \"b\": 2\n}\n";
        let key = 999_001;
        let owned: Arc<str> = Arc::from(text);
        prewarm(Arc::clone(&owned), key);
        fold_all(key);
        assert!(folded_count(key) > 0);
        note_reveal("test-1", 2); // a line hidden inside the root fold
        let view = view_for(&owned, key, view_count_lines(&owned), "test-1");
        assert!(
            view.doc_to_visual(2).is_some(),
            "revealed line now visible"
        );
    }

    #[test]
    fn reveal_inside_a_deep_fold_unfolds_every_ancestor() {
        let text = "{\n  \"a\": {\n    \"b\": {\n      \"x\": 1\n    }\n  }\n}\n";
        let key = 999_002;
        let owned: Arc<str> = Arc::from(text);
        prewarm(Arc::clone(&owned), key);
        fold_all(key);
        note_reveal("test-2", 3);
        let view = view_for(&owned, key, view_count_lines(&owned), "test-2");
        assert_eq!(view.doc_to_visual(3), Some(3), "all three folds opened");
        assert_eq!(folded_count(key), 0);
    }

    #[test]
    fn toggle_near_targets_the_container_around_the_line() {
        let text = "{\n  \"a\": {\n    \"x\": 1\n  },\n  \"b\": 2\n}\n";
        let key = 999_003;
        let owned: Arc<str> = Arc::from(text);
        prewarm(Arc::clone(&owned), key);
        let (region, folded) = toggle_near(key, 2).expect("target");
        assert_eq!(region.start, 1);
        assert!(folded);
        assert_eq!(folded_starts(key), vec![1]);
        let (_, folded) = toggle_near(key, 2).unwrap();
        assert!(!folded);
        assert_eq!(folded_starts(key), Vec::<u32>::new());
    }

    #[test]
    fn small_documents_build_eagerly_and_provisional_is_bounded() {
        let key = 999_004;
        let owned: Arc<str> = Arc::from("{\n  \"a\": 1\n}\n");
        let view = view_for(&owned, key, 4, "test");
        assert_eq!(view.index().line_count(), 4);
        assert!(view.index().is_complete(), "small docs build eagerly");
        // A cold placeholder reports the caller's line count and no regions.
        let placeholder = FoldIndex::provisional(42);
        assert!(!placeholder.is_complete());
        assert_eq!(placeholder.line_count(), 42);
        assert!(placeholder.is_empty());
    }
}
