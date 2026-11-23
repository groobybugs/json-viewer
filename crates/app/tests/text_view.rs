//! Integration evidence for the virtualized text view: headless PNGs, mono
//! font measurement, line index build timing and `Runtime::frame` timings at
//! the top/middle/bottom of a 2M-line document.
//!
//! Run in release for representative frame times:
//! `cargo test --release -p json-viewer --test text_view -- --nocapture`

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use json_viewer::headless::{element_tree, text_system};
use json_viewer::line_index;
use json_viewer::state::{AppState, TabView};
use json_viewer::ui::text_view::{TextView, LARGE_FILE_BANNER};
use json_viewer::ui::text_view::{char_width, LONG_LINE_LIMIT_BYTES};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::text::TextSystem;
use rune::{col, tok, Cx, El, Env, Runtime, ScrollSpec, TextEdit};
use serde_json::Value;

const SIZE: (f32, f32) = (960.0, 640.0);
/// Smaller viewport for the single-line evidence: keeps offscreen readback
/// (fixed GPU/PNG overhead) out of the way of the view-timing measurement.
const LONG_SIZE: (f32, f32) = (640.0, 360.0);
const SCALE: f32 = 1.0;
const BASE: &str = "output-text";

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("headless");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless")
}

fn doc_state(doc: String) -> AppState {
    doc_state_rev(doc, 0)
}

fn doc_state_rev(doc: String, revision: u64) -> AppState {
    let mut state = AppState::default();
    state.tabs[0].view = TabView::Output;
    state.tabs[0].output = doc;
    state.tabs[0].output_revision = revision;
    state
}

fn scroller(state: &AppState, first_column: usize, visible_columns: usize, hscroll: bool) -> El {
    let body = TextView::read_only(&state.tabs[0].output)
        .revision(state.tabs[0].output_revision)
        .line_numbers(true)
        .first_column(first_column)
        .visible_columns(visible_columns)
        .horizontal_scroll(hscroll)
        .build(BASE);
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(body)
}

fn view_default(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, 0, 240, false)
}

fn view_align(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, 0, 240, false)
}

fn view_long_start(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, 0, 160, true)
}

fn view_long_middle(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, 50_000_000, 160, true)
}

fn view_large_editable(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let edit = state.editor(state.active).expect("active editor");
    let body = TextView::editable(&state.tabs[0].input, edit)
        .editable_limit(64)
        .build("editor-1");
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(body)
}

/// Editable view at its default limits; used for the long-line gate, where
/// the input is one 6.9 MB line and the read-only virtual view takes over.
fn view_long_one_line(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let edit = state.editor(state.active).expect("active editor");
    let body = TextView::editable(&state.tabs[0].input, edit).build("editor-one-line");
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(body)
}

/// The measured advance must be Rune's actual monospace pitch, and the face it
/// measures must be fixed pitch: no test asserts an IBM Plex specific value,
/// only that equal-length runs of different glyphs are equally wide.
#[test]
fn measured_char_width_is_fixed_pitch() {
    let mono = tok::TextStyle::new(12.0, 400.0).mono();
    let mut app = text_system();
    let (width, zeros, i_run, w_run) = rune::text::lend(&mut app, || {
        (
            char_width(),
            rune::text::advance("0000", mono),
            rune::text::advance("iiii", mono),
            rune::text::advance("WWWW", mono),
        )
    });
    println!(
        "MEASURED_CHAR_WIDTH={width:.4} ZERO_RUN={zeros:.4} I_RUN={i_run:.4} W_RUN={w_run:.4}"
    );
    assert!(width.is_finite() && width > 0.0, "char width not measured");
    assert!(
        (zeros - 4.0 * width).abs() < 0.05,
        "char_width() is not the measured advance: {width} vs {}",
        zeros / 4.0
    );
    assert!(
        (i_run - w_run).abs() < 0.01,
        "the mono slot is not fixed pitch: i={i_run} W={w_run}"
    );
    assert!((zeros - i_run).abs() < 0.05, "digit and letter pitch differ");
}

/// Addendum 2: a single line longer than [`LONG_LINE_LIMIT_BYTES`] stays on
/// the read-only virtual path even when the file is under the size limit, and
/// the banner says why (so `Format` is the suggested fix).
#[test]
fn long_line_gate_reports_the_reason() {
    let doc = format!("[{}0]", "1,".repeat(100_000)); // one ~200 KB line
    let edit = TextEdit::default();
    let view = TextView::editable(&doc, &edit);
    assert!(
        view.editing_disabled(),
        "a 200 KB line must not enter editable mode"
    );
    assert!(doc.len() > LONG_LINE_LIMIT_BYTES);

    let mut state = AppState::default();
    state.tabs[0].input = doc;
    let gpu = Gpu::headless();
    let mut rt = fresh_runtime(state, view_long_one_line, SIZE);
    let tree = element_tree(&rt).to_string();
    assert!(
        tree.contains("Very long line") && tree.contains("Format to edit"),
        "long-line banner missing: {tree}"
    );
    let path = target_dir().join("text-view-long-line.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &gpu, &path, SIZE);
}

/// The benchmark fixture: 6.9 MB on one line is classified as a very long
/// line, not just a large file, so the banner can suggest Format.
#[test]
fn six_point_nine_megabyte_single_line_is_a_long_line() {
    let mut doc = String::with_capacity(7_300_000);
    doc.push_str("[1");
    while doc.len() < 7_236_000 {
        doc.push_str(",1");
    }
    doc.push(']');
    let edit = TextEdit::default();
    let view = TextView::editable(&doc, &edit);
    assert!(view.editing_disabled());

    let mut state = AppState::default();
    state.tabs[0].input = doc;
    let gpu = Gpu::headless();
    let mut rt = fresh_runtime(state, view_long_one_line, SIZE);
    let tree = element_tree(&rt).to_string();
    assert!(
        tree.contains("Very long line (6.9 MB on one line)"),
        "expected the long-line reason for a 6.9 MB one-liner: {tree}"
    );
    let path = target_dir().join("text-view-long-one-line.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &gpu, &path, SIZE);
}

type ViewFn = fn(&AppState, &mut Cx<AppState>) -> El;

fn fresh_runtime(state: AppState, view: ViewFn, size: (f32, f32)) -> Runtime<AppState> {
    // `Runtime::new` uses TextSystem::default() == ("Inter", "Inter"), which
    // makes mono() proportional. Use the app's registered faces everywhere.
    let mut rt = Runtime::with_text(state, view, size, Env::new(true, SCALE), text_system());
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    rt
}

fn reveal(rt: &mut Runtime<AppState>, line: usize) {
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_into_view(TextView::row_key(BASE, line));
    });
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
}

/// Draws with an already-created `Gpu` so repeated renders don't pay adapter
/// setup. The offscreen target is `size * SCALE` physical pixels.
fn draw(rt: &mut Runtime<AppState>, gpu: &Gpu, path: &Path, size: (f32, f32)) -> Value {
    let width = (size.0 * SCALE).round() as u32;
    let height = (size.1 * SCALE).round() as u32;
    let format = OFFSCREEN_FORMAT.remove_srgb_suffix();
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(true);
    let target = Offscreen::with_format(&gpu.device, width, height, format);
    renderer.prepare(
        &gpu.device,
        &gpu.queue,
        &mut rt.text.atlas,
        &rt.instances,
        (width, height),
    );
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&mut encoder, &target.view, rt.clear_color());
    target.copy(&mut encoder);
    renderer.submit(&gpu.queue, encoder);
    let pixels = target.read(&gpu.device);
    write_png(path, width, height, &pixels).expect("write_png failed");
    element_tree(rt)
}

/// Renders and prints the UI-thread view work separately from the fixed
/// offscreen GPU/PNG cost.
fn render(
    state: AppState,
    view: ViewFn,
    gpu: &Gpu,
    path: &Path,
    size: (f32, f32),
    label: &str,
) -> Value {
    let total = Instant::now();
    let t = Instant::now();
    let mut rt = fresh_runtime(state, view, size);
    let view_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let tree = draw(&mut rt, gpu, path, size);
    let draw_ms = t.elapsed().as_secs_f64() * 1000.0;
    let render_ms = total.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{label}_VIEW_MS={view_ms:.3} {label}_DRAW_MS={draw_ms:.1} {label}_RENDER_MS={render_ms:.1}"
    );
    assert!(std::fs::metadata(path).expect("PNG missing").len() > 100);
    tree
}

/// 2,000,000 lines, exactly 100,000,000 bytes (50 bytes per line).
fn huge_document() -> String {
    let mut doc = String::with_capacity(100 * 1024 * 1024 + 64);
    for i in 0..2_000_000usize {
        writeln!(
            doc,
            "     \"k{i:07}\": {{\"v\":{i:07},\"b\":true,\"n\":null}},"
        )
        .unwrap();
    }
    doc
}

fn alignment_doc() -> String {
    "{\n  \"iiii\": \"WWWW\",\n  \"WWWW\": 12345,\n  \"nested\": {\n    \"iWiWiW\": true,\n    \"WWWiii\": null\n  }\n}\n".to_owned()
}

/// Issue 1: prove the default ("Inter", "Inter") slots are proportional and
/// the app's registered slots resolve to a fixed-pitch face. Equal advances
/// for 'i' and 'W' runs, plus equal advances for padded gutter numbers, are
/// what make the PNG columns line up.
#[test]
fn mono_slot_resolves_to_a_fixed_pitch_face() {
    let mono = tok::TextStyle::new(12.0, 400.0).mono();

    let mut default = TextSystem::new(&[], ("Inter", "Inter"));
    let (bad_i, bad_w, bad_fixed) = rune::text::lend(&mut default, || {
        (
            rune::text::advance("iiii", mono),
            rune::text::advance("WWWW", mono),
            rune::text::ascii_metrics(mono).map(|m| m.2),
        )
    });
    println!(
        "DEFAULT_SLOTS_ADVANCE_i={bad_i:.3} W={bad_w:.3} FIXED_PITCH={bad_fixed:?}"
    );
    assert!(
        (bad_i - bad_w).abs() > 1.0,
        "expected proportional Inter in the default mono slot"
    );

    let mut app = text_system();
    let (good_i, good_w, fixed, gutter_1, gutter_2m) = rune::text::lend(&mut app, || {
        (
            rune::text::advance("iiii", mono),
            rune::text::advance("WWWW", mono),
            rune::text::ascii_metrics(mono).map(|m| m.2),
            rune::text::advance(&format!("{:>7}", 1), mono),
            rune::text::advance(&format!("{:>7}", 2_000_000), mono),
        )
    });
    println!(
        "APP_SLOTS_ADVANCE_i={good_i:.3} W={good_w:.3} FIXED_PITCH={fixed:?} GUTTER_1={gutter_1:.3} GUTTER_2M={gutter_2m:.3}"
    );
    assert_eq!(fixed, Some(true), "mono slot is not fixed pitch");
    assert!((good_i - good_w).abs() < 0.01, "mono advances differ");
    assert!(
        (gutter_1 - gutter_2m).abs() < 0.01,
        "right-aligned gutter numbers have different widths"
    );

    // `Family::SystemMono` on Linux resolves through fontconfig's `monospace`
    // (DejaVu Sans Mono here). It works, but the app uses `TextStyle::mono()`
    // == `Family::Mono`, i.e. the *slot*, so that is what must be registered.
    let system_mono = tok::TextStyle::new(12.0, 400.0).family(rune::Family::SystemMono);
    let (sys_i, sys_w, sys_fixed) = rune::text::lend(&mut app, || {
        (
            rune::text::advance("iiii", system_mono),
            rune::text::advance("WWWW", system_mono),
            rune::text::ascii_metrics(system_mono).map(|m| m.2),
        )
    });
    println!(
        "SYSTEM_MONO_ADVANCE_i={sys_i:.3} W={sys_w:.3} FIXED_PITCH={sys_fixed:?}"
    );

    // The real window path: `tooling::runtime_for` calls exactly this.
    let mut window = TextSystem::for_window(
        json_viewer::headless::APP_FONTS,
        json_viewer::headless::APP_FAMILIES,
    );
    let (sans, mono_family) = window.families();
    println!("WINDOW_FAMILIES=({sans}, {mono_family})");
    assert_eq!((sans, mono_family), json_viewer::headless::APP_FAMILIES);
    let window_fixed = rune::text::lend(&mut window, || {
        rune::text::ascii_metrics(mono).map(|m| m.2)
    });
    assert_eq!(window_fixed, Some(true), "window mono slot is not fixed pitch");
}

/// A PNG whose columns align: mixed 'i'/'W' runs and nested indentation.
#[test]
fn alignment_png_has_a_monospaced_gutter() {
    let directory = target_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let gpu = Gpu::headless();
    let tree = render(
        doc_state(alignment_doc()),
        view_align,
        &gpu,
        &directory.join("text-view-mono-align.png"),
        SIZE,
        "MONO_ALIGN",
    );
    let rendered = tree.to_string();
    assert!(rendered.contains("iWiWiW"), "alignment text missing");
    assert!(tree["elements"].as_array().is_some_and(|e| !e.is_empty()));
}

/// Issue 3: a cold 100 MB miss must cost only the bounded window, not O(n).
#[test]
fn cold_large_miss_is_o_window() {
    let doc = "z".repeat(100 * 1024 * 1024);
    let t = Instant::now();
    let snapshot = line_index::snapshot(&doc);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "COLD_SNAPSHOT_100MB_MS={ms:.3} PARTIAL={} PREFIX_BYTES={} TOTAL_LEN={}",
        snapshot.is_partial(),
        snapshot.text().len(),
        snapshot.index().total_len()
    );
    assert!(snapshot.is_partial());
    assert_eq!(snapshot.text().len(), line_index::PARTIAL_SCAN_BYTES);
    assert_eq!(snapshot.index().total_len(), doc.len());
    assert!(ms < 16.0, "cold first render did O(document) work: {ms:.3} ms");
}

#[test]
fn editable_fallback_shows_the_large_file_banner() {
    let mut state = AppState::default();
    state.tabs[0].input = "x".repeat(128);
    let gpu = Gpu::headless();
    let mut rt = fresh_runtime(state, view_large_editable, SIZE);
    let tree = element_tree(&rt).to_string();
    assert!(
        tree.contains(LARGE_FILE_BANNER),
        "banner missing from the element tree: {tree}"
    );
    let path = target_dir().join("text-view-large-file.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &gpu, &path, SIZE);
}

#[test]
fn small_formatted_json_has_syntax_colours() {
    let pretty = serde_json::to_string_pretty(&serde_json::json!({
        "name": "json-viewer",
        "version": 3,
        "enabled": true,
        "nothing": null,
        "items": [1, 2.5, -4, {"key": "value"}]
    }))
    .unwrap();
    let gpu = Gpu::headless();
    let mut rt = fresh_runtime(doc_state(pretty), view_default, SIZE);
    let inks = rt
        .instances
        .iter()
        .map(|i| {
            [
                i.fill[0].to_bits(),
                i.fill[1].to_bits(),
                i.fill[2].to_bits(),
                i.fill[3].to_bits(),
            ]
        })
        .collect::<std::collections::HashSet<_>>()
        .len();
    println!("SMALL_JSON_DISTINCT_INKS={inks}");
    assert!(inks >= 5, "expected distinct syntax inks, got {inks}");
    let tree = element_tree(&rt).to_string();
    assert!(tree.contains("json-viewer"), "formatted JSON text missing");
    let path = target_dir().join("text-view-small-colors.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &gpu, &path, SIZE);
}

#[test]
fn headless_pngs_and_frame_timings() {
    let directory = target_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let gpu = Gpu::headless();

    // ── 100 MB / 2M-line document, index built OFF the UI thread ───────
    let built = Instant::now();
    let doc = huge_document();
    println!(
        "DOCUMENT_BYTES={} DOCUMENT_LINES={} GENERATE_MS={:.1}",
        doc.len(),
        doc.lines().count(),
        built.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(doc.lines().count(), 2_000_000);
    assert_eq!(doc.len(), 100_000_000);

    // The JSON worker owns the output String; converting it to Arc<str> and
    // building the index happens on that worker, never in the view.
    let owned: Arc<str> = Arc::from(doc.as_str());
    let revision = line_index::next_revision();
    let started = Instant::now();
    let snapshot = line_index::prewarm_off_thread(owned, revision)
        .join()
        .expect("prewarm thread panicked");
    let index_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "LINE_INDEX_OFF_THREAD_100MB_MS={index_ms:.1} LINES={} INDEX_BYTES={} COMPLETE={}",
        snapshot.line_count(),
        snapshot.index().len() * std::mem::size_of::<usize>(),
        snapshot.index().is_complete()
    );
    assert_eq!(snapshot.line_count(), 2_000_001); // trailing newline
    assert!(!snapshot.is_partial());

    let cached = Instant::now();
    let again = line_index::snapshot_for_revision(&doc, revision);
    println!(
        "LINE_INDEX_CACHED_LOOKUP_US={:.1}",
        cached.elapsed().as_secs_f64() * 1_000_000.0
    );
    assert!(Arc::ptr_eq(&snapshot, &again));
    drop(again);

    // ── Frame timings at top / middle / bottom (release) ───────────────
    {
        let mut rt = fresh_runtime(doc_state_rev(doc.clone(), revision), view_default, SIZE);
        let top = Instant::now();
        rt.frame(1.0 / 60.0);
        println!("FRAME_TOP_MS={:.3}", top.elapsed().as_secs_f64() * 1000.0);

        reveal(&mut rt, 1_000_000);
        let middle = Instant::now();
        rt.frame(1.0 / 60.0);
        println!(
            "FRAME_MIDDLE_MS={:.3}",
            middle.elapsed().as_secs_f64() * 1000.0
        );

        reveal(&mut rt, 1_999_900);
        let bottom = Instant::now();
        rt.frame(1.0 / 60.0);
        println!(
            "FRAME_BOTTOM_MS={:.3}",
            bottom.elapsed().as_secs_f64() * 1000.0
        );

        reveal(&mut rt, 1_000_000);
        let tree = draw(&mut rt, &gpu, &directory.join("text-view-100mb-middle.png"), SIZE);
        let rendered = tree.to_string();
        println!(
            "MIDDLE_TREE_ELEMENTS={} HAS_TARGET_ROW={}",
            tree["elements"].as_array().map_or(0, Vec::len),
            rendered.contains("output-text-row-1000000")
        );
        assert!(
            rendered.contains("output-text-row-1000000"),
            "virtual row at line 1,000,000 was not mounted after scroll_into_view"
        );
    }
    drop(snapshot);

    // ── 100 MB minified single line: column window, not the whole line ──
    {
        let mut minified = String::with_capacity(100 * 1024 * 1024 + 32);
        minified.push_str("{\"data\":\"");
        minified.push_str(&"x".repeat(100 * 1024 * 1024));
        minified.push_str("\"}");
        println!(
            "MINIFIED_BYTES={} MINIFIED_LINES={}",
            minified.len(),
            minified.lines().count()
        );

        // Full index for the column-50M window is built off the UI thread.
        let owned: Arc<str> = Arc::from(minified.as_str());
        let revision = line_index::next_revision();
        let started = Instant::now();
        let warmed = line_index::prewarm_off_thread(owned, revision)
            .join()
            .expect("prewarm thread panicked");
        println!(
            "LINE_INDEX_OFF_THREAD_MINIFIED_MS={:.1} LINES={}",
            started.elapsed().as_secs_f64() * 1000.0,
            warmed.line_count()
        );
        assert_eq!(warmed.line_count(), 1);

        let start_tree = render(
            doc_state_rev(minified.clone(), revision),
            view_long_start,
            &gpu,
            &directory.join("text-view-long-line-start.png"),
            LONG_SIZE,
            "LONG_LINE_START",
        );
        println!(
            "LONG_LINE_START_ELEMENTS={}",
            start_tree["elements"].as_array().map_or(0, Vec::len)
        );
        assert!(start_tree.to_string().contains("output-text-row-0"));

        let middle_tree = render(
            doc_state_rev(minified, revision),
            view_long_middle,
            &gpu,
            &directory.join("text-view-long-line-middle.png"),
            LONG_SIZE,
            "LONG_LINE_MIDDLE",
        );
        println!(
            "LONG_LINE_MIDDLE_ELEMENTS={}",
            middle_tree["elements"].as_array().map_or(0, Vec::len)
        );
        assert!(middle_tree.to_string().contains("output-text-row-0"));
    }
}
