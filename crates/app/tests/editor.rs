//! Editor evidence (rp-editor E2): headless renders of the rope editor —
//! caret/selection, mouse hit-testing, the 6.9 MB single-line column window
//! at column 3,000,000, and 100 MB windowing/scroll timings.
//!
//! Run: `cargo test -p json-viewer --test editor`
//! Release timings: `cargo test --release -p json-viewer --test editor -- --ignored --nocapture`

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use json_viewer::editor::{state as editor_state, view};
use json_viewer::state::{AppState, Msg, TabView};
use json_viewer::ui::text_view::char_width;
use json_viewer::{headless, runner};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{col, Cx, El, Env, Ime, Key, KeyEvent, Mods, Runtime, ScrollSpec, WheelEvent};

const SIZE: (f32, f32) = (1280.0, 800.0);
const MIB: usize = 1024 * 1024;

fn editor_view(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let tab = state.active_tab().expect("active tab");
    let base = format!("editor-{}", tab.id);
    col()
        .key(format!("document-scroll-{}", tab.id))
        .w_full()
        .h_full()
        .scroll(ScrollSpec { stick_bottom: false })
        .child(view::build(state, cx, tab, &base))
}

fn runtime(state: AppState) -> Runtime<AppState> {
    let mut rt = Runtime::with_text(state, editor_view, SIZE, Env::new(true, 1.0), headless::text_system());
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    rt
}

fn doc_state(doc: String) -> AppState {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, doc);
    state
}

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("headless");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless")
}

fn draw(rt: &mut Runtime<AppState>, name: &str) {
    rt.settle_all();
    rt.frame(0.0);
    let (w, h) = SIZE;
    let gpu = Gpu::headless();
    let format = OFFSCREEN_FORMAT.remove_srgb_suffix();
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(true);
    let target = Offscreen::with_format(&gpu.device, w as u32, h as u32, format);
    renderer.prepare(&gpu.device, &gpu.queue, &mut rt.text.atlas, &rt.instances, (w as u32, h as u32));
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&mut encoder, &target.view, rt.clear_color());
    target.copy(&mut encoder);
    renderer.submit(&gpu.queue, encoder);
    let pixels = target.read(&gpu.device);
    let path = target_dir().join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_png(&path, w as u32, h as u32, &pixels).expect("write PNG");
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
}

fn pretty_fixture(target: usize) -> String {
    let mut out = String::with_capacity(target + 4096);
    out.push_str("{\n  \"items\": [\n");
    let mut i = 0usize;
    while out.len() < target.saturating_sub(300) {
        if i > 0 {
            out.push_str(",\n");
        }
        write!(
            out,
            "    {{\n      \"id\": {i},\n      \"name\": \"item-{i:07}\",\n      \
             \"description\": \"bench record {i:07} with some plain ascii text\",\n      \
             \"tags\": [\"alpha\", \"beta\"],\n      \"value\": {}.{}}}\n    }}",
            i % 1000,
            i % 100
        )
        .unwrap();
        i += 1;
    }
    out.push_str("\n  ]\n}");
    out
}

fn minified_fixture(target: usize) -> String {
    let mut out = String::with_capacity(target + 64);
    out.push_str("{\"items\":[");
    let mut i = 0usize;
    while out.len() < target.saturating_sub(200) {
        if i > 0 {
            out.push(',');
        }
        write!(out, "{{\"id\":{},\"name\":\"item-{:07}\"}}", i % 1000, i % 1000).unwrap();
        i += 1;
    }
    out.push_str("]}");
    assert!(!out.contains('\n'));
    out
}

/// Text origin inside a row: padding + gutter + error cell + gap.
fn text_x(row_x: f32, gutter_w: usize, cw: f32) -> f32 {
    row_x + gutter_w as f32 * cw + cw * 1.2 + 6.0
}

fn row_rect(rt: &Runtime<AppState>, line: usize) -> [f32; 4] {
    rt.elements()
        .iter()
        .find(|e| e.key.as_deref() == Some(format!("editor-1-row-{line}").as_str()))
        .map(|e| e.rect)
        .unwrap_or_else(|| panic!("row {line} not mounted"))
}

#[test]
fn small_doc_renders_caret_gutter_and_rows() {
    let state = doc_state("{\n  \"a\": 1,\n  \"b\": [true, null]\n}".into());
    let mut rt = runtime(state);
    let keys: Vec<String> = rt
        .elements()
        .iter()
        .filter_map(|e| e.key.clone())
        .collect();
    for line in 0..4 {
        assert!(keys.contains(&format!("editor-1-row-{line}")), "row {line} missing");
    }
    assert!(keys.contains(&"editor-1-caret".to_string()), "caret missing");
    let elements = rt.elements();
    let gutter = elements
        .iter()
        .find(|e| e.text.as_deref() == Some("    1"))
        .expect("gutter line number 1");
    assert!(gutter.rect[2] > 0.0);
    draw(&mut rt, "editor-small.png");
}

#[test]
fn click_places_caret_and_drag_selects() {
    let doc = "hello world\nsecond line\nthird line";
    let state = doc_state(doc.into());
    let mut rt = runtime(state);
    let cw = char_width();
    let row = row_rect(&rt, 1);
    let x0 = text_x(row[0], 5, cw);
    let y = row[1] + row[3] * 0.5;

    // Click at column 3 of line 1 ("second line" -> offset 12+3).
    rt.pointer_move(x0 + 3.0 * cw + cw * 0.5, y);
    rt.pointer_down(x0 + 3.0 * cw + cw * 0.5, y);
    rt.pointer_up(x0 + 3.0 * cw + cw * 0.5, y);
    rt.frame(1.0 / 60.0);
    let id = rt.state.active;
    let (input, rev) = {
        let tab = rt.state.active_tab().unwrap();
        (tab.input.clone(), tab.input_revision)
    };
    let c = editor_state::with(id, &input, rev, |ed| ed.caret);
    assert_eq!(c, 12 + 3, "click caret");

    // Plain drag from column 5 to column 9 -> selection 17..21. (Starting
    // at the previous click point would register as a double-click word
    // select, which the last part of this test covers.)
    rt.pointer_down(x0 + 5.0 * cw + cw * 0.5, y);
    rt.pointer_move(x0 + 9.0 * cw + cw * 0.5, y);
    rt.pointer_up(x0 + 9.0 * cw + cw * 0.5, y);
    rt.frame(1.0 / 60.0);
    let sel = editor_state::with(id, &input, rev, |ed| ed.selection());
    assert_eq!(sel, Some(17..21), "drag selection");
    draw(&mut rt, "editor-selection.png");

    // Double-click selects the word under the pointer ("second").
    let xw = x0 + 1.0 * cw;
    rt.pointer_down(xw, y);
    rt.pointer_up(xw, y);
    rt.pointer_down(xw, y);
    rt.pointer_up(xw, y);
    rt.frame(1.0 / 60.0);
    let sel = editor_state::with(id, &input, rev, |ed| ed.selection());
    assert_eq!(sel, Some(12..18), "double-click word selection");
}

#[test]
fn wheel_pans_the_column_window() {
    let doc = minified_fixture(2 * MIB);
    let state = doc_state(doc);
    let mut rt = runtime(state);
    let cw = char_width();
    let row = row_rect(&rt, 0);
    let ev = WheelEvent::new(10.0 * cw, 0.0).at(row[0] + 200.0, row[1] + 8.0);
    rt.wheel_event(ev);
    rt.frame(1.0 / 60.0);
    let id = rt.state.active;
    let (input, rev) = {
        let tab = rt.state.active_tab().unwrap();
        (tab.input.clone(), tab.input_revision)
    };
    let col = editor_state::with(id, &input, rev, |ed| ed.first_column);
    assert_eq!(col, 10, "wheel pan columns");
}

#[test]
fn single_line_6_9mb_renders_window_at_column_3m() {
    let doc = minified_fixture(6_900_000);
    let state = doc_state(doc.clone());
    let id = state.active;
    // Caret and window at column 3,000,000 (E5 evidence position).
    let rev = state.tab(id).unwrap().input_revision;
    editor_state::with(id, &state.tab(id).unwrap().input, rev, |ed| {
        ed.caret = 3_000_000;
        ed.first_column = 3_000_000 - 40;
    });
    let t = Instant::now();
    let mut rt = runtime(state);
    let frame_ms = t.elapsed().as_secs_f64() * 1000.0;
    let row = row_rect(&rt, 0);
    let elements = rt.elements();
    let mut line_text = elements
        .iter()
        .find(|e| e.key.as_deref() == Some("editor-1-line-0"))
        .and_then(|e| e.text.clone())
        .expect("line 0 text");
    if let Some(after) = elements
        .iter()
        .find(|e| e.key.as_deref() == Some("editor-1-line-0-after"))
        .and_then(|e| e.text.clone())
    {
        line_text.push_str(&after);
    }
    assert!(line_text.len() <= 260, "window not bounded: {} bytes", line_text.len());
    let expected = &doc[3_000_000 - 40..3_000_000 - 40 + 200];
    assert!(line_text.contains(&expected[..60]), "window content mismatch");
    assert!(rt.elements().iter().any(|e| e.key.as_deref() == Some("editor-1-caret")));
    assert!(row[2] > 0.0);
    draw(&mut rt, "editor-single-line-col-3m.png");
    println!("6.9 MB single-line first frame (debug): {frame_ms:.1} ms");
}

#[test]
#[ignore]
fn hundred_mb_windowing_and_scroll_timings() {
    let doc = pretty_fixture(100 * MIB);
    let lines = doc.bytes().filter(|b| *b == b'\n').count() + 1;
    let mut state = AppState::default();
    let id = state.active;
    let t = Instant::now();
    state.update_input(id, doc);
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let mut rt = Runtime::with_text(state, editor_view, SIZE, Env::new(true, 1.0), headless::text_system());
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    let first_frame_ms = t.elapsed().as_secs_f64() * 1000.0;
    rt.settle_all();
    rt.frame(0.0);

    let mounted = rt.elements().iter().filter(|e| e.key.as_deref().is_some_and(|k| k.starts_with("editor-1-row-"))).count();
    assert!(mounted < 200, "windowing built {mounted} rows");

    // Scroll to the middle: the window must follow without rebuilding keys.
    let mid_line = lines / 2;
    let t = Instant::now();
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_to_y(format!("document-scroll-{id}"), mid_line as f32 * 16.0 - 200.0);
    });
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
    let scroll_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert!(
        rt.elements().iter().any(|e| e.key.as_deref() == Some(format!("editor-1-row-{mid_line}").as_str())),
        "middle row {mid_line} not mounted after scroll"
    );

    // A keystroke-equivalent state change at the middle (caret move) must
    // stay O(visible): time 20 caret moves with frames.
    let mut samples = Vec::new();
    for i in 0..20 {
        let t = Instant::now();
        rt.update(|s, _| {
            let tab = s.active_tab().unwrap();
            editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| {
                ed.caret = (mid_line * 17 + i * 3).min(ed.buffer.len_bytes());
                ed.reveal = None;
            });
        });
        rt.frame(1.0 / 60.0);
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let p95 = samples[((samples.len() - 1) as f64 * 0.95).round() as usize];
    draw(&mut rt, "editor-100mb-middle.png");
    println!(
        "100 MB pretty ({lines} lines): load {load_ms:.1} ms, first frame {first_frame_ms:.1} ms, \
         scroll-to-middle {scroll_ms:.1} ms, caret-move p95 {p95:.2} ms, mounted rows {mounted}"
    );
    assert!(first_frame_ms < 5000.0, "first frame guard");
    assert!(p95 < 200.0, "caret move guard (debug)");
}

#[test]
fn error_marker_strip_and_gutter_dot_render() {
    let input = "{\n  \"a\": 1,\n  \"b\": oops\n}";
    let mut state = doc_state(input.into());
    let id = state.active;
    let error = json_viewer::json_ops::validate(input).expect_err("invalid");
    let revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: revision,
        result: Err(error),
    });
    let mut rt = runtime(state);
    let keys: Vec<String> = rt.elements().iter().filter_map(|e| e.key.clone()).collect();
    assert!(keys.contains(&"editor-1-error-dot".to_string()));
    assert!(keys.contains(&"editor-1-error-dot-2".to_string()));
    let label = rt
        .elements()
        .iter()
        .find(|e| e.key.as_deref() == Some("editor-1-error-label"))
        .and_then(|e| e.text.clone())
        .expect("error label");
    assert_eq!(label, "line 3, column 8");
    draw(&mut rt, "editor-error-marker.png");
}

// ── E3: editing through the real app composition ─────────────────────────

fn app_runtime(state: AppState) -> Runtime<AppState> {
    let mut rt = headless::runtime(state, SIZE, 1.0);
    rt.update(|_, tx| tx.focus("editor-1", true));
    rt.frame(0.0);
    rt
}

fn press(rt: &mut Runtime<AppState>, key: Key, mods: Mods) {
    rt.key(KeyEvent { key, mods });
    // Handlers dispatch during the frame; the runner applies on the next
    // poll, then the view renders the applied state.
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
}

fn cmd_mods() -> Mods {
    Mods { cmd: true, ..Mods::default() }
}

fn buffer_text(rt: &Runtime<AppState>) -> String {
    let tab = rt.state.active_tab().unwrap();
    editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| ed.buffer.to_string())
}

#[test]
fn app_typing_undo_redo_and_enter() {
    let state = doc_state("{\n  \"a\": 1\n}".into());
    let mut rt = app_runtime(state);
    let rev0 = rt.state.active_tab().unwrap().input_revision;

    // Type at the caret (offset 0): chars coalesce into one undo step.
    for c in ["X", "Y"] {
        press(&mut rt, Key::Text(c.into()), Mods::default());
    }
    assert_eq!(buffer_text(&rt), "XY{\n  \"a\": 1\n}");
    assert!(rt.state.active_tab().unwrap().input_revision > rev0);
    // The model cache is stale between bulk actions (no per-key copy).
    assert_eq!(rt.state.active_tab().unwrap().input, "{\n  \"a\": 1\n}");

    press(&mut rt, Key::Command('z'), cmd_mods());
    assert_eq!(buffer_text(&rt), "{\n  \"a\": 1\n}");
    press(&mut rt, Key::Command('y'), cmd_mods());
    assert_eq!(buffer_text(&rt), "XY{\n  \"a\": 1\n}");

    // Enter after '{' auto-indents.
    press(&mut rt, Key::End, Mods::default()); // caret to end of line 0 ("XY{")
    press(&mut rt, Key::Enter, Mods::default());
    assert_eq!(buffer_text(&rt), "XY{\n  \n  \"a\": 1\n}");
}

#[test]
fn app_paste_materializes_and_is_one_undo_step() {
    let state = doc_state("old".into());
    let mut rt = app_runtime(state);
    press(&mut rt, Key::Command('a'), cmd_mods());
    rt.paste("{\"pasted\":true}".to_string());
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    assert_eq!(buffer_text(&rt), "{\"pasted\":true}");
    assert_eq!(rt.state.active_tab().unwrap().input, "{\"pasted\":true}");
    press(&mut rt, Key::Command('z'), cmd_mods());
    assert_eq!(buffer_text(&rt), "old");
}

#[test]
fn app_select_all_copy_cut() {
    let state = doc_state("{\"a\":1}".into());
    let mut rt = app_runtime(state);
    rt.key(KeyEvent { key: Key::Command('a'), mods: cmd_mods() });
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    let sel = {
        let tab = rt.state.active_tab().unwrap();
        editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| ed.selection())
    };
    assert_eq!(sel, Some(0..7));

    rt.key(KeyEvent { key: Key::Command('c'), mods: cmd_mods() });
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.status.as_deref(), Some("Copied to clipboard"));
    assert_eq!(buffer_text(&rt), "{\"a\":1}");

    rt.key(KeyEvent { key: Key::Command('x'), mods: cmd_mods() });
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    assert_eq!(buffer_text(&rt), "");
    // Empty boundary materializes so the toolbar disables correctly.
    assert_eq!(rt.state.active_tab().unwrap().input, "");
}

#[test]
fn ime_preedit_and_commit() {
    let state = doc_state("".into());
    let mut rt = app_runtime(state);
    let mut taken = false;
    rt.update(|s, tx| {
        taken = json_viewer::editor::keys::on_ime(s, &Ime::Preedit("ni".into(), None, vec![]), tx);
    });
    assert!(taken);
    rt.frame(0.0);
    let preedit = {
        let tab = rt.state.active_tab().unwrap();
        editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| ed.preedit.clone())
    };
    assert_eq!(preedit.as_deref(), Some("ni"));
    assert!(rt.elements().iter().any(|e| e.key.as_deref() == Some("editor-1-caret")));

    rt.update(|s, tx| {
        json_viewer::editor::keys::on_ime(s, &Ime::Commit("你".into()), tx);
    });
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    assert_eq!(buffer_text(&rt), "你");
    let tab = rt.state.active_tab().unwrap();
    let caret = editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| ed.caret);
    assert_eq!(caret, "你".len());
    // One undo step removes the whole composition.
    press(&mut rt, Key::Command('z'), cmd_mods());
    assert_eq!(buffer_text(&rt), "");
}

#[test]
fn ten_mb_paste_lands_quickly() {
    let state = doc_state("{}".into());
    let mut rt = app_runtime(state);
    press(&mut rt, Key::Command('a'), cmd_mods());
    let payload = format!("{{\"big\":\"{}\"}}", "x".repeat(10 * MIB));
    let t = Instant::now();
    rt.paste(payload.clone());
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(rt.state.active_tab().unwrap().input.len(), payload.len());
    assert_eq!(buffer_text(&rt).len(), payload.len());
    println!("10 MB paste (debug): {ms:.1} ms");
    assert!(ms < 5000.0, "10 MB paste guard");
    // Undo of the large paste is one step.
    let t = Instant::now();
    press(&mut rt, Key::Command('z'), cmd_mods());
    let undo_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(buffer_text(&rt), "{}");
    println!("10 MB paste undo (debug): {undo_ms:.1} ms");
}

#[test]
fn line_count_matches_str_lines_semantics() {
    let docs = [
        "",
        "a",
        "a\n",
        "a\r\n",
        "a\r",
        "\n",
        "\r",
        "a\nb",
        "a\nb\n",
        "a\r\nb\r\n",
        "{\n  \"a\": 1\n}\n",
    ];
    for doc in docs {
        // Fallback path (no editor entry), including the memo hit and a
        // revision bump invalidating it.
        editor_state::clear_all();
        assert_eq!(
            editor_state::display_line_count(999, doc, 1),
            doc.lines().count(),
            "fallback {doc:?}"
        );
        assert_eq!(
            editor_state::display_line_count(999, doc, 1),
            doc.lines().count(),
            "memo {doc:?}"
        );
        assert_eq!(
            editor_state::display_line_count(999, "x\ny\nz", 2),
            3,
            "revision bump {doc:?}"
        );
        // Rope path (editor entry exists).
        editor_state::with(999, doc, 3, |_| {});
        assert_eq!(
            editor_state::display_line_count(999, doc, 3),
            doc.lines().count(),
            "rope {doc:?}"
        );
    }
    editor_state::clear_all();
}

fn summary_text(rt: &Runtime<AppState>) -> String {
    rt.elements()
        .iter()
        .find(|e| e.key.as_deref() == Some("document-summary"))
        .and_then(|e| e.text.clone())
        .expect("document-summary")
}

#[test]
fn status_bar_line_count_stays_correct_after_edits() {
    let state = doc_state("a\nb\n".into());
    let mut rt = app_runtime(state);
    assert!(summary_text(&rt).contains("2 lines"), "{}", summary_text(&rt));

    // Enter at the end adds a line; the model cache is stale but the
    // status bar reads the rope.
    press(&mut rt, Key::End, Mods::default());
    press(&mut rt, Key::Enter, Mods::default());
    assert!(summary_text(&rt).contains("3 lines"), "{}", summary_text(&rt));
    press(&mut rt, Key::Text("c".into()), Mods::default());
    assert!(summary_text(&rt).contains("3 lines"), "{}", summary_text(&rt));

    // Select-all + delete empties the document.
    press(&mut rt, Key::Command('a'), cmd_mods());
    press(&mut rt, Key::Backspace, Mods::default());
    assert!(summary_text(&rt).contains("0 lines"), "{}", summary_text(&rt));
}

#[test]
fn status_bar_line_count_after_external_replace_in_output_view() {
    let state = doc_state("a\nb\n".into());
    let mut rt = app_runtime(state);
    assert!(summary_text(&rt).contains("2 lines"));
    // Leave the input view, then replace the text through the reducer:
    // the editor entry goes stale and the count must follow the new text.
    rt.state.dispatch(Msg::SwitchView(TabView::Output));
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    let id = rt.state.active;
    rt.update(|s, _| s.update_input(id, "one\ntwo\nthree\nfour\n".into()));
    rt.frame(1.0 / 60.0);
    assert!(summary_text(&rt).contains("4 lines"), "{}", summary_text(&rt));
}

#[test]
#[ignore]
fn probe_line_count_scan_cost() {
    let doc = pretty_fixture(100 * MIB);
    let t = Instant::now();
    let n = doc.lines().count();
    println!("100 MB lines().count() = {n} in {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let empty = doc.trim().is_empty();
    println!("100 MB trim().is_empty() = {empty} in {:.3} ms", t.elapsed().as_secs_f64() * 1000.0);
}

#[test]
fn app_switch_to_output_and_back_keeps_rope() {
    let state = doc_state("{\"a\":1}".into());
    let mut rt = app_runtime(state);
    press(&mut rt, Key::Text("Z".into()), Mods::default());
    rt.state.dispatch(Msg::SwitchView(TabView::Output));
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    rt.state.dispatch(Msg::SwitchView(TabView::Input));
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    assert_eq!(buffer_text(&rt), "Z{\"a\":1}");
}