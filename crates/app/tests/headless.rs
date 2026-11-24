//! Headless QA tests: drive the app with Rune scripted input, pump
//! `runner::poll`, render PNGs to `target/headless/qa-*.png`, and assert on
//! the element tree and application state.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use json_viewer::json_ops::{self, JsonAction};
use json_viewer::search::{self, SearchOptions};
use json_viewer::state::{AppState, Msg, TabView};
use json_viewer::{headless, runner};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::Runtime;
use serde_json::Value;

/// `fold_index` keeps one process-wide store, and `TextView::row_key` records a
/// pending reveal keyed by view base. Every test here drives the real
/// `ui::view`, whose Output base is `output-text`, so an unguarded test's
/// frame can consume another test's reveal. No folds are set yet, but the
/// store is shared: serialize the tests that render the app.
static FOLD_STORE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    FOLD_STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn find_center(rt: &Runtime<AppState>, key_or_label: &str) -> (f32, f32) {
    if let Some(center) = rt.find(key_or_label) {
        return center;
    }
    for el in rt.elements() {
        if el.exiting || el.visible[2] <= 0.0 || el.visible[3] <= 0.0 {
            continue;
        }
        let matches = el.key.as_deref() == Some(key_or_label)
            || el.label.as_deref() == Some(key_or_label)
            || el.text.as_deref() == Some(key_or_label);
        if matches {
            return (
                el.rect[0] + el.rect[2] * 0.5,
                el.rect[1] + el.rect[3] * 0.5,
            );
        }
    }
    panic!("element {key_or_label:?} not found or not visible in runtime");
}

fn click(rt: &mut Runtime<AppState>, key_or_label: &str) {
    let (x, y) = find_center(rt, key_or_label);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
}

fn pump_until(rt: &mut Runtime<AppState>, label: &str, mut pred: impl FnMut(&AppState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.frame(1.0 / 60.0);
        let mut done = false;
        rt.update_if(|state, _| {
            done = pred(state);
            false
        });
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for condition: {label}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    rt.settle_all();
    rt.frame(0.0);
}

fn render_qa_png(rt: &mut Runtime<AppState>, name: &str) -> Value {
    let (w, h) = headless::DEFAULT_SIZE;
    let width = w.round().max(1.0) as u32;
    let height = h.round().max(1.0) as u32;

    rt.settle_all();
    rt.frame(0.0);

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
    let path = target_dir().join("headless").join(format!("qa-{name}.png"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create headless output dir");
    }
    write_png(&path, width, height, &pixels).expect("write QA PNG");
    let meta = std::fs::metadata(&path).expect("QA PNG missing");
    assert!(meta.len() > 0, "QA PNG {} is empty", path.display());

    headless::element_tree(rt)
}

#[test]
fn scripted_type_format_and_switch_to_output() {
    let _guard = serial();
    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);
    let initial_tree = render_qa_png(&mut rt, "initial");
    assert!(initial_tree.to_string().contains("JSON Viewer"));

    click(&mut rt, "editor-1");
    rt.paste(r#"{"z":1,"a":{"needle":"hello"}}"#);
    pump_until(&mut rt, "input updated", |s| {
        s.active_tab().is_some_and(|t| t.input.contains("needle"))
    });

    click(&mut rt, "Format");
    pump_until(&mut rt, "format finished", |s| {
        s.active_tab()
            .is_some_and(|t| !t.is_processing && !t.output.is_empty())
    });

    click(&mut rt, "Output");
    pump_until(&mut rt, "switched to output", |s| {
        s.active_tab().is_some_and(|t| t.view == TabView::Output)
    });

    let tree = render_qa_png(&mut rt, "formatted-output");
    let tree_json = tree.to_string();
    assert!(
        tree_json.contains("editor-1-row-0"),
        "expected the editable document in Output"
    );
    assert!(
        tree_json.contains("needle"),
        "expected formatted output content in tree"
    );

    // Output is editable: click into the document, then paste.
    click(&mut rt, "editor-1");
    rt.paste("\"pasted\"");
    pump_until(&mut rt, "output edit applied", |s| {
        s.active_tab().is_some_and(|t| {
            t.view == TabView::Output && t.input.contains("\"pasted\"") && t.input.contains("needle")
        })
    });
}

#[test]
fn scripted_new_tab_and_tab_switching() {
    let _guard = serial();
    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);

    click(&mut rt, "new-tab");
    pump_until(&mut rt, "second tab created", |s| {
        s.tabs.len() == 2 && s.active == 2
    });

    click(&mut rt, "editor-2");
    rt.paste(r#"{"second":true}"#);
    pump_until(&mut rt, "second tab input updated", |s| {
        s.tab(2).is_some_and(|t| t.input.contains("second"))
    });

    click(&mut rt, "tab-1");
    pump_until(&mut rt, "switched back to tab 1", |s| s.active == 1);

    let tree = render_qa_png(&mut rt, "tabs-switched");
    let tree_json = tree.to_string();
    assert!(tree_json.contains("JSON 1"));
    assert!(tree_json.contains("JSON 2"));
}

#[test]
fn scripted_search_displays_results() {
    let _guard = serial();
    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);

    click(&mut rt, "editor-1");
    rt.paste(r#"{"user":{"name":"Alice","role":"admin"}}"#);
    pump_until(&mut rt, "input updated for search", |s| {
        s.active_tab().is_some_and(|t| t.input.contains("Alice"))
    });

    click(&mut rt, "search");
    rt.paste("Alice");
    pump_until(&mut rt, "search results populated", |s| {
        s.active_tab()
            .is_some_and(|t| !t.is_processing && !t.search_results.is_empty())
    });

    let tree = render_qa_png(&mut rt, "search-results");
    let tree_json = tree.to_string();
    assert!(
        tree_json.contains("Search Results (1)"),
        "expected Search Results (1) in tree"
    );
    assert!(
        tree_json.contains("user → name"),
        "expected hit path user → name in tree"
    );
}

#[test]
fn scripted_clearing_search_clears_results() {
    let _guard = serial();
    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);

    click(&mut rt, "editor-1");
    rt.paste(r#"{"user":{"name":"Alice","role":"admin"}}"#);
    pump_until(&mut rt, "input ready for search", |s| {
        s.active_tab().is_some_and(|t| t.input.contains("Alice"))
    });

    click(&mut rt, "search");
    rt.paste("Alice");
    pump_until(&mut rt, "results before clear", |s| {
        s.active_tab()
            .is_some_and(|t| !t.is_processing && !t.search_results.is_empty())
    });

    click(&mut rt, "clear-search");
    pump_until(&mut rt, "results cleared", |s| {
        s.search_edit.text.is_empty()
            && s.active_tab().is_some_and(|t| {
                t.search_query.is_empty() && t.search_results.is_empty()
            })
    });

    let tree = render_qa_png(&mut rt, "search-cleared");
    assert!(
        !tree.to_string().contains("Search Results"),
        "clearing the query must clear the results panel"
    );
}

/// A ~6 MiB, single-line, valid JSON document with non-ASCII text. Generating
/// it keeps the regression independent of any downloaded file.
fn large_single_line_fixture() -> String {
    let mut out = String::with_capacity(7 * 1024 * 1024);
    out.push_str("{\"models\":[");
    let mut index = 0usize;
    while out.len() < 6 * 1024 * 1024 {
        if index > 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"id\":\"model-{index:05}\",\"name\":\"Mødel {index:05} 日本語\",\
             \"description\":\"Ærlig beskrivelse — 中文 {index:05}\",\
             \"tags\":[\"alpha\",\"beta\"]}}"
        )
        .expect("write fixture");
        index += 1;
    }
    out.push_str("]}");
    assert!(!out.contains('\n'), "fixture must be a single line");
    out
}

#[test]
fn scripted_large_single_line_format_from_open_and_paste() {
    let _guard = serial();
    let content = large_single_line_fixture();
    assert!(content.len() > 6 * 1024 * 1024);
    assert!(!content.is_ascii());

    for (label, paste) in [("open", false), ("paste", true)] {
        let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);
        if paste {
            click(&mut rt, "editor-1");
            rt.paste(&content);
        } else {
            rt.update(|state, _| {
                state.dispatch(Msg::FileLoaded {
                    path: PathBuf::from("/tmp/models.json"),
                    content: content.clone(),
                })
            });
        }
        pump_until(&mut rt, &format!("{label}: large input loaded"), |state| {
            state
                .active_tab()
                .is_some_and(|tab| tab.input.len() == content.len())
        });

        // The large-file Input view is the read-only fallback; Format must
        // still be enabled and must publish output and switch to Output.
        click(&mut rt, "format");
        pump_until(&mut rt, &format!("{label}: format finished"), |state| {
            state
                .active_tab()
                .is_some_and(|tab| !tab.is_processing && tab.view == TabView::Output)
        });

        let tab = rt.state.active_tab().expect("active tab");
        assert!(!tab.output.is_empty(), "{label}: output is empty");
        assert!(tab.error.is_none(), "{label}: error {:?}", tab.error);
        assert_eq!(tab.view, TabView::Output, "{label}: view did not switch");
        assert!(
            tab.output.lines().count() > 100_000,
            "{label}: formatted output has only {} lines",
            tab.output.lines().count()
        );

        let tree = render_qa_png(&mut rt, &format!("large-format-{label}"));
        let tree_json = tree.to_string();
        assert!(
            tree_json.contains("editor-") && tree_json.contains("-row-"),
            "{label}: output view did not mount"
        );
        // Output is the live editor; it reveals the caret, which keeps its
        // relative place after Format, so any formatted row may be on screen.
        assert!(
            tree_json.contains("model-"),
            "{label}: formatted output content missing from the tree"
        );
    }
}

#[test]
fn scripted_invalid_json_shows_error() {
    let _guard = serial();
    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);

    click(&mut rt, "editor-1");
    rt.paste("{bad json");
    pump_until(&mut rt, "invalid input updated", |s| {
        s.active_tab().is_some_and(|t| t.input == "{bad json")
    });

    click(&mut rt, "Format");
    pump_until(&mut rt, "format error surfaced", |s| {
        s.active_tab()
            .is_some_and(|t| !t.is_processing && t.error.is_some())
    });

    let tree = render_qa_png(&mut rt, "invalid-json");
    assert!(
        tree.to_string().contains("Invalid JSON:"),
        "expected Invalid JSON error banner in tree"
    );
}

#[test]
fn scripted_file_drop_loads_tab() {
    let _guard = serial();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("dropped.json");
    std::fs::write(&path, r#"{"dropped":true}"#).expect("write dropped fixture");

    let mut rt = headless::runtime(AppState::default(), headless::DEFAULT_SIZE, 1.0);
    rt.frame(0.0);
    let (x, y) = find_center(&rt, "json-drop-zone");
    rt.drop_files(vec![path], Some((x, y)));
    pump_until(&mut rt, "dropped file loaded", |s| {
        s.active_tab()
            .is_some_and(|t| t.input.contains("\"dropped\""))
    });

    let tree = render_qa_png(&mut rt, "file-drop");
    assert!(
        tree.to_string().contains("dropped"),
        "expected dropped JSON in the editor"
    );
}

#[test]
fn scripted_search_hit_scrolls_output_to_line() {
    let _guard = serial();
    let mut input = String::from("{\"items\":[");
    for index in 0..120 {
        if index > 0 {
            input.push(',');
        }
        input.push_str(&format!("{{\"id\":\"item-{index:03}\"}}"));
    }
    input.push_str("]}");
    let formatted = json_ops::process(&input, JsonAction::Format).expect("format fixture");

    let cancel = AtomicBool::new(false);
    let hits = search::search(
        &formatted,
        "item-100",
        SearchOptions::default(),
        &cancel,
    );
    let hit = hits.first().expect("fixture has one hit").clone();
    let line = hit.line.expect("value hit has a line");
    assert!(line > 100, "hit should be far down the document");
    assert!(
        formatted
            .lines()
            .nth(line - 1)
            .is_some_and(|text| text.contains("item-100")),
        "hit.line must be a 1-based line in the formatted document"
    );

    let mut state = AppState::default();
    state.tabs[0].input = input;
    state.tabs[0].search_query = "item-100".into();
    let tab = state.active;
    state.apply_json_result(tab, Ok(formatted));
    state.update(Msg::SearchDone {
        tab,
        hits: vec![hit],
    });

    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    rt.frame(0.0);
    click(&mut rt, "search-hit-0");
    pump_until(&mut rt, "switched to output", |s| {
        s.active_tab().is_some_and(|t| t.view == TabView::Output)
    });

    let row = format!("editor-{tab}-row-{}", line - 1);
    let mut mounted = false;
    for _ in 0..30 {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.settle_all();
        rt.frame(1.0 / 60.0);
        if rt.find(&row).is_some() {
            mounted = true;
            break;
        }
    }
    assert!(
        mounted,
        "search hit line {line} should reveal row {row} in the output viewport"
    );

    let tree = render_qa_png(&mut rt, "search-jump");
    assert!(
        tree.to_string().contains("item-100"),
        "expected the target line content after the jump"
    );
}
