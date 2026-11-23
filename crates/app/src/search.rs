//! JSON search, ported from `searchInObject` in the old Web Worker.
//!
//! Semantics kept from the TypeScript version:
//!
//! * object keys and string values match case-insensitively by default;
//! * numbers match when their decimal text contains the query (always
//!   case-insensitive in practice);
//! * booleans and `null` never match;
//! * arrays are walked like objects, with their indices as path segments;
//! * a hit is recorded for a matching key and, separately, for a matching
//!   value, so a key whose value also matches produces two hits.
//!
//! `SearchHit::line` points at the value's line in the pretty-printed document
//! (1-based), so the UI can jump to it once formatting has run.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
/// Invalid JSON returns no hits; parse failures are the formatter's job to
/// report. `cancel` is checked for every visited value, so a large document
/// can be abandoned from another thread without waiting for the walk.
pub fn search(
    input: &str,
    query: &str,
    opts: SearchOptions,
    cancel: &AtomicBool,
) -> Vec<SearchHit> {
    if query.is_empty() || cancel.load(Ordering::SeqCst) {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return Vec::new();
    };
    let lines = line_index(&value);
    let needle = if opts.case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let mut hits = Vec::new();
    let mut path = Vec::new();
    walk(&value, &mut path, &needle, opts, cancel, &lines, &mut hits);
    hits
}

fn matches(text: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        text.contains(needle)
    } else {
        text.to_lowercase().contains(needle)
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
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
                if matches(key, needle, opts.case_sensitive) {
                    hits.push(hit(path, Some(key.clone()), child, lines));
                }
                if value_matches(child, needle, opts.case_sensitive) {
                    hits.push(hit(path, Some(key.clone()), child, lines));
                }
                if (child.is_object() || child.is_array())
                    && !walk(child, path, needle, opts, cancel, lines, hits)
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
                if matches(&key, needle, opts.case_sensitive) {
                    hits.push(hit(path, Some(key.clone()), child, lines));
                }
                if value_matches(child, needle, opts.case_sensitive) {
                    hits.push(hit(path, Some(key.clone()), child, lines));
                }
                if (child.is_object() || child.is_array())
                    && !walk(child, path, needle, opts, cancel, lines, hits)
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

fn value_matches(value: &Value, needle: &str, case_sensitive: bool) -> bool {
    match value {
        Value::String(text) => matches(text, needle, case_sensitive),
        Value::Number(number) => number.to_string().contains(needle),
        _ => false,
    }
}

fn hit(
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

fn join_path(path: &[String]) -> String {
    path.join(" → ")
}

fn preview(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| String::from("<unprintable>")),
    }
}

/// Maps each path in `value` to the 0-based line where `serde_json`'s
/// two-space pretty printer puts it.
///
/// The walk follows the same newline rules as the printer: a container's
/// opening brace shares the key's line, each child gets a new line, and a
/// non-empty container's closing brace gets its own line. Empty containers
/// stay on one line.
fn line_index(value: &Value) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    let mut line = 0usize;
    let mut path = Vec::new();
    index_value(value, &mut path, &mut line, &mut map);
    map
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, query: &str) -> Vec<SearchHit> {
        search(
            input,
            query,
            SearchOptions::default(),
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn matches_keys_and_string_values_case_insensitively() {
        let hits = run(r#"{"Alpha":1,"nested":{"alpha":"x"}}"#, "alph");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "Alpha");
        assert_eq!(hits[1].path, "nested → alpha");
        assert_eq!(hits[1].key.as_deref(), Some("alpha"));
    }

    #[test]
    fn matches_string_values_but_not_booleans_or_null() {
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
        let hits = run(r#"{"n":142,"m":42.5}"#, "42");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].value_preview, "142");
        assert_eq!(hits[1].value_preview, "42.5");
    }

    #[test]
    fn case_sensitive_option_is_honoured() {
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
        let input = r#"{"a":[{"b":"needle"}]}"#;
        let hits = run(input, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a → 0 → b");
        assert_eq!(hits[0].key.as_deref(), Some("b"));
        assert!(hits[0].line.is_some());
    }

    #[test]
    fn max_results_caps_the_walk() {
        let opts = SearchOptions {
            max_results: 2,
            ..SearchOptions::default()
        };
        let cancel = AtomicBool::new(false);
        let hits = search(r#"["a","a","a","a"]"#, "a", opts, &cancel);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn cancel_before_the_walk_returns_nothing() {
        let cancel = AtomicBool::new(true);
        let hits = search(r#"{"a":"a"}"#, "a", SearchOptions::default(), &cancel);
        assert!(hits.is_empty());
    }

    #[test]
    fn invalid_json_returns_no_hits() {
        assert!(run("{", "x").is_empty());
    }

    #[test]
    fn empty_query_returns_no_hits() {
        assert!(run(r#"{"a":1}"#, "").is_empty());
    }

    #[test]
    fn pretty_lines_point_at_values() {
        let input = r#"{"a":1,"b":{"c":"needle"}}"#;
        let hit = run(input, "needle").pop().unwrap();
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(input).unwrap()).unwrap();
        let line = pretty.lines().nth(hit.line.unwrap() - 1).unwrap();
        assert!(line.contains("needle"), "{line:?}");
    }
}
