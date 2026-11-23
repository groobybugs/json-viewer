//! Evidence for Output-view code folding and the Validate error marker.
//!
//! Fast tests render PNGs into `target/headless/fold-*.png` and assert on the
//! element tree; the ignored `release_*` tests print the timings the feature
//! report quotes. Run those with:
//!
//! `cargo test --release -p json-viewer --test folding -- --ignored --nocapture`

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use json_viewer::fold_index::{self, FoldIndex};
use json_viewer::headless::{self, element_tree, text_system};
use json_viewer::line_index;
use json_viewer::runner;
use json_viewer::search::SearchHit;
use json_viewer::state::{AppState, TabView};
use json_viewer::ui::text_view::{apply_fold_action, FoldAction, TextView};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{col, Cx, El, Env, KeyEvent, Runtime, ScrollSpec};
use serde_json::Value;

const SIZE: (f32, f32) = (960.0, 640.0);
const SCALE: f32 = 1.0;

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("headless");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless")
}

fn doc_state(doc: &str, revision: u64) -> AppState {
    let mut state = AppState::default();
    state.tabs[0].view = TabView::Output;
    state.tabs[0].output = doc.to_owned();
    state.tabs[0].output_revision = revision;
    state
}

fn scroller(state: &AppState, base: &str) -> El {
    let body = TextView::read_only(&state.tabs[0].output)
        .revision(state.tabs[0].output_revision)
        .line_numbers(true)
        .build(base);
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body)
}

// One view fn per test: the fold store keys pending reveals by view base, so
// tests running in parallel must not share one.
fn view_small(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-small")
}
fn view_all(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-all")
}
fn view_levels(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-levels")
}
fn view_jump(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-jump")
}
fn view_numbers(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-numbers")
}
fn view_png(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    scroller(state, "fold-png")
}
fn view_error(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let body = TextView::read_only(&state.tabs[0].output)
        .revision(state.tabs[0].output_revision)
        .line_numbers(true);
    let body = match state.tabs[0].validation_error() {
        Some(error) => body.error_marker(error.line, error.column),
        None => body,
    };
    let body = body.build("fold-error");
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body)
}
fn view_error_editable(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let edit = state.editor(state.active).expect("active editor");
    let body =
        TextView::editable(&state.tabs[0].input, edit).revision(state.tabs[0].input_revision);
    let body = match state.tabs[0].validation_error() {
        Some(error) => body.error_marker(error.line, error.column),
        None => body,
    };
    let body = body.build("fold-error-input");
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body)
}

fn fresh_runtime(
    state: AppState,
    view: fn(&AppState, &mut Cx<AppState>) -> El,
) -> Runtime<AppState> {
    let mut rt = Runtime::with_text(state, view, SIZE, Env::new(true, SCALE), text_system());
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    rt
}

fn find_center(rt: &Runtime<AppState>, key: &str) -> Option<(f32, f32)> {
    for el in rt.elements() {
        if el.exiting || el.visible[2] <= 0.0 || el.visible[3] <= 0.0 {
            continue;
        }
        if el.key.as_deref() == Some(key) {
            return Some((el.rect[0] + el.rect[2] * 0.5, el.rect[1] + el.rect[3] * 0.5));
        }
    }
    None
}

fn click(rt: &mut Runtime<AppState>, key: &str) {
    let (x, y) = find_center(rt, key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    rt.settle_all();
    rt.frame(0.0);
}

fn draw(rt: &mut Runtime<AppState>, gpu: &Gpu, path: &Path) -> Value {
    let width = (SIZE.0 * SCALE).round() as u32;
    let height = (SIZE.1 * SCALE).round() as u32;
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

fn render_to(rt: &mut Runtime<AppState>, name: &str) -> Value {
    let dir = target_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let gpu = Gpu::headless();
    let tree = draw(rt, &gpu, &path);
    assert!(std::fs::metadata(&path).expect("PNG missing").len() > 100);
    let tree_path = path.with_extension("tree.json");
    std::fs::write(&tree_path, serde_json::to_string_pretty(&tree).unwrap()).unwrap();
    tree
}

fn small_doc() -> String {
    "{\n  \"a\": {\n    \"x\": 1,\n    \"y\": 2\n  },\n  \"b\": [1, 2, 3]\n}\n".to_owned()
}

fn api_doc() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": "json-viewer",
        "version": 3,
        "enabled": true,
        "tags": ["json", "rust", "rune", "desktop"],
        "limits": {"maxBytes": 104857600, "maxLines": 2000000, "strict": true},
        "tabs": [
            {"id": 1, "title": "JSON 1", "dirty": false},
            {"id": 2, "title": "JSON 2", "dirty": true},
            {"id": 3, "title": "JSON 3", "dirty": false}
        ],
        "nested": {"a": {"b": {"c": {"d": 1}}}}
    }))
    .unwrap()
}

fn element_keys(tree: &Value) -> Vec<String> {
    tree["elements"]
        .as_array()
        .map(|els| {
            els.iter()
                .filter_map(|el| el["key"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn fold_arrow_collapses_to_a_placeholder_with_a_badge() {
    let revision = 7_100_001;
    let mut rt = fresh_runtime(doc_state(&small_doc(), revision), view_small);
    assert!(
        find_center(&rt, "fold-small-row-2").is_some(),
        "nested line should be mounted before folding"
    );

    click(&mut rt, "fold-small-fold-1");
    let tree = render_to(&mut rt, "fold-small.png");
    let text = tree.to_string();
    assert!(text.contains("{…}"), "placeholder missing: {text}");
    assert!(text.contains("2 keys"), "badge missing: {text}");
    assert!(
        find_center(&rt, "fold-small-row-2").is_none(),
        "hidden line must not be mounted"
    );
    assert_eq!(
        tree["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|el| el["key"].as_str() == Some("fold-small-row-1"))
            .count(),
        1,
        "the folded opening line stays visible"
    );

    // Clicking the muted badge unfolds.
    click(&mut rt, "fold-small-fold-badge-1");
    assert!(
        find_center(&rt, "fold-small-row-2").is_some(),
        "badge click should unfold"
    );

    // Clicking the placeholder line unfolds too.
    click(&mut rt, "fold-small-fold-1");
    assert!(find_center(&rt, "fold-small-row-2").is_none());
    click(&mut rt, "fold-small-line-1");
    assert!(
        find_center(&rt, "fold-small-row-2").is_some(),
        "placeholder click should unfold"
    );
}

#[test]
fn fold_all_and_fold_level_pngs() {
    let revision = 7_100_002;
    let mut state = doc_state(&api_doc(), revision);
    let key = fold_index::key(revision, &state.tabs[0].output);
    apply_fold_action(&mut state, FoldAction::FoldAll);
    assert!(fold_index::folded_count(key) > 0);
    let mut rt = fresh_runtime(state, view_all);
    let tree = render_to(&mut rt, "fold-all.png");
    let text = tree.to_string();
    assert!(
        text.contains("{…}") || text.contains("[…]"),
        "no placeholder: {text}"
    );
    // Every container is folded, so almost every document line is hidden.
    assert!(
        find_center(&rt, "fold-all-row-5").is_none(),
        "line 5 should be hidden by fold-all"
    );

    // "Fold to level 2": top-level containers stay open, deeper ones collapse.
    let revision = 7_100_003;
    let mut state = doc_state(&api_doc(), revision);
    apply_fold_action(&mut state, FoldAction::Level(2));
    let mut rt = fresh_runtime(state, view_levels);
    let tree = render_to(&mut rt, "fold-level-2.png");
    let text = tree.to_string();
    assert!(
        text.contains("[…]") || text.contains("{…}"),
        "level 2 should fold nested containers: {text}"
    );
    assert!(
        find_center(&rt, "fold-levels-row-1").is_some(),
        "level 2 keeps the root's own members visible"
    );
    let root = 0u32;
    let root_region = FoldIndex::build(&api_doc()).region_at(root).unwrap();
    assert!(root_region.child_count >= 5);
}

#[test]
fn toolbar_fold_buttons_collapse_and_restore_the_output() {
    let revision = 7_100_004;
    let mut state = doc_state(&api_doc(), revision);
    state.tabs[0].output_revision = revision;
    let key = fold_index::key(revision, &state.tabs[0].output);

    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    rt.frame(0.0);
    assert!(
        find_center(&rt, "fold-all").is_some(),
        "fold-all button missing"
    );

    click(&mut rt, "fold-all");
    for _ in 0..4 {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.frame(1.0 / 60.0);
    }
    rt.settle_all();
    rt.frame(0.0);
    assert!(fold_index::folded_count(key) > 0, "toolbar did not fold");
    let tree = element_tree(&rt).to_string();
    assert!(
        tree.contains("{…}") || tree.contains("[…]"),
        "folded output should show placeholders: {tree}"
    );

    click(&mut rt, "unfold-all");
    for _ in 0..4 {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.frame(1.0 / 60.0);
    }
    rt.settle_all();
    rt.frame(0.0);
    assert_eq!(fold_index::folded_count(key), 0, "unfold-all left folds");
}

#[test]
fn reveal_of_a_folded_line_unfolds_it_before_scrolling() {
    let revision = 7_100_005;
    let doc = api_doc();
    let mut state = doc_state(&doc, revision);
    let key = fold_index::key(revision, &state.tabs[0].output);
    apply_fold_action(&mut state, FoldAction::FoldAll);
    let before = fold_index::folded_count(key);
    assert!(before > 0);

    let mut rt = fresh_runtime(state, view_jump);
    // A document line deep inside the folded root.
    let target = 12usize;
    assert!(
        find_center(&rt, &format!("fold-jump-row-{target}")).is_none(),
        "target row must start hidden"
    );

    // `row_key` arms the reveal; the next build unfolds its ancestors.
    let row = TextView::row_key("fold-jump", target);
    // Force the commit a real caller gets from `SwitchView`/`Status`.
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    rt.settle_all();
    rt.frame(0.0);
    assert!(
        fold_index::folded_count(key) < before,
        "reveal should have unfolded the enclosing folds"
    );
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_into_view(row.clone());
    });
    for _ in 0..20 {
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        if find_center(&rt, &row).is_some() {
            break;
        }
    }
    assert!(
        find_center(&rt, &row).is_some(),
        "revealed row {row} was not mounted after unfolding"
    );
}

#[test]
fn line_numbers_stay_document_numbers_when_folded() {
    let revision = 7_100_006;
    let doc = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n  \"d\": 4\n}\n";
    let mut state = doc_state(doc, revision);
    apply_fold_action(&mut state, FoldAction::FoldAll);
    let rt = fresh_runtime(state, view_numbers);

    // Root fold hides lines 1..=5; the closing brace (doc line 6) stays.
    assert!(find_center(&rt, "fold-numbers-row-1").is_none());
    let row = find_center(&rt, "fold-numbers-row-6").expect("closing line visible");
    let gutter: Vec<String> = rt
        .elements()
        .iter()
        .filter(|el| {
            el.key.is_none()
                && el.rect[1] <= row.1
                && row.1 <= el.rect[1] + el.rect[3]
                && el.rect[0] < 200.0
        })
        .filter_map(|el| el.text.clone())
        .filter(|text| {
            let trimmed = text.trim();
            !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    assert!(
        gutter.iter().any(|text| text.trim() == "7"),
        "gutter for the closing brace should say 7, got {gutter:?}"
    );
}

#[test]
fn error_marker_pngs_read_only_and_editable() {
    let revision = 7_100_007;
    let input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3\n}\n";
    let error = json_viewer::json_ops::validate(input).expect_err("invalid fixture");
    assert_eq!((error.line, error.column), (3, 8));
    assert_eq!(input.lines().nth(2).unwrap().as_bytes()[7], b'o');

    let mut read_only = doc_state(input, revision);
    read_only.tabs[0].validation =
        Some(json_viewer::state::ValidationResult::Invalid(error.clone()));
    let mut rt = fresh_runtime(read_only, view_error);
    let tree = render_to(&mut rt, "fold-error-read-only.png");
    assert!(
        element_keys(&tree)
            .iter()
            .any(|key| key == "fold-error-error-dot-2"),
        "error gutter dot missing: {tree}"
    );

    let mut state = AppState::default();
    let active = state.active;
    state.update_input(active, input.to_owned());
    state.tabs[0].view = TabView::Input;
    state.tabs[0].validation = Some(json_viewer::state::ValidationResult::Invalid(error));
    let mut rt = fresh_runtime(state, view_error_editable);
    let tree = render_to(&mut rt, "fold-error-editable.png");
    let text = tree.to_string();
    assert!(
        text.contains("line 3, column 8"),
        "error label missing: {text}"
    );
    assert!(
        element_keys(&tree)
            .iter()
            .any(|key| key == "fold-error-input-error-dot"),
        "editable gutter dot missing"
    );
}

#[test]
fn output_search_hit_click_unfolds_and_reveals_the_row() {
    let revision = 7_100_009;
    let doc = api_doc();
    let target = doc
        .lines()
        .position(|line| line.contains("\"rust\""))
        .unwrap();
    let mut state = doc_state(&doc, revision);
    let key = fold_index::key(revision, &state.tabs[0].output);
    apply_fold_action(&mut state, FoldAction::FoldAll);
    let folded = fold_index::folded_count(key);
    assert!(folded > 0);
    state.apply_search_results(
        state.active,
        vec![SearchHit {
            path: "tags → [1]".into(),
            key: None,
            value_preview: "rust".into(),
            line: Some(target + 1),
        }],
    );

    let mut rt = headless::runtime(state, SIZE, 1.0);
    let row = format!("output-text-row-{target}");
    assert!(find_center(&rt, &row).is_none(), "target starts folded");
    click(&mut rt, "search-hit-0");
    for _ in 0..20 {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        if find_center(&rt, &row).is_some() {
            break;
        }
    }
    assert_eq!(rt.state.active_tab().unwrap().view, TabView::Output);
    assert!(fold_index::folded_count(key) < folded, "reveal kept folds");
    assert!(find_center(&rt, &row).is_some(), "hit row not mounted");
    let tree = render_to(&mut rt, "fold-search-hit.png");
    assert!(tree.to_string().contains("rust"));
}

/// Format the keyboard chord exactly as the window receives it, so the
/// toolbar/keyboard layer can be tested without a window.
#[test]
fn keyboard_fold_chords_apply_to_the_output() {
    let revision = 7_100_008;
    let state = doc_state(&api_doc(), revision);
    let key = fold_index::key(revision, &state.tabs[0].output);
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    // `headless::runtime` does not install the window's key hook; `main.rs`
    // does. Installing it here exercises the same path the window uses.
    rt.on_key = Some(json_viewer::ui::on_key);
    rt.frame(0.0);
    // Ctrl+K arms the chord; Ctrl+0 completes it.
    let prefix = KeyEvent {
        key: rune::Key::Command('k'),
        mods: rune::Mods {
            cmd: true,
            ctrl: true,
            ..Default::default()
        },
    };
    let zero = KeyEvent {
        key: rune::Key::Command('0'),
        mods: rune::Mods {
            cmd: true,
            ctrl: true,
            ..Default::default()
        },
    };
    let mut armed = false;
    rt.update_if(|state, _| {
        armed = state.fold_chord;
        false
    });
    assert!(!armed);
    rt.key(prefix.clone());
    rt.frame(1.0 / 60.0);
    rt.update_if(|state, _| {
        armed = state.fold_chord;
        false
    });
    assert!(armed, "Ctrl+K should arm the fold chord");
    rt.key(zero);
    rt.frame(1.0 / 60.0);
    rt.settle_all();
    rt.frame(0.0);
    assert!(
        fold_index::folded_count(key) > 0,
        "Ctrl+K Ctrl+0 should fold all"
    );
    let mut status = None;
    rt.update_if(|state, tx| {
        runner::poll(state, 0.0, tx);
        status = state.status.clone();
        false
    });
    assert!(
        status.as_deref().is_some_and(|s| s.starts_with("Folded")),
        "status should report the fold: {status:?}"
    );
    // The same chord with Ctrl+J unfolds everything.
    let j = KeyEvent {
        key: rune::Key::Command('j'),
        mods: rune::Mods {
            cmd: true,
            ctrl: true,
            ..Default::default()
        },
    };
    rt.key(prefix);
    rt.frame(1.0 / 60.0);
    rt.key(j);
    rt.frame(1.0 / 60.0);
    rt.settle_all();
    rt.frame(0.0);
    assert_eq!(fold_index::folded_count(key), 0);
}

// ── Release timings (ignored by default) ─────────────────────────────────

/// 6.9 MB / 446k lines, shaped like a formatted API response.
fn doc_6_9mb() -> String {
    let mut doc = String::with_capacity(7 * 1024 * 1024);
    doc.push_str("{\n  \"items\": [\n");
    let mut i = 0usize;
    while doc.len() < 7_236_000 {
        writeln!(
            doc,
            "    {{\"id\": {i}, \"name\": \"item-{i}\", \"active\": true, \"score\": {i}.5}},"
        )
        .unwrap();
        i += 1;
    }
    doc.push_str("  ]\n}\n");
    doc
}

/// 100 MB / 2M lines: one object per line inside a large array.
fn doc_100mb() -> String {
    let mut doc = String::with_capacity(100 * 1024 * 1024 + 64);
    doc.push_str("{\n  \"items\": [\n");
    for i in 0..2_000_000usize {
        // 50 bytes per line, the same shape the text_view perf harness uses.
        writeln!(
            doc,
            "     \"k{i:07}\": {{\"v\":{i:07},\"b\":true,\"n\":null}},"
        )
        .unwrap();
    }
    doc.push_str("  ]\n}\n");
    doc
}

/// ~100 MB / 2M lines with one multi-line object per item (7 lines each), so
/// fold regions scale with the document instead of staying at 2.
fn doc_2m_nested() -> String {
    let mut doc = String::with_capacity(101 * 1024 * 1024);
    doc.push_str("{\n  \"items\": [\n");
    // Eight lines per item: 250_000 items pad to exactly 2M lines.
    let payload = "p".repeat(230);
    for i in 0..250_000usize {
        writeln!(doc, "    {{\n      \"id\": {i:08},\n      \"name\": \"item-{i:08}\",\n      \"active\": true,\n      \"score\": {i}.5,\n      \"note\": \"fold benchmark note {i:08}\",\n      \"payload\": \"{payload}\"\n    }},").unwrap();
    }
    doc.push_str("  ]\n}\n");
    doc
}

fn line_count(text: &str) -> usize {
    text.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1
}

#[test]
#[ignore = "release timing evidence: cargo test --release --test folding -- --ignored"]
fn release_fold_index_build_7mb_and_100mb() {
    for (label, doc) in [("7MB", doc_6_9mb()), ("100MB", doc_100mb())] {
        let lines = line_count(&doc);
        let t = Instant::now();
        let index = FoldIndex::build(&doc);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "FOLD_INDEX_{label}_MS={ms:.1} BYTES={} LINES={lines} REGIONS={} COMPLETE={}",
            doc.len(),
            index.len(),
            index.is_complete()
        );
        assert!(ms < 5_000.0, "fold index build too slow: {ms:.1} ms");
    }
}

#[test]
#[ignore = "release timing evidence: cargo run in release"]
fn release_fold_all_and_frame_time_on_2m_lines() {
    let doc = doc_2m_nested();
    let revision = 7_200_001;
    let key = revision;
    // Build the full line index too: a provisional snapshot only holds the
    // first 64 KiB, and rows past it would render empty.
    let text: Arc<str> = Arc::from(doc.as_str());
    let t = Instant::now();
    line_index::prewarm_off_thread(Arc::clone(&text), key)
        .join()
        .expect("line index prewarm");
    let index = fold_index::view_for(&doc, key, 0, "timing-probe")
        .index()
        .clone();
    let prewarm_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "FOLD_PREWARM_2M_MS={prewarm_ms:.1} REGIONS={} LINES={} BYTES={}",
        index.len(),
        index.line_count(),
        doc.len()
    );

    let mut state = doc_state(&doc, revision);
    state.tabs[0].output_revision = revision;
    let t = Instant::now();
    let mut rt = fresh_runtime(state, view_all);
    let initial_unfolded_ms = t.elapsed().as_secs_f64() * 1000.0;

    // A fold action re-runs the view with a new visible-row count, which is
    // the one O(rows) pass in Rune's keyed virtual list. Measure it alone.
    let t = Instant::now();
    let folded = fold_index::fold_all(key);
    let fold_all_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    let rekey_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mut frame_total = 0.0f64;
    let mut worst = 0.0f64;
    for _ in 0..60 {
        let t = Instant::now();
        rt.frame(1.0 / 60.0);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        frame_total += ms;
        worst = worst.max(ms);
    }
    println!(
        "INITIAL_UNFOLDED_VIEW_MS={initial_unfolded_ms:.1} FOLD_ALL_2M_MS={fold_all_ms:.1} \
         FOLD_MARKS={folded} FOLD_ALL_COMMIT_MS={rekey_ms:.1} AVG_FRAME_MS={:.3} \
         WORST_FRAME_MS={worst:.3}",
        frame_total / 60.0
    );
    assert!(
        fold_all_ms < 250.0,
        "fold all blocked the UI: {fold_all_ms:.1} ms"
    );

    // Unfolding restores 2M rows: measure that commit too.
    let t = Instant::now();
    fold_index::unfold_all(key);
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    let unfold_commit_ms = t.elapsed().as_secs_f64() * 1000.0;
    fold_index::fold_all(key);
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    println!("FOLD_UNFOLD_COMMIT_MS={unfold_commit_ms:.1}");

    // Search jump into the folded region: arm a reveal, then measure the
    // rebuilt view and the scroll into view.
    let target = 1_500_000usize;
    let row = TextView::row_key("fold-all", target);
    let t = Instant::now();
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    let unfold_ms = t.elapsed().as_secs_f64() * 1000.0;
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_into_view(row.clone());
    });
    let t = Instant::now();
    for _ in 0..10 {
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        if find_center(&rt, &row).is_some() {
            break;
        }
    }
    let jump_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "FOLD_REVEAL_REBUILD_MS={unfold_ms:.1} SEARCH_JUMP_2M_MS={jump_ms:.1} \
         ROW_FOUND={} FOLDS_LEFT={}",
        find_center(&rt, &row).is_some(),
        fold_index::folded_count(key)
    );
    assert!(
        find_center(&rt, &row).is_some(),
        "search jump did not reveal a row inside the folded region"
    );
}

/// PNG evidence for a 100 MB document with one region folded in the middle of
/// the document, scrolled to it.
#[test]
#[ignore = "release evidence PNG: cargo test --release --test folding -- --ignored"]
fn release_png_100mb_folded_mid_document() {
    let doc = doc_2m_nested();
    let revision = 7_300_001;
    let text: Arc<str> = Arc::from(doc.as_str());
    line_index::prewarm_off_thread(Arc::clone(&text), revision)
        .join()
        .expect("line index prewarm");
    let index = fold_index::view_for(&doc, revision, 0, "png-probe")
        .index()
        .clone();
    // A region roughly in the middle of the document.
    let middle = index.region(index.len() / 2);
    println!(
        "FOLD_MID_REGION start={} end={} children={} depth={}",
        middle.start, middle.end, middle.child_count, middle.depth
    );
    assert!(fold_index::toggle(revision, middle.start));
    let state = doc_state(&doc, revision);
    let mut rt = fresh_runtime(state, view_png);
    // Reveal the folded opening line: its own fold stays, only ancestors open.
    let row = TextView::row_key("fold-png", middle.start as usize);
    rt.update(|state, _| state.fold_epoch = state.fold_epoch.wrapping_add(1));
    rt.frame(1.0 / 60.0);
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_into_view(row.clone());
    });
    for _ in 0..20 {
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        if find_center(&rt, &row).is_some() {
            break;
        }
    }
    assert!(
        find_center(&rt, &row).is_some(),
        "folded middle region was not scrolled into view"
    );
    let tree = render_to(&mut rt, "fold-100mb-mid-document.png");
    let text = tree.to_string();
    assert!(text.contains("{…}"), "folded placeholder missing: {text}");
    println!(
        "FOLD_100MB_PNG_BYTES={} BYTES={} LINES={}",
        std::fs::metadata(target_dir().join("fold-100mb-mid-document.png"))
            .map(|m| m.len())
            .unwrap_or(0),
        doc.len(),
        index.line_count()
    );
}
