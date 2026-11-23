//! Exact JSON lexer state for column windows (rp-editor follow-up).
//!
//! A column window of a very long line must be colored exactly as if the
//! whole line were lexed: whether the window starts inside a string (and
//! whether its first byte is escaped) decides every token color in it. The
//! old bounded quote-parity lookback guessed that state and inverted
//! string/number colors on minified documents.
//!
//! Instead, each long line keeps lexer **checkpoints** every
//! [`CHECKPOINT_STRIDE`] bytes (the `LexState` at that byte), built lazily —
//! off the UI thread above [`OFF_THREAD_BYTES`] — and invalidated through
//! the buffer's edit log: an entry survives edits that do not touch its
//! line range. A window lexes from the nearest checkpoint at or before its
//! start, so the scan is at most one stride (a simple byte scan over 64 KB
//! is tens of microseconds). While a big line's checkpoints are building,
//! the view falls back to an exact scan from the line start.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Mutex;

use ropey::Rope;
use rune::Color;

use super::buffer::Buffer;

/// Bytes between lexer checkpoints on one line.
pub const CHECKPOINT_STRIDE: usize = 64 * 1024;
/// Lines longer than this build their checkpoints on a worker thread.
const OFF_THREAD_BYTES: usize = 256 * 1024;

/// JSON lexer state at a byte position: inside a string, and whether the
/// next byte is escaped by a preceding backslash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LexState {
    pub in_string: bool,
    pub escaped: bool,
}

impl LexState {
    /// Advances over `bytes`; only `"` and `\` change the state.
    pub fn advance(mut self, bytes: &[u8]) -> Self {
        let mut i = 0;
        while i < bytes.len() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                    i += 1;
                    continue;
                }
                // Inside a string only quotes and backslashes matter: skip
                // straight to the next one.
                match find2(&bytes[i..], b'"', b'\\') {
                    Some(pos) => {
                        let b = bytes[i + pos];
                        if b == b'\\' {
                            self.escaped = true;
                        } else {
                            self.in_string = false;
                        }
                        i += pos + 1;
                    }
                    None => {
                        // Rest of the chunk is plain string content (no
                        // quote or backslash left, so nothing is escaped).
                        self.escaped = false;
                        i = bytes.len();
                    }
                }
            } else {
                match bytes[i..].iter().position(|&x| x == b'"') {
                    Some(pos) => {
                        self.in_string = true;
                        i += pos + 1;
                    }
                    None => i = bytes.len(),
                }
            }
        }
        self
    }
}

/// Position of the first `a` or `b` in `bytes` (linear scan; the compiler
/// vectorizes the equality loop, and chunks are at most one stride).
fn find2(bytes: &[u8], a: u8, b: u8) -> Option<usize> {
    bytes.iter().position(|&x| x == a || x == b)
}

/// Lexer checkpoints of one line: `points[k]` is the state at byte
/// `line_start + k * CHECKPOINT_STRIDE`, before processing that byte.
#[derive(Clone, Debug)]
pub struct LineCheckpoints {
    pub line_start: usize,
    pub line_end: usize,
    pub revision: u64,
    pub points: Vec<LexState>,
}

enum Entry {
    Ready(LineCheckpoints),
    Building { revision: u64, line_start: usize, line_end: usize },
}

/// Shared across threads so the lex worker can publish its result.
static STORE: Mutex<Option<HashMap<(u64, usize), Entry>>> = Mutex::new(None);

fn with_store<R>(f: impl FnOnce(&mut HashMap<(u64, usize), Entry>) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|err| err.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Exact lexer state at `col_start` (a byte offset on `line`).
pub fn window_state(tab: u64, line: usize, buf: &Buffer, col_start: usize) -> LexState {
    let range = buf.line_byte_range(line);
    let col_start = col_start.clamp(range.start, range.end);
    if col_start == range.start {
        return LexState::default();
    }
    let revision = buf.revision();
    let ready = with_store(|store| match store.get(&(tab, line)) {
        Some(Entry::Ready(cp)) => Some(cp.clone()),
        _ => None,
    });
    if let Some(cp) = ready {
        // Only the line start anchors checkpoint offsets; the line end moves
        // with every insert on the line and is covered by the salvage cut.
        if cp.line_start == range.start && cp.revision <= revision {
            if let Some(edits) = buf.edits_since(cp.revision) {
                let first_touch = edits
                    .iter()
                    .filter(|r| r.end > range.start && r.start < range.end)
                    .map(|r| r.start)
                    .min();
                match first_touch {
                    None => return state_from_checkpoints(buf, &cp, col_start),
                    Some(cut) => {
                        // Checkpoints before the first edit are still exact
                        // (content and offsets unchanged); lex from the last
                        // of them instead of rescanning the whole line while
                        // the worker rebuilds.
                        let keep = cut.saturating_sub(range.start) / CHECKPOINT_STRIDE;
                        let mut salvaged = cp;
                        salvaged.points.truncate(keep + 1);
                        return state_from_checkpoints(buf, &salvaged, col_start);
                    }
                }
            }
        }
    }
    if range.end - range.start > OFF_THREAD_BYTES {
        spawn_build(tab, line, buf.snapshot(), range.clone(), revision);
        // Provisional but exact: scan from the line start until the worker
        // publishes checkpoints (a few frames at most).
        let bytes = buf.slice_bytes(range.start..col_start);
        return LexState::default().advance(bytes.as_bytes());
    }
    let cp = build_checkpoints(&buf.snapshot(), &range, revision);
    with_store(|store| {
        store.insert((tab, line), Entry::Ready(cp.clone()));
    });
    state_from_checkpoints(buf, &cp, col_start)
}

fn state_from_checkpoints(buf: &Buffer, cp: &LineCheckpoints, col_start: usize) -> LexState {
    let rel = col_start - cp.line_start;
    // A salvaged (truncated) checkpoint list may end before the window:
    // fall back to its last point and scan the gap.
    let k = (rel / CHECKPOINT_STRIDE).min(cp.points.len() - 1);
    let from = cp.line_start + k * CHECKPOINT_STRIDE;
    let bytes = buf.slice_bytes(from..col_start);
    cp.points[k].advance(bytes.as_bytes())
}

/// Scans the whole line once, recording the state at every stride boundary.
fn build_checkpoints(rope: &Rope, range: &Range<usize>, revision: u64) -> LineCheckpoints {
    let mut points = vec![LexState::default()];
    let mut state = LexState::default();
    let mut at = range.start;
    while at < range.end {
        let to = (at + CHECKPOINT_STRIDE).min(range.end);
        let chunk = rope.slice(rope.byte_to_char(at)..rope.byte_to_char(to));
        for piece in chunk.chunks() {
            state = state.advance(piece.as_bytes());
        }
        at = to;
        if at < range.end {
            points.push(state);
        }
    }
    LineCheckpoints {
        line_start: range.start,
        line_end: range.end,
        revision,
        points,
    }
}

fn spawn_build(tab: u64, line: usize, rope: Rope, range: Range<usize>, revision: u64) {
    let duplicate = with_store(|store| {
        match store.get(&(tab, line)) {
            Some(Entry::Building { revision: r, line_start, line_end }) => {
                *r == revision && *line_start == range.start && *line_end == range.end
            }
            _ => false,
        }
    });
    if duplicate {
        return;
    }
    with_store(|store| {
        store.insert(
            (tab, line),
            Entry::Building {
                revision,
                line_start: range.start,
                line_end: range.end,
            },
        );
    });
    std::thread::Builder::new()
        .name("json-viewer-lex".into())
        .spawn(move || {
            let cp = build_checkpoints(&rope, &range, revision);
            with_store(|store| {
                store.insert((tab, line), Entry::Ready(cp));
            });
        })
        .expect("spawn lex checkpoint thread");
}

/// Drops every checkpoint (tab restore / tests).
pub fn clear_all() {
    with_store(|store| store.clear());
}

/// Whether published checkpoints exist for a line (tests wait on this).
pub fn is_ready(tab: u64, line: usize) -> bool {
    with_store(|store| matches!(store.get(&(tab, line)), Some(Entry::Ready(_))))
}

fn palette() -> (Color, Color, Color, Color, Color) {
    (
        rune::oklch(0.72, 0.14, 250.0), // object keys
        rune::oklch(0.82, 0.12, 145.0), // strings
        rune::oklch(0.84, 0.13, 65.0),  // numbers
        rune::oklch(0.76, 0.15, 320.0), // true / false / null
        rune::oklch(0.66, 0.02, 260.0), // punctuation
    )
}

/// Bytes just past a window's end, needed for exact ink decisions: the
/// whitespace run plus one char after a closing quote (key vs string) and
/// up to five chars to complete a literal (`true`/`false`/`null`) cut by the
/// window edge. Scans only the whitespace run, never the rest of the line.
pub fn tail_for(buf: &Buffer, col_end: usize, line_end: usize) -> String {
    let rope = buf.snapshot();
    let mut out = String::new();
    let mut chars = rope.chars_at(rope.byte_to_char(col_end));
    let mut seen_non_ws = false;
    let mut extra = 0usize;
    while col_end + out.len() < line_end {
        let Some(c) = chars.next() else { break };
        if !seen_non_ws && (c == ' ' || c == '\t') {
            out.push(c);
            continue;
        }
        seen_non_ws = true;
        out.push(c);
        extra += 1;
        if extra >= 5 {
            break;
        }
    }
    out
}

/// Whether a string cut by the window end is an object key: scans forward
/// (bounded) for its unescaped closing quote and the `:` after it. A string
/// whose close is farther than the bound is a long value, never a key.
pub fn cut_string_is_key(buf: &Buffer, col_end: usize, line_end: usize) -> bool {
    const BOUND: usize = 4096;
    let to = (col_end + BOUND).min(line_end);
    let bytes = buf.slice_bytes(col_end..to);
    let bytes = bytes.as_bytes();
    let mut i = 0;
    let mut close = None;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'"' {
            close = Some(i);
            break;
        }
        i += 1;
    }
    let Some(mut at) = close else {
        return false;
    };
    at += 1;
    while at < bytes.len() && (bytes[at] == b' ' || bytes[at] == b'\t') {
        at += 1;
    }
    at < bytes.len() && bytes[at] == b':'
}

/// JSON token colors for one window, starting from the exact lexer state
/// at its first byte. `offset` shifts byte ranges into the display string;
/// `tail` is [`tail_for`] (bytes past the window end) so ink decisions match
/// whole-line lexing; `cut_key` is [`cut_string_is_key`] when the window ends
/// inside a string (its closing quote lies beyond the window). A window that
/// starts inside a string emits the leading string fragment first.
pub fn json_tokens(
    slice: &str,
    offset: usize,
    start: LexState,
    tail: &str,
    cut_key: Option<bool>,
) -> Vec<(Range<usize>, Color)> {
    let (key_ink, string_ink, number_ink, literal_ink, punctuation_ink) = palette();
    let bytes = slice.as_bytes();
    let tail_bytes = tail.as_bytes();
    // Key-vs-string ink: skip spaces/tabs after the closing quote, crossing
    // into the tail when the window ends right after the quote.
    let key_ink_at = |close: usize| -> bool {
        let mut look = close;
        while look < bytes.len() && (bytes[look] == b' ' || bytes[look] == b'\t') {
            look += 1;
        }
        if look < bytes.len() {
            return bytes[look] == b':';
        }
        let mut t = 0;
        while t < tail_bytes.len() && (tail_bytes[t] == b' ' || tail_bytes[t] == b'\t') {
            t += 1;
        }
        t < tail_bytes.len() && tail_bytes[t] == b':'
    };
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut state = start;
    if state.in_string {
        let begin = 0;
        while i < bytes.len() {
            if state.escaped {
                state.escaped = false;
            } else if bytes[i] == b'\\' {
                state.escaped = true;
            } else if bytes[i] == b'"' {
                i += 1;
                state.in_string = false;
                break;
            }
            i += 1;
        }
        let ink = if (!state.in_string && key_ink_at(i))
            || (state.in_string && cut_key.unwrap_or(false))
        {
            key_ink
        } else {
            string_ink
        };
        tokens.push((offset + begin..offset + i, ink));
    }
    while i < bytes.len() {
        let begin = i;
        match bytes[i] {
            b'"' => {
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'"' {
                        i += 1;
                        closed = true;
                        break;
                    } else {
                        i += 1;
                    }
                }
                let ink = if closed {
                    if key_ink_at(i) {
                        key_ink
                    } else {
                        string_ink
                    }
                } else if cut_key.unwrap_or(false) {
                    key_ink
                } else {
                    string_ink
                };
                tokens.push((offset + begin..offset + i, ink));
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                tokens.push((offset + begin..offset + i, number_ink));
            }
            b't' | b'f' | b'n' => {
                let literal: &[u8] = match bytes[i] {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                let rest = &bytes[i..];
                let completed = if rest.len() >= literal.len() {
                    rest.starts_with(literal)
                } else {
                    // Literal cut by the window edge: complete it from the
                    // tail before deciding.
                    let mut joined = rest.to_vec();
                    joined.extend_from_slice(&tail_bytes[..(literal.len() - rest.len()).min(tail_bytes.len())]);
                    joined.starts_with(literal)
                };
                if completed {
                    i += literal.len().min(bytes.len() - i);
                    tokens.push((offset + begin..offset + i, literal_ink));
                } else {
                    i += 1;
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                i += 1;
                tokens.push((offset + begin..offset + i, punctuation_ink));
            }
            _ => i += 1,
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_tracks_strings_and_escapes() {
        let s = LexState::default();
        let s = s.advance(b"\"abc");
        assert!(s.in_string && !s.escaped);
        let s = s.advance(b"\\\"");
        assert!(s.in_string && !s.escaped); // escaped quote stays inside
        let s = s.advance(b"\"");
        assert!(!s.in_string);
        let s = LexState::default().advance(b"\"trailing\\");
        assert!(s.in_string && s.escaped);
        let s = s.advance(b"\"x\"");
        assert!(!s.in_string); // escaped quote, then real close
        let s = LexState::default().advance(b"\"a\\\\\"");
        assert!(!s.in_string); // escaped backslash, then real close
    }

    #[test]
    fn checkpoints_every_stride_and_window_state_exact() {
        let mut line = String::new();
        for i in 0..3000 {
            line.push_str(&format!(
                "{{\"id\":{i},\"s\":\"digits 12345 and \\\"quote\\\" and \\\\ back\",\"v\":{i}.5}},"
            ));
        }
        let buf = Buffer::from_text(&line);
        clear_all();
        let range = buf.line_byte_range(0);
        assert!(range.end > 3 * CHECKPOINT_STRIDE);
        // Whole-line reference states at probe columns.
        let probes = [
            1,
            40,
            CHECKPOINT_STRIDE - 1,
            CHECKPOINT_STRIDE,
            CHECKPOINT_STRIDE + 1,
            2 * CHECKPOINT_STRIDE + 17,
            range.end - 1,
            range.end,
        ];
        let whole = buf.slice_bytes(range.clone());
        for col in probes {
            let want = LexState::default().advance(&whole.as_bytes()[..col]);
            let got = window_state(7, 0, &buf, range.start + col);
            assert_eq!(got, want, "state at column {col}");
        }
        clear_all();
    }
}