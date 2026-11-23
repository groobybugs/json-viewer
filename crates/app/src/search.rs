//! JSON search, ported from `searchInObject` in the old Web Worker
//! (`src/workers/jsonWorker.ts`) and the result rendering of
//! `src/components/SearchBar.tsx`.
//!
//! Semantics kept from the TypeScript version:
//!
//! * object keys and string values match case-insensitively by default;
//! * numbers match when their decimal text contains the query (always
//!   case-insensitive in practice, since the needle is lowercased once);
//! * booleans and `null` never match;
//! * arrays are walked like objects, with their indices as path segments;
//! * a hit is recorded for a matching key and, separately, for a matching
//!   value, so a key whose value also matches produces two hits;
//! * the path is joined with ` → `, the way the old results panel renders
//!   `result.path.join(' → ')`;
//! * the preview follows the old panel too: strings verbatim, everything
//!   else as compact JSON (`JSON.stringify(result.value)`).
//!
//! The walk is streaming: `serde_json` parse events are visited as they
//! arrive, so no syntax tree of the document is built and memory stays
//! proportional to the nesting depth plus the hit list, which keeps a
//! 100 MB document affordable. Two consequences of streaming are
//! documented here: duplicate object keys are visited as often as they
//! appear (a tree or a JS object would keep only the last one), and a
//! reached result cap keeps serializing the previews it already opened
//! (so the boundary hit matches the tree-based walk), while a cancel
//! drops a key hit whose preview has not finished instead of half-filling
//! it — the results of a cancelled walk are discarded anyway.
//!
//! `SearchHit::line` points at the value's line in the pretty-printed
//! document (1-based), so the UI can jump to it once formatting has run.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as _, Serialize};

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Display path, joined with ` → ` (for example `users → 0 → name`).
    pub path: String,
    /// Last path segment, when it came from an object key or array index.
    pub key: Option<String>,
    /// Compact rendering of the value: strings verbatim, everything else as
    /// JSON text.
    pub value_preview: String,
    /// 1-based line in the pretty-printed document, when it could be located.
    pub line: Option<usize>,
}

/// Search tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Match keys and string values with exact case. Defaults to `false`,
    /// matching the original worker's `toLowerCase()` comparison.
    pub case_sensitive: bool,
    /// Stop after this many hits. `0` means unlimited; defaults to 1000.
    pub max_results: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            max_results: 1000,
        }
    }
}

/// Searches a JSON document for `query`.
///
/// The document is visited as `serde_json` parse events: no syntax tree of
/// the input is built, so the scan stays fast and small on very large
/// documents. `cancel` is checked before every entry, so another thread
/// can abandon a walk over a 100 MB document at entry granularity; the
/// hits gathered until then are returned. Invalid JSON (including
/// truncated input and trailing characters) returns no hits — parse
/// failures are the formatter's job to report.
pub fn search(
    input: &str,
    query: &str,
    opts: SearchOptions,
    cancel: &AtomicBool,
) -> Vec<SearchHit> {
    if query.is_empty() || cancel.load(Ordering::SeqCst) {
        return Vec::new();
    }
    let needle = if opts.case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let mut searcher = Searcher::new(&needle, opts, cancel);
    let mut de = serde_json::Deserializer::from_str(input);
    let outcome = (&mut de)
        .deserialize_any(ValVisitor {
            s: &mut searcher,
            entry_line: 0,
        })
        .and_then(|()| de.end());
    if outcome.is_err() && !searcher.aborted {
        // serde_json rejects grammar the application's hand-rolled parser
        // deliberately accepts (lone surrogates, arbitrary-size numbers and
        // deep nesting). Reuse the tree tokenizer for that exceptional path;
        // the ordinary 100 MB scan never pays for a second parser or a DOM.
        if crate::json_ops::validate(input).is_err() {
            return Vec::new();
        }
        return search_tree_fallback(input, &needle, opts, cancel);
    }
    searcher.hits
}

/// Non-recursive walk over the application's own JSON grammar. Tree nodes are
/// already in document order; explicit events preserve the serde walk's path,
/// pretty-line, preview-capture, cancellation and result-cap semantics.
fn search_tree_fallback(
    input: &str,
    needle: &str,
    opts: SearchOptions,
    cancel: &AtomicBool,
) -> Vec<SearchHit> {
    use crate::tree_index::{NodeKind, TreeIndex};

    let Ok(index) = TreeIndex::build(input) else {
        // This is reachable only if the validator and tree tokenizer drift
        // apart. Never disguise that as "No matches": raw hits explicitly
        // identify themselves as an unstructured fallback in the panel.
        return raw_substring_fallback(input, needle, opts, cancel);
    };
    let mut s = Searcher::new(needle, opts, cancel);

    enum Event {
        Entry {
            node: u32,
            key: String,
            quoted_key: Option<String>,
        },
        Value {
            node: u32,
            entry_line: usize,
            pop_path: bool,
        },
        Close {
            node: u32,
            close: char,
        },
        Finish { pop_path: bool },
    }

    let mut events = vec![Event::Value {
        node: 0,
        entry_line: 0,
        pop_path: false,
    }];
    let outcome: Result<(), serde_json::Error> = (|| {
        while let Some(event) = events.pop() {
            match event {
                Event::Entry {
                    node,
                    key,
                    quoted_key,
                } => {
                    let entry_line = s.enter_entry(&key)?;
                    if !s.captures.is_empty() {
                        s.capture_entry(quoted_key.as_deref());
                    }
                    events.push(Event::Value {
                        node,
                        entry_line: entry_line.unwrap_or(0),
                        pop_path: entry_line.is_some(),
                    });
                }
                Event::Value {
                    node,
                    entry_line,
                    pop_path,
                } => {
                    match index.kind(node) {
                        NodeKind::String => {
                            let decoded = crate::tree_index::decode_string_token(
                                index.value_text(input, node),
                            );
                            if !s.captures.is_empty() {
                                let quoted =
                                    serde_json::to_string(decoded.as_ref()).unwrap_or_default();
                                for cap in &mut s.captures {
                                    cap.buf.push_str(&quoted);
                                }
                            }
                            if let Some(hit) = s.pending.take() {
                                s.hits[hit].value_preview = decoded.as_ref().to_owned();
                            }
                            if !s.serialize_only
                                && !s.path.is_empty()
                                && matches(&decoded, s.needle, s.opts.case_sensitive)
                            {
                                s.push_value_hit(entry_line, &decoded);
                            }
                        }
                        NodeKind::Number => {
                            s.number_text(entry_line, index.value_text(input, node));
                        }
                        NodeKind::Bool | NodeKind::Null => {
                            let text = index.value_text(input, node);
                            for cap in &mut s.captures {
                                cap.buf.push_str(text);
                            }
                            if let Some(hit) = s.pending.take() {
                                s.hits[hit].value_preview = text.to_owned();
                            }
                        }
                        NodeKind::Object | NodeKind::Array => {
                            s.container_checks()?;
                            let object = index.kind(node) == NodeKind::Object;
                            s.open_container(if object { '{' } else { '[' });
                            events.push(Event::Finish { pop_path });
                            events.push(Event::Close {
                                node,
                                close: if object { '}' } else { ']' },
                            });
                            for (child_index, child) in
                                index.child_ids(node).into_iter().enumerate().rev()
                            {
                                let key = if object {
                                    index
                                        .key_text(input, child)
                                        .expect("object child has a key")
                                        .into_owned()
                                } else {
                                    child_index.to_string()
                                };
                                let quoted_key = object
                                    .then(|| serde_json::to_string(&key).unwrap_or_default());
                                events.push(Event::Entry {
                                    node: child,
                                    key,
                                    quoted_key,
                                });
                            }
                            continue;
                        }
                    }
                    if pop_path {
                        s.path.pop();
                    }
                }
                Event::Close { node, close } => {
                    if index.child_count(node) > 0 {
                        s.line += 1;
                    }
                    s.close_container(close);
                }
                Event::Finish { pop_path } => {
                    if pop_path {
                        s.path.pop();
                    }
                }
            }
        }
        Ok(())
    })();
    if outcome.is_err() && !s.aborted {
        return raw_substring_fallback(input, needle, opts, cancel);
    }
    s.hits
}

fn raw_substring_fallback(
    input: &str,
    needle: &str,
    opts: SearchOptions,
    cancel: &AtomicBool,
) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if cancel.load(Ordering::SeqCst)
            || (opts.max_results != 0 && hits.len() >= opts.max_results)
        {
            break;
        }
        if matches(line, needle, opts.case_sensitive) {
            hits.push(SearchHit {
                path: format!(
                    "Raw text match (structured search unavailable) · line {}",
                    index + 1
                ),
                key: None,
                value_preview: line.trim().to_owned(),
                line: Some(index + 1),
            });
        }
    }
    hits
}

fn matches(text: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        text.contains(needle)
    } else {
        text.to_lowercase().contains(needle)
    }
}

/// Streaming walk state.
struct Searcher<'a> {
    needle: &'a str,
    opts: SearchOptions,
    cancel: &'a AtomicBool,
    /// Path segments of the entry currently being visited.
    path: Vec<String>,
    /// 0-based line of the most recent entry in the pretty-printed document.
    line: usize,
    hits: Vec<SearchHit>,
    /// Set when the walk stopped early (cancel or result cap); the hits
    /// gathered until then are returned.
    aborted: bool,
    /// Set once the result cap is reached while a preview is still open:
    /// the walk then only finishes serializing those previews, without
    /// recording further hits.
    serialize_only: bool,
    /// Hit index of a key match whose value preview is still open.
    pending: Option<usize>,
    /// Active subtree serializers, outermost first.
    captures: Vec<Capture>,
}

/// Serializes one matching key's container value as compact JSON, the way
/// `JSON.stringify(result.value)` rendered it in the old results panel.
struct Capture {
    buf: String,
    /// Per open container: whether an entry was emitted (comma placement).
    stack: Vec<bool>,
    /// Hit index whose preview this buffer fills.
    placeholder: usize,
}

impl<'a> Searcher<'a> {
    fn new(needle: &'a str, opts: SearchOptions, cancel: &'a AtomicBool) -> Self {
        Self {
            needle,
            opts,
            cancel,
            path: Vec::new(),
            line: 0,
            hits: Vec::new(),
            aborted: false,
            serialize_only: false,
            pending: None,
            captures: Vec::new(),
        }
    }
}

impl Searcher<'_> {
    /// True when every open preview has been filled.
    fn serialize_finished(&self) -> bool {
        self.pending.is_none() && self.captures.is_empty()
    }

    /// Shared stop gate: a cancel aborts on the spot; a reached result
    /// cap either switches to serialize-only mode (an open preview must
    /// still be completed, exactly like the tree-based walk's boundary
    /// hit) or stops the walk outright. Returns `true` for serialize-only.
    fn gate<E: DeError>(&mut self) -> Result<bool, E> {
        if self.cancel.load(Ordering::SeqCst) {
            self.abort();
            return Err(E::custom("search cancelled"));
        }
        if self.opts.max_results != 0 && self.hits.len() >= self.opts.max_results {
            if !self.serialize_finished() {
                self.serialize_only = true;
                return Ok(true);
            }
            self.aborted = true;
            return Err(E::custom("result limit reached"));
        }
        Ok(false)
    }

    /// Container-entry gate.
    fn container_checks<E: DeError>(&mut self) -> Result<(), E> {
        if self.serialize_only {
            if self.cancel.load(Ordering::SeqCst) {
                self.abort();
                return Err(E::custom("search cancelled"));
            }
            if self.serialize_finished() {
                self.aborted = true;
                return Err(E::custom("result limit reached"));
            }
            return Ok(());
        }
        self.gate().map(|_| ())
    }

    /// Child-entry gate: `Ok(Some(line))` records hits for the entry,
    /// `Ok(None)` visits it silently (serialize-only), `Err` stops the walk.
    fn enter_entry<E: DeError>(&mut self, key: &str) -> Result<Option<usize>, E> {
        if self.serialize_only {
            if self.cancel.load(Ordering::SeqCst) {
                self.abort();
                return Err(E::custom("search cancelled"));
            }
            if self.serialize_finished() {
                self.aborted = true;
                return Err(E::custom("result limit reached"));
            }
            return Ok(None);
        }
        if self.gate()? {
            return Ok(None);
        }
        self.line += 1;
        let entry_line = self.line;
        self.path.push(key.to_owned());
        if matches(key, self.needle, self.opts.case_sensitive) {
            self.push_key_hit(entry_line, key);
        }
        Ok(Some(entry_line))
    }

    /// Stops the walk, dropping hits whose preview could not be completed.
    fn abort(&mut self) {
        self.aborted = true;
        let mut unfilled: Vec<usize> = self.captures.iter().map(|c| c.placeholder).collect();
        if let Some(index) = self.pending.take() {
            unfilled.push(index);
        }
        unfilled.sort_unstable();
        for index in unfilled.into_iter().rev() {
            self.hits.remove(index);
        }
    }

    /// Records a placeholder hit for a matching key; the preview is filled
    /// once the value has been visited.
    fn push_key_hit(&mut self, entry_line: usize, key: &str) {
        self.hits.push(SearchHit {
            path: self.path.join(" → "),
            key: Some(key.to_owned()),
            value_preview: String::new(),
            line: Some(entry_line + 1),
        });
        self.pending = Some(self.hits.len() - 1);
    }

    /// Records a hit for a matching scalar value.
    fn push_value_hit(&mut self, entry_line: usize, preview: &str) {
        self.hits.push(SearchHit {
            path: self.path.join(" → "),
            key: self.path.last().cloned(),
            value_preview: preview.to_owned(),
            line: Some(entry_line + 1),
        });
    }

    /// Capture-side entry emit: the container comma, then `"key":` for
    /// object entries.
    fn capture_entry(&mut self, quoted_key: Option<&str>) {
        for cap in &mut self.captures {
            if let Some(flag) = cap.stack.last_mut() {
                if *flag {
                    cap.buf.push(',');
                } else {
                    *flag = true;
                }
            }
            if let Some(quoted) = quoted_key {
                cap.buf.push_str(quoted);
                cap.buf.push(':');
            }
        }
    }

    /// Opens a container: existing captures grow a level, and a matching
    /// key's container value starts its own serializer.
    fn open_container(&mut self, open: char) {
        for cap in &mut self.captures {
            cap.buf.push(open);
            cap.stack.push(false);
        }
        if let Some(placeholder) = self.pending.take() {
            self.captures.push(Capture {
                buf: open.to_string(),
                stack: vec![false],
                placeholder,
            });
        }
    }

    /// Closes a container: existing captures drop a level, and serializers
    /// whose container ended fill their hit preview.
    fn close_container(&mut self, close: char) {
        for cap in &mut self.captures {
            cap.buf.push(close);
            cap.stack.pop();
        }
        let mut i = 0;
        while i < self.captures.len() {
            if self.captures[i].stack.is_empty() {
                let cap = self.captures.remove(i);
                self.hits[cap.placeholder].value_preview = cap.buf;
            } else {
                i += 1;
            }
        }
    }

    /// Scalar number bookkeeping: capture emit, placeholder fill, value hit.
    fn number_text(&mut self, entry_line: usize, text: &str) {
        for cap in &mut self.captures {
            cap.buf.push_str(text);
        }
        if let Some(index) = self.pending.take() {
            self.hits[index].value_preview = text.to_owned();
        }
        if !self.serialize_only
            && !self.path.is_empty()
            && text.contains(self.needle)
        {
            self.push_value_hit(entry_line, text);
        }
    }
}

/// Deserializes one map value (or the document root).
struct ValueSeed<'s, 'a> {
    s: &'s mut Searcher<'a>,
    entry_line: usize,
}

impl<'de, 's, 'a> DeserializeSeed<'de> for ValueSeed<'s, 'a> {
    type Value = ();

    fn deserialize<D>(self, de: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        de.deserialize_any(ValVisitor {
            s: self.s,
            entry_line: self.entry_line,
        })
    }
}

/// Deserializes one array element: its index is the path segment.
struct ElementSeed<'s, 'a> {
    s: &'s mut Searcher<'a>,
    index: usize,
}

impl<'de, 's, 'a> DeserializeSeed<'de> for ElementSeed<'s, 'a> {
    type Value = ();

    fn deserialize<D>(self, de: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = self.s;
        let key = self.index.to_string();
        let entry_line = s.enter_entry(&key)?;
        if !s.captures.is_empty() {
            s.capture_entry(None);
        }
        let outcome = de.deserialize_any(ValVisitor {
            s: &mut *s,
            entry_line: entry_line.unwrap_or(0),
        });
        if entry_line.is_some() {
            s.path.pop();
        }
        outcome
    }
}

/// Visits one value: scalars record hits, containers keep walking.
struct ValVisitor<'s, 'a> {
    s: &'s mut Searcher<'a>,
    entry_line: usize,
}

impl<'de, 's, 'a> Visitor<'de> for ValVisitor<'s, 'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_str<E>(self, v: &str) -> Result<(), E>
    where
        E: DeError,
    {
        let s = self.s;
        if !s.captures.is_empty() {
            let quoted = serde_json::to_string(v).unwrap_or_default();
            for cap in &mut s.captures {
                cap.buf.push_str(&quoted);
            }
        }
        if let Some(index) = s.pending.take() {
            s.hits[index].value_preview = v.to_owned();
        }
        if !s.serialize_only && !s.path.is_empty() && matches(v, s.needle, s.opts.case_sensitive) {
            s.push_value_hit(self.entry_line, v);
        }
        Ok(())
    }

    fn visit_u64<E>(self, v: u64) -> Result<(), E>
    where
        E: DeError,
    {
        let text = v.to_string();
        self.s.number_text(self.entry_line, &text);
        Ok(())
    }

    fn visit_i64<E>(self, v: i64) -> Result<(), E>
    where
        E: DeError,
    {
        let text = v.to_string();
        self.s.number_text(self.entry_line, &text);
        Ok(())
    }

    fn visit_f64<E>(self, v: f64) -> Result<(), E>
    where
        E: DeError,
    {
        // Integers outside serde_json's i64/u64 storage are silently routed
        // through f64 (and may be rounded) instead of producing a parse error.
        // Reject that lossy representation so the caller retries with the
        // application's raw-number tokenizer. In-range floating-point values
        // keep the original fast path and formatting.
        if v <= i64::MIN as f64 || v >= u64::MAX as f64 {
            return Err(E::custom("number requires the raw-token search walk"));
        }
        // Format like `serde_json::Value` would, so previews and matches are
        // identical to the tree-based scaffold.
        let text = serde_json::Number::from_f64(v)
            .map(|number| number.to_string())
            .unwrap_or_else(|| v.to_string());
        self.s.number_text(self.entry_line, &text);
        Ok(())
    }

    fn visit_bool<E>(self, v: bool) -> Result<(), E>
    where
        E: DeError,
    {
        let s = self.s;
        let text = if v { "true" } else { "false" };
        for cap in &mut s.captures {
            cap.buf.push_str(text);
        }
        if let Some(index) = s.pending.take() {
            s.hits[index].value_preview = text.to_owned();
        }
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E>
    where
        E: DeError,
    {
        let s = self.s;
        for cap in &mut s.captures {
            cap.buf.push_str("null");
        }
        if let Some(index) = s.pending.take() {
            s.hits[index].value_preview = "null".to_owned();
        }
        Ok(())
    }

    fn visit_map<A>(self, mut access: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let s = self.s;
        s.container_checks()?;
        s.open_container('{');
        let mut children = 0usize;
        while let Some(key) = access.next_key::<String>()? {
            children += 1;
            let entry_line = s.enter_entry(&key)?;
            if !s.captures.is_empty() {
                let quoted = serde_json::to_string(&key).unwrap_or_default();
                s.capture_entry(Some(&quoted));
            }
            access.next_value_seed(ValueSeed {
                s: &mut *s,
                entry_line: entry_line.unwrap_or(0),
            })?;
            if entry_line.is_some() {
                s.path.pop();
            }
        }
        if children > 0 {
            s.line += 1;
        }
        s.close_container('}');
        Ok(())
    }

    fn visit_seq<A>(self, mut access: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        let s = self.s;
        s.container_checks()?;
        s.open_container('[');
        let mut children = 0usize;
        let mut index = 0usize;
        while access
            .next_element_seed(ElementSeed {
                s: &mut *s,
                index,
            })?
            .is_some()
        {
            children += 1;
            index += 1;
        }
        if children > 0 {
            s.line += 1;
        }
        s.close_container(']');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn run(input: &str, query: &str) -> Vec<SearchHit> {
        search(
            input,
            query,
            SearchOptions::default(),
            &AtomicBool::new(false),
        )
    }

    // --- rules ported from searchInObject in src/workers/jsonWorker.ts ---

    #[test]
    fn matches_keys_and_string_values_case_insensitively() {
        // key.toLowerCase().includes(lowerQuery), and the same for string
        // values.
        let hits = run(r#"{"Alpha":1,"nested":{"alpha":"x"}}"#, "alph");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "Alpha");
        assert_eq!(hits[1].path, "nested → alpha");
        assert_eq!(hits[1].key.as_deref(), Some("alpha"));
    }

    #[test]
    fn matches_string_values_but_not_booleans_or_null() {
        // typeof value === 'string' matches; booleans and null fall through
        // every branch and never match.
        let hits = run(
            r#"{"greeting":"Hello World","on":true,"none":null}"#,
            "hello",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value_preview, "Hello World");
        assert!(run(r#"{"on":true,"none":null}"#, "true").is_empty());
        assert!(run(r#"{"on":true,"none":null}"#, "null").is_empty());
    }

    #[test]
    fn matches_numbers_by_text() {
        // typeof value === 'number' && value.toString().includes(query).
        let hits = run(r#"{"n":142,"m":42.5}"#, "42");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].value_preview, "142");
        assert_eq!(hits[1].value_preview, "42.5");
    }

    #[test]
    fn case_sensitive_option_is_honoured() {
        // Rust extension: opt into the exact-case comparison the worker
        // never had.
        let opts = SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        };
        let cancel = AtomicBool::new(false);
        let hits = search(r#"{"Alpha":"alpha"}"#, "alpha", opts, &cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value_preview, "alpha");
    }

    #[test]
    fn arrays_use_index_segments_and_record_lines() {
        // Object.entries(array) yields index keys, so the path carries them
        // and the results panel renders "a → 0 → b".
        let input = r#"{"a":[{"b":"needle"}]}"#;
        let hits = run(input, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a → 0 → b");
        assert_eq!(hits[0].key.as_deref(), Some("b"));
        assert!(hits[0].line.is_some());
    }

    #[test]
    fn key_and_value_hits_are_separate() {
        // The worker pushes a key-match hit and a value-match hit in two
        // consecutive ifs, so both match = two identical rows.
        let hits = run(r#"{"needle":"needle"}"#, "needle");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], hits[1]);
    }

    #[test]
    fn object_and_array_previews_are_compact_json() {
        // SearchBar.tsx renders typeof value === 'object' with
        // JSON.stringify(value); strings render verbatim via String(value).
        let hits = run(r#"{"data":{"z":1,"a":[2,3]}}"#, "data");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value_preview, r#"{"z":1,"a":[2,3]}"#);
        let hits = run(r#"{"items":[{"id":7}]}"#, "items");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value_preview, r#"[{"id":7}]"#);
    }

    #[test]
    fn nested_container_key_hits_get_their_own_preview() {
        let hits = run(r#"{"a":{"a":{"b":1}}}"#, "a");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "a");
        assert_eq!(hits[0].value_preview, r#"{"a":{"b":1}}"#);
        assert_eq!(hits[1].path, "a → a");
        assert_eq!(hits[1].value_preview, r#"{"b":1}"#);
    }

    #[test]
    fn empty_containers_preview_as_braces() {
        assert_eq!(run(r#"{"a":{}}"#, "a")[0].value_preview, "{}");
        assert_eq!(run(r#"{"b":[]}"#, "b")[0].value_preview, "[]");
    }

    #[test]
    fn tree_fallback_matches_normal_walk_previews_lines_and_caps() {
        let fallback = r#"{"other":"\uD800","a":{"a":[1,true,null,"needle"]}}"#;
        let normal = r#"{"other":"ok","a":{"a":[1,true,null,"needle"]}}"#;
        for query in ["a", "needle", "1", "true"] {
            for max_results in [0, 1, 2, 1000] {
                let opts = SearchOptions {
                    max_results,
                    ..SearchOptions::default()
                };
                let cancel = AtomicBool::new(false);
                assert_eq!(
                    search(fallback, query, opts, &cancel),
                    search(normal, query, opts, &cancel),
                    "query {query:?}, cap {max_results}"
                );
            }
        }
    }

    #[test]
    fn raw_fallback_labels_its_status_and_matches_case_insensitively() {
        let hits = raw_substring_fallback(
            "first\nNeedle here",
            "needle",
            SearchOptions::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("structured search unavailable"));
        assert_eq!(hits[0].line, Some(2));
    }

    #[test]
    fn max_results_caps_the_walk() {
        // Rust extension: SearchOptions.max_results (default 1000).
        let opts = SearchOptions {
            max_results: 2,
            ..SearchOptions::default()
        };
        let cancel = AtomicBool::new(false);
        let hits = search(r#"["a","a","a","a"]"#, "a", opts, &cancel);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn cap_is_checked_per_entry_like_the_scaffold() {
        // The check happens once per entry, so a key hit plus a value hit
        // of the same entry may overshoot the cap by one.
        let opts = SearchOptions {
            max_results: 1,
            ..SearchOptions::default()
        };
        let cancel = AtomicBool::new(false);
        let hits = search(r#"{"a":"a"}"#, "a", opts, &cancel);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn unlimited_results_when_max_is_zero() {
        let doc = (0..2500)
            .map(|i| format!("\"hit-{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let doc = format!("[{doc}]");
        let opts = SearchOptions {
            max_results: 0,
            ..SearchOptions::default()
        };
        let hits = search(&doc, "hit", opts, &AtomicBool::new(false));
        assert_eq!(hits.len(), 2500);
    }

    #[test]
    fn invalid_json_returns_no_hits() {
        // The worker caught the parse error and posted no results.
        assert!(run("{", "x").is_empty());
        assert!(run("", "x").is_empty());
    }

    #[test]
    fn truncated_json_returns_no_hits() {
        // Matches serde_json::from_str::<Value>: a partial document is a
        // parse failure, so no partial hits either.
        assert!(run(r#"{"a":"needle""#, "needle").is_empty());
    }

    #[test]
    fn trailing_characters_drop_hits() {
        // from_str rejects trailing garbage; end() reproduces that.
        assert!(run(r#"{"a":"needle"} trailing"#, "needle").is_empty());
    }

    #[test]
    fn root_scalar_produces_no_hits() {
        // searchInObject only iterates entries of the root; scalars have
        // none.
        assert!(run(r#""needle""#, "needle").is_empty());
        assert!(run("42", "4").is_empty());
    }

    #[test]
    fn duplicate_keys_are_both_visited() {
        // The tree-based scaffold (and JSON.parse) kept only the last
        // duplicate; a streaming scan sees the raw document, so both are
        // reported.
        let hits = run(r#"{"a":1,"a":"x"}"#, "a");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].value_preview, "1");
        assert_eq!(hits[1].value_preview, "x");
    }

    #[test]
    fn empty_query_returns_no_hits() {
        // if (!searchQuery) return [].
        assert!(run(r#"{"a":1}"#, "").is_empty());
    }

    #[test]
    fn cancel_before_the_walk_returns_nothing() {
        let cancel = AtomicBool::new(true);
        let hits = search(r#"{"a":"a"}"#, "a", SearchOptions::default(), &cancel);
        assert!(hits.is_empty());
    }

    #[test]
    fn pretty_lines_point_at_values() {
        let input = r#"{"a":1,"b":{"c":"needle"}}"#;
        let hit = run(input, "needle").pop().unwrap();
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(input).unwrap())
                .unwrap();
        let line = pretty.lines().nth(hit.line.unwrap() - 1).unwrap();
        assert!(line.contains("needle"), "{line:?}");
    }

    // --- scale behavior: cancellation and 100 MB timing ---

    /// Builds a JSON array of ~1.8 KB records until it reaches
    /// `target_bytes`; every record's `name` contains "needle".
    fn fixture(target_bytes: usize) -> (String, usize) {
        let note = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(28);
        let mut doc = String::with_capacity(target_bytes + 4096);
        doc.push('[');
        let mut records = 0usize;
        while doc.len() < target_bytes {
            write!(
                doc,
                r#"{{"id":{records},"name":"needle-user-{records}","email":"user-{records}@example.test","tags":["alpha","beta"],"note":"{note}","active":true}}, "#
            )
            .unwrap();
            records += 1;
        }
        doc.pop();
        doc.pop();
        doc.push(']');
        (doc, records)
    }

    #[test]
    fn cancel_mid_walk_aborts_promptly() {
        let (doc, records) = fixture(100 * 1024 * 1024);
        let cancel = AtomicBool::new(false);
        thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let (doc, cancel) = (&doc, &cancel);
            scope.spawn(move || {
                let opts = SearchOptions {
                    max_results: 0,
                    ..SearchOptions::default()
                };
                let hits = search(doc, "needle", opts, cancel);
                tx.send(hits.len()).unwrap();
            });
            thread::sleep(Duration::from_millis(10));
            cancel.store(true, Ordering::SeqCst);
            let seen = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("search ignored the cancel flag");
            assert!(
                seen < records,
                "walk finished before the cancel landed: {seen}/{records}"
            );
        });
    }

    #[test]
    #[ignore = "generates a 100 MB fixture; run with cargo test --release -- --ignored --nocapture"]
    fn hundred_megabyte_scan_timing() {
        let (doc, records) = fixture(100 * 1024 * 1024);
        let cancel = AtomicBool::new(false);
        let unlimited = SearchOptions {
            max_results: 0,
            ..SearchOptions::default()
        };

        let start = Instant::now();
        let hits = search(&doc, "zzz-not-present", unlimited, &cancel);
        let miss_scan = start.elapsed();
        assert!(hits.is_empty());

        let start = Instant::now();
        let hits = search(&doc, "needle", unlimited, &cancel);
        let match_scan = start.elapsed();
        assert_eq!(hits.len(), records);

        let start = Instant::now();
        let hits = search(&doc, "needle", SearchOptions::default(), &cancel);
        let capped = start.elapsed();
        assert_eq!(hits.len(), 1000);

        let mib = doc.len() as f64 / 1024.0 / 1024.0;
        println!(
            "search over {mib:.0} MB / {records} records: miss scan {} ms ({:.0} MB/s), match-all scan {} ms ({} hits), capped-at-1000 {} ms",
            miss_scan.as_millis(),
            mib / miss_scan.as_secs_f64(),
            match_scan.as_millis(),
            records,
            capped.as_millis(),
        );
        assert!(miss_scan.as_secs() < 60, "scan hung");
    }

    // Parity harness: the same tree-based walk the scaffold used, kept as a
    // reference. The streaming implementation above must produce exactly
    // its output — paths, previews, lines, order, and cap behavior — for
    // arbitrary documents; this fuzz asserts that over generated trees.
    mod differential {
        use super::*;
        use std::collections::HashMap;
        use serde_json::Value;

        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state >> 33
        }

        fn scalar(state: &mut u64) -> Value {
            match lcg(state) % 8 {
                0 => Value::String("needle".into()),
                1 => Value::String("NeeDle xa".into()),
                2 => Value::String("nëedle ü".into()),
                3 => Value::String(format!("side {}", lcg(state) % 100)),
                4 => Value::Number((lcg(state) % 5000).into()),
                5 => serde_json::Number::from_f64(lcg(state) as f64 / 8.0)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                6 => Value::Bool(lcg(state).is_multiple_of(2)),
                _ => Value::Null,
            }
        }

        fn rand_json(state: &mut u64, depth: usize, keys: &mut usize) -> Value {
            if depth == 0 || lcg(state).is_multiple_of(5) {
                return scalar(state);
            }
            if lcg(state).is_multiple_of(2) {
                Value::Array(
                    (0..lcg(state) % 4)
                        .map(|_| rand_json(state, depth - 1, keys))
                        .collect(),
                )
            } else {
                Value::Object(
                    (0..lcg(state) % 4)
                        .map(|_| {
                            *keys += 1;
                            let key = if lcg(state).is_multiple_of(4) {
                                format!("needle-key-{keys}")
                            } else {
                                format!("k{keys}")
                            };
                            (key, rand_json(state, depth - 1, keys))
                        })
                        .collect(),
                )
            }
        }

        fn join_path(path: &[String]) -> String {
            path.join(" → ")
        }

        fn preview(value: &Value) -> String {
            match value {
                Value::String(text) => text.clone(),
                _ => serde_json::to_string(value).unwrap_or_else(|_| String::from("<x>")),
            }
        }

        fn ref_hit(
            path: &[String],
            key: Option<String>,
            value: &Value,
            lines: &HashMap<String, usize>,
        ) -> SearchHit {
            let joined = join_path(path);
            SearchHit {
                line: lines.get(&joined).map(|line| line + 1),
                path: joined,
                key,
                value_preview: preview(value),
            }
        }

        fn ref_matches(text: &str, needle: &str, case_sensitive: bool) -> bool {
            if case_sensitive {
                text.contains(needle)
            } else {
                text.to_lowercase().contains(needle)
            }
        }

        fn ref_value_matches(value: &Value, needle: &str, case_sensitive: bool) -> bool {
            match value {
                Value::String(text) => ref_matches(text, needle, case_sensitive),
                Value::Number(number) => number.to_string().contains(needle),
                _ => false,
            }
        }

        fn index_value(
            value: &Value,
            path: &mut Vec<String>,
            line: &mut usize,
            map: &mut HashMap<String, usize>,
        ) {
            map.entry(join_path(path)).or_insert(*line);
            match value {
                Value::Object(object) if !object.is_empty() => {
                    for (key, child) in object {
                        *line += 1;
                        path.push(key.clone());
                        map.entry(join_path(path)).or_insert(*line);
                        index_value(child, path, line, map);
                        path.pop();
                    }
                    *line += 1;
                }
                Value::Array(items) if !items.is_empty() => {
                    for (index, child) in items.iter().enumerate() {
                        *line += 1;
                        path.push(index.to_string());
                        map.entry(join_path(path)).or_insert(*line);
                        index_value(child, path, line, map);
                        path.pop();
                    }
                    *line += 1;
                }
                _ => {}
            }
        }

        fn ref_walk(
            value: &Value,
            path: &mut Vec<String>,
            needle: &str,
            opts: SearchOptions,
            cancel: &AtomicBool,
            lines: &HashMap<String, usize>,
            hits: &mut Vec<SearchHit>,
        ) -> bool {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            if opts.max_results > 0 && hits.len() >= opts.max_results {
                return false;
            }
            match value {
                Value::Object(map) => {
                    for (key, child) in map {
                        if cancel.load(Ordering::SeqCst) {
                            return false;
                        }
                        if opts.max_results > 0 && hits.len() >= opts.max_results {
                            return false;
                        }
                        path.push(key.clone());
                        if ref_matches(key, needle, opts.case_sensitive) {
                            hits.push(ref_hit(path, Some(key.clone()), child, lines));
                        }
                        if ref_value_matches(child, needle, opts.case_sensitive) {
                            hits.push(ref_hit(path, Some(key.clone()), child, lines));
                        }
                        if (child.is_object() || child.is_array())
                            && !ref_walk(child, path, needle, opts, cancel, lines, hits)
                        {
                            path.pop();
                            return false;
                        }
                        path.pop();
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        if cancel.load(Ordering::SeqCst) {
                            return false;
                        }
                        if opts.max_results > 0 && hits.len() >= opts.max_results {
                            return false;
                        }
                        let key = index.to_string();
                        path.push(key.clone());
                        if ref_matches(&key, needle, opts.case_sensitive) {
                            hits.push(ref_hit(path, Some(key.clone()), child, lines));
                        }
                        if ref_value_matches(child, needle, opts.case_sensitive) {
                            hits.push(ref_hit(path, Some(key.clone()), child, lines));
                        }
                        if (child.is_object() || child.is_array())
                            && !ref_walk(child, path, needle, opts, cancel, lines, hits)
                        {
                            path.pop();
                            return false;
                        }
                        path.pop();
                    }
                }
                _ => {}
            }
            true
        }

        fn reference(input: &str, query: &str, opts: SearchOptions) -> Vec<SearchHit> {
            if query.is_empty() {
                return Vec::new();
            }
            let Ok(value) = serde_json::from_str::<Value>(input) else {
                return Vec::new();
            };
            let mut lines = HashMap::new();
            let mut line = 0usize;
            index_value(&value, &mut Vec::new(), &mut line, &mut lines);
            let needle = if opts.case_sensitive {
                query.to_string()
            } else {
                query.to_lowercase()
            };
            let mut hits = Vec::new();
            let mut path = Vec::new();
            let cancel = AtomicBool::new(false);
            ref_walk(
                &value,
                &mut path,
                &needle,
                opts,
                &cancel,
                &lines,
                &mut hits,
            );
            hits
        }

        #[test]
        fn streaming_matches_the_tree_reference() {
            let mut state = 42u64;
            for case in 0..3000 {
                let mut keys = 0usize;
                let value = rand_json(&mut state, 4, &mut keys);
                let input = serde_json::to_string(&value).unwrap();
                for (query, case_sensitive) in [
                    ("needle", false),
                    ("NEEDLE", false),
                    ("needle", true),
                    ("k1", false),
                    ("1", false),
                    ("e", false),
                    ("zz", false),
                ] {
                    for max_results in [0usize, 1, 3, 1000] {
                        let opts = SearchOptions {
                            case_sensitive,
                            max_results,
                        };
                        let mine = search(&input, query, opts, &AtomicBool::new(false));
                        let refd = reference(&input, query, opts);
                        assert_eq!(
                            mine, refd,
                            "case {case} query {query:?} opts {opts:?}\ninput: {input}"
                        );
                    }
                }
            }
        }
    }
}
