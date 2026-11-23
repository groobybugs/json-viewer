//! Visible-line editor view over the rope (rp-editor E2).
//!
//! Renders only the lines the scroll viewport shows, as plain keyed rows
//! between two spacer elements — no `virtual_rows`, whose O(line count) key
//! rematerialization on every line-count change would make Enter/paste on a
//! multi-million-line document cost hundreds of milliseconds. The visible
//! window comes from tracked layout reads ([`Cx::window_layout_with`]): the
//! top spacer's window y against the viewport's y gives the scroll offset,
//! and the view re-runs whenever scrolling moves it. Row heights are exactly
//! [`ROW_HEIGHT`] (16.0), so every spacer height is a multiple of 16 and f32
//! layout positions stay exact past 4M lines.
//!
//! Very long lines are never materialized: each row slices only its visible
//! column window out of the rope ([`Buffer::slice_bytes`]) and tokenizes that
//! window, with a bounded lookback deciding whether the window starts inside
//! a JSON string. A small content-hash cache keeps unchanged windows from
//! being re-sliced/re-tokenized; Rune's own shape cache keys on the display
//! string, so unchanged lines are not reshaped either.
//!
//! Mouse: click places the caret, shift-click extends, drag selects by
//! char/word/line (double/triple click), horizontal wheel pans the column
//! window. Caret and selection are drawn inline (split text + bg spans);
//! vertical caret reveal is the runner's job (`EditorState::reveal`).

use std::cell::RefCell;
use std::ops::Range;

use rune::{col, el, row, text, tok, Align, Color, Cx, Drag, El, Phase, Span, Tx, WheelEvent};
use rustc_hash::{FxHashMap, FxHasher};
use std::hash::{Hash, Hasher};

use crate::state::{AppState, Tab};
use crate::ui::text_view::{char_width, ROW_HEIGHT};
use crate::ui::{ACCENT, EDITOR_BG, MUTED, TEXT};

use super::state::{self, DragMode, EditorState};

/// Default shaped width per row (columns); the view narrows it to the
/// viewport so rows never overflow into native horizontal scrolling.
pub const VISIBLE_COLUMNS: usize = 240;
/// Editor container padding (points); mouse math shares it.
const PAD: f32 = 12.0;
/// Rows built beyond the viewport on each side.
const OVERSCAN: usize = 8;
/// Gap between the gutter cells and the text (points).
const GAP: f32 = 6.0;
/// Fallback viewport size before the first layout read lands.
const FALLBACK_VIEWPORT: (f32, f32) = (1280.0, 600.0);

const ERROR_INK: Color = Color { r: 0.86, g: 0.22, b: 0.22, a: 1.0 };
const ERROR_TINT: Color = Color { r: 0.30, g: 0.06, b: 0.08, a: 0.20 };
const ERROR_CARET_BG: Color = Color { r: 0.60, g: 0.12, b: 0.14, a: 0.55 };

fn mono() -> tok::TextStyle {
    tok::TextStyle::new(12.0, 400.0).mono()
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

/// Row key of a 0-based document line; matches `TextView::row_key(base, l)`
/// so existing reveal callers (`validation_panel`) keep working.
pub fn row_key(base: &str, line: usize) -> String {
    format!("{base}-row-{line}")
}

/// Builds the editor for `tab` under `base` (`editor-{id}`). The caller puts
/// it inside the `document-scroll-{id}` scroll viewport.
pub fn build(_state: &AppState, cx: &mut Cx<AppState>, tab: &Tab, base: &str) -> El {
    let id = tab.id;
    let cw = char_width();
    let error = tab.validation_error().map(|e| (e.line, e.column));

    // Tracked layout reads: re-run the view when scrolling moves the editor
    // container or the viewport resizes/moves. First frame reads None →
    // window at 0. The container itself is the anchor: its top is the
    // content top, and unlike a zero-height spacer it is always laid out.
    let scroll_key = format!("document-scroll-{id}");
    let viewport = cx.window_layout_with(&scroll_key, |r| (r[1], r[2], r[3]));
    let anchor_y = cx.window_layout_with(base, |r| r[1]);
    let (vp_top, vp_w, vp_h) = viewport.unwrap_or((0.0, FALLBACK_VIEWPORT.0, FALLBACK_VIEWPORT.1));
    let scroll_y = (vp_top - anchor_y.unwrap_or(vp_top)).max(0.0);

    let (lines, first, last, rows, gutter_w) = state::with(id, &tab.input, tab.input_revision, |ed| {
        ed.scroll_y = scroll_y;
        ed.viewport_h = vp_h;
        let lines = ed.buffer.len_lines();
        let gutter_w = lines.to_string().len().max(5);
        let text_w = vp_w - 2.0 * PAD - gutter_w as f32 * cw - cw * 1.2 - GAP - 2.0;
        let columns = ((text_w / cw).floor() as usize).max(16);
        ed.visible_columns = columns;
        let first_visible = (scroll_y / ROW_HEIGHT).floor() as usize;
        let visible = (vp_h / ROW_HEIGHT).ceil() as usize + 1;
        let first = first_visible.saturating_sub(OVERSCAN);
        let last = lines.min(first_visible + visible + OVERSCAN);
        let caret_line = ed.buffer.byte_to_line(ed.caret);
        let mut rows = Vec::with_capacity(last - first);
        for line in first..last {
            rows.push(render_row(ed, id, base, line, gutter_w, cw, columns, caret_line, error));
        }
        (lines, first, last, rows, gutter_w)
    });

    let strip_h = if error.is_some() { ROW_HEIGHT } else { 0.0 };
    let mut container = col()
        .key(base.to_string())
        .label("JSON input")
        .role("textbox")
        .focusable(true)
        .ime_cursor()
        .w_full()
        .padding(PAD)
        .fill(EDITOR_BG)
        .on_drag(move |s: &mut AppState, d: &mut Drag, tx: &mut Tx| {
            pointer_drag(s, id, d, tx, cw, gutter_w, strip_h);
        })
        .on_wheel(move |s: &mut AppState, e: WheelEvent, _tx: &mut Tx| {
            // Horizontal deltas (or shift+wheel) pan the column window;
            // vertical deltas stay with the scroll viewport.
            let horizontal = e.mods.shift || e.dx.abs() > e.dy.abs();
            if !horizontal || (e.dx == 0.0 && e.dy == 0.0) {
                return false;
            }
            let delta = if e.dx.abs() > e.dy.abs() { e.dx } else { e.dy };
            let columns = (delta / cw).round() as isize;
            if columns == 0 {
                return true;
            }
            if let Some(tab) = s.tab(id) {
                state::with(id, &tab.input, tab.input_revision, |ed| {
                    ed.pan_columns(columns);
                });
            }
            true
        });

    // Error strip (same keys the Validate panel's evidence tests assert).
    if let Some((line, column)) = error {
        container = container.child(
            row()
                .key(format!("{base}-error-strip"))
                .w_full()
                .h(ROW_HEIGHT)
                .gap(6.0)
                .center_y()
                .child(
                    el()
                        .key(format!("{base}-error-dot"))
                        .w(6.0)
                        .h(6.0)
                        .radius(3.0)
                        .fill(ERROR_INK)
                        .a11y_hidden(),
                )
                .child(
                    text(format!("line {line}, column {column}"))
                        .key(format!("{base}-error-label"))
                        .font(tok::type_::LABEL)
                        .color(ERROR_INK),
                ),
        );
    }

    container
        .child(el().key(format!("{base}-anchor")).w_full().h(first as f32 * ROW_HEIGHT))
        .children(rows)
        .child(el().key(format!("{base}-tail")).w_full().h((lines - last) as f32 * ROW_HEIGHT))
}

/// One visible row: gutter, error dot, and the column-windowed text with
/// syntax colors, selection background, error underline and the caret.
#[allow(clippy::too_many_arguments)]
fn render_row(
    ed: &EditorState,
    id: u64,
    base: &str,
    line: usize,
    gutter_w: usize,
    cw: f32,
    columns: usize,
    caret_line: usize,
    error: Option<(usize, usize)>,
) -> El {
    let theme = tok::theme(true);
    let buf = &ed.buffer;
    let line_range = buf.line_byte_range(line);
    let line_len = line_range.end - line_range.start;

    // Column window, snapped to UTF-8 boundaries, sliced straight from the
    // rope: a 6.9 MB line costs `columns` bytes of copy, never a line clone.
    let raw_start = line_range.start + ed.first_column.min(line_len);
    let col_start = buf.char_to_byte(buf.byte_to_char(raw_start));
    let raw_end = (col_start + columns).min(line_range.end);
    let col_end = buf.char_to_byte(buf.byte_to_char(raw_end));

    let cached = cached_window(id, line, col_start, col_end, buf, line_range.start);
    let mut display = cached.display.clone();
    let span_offset = cached.span_offset;
    let mut spans: Vec<Span> = cached
        .tokens
        .iter()
        .map(|(range, color)| Span::new(range.start, range.end).color(*color))
        .collect();

    let is_error = error.is_some_and(|(l, _)| l == line + 1);
    if is_error {
        if let Some((_, column)) = error {
            let c = column.saturating_sub(1);
            let rel_start = col_start - line_range.start;
            if c >= rel_start && c < col_end - line_range.start {
                let at = span_offset + (c - rel_start);
                let end = (at + 1).min(span_offset + cached.slice_len);
                if at < end {
                    spans.push(Span::new(at, end).underline(ERROR_INK).bg(ERROR_CARET_BG, 1.0));
                }
            }
        }
    }

    // Selection background over the visible part of the range.
    if let Some(sel) = ed.selection() {
        let from = sel.start.max(col_start);
        let to = sel.end.min(col_end);
        if from < to {
            let s = span_offset + (from - col_start);
            let e = span_offset + (to - col_start);
            spans.push(Span::new(s, e).bg(ACCENT.alpha(0.25), 2.0));
        }
    }

    let caret_here = caret_line == line && ed.caret >= col_start && ed.caret <= col_end;
    let mut caret_disp = if caret_here { span_offset + (ed.caret - col_start) } else { 0 };

    // IME composition: insert the preedit at the caret, underlined; the
    // caret sits after it. Tokens at/after the insertion shift with it.
    let preedit = ed.preedit.as_deref().filter(|_| caret_here);
    if let Some(preedit) = preedit {
        let at = caret_disp;
        display.insert_str(at, preedit);
        for span in &mut spans {
            if span.start as usize >= at {
                span.start += preedit.len() as u32;
            }
            if span.end as usize > at {
                span.end += preedit.len() as u32;
            }
        }
        spans.push(Span::new(at, at + preedit.len()).underline(TEXT));
        caret_disp = at + preedit.len();
    }

    let mut content = row()
        .key(row_key(base, line))
        .w_full()
        .h(ROW_HEIGHT)
        .center_y()
        .child(
            text(format!("{:>width$}", line + 1, width = gutter_w))
                .font(mono())
                .color(theme.text_secondary)
                .w(gutter_w as f32 * cw)
                .align(Align::End),
        )
        .child(error_gutter(base, line, cw, is_error));

    if display.is_empty() && line_len == 0 && ed.buffer.is_empty() {
        // Empty document: placeholder behind the caret.
        let mut line_row = row().gap(0.0).center_y();
        if caret_here {
            line_row = line_row.child(caret_el(base));
        }
        line_row = line_row.child(
            text("Paste your JSON here or drop a JSON file…")
                .key(format!("{base}-line-{line}"))
                .font(mono())
                .color(MUTED),
        );
        content = content.child(line_row);
    } else if caret_here {
        let (before, after) = display.split_at(caret_disp);
        let mut line_row = row().gap(0.0).center_y();
        if !before.is_empty() {
            line_row = line_row.child(
                text(before.to_owned())
                    .key(format!("{base}-line-{line}"))
                    .font(mono())
                    .color(TEXT)
                    .spans(clip_spans(&spans, 0, caret_disp)),
            );
        }
        line_row = line_row.child(caret_el(base));
        if !after.is_empty() {
            line_row = line_row.child(
                text(after.to_owned())
                    .key(format!("{base}-line-{line}-after"))
                    .font(mono())
                    .color(TEXT)
                    .spans(clip_spans(&spans, caret_disp, display.len())),
            );
        }
        content = content.child(line_row);
    } else {
        content = content.child(
            text(display)
                .key(format!("{base}-line-{line}"))
                .font(mono())
                .color(TEXT)
                .spans(spans),
        );
    }

    if is_error {
        content = content.fill(ERROR_TINT);
    }
    content
}

fn caret_el(base: &str) -> El {
    el()
        .key(format!("{base}-caret"))
        .w(2.0)
        .h(ROW_HEIGHT - 4.0)
        .radius(1.0)
        .fill(TEXT)
        .a11y_hidden()
}

fn error_gutter(base: &str, line: usize, cw: f32, is_error: bool) -> El {
    let slot = el().w(cw * 1.2).h(ROW_HEIGHT).center();
    if !is_error {
        return slot;
    }
    slot.child(
        el()
            .key(format!("{base}-error-dot-{line}"))
            .w(6.0)
            .h(6.0)
            .radius(3.0)
            .fill(ERROR_INK)
            .a11y_hidden(),
    )
}

/// Clips spans to a display-byte window and shifts them to the window's
/// origin (the caret splits one line into two text elements).
fn clip_spans(spans: &[Span], from: usize, to: usize) -> Vec<Span> {
    spans
        .iter()
        .filter_map(|span| {
            let start = (span.start as usize).max(from);
            let end = (span.end as usize).min(to);
            if start >= end {
                return None;
            }
            let mut out = Span::new(start - from, end - from);
            out.color = span.color;
            out.bg = span.bg;
            out.bg_pad = span.bg_pad;
            out.underline = span.underline;
            Some(out)
        })
        .collect()
}

// ── Shaped-window cache ──────────────────────────────────────────────────

struct CachedWindow {
    display: String,
    span_offset: usize,
    slice_len: usize,
    tokens: Vec<(Range<usize>, Color)>,
}

/// Cache key: tab, line, window start/end, content hash.
type WindowKey = (u64, usize, usize, usize, u64);

thread_local! {
    static CACHE: RefCell<FxHashMap<WindowKey, CachedWindow>> =
        RefCell::new(FxHashMap::default());
}

/// Cache key: tab, line, window bounds and a content hash of the window (the
/// per-line stand-in for a buffer revision). Unchanged windows skip the rope
/// slice and the tokenizer; Rune's shape cache keys on the display string.
fn cached_window(
    id: u64,
    line: usize,
    col_start: usize,
    col_end: usize,
    buf: &super::buffer::Buffer,
    line_start: usize,
) -> CachedWindow {
    let slice = buf.slice_bytes(col_start..col_end);
    let in_string = starts_inside_string(buf, line_start, col_start);
    let mut hasher = FxHasher::default();
    slice.hash(&mut hasher);
    in_string.hash(&mut hasher);
    let hash = hasher.finish();
    let key = (id, line, col_start, col_end, hash);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(hit) = cache.get(&key) {
            return clone_window(hit);
        }
        let left_elided = col_start > line_start;
        let right_elided = col_end < buf.line_byte_range(line).end;
        let mut display = String::with_capacity(slice.len() + 6);
        if left_elided {
            display.push('…');
        }
        let span_offset = display.len();
        display.push_str(&slice);
        if right_elided {
            display.push('…');
        }
        let tokens = window_tokens(&slice, span_offset, in_string);
        let entry = CachedWindow {
            display,
            span_offset,
            slice_len: slice.len(),
            tokens,
        };
        if cache.len() > 8192 {
            cache.clear();
        }
        let out = clone_window(&entry);
        cache.insert(key, entry);
        out
    })
}

fn clone_window(w: &CachedWindow) -> CachedWindow {
    CachedWindow {
        display: w.display.clone(),
        span_offset: w.span_offset,
        slice_len: w.slice_len,
        tokens: w.tokens.clone(),
    }
}

/// Bounded lookback (512 bytes): is `col_start` inside a JSON string?
/// Unescaped-quote parity over the lookback approximates the string state
/// (exact when the opening quote is within the window; an opener farther
/// back reads as plain text — coloring only, never correctness).
fn starts_inside_string(buf: &super::buffer::Buffer, line_start: usize, col_start: usize) -> bool {
    if col_start <= line_start {
        return false;
    }
    let lo = line_start.max(col_start.saturating_sub(512));
    let lookback = buf.slice_bytes(lo..col_start);
    let bytes = lookback.as_bytes();
    let mut quotes = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'"' {
            quotes += 1;
        }
        i += 1;
    }
    quotes % 2 == 1
}

/// JSON token colors for one window. When the window starts inside a
/// string, a synthetic opening quote is prepended for the tokenizer and
/// clipped off the resulting spans.
fn window_tokens(slice: &str, offset: usize, in_string: bool) -> Vec<(Range<usize>, Color)> {
    let (key_ink, string_ink, number_ink, literal_ink, punctuation_ink) = palette();
    let owned;
    let text: &str = if in_string {
        owned = format!("\"{slice}");
        &owned
    } else {
        slice
    };
    let shift = if in_string { offset.saturating_sub(1) } else { offset };
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
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
                tokens.push((shift + start..shift + i, ink));
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                tokens.push((shift + start..shift + i, number_ink));
            }
            b't' | b'f' | b'n' => {
                let literal: &[u8] = match bytes[i] {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                if bytes[i..].starts_with(literal) {
                    i += literal.len();
                    tokens.push((shift + start..shift + i, literal_ink));
                } else {
                    i += 1;
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                i += 1;
                tokens.push((shift + start..shift + i, punctuation_ink));
            }
            _ => i += 1,
        }
    }
    if in_string {
        for (range, _) in &mut tokens {
            range.start = range.start.max(offset);
        }
        tokens.retain(|(range, _)| range.start < range.end);
    }
    tokens
}

// ── Mouse ────────────────────────────────────────────────────────────────

/// Click/drag selection. Presentation-only (like fold toggles): mutates the
/// thread-local editor state directly; the handler event re-runs the view.
fn pointer_drag(s: &mut AppState, id: u64, d: &mut Drag, tx: &mut Tx, cw: f32, gutter_w: usize, strip_h: f32) {
    let Some(tab) = s.tab(id) else {
        return;
    };
    let input = tab.input.as_str();
    let revision = tab.input_revision;
    let x_origin = PAD + gutter_w as f32 * cw + cw * 1.2 + GAP;
    let y_origin = PAD + strip_h;
    state::with(id, input, revision, |ed| {
        let lines = ed.buffer.len_lines();
        let line = (((d.y - y_origin) / ROW_HEIGHT).floor().max(0.0) as usize).min(lines - 1);
        let range = ed.buffer.line_byte_range(line);
        let columns = ((d.x - x_origin) / cw).floor().max(0.0) as usize;
        let raw = (range.start + ed.first_column + columns).min(range.end);
        let offset = ed.buffer.char_to_byte(ed.buffer.byte_to_char(raw));
        match d.phase {
            Phase::Down => {
                let shift = tx.mods().shift;
                match d.clicks {
                    2 => {
                        ed.select_word(offset);
                        ed.drag = Some(DragMode::Word);
                        ed.drag_origin = ed.anchor.unwrap_or(offset);
                        ed.drag_end = ed.caret;
                    }
                    3 => {
                        ed.select_line(line);
                        ed.drag = Some(DragMode::Line);
                        ed.drag_origin = ed.anchor.unwrap_or(offset);
                        ed.drag_end = ed.caret;
                    }
                    _ => {
                        ed.set_caret(offset, shift);
                        ed.drag = Some(DragMode::Char);
                        ed.drag_origin = offset;
                        ed.drag_end = offset;
                    }
                }
            }
            Phase::Move => match ed.drag {
                Some(DragMode::Char) => ed.set_caret(offset, true),
                Some(DragMode::Word) => {
                    let word = ed.word_range(offset);
                    if offset >= ed.drag_origin {
                        ed.anchor = Some(ed.drag_origin);
                        ed.caret = word.end;
                    } else {
                        ed.anchor = Some(ed.drag_end);
                        ed.caret = word.start;
                    }
                    ed.reveal = Some(line);
                }
                Some(DragMode::Line) => {
                    let target = ed.buffer.line_byte_range(line);
                    let origin_line = ed.buffer.byte_to_line(ed.drag_origin);
                    if line >= origin_line {
                        ed.anchor = Some(ed.drag_origin);
                        ed.caret = target.end;
                    } else {
                        ed.anchor = Some(ed.drag_end);
                        ed.caret = target.start;
                    }
                    ed.reveal = Some(line);
                }
                None => {}
            },
            Phase::Up => {
                ed.drag = None;
            }
        }
    });
}