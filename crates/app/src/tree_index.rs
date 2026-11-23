//! Compact JSON tree index for the Tree explorer view.
//!
//! The classic JSON-editor tree needs, per node: the kind, the byte range of
//! its key token (object members only), the byte range of its value (a whole
//! subtree for containers), its child count, and sibling/child links. This
//! module builds exactly that from the raw text in **one streaming pass with
//! a hand-rolled tokenizer**: no `serde_json::Value` DOM is ever built, so a
//! 100 MB / 6.9 MB single-line document costs one bounded pass and a
//! struct-of-arrays index, not a full object graph.
//!
//! ## Memory: 24 bytes per node
//!
//! Six `u32` arrays hold everything ([`TreeIndex`]): `key_off`, `val_off`,
//! `val_end`, `meta` (kind + child count packed), `first_child`, `next`.
//! The node `depth` and `parent` are *not* stored: the flattened row list
//! ([`TreeRow`]) carries them for visible nodes only, and paths are built by
//! walking the row chain, so the index stays 24 bytes/node even for very deep
//! documents. Key lengths are also derived (a bounded scan for the closing
//! quote) because keys are re-read only for visible rows, sorting and
//! filtering.
//!
//! ## Threading model (no `Effect`, no runner edits)
//!
//! `Effect` execution lives in `runner.rs`, which this branch does not own,
//! so tree work rides Rune's own cross-thread channel instead:
//! [`set_app_handle`] (wired once in `main.rs`) stores the live
//! [`rune::AppHandle`]; [`ensure_index`] then builds off the UI thread and
//! delivers its result through `AppHandle::update`, which wakes the event loop
//! and queues a [`crate::state::Msg`] for `runner::poll` to drain — the same
//! message flow as the JSON/search workers, without touching the runner.
//! Without a handle (tests, headless CLI) every job runs synchronously, so
//! reducers stay deterministic and unit-testable.
//!
//! Row-list rebuilds that can be huge (expand-all, sort re-flattening, filter)
//! go through [`run_job`] with a per-tab cancel flag
//! ([`register_job_cancel`]); results are generation-gated in the reducer.
//! Expand-all therefore never freezes the UI: below
//! [`SYNC_ROW_LIMIT`] nodes it is computed inline, above that it runs
//! off-thread with a status line and a cancelable worker (see AGENTS.md).
//!
//! ## Filter and sort semantics
//!
//! The filter mirrors `search.rs` matching rules: keys and string values
//! match case-insensitively (keys via their unescaped text), numbers match by
//! decimal text, booleans and `null` never match. Matched nodes keep their
//! ancestors, and containers with matches below are auto-expanded. Sort A→Z
//! is a *view-order* permutation per object computed at enumeration time; it
//! never touches the index, and array order is untouched.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::state::{AppState, Msg, TabId};

/// Sentinel for "no link" / "no key offset".
pub const NONE: u32 = u32::MAX;
/// Node counts at or below this are rebuilt synchronously in the reducer;
/// larger expand-all / sort / filter jobs run off the UI thread.
pub const SYNC_ROW_LIMIT: usize = 200_000;
/// Rows deeper than this keep their depth but stop drawing guides.
pub const MAX_ROW_DEPTH: u16 = 1_000;

// ── Node kinds ─────────────────────────────────────────────────────────────

/// What one JSON value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// `{ … }`
    Object,
    /// `[ … ]`
    Array,
    /// String value.
    String,
    /// Number value.
    Number,
    /// `true` / `false`.
    Bool,
    /// `null`.
    Null,
}

impl NodeKind {
    const fn to_u8(self) -> u8 {
        match self {
            NodeKind::Object => 0,
            NodeKind::Array => 1,
            NodeKind::String => 2,
            NodeKind::Number => 3,
            NodeKind::Bool => 4,
            NodeKind::Null => 5,
        }
    }

    const fn from_u8(b: u8) -> Self {
        match b {
            0 => NodeKind::Object,
            1 => NodeKind::Array,
            2 => NodeKind::String,
            3 => NodeKind::Number,
            4 => NodeKind::Bool,
            _ => NodeKind::Null,
        }
    }

    /// Objects and arrays have children.
    pub const fn is_container(self) -> bool {
        matches!(self, NodeKind::Object | NodeKind::Array)
    }

    /// Name shown in the root row's label.
    pub const fn display(self) -> &'static str {
        match self {
            NodeKind::Object => "object",
            NodeKind::Array => "array",
            NodeKind::String => "string",
            NodeKind::Number => "number",
            NodeKind::Bool => "boolean",
            NodeKind::Null => "null",
        }
    }
}

// ── Errors ─────────────────────────────────────────────────────────────────

/// Why a tree index could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeError {
    /// Byte offset in the source text where parsing stopped.
    pub offset: usize,
    /// Human-readable reason, including the 1-based line and column.
    pub message: String,
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Renders `message` with the 1-based line and column of `offset` in `text`.
fn error_at(text: &str, offset: usize, message: &str) -> TreeError {
    let clipped = offset.min(text.len());
    let upto = &text.as_bytes()[..clipped];
    let line = upto.iter().filter(|&&b| b == b'\n').count() + 1;
    let column = match upto.iter().rposition(|&b| b == b'\n') {
        Some(nl) => clipped - nl - 1,
        None => clipped,
    } + 1;
    TreeError {
        offset,
        message: format!("Invalid JSON: {message} at line {line} column {column}"),
    }
}

// ── The index ──────────────────────────────────────────────────────────────

/// Struct-of-arrays node index, 24 bytes per node.
///
/// Node ids are indices into the arrays, assigned in document order (a
/// container's id is smaller than every child's). Array elements and the
/// root have [`NONE`] in `key_off`; key token *lengths* are not stored —
/// [`TreeIndex::key_token_range`] rescans the (short) token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeIndex {
    /// Byte offset of the key token's opening quote, or [`NONE`].
    key_off: Vec<u32>,
    /// Byte offset of the value token (the `{`/`[` for containers).
    val_off: Vec<u32>,
    /// Byte offset just past the value (after the closing brace/bracket).
    val_end: Vec<u32>,
    /// Low 3 bits: [`NodeKind`]. Rest: child count.
    meta: Vec<u32>,
    /// First child id, or [`NONE`] for leaves and empty containers.
    first_child: Vec<u32>,
    /// Next sibling id, or [`NONE`] for the last child.
    next: Vec<u32>,
}

const COUNT_SHIFT: u32 = 3;

/// Streaming builder. Iterative (explicit stack), so a pathologically deep
/// document cannot overflow the real stack.
struct Builder<'a> {
    text: &'a [u8],
    pos: usize,
    key_off: Vec<u32>,
    val_off: Vec<u32>,
    val_end: Vec<u32>,
    meta: Vec<u32>,
    first_child: Vec<u32>,
    next: Vec<u32>,
    /// Open containers, innermost last: `(node, last child id, member count)`.
    stack: Vec<(u32, u32, u32)>,
}

impl<'a> Builder<'a> {
    fn new(text: &'a str) -> Self {
        let estimate = (text.len() / 24).max(16);
        Self {
            text: text.as_bytes(),
            pos: 0,
            key_off: Vec::with_capacity(estimate),
            val_off: Vec::with_capacity(estimate),
            val_end: Vec::with_capacity(estimate),
            meta: Vec::with_capacity(estimate),
            first_child: Vec::with_capacity(estimate),
            next: Vec::with_capacity(estimate),
            stack: Vec::new(),
        }
    }

    fn finish(self) -> TreeIndex {
        TreeIndex {
            key_off: self.key_off,
            val_off: self.val_off,
            val_end: self.val_end,
            meta: self.meta,
            first_child: self.first_child,
            next: self.next,
        }
    }

    fn err(&self, text: &str, message: &str) -> TreeError {
        error_at(text, self.pos.min(self.text.len()), message)
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.text.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Records one node against the enclosing container (or the root) and
    /// returns its id. `key` is the key token's opening-quote offset. Also
    /// links the node into its parent's sibling chain.
    fn attach(&mut self, kind: NodeKind, key: u32, val_off: usize) -> u32 {
        let id = self.meta.len() as u32;
        self.key_off.push(key);
        self.val_off.push(val_off as u32);
        self.val_end.push(0); // filled when the value completes
        self.meta.push(kind.to_u8() as u32);
        self.first_child.push(NONE);
        self.next.push(NONE);
        if let Some((parent, prev, count)) = self.stack.last_mut() {
            let parent = *parent;
            let prev = *prev;
            *count += 1;
            self.stack.last_mut().expect("just checked").1 = id;
            if prev == NONE {
                self.first_child[parent as usize] = id;
            } else {
                self.next[prev as usize] = id;
            }
        }
        id
    }

    /// Sets the child count on a closing container.
    fn seal(&mut self, node: u32, count: u32) {
        self.meta[node as usize] = (self.meta[node as usize] & 0x7) | (count << COUNT_SHIFT);
    }

    /// Scans a string token starting at the opening quote. Returns the offset
    /// just past the closing quote. Escape-aware; control bytes are rejected.
    fn scan_string(&mut self) -> Result<usize, ()> {
        debug_assert_eq!(self.text.get(self.pos), Some(&b'"'));
        let mut i = self.pos + 1;
        while i < self.text.len() {
            match self.text[i] {
                b'"' => return Ok(i + 1),
                b'\\' => {
                    i += 1;
                    match self.text.get(i).copied() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            i += 1;
                        }
                        Some(b'u') => {
                            let hex = self.text.get(i + 1..i + 5).ok_or(())?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return Err(());
                            }
                            i += 5;
                        }
                        _ => return Err(()),
                    }
                }
                b if b < 0x20 => return Err(()),
                _ => i += 1,
            }
        }
        Err(())
    }

    /// Scans a JSON number token, leaving `pos` just past it. Returns the
    /// token's start offset. `Err(Some(pos))` names the offending byte.
    fn scan_number(&mut self) -> Result<usize, Option<usize>> {
        let start = self.pos;
        let mut i = self.pos;
        if self.text.get(i) == Some(&b'-') {
            i += 1;
        }
        let digits_start = i;
        while matches!(self.text.get(i), Some(b) if b.is_ascii_digit()) {
            i += 1;
        }
        if i == digits_start {
            return Err(None); // no digits
        }
        if self.text[digits_start] == b'0' && i - digits_start > 1 {
            return Err(Some(digits_start)); // leading zero
        }
        if self.text.get(i) == Some(&b'.') {
            let frac = i + 1;
            let mut j = frac;
            while matches!(self.text.get(j), Some(b) if b.is_ascii_digit()) {
                j += 1;
            }
            if j == frac {
                return Err(Some(i));
            }
            i = j;
        }
        if matches!(self.text.get(i), Some(b'e') | Some(b'E')) {
            let mut j = i + 1;
            if matches!(self.text.get(j), Some(b'+') | Some(b'-')) {
                j += 1;
            }
            let exp = j;
            while matches!(self.text.get(j), Some(b) if b.is_ascii_digit()) {
                j += 1;
            }
            if j == exp {
                return Err(Some(i));
            }
            i = j;
        }
        self.pos = i;
        Ok(start)
    }

    /// Matches a literal (`true`/`false`/`null`), leaving `pos` past it.
    fn scan_literal(&mut self, word: &[u8]) -> Result<usize, ()> {
        if self.text.len() >= self.pos + word.len()
            && &self.text[self.pos..self.pos + word.len()] == word
        {
            self.pos += word.len();
            Ok(self.pos - word.len())
        } else {
            Err(())
        }
    }
}

impl TreeIndex {
    /// Builds the index for `text` in one streaming pass.
    ///
    /// Invalid JSON (bad tokens, trailing characters) is an error, matching
    /// `search.rs`, which also returns no results for unparseable input.
    pub fn build(text: &str) -> Result<Self, TreeError> {
        let mut b = Builder::new(text);
        b.skip_ws();
        if b.pos >= b.text.len() {
            return Err(error_at(text, 0, "empty document"));
        }

        // Parse the root value; containers push a frame and fall through to
        // the member loop, scalars complete at once.
        parse_value(&mut b, text, NONE)?;
        if b.stack.is_empty() {
            b.skip_ws();
            if b.pos != b.text.len() {
                return Err(b.err(text, "trailing characters after the document"));
            }
        }

        // Member loop: each iteration parses one member of the innermost open
        // container, or closes it.
        while let Some(&(frame_node, _, frame_count)) = b.stack.last() {
            b.skip_ws();
            let is_object = b.meta[frame_node as usize] & 0x7 == NodeKind::Object.to_u8() as u32;
            let close = if is_object { b'}' } else { b']' };
            if b.peek() == Some(close) {
                b.pos += 1;
                b.val_end[frame_node as usize] = b.pos as u32;
                b.seal(frame_node, frame_count);
                b.stack.pop();
                // A closed container is a completed value; when the root
                // itself closed, only trailing characters can remain.
                if b.stack.is_empty() {
                    b.skip_ws();
                    if b.pos != b.text.len() {
                        return Err(b.err(text, "trailing characters after the document"));
                    }
                }
                continue;
            }

            // Separator before every member except the first.
            if frame_count > 0 {
                match b.peek() {
                    Some(b',') => b.pos += 1,
                    _ => return Err(b.err(text, "expected ',' or a closing bracket")),
                }
            }

            // One member: objects read `"key":` first, arrays go straight to
            // the value.
            let key = if is_object {
                b.skip_ws();
                match b.peek() {
                    Some(b'"') => {}
                    _ => return Err(b.err(text, "expected an object key")),
                }
                let key_off = b.pos as u32;
                match b.scan_string() {
                    Ok(end) => b.pos = end,
                    Err(()) => return Err(b.err(text, "invalid object key")),
                }
                b.skip_ws();
                match b.peek() {
                    Some(b':') => b.pos += 1,
                    _ => return Err(b.err(text, "expected ':' after the object key")),
                }
                key_off
            } else {
                NONE
            };

            b.skip_ws();
            if b.pos >= b.text.len() {
                return Err(b.err(text, "unexpected end of input"));
            }
            parse_value(&mut b, text, key)?;
        }

        Ok(b.finish())
    }

    /// Number of nodes (the root included).
    pub fn node_count(&self) -> usize {
        self.meta.len()
    }

    /// Bytes of index storage per node.
    pub const BYTES_PER_NODE: usize = 6 * std::mem::size_of::<u32>();

    /// The node's kind.
    pub fn kind(&self, node: u32) -> NodeKind {
        NodeKind::from_u8((self.meta[node as usize] & 0x7) as u8)
    }

    /// Direct children of a container.
    pub fn child_count(&self, node: u32) -> u32 {
        self.meta[node as usize] >> COUNT_SHIFT
    }

    /// Objects and arrays are containers (whatever their child count).
    pub fn is_container(&self, node: u32) -> bool {
        self.kind(node).is_container()
    }

    /// First child id, or `None` for leaves and empty containers.
    pub fn first_child(&self, node: u32) -> Option<u32> {
        match self.first_child[node as usize] {
            NONE => None,
            id => Some(id),
        }
    }

    /// Id of the sibling after `node`, or `None` for the last child.
    pub fn next_sibling(&self, node: u32) -> Option<u32> {
        match self.next[node as usize] {
            NONE => None,
            id => Some(id),
        }
    }

    /// Direct children in document order: `(id, index among siblings)`.
    pub fn children(&self, node: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut cur = self.first_child[node as usize];
        let mut index = 0u32;
        while cur != NONE {
            out.push((cur, index));
            index += 1;
            cur = self.next[cur as usize];
        }
        out
    }

    /// Ids of this container's direct children in document order. Cached by
    /// [`TreeUi`] across expands; off-thread jobs enumerate directly.
    pub fn child_ids(&self, node: u32) -> Vec<u32> {
        self.children(node).into_iter().map(|(id, _)| id).collect()
    }

    /// Byte range of the key token (opening quote included), if the node is
    /// an object member.
    pub fn key_token_range(&self, text: &str, node: u32) -> Option<(usize, usize)> {
        let start = self.key_off[node as usize];
        if start == NONE {
            return None;
        }
        let start = start as usize;
        Some((start, scan_string_end(text.as_bytes(), start)))
    }

    /// Unescaped key text, if the node is an object member.
    pub fn key_text<'a>(&self, text: &'a str, node: u32) -> Option<Cow<'a, str>> {
        let (start, end) = self.key_token_range(text, node)?;
        Some(decode_string_token(&text[start..end]))
    }

    /// Byte range of the value token (a whole subtree for containers).
    pub fn value_range(&self, node: u32) -> (usize, usize) {
        (
            self.val_off[node as usize] as usize,
            self.val_end[node as usize] as usize,
        )
    }

    /// The value's raw text (a whole subtree for containers).
    pub fn value_text<'a>(&self, text: &'a str, node: u32) -> &'a str {
        let (start, end) = self.value_range(node);
        &text[start.min(text.len())..end.min(text.len())]
    }

    /// Resolves one path segment from `node`: an object member's key, or an
    /// array element's index. The first matching child wins, matching the
    /// streaming search walk's behaviour on duplicate keys.
    pub fn child_by_segment(&self, text: &str, node: u32, segment: &str) -> Option<u32> {
        if self.kind(node) == NodeKind::Object {
            let mut child = self.first_child(node);
            while let Some(c) = child {
                if self.key_text(text, c).as_deref() == Some(segment) {
                    return Some(c);
                }
                child = self.next_sibling(c);
            }
            None
        } else {
            let index = segment.parse::<u32>().ok()?;
            self.children(node)
                .get(index as usize)
                .map(|&(child, _)| child)
        }
    }

    /// Finds the node a ` → `-joined path (the search results' format)
    /// points at, starting from the root. An empty path is the root itself.
    /// Keys may themselves contain the display separator, so an exact match
    /// for the remaining path wins before the split-path interpretation.
    pub fn node_for_path(&self, text: &str, path: &str) -> Option<u32> {
        self.node_for_path_from(text, 0, path)
    }

    fn node_for_path_from(&self, text: &str, node: u32, path: &str) -> Option<u32> {
        if path.is_empty() {
            return Some(node);
        }
        if let Some(exact) = self.child_by_segment(text, node, path) {
            return Some(exact);
        }
        for (split, _) in path.match_indices(" → ") {
            let first = &path[..split];
            let rest = &path[split + " → ".len()..];
            let Some(child) = self.child_by_segment(text, node, first) else {
                continue;
            };
            if let Some(found) = self.node_for_path_from(text, child, rest) {
                return Some(found);
            }
        }
        None
    }
}

/// Parses one value at `b.pos`, recording its node. Containers stay on the
/// builder's stack; the caller's member loop closes them.
fn parse_value(b: &mut Builder<'_>, text: &str, key: u32) -> Result<u32, TreeError> {
    b.skip_ws();
    let Some(byte) = b.peek() else {
        return Err(b.err(text, "unexpected end of input"));
    };
    let val_off = b.pos;
    match byte {
        b'{' | b'[' => {
            b.pos += 1;
            let kind = if byte == b'{' { NodeKind::Object } else { NodeKind::Array };
            let node = b.attach(kind, key, val_off);
            b.stack.push((node, NONE, 0));
            Ok(node)
        }
        b'"' => {
            match b.scan_string() {
                Ok(end) => b.pos = end,
                Err(()) => return Err(b.err(text, "unterminated string")),
            }
            let node = b.attach(NodeKind::String, key, val_off);
            b.val_end[node as usize] = b.pos as u32;
            Ok(node)
        }
        b'-' | b'0'..=b'9' => {
            match b.scan_number() {
                Ok(_) => {}
                Err(Some(pos)) => {
                    b.pos = pos;
                    return Err(b.err(text, "invalid number"));
                }
                Err(None) => return Err(b.err(text, "invalid number")),
            }
            let node = b.attach(NodeKind::Number, key, val_off);
            b.val_end[node as usize] = b.pos as u32;
            Ok(node)
        }
        b't' => {
            b.scan_literal(b"true")
                .map_err(|_| b.err(text, "invalid literal"))?;
            let node = b.attach(NodeKind::Bool, key, val_off);
            b.val_end[node as usize] = b.pos as u32;
            Ok(node)
        }
        b'f' => {
            b.scan_literal(b"false")
                .map_err(|_| b.err(text, "invalid literal"))?;
            let node = b.attach(NodeKind::Bool, key, val_off);
            b.val_end[node as usize] = b.pos as u32;
            Ok(node)
        }
        b'n' => {
            b.scan_literal(b"null")
                .map_err(|_| b.err(text, "invalid literal"))?;
            let node = b.attach(NodeKind::Null, key, val_off);
            b.val_end[node as usize] = b.pos as u32;
            Ok(node)
        }
        _ => Err(b.err(text, "expected a JSON value")),
    }
}

// ── String decoding ────────────────────────────────────────────────────────

/// Offset just past the closing quote of the string token at `start`.
fn scan_string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return i + 1,
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Decodes the inner text of a JSON string token (quotes stripped, escapes
/// resolved, surrogate pairs combined). Borrowed when there is no backslash.
pub fn decode_string_token(token: &str) -> Cow<'_, str> {
    let inner = &token[1..token.len().saturating_sub(1)];
    if !inner.contains('\\') {
        return Cow::Borrowed(inner);
    }
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            // Multi-byte UTF-8 passes through whole.
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                i += 1;
            }
            out.push_str(&inner[start..i]);
            continue;
        }
        i += 1;
        let Some(&esc) = bytes.get(i) else { break };
        i += 1;
        match esc {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let mut code = match parse_hex4(bytes, i) {
                    Some((code, next)) => {
                        i = next;
                        code
                    }
                    None => 0xFFFD,
                };
                // Surrogate pair.
                if (0xD800..0xDC00).contains(&code)
                    && bytes.get(i) == Some(&b'\\')
                    && bytes.get(i + 1) == Some(&b'u')
                {
                    if let Some((low, next)) = parse_hex4(bytes, i + 2) {
                        if (0xDC00..0xE000).contains(&low) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            i = next;
                        }
                    }
                }
                match char::from_u32(code) {
                    Some(ch) => out.push(ch),
                    None => out.push('\u{FFFD}'),
                }
            }
            _ => out.push('\u{FFFD}'),
        }
    }
    Cow::Owned(out)
}

fn parse_hex4(bytes: &[u8], at: usize) -> Option<(u32, usize)> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    let mut code = 0u32;
    for &b in slice {
        code = (code << 4) | (b as char).to_digit(16)?;
    }
    Some((code, at + 4))
}

/// Quotes and backslashes back on, so a decoded string still reads as one
/// JSON string; non-ASCII stays raw (unlike `JSON.stringify`).
pub fn display_string(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len() + 2);
    out.push('"');
    for ch in decoded.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ── Matching (search.rs rules) ──────────────────────────────────────────────

/// Case-insensitive `contains` without allocating when both sides are ASCII.
fn matches_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() {
        let needle = needle.to_ascii_lowercase();
        let hay = hay.as_bytes();
        let needle = needle.as_bytes();
        if needle.len() > hay.len() {
            return false;
        }
        hay.windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    } else {
        hay.to_lowercase().contains(&needle.to_lowercase())
    }
}

/// Does this node's key or scalar value match `needle` (search.rs rules:
/// keys and strings case-insensitively, numbers by text, bool/null never)?
fn node_matches(index: &TreeIndex, text: &str, node: u32, needle: &str) -> bool {
    if let Some(key) = index.key_text(text, node) {
        if matches_ci(&key, needle) {
            return true;
        }
    }
    match index.kind(node) {
        NodeKind::String => {
            let (start, end) = index.value_range(node);
            let decoded = decode_string_token(&text[start..end]);
            matches_ci(&decoded, needle)
        }
        NodeKind::Number => matches_ci(index.value_text(text, node), needle),
        _ => false,
    }
}

// ── Rows (flattened visible nodes) ─────────────────────────────────────────

/// One visible row of the flattened tree. Rows are view state; the index
/// never stores depth or parents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRow {
    /// Node id (index slot).
    pub node: u32,
    /// Indentation level, saturating at [`MAX_ROW_DEPTH`].
    pub depth: u16,
    /// Row index of the parent row, or [`NONE`] for the root row.
    pub parent_row: u32,
    /// Document-order position among siblings (what `[index]` shows).
    pub child_index: u32,
    /// Containers only: are this row's children spliced in below?
    pub expanded: bool,
}

/// The selected node, with everything the footer and copy actions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSelection {
    /// Node id.
    pub node: u32,
    /// ` → `-joined path (root excluded), the search results' format.
    pub path: String,
    /// Byte range of the value in the source text.
    pub value_off: u32,
    pub value_len: u32,
    /// Containers copy their whole subtree text.
    pub is_container: bool,
}

/// Per-tab tree explorer state. Lives in `AppState::tree`, *not* in `Tab`:
/// it is runtime-only (never persisted) and holds an index keyed by the
/// tab's `input_revision`.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeUi {
    /// Whether this tab's document area shows the tree explorer.
    pub open: bool,
    /// An index build is in flight.
    pub building: bool,
    /// `input_revision` of the in-flight build request.
    pub build_pending_revision: Option<u64>,
    /// `input_revision` the installed index covers.
    pub index_revision: u64,
    /// The compact node index.
    pub index: Option<Arc<TreeIndex>>,
    /// Parse failure from the last build.
    pub error: Option<String>,
    /// Flattened visible rows.
    pub rows: Vec<TreeRow>,
    /// Bumped on every rows mutation; keys `El::virtual_rows_version`.
    pub rows_gen: u64,
    /// Rows saved while a filter is active, restored when it clears.
    pub base_rows: Option<Vec<TreeRow>>,
    /// Active filter query (empty = unfiltered).
    pub filter: String,
    /// Matched-node count from the last filter run.
    pub filter_matched: Option<usize>,
    /// Sort A→Z toggle.
    pub sort_keys: bool,
    /// Selected node.
    pub selected: Option<TreeSelection>,
    /// Document-order child enumeration per expanded container.
    children_cache: HashMap<u32, Arc<Vec<u32>>>,
    /// Generation of the newest off-thread rows job.
    pub jobs_gen: u64,
    /// An off-thread rows job (expand-all / sort / filter) is in flight.
    pub job_pending: bool,
    /// What the in-flight job is, for the status label.
    pub job_kind: &'static str,
}

impl TreeUi {
    /// Fresh state for a tab entering tree mode.
    pub fn new(open: bool) -> Self {
        Self {
            open,
            building: false,
            build_pending_revision: None,
            index_revision: 0,
            index: None,
            error: None,
            rows: Vec::new(),
            rows_gen: 0,
            base_rows: None,
            filter: String::new(),
            filter_matched: None,
            sort_keys: false,
            selected: None,
            children_cache: HashMap::new(),
            jobs_gen: 0,
            job_pending: false,
            job_kind: "",
        }
    }

    /// A filter query is active.
    pub fn filter_active(&self) -> bool {
        !self.filter.is_empty()
    }

    /// The index is installed and usable.
    pub fn is_ready(&self) -> bool {
        self.index.is_some()
    }

    /// Version for `El::virtual_rows_version`: row keys change only when the
    /// rows themselves change, so commits stay O(visible) at any size.
    pub fn rows_version(&self) -> (u64, u64) {
        (self.index_revision, self.rows_gen)
    }

    /// Drops the index, rows and selection (input changed or tab cleared).
    pub fn invalidate(&mut self) {
        self.index = None;
        self.index_revision = 0;
        self.build_pending_revision = None;
        self.rows.clear();
        self.base_rows = None;
        self.filter_matched = None;
        self.selected = None;
        self.children_cache.clear();
        self.job_pending = false;
        self.job_kind = "";
        self.building = false;
        self.error = None;
    }

    /// Installs a fresh index and resets to the initial view: the root row,
    /// expanded once, with its direct children below it.
    pub fn install_index(&mut self, index: Arc<TreeIndex>, revision: u64) {
        self.invalidate();
        self.index = Some(index);
        self.index_revision = revision;
        self.error = None;
        self.reset_rows();
    }

    /// Rows for a fresh index: root (expanded) + direct children.
    fn reset_rows(&mut self) {
        let Some(index) = self.index.clone() else {
            return;
        };
        self.rows = initial_rows(&index);
        self.rows_gen = self.rows_gen.wrapping_add(1);
        self.selected = None;
    }

    /// Row index of `node`, or `None`. A linear scan: clicking is rare and
    /// rows are at most one entry per visible node.
    pub fn row_of_node(&self, node: u32) -> Option<usize> {
        self.rows.iter().position(|row| row.node == node)
    }

    /// Cached document-order children of `node`.
    fn cached_children(&mut self, index: &TreeIndex, node: u32) -> Arc<Vec<u32>> {
        if let Some(ids) = self.children_cache.get(&node) {
            return Arc::clone(ids);
        }
        let ids = Arc::new(index.child_ids(node));
        self.children_cache.insert(node, Arc::clone(&ids));
        ids
    }

    /// Children of `node` in display order (sorted when `sort_keys` is on and
    /// the parent is an object), paired with their document-order index.
    fn ordered_children(&mut self, index: &TreeIndex, text: &str, node: u32) -> Vec<(u32, u32)> {
        let ids = self.cached_children(index, node);
        let doc: Vec<(u32, u32)> = ids.iter().copied().zip(0..).collect();
        if self.sort_keys && index.kind(node) == NodeKind::Object {
            sorted_pairs(index, text, doc)
        } else {
            doc
        }
    }

    /// Splices `node`'s children below its row. Returns `true` when the rows
    /// changed. No-ops for missing rows, non-containers and already-expanded
    /// rows (the reducer guards the filtered view before calling).
    pub fn expand(&mut self, index: &TreeIndex, text: &str, node: u32) -> bool {
        let Some(row) = self.row_of_node(node) else {
            return false;
        };
        if self.rows[row].expanded || !index.is_container(node) {
            return false;
        }
        let children = self.ordered_children(index, text, node);
        let depth = self.rows[row].depth.saturating_add(1).min(MAX_ROW_DEPTH);
        let parent_row = row as u32;
        self.rows[row].expanded = true;
        let at = row + 1;
        for (offset, (child, child_index)) in children.iter().copied().enumerate() {
            self.rows.insert(
                at + offset,
                TreeRow {
                    node: child,
                    depth,
                    parent_row,
                    child_index,
                    expanded: false,
                },
            );
        }
        self.rows_gen = self.rows_gen.wrapping_add(1);
        true
    }

    /// Removes `node`'s spliced-in subtree. Returns `true` when rows changed.
    pub fn collapse(&mut self, node: u32) -> bool {
        let Some(row) = self.row_of_node(node) else {
            return false;
        };
        if !self.rows[row].expanded {
            return false;
        }
        self.rows[row].expanded = false;
        let depth = self.rows[row].depth;
        let mut end = row + 1;
        while end < self.rows.len() && self.rows[end].depth > depth {
            end += 1;
        }
        self.rows.drain(row + 1..end);
        self.rows_gen = self.rows_gen.wrapping_add(1);
        true
    }

    /// Selects `node`, computing the footer path and value range. Returns
    /// `true` when the node is visible.
    pub fn select(&mut self, index: &TreeIndex, text: &str, node: u32) -> bool {
        let Some(row) = self.row_of_node(node) else {
            return false;
        };
        // Path: walk the row chain to the root, collecting each ancestor's
        // key (or `[index]` for array elements); the root row (always
        // `rows[0]`) is excluded, matching the search results' path format.
        let mut segments: Vec<String> = Vec::new();
        let mut current = row as u32;
        while current != 0 && current != NONE {
            let ancestor = &self.rows[current as usize];
            match index.key_text(text, ancestor.node) {
                Some(key) => segments.push(key.into_owned()),
                None => segments.push(ancestor.child_index.to_string()),
            }
            current = ancestor.parent_row;
        }
        segments.reverse();
        let (value_off, value_end) = index.value_range(node);
        self.selected = Some(TreeSelection {
            node,
            path: segments.join(" → "),
            value_off: value_off as u32,
            value_len: (value_end - value_off) as u32,
            is_container: index.is_container(node),
        });
        true
    }

    /// Collapse-all: the root stays expanded, everything below collapses.
    /// The result is always tiny (root + direct children), so this is cheap
    /// enough to run inline.
    pub fn collapse_all(&mut self, index: &TreeIndex, text: &str) {
        let sort = self.sort_keys;
        self.reset_rows();
        if sort {
            let expanded: HashSet<u32> = [0u32].into_iter().collect();
            let cancel = AtomicBool::new(false);
            if let Some(rows) = flatten(index, text, &expanded, sort, &cancel) {
                self.rows = rows;
            }
        }
        self.selected = None;
    }

    /// Rebuilds every row from an explicit expanded set (used by expand-all
    /// below [`SYNC_ROW_LIMIT`] and by the sort toggle).
    pub fn rebuild_all(&mut self, index: &TreeIndex, text: &str, expanded: &HashSet<u32>) {
        let cancel = AtomicBool::new(false);
        if let Some(rows) = flatten(index, text, expanded, self.sort_keys, &cancel) {
            self.rows = rows;
            self.rows_gen = self.rows_gen.wrapping_add(1);
            self.selected = None;
        }
    }

    /// Saves the current rows before a filter replaces them.
    pub fn save_filter_base(&mut self) {
        if self.base_rows.is_none() {
            self.base_rows = Some(std::mem::take(&mut self.rows));
        }
    }

    /// Clears the filter and restores the saved rows (expansion intact).
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_matched = None;
        if let Some(base) = self.base_rows.take() {
            self.rows = base;
            self.rows_gen = self.rows_gen.wrapping_add(1);
        }
    }

    /// Applies rows computed off-thread (or inline) for a filter/rebuild job.
    pub fn apply_rows(&mut self, rows: Vec<TreeRow>, matched: Option<usize>) {
        if matched.is_some() {
            self.filter_matched = matched;
        }
        self.rows = rows;
        self.selected = None;
        self.rows_gen = self.rows_gen.wrapping_add(1);
    }
}

/// Initial rows for a fresh index: the root row, expanded, plus its children.
fn initial_rows(index: &TreeIndex) -> Vec<TreeRow> {
    let mut rows = vec![TreeRow {
        node: 0,
        depth: 0,
        parent_row: NONE,
        child_index: 0,
        expanded: index.is_container(0),
    }];
    for (child, child_index) in index.children(0) {
        rows.push(TreeRow {
            node: child,
            depth: 1,
            parent_row: 0,
            child_index,
            expanded: false,
        });
    }
    rows
}

/// Sorts `(id, doc_index)` pairs in case-insensitive **natural** order
/// (`alpha_2` before `alpha_10`, digit runs compared numerically, leading
/// zeros tying), ties broken by document order so the permutation is
/// stable. Pure: called from UI thread and job threads.
fn sorted_pairs(index: &TreeIndex, text: &str, pairs: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut keyed: Vec<(String, u32, u32)> = pairs
        .into_iter()
        .map(|(id, doc_index)| {
            let key = index
                .key_text(text, id)
                .map(|key| key.into_owned())
                .unwrap_or_default();
            (key, doc_index, id)
        })
        .collect();
    keyed.sort_by(|a, b| natural_cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
    keyed
        .into_iter()
        .map(|(_, doc_index, id)| (id, doc_index))
        .collect()
}

/// Case-insensitive natural order: runs of ASCII digits compare numerically
/// (so `alpha_2` < `alpha_10`), every other character compares by its
/// lowercased form, and one string running out first sorts first. Equal by
/// this order (for example `alpha_02` vs `alpha_2`) is a tie the caller
/// breaks, keeping the sort stable.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ia = 0usize;
    let mut ib = 0usize;
    while ia < a.len() && ib < b.len() {
        let (ca, na) = char_at(a, ia);
        let (cb, nb) = char_at(b, ib);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let (a_run, a_end) = digit_run(a, ia);
            let (b_run, b_end) = digit_run(b, ib);
            let at = a_run.trim_start_matches('0');
            let bt = b_run.trim_start_matches('0');
            match at.len().cmp(&bt.len()).then_with(|| at.cmp(bt)) {
                Ordering::Equal => {
                    ia = a_end;
                    ib = b_end;
                }
                other => return other,
            }
        } else {
            match lower_key(ca).cmp(&lower_key(cb)) {
                Ordering::Equal => {
                    ia = na;
                    ib = nb;
                }
                other => return other,
            }
        }
    }
    (a.len() - ia).cmp(&(b.len() - ib))
}

fn char_at(s: &str, byte: usize) -> (char, usize) {
    let c = s[byte..].chars().next().expect("byte index on a boundary");
    (c, byte + c.len_utf8())
}

fn digit_run(s: &str, from: usize) -> (&str, usize) {
    let bytes = s.as_bytes();
    let end = bytes[from..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(s.len(), |offset| from + offset);
    (&s[from..end], end)
}

fn lower_key(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

// ── Off-thread row jobs ────────────────────────────────────────────────────

/// A rows rebuild that may be too big for the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeJob {
    /// Expand every container (the set is collected inside the job).
    ExpandAll,
    /// Re-flatten with the given expanded container ids (sort toggled on a
    /// big doc).
    RebuildSorted(Vec<u32>),
    /// Filter to nodes matching `query`, ancestors kept and auto-expanded.
    Filter(String),
}

impl TreeJob {
    /// Status label while the job runs.
    pub const fn label(&self) -> &'static str {
        match self {
            TreeJob::ExpandAll => "Expanding tree…",
            TreeJob::RebuildSorted(_) => "Sorting tree…",
            TreeJob::Filter(_) => "Filtering tree…",
        }
    }
}

/// Result of a [`TreeJob`].
#[derive(Debug, Clone, PartialEq)]
pub struct RowsOutcome {
    /// The new flattened rows.
    pub rows: Vec<TreeRow>,
    /// Matched-node count (filter jobs only).
    pub matched: Option<usize>,
}

/// Iterative pre-order flatten of the nodes reachable through `expanded`,
/// applying the sort permutation per object when `sort` is on. Returns `None`
/// when cancelled.
fn flatten(
    index: &TreeIndex,
    text: &str,
    expanded: &HashSet<u32>,
    sort: bool,
    cancel: &AtomicBool,
) -> Option<Vec<TreeRow>> {
    let mut rows = Vec::new();
    if index.node_count() == 0 {
        return Some(rows);
    }
    rows.push(TreeRow {
        node: 0,
        depth: 0,
        parent_row: NONE,
        child_index: 0,
        expanded: index.is_container(0) && expanded.contains(&0),
    });
    // Stack of open parent frames, innermost last.
    struct Frame {
        parent_row: u32,
        depth: u16,
        children: Vec<(u32, u32)>,
        next: usize,
    }
    let mut stack: Vec<Frame> = vec![Frame {
        parent_row: 0,
        depth: 1,
        children: ordered_children_pure(index, text, 0, sort),
        next: 0,
    }];
    let mut visited = 0usize;
    while let Some(frame) = stack.last_mut() {
        if frame.next >= frame.children.len() {
            stack.pop();
            continue;
        }
        let (node, child_index) = frame.children[frame.next];
        frame.next += 1;
        visited += 1;
        if visited.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let parent_row = frame.parent_row;
        let depth = frame.depth;
        let expanded_child = index.is_container(node) && expanded.contains(&node);
        let row_index = rows.len() as u32;
        rows.push(TreeRow {
            node,
            depth,
            parent_row,
            child_index,
            expanded: expanded_child,
        });
        if expanded_child {
            let children = ordered_children_pure(index, text, node, sort);
            stack.push(Frame {
                parent_row: row_index,
                depth: depth.saturating_add(1).min(MAX_ROW_DEPTH),
                children,
                next: 0,
            });
        }
    }
    Some(rows)
}

/// Sort-aware children enumeration without the per-tab cache (job threads
/// cannot touch `TreeUi`).
fn ordered_children_pure(index: &TreeIndex, text: &str, node: u32, sort: bool) -> Vec<(u32, u32)> {
    let pairs: Vec<(u32, u32)> = index.children(node);
    if sort && index.kind(node) == NodeKind::Object {
        sorted_pairs(index, text, pairs)
    } else {
        pairs
    }
}

/// Filter: marks matching nodes, keeps ancestors, auto-expands containers
/// with matches below. Two iterative passes over the index; no DOM, no
/// per-node allocation beyond the marks.
fn filter_rows(
    index: &TreeIndex,
    text: &str,
    query: &str,
    sort: bool,
    cancel: &AtomicBool,
) -> Option<(Vec<TreeRow>, usize)> {
    let count = index.node_count();
    let mut matched = vec![false; count];
    let mut show_below = vec![false; count];

    // Pass 1: post-order marks via an explicit child-walk stack.
    struct MarkFrame {
        node: u32,
        children: Vec<(u32, u32)>,
        next: usize,
    }
    let mut stack: Vec<MarkFrame> = vec![MarkFrame {
        node: 0,
        children: ordered_children_pure(index, text, 0, sort),
        next: 0,
    }];
    let mut visited = 0usize;
    while let Some(frame) = stack.last_mut() {
        if frame.next >= frame.children.len() {
            let node = frame.node;
            stack.pop();
            // This node has a shown descendant when any child is shown
            // (matched itself, or shown from below).
            let mut any = false;
            let mut child = index.first_child(node);
            while let Some(c) = child {
                any |= matched[c as usize] || show_below[c as usize];
                child = index.next_sibling(c);
            }
            show_below[node as usize] = any;
            continue;
        }
        visited += 1;
        if visited.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let (node, _) = frame.children[frame.next];
        frame.next += 1;
        if index.is_container(node) {
            // Containers match by key only (their value is a subtree).
            matched[node as usize] = index
                .key_text(text, node)
                .is_some_and(|key| matches_ci(&key, query));
            stack.push(MarkFrame {
                node,
                children: ordered_children_pure(index, text, node, sort),
                next: 0,
            });
        } else {
            matched[node as usize] = node_matches(index, text, node, query);
        }
    }
    let matched_count = matched.iter().filter(|&&m| m).count();
    if !(show_below[0] || matched[0]) {
        return Some((Vec::new(), 0));
    }

    // Pass 2: flatten shown nodes; containers with matches below expand.
    let mut rows = vec![TreeRow {
        node: 0,
        depth: 0,
        parent_row: NONE,
        child_index: 0,
        expanded: show_below[0],
    }];
    struct RowFrame {
        parent_row: u32,
        depth: u16,
        children: Vec<(u32, u32)>,
        next: usize,
    }
    let mut stack: Vec<RowFrame> = vec![RowFrame {
        parent_row: 0,
        depth: 1,
        children: ordered_children_pure(index, text, 0, sort),
        next: 0,
    }];
    while let Some(frame) = stack.last_mut() {
        if frame.next >= frame.children.len() {
            stack.pop();
            continue;
        }
        let (node, child_index) = frame.children[frame.next];
        frame.next += 1;
        if !matched[node as usize] && !show_below[node as usize] {
            continue;
        }
        let parent_row = frame.parent_row;
        let depth = frame.depth;
        let row_index = rows.len() as u32;
        let expanded = index.is_container(node) && show_below[node as usize];
        rows.push(TreeRow {
            node,
            depth,
            parent_row,
            child_index,
            expanded,
        });
        if expanded {
            stack.push(RowFrame {
                parent_row: row_index,
                depth: depth.saturating_add(1).min(MAX_ROW_DEPTH),
                children: ordered_children_pure(index, text, node, sort),
                next: 0,
            });
        }
    }
    Some((rows, matched_count))
}

/// Runs a job. `None` = cancelled.
pub fn run_job(
    index: &TreeIndex,
    text: &str,
    job: TreeJob,
    sort: bool,
    cancel: &AtomicBool,
) -> Option<RowsOutcome> {
    match job {
        TreeJob::ExpandAll => {
            let mut expanded = HashSet::with_capacity(index.node_count() / 4 + 1);
            for node in 0..index.node_count() as u32 {
                if index.is_container(node) {
                    expanded.insert(node);
                }
            }
            flatten(index, text, &expanded, sort, cancel)
                .map(|rows| RowsOutcome { rows, matched: None })
        }
        TreeJob::RebuildSorted(expanded) => {
            let expanded: HashSet<u32> = expanded.into_iter().collect();
            flatten(index, text, &expanded, sort, cancel)
                .map(|rows| RowsOutcome { rows, matched: None })
        }
        TreeJob::Filter(query) => {
            filter_rows(index, text, &query, sort, cancel).map(|(rows, matched)| RowsOutcome {
                rows,
                matched: Some(matched),
            })
        }
    }
}

// ── Cross-thread delivery (AppHandle; no runner edits) ─────────────────────

type Handle = rune::AppHandle<AppState>;

static APP_HANDLE: OnceLock<Mutex<Option<Handle>>> = OnceLock::new();

/// Called once by `main` before `App::run`. Without a handle (tests, the
/// `--headless` CLI) every job runs synchronously instead.
pub fn set_app_handle(handle: Handle) {
    let slot = APP_HANDLE.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(handle);
}

fn app_handle_ref() -> Option<Handle> {
    let slot = APP_HANDLE.get()?;
    let guard = slot.lock().ok()?;
    guard.clone()
}

/// Outcome of an index build.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexOutcome {
    /// The index built.
    Ready(Arc<TreeIndex>),
    /// The document does not parse.
    Failed(TreeError),
}

/// What [`ensure_index`] did.
pub enum IndexSync {
    /// Built synchronously (no app handle): the caller applies this now.
    Sync(IndexOutcome),
    /// Spawned a worker; it delivers [`Msg::TreeIndexDone`] via the handle.
    Spawned,
}

/// Builds the tree index for `text`, off the UI thread when a handle is
/// registered. `make_msg` converts the outcome into the reducer's message
/// (the revision it belongs to is the caller's business); it is consumed
/// either inline or on the worker thread.
pub fn ensure_index(
    text: Arc<str>,
    make_msg: impl FnOnce(IndexOutcome) -> Msg + Send + 'static,
) -> IndexSync {
    let Some(handle) = app_handle_ref() else {
        let outcome = match TreeIndex::build(&text) {
            Ok(index) => IndexOutcome::Ready(Arc::new(index)),
            Err(error) => IndexOutcome::Failed(error),
        };
        // `make_msg` is only needed on the async path; drop it here.
        drop(make_msg);
        return IndexSync::Sync(outcome);
    };
    std::thread::Builder::new()
        .name("json-viewer-tree".into())
        .spawn(move || {
            let outcome = match TreeIndex::build(&text) {
                Ok(index) => IndexOutcome::Ready(Arc::new(index)),
                Err(error) => IndexOutcome::Failed(error),
            };
            // Wakes the event loop; the next poll drains the message.
            handle.update(move |state: &mut AppState, _tx| {
                state.dispatch(make_msg(outcome));
            });
        })
        .expect("spawn tree index thread");
    IndexSync::Spawned
}

/// Cancel flags for per-tab row jobs: starting a new job cancels the old.
static JOB_CANCELS: OnceLock<Mutex<HashMap<TabId, Arc<AtomicBool>>>> = OnceLock::new();

/// Registers a fresh cancel flag for `tab`, cancelling any previous job.
pub fn register_job_cancel(tab: TabId) -> Arc<AtomicBool> {
    let slot = JOB_CANCELS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = slot.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(previous) = map.insert(tab, Arc::new(AtomicBool::new(false))) {
        previous.store(true, Ordering::Relaxed);
    }
    Arc::clone(map.get(&tab).expect("just inserted"))
}

/// What [`dispatch_rows_job`] did.
pub enum RowsDispatch {
    /// Computed synchronously (no app handle): `None` = cancelled.
    Done(Option<RowsOutcome>),
    /// Spawned; it delivers [`Msg::TreeRowsReady`] via the handle.
    Spawned,
}

/// Runs a row job, off the UI thread when a handle is registered.
pub fn dispatch_rows_job(
    tab: TabId,
    gen: u64,
    index: Arc<TreeIndex>,
    text: Arc<str>,
    sort: bool,
    job: TreeJob,
    cancel: Arc<AtomicBool>,
) -> RowsDispatch {
    let Some(handle) = app_handle_ref() else {
        return RowsDispatch::Done(run_job(&index, &text, job, sort, &cancel));
    };
    std::thread::Builder::new()
        .name("json-viewer-tree-rows".into())
        .spawn(move || {
            if let Some(outcome) = run_job(&index, &text, job, sort, &cancel) {
                handle.update(move |state: &mut AppState, _tx| {
                    state.dispatch(Msg::TreeRowsReady {
                        tab,
                        gen,
                        rows: outcome.rows,
                        matched: outcome.matched,
                    });
                });
            }
        })
        .expect("spawn tree rows thread");
    RowsDispatch::Spawned
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"scripts":{"name":"simple-json-viewer","items":[12,"x",true,null]},"n":-4.5,"flag":true,"nothing":null}"#;

    fn doc_index() -> TreeIndex {
        TreeIndex::build(DOC).unwrap()
    }

    #[test]
    fn builds_the_node_skeleton() {
        let index = doc_index();
        // Root + scripts + its 2 members + items + 4 elements + n, flag, nothing.
        assert_eq!(index.node_count(), 11);
        assert_eq!(index.kind(0), NodeKind::Object);
        assert_eq!(index.child_count(0), 4);
        assert!(index.is_container(0));
        // Child order follows the document.
        let children = index.child_ids(0);
        let keys: Vec<String> = children
            .iter()
            .map(|&c| index.key_text(DOC, c).unwrap().into_owned())
            .collect();
        assert_eq!(keys, ["scripts", "n", "flag", "nothing"]);
        // Sibling chain matches the children vector.
        assert_eq!(index.first_child(0), Some(children[0]));
        assert_eq!(index.next_sibling(children[0]), Some(children[1]));
        assert_eq!(index.next_sibling(children[3]), None);
        // Array elements have no key; their kinds and values read back.
        let scripts = children[0];
        let items = index.child_ids(scripts)[1];
        assert_eq!(index.kind(items), NodeKind::Array);
        let element = index.first_child(items).unwrap();
        assert_eq!(index.key_off[element as usize], NONE);
        assert_eq!(index.kind(element), NodeKind::Number);
        assert_eq!(index.value_text(DOC, element), "12");
        let last = index
            .next_sibling(index.next_sibling(index.next_sibling(element).unwrap()).unwrap())
            .unwrap();
        assert_eq!(index.kind(last), NodeKind::Null);
    }

    #[test]
    fn value_ranges_cover_subtrees() {
        let index = doc_index();
        let scripts = index.first_child(0).unwrap();
        let (start, end) = index.value_range(scripts);
        assert_eq!(
            &DOC[start..end],
            r#"{"name":"simple-json-viewer","items":[12,"x",true,null]}"#
        );
        assert_eq!(index.value_range(0).1, DOC.len());
        let name = index.first_child(scripts).unwrap();
        assert_eq!(index.value_text(DOC, name), r#""simple-json-viewer""#);
    }

    #[test]
    fn rejects_broken_documents_with_positions() {
        let err = TreeIndex::build("{\n  \"a\": tru\n}").unwrap_err();
        assert!(err.message.contains("Invalid JSON"), "{}", err.message);
        assert!(err.message.contains("line 2"), "{}", err.message);
        assert!(TreeIndex::build("{\"a\":1} extra").is_err());
        assert!(TreeIndex::build("{\"a\":").is_err());
        assert!(TreeIndex::build("[1,]").is_err());
        assert!(TreeIndex::build("").is_err());
        assert!(TreeIndex::build("{\"a\":01}").is_err());
        assert!(TreeIndex::build(r#"{"a":"unterminated}"#).is_err());
        assert!(TreeIndex::build("[1 2]").is_err());
        assert!(TreeIndex::build(r#"{"a" 1}"#).is_err());
        // Deep nesting must not overflow the stack.
        let deep = format!("{}{}", "[".repeat(50_000), "]".repeat(50_000));
        assert!(TreeIndex::build(&deep).is_ok());
    }

    #[test]
    fn handles_empty_containers_and_scalars() {
        let index = TreeIndex::build(r#"{"a":{},"b":[],"c":[1]}"#).unwrap();
        let a = index.first_child(0).unwrap();
        assert_eq!(index.child_count(a), 0);
        assert_eq!(index.first_child(a), None);
        let b = index.next_sibling(a).unwrap();
        assert_eq!(index.kind(b), NodeKind::Array);
        assert_eq!(index.child_count(b), 0);
        assert_eq!(index.value_text(r#"{"a":{},"b":[],"c":[1]}"#, b), "[]");
        let scalar_root = TreeIndex::build("42").unwrap();
        assert_eq!(scalar_root.node_count(), 1);
        assert_eq!(scalar_root.kind(0), NodeKind::Number);
        assert_eq!(scalar_root.value_text("42", 0), "42");
        let array_root = TreeIndex::build("[1,2]").unwrap();
        assert_eq!(array_root.child_count(0), 2);
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let doc = r#"{"kéy\":\"quoted":"tab\tnewline\n","uni":"😀"}"#;
        let index = TreeIndex::build(doc).unwrap();
        let key_node = index.first_child(0).unwrap();
        // The raw key token `"kéy\",\"quoted"` decodes to a literal quote pair.
        let key = index.key_text(doc, key_node).unwrap().into_owned();
        assert_eq!(key, "kéy\":\"quoted");
        let second = index.next_sibling(key_node).unwrap();
        let decoded = decode_string_token(index.value_text(doc, second));
        assert_eq!(decoded, "😀");
        assert_eq!(display_string("a\"b"), r#""a\"b""#);
        assert_eq!(decode_string_token(r#""plain""#), "plain");
        assert_eq!(decode_string_token(r#""café""#), "café");
    }

    #[test]
    fn filter_keeps_ancestors_and_expands() {
        let index = doc_index();
        let cancel = AtomicBool::new(false);
        let (rows, matched) = filter_rows(&index, DOC, "simple", false, &cancel).unwrap();
        assert_eq!(matched, 1);
        // Root + scripts (ancestor, expanded) + the matching string value.
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .any(|r| index.kind(r.node) == NodeKind::Object && r.expanded));
        let (none_rows, none_matched) = filter_rows(&index, DOC, "zzz", false, &cancel).unwrap();
        assert_eq!(none_matched, 0);
        assert!(none_rows.is_empty());
        // Numbers match by text; booleans and null never do.
        let (_, num_matched) = filter_rows(&index, DOC, "4.5", false, &cancel).unwrap();
        assert_eq!(num_matched, 1);
        let (_, bool_matched) = filter_rows(&index, DOC, "true", false, &cancel).unwrap();
        assert_eq!(bool_matched, 0);
    }

    #[test]
    fn filter_matches_keys_case_insensitively() {
        let doc = r#"{"Alpha":1,"nested":{"alpha":"x"}}"#;
        let index = TreeIndex::build(doc).unwrap();
        let cancel = AtomicBool::new(false);
        let (_, matched) = filter_rows(&index, doc, "alph", false, &cancel).unwrap();
        // Both keys match; the string value "x" does not.
        assert_eq!(matched, 2);
    }

    #[test]
    fn expand_all_rows_cover_every_node() {
        let index = doc_index();
        let cancel = AtomicBool::new(false);
        let outcome = run_job(&index, DOC, TreeJob::ExpandAll, false, &cancel).unwrap();
        assert_eq!(outcome.rows.len(), index.node_count());
        assert!(outcome
            .rows
            .iter()
            .filter(|r| index.is_container(r.node))
            .all(|r| r.expanded));
    }

    #[test]
    fn sort_permutes_objects_only() {
        let doc = r#"{"z":1,"a":2,"m":[3,1,2]}"#;
        let index = TreeIndex::build(doc).unwrap();
        let pairs = index.children(0);
        let sorted = sorted_pairs(&index, doc, pairs);
        let keys: Vec<String> = sorted
            .iter()
            .map(|&(id, _)| index.key_text(doc, id).unwrap().into_owned())
            .collect();
        assert_eq!(keys, ["a", "m", "z"]);
        // Document-order indices survive the permutation.
        assert_eq!(sorted[0].1, 1);
        assert_eq!(sorted[2].1, 0);
        // Arrays keep document order even when sorting.
        let array = index
            .children(0)
            .iter()
            .find(|&&(id, _)| index.key_text(doc, id).as_deref() == Some("m"))
            .map(|&(id, _)| id)
            .unwrap();
        let ordered = ordered_children_pure(&index, doc, array, true);
        let values: Vec<&str> = ordered
            .iter()
            .map(|&(id, _)| index.value_text(doc, id))
            .collect();
        assert_eq!(values, ["3", "1", "2"]);
    }

    #[test]
    fn natural_order_compares_digit_runs_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("alpha_2", "alpha_10"), Less);
        assert_eq!(natural_cmp("alpha_10", "alpha_2"), Greater);
        // Case-insensitive: byte order would put "Bravo" first.
        assert_eq!(natural_cmp("Bravo_1", "alpha_0"), Greater);
        assert_eq!(natural_cmp("Alpha_1", "alpha_2"), Less);
        assert_eq!(natural_cmp("Alpha_1", "alpha_1"), Equal);
        // Leading zeros tie; the caller's stability breaks them.
        assert_eq!(natural_cmp("alpha_02", "alpha_2"), Equal);
        assert_eq!(natural_cmp("alpha_02", "alpha_10"), Less);
        // Prefixes and embedded digits.
        assert_eq!(natural_cmp("abc", "abcd"), Less);
        assert_eq!(natural_cmp("x1y2", "x1y10"), Less);
        assert_eq!(natural_cmp("", ""), Equal);
        assert_eq!(natural_cmp("a", ""), Greater);
    }

    #[test]
    fn sorted_pairs_use_natural_order() {
        let doc = r#"{"zeta_2":1,"zeta_10":2,"Alpha_1":3,"alpha_2":4,"m":5}"#;
        let index = TreeIndex::build(doc).unwrap();
        let sorted = sorted_pairs(&index, doc, index.children(0));
        let keys: Vec<String> = sorted
            .iter()
            .map(|&(id, _)| index.key_text(doc, id).unwrap().into_owned())
            .collect();
        // Case-insensitive and numeric: alpha_2 before zeta_10, not between
        // zeta_10 and zeta_2 the way byte order would place it.
        assert_eq!(keys, ["Alpha_1", "alpha_2", "m", "zeta_2", "zeta_10"]);
    }

    #[test]
    fn node_for_path_resolves_search_paths() {
        let index = doc_index();
        // Object keys, then an array index.
        let element = index
            .node_for_path(DOC, "scripts → items → 1")
            .expect("path resolves");
        assert_eq!(index.kind(element), NodeKind::String);
        assert_eq!(index.value_text(DOC, element), r#""x""#);
        assert!(index.node_for_path(DOC, "scripts → name").is_some());
        // Unknown keys and out-of-range indices do not resolve.
        assert!(index.node_for_path(DOC, "scripts → nope").is_none());
        assert!(index.node_for_path(DOC, "scripts → items → 9").is_none());
        // An empty path is the root; a scalar root resolves too.
        assert_eq!(index.node_for_path(DOC, ""), Some(0));
        let scalar = TreeIndex::build("42").unwrap();
        assert_eq!(scalar.node_for_path("42", ""), Some(0));
    }

    #[test]
    fn selection_paths_use_arrow_format() {
        let index = doc_index();
        let mut tree = TreeUi::new(true);
        tree.install_index(Arc::new(doc_index()), 1);
        // Expand scripts (row 1): rows become root, scripts, name, items, …
        let scripts = tree.rows[1].node;
        assert!(tree.expand(&index, DOC, scripts));
        // Select `name`: the first depth-2 row.
        let name = tree.rows[2].node;
        assert!(tree.select(&index, DOC, name));
        assert_eq!(tree.selected.as_ref().unwrap().path, "scripts → name");
        // Select `items`: the second depth-2 row.
        let items = tree.rows[3].node;
        assert!(tree.select(&index, DOC, items));
        assert_eq!(tree.selected.as_ref().unwrap().path, "scripts → items");
        let (start, end) = index.value_range(items);
        let selection = tree.selected.as_ref().unwrap();
        assert_eq!(selection.value_off as usize, start);
        assert_eq!(selection.value_len as usize, end - start);
        // An array element's path segment is its index.
        assert!(tree.expand(&index, DOC, items));
        let element = tree.rows[4].node;
        assert!(tree.select(&index, DOC, element));
        assert_eq!(
            tree.selected.as_ref().unwrap().path,
            "scripts → items → 0"
        );
        // Selecting an unknown node fails.
        assert!(!tree.select(&index, DOC, 999));
    }

    #[test]
    fn collapse_and_collapse_all_leave_the_root() {
        let index = doc_index();
        let mut tree = TreeUi::new(true);
        tree.install_index(Arc::new(doc_index()), 1);
        let scripts = tree.rows[1].node;
        assert!(tree.expand(&index, DOC, scripts));
        assert!(tree.rows.len() > 5);
        assert!(tree.collapse(scripts));
        assert_eq!(tree.rows.len(), 5); // root + 4 members
        tree.expand(&index, DOC, scripts);
        tree.collapse_all(&index, DOC);
        assert_eq!(tree.rows.len(), 5);
        assert!(tree.rows[0].expanded);
        assert!(tree.rows.iter().skip(1).all(|r| !r.expanded));
        assert_eq!(tree.selected, None);
    }

    #[test]
    fn filter_base_restores_expansion_state() {
        let index = doc_index();
        let mut tree = TreeUi::new(true);
        tree.install_index(Arc::new(doc_index()), 1);
        let scripts = tree.rows[1].node;
        tree.expand(&index, DOC, scripts);
        let expanded_rows = tree.rows.len();
        // A filter replaces the rows; clearing restores the old ones.
        tree.filter = "simple".into();
        tree.save_filter_base();
        let cancel = AtomicBool::new(false);
        let (rows, matched) = filter_rows(&index, DOC, "simple", false, &cancel).unwrap();
        tree.apply_rows(rows, Some(matched));
        assert_eq!(tree.rows.len(), 3);
        tree.clear_filter();
        assert_eq!(tree.rows.len(), expanded_rows);
        assert!(tree.rows.iter().any(|r| r.node == scripts && r.expanded));
    }

    #[test]
    fn rows_version_changes_with_mutations() {
        let mut tree = TreeUi::new(true);
        tree.install_index(Arc::new(doc_index()), 7);
        let before = tree.rows_version();
        let idx = tree.index.clone().unwrap();
        tree.expand(&idx, DOC, tree.rows[1].node);
        assert_ne!(tree.rows_version(), before);
        let stable = tree.rows_version();
        tree.clear_filter();
        assert_eq!(tree.rows_version(), stable);
    }

    #[test]
    fn matches_ci_is_case_insensitive() {
        assert!(matches_ci("Hello World", "hello"));
        assert!(matches_ci("CAFÉ", "café"));
        assert!(matches_ci("hello", "hell"));
        assert!(matches_ci("anything", ""));
        assert!(!matches_ci("hell", "hello"));
        assert!(!matches_ci("greeting", "hello"));
    }

    #[test]
    fn bytes_per_node_is_compact() {
        assert_eq!(TreeIndex::BYTES_PER_NODE, 24);
        let doc = format!(
            "{{{}}}",
            (0..10_000)
                .map(|i| format!(r#""k{i}":{{"v":{i},"nested":[true,false,null]}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let index = TreeIndex::build(&doc).unwrap();
        // 1 root + 10k members × (1 object + v + nested + 3 array elements).
        assert_eq!(index.node_count(), 10_000 * 6 + 1);
    }

    #[test]
    fn register_job_cancel_replaces_the_previous_flag() {
        let first = register_job_cancel(4242);
        let second = register_job_cancel(4242);
        assert!(!second.load(Ordering::Relaxed));
        assert!(first.load(Ordering::Relaxed));
    }
}
