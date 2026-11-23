//! Virtualized text component.
//!
//! Editable input uses Rune's [`TextEdit`] (the caret/selection live in app
//! state). Read-only output is a [`El::virtual_rows`] list over a
//! [`crate::line_index::LineIndex`]: only the mounted rows are built, so a
//! 100 MB / 2M-line document costs O(visible rows) per frame instead of
//! shaping the whole file.
//!
//! ```text
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
//! full index through `line_index::prewarm_off_thread` under the document's
//! revision. Above
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
//! `AGENTS.md`. The line-number gutter is a fixed-width box with
//! `Align::End`, so digits are right-aligned.

use std::borrow::Cow;
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use rune::{col, el, oklch, oklcha, row, text, tok, Align, Color, El, GrowthSpec, Span, TextEdit, TextSelection};

use crate::fold_index::{self, FoldView};
use crate::line_index::{self, Snapshot};
use crate::state::{AppState, Msg};

/// Estimated height of one monospace row; unvisited virtual rows use this.
pub const ROW_HEIGHT: f32 = 16.0;
/// Visible-line count at or below which the read-only view builds every row
/// directly instead of a virtual list. A folded large document usually lands
/// here (a 2M-line file folds to a few rows), which keeps the common fold
/// action from paying Rune's O(visible rows) key-list rebuild.
const PLAIN_ROWS_LIMIT: usize = 512;
/// Nominal 12pt monospace advance, used only as a last-resort fallback. The
/// value actually used at render time is [`char_width`], measured from Rune's
/// active text system, so it is correct whether the mono slot resolves to IBM
/// Plex Mono (7.2pt at 12pt), a DejaVu fallback (~7.2245), Consolas, Menlo,
/// etc. No code path assumes a particular face.
pub const CHAR_WIDTH: f32 = 7.2;

/// Advance of one monospace glyph at 12pt, measured from Rune's active text
/// system (`rune::text::advance`). A single `'0'` gives the exact column width
/// because every mono face is fixed pitch, so gutter boxes and horizontal
/// spacers follow whatever face the app actually registered (IBM Plex Mono,
/// DejaVu Sans Mono, Consolas, Menlo, …) without hard-coding its metrics. Falls
/// back to [`CHAR_WIDTH`] only before any face has been registered. Rune caches
/// the shaped line, so a repeat call is a hash lookup, not a shaping pass.
pub fn char_width() -> f32 {
    let measured = rune::text::advance("0", mono());
    if measured.is_finite() && measured > 0.0 {
        measured
    } else {
        CHAR_WIDTH
    }
}

/// Height of one monospace line at 12pt, measured from the same text system.
/// The editable error marker uses it instead of assuming a fixed 16pt row.
pub fn line_height() -> f32 {
    let measured = rune::text::measure("0", &[], mono(), None).1;
    if measured.is_finite() && measured > 0.0 {
        measured
    } else {
        ROW_HEIGHT
    }
}
/// Default column window width. A minified 100 MB single line shapes only
/// this many columns, never the whole line.
pub const DEFAULT_VISIBLE_COLUMNS: usize = 240;
/// Above this input size, editing is disabled and the read-only virtual view
/// takes over. Raised from 2 MiB to 5 MiB after rp-editbench measured Rune's
/// native `TextEdit` whole-buffer re-shape: pretty-printed 5 MiB has p95
/// keystroke 12.26 ms (< 16 ms), 10 MiB has 24.33 ms (too slow).
pub const EDITABLE_LIMIT_BYTES: usize = 5 * 1024 * 1024;
/// Longest single line still allowed in editable mode. A minified/single-line
/// document is wrap-laid-out in full on every edit: rp-editbench measured
/// 250 ms/keystroke already at 2 MiB (16 ms budget reached near ~128 KiB).
/// Such documents stay on the read-only virtualized path regardless of total
/// size; the longest line comes from the line index's single build pass.
pub const LONG_LINE_LIMIT_BYTES: usize = 128 * 1024;
/// Tail of the "large file" banner, kept as a stable marker for tests. The
/// full banner text prepends the reason and size; see `large_file_banner`.
pub const LARGE_FILE_BANNER: &str = "editing disabled — Format/Minify/Search still work";
/// How far a truncated row can be panned horizontally, in columns.
const HORIZONTAL_PAN_COLUMNS: usize = 8_192;
/// Red ink / tint / caret used by `TextView::error_marker`.
const ERROR_INK: Color = Color { r: 0.86, g: 0.22, b: 0.22, a: 1.0 };
const ERROR_TINT: Color = Color { r: 0.30, g: 0.06, b: 0.08, a: 0.20 };
const ERROR_CARET_BG: Color = Color { r: 0.60, g: 0.12, b: 0.14, a: 0.55 };

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
    revision: Option<u64>,
    /// 1-based error line/column for `error_marker` (additive Validate API).
    error_line: Option<usize>,
    error_column: Option<usize>,
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
            revision: None,
            error_line: None,
            error_column: None,
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
            revision: None,
            error_line: None,
            error_column: None,
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

    /// Additive Validate marker: highlights the 1-based error `line` (red row
    /// tint + gutter dot) and draws a caret/underline at the 1-based `column`,
    /// in both editable and read-only/virtualized modes. The caller reveals it
    /// with the existing [`TextView::row_key`] + `Tx::scroll_into_view`; any
    /// folds covering the line are unfolded first (see `fold_index::note_reveal`).
    pub fn error_marker(mut self, line: usize, column: usize) -> Self {
        self.error_line = Some(line.max(1));
        self.error_column = Some(column.max(1));
        self
    }

    /// Keys the shared line index by the reducer-assigned document revision.
    /// Without it the index is keyed by the rendered buffer's identity, which
    /// is enough for one-off views but not for swapping documents in place.
    pub fn revision(mut self, revision: u64) -> Self {
        self.revision = (revision != 0).then_some(revision);
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

    /// Key of virtual row `line` (a 0-based document line, not a visual row).
    /// Hand it to `Tx::scroll_into_view` (or `Tx::scroll_center`) to jump to a
    /// search hit even when its row is not mounted yet. As an additive side
    /// effect it arms `fold_index::note_reveal(line)`: the next `build` unfolds
    /// any ancestor folds covering the line, so a jump into a folded region
    /// always lands on a mounted row. Callers that only compute a key (tests,
    /// pure lookups) are unaffected — with no folds the reveal is a no-op.
    pub fn row_key(key: &str, line: usize) -> String {
        fold_index::note_reveal(key, line);
        format!("{key}-row-{line}")
    }

    /// Builds the Rune element. `key` must be unique in the window.
    ///
    /// Editable mode emits a multi-line text input (below the size limit);
    /// read-only mode emits a `virtual_rows` growth stack that mounts only the
    /// rows the scroll viewport shows, or plain keyed rows when a fold leaves
    /// few visible (`PLAIN_ROWS_LIMIT`).
    ///
    /// Whether editing is disabled: total size over the limit, or a single
    /// line longer than [`LONG_LINE_LIMIT_BYTES`] (minified input).
    ///
    /// Both facts are O(1) from the cached line index
    /// ([`Snapshot::max_line_len_exact`], computed in the same pass as the line
    /// starts), so the per-frame view never scans the buffer. A cold index for
    /// a document under [`line_index::LARGE_DOCUMENT_BYTES`] is built once on
    /// the UI thread (bounded by that constant); larger documents stay
    /// provisional and are classified as large files until the index warms.
    pub fn editing_disabled(&self) -> bool {
        self.disable_reason(&self.snapshot()).is_some()
    }

    fn disable_reason(&self, snapshot: &Snapshot) -> Option<EditDisabled> {
        let n = self.text.len();
        // A single very long line is the worse failure mode (Rune wraps and
        // re-shapes the whole line per keystroke), so report it first. The
        // longest line is only known once the full index exists; a provisional
        // index belongs to a document above the readable size limit anyway.
        if let Some(max_line) = snapshot.max_line_len_exact() {
            if max_line > LONG_LINE_LIMIT_BYTES {
                return Some(EditDisabled::LongLine(n));
            }
        }
        (n > self.editable_limit).then_some(EditDisabled::Large(n))
    }

    /// The cached line index for this document (revision-keyed when set).
    fn snapshot(&self) -> Arc<Snapshot> {
        match self.revision {
            Some(revision) => line_index::snapshot_for_revision(self.text, revision),
            None => line_index::snapshot(self.text),
        }
    }

    pub fn build(&self, key: &str) -> El {
        let theme = tok::theme(true);
        // One cached line-index lookup serves both the editable gate and the
        // read-only row window.
        let snapshot = self.snapshot();
        let disabled = self.disable_reason(&snapshot);
        match &self.mode {
            TextViewMode::Editable(edit) if disabled.is_none() => {
                let input = rune::text_input(edit, mono())
                    .key(key.to_string())
                    .label("JSON input")
                    .w_full()
                    .min_h(240.0)
                    .padding(12.0)
                    .fill(theme.card)
                    .color(theme.text)
                    .caret(theme.text, tok::accent(0, true));
                match (self.error_line, self.error_column) {
                    (Some(line), Some(column)) => {
                        editable_error_view(input, key, self.text, line, column)
                    }
                    _ => input,
                }
            }
            _ => {
                let fold_key = fold_index::key(self.revision.unwrap_or(0), self.text);
                let fold = fold_index::view_for(self.text, fold_key, snapshot.line_count(), key);
                let rows = self.virtual_rows(key, &snapshot, &fold);
                let editable_fallback = matches!(self.mode, TextViewMode::Editable(_));
                if let (Some(reason), true) = (disabled, editable_fallback) {
                    col()
                        .key(format!("{key}-frame"))
                        .w_full()
                        .child(large_file_banner(reason))
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
        fold_index::note_reveal(key, line);
        let fired = Rc::new(Cell::new(false));
        let target = Self::row_key(key, line);
        self.build(key).on_visible::<S>(move |_, on, tx| {
            if on && !fired.replace(true) {
                tx.instant();
                tx.scroll_into_view(target.clone());
            }
        })
    }

    fn virtual_rows(&self, key: &str, snapshot: &Arc<Snapshot>, fold: &Arc<FoldView>) -> El {
        let base = key.to_string();
        let count = fold.visible_count();
        let opts = Arc::new(RowOpts {
            base: base.clone(),
            first_column: self.first_column,
            visible_columns: self.visible_columns,
            gutter_width: snapshot.line_count().to_string().len().max(5),
            line_numbers: self.line_numbers,
            query: self.query.map(str::to_owned),
            highlight_line: self.highlight_line,
            horizontal_scroll: self.horizontal_scroll,
            char_width: char_width(),
            fold: Arc::clone(fold),
            fold_key: fold_index::key(self.revision.unwrap_or(0), self.text),
            error_line: self.error_line,
            error_column: self.error_column,
        });

        let key_base = base.clone();
        let key_fold = Arc::clone(fold);
        let build_opts = Arc::clone(&opts);
        let build_snapshot = Arc::clone(snapshot);
        let copy_opts = Arc::clone(&opts);
        let copy_snapshot = Arc::clone(snapshot);
        // Re-key rows when the document, the fold generation or the error
        // marker changes. A fold/unfold also changes `count` outright.
        let version = (
            snapshot.version(),
            fold.epoch(),
            self.error_line,
            self.error_column,
        );

        let container = col()
            .key(base.clone())
            .w_full()
            .selection_group(true)
            .selection_copy(move |selection| {
                copy_range(&copy_snapshot, &copy_opts, selection)
            });
        if count <= PLAIN_ROWS_LIMIT {
            // A folded 2M-line document can leave only a handful of visible
            // rows. Building them directly avoids Rune's keyed virtual list
            // entirely, whose key list is O(visible rows) on every count
            // change — the one part of folding that is not O(log n).
            let rows: Vec<El> = (0..count)
                .map(|i| {
                    render_row(snapshot, &opts, i)
                        .key(format!("{base}-row-{}", fold.visual_to_doc(i)))
                })
                .collect();
            return container.children(rows);
        }
        container
            .growth(GrowthSpec::default())
            .virtual_rows::<AppState>(
                count,
                ROW_HEIGHT,
                move |_state: &AppState, i: usize| {
                    // Key each row by its document line (not its visual row),
                    // so scroll_into_view and line numbers stay stable across
                    // folds.
                    Cow::Owned(format!("{key_base}-row-{}", key_fold.visual_to_doc(i)))
                },
                move |_state: &AppState, i: usize| render_row(&build_snapshot, &build_opts, i),
            )
            .virtual_rows_version(version)
    }
}

/// Why editable mode fell back to the read-only virtualized view.
#[derive(Clone, Copy)]
enum EditDisabled {
    Large(usize),
    LongLine(usize),
}

/// A folding command shared by the toolbar buttons and the keyboard layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldAction {
    /// Collapse every foldable container.
    FoldAll,
    /// Clear every fold.
    UnfoldAll,
    /// Collapse containers at nesting level `N` (1..=3) or deeper.
    Level(u32),
    /// Toggle the region at the hovered line (else the innermost enclosing).
    ToggleAtHover,
    /// Open the region at the hovered line if it is folded.
    UnfoldAtHover,
}

/// Applies `action` to the active tab's formatted Output and queues a short
/// status message. Folding is keyed by the tab's `output_revision`, so each
/// document keeps its own fold set. A no-op (with a status hint) when there is
/// no output.
pub fn apply_fold_action(state: &mut AppState, action: FoldAction) {
    let Some(tab) = state.active_tab() else {
        return;
    };
    let key = fold_index::key(tab.output_revision, &tab.output);
    let empty = tab.output.is_empty();
    if empty {
        state.dispatch(Msg::Status("Nothing to fold yet — Format first".into()));
        return;
    }
    if let Some(tab) = state.active_tab() {
        fold_index::ensure_for(&tab.output, key);
    }
    let status = match action {
        FoldAction::FoldAll => {
            let count = fold_index::fold_all(key);
            format!("Folded {count} regions")
        }
        FoldAction::UnfoldAll => {
            let count = fold_index::unfold_all(key);
            format!("Unfolded {count} regions")
        }
        FoldAction::Level(level) => {
            let count = fold_index::fold_level(key, level);
            format!("Folded to level {level} ({count} regions)")
        }
        FoldAction::ToggleAtHover => {
            let line = fold_index::hovered(key).unwrap_or(0) as u32;
            match fold_index::toggle_near(key, line) {
                Some((region, now)) => format!(
                    "{} {} (lines {}-{})",
                    if now { "Folded" } else { "Unfolded" },
                    if region.is_array { "array" } else { "object" },
                    region.start + 1,
                    region.end + 1
                ),
                None => "No foldable region here".to_string(),
            }
        }
        FoldAction::UnfoldAtHover => {
            let line = fold_index::hovered(key).unwrap_or(0) as u32;
            match fold_index::unfold_near(key, line) {
                Some(region) => format!(
                    "Unfolded {} (lines {}-{})",
                    if region.is_array { "array" } else { "object" },
                    region.start + 1,
                    region.end + 1
                ),
                None => "Nothing folded here".to_string(),
            }
        }
    };
    state.fold_epoch = state.fold_epoch.wrapping_add(1);
    state.dispatch(Msg::Status(status));
}

fn mib(bytes: usize) -> f32 {
    bytes as f32 / (1024.0 * 1024.0)
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
    /// Measured mono advance (from `char_width`), not a hard-coded constant.
    char_width: f32,
    fold: Arc<FoldView>,
    fold_key: u64,
    error_line: Option<usize>,
    error_column: Option<usize>,
}

fn large_file_banner(reason: EditDisabled) -> El {
    let (label, tint) = match reason {
        EditDisabled::Large(n) => (
            format!("Large file ({:.1} MB) — {LARGE_FILE_BANNER}", mib(n)),
            oklcha(0.84, 0.12, 75.0, 0.12),
        ),
        EditDisabled::LongLine(n) => (
            format!("Very long line ({:.1} MB on one line) — Format to edit", mib(n)),
            oklcha(0.72, 0.15, 25.0, 0.14),
        ),
    };
    text(label)
        .font(tok::type_::LABEL)
        .color(oklch(0.84, 0.12, 75.0))
        .padding(8.0)
        .fill(tint)
}

/// Byte range of the 1-based `line` in `text`, line ending excluded.
fn line_byte_range(text: &str, line: usize) -> Option<(usize, usize)> {
    let mut start = 0usize;
    for (i, chunk) in text.split_inclusive('\n').enumerate() {
        let end = start + chunk.trim_end_matches(['\n', '\r']).len();
        if i + 1 == line {
            return Some((start, end));
        }
        start += chunk.len();
    }
    None
}

/// Editable-mode error marker. The exact line tint and column underline are
/// `TextEdit` decorations (`Span::bg` / `Span::underline`), which Rune maps
/// through the field's own layout, wrapping and IME preedit, so they stay on
/// the right glyphs without assuming row geometry or the caller's padding.
///
/// The gutter dot and the `line N, column C` label sit in a strip above the
/// field; the strip carries the reveal key, so
/// `Tx::scroll_into_view(TextView::row_key(key, line - 1))` scrolls the field
/// into view. (A text field is one scroll target; the accurate position is the
/// underline inside it.)
fn editable_error_view(input: El, key: &str, source: &str, line: usize, column: usize) -> El {
    let mut decorations = Vec::new();
    if let Some((start, end)) = line_byte_range(source, line) {
        decorations.push(Span::new(start, end).bg(ERROR_TINT, 1.0));
        let at = start + column.saturating_sub(1);
        if at < end {
            decorations.push(
                Span::new(at, (at + 1).min(end))
                    .underline(ERROR_INK)
                    .color(ERROR_INK),
            );
        }
    }
    let strip = row()
        .key(format!("{key}-row-{}", line.saturating_sub(1)))
        .w_full()
        .h(line_height())
        .gap(6.0)
        .center_y()
        .child(
            el()
                .key(format!("{key}-error-dot"))
                .w(6.0)
                .h(6.0)
                .radius(3.0)
                .fill(ERROR_INK)
                .a11y_hidden(),
        )
        .child(
            text(format!("line {line}, column {column}"))
                .key(format!("{key}-error-label"))
                .font(tok::type_::LABEL)
                .color(ERROR_INK),
        );
    col()
        .w_full()
        .min_h(240.0)
        .gap(4.0)
        .child(strip)
        .child(
            input
                .decorations(decorations)
                .border(1.0, ERROR_INK)
                .radius(6.0),
        )
}

/// A folded opening line becomes `"key": {…}` / `[…]`.
fn fold_placeholder(line: &str, is_array: bool) -> String {
    let (open, close) = if is_array { ('[', ']') } else { ('{', '}') };
    if let Some(pos) = line.rfind(open) {
        let mut out = String::with_capacity(line.len() + 4);
        out.push_str(&line[..=pos]);
        out.push('…');
        out.push(close);
        out
    } else {
        format!("{open}…{close}")
    }
}

/// Display-byte range of a 1-based error column, or `None` when the column is
/// outside the shaped horizontal window.
fn error_column_range(window: &ColumnWindow, column: usize) -> Option<(usize, usize)> {
    let c = column.saturating_sub(1);
    if c < window.byte_start || c >= window.byte_end {
        return None;
    }
    let start = window.span_offset + (c - window.byte_start);
    let end = (start + 1).min(window.span_offset + window.span_len);
    (start < end).then_some((start, end))
}

/// Fixed-width fold gutter: a clickable ▸/▾ arrow on foldable lines, blank
/// otherwise. Every row reserves the same width so the document line numbers
/// stay aligned.
fn fold_gutter(
    opts: &RowOpts,
    doc_line: usize,
    region: Option<fold_index::Region>,
    folded: bool,
) -> El {
    let w = opts.char_width * 1.6;
    let slot = el().w(w).h(ROW_HEIGHT).center();
    let Some(region) = region else {
        return slot;
    };
    let glyph = if folded { "▸" } else { "▾" };
    let start = region.start;
    let key = opts.fold_key;
    let label = if folded { "Unfold region" } else { "Fold region" };
    // The whole gutter cell is the hit target; the arrow is its label.
    slot.on_press(move |state: &mut AppState| {
        fold_index::toggle(key, start);
        state.fold_epoch = state.fold_epoch.wrapping_add(1);
    })
    .child(
        text(glyph)
            .key(format!("{}-fold-{doc_line}", opts.base))
            .font(mono())
            .color(oklch(0.72, 0.02, 250.0))
            .role("button")
            .label(label),
    )
}

/// Red dot reserved for `TextView::error_marker`, kept in its own gutter cell
/// so an error on a foldable line still shows both the dot and the arrow. The
/// cell is reserved on every row, blank when the row is fine, so a single
/// marked line never shifts its own text out of the column.
fn error_gutter(opts: &RowOpts, doc_line: usize, is_error: bool) -> El {
    let slot = el().w(opts.char_width * 1.2).h(ROW_HEIGHT).center();
    if !is_error {
        return slot;
    }
    slot.child(
        el()
            .key(format!("{}-error-dot-{doc_line}", opts.base))
            .w(6.0)
            .h(6.0)
            .radius(3.0)
            .fill(ERROR_INK)
            .a11y_hidden(),
    )
}

fn render_row(snapshot: &Snapshot, opts: &RowOpts, visual_row: usize) -> El {
    let theme = tok::theme(true);
    let index = opts.fold.visual_to_doc(visual_row); // 0-based document line
    let full_line = snapshot.line(index).unwrap_or_default();
    let region = opts.fold.region(index);
    let folded = opts.fold.is_folded(index);
    let is_error = opts.error_line == Some(index + 1);
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

    // A folded row shows a compact summary instead of its opening line.
    let display_source: Cow<'_, str> = if folded {
        Cow::Owned(fold_placeholder(full_line, region.is_some_and(|r| r.is_array)))
    } else {
        Cow::Borrowed(full_line)
    };
    let window = column_window(&display_source, opts.first_column, opts.visible_columns);
    let mut spans = json_spans(
        &window.display[window.span_offset..window.span_offset + window.span_len],
        window.span_offset,
    );
    if is_error {
        if let Some(column) = opts.error_column {
            if let Some((s, e)) = error_column_range(&window, column) {
                spans.push(Span::new(s, e).underline(ERROR_INK).bg(ERROR_CARET_BG, 1.0));
            }
        }
    }
    let mut line_el = text(window.display.clone())
        .font(mono())
        .color(base_ink)
        .spans(spans)
        .selectable(true)
        .key(format!("{}-line-{index}", opts.base));
    if folded {
        if let Some(r) = region {
            // Clicking the placeholder line (`"key": {…}`) unfolds the block.
            let start = r.start;
            let key = opts.fold_key;
            line_el = line_el.selectable(false).on_press(move |state: &mut AppState| {
                fold_index::toggle(key, start);
                state.fold_epoch = state.fold_epoch.wrapping_add(1);
            });
        }
    }

    let doc_line = index;
    let opts_fold_key = opts.fold_key;
    let mut content = row()
        .w_full()
        .gap(12.0)
        // The keyboard fold commands have no text cursor in this view; the
        // last hovered row is their target (see `fold_index::note_hover`).
        .on_hover_change(move |_state: &mut AppState, on: bool, _tx: &mut rune::Tx| {
            if on {
                fold_index::note_hover(opts_fold_key, doc_line);
            }
        });
    if opts.line_numbers {
        // Right-aligned document line numbers. Folded lines are skipped, so
        // these remain the real document line numbers.
        content = content.child(
            text(format!("{:>width$}", index + 1, width = opts.gutter_width))
                .font(mono())
                .color(theme.text_secondary)
                .w(opts.gutter_width as f32 * opts.char_width)
                .align(Align::End),
        );
    }
    content = content.child(error_gutter(opts, index, is_error));
    content = content.child(fold_gutter(opts, index, region, folded));

    if opts.horizontal_scroll && (window.left_elided || window.right_elided) {
        let pan = window
            .total_bytes
            .saturating_sub(window.byte_end)
            .min(HORIZONTAL_PAN_COLUMNS);
        let tail = el().w(pan as f32 * opts.char_width);
        content = content.child(row().scroll_x().w_full().child(line_el).child(tail));
    } else {
        content = content.child(line_el);
    }

    if folded {
        if let Some(r) = region {
            let noun = match (r.is_array, r.child_count) {
                (true, 1) => "item",
                (true, _) => "items",
                (false, 1) => "key",
                (false, _) => "keys",
            };
            let start = r.start;
            let key = opts.fold_key;
            content = content.child(
                text(format!("· {} {}", r.child_count, noun))
                    .key(format!("{}-fold-badge-{index}", opts.base))
                    .font(mono())
                    .color(super::MUTED)
                    .label("Unfold region")
                    .on_press(move |state: &mut AppState| {
                        // The placeholder summary is the unfold target.
                        fold_index::toggle(key, start);
                        state.fold_epoch = state.fold_epoch.wrapping_add(1);
                    }),
            );
        }
    }

    if is_error {
        content = content.fill(ERROR_TINT);
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
    fn error_column_underlines_the_identifier_start() {
        let input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3\n}";
        let error = crate::json_ops::validate(input).expect_err("invalid fixture");
        assert_eq!((error.line, error.column), (3, 8));
        let window = column_window(input.lines().nth(2).unwrap(), 0, 240);
        let (start, end) = error_column_range(&window, error.column).expect("visible error");
        assert_eq!(&window.display[start..end], "o");
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
