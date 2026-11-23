//! Integration evidence for the virtualized text view: headless PNGs, line
//! index build timing and `Runtime::frame` timings at the top/middle/bottom of
//! a 2M-line document.
//!
//! Run in release for representative frame times:
//! `cargo test --release -p json-viewer --test text_view -- --nocapture`

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use json_viewer::headless::element_tree;
use json_viewer::line_index;
use json_viewer::state::{AppState, TabView};
use json_viewer::ui::text_view::{TextView, LARGE_FILE_BANNER};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{col, Cx, El, Env, Runtime, ScrollSpec};
use serde_json::Value;

const SIZE: (f32, f32) = (960.0, 640.0);
const SCALE: f32 = 1.0;
const BASE: &str = "output-text";

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("headless");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless")
}

fn doc_state(doc: String) -> AppState {
    let mut state = AppState::default();
    state.tabs[0].view = TabView::Output;
    state.tabs[0].output = doc;
    state
}

fn scroller(state: &AppState, first_column: usize, visible_columns: usize, hscroll: bool) -> El {
    let body = TextView::read_only(&state.tabs[0].output)
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

type ViewFn = fn(&AppState, &mut Cx<AppState>) -> El;

fn fresh_runtime(state: AppState, view: ViewFn) -> Runtime<AppState> {
    let mut rt = Runtime::new(state, view, SIZE, Env::new(true, SCALE));
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

fn draw(rt: &mut Runtime<AppState>, path: &Path) -> Value {
    let width = (SIZE.0 * SCALE).round() as u32;
    let height = (SIZE.1 * SCALE).round() as u32;
    let gpu = Gpu::headless();
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

fn render(state: AppState, view: ViewFn, path: &Path) -> Value {
    let mut rt = fresh_runtime(state, view);
    let tree = draw(&mut rt, path);
    assert!(std::fs::metadata(path).expect("PNG missing").len() > 100);
    tree
}

fn distinct_inks(rt: &Runtime<AppState>) -> usize {
    rt.instances
        .iter()
        .map(|i| {
            [
                i.fill[0].to_bits(),
                i.fill[1].to_bits(),
                i.fill[2].to_bits(),
                i.fill[3].to_bits(),
            ]
        })
        .collect::<HashSet<_>>()
        .len()
}

/// 2,000,000 lines, ~100 MB: each line is exactly 50 bytes.
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

#[test]
fn editable_fallback_shows_the_large_file_banner() {
    let mut state = AppState::default();
    state.tabs[0].input = "x".repeat(128);
    let mut rt = fresh_runtime(state, view_large_editable);
    let tree = element_tree(&rt).to_string();
    assert!(
        tree.contains(LARGE_FILE_BANNER),
        "banner missing from the element tree: {tree}"
    );
    let path = target_dir().join("text-view-large-file.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &path);
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
    let mut rt = fresh_runtime(doc_state(pretty), view_default);
    let inks = distinct_inks(&rt);
    println!("SMALL_JSON_DISTINCT_INKS={inks}");
    assert!(
        inks >= 5,
        "expected at least five distinct glyph/box inks for keys, strings, numbers, literals and punctuation, got {inks}"
    );
    let tree = element_tree(&rt).to_string();
    assert!(tree.contains("json-viewer"), "formatted JSON text missing");
    let path = target_dir().join("text-view-small-colors.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    draw(&mut rt, &path);
}

#[test]
fn headless_pngs_and_frame_timings() {
    let directory = target_dir();
    std::fs::create_dir_all(&directory).unwrap();
    line_index::clear_cache();

    // ── 100 MB / 2M-line document ─────────────────────────────────────
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

    let started = Instant::now();
    let snapshot = line_index::snapshot(&doc);
    let index_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "LINE_INDEX_BUILD_100MB_MS={index_ms:.1} LINES={} INDEX_BYTES={}",
        snapshot.line_count(),
        snapshot.index().len() * std::mem::size_of::<usize>()
    );
    assert_eq!(snapshot.line_count(), 2_000_001); // trailing newline

    // Cached lookup must be O(1), no rebuild.
    let cached = Instant::now();
    let again = line_index::snapshot(&doc);
    println!(
        "LINE_INDEX_CACHED_LOOKUP_US={:.1}",
        cached.elapsed().as_secs_f64() * 1_000_000.0
    );
    assert!(std::sync::Arc::ptr_eq(&snapshot, &again));
    drop(again);

    // ── Frame timings at top / middle / bottom (release) ───────────────
    {
        let mut rt = fresh_runtime(doc_state(doc.clone()), view_default);
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

        // Back to the middle and render the PNG all load-bearing evidence.
        reveal(&mut rt, 1_000_000);
        let tree = draw(&mut rt, &directory.join("text-view-100mb-middle.png"));
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
        println!("MINIFIED_BYTES={} MINIFIED_LINES={}", minified.len(), minified.lines().count());

        let t = Instant::now();
        let start_tree = render(
            doc_state(minified.clone()),
            view_long_start,
            &directory.join("text-view-long-line-start.png"),
        );
        println!(
            "LONG_LINE_START_ELEMENTS={} RENDER_MS={:.1}",
            start_tree["elements"].as_array().map_or(0, Vec::len),
            t.elapsed().as_secs_f64() * 1000.0
        );
        assert!(start_tree.to_string().contains("output-text-row-0"));

        let t = Instant::now();
        let middle_tree = render(
            doc_state(minified),
            view_long_middle,
            &directory.join("text-view-long-line-middle.png"),
        );
        println!(
            "LONG_LINE_MIDDLE_ELEMENTS={} RENDER_MS={:.1}",
            middle_tree["elements"].as_array().map_or(0, Vec::len),
            t.elapsed().as_secs_f64() * 1000.0
        );
        assert!(middle_tree.to_string().contains("output-text-row-0"));
    }

    // ── Off-UI-thread index build for a large document ─────────────────
    // `prewarm_off_thread` is the entry point a background job calls before
    // switching the view to a large result; the cached lookup afterwards is
    // O(1) and never rebuilds.
    line_index::clear_cache();
    let owned: std::sync::Arc<str> = std::sync::Arc::from(doc.as_str());
    let started = Instant::now();
    let handle = line_index::prewarm_off_thread(owned);
    let warmed = handle.join().expect("prewarm thread panicked");
    println!(
        "LINE_INDEX_OFF_THREAD_100MB_MS={:.1} LINES={}",
        started.elapsed().as_secs_f64() * 1000.0,
        warmed.line_count()
    );
    let cached = Instant::now();
    let hit = line_index::snapshot(&doc);
    println!(
        "LINE_INDEX_OFF_THREAD_CACHED_US={:.1}",
        cached.elapsed().as_secs_f64() * 1_000_000.0
    );
    assert!(std::sync::Arc::ptr_eq(&warmed, &hit));
    assert_eq!(warmed.line_count(), 2_000_001);
}
