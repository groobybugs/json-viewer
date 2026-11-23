//! Virtualized text component.
//!
//! Editable input uses Rune's [`TextEdit`] (the caret/selection live in app
//! state). Read-only output is a [`El::virtual_rows`] list over a
//! [`crate::line_index::LineIndex`]: only the mounted rows are built, so a
//! 100 MB / 2M-line document costs O(visible rows) per frame instead of
//! shaping the whole file.
//!
//! ```ignore
//! TextView::read_only(&tab.output)
//!     .line_numbers(true)
//!     .first_column(0)
//!     .visible_columns(240)
//!     .highlight("needle")
//!     .highlight_line(hit.line)
//!     .build("output")
//! // then, from a handler:
//! tx.scroll_into_view(TextView::row_key("output", hit.line));
//! ```
//!
//! ## Frozen contract
//!
//! `first_line` / `visible_lines` keep their original windowed meaning
//! ([`TextView::visible_range`], [`TextView::line`]) for callers that manage
//! their own viewport. The default read-only `build` is the real
//! `virtual_rows` path; scroll position comes from Rune's scroll viewport and
//! `Tx::scroll_into_view(TextView::row_key(..))`. Use
//! [`TextView::build_revealing`] for a one-shot scroll-to-line.
//!
//! ## State coupling
//!
//! `virtual_rows` is generic over the window's state `S`; its row builders are
//! `'static` closures that capture an `Arc<Snapshot>` and ignore `&S`. This
//! crate has exactly one window state, [`crate::state::AppState`], so `build`
//! names it explicitly. Any `Runtime` running a `TextView` must be
//! `Runtime<AppState>` (the app, `headless::runtime` and this crate's tests
//! all are). See `NOTES` on the frozen API if a second state type is ever
//! introduced.
//!
//! ## Large documents
//!
//! The line index is built once per document and cached by content
//! fingerprint in [`crate::line_index`]. A cold miss on a large document does
//! no O(document) work on the UI thread: `snapshot` copies and scans only a
//! bounded prefix ([`crate::line_index::PARTIAL_SCAN_BYTES`]), which is enough
//! to mount the first viewport, then a background job replaces it with the
//! full index through `line_index::prewarm_off_thread`. Above
//! [`EDITABLE_LIMIT_BYTES`] the editable mode falls back to this read-only
//! virtualized view with [`LARGE_FILE_BANNER`].
//!
//! ## Monospace
//!
//! `mono()` is `Family::Mono`, i.e. the app's **mono slot**, not
//! `Family::SystemMono`. `AppSpec::new` and `TextSystem::default()` both put
//! `"Inter"` (proportional) behind that slot. `headless::APP_FONTS` /
//! `headless::APP_FAMILIES` register IBM Plex Mono there for both the window
//! (`AppSpec::fonts`/`families`) and headless (`Runtime::with_text`). See
//! `NOTES-rp-view.md`. The line-number gutter is a fixed-width box with
//! `Align::End`, so digits are right-aligned.

use std::borrow::Cow;
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use rune::{col, el, oklch, oklcha, row, text, tok, Align, Color, El, GrowthSpec, Span, TextEdit, TextSelection};

use crate::line_index::{self, Snapshot};
use crate::state::AppState;

/// Estimated height of one monospace row; unvisited virtual rows use this.
pub const ROW_HEIGHT: f32 = 16.0;
/// Advance width of one 12pt monospace character, for horizontal spacers.
pub const CHAR_WIDTH: f32 = 7.2;
/// Default column window width. A minified 100 MB single line shapes only
/// this many columns, never the whole line.
pub const DEFAULT_VISIBLE_COLUMNS: usize = 240;
/// Above this input size, editing is disabled and the read-only virtual view
/// takes over. 2 MB is roughly where Rune's native `TextEdit` (which shapes
/// the whole buffer) stops being interactive.
pub const EDITABLE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Banner shown when the editable view falls back to read-only.
pub const LARGE_FILE_BANNER: &str =
    "Large file: editing disabled — Format/Minify/Search still work";
/// How far a truncated row can be panned horizontally, in columns.
const HORIZONTAL_PAN_COLUMNS: usize = 8_192;

/// How the text is edited.
pub enum TextViewMode<'a> {
    /// Editable through Rune's text input. The caret and selection live in
    /// `TextEdit`; the app syncs `TextEdit::text` back into its model in
    /// `on_input_change`.
    Editable(&'a TextEdit),
    /// Read-only text, windowed by line index.
    ReadOnly,
}

/// A windowed text area.
pub struct TextView<'a> {
    text: &'a str,
    mode: TextViewMode<'a>,
    first_line: usize,
    visible_lines: usize,
    line_numbers: bool,
    query: Option<&'a str>,
    first_column: usize,
    visible_columns: usize,
    highlight_line: Option<usize>,
    editable_limit: usize,
    horizontal_scroll: bool,
}

impl<'a> TextView<'a> {
    /// Creates an editable view over `text`.
    ///
    /// `edit` must be the [`TextEdit`] stored in app state; Rune routes edits
    /// back to it through `.on_edit`. Inputs over [`EDITABLE_LIMIT_BYTES`]
    /// render read-only instead (see [`LARGE_FILE_BANNER`]).
    pub fn editable(text: &'a str, edit: &'a TextEdit) -> Self {
        Self {
            text,
            mode: TextViewMode::Editable(edit),
            first_line: 0,
            visible_lines: 40,
            line_numbers: false,
            query: None,
            first_column: 0,
            visible_columns: DEFAULT_VISIBLE_COLUMNS,
            highlight_line: None,
            editable_limit: EDITABLE_LIMIT_BYTES,
            horizontal_scroll: true,
        }
    }

    /// Creates a read-only view.
    pub fn read_only(text: &'a str) -> Self {
        Self {
            text,
            mode: TextViewMode::ReadOnly,
            first_line: 0,
            visible_lines: 40,
            line_numbers: false,
            query: None,
            first_column: 0,
            visible_columns: DEFAULT_VISIBLE_COLUMNS,
            highlight_line: None,
            editable_limit: EDITABLE_LIMIT_BYTES,
            horizontal_scroll: true,
        }
    }

    /// First visible line (0-based). Clamped to the document length.
    pub fn first_line(mut self, line: usize) -> Self {
        self.first_line = line;
        self
    }

    /// Number of rows to build. [`TextView::build`] renders exactly this many
    /// lines, never the whole document.
    pub fn visible_lines(mut self, lines: usize) -> Self {
        self.visible_lines = lines;
        self
    }

    /// Draws a right-aligned line-number gutter in read-only mode.
    pub fn line_numbers(mut self, on: bool) -> Self {
        self.line_numbers = on;
        self
    }

    /// Tints lines containing `query` (case-insensitive) in read-only mode.
    pub fn highlight(mut self, query: &'a str) -> Self {
        self.query = Some(query);
        self
    }

    /// Number of lines; empty text counts as one empty line.
    pub fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    /// Visible line range, clamped to the document.
    pub fn visible_range(&self) -> Range<usize> {
        let count = self.line_count();
        let start = self.first_line.min(count);
        let end = start.saturating_add(self.visible_lines).min(count);
        start..end
    }

    /// Returns line `index` (0-based), or `None` past the end.
    pub fn line(&self, index: usize) -> Option<&'a str> {
        self.text.split('\n').nth(index)
    }

    // ── Additive builders ───────────────────────────────────────────────

    /// First column of the horizontal window (byte columns; ASCII JSON maps
    /// 1:1 to display columns). Use this as the horizontal scroll position:
    /// it never shapes more than `visible_columns` bytes of a line.
    pub fn first_column(mut self, column: usize) -> Self {
        self.first_column = column;
        self
    }

    /// Maximum columns shaped per row. Super-long lines get `…` markers at
    /// the elided edges instead of being laid out in full.
    pub fn visible_columns(mut self, columns: usize) -> Self {
        self.visible_columns = columns.max(1);
        self
    }

    /// The current column window.
    pub fn column_window(&self) -> Range<usize> {
        self.first_column..self.first_column.saturating_add(self.visible_columns)
    }

    /// Paints `line` as the active search hit (bright ink + tinted row).
    pub fn highlight_line(mut self, line: usize) -> Self {
        self.highlight_line = Some(line);
        self
    }

    /// Overrides the input size at which editing is disabled.
    pub fn editable_limit(mut self, bytes: usize) -> Self {
        self.editable_limit = bytes;
        self
    }

    /// Lets truncated rows pan horizontally. Off by default is not an option
    /// for large files: the column window always bounds shaping either way.
    pub fn horizontal_scroll(mut self, on: bool) -> Self {
        self.horizontal_scroll = on;
        self
    }

    /// Key of virtual row `line`. Hand it to `Tx::scroll_into_view` (or
    /// `Tx::scroll_center`) to jump to a search hit even when its row is not
    /// mounted yet.
    pub fn row_key(key: &str, line: usize) -> String {
        format!("{key}-row-{line}")
    }

    /// Builds the Rune element. `key` must be unique in the window.
    ///
    /// Editable mode emits a multi-line text input (below the size limit);
    /// read-only mode emits a `virtual_rows` growth stack that mounts only the
    /// rows the scroll viewport shows.
    /// Whether the input is over the editing threshold (read-only fallback).
    pub fn editing_disabled(&self) -> bool {
        self.text.len() > self.editable_limit
    }

    pub fn build(&self, key: &str) -> El {
        let theme = tok::theme(true);
        let editing_disabled = self.editing_disabled();
        match &self.mode {
            TextViewMode::Editable(edit) if !editing_disabled => rune::text_input(edit, mono())
                .key(key.to_string())
                .label("JSON input")
                .w_full()
                .min_h(240.0)
                .padding(12.0)
                .fill(theme.card)
                .color(theme.text)
                .caret(theme.text, tok::accent(0, true)),
            _ => {
                let snapshot = line_index::snapshot(self.text);
                let rows = self.virtual_rows(key, &snapshot);
                if editing_disabled && matches!(self.mode, TextViewMode::Editable(_)) {
                    col()
                        .key(format!("{key}-frame"))
                        .w_full()
                        .child(large_file_banner())
                        .child(rows)
                } else {
                    rows
                }
            }
        }
    }

    /// [`TextView::build`] plus a one-shot reveal of `line` for search hits.
    /// `S` is the runtime's state type; the handler only needs `Tx`.
    pub fn build_revealing<S: 'static>(&self, key: &str, line: usize) -> El {
        let fired = Rc::new(Cell::new(false));
        let target = Self::row_key(key, line);
        self.build(key).on_visible::<S>(move |_, on, tx| {
            if on && !fired.replace(true) {
                tx.instant();
                tx.scroll_into_view(target.clone());
            }
        })
    }

    fn virtual_rows(&self, key: &str, snapshot: &Arc<Snapshot>) -> El {
        let base = key.to_string();
        let count = snapshot.line_count();
        let opts = Arc::new(RowOpts {
            base: base.clone(),
            first_column: self.first_column,
            visible_columns: self.visible_columns,
            gutter_width: count.to_string().len().max(5),
            line_numbers: self.line_numbers,
            query: self.query.map(str::to_owned),
            highlight_line: self.highlight_line,
            horizontal_scroll: self.horizontal_scroll,
        });

        let key_base = base.clone();
        let build_opts = Arc::clone(&opts);
        let build_snapshot = Arc::clone(snapshot);
        let copy_opts = Arc::clone(&opts);
        let copy_snapshot = Arc::clone(snapshot);

        col()
            .key(base)
            .w_full()
            .growth(GrowthSpec::default())
            .selection_group(true)
            .selection_copy(move |selection| {
                copy_range(&copy_snapshot, &copy_opts, selection)
            })
            .virtual_rows::<AppState>(
                count,
                ROW_HEIGHT,
                move |_state: &AppState, i: usize| Cow::Owned(format!("{key_base}-row-{i}")),
                move |_state: &AppState, i: usize| render_row(&build_snapshot, &build_opts, i),
            )
            .virtual_rows_version(snapshot.version())
    }
}

/// Settings captured by the `'static` `virtual_rows` closures.
struct RowOpts {
    base: String,
    first_column: usize,
    visible_columns: usize,
    gutter_width: usize,
    line_numbers: bool,
    query: Option<String>,
    highlight_line: Option<usize>,
    horizontal_scroll: bool,
}

fn large_file_banner() -> El {
    text(LARGE_FILE_BANNER)
        .font(tok::type_::LABEL)
        .color(oklch(0.84, 0.12, 75.0))
        .padding(8.0)
        .fill(oklcha(0.84, 0.12, 75.0, 0.12))
}

fn render_row(snapshot: &Snapshot, opts: &RowOpts, index: usize) -> El {
    let theme = tok::theme(true);
    let full_line = snapshot.line(index).unwrap_or_default();
    let window = column_window(full_line, opts.first_column, opts.visible_columns);
    let (_, _, _, _, punctuation_ink) = palette();

    let is_hit = opts.highlight_line == Some(index);
    let matches_query = opts.query.as_ref().is_some_and(|query| {
        !query.is_empty() && full_line.to_lowercase().contains(&query.to_lowercase())
    });
    let base_ink = if matches_query || is_hit {
        tok::accent(0, true)
    } else {
        punctuation_ink
    };

    let spans = json_spans(
        &window.display[window.span_offset..window.span_offset + window.span_len],
        window.span_offset,
    );
    let line_el = text(window.display.clone())
        .font(mono())
        .color(base_ink)
        .spans(spans)
        .selectable(true)
        .key(format!("{}-line-{index}", opts.base));

    let mut content = row().w_full().gap(12.0);
    if opts.line_numbers {
        // Fixed-width box + Align::End right-aligns the digits. With a real
        // monospaced face the `>width` padding is exact too; both together keep
        // the gutter aligned even if a proportional fallback ever resolves.
        content = content.child(
            text(format!("{:>width$}", index + 1, width = opts.gutter_width))
                .font(mono())
                .color(theme.text_secondary)
                .w(opts.gutter_width as f32 * CHAR_WIDTH)
                .align(Align::End),
        );
    }

    if opts.horizontal_scroll && (window.left_elided || window.right_elided) {
        let pan = window
            .total_bytes
            .saturating_sub(window.byte_end)
            .min(HORIZONTAL_PAN_COLUMNS);
        let tail = el().w(pan as f32 * CHAR_WIDTH);
        content = content.child(
            row().scroll_x().w_full().child(line_el).child(tail),
        );
    } else {
        content = content.child(line_el);
    }

    if is_hit {
        content = content.fill(oklcha(0.78, 0.13, 150.0, 0.16));
    }
    content
}

/// The shaped slice of one line. `display` includes `…` markers; spans and
/// selection ranges are byte offsets into `display`.
struct ColumnWindow {
    display: String,
    span_offset: usize,
    span_len: usize,
    byte_start: usize,
    byte_end: usize,
    total_bytes: usize,
    left_elided: bool,
    right_elided: bool,
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Cuts `first_column..first_column + columns` out of a line in O(columns),
/// regardless of how long the line is (a 100 MB minified line costs 240 bytes
/// of shaping). Columns are byte offsets, snapped down to a UTF-8 boundary.
fn column_window(line: &str, first_column: usize, columns: usize) -> ColumnWindow {
    let start = floor_boundary(line, first_column.min(line.len()));
    let end = floor_boundary(line, start.saturating_add(columns).min(line.len()));
    let slice = &line[start..end];
    let left_elided = start > 0;
    let right_elided = end < line.len();
    let mut display = String::with_capacity(slice.len() + 6);
    if left_elided {
        display.push('…');
    }
    let span_offset = display.len();
    display.push_str(slice);
    let span_len = slice.len();
    if right_elided {
        display.push('…');
    }
    ColumnWindow {
        display,
        span_offset,
        span_len,
        byte_start: start,
        byte_end: end,
        total_bytes: line.len(),
        left_elided,
        right_elided,
    }
}

fn palette() -> (Color, Color, Color, Color, Color) {
    (
        oklch(0.72, 0.14, 250.0), // object keys
        oklch(0.82, 0.12, 145.0), // strings
        oklch(0.84, 0.13, 65.0),  // numbers
        oklch(0.76, 0.15, 320.0), // true / false / null
        oklch(0.66, 0.02, 260.0), // punctuation
    )
}

/// JSON token spans for one visible line. `offset` shifts byte ranges into
/// the display string (which may have a leading `…` marker). Spans only set
/// glyph ink, so Rune does not reshape the text to paint them.
fn json_spans(slice: &str, offset: usize) -> Vec<Span> {
    let (key_ink, string_ink, number_ink, literal_ink, punctuation_ink) = palette();
    let bytes = slice.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'"' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                let mut look = i;
                while look < bytes.len() && (bytes[look] == b' ' || bytes[look] == b'\t') {
                    look += 1;
                }
                let ink = if look < bytes.len() && bytes[look] == b':' {
                    key_ink
                } else {
                    string_ink
                };
                spans.push(Span::new(offset + start, offset + i).color(ink));
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                spans.push(Span::new(offset + start, offset + i).color(number_ink));
            }
            b't' | b'f' | b'n' => {
                let literal: &[u8] = match bytes[i] {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                if bytes[i..].starts_with(literal) {
                    i += literal.len();
                    spans.push(Span::new(offset + start, offset + i).color(literal_ink));
                } else {
                    i += 1;
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                i += 1;
                spans.push(Span::new(offset + start, offset + i).color(punctuation_ink));
            }
            _ => i += 1,
        }
    }
    spans
}

/// Maps `(line, drawn byte range)` pairs back onto the source document.
/// A range covering the whole drawn window copies the whole (possibly elided)
/// line, so `select + copy range` is not limited to what was shaped.
fn copy_lines(
    snapshot: &Snapshot,
    first_column: usize,
    visible_columns: usize,
    ranges: &[(usize, Range<usize>)],
) -> Option<String> {
    let mut out = String::new();
    let mut any = false;
    for (line_index, range) in ranges {
        let Some(line) = snapshot.line(*line_index) else {
            continue;
        };
        let window = column_window(line, first_column, visible_columns);
        let from_display = range.start.max(window.span_offset);
        let to_display = range.end.min(window.span_offset + window.span_len);
        if from_display >= to_display {
            continue;
        }
        let whole_window = from_display == window.span_offset
            && to_display == window.span_offset + window.span_len;
        let (from, to) = if whole_window {
            (0, line.len())
        } else {
            (
                window.byte_start + (from_display - window.span_offset),
                window.byte_start + (to_display - window.span_offset),
            )
        };
        let Some(part) = line.get(from..to) else {
            continue;
        };
        if any {
            out.push('\n');
        }
        out.push_str(part);
        any = true;
    }
    any.then_some(out)
}

/// Rune selection → source ranges. Row text keys are `<base>-line-<i>`;
/// `SelectedText::range()` is a byte range into the drawn window string.
fn copy_range(snapshot: &Snapshot, opts: &RowOpts, selection: &TextSelection) -> Option<String> {
    let prefix = format!("{}-line-", opts.base);
    let ranges: Vec<(usize, Range<usize>)> = selection
        .texts()
        .iter()
        .filter(|text| text.is_selected())
        .filter_map(|text| {
            let rest = text.key()?.strip_prefix(&prefix)?;
            Some((rest.parse::<usize>().ok()?, text.range()))
        })
        .collect();
    copy_lines(
        snapshot,
        opts.first_column,
        opts.visible_columns,
        &ranges,
    )
}

fn mono() -> tok::TextStyle {
    tok::TextStyle::new(12.0, 400.0).mono()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_clamps_to_the_document() {
        let view = TextView::read_only("a\nb\nc")
            .first_line(1)
            .visible_lines(5);
        assert_eq!(view.line_count(), 3);
        assert_eq!(view.visible_range(), 1..3);
        assert_eq!(view.line(1), Some("b"));
        assert_eq!(view.line(9), None);
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        let view = TextView::read_only("");
        assert_eq!(view.line_count(), 1);
        assert_eq!(view.line(0), Some(""));
    }

    #[test]
    fn first_line_past_the_end_is_empty() {
        let view = TextView::read_only("a\nb").first_line(10).visible_lines(5);
        assert_eq!(view.visible_range(), 2..2);
    }

    #[test]
    fn column_window_shapes_only_a_slice_of_a_huge_line() {
        let line = "x".repeat(100_000_000);
        let window = column_window(&line, 40_000_000, 240);
        assert_eq!(window.span_len, 240);
        assert_eq!(window.byte_start, 40_000_000);
        assert_eq!(window.byte_end, 40_000_240);
        assert!(window.left_elided && window.right_elided);
        assert!(window.display.starts_with('…'));
        assert!(window.display.ends_with('…'));
        assert_eq!(window.display.len(), 240 + 6); // two 3-byte markers
    }

    #[test]
    fn column_window_respects_utf8_boundaries() {
        let line = "ééééé";
        let window = column_window(line, 3, 4);
        assert_eq!(window.byte_start, 2);
        assert_eq!(window.byte_end, 6);
        assert_eq!(window.display, "…éé…");
    }

    #[test]
    fn column_window_short_line_has_no_markers() {
        let window = column_window("{\"a\": 1}", 0, 240);
        assert!(!window.left_elided && !window.right_elided);
        assert_eq!(window.display, "{\"a\": 1}");
    }

    #[test]
    fn syntax_spans_classify_every_json_token_kind() {
        let line = r#"{"key": "str", "n": -12.5, "b": true, "z": null}"#;
        let spans = json_spans(line, 0);
        let colours: Vec<Color> = spans.iter().filter_map(|s| s.color).collect();
        assert!(colours.contains(&palette().0), "no key span");
        assert!(colours.contains(&palette().1), "no string span");
        assert!(colours.contains(&palette().2), "no number span");
        assert!(colours.contains(&palette().3), "no literal span");
        assert!(colours.contains(&palette().4), "no punctuation span");
        assert!(spans.windows(2).all(|w| w[0].end <= w[1].start));
    }

    #[test]
    fn syntax_spans_shift_for_a_leading_elision_marker() {
        let spans = json_spans(r#""k": 1"#, 3);
        assert_eq!(spans[0].start, 3);
        assert!(spans.windows(2).all(|w| w[0].end <= w[1].start));
    }

    #[test]
    fn row_key_matches_the_virtual_rows_key() {
        assert_eq!(TextView::row_key("output-text", 1_234_567), "output-text-row-1234567");
    }

    #[test]
    fn copy_range_maps_window_bytes_back_to_source() {
        let snapshot = Snapshot::new(Arc::from("abcdefghij\nsecond"));
        assert_eq!(
            copy_lines(&snapshot, 0, 4, &[(0, 1..3)]).as_deref(),
            Some("bc")
        );
        // Selecting the whole shaped window copies the whole elided line.
        assert_eq!(
            copy_lines(&snapshot, 0, 4, &[(0, 0..4)]).as_deref(),
            Some("abcdefghij")
        );
        // With a leading "…" marker, display offsets are shifted by 3 bytes.
        assert_eq!(
            copy_lines(&snapshot, 4, 4, &[(0, 4..6)]).as_deref(),
            Some("fg")
        );
        assert_eq!(
            copy_lines(&snapshot, 4, 4, &[(0, 3..7)]).as_deref(),
            Some("abcdefghij")
        );
        assert_eq!(copy_lines(&snapshot, 0, 4, &[]), None);
    }

    #[test]
    fn editable_threshold_switches_to_the_read_only_view() {
        let edit = TextEdit::default();
        let huge = "x".repeat(64);
        let large = TextView::editable(&huge, &edit).editable_limit(32);
        assert!(large.editing_disabled());
        let small = TextView::editable("{}", &edit);
        assert!(!small.editing_disabled());
        // Both build paths are exercised by the integration tests (element tree).
        let _ = large.build("editor-large");
        let _ = small.build("editor-small");
    }
}
