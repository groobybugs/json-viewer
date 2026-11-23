//! JSON formatting and minification, on the caller's thread or a worker.
//!
//! Formatting mirrors the old Web Worker: parse, then `JSON.stringify` with
//! two-space indentation (pretty) or no whitespace (minified). Key order is
//! preserved by `serde_json`'s `preserve_order` feature.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use serde::{Deserialize, Serialize};

/// What to do with a parsed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonAction {
    /// Pretty-print with two-space indentation.
    Format,
    /// Remove all insignificant whitespace.
    Minify,
}

/// A JSON parse or serialization failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonError {
    /// Human-readable message without the location suffix.
    pub message: String,
    /// 1-based line of the failure (`0` when the location is unknown).
    pub line: usize,
    /// 1-based column of the failure (`0` when the location is unknown).
    pub column: usize,
}

impl JsonError {
    /// Builds an error from a message and a location.
    pub fn new(message: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            message: message.into(),
            line,
            column,
        }
    }

    fn from_serde(error: serde_json::Error) -> Self {
        let line = error.line();
        let column = error.column();
        let rendered = error.to_string();
        // serde renders "expected value at line 3 column 4"; keep only the message.
        let message = match rendered.rsplit_once(" at line ") {
            Some((head, _)) => head.to_string(),
            None => rendered,
        };
        Self {
            message,
            line,
            column,
        }
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(
                f,
                "{} at line {}, column {}",
                self.message, self.line, self.column
            )
        }
    }
}

impl std::error::Error for JsonError {}

/// Parses `input` and returns formatted or minified JSON.
///
/// This is the synchronous core used by both the UI thread (tests, small
/// documents) and [`spawn_process`].
pub fn process(input: &str, action: JsonAction) -> Result<String, JsonError> {
    let value: serde_json::Value = serde_json::from_str(input).map_err(JsonError::from_serde)?;
    let rendered = match action {
        JsonAction::Format => serde_json::to_string_pretty(&value),
        JsonAction::Minify => serde_json::to_string(&value),
    };
    rendered.map_err(JsonError::from_serde)
}

/// A background formatting job. Cancel it or wait for it through its handle.
pub struct JsonJob;

/// Handle to a running [`JsonJob`].
///
/// Dropping the handle cancels the job and joins its thread; call
/// [`JobHandle::cancel`] first when the result is no longer wanted, then drop.
/// The callback is never called after cancellation was requested.
pub struct JobHandle {
    cancel: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl JobHandle {
    /// Asks the worker to stop. A running parse cannot be interrupted, but its
    /// result is discarded.
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Returns `true` when cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Waits for the worker to finish and returns whether it panicked.
    pub fn join(mut self) -> thread::Result<()> {
        match self.join.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// Runs [`process`] on a named worker thread and calls `on_done` with the
/// result unless the job was cancelled first.
///
/// `input` is an `Arc<str>` so the caller can drop or mutate its copy without
/// copying the document again.
pub fn spawn_process<F>(input: Arc<str>, action: JsonAction, on_done: F) -> JobHandle
where
    F: FnOnce(Result<String, JsonError>) + Send + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let join = thread::Builder::new()
        .name("json-viewer-process".into())
        .spawn(move || {
            if worker_cancel.load(Ordering::SeqCst) {
                return;
            }
            let result = process(&input, action);
            if worker_cancel.load(Ordering::SeqCst) {
                return;
            }
            on_done(result);
        })
        .expect("spawn JSON worker thread");
    JobHandle {
        cancel,
        join: Some(join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn format_uses_two_spaces_and_keeps_key_order() {
        let out = process(r#"{"b":1,"a":[true,null]}"#, JsonAction::Format).unwrap();
        assert_eq!(
            out,
            "{\n  \"b\": 1,\n  \"a\": [\n    true,\n    null\n  ]\n}"
        );
        assert!(out.find("\"b\"").unwrap() < out.find("\"a\"").unwrap());
    }

    #[test]
    fn minify_removes_whitespace() {
        let out = process("{\n  \"a\" : 1\n}", JsonAction::Minify).unwrap();
        assert_eq!(out, r#"{"a":1}"#);
    }

    #[test]
    fn parse_errors_carry_line_and_column() {
        let err = process("{\n  \"a\":\n}", JsonAction::Format).unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.column > 0, "{err:?}");
        assert!(!err.message.contains("at line"), "{err:?}");
        assert_eq!(
            err.to_string(),
            format!("{} at line 3, column {}", err.message, err.column)
        );
    }

    #[test]
    fn worker_delivers_the_result() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn_process(Arc::from("{\"a\":1}"), JsonAction::Minify, move |result| {
            tx.send(result).unwrap();
        });
        handle.join().unwrap();
        assert_eq!(rx.recv().unwrap().unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn cancel_sets_the_flag() {
        let mut handle = spawn_process(Arc::from("{}"), JsonAction::Format, |_| {});
        handle.cancel();
        assert!(handle.is_cancelled());
        handle.join().unwrap();
    }
}
