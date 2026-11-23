//! JSON formatting, minification and validation, on the caller's thread or a worker.
//!
//! The old Web Worker parsed with `JSON.parse` and re-serialized with
//! `JSON.stringify(value, null, 2)` (pretty) or `JSON.stringify(value)`
//! (minified). This module reproduces that output byte for byte, with two
//! deliberate differences required by the port:
//!
//! * Number literals are copied verbatim, so `1.0`, `1e5` and integers beyond
//!   f64 precision keep their spelling instead of going through JavaScript's
//!   number-to-string algorithm.
//! * `JSON.parse` collapses duplicate object keys (last value, first
//!   position); a single-pass formatter emits the document as written, so
//!   duplicate keys are preserved.
//!
//! Formatting is a streaming, single-pass formatter: the document is
//! validated and re-emitted in one scan. The container stack is explicit
//! rather than recursive, so deeply nested documents (tens of thousands of
//! levels) cannot overflow the thread stack. Strings are decoded and
//! re-escaped exactly like `JSON.stringify`: `\u` escapes for printable code
//! points become raw UTF-8, `\/` becomes `/`, control characters use the
//! shortest JavaScript escape, non-ASCII stays raw and lone surrogates stay
//! escaped.
//!
//! Long jobs can be interrupted: [`spawn_process`] passes the job's cancel
//! flag to the formatter, which checks it every 64 KiB of input.

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

/// Statistics collected while validating a JSON document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationInfo {
    /// Source size in bytes.
    pub bytes: usize,
    /// Number of source lines, including a final empty line after a newline.
    pub lines: usize,
    /// Deepest array/object nesting level. A top-level scalar has depth zero.
    pub max_depth: usize,
    /// Number of JSON values, including containers but excluding object keys.
    pub value_count: usize,
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
/// documents) and [`spawn_process`]. Runs are not interruptible.
pub fn process(input: &str, action: JsonAction) -> Result<String, JsonError> {
    static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);
    process_with_cancel(input, action, &NEVER_CANCEL)
}

/// Validates `input` without building an output string.
///
/// This uses the same streaming parser as [`process`], with its output sink
/// disabled. The explicit container stack remains safe for deeply nested input.
pub fn validate(input: &str) -> Result<ValidationInfo, JsonError> {
    static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);
    validate_with_cancel(input, &NEVER_CANCEL)
}

fn validate_with_cancel(input: &str, cancel: &AtomicBool) -> Result<ValidationInfo, JsonError> {
    let mut validator = Formatter::validating(input, cancel);
    validator.run()?;
    Ok(validator.finish_validation())
}

/// Async-free core shared by [`process`] and [`spawn_process`]. Returns a
/// `line == 0` error when `cancel` becomes set mid-run.
fn process_with_cancel(
    input: &str,
    action: JsonAction,
    cancel: &AtomicBool,
) -> Result<String, JsonError> {
    let mut formatter = Formatter::new(input, action, cancel);
    formatter.run()?;
    Ok(formatter.finish())
}

fn cancelled_error() -> JsonError {
    JsonError::new("JSON processing cancelled", 0, 0)
}

/// Indentation width used by `JSON.stringify(value, null, 2)`.
const INDENT_WIDTH: usize = 2;

/// Input bytes between cancellation checks inside a token.
const CANCEL_STRIDE: usize = 64 * 1024;

/// Open container on the formatter's explicit stack.
#[derive(Clone, Copy)]
struct Frame {
    object: bool,
}

/// Whether [`Formatter::parse_value`] consumed a complete value or opened a
/// container whose contents are still pending.
enum ValueKind {
    Scalar,
    ContainerOpened,
}

/// A string sink that can be disabled for validation-only parsing.
struct Output {
    text: String,
    enabled: bool,
}

impl Output {
    fn writing(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            enabled: true,
        }
    }

    fn discarded() -> Self {
        Self {
            text: String::new(),
            enabled: false,
        }
    }

    #[inline]
    fn push(&mut self, ch: char) {
        if self.enabled {
            self.text.push(ch);
        }
    }

    #[inline]
    fn push_str(&mut self, text: &str) {
        if self.enabled {
            self.text.push_str(text);
        }
    }
}

struct Formatter<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    out: Output,
    pretty: bool,
    cancel: &'a AtomicBool,
    cancel_at: usize,
    stats: Option<ValidationInfo>,
}

impl<'a> Formatter<'a> {
    fn new(input: &'a str, action: JsonAction, cancel: &'a AtomicBool) -> Self {
        let pretty = action == JsonAction::Format;
        let capacity = if pretty {
            input
                .len()
                .saturating_add(input.len() / 8)
                .saturating_add(32)
        } else {
            input.len().saturating_add(1)
        };
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            out: Output::writing(capacity),
            pretty,
            cancel,
            cancel_at: 0,
            stats: None,
        }
    }

    fn validating(input: &'a str, cancel: &'a AtomicBool) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            out: Output::discarded(),
            pretty: false,
            cancel,
            cancel_at: 0,
            stats: Some(ValidationInfo {
                bytes: input.len(),
                lines: 1,
                max_depth: 0,
                value_count: 0,
            }),
        }
    }

    fn finish(self) -> String {
        self.out.text
    }

    fn finish_validation(self) -> ValidationInfo {
        self.stats.expect("validation parser has statistics")
    }

    #[inline]
    fn note_value(&mut self) {
        if let Some(stats) = &mut self.stats {
            stats.value_count += 1;
        }
    }

    #[inline]
    fn note_container(&mut self, depth: usize) {
        if let Some(stats) = &mut self.stats {
            stats.max_depth = stats.max_depth.max(depth);
        }
    }

    /// Validates and re-emits the whole document.
    ///
    /// The loop keeps the container stack here instead of recursing, which is
    /// what makes very deep nesting safe.
    fn run(&mut self) -> Result<(), JsonError> {
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            match self.parse_value(&mut stack)? {
                ValueKind::Scalar => loop {
                    self.skip_ws()?;
                    let frame = match stack.last().copied() {
                        Some(frame) => frame,
                        None => {
                            if self.pos < self.bytes.len() {
                                return Err(
                                    self.error_here("Unexpected token after top-level JSON value")
                                );
                            }
                            return Ok(());
                        }
                    };
                    let byte = self.peek().ok_or_else(|| self.error_eof())?;
                    match byte {
                        b',' => {
                            self.pos += 1;
                            self.out.push(',');
                            self.newline_indent(stack.len());
                            if frame.object {
                                self.skip_ws()?;
                                self.parse_key_and_colon()?;
                            }
                            break;
                        }
                        b'}' if frame.object => {
                            self.pos += 1;
                            stack.pop();
                            self.newline_indent(stack.len());
                            self.out.push('}');
                        }
                        b']' if !frame.object => {
                            self.pos += 1;
                            stack.pop();
                            self.newline_indent(stack.len());
                            self.out.push(']');
                        }
                        _ => {
                            return Err(self.error_here(if frame.object {
                                "Expected ',' or '}' after property value"
                            } else {
                                "Expected ',' or ']' after array element"
                            }));
                        }
                    }
                },
                ValueKind::ContainerOpened => {}
            }
        }
    }

    /// Parses one value. Containers are opened here and their first member is
    /// started, but their contents are consumed by the caller's loop so the
    /// call stack never grows with nesting depth.
    fn parse_value(&mut self, stack: &mut Vec<Frame>) -> Result<ValueKind, JsonError> {
        self.note_value();
        self.skip_ws()?;
        let byte = self.peek().ok_or_else(|| self.error_eof())?;
        match byte {
            b'{' => {
                self.pos += 1;
                self.note_container(stack.len() + 1);
                self.skip_ws()?;
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    self.out.push_str("{}");
                    return Ok(ValueKind::Scalar);
                }
                self.out.push('{');
                stack.push(Frame { object: true });
                self.newline_indent(stack.len());
                self.parse_key_and_colon()?;
                Ok(ValueKind::ContainerOpened)
            }
            b'[' => {
                self.pos += 1;
                self.note_container(stack.len() + 1);
                self.skip_ws()?;
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    self.out.push_str("[]");
                    return Ok(ValueKind::Scalar);
                }
                self.out.push('[');
                stack.push(Frame { object: false });
                self.newline_indent(stack.len());
                Ok(ValueKind::ContainerOpened)
            }
            b'"' => {
                self.parse_string()?;
                Ok(ValueKind::Scalar)
            }
            b't' => {
                self.expect_literal("true")?;
                Ok(ValueKind::Scalar)
            }
            b'f' => {
                self.expect_literal("false")?;
                Ok(ValueKind::Scalar)
            }
            b'n' => {
                self.expect_literal("null")?;
                Ok(ValueKind::Scalar)
            }
            b'-' | b'0'..=b'9' => {
                self.parse_number()?;
                Ok(ValueKind::Scalar)
            }
            _ => Err(self.unexpected_token()),
        }
    }

    /// Parses an object key and the following colon, emitting both.
    fn parse_key_and_colon(&mut self) -> Result<(), JsonError> {
        self.maybe_cancel()?;
        match self.peek() {
            Some(b'"') => {}
            Some(_) => return Err(self.error_here("Expected string property name")),
            None => return Err(self.error_eof()),
        }
        self.parse_string()?;
        self.skip_ws()?;
        if self.peek() != Some(b':') {
            return Err(self.error_here("Expected ':' after property name"));
        }
        self.pos += 1;
        self.out.push_str(if self.pretty { ": " } else { ":" });
        Ok(())
    }

    /// Parses a string literal, emitting its canonical `JSON.stringify` form
    /// (opening and closing quote included).
    fn parse_string(&mut self) -> Result<(), JsonError> {
        self.pos += 1; // opening quote
        self.out.push('"');
        let mut run_start = self.pos;
        loop {
            let byte = match self.bytes.get(self.pos) {
                Some(&byte) => byte,
                None => return Err(self.error_eof()),
            };
            match byte {
                b'"' => {
                    if run_start < self.pos {
                        self.out.push_str(&self.input[run_start..self.pos]);
                    }
                    self.pos += 1;
                    self.out.push('"');
                    return Ok(());
                }
                b'\\' => {
                    if run_start < self.pos {
                        self.out.push_str(&self.input[run_start..self.pos]);
                    }
                    self.pos += 1;
                    self.decode_escape()?;
                    run_start = self.pos;
                }
                0x00..=0x1F => {
                    return Err(self.error_here("Bad control character in string literal"));
                }
                _ => {
                    self.pos += 1;
                    if self.pos >= self.cancel_at {
                        self.maybe_cancel()?;
                    }
                }
            }
        }
    }

    /// Decodes one escape sequence (the backslash was already consumed) and
    /// writes the shortest canonical form, exactly like `JSON.stringify`.
    fn decode_escape(&mut self) -> Result<(), JsonError> {
        let escape_pos = self.pos;
        let byte = self.peek().ok_or_else(|| self.error_eof())?;
        self.pos += 1;
        match byte {
            b'"' => self.out.push_str("\\\""),
            b'\\' => self.out.push_str("\\\\"),
            b'/' => self.out.push('/'),
            b'b' => self.out.push_str("\\b"),
            b'f' => self.out.push_str("\\f"),
            b'n' => self.out.push_str("\\n"),
            b'r' => self.out.push_str("\\r"),
            b't' => self.out.push_str("\\t"),
            b'u' => {
                let unit = self.parse_hex4()?;
                if (0xD800..=0xDBFF).contains(&unit) {
                    // A high surrogate only combines with an immediately
                    // following low-surrogate escape; otherwise JavaScript
                    // keeps it as an escaped lone surrogate.
                    if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                        let replay = self.pos;
                        self.pos += 2;
                        let low = self.parse_hex4()?;
                        if (0xDC00..=0xDFFF).contains(&low) {
                            let code = 0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                            let ch = char::from_u32(code).expect("surrogate pair is a scalar");
                            self.push_escaped_char(ch);
                            return Ok(());
                        }
                        self.pos = replay;
                    }
                    self.push_escaped_unit(unit);
                } else if (0xDC00..=0xDFFF).contains(&unit) {
                    self.push_escaped_unit(unit);
                } else {
                    let ch = char::from_u32(unit).expect("non-surrogate code unit is a char");
                    self.push_escaped_char(ch);
                }
            }
            _ => return Err(self.error_at(escape_pos, "Invalid escape sequence")),
        }
        Ok(())
    }

    /// Parses exactly four hexadecimal digits.
    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.peek().ok_or_else(|| self.error_eof())?;
            let digit = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a') + 10,
                b'A'..=b'F' => u32::from(byte - b'A') + 10,
                _ => return Err(self.error_here("Invalid unicode escape")),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    /// Writes a decoded scalar with JavaScript's escape rules.
    fn push_escaped_char(&mut self, ch: char) {
        match ch {
            '"' => self.out.push_str("\\\""),
            '\\' => self.out.push_str("\\\\"),
            '\u{0008}' => self.out.push_str("\\b"),
            '\u{0009}' => self.out.push_str("\\t"),
            '\u{000A}' => self.out.push_str("\\n"),
            '\u{000C}' => self.out.push_str("\\f"),
            '\u{000D}' => self.out.push_str("\\r"),
            ch if (ch as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let code = ch as u32;
                self.out.push_str("\\u00");
                self.out.push(char::from(HEX[(code >> 4) as usize]));
                self.out.push(char::from(HEX[(code & 0xF) as usize]));
            }
            _ => self.out.push(ch),
        }
    }

    /// Writes an escaped UTF-16 code unit (used for lone surrogates).
    fn push_escaped_unit(&mut self, unit: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.out.push_str("\\u");
        for shift in [12, 8, 4, 0] {
            self.out
                .push(char::from(HEX[((unit >> shift) & 0xF) as usize]));
        }
    }

    /// Validates a JSON number and copies its literal form unchanged.
    fn parse_number(&mut self) -> Result<(), JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            Some(_) => return Err(self.error_here("Invalid number")),
            None => return Err(self.error_eof()),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.invalid_number_or_eof());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.invalid_number_or_eof());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        self.out.push_str(&self.input[start..self.pos]);
        Ok(())
    }

    fn invalid_number_or_eof(&self) -> JsonError {
        if self.peek().is_none() {
            self.error_eof()
        } else {
            self.error_here("Invalid number")
        }
    }

    fn expect_literal(&mut self, literal: &'static str) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            self.out.push_str(literal);
            return Ok(());
        }
        if self.stats.is_some() {
            // Validation can be more precise without changing the established
            // Format/Minify errors: point at the first mismatching literal byte.
            for (offset, expected) in literal.bytes().enumerate() {
                match self.bytes.get(self.pos + offset) {
                    Some(&actual) if actual == expected => {}
                    Some(_) => {
                        return Err(self.error_at(
                            self.pos + offset,
                            format!("Invalid literal, expected '{literal}'"),
                        ));
                    }
                    None => return Err(self.error_eof()),
                }
            }
        }
        if self.bytes.len() - self.pos < literal.len() {
            Err(self.error_eof())
        } else {
            Err(self.unexpected_token())
        }
    }

    /// Skips the four whitespace bytes JSON allows.
    fn skip_ws(&mut self) -> Result<(), JsonError> {
        while let Some(byte) = self.bytes.get(self.pos) {
            match byte {
                b'\n' => {
                    self.pos += 1;
                    if let Some(stats) = &mut self.stats {
                        stats.lines += 1;
                    }
                }
                b' ' | b'\t' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
        self.maybe_cancel()
    }

    /// Emits `\n` plus `depth * 2` spaces in pretty mode; nothing in minify.
    fn newline_indent(&mut self, depth: usize) {
        if !self.pretty {
            return;
        }
        const PAD: &str = "                                                                ";
        self.out.push('\n');
        let mut remaining = depth * INDENT_WIDTH;
        while remaining >= PAD.len() {
            self.out.push_str(PAD);
            remaining -= PAD.len();
        }
        self.out.push_str(&PAD[..remaining]);
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Cooperative cancellation: checked at least every [`CANCEL_STRIDE`]
    /// input bytes so a job never blocks on a stale document.
    #[inline]
    fn maybe_cancel(&mut self) -> Result<(), JsonError> {
        if self.pos >= self.cancel_at {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(cancelled_error());
            }
            self.cancel_at = self.pos.saturating_add(CANCEL_STRIDE);
        }
        Ok(())
    }

    fn unexpected_token(&self) -> JsonError {
        match self
            .input
            .get(self.pos..)
            .and_then(|rest| rest.chars().next())
        {
            Some(ch) => self.error_here(format!("Unexpected token '{ch}'")),
            None => self.error_eof(),
        }
    }

    fn error_eof(&self) -> JsonError {
        self.error_at(self.bytes.len(), "Unexpected end of JSON input")
    }

    fn error_here(&self, message: impl Into<String>) -> JsonError {
        self.error_at(self.pos, message)
    }

    /// Builds an error with a 1-based line and column computed lazily; errors
    /// are exceptional, so the hot loop does not track line numbers.
    fn error_at(&self, pos: usize, message: impl Into<String>) -> JsonError {
        let pos = pos.min(self.bytes.len());
        let consumed = &self.bytes[..pos];
        let line_start = consumed
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |index| index + 1);
        let line = 1 + consumed.iter().filter(|&&byte| byte == b'\n').count();
        let column = pos - line_start + 1;
        JsonError::new(message, line, column)
    }
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
    /// Asks the worker to stop. The formatter checks the flag between tokens
    /// and at least every 64 KiB of input, discards its result, and never
    /// calls the callback.
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
            let result = process_with_cancel(&input, action, &worker_cancel);
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

/// Runs [`validate`] on a named worker thread and calls `on_done` with the
/// result unless the job was cancelled first.
pub fn spawn_validate<F>(input: Arc<str>, on_done: F) -> JobHandle
where
    F: FnOnce(Result<ValidationInfo, JsonError>) + Send + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let join = thread::Builder::new()
        .name("json-viewer-validate".into())
        .spawn(move || {
            if worker_cancel.load(Ordering::SeqCst) {
                return;
            }
            let result = validate_with_cancel(&input, &worker_cancel);
            if worker_cancel.load(Ordering::SeqCst) {
                return;
            }
            on_done(result);
        })
        .expect("spawn JSON validation worker thread");
    JobHandle {
        cancel,
        join: Some(join),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

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
    fn format_matches_javascript_layout() {
        let input = r#"{"s":"x","n":12,"a":[1,{"b":false}],"e":[],"o":{},"z":null}"#;
        let expected = concat!(
            "{\n",
            "  \"s\": \"x\",\n",
            "  \"n\": 12,\n",
            "  \"a\": [\n",
            "    1,\n",
            "    {\n",
            "      \"b\": false\n",
            "    }\n",
            "  ],\n",
            "  \"e\": [],\n",
            "  \"o\": {},\n",
            "  \"z\": null\n",
            "}"
        );
        assert_eq!(process(input, JsonAction::Format).unwrap(), expected);
        assert_eq!(
            process(expected, JsonAction::Minify).unwrap(),
            r#"{"s":"x","n":12,"a":[1,{"b":false}],"e":[],"o":{},"z":null}"#
        );
    }

    #[test]
    fn empty_containers_stay_inline() {
        let out = process(r#"{"a":[],"b":{},"c":[[],{}]}"#, JsonAction::Format).unwrap();
        assert_eq!(
            out,
            concat!(
                "{\n",
                "  \"a\": [],\n",
                "  \"b\": {},\n",
                "  \"c\": [\n",
                "    [],\n",
                "    {}\n",
                "  ]\n",
                "}"
            )
        );
    }

    #[test]
    fn number_lexemes_are_preserved() {
        let input =
            r#"{"a":1.0,"b":1e5,"c":1E+2,"d":-0,"e":123456789012345678901234567890,"f":0.000001}"#;
        assert_eq!(process(input, JsonAction::Minify).unwrap(), input);
        let pretty = process(input, JsonAction::Format).unwrap();
        for literal in [
            "1.0",
            "1e5",
            "1E+2",
            "-0",
            "123456789012345678901234567890",
            "0.000001",
        ] {
            assert!(
                pretty.contains(literal),
                "{literal:?} missing from {pretty}"
            );
        }
    }

    #[test]
    fn strings_are_reencoded_like_javascript() {
        let input = r#"{"plain":"caf\u00e9","pair":"\ud83d\ude00","slash":"a\/b","quotes":"a\"b\\c","controls":"\b\f\n\r\t\u0000\u001f\u007f","lone":"\ud800"}"#;
        let expected = concat!(
            "{\n",
            "  \"plain\": \"café\",\n",
            "  \"pair\": \"😀\",\n",
            "  \"slash\": \"a/b\",\n",
            "  \"quotes\": \"a\\\"b\\\\c\",\n",
            "  \"controls\": \"\\b\\f\\n\\r\\t\\u0000\\u001f\u{7f}\",\n",
            "  \"lone\": \"\\ud800\"\n",
            "}"
        );
        assert_eq!(process(input, JsonAction::Format).unwrap(), expected);
    }

    #[test]
    fn unicode_escapes_use_the_shortest_form() {
        let input = r#""\u0041\u0022\u005c\u002f\u0009""#;
        assert_eq!(
            process(input, JsonAction::Minify).unwrap(),
            "\"A\\\"\\\\/\\t\""
        );
    }

    #[test]
    fn top_level_scalars_round_trip() {
        for (input, expected) in [
            ("1", "1"),
            ("-1.5e+10", "-1.5e+10"),
            ("true", "true"),
            ("false", "false"),
            ("null", "null"),
            (r#""caf\u00e9""#, "\"café\""),
        ] {
            assert_eq!(process(input, JsonAction::Format).unwrap(), expected);
            assert_eq!(process(input, JsonAction::Minify).unwrap(), expected);
        }
    }

    #[test]
    fn format_and_minify_round_trip() {
        let input = r#"{"a":[1,2,{"b":"x\ty"}],"c":{"d":[-1.25e-3,true,null]}}"#;
        let formatted = process(input, JsonAction::Format).unwrap();
        let minified = process(input, JsonAction::Minify).unwrap();
        assert_eq!(process(&formatted, JsonAction::Minify).unwrap(), minified);
    }

    #[test]
    fn deep_nesting_10k_does_not_overflow_the_stack() {
        const DEPTH: usize = 10_000;
        let arrays = format!("{}{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        assert_eq!(process(&arrays, JsonAction::Minify).unwrap(), arrays);

        let mut objects = String::with_capacity(DEPTH * 6 + 1);
        for _ in 0..DEPTH {
            objects.push_str("{\"a\":");
        }
        objects.push('1');
        for _ in 0..DEPTH {
            objects.push('}');
        }
        assert_eq!(process(&objects, JsonAction::Minify).unwrap(), objects);
    }

    #[test]
    fn parse_errors_carry_line_and_column() {
        let cases = [
            ("", 1, 1),
            ("   ", 1, 4),
            ("{", 1, 2),
            (r#"{"a":1,}"#, 1, 8),
            ("[1,]", 1, 4),
            (r#"{"a" 1}"#, 1, 6),
            (r#"{"a":}"#, 1, 6),
            ("[1 2]", 1, 4),
            ("01", 1, 2),
            ("+1", 1, 1),
            ("1.", 1, 3),
            ("1e+", 1, 4),
            ("NaN", 1, 1),
            ("[1, 2}", 1, 6),
            ("{} x", 1, 4),
            ("\"unterminated", 1, 14),
            ("\"a\nb\"", 1, 3),
            ("[\n  1,\n  ]", 3, 3),
            (r#"{"a":"\x"}"#, 1, 8),
            (r#"{"a":"\u12G4"}"#, 1, 11),
            ("\u{feff}{}", 1, 1),
        ];
        for (input, line, column) in cases {
            let err = process(input, JsonAction::Format).unwrap_err();
            assert_eq!((err.line, err.column), (line, column), "{input:?}: {err:?}");
            assert!(!err.message.contains("at line"), "{err:?}");
            assert_eq!(
                err.to_string(),
                format!(
                    "{} at line {}, column {}",
                    err.message, err.line, err.column
                )
            );
        }
    }

    #[test]
    fn cancellation_aborts_before_any_work() {
        let input = "[1,2,3]";
        let cancel = AtomicBool::new(true);
        let err = process_with_cancel(input, JsonAction::Minify, &cancel).unwrap_err();
        assert_eq!(err.line, 0);
        assert!(err.message.contains("cancelled"), "{err:?}");
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

    /// Builds a valid top-level array of records close to `target_bytes`.
    fn make_fixture(target_bytes: usize) -> String {
        const RECORD: &str = concat!(
            r#"{"id":12345,"name":"widget-äöü-日本","price":19.99,"#,
            r#""tags":["a","b",null,true],"nested":{"x":1e5,"y":[1,2,3],"ok":false},"#,
            r#""text":"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor."}"#
        );
        let mut out = String::with_capacity(target_bytes + RECORD.len() + 2);
        out.push('[');
        while out.len() + RECORD.len() + 2 <= target_bytes {
            if out.len() > 1 {
                out.push(',');
            }
            out.push_str(RECORD);
        }
        out.push(']');
        out
    }

    fn measure(label: &str, mb: f64, mut run: impl FnMut() -> bool) -> f64 {
        let started = Instant::now();
        let ok = run();
        let seconds = started.elapsed().as_secs_f64();
        assert!(ok, "{label} produced an unexpected result");
        println!(
            "{label:<22}{mb:>10.1}{:>12.1}{:>12.1}",
            seconds * 1000.0,
            mb / seconds
        );
        seconds
    }

    #[test]
    #[ignore = "large-document timing table; run in release with --ignored --nocapture"]
    fn json_ops_timing_table() {
        println!("{:<22}{:>10}{:>12}{:>12}", "case", "input MB", "ms", "MB/s");
        for target_mb in [10usize, 50, 100] {
            let fixture = make_fixture(target_mb << 20);
            let mb = fixture.len() as f64 / (1024.0 * 1024.0);
            println!("--- fixture {:.1} MiB ({target_mb} MB target)", mb);

            let mut format_len = 0usize;
            measure("format", mb, || {
                let out = process(&fixture, JsonAction::Format).unwrap();
                format_len = out.len();
                format_len > fixture.len()
            });

            let mut minified = String::new();
            measure("minify", mb, || {
                minified = process(&fixture, JsonAction::Minify).unwrap();
                minified == fixture
            });

            let invalid = &fixture[..fixture.len() - 1];
            measure("invalid-at-end", mb, || {
                let err = process(invalid, JsonAction::Minify).unwrap_err();
                err.line == 1 && err.column > 0
            });

            println!("format output: {format_len} bytes");
        }
    }

    #[test]
    #[ignore = "writes ~200 MB; run in release with --ignored"]
    fn json_ops_format_deep_nesting_10k() {
        const DEPTH: usize = 10_000;
        let input = format!("{}{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let out = process(&input, JsonAction::Format).unwrap();
        let expected_len = 4 * DEPTH - 2 + 2 * (DEPTH - 1) * (DEPTH - 1);
        assert_eq!(out.len(), expected_len);
    }
}
