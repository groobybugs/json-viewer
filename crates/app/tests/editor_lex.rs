//! Windowed JSON coloring must equal whole-line lexing (rp-editor
//! follow-up): the column window's starting lexer state is exact, including
//! inside strings, right after escaped quotes/backslashes, and across
//! checkpoint/window edges — and stays exact after edits.

use std::ops::Range;
use std::time::{Duration, Instant};

use json_viewer::editor::buffer::{Buffer, EditKind};
use json_viewer::editor::lex::{self, LexState, CHECKPOINT_STRIDE};
use rune::Color;

/// The checkpoint store is process-global; keep these tests serial so one
/// test's clear_all cannot wipe another's entries mid-assertion.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn guard() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// One long minified-style line with digits inside strings, escaped quotes
/// and escaped backslashes, spanning several checkpoint strides.
fn fixture_line(records: usize) -> String {
    let mut line = String::new();
    for i in 0..records {
        line.push_str(&format!(
            "{{\"id\":{i},\"name\":\"item-{i:07}\",\"desc\":\"digits 12345 say \\\"hi\\\" \\\\ end\",\"ok\":true,\"value\":{i}.25}},"
        ));
    }
    line
}

fn reference(buf: &Buffer, line: usize) -> Vec<(Range<usize>, Color)> {
    let whole = buf.line_text(line);
    lex::json_tokens(&whole, 0, LexState::default(), "", None)
}

fn clipped(reference: &[(Range<usize>, Color)], window: Range<usize>) -> Vec<(Range<usize>, Color)> {
    reference
        .iter()
        .filter_map(|(r, c)| {
            let s = r.start.max(window.start);
            let e = r.end.min(window.end);
            (s < e).then_some((s..e, *c))
        })
        .collect()
}

fn windowed(buf: &Buffer, tab: u64, line: usize, col: usize, width: usize) -> Vec<(Range<usize>, Color)> {
    let range = buf.line_byte_range(line);
    let end = (col + width).min(range.end);
    let slice = buf.slice_bytes(col..end);
    let state = lex::window_state(tab, line, buf, col);
    let tail = lex::tail_for(buf, end, range.end);
    let cut_key = if state.advance(slice.as_bytes()).in_string {
        Some(lex::cut_string_is_key(buf, end, range.end))
    } else {
        None
    };
    lex::json_tokens(&slice, col, state, &tail, cut_key)
}

fn assert_matches_reference(buf: &Buffer, tab: u64, line: usize, col: usize, width: usize) {
    let range = buf.line_byte_range(line);
    let end = (col + width).min(range.end);
    let want = clipped(&reference(buf, line), col..end);
    let got = windowed(buf, tab, line, col, width);
    if got != want {
        let n = got.len().min(want.len());
        let at = (0..n).find(|&i| got[i] != want[i]).unwrap_or(n);
        panic!(
            "window at col {col} width {width}: first diff at token {at} (got {} / want {} tokens)\n got: {:?}\nwant: {:?}",
            got.len(),
            want.len(),
            got.get(at..(at + 3).min(got.len())),
            want.get(at..(at + 3).min(want.len()))
        );
    }
}

#[test]
fn window_colors_equal_whole_line_at_many_columns() {
    let _g = guard();
    let line = fixture_line(2000); // ~190 KB: several strides, inline build
    assert!(line.len() > 2 * CHECKPOINT_STRIDE);
    let buf = Buffer::from_text(&line);
    lex::clear_all();
    let len = buf.len_bytes();

    // Locate interesting byte columns inside the first records.
    let esc_quote = line.find("\\\"").expect("escaped quote") + 2; // right after \"
    let esc_back = line.find("\\\\").expect("escaped backslash") + 2; // right after \\
    let digits = line.find("12345").expect("digits in string");
    let columns = [
        0,
        1,
        2,
        7,
        digits,
        digits + 3,
        esc_quote,
        esc_quote + 1,
        esc_back,
        esc_back + 1,
        CHECKPOINT_STRIDE - 1,
        CHECKPOINT_STRIDE,
        CHECKPOINT_STRIDE + 1,
        2 * CHECKPOINT_STRIDE - 1,
        2 * CHECKPOINT_STRIDE,
        2 * CHECKPOINT_STRIDE + 17,
        len - 1,
        len,
    ];
    for col in columns {
        for width in [1, 160, CHECKPOINT_STRIDE + 7] {
            assert_matches_reference(&buf, 11, 0, col, width);
        }
    }
    lex::clear_all();
}

#[test]
fn digits_inside_strings_keep_string_ink() {
    let _g = guard();
    // The reported bug: digits inside a string rendered as numbers and the
    // real number rendered as string ink.
    let line = fixture_line(2000);
    let buf = Buffer::from_text(&line);
    lex::clear_all();
    let digits = line.find("12345").unwrap();
    let real_number = line.find("\"value\":").unwrap() + "\"value\":".len();
    let string_ink = reference(&buf, 0)
        .iter()
        .find(|(r, _)| r.start <= digits && r.end > digits)
        .map(|(_, c)| *c)
        .expect("digits are inside a string token");
    for col in [digits - 40, digits - 1, digits, real_number - 40] {
        let tokens = windowed(&buf, 12, 0, col, 160);
        let digit_color = tokens
            .iter()
            .find(|(r, _)| r.start <= digits && r.end > digits)
            .map(|(_, c)| *c);
        if col <= digits {
            assert_eq!(digit_color, Some(string_ink), "digits in string at col {col}");
        }
        if col <= real_number && real_number < col + 160 {
            let number_color = tokens
                .iter()
                .find(|(r, _)| r.start <= real_number && r.end > real_number)
                .map(|(_, c)| *c);
            assert_ne!(number_color, Some(string_ink), "real number at col {col}");
        }
    }
    lex::clear_all();
}

#[test]
fn edits_invalidate_and_other_line_edits_reuse_checkpoints() {
    let _g = guard();
    let line = fixture_line(2000);
    let mut buf = Buffer::from_text(&line);
    lex::clear_all();
    let col = CHECKPOINT_STRIDE + 500;
    assert_matches_reference(&buf, 13, 0, col, 160);
    let rev = buf.revision();

    // An edit in a *different* line leaves line 0's checkpoints valid.
    buf.insert(buf.len_bytes(), "\n{\"other\":1}", EditKind::Typing, 0, 0);
    let edits = buf.edits_since(rev).expect("log covers");
    let range0 = buf.line_byte_range(0);
    assert!(edits.iter().all(|r| r.end <= range0.start || r.start >= range0.end));
    assert!(lex::is_ready(13, 0), "checkpoints survive other-line edits");
    assert_matches_reference(&buf, 13, 0, col, 160);

    // An edit inside line 0 before the window flips string state: colors
    // must follow the new whole-line truth.
    let quote = line.find("\"desc\":\"").unwrap() + "\"desc\":\"".len();
    buf.insert(quote, "\"", EditKind::Typing, quote, quote + 1);
    assert_matches_reference(&buf, 13, 0, col, 160);
    assert_matches_reference(&buf, 13, 0, quote + 1, 160);

    // Undo restores the previous truth.
    buf.undo();
    assert_matches_reference(&buf, 13, 0, col, 160);
    lex::clear_all();
}

#[test]
fn off_thread_checkpoints_are_exact_before_and_after_publication() {
    let _g = guard();
    let line = fixture_line(12000); // ~1.1 MB: off-thread build
    assert!(line.len() > 256 * 1024);
    let buf = Buffer::from_text(&line);
    lex::clear_all();
    let col = 700_000;
    // Provisional path (scan from line start) is already exact.
    assert_matches_reference(&buf, 14, 0, col, 160);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lex::is_ready(14, 0) {
        assert!(Instant::now() < deadline, "lex worker did not publish");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_matches_reference(&buf, 14, 0, col, 160);
    assert_matches_reference(&buf, 14, 0, CHECKPOINT_STRIDE * 5 + 3, 160);
    lex::clear_all();
}