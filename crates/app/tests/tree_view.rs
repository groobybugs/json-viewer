//! Tree explorer evidence: headless PNGs (small doc, api.json-shaped doc
//! collapsed/partially expanded/filter/sort, a 100 MB document scrolled
//! mid-tree) and release timings (index build for 7 MB and 100 MB,
//! expand-all strategy, frame time under deep expansion, filter on 100 MB,
//! index memory per node).
//!
//! Run the always-on evidence with:
//! `cargo test -p json-viewer --test tree_view -- --nocapture`
//!
//! Release performance evidence (the `#[ignore]`d tests):
//! `cargo test --release -p json-viewer --test tree_view -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use json_viewer::headless::{self, element_tree};
use json_viewer::search::SearchHit;
use json_viewer::state::{AppState, Msg, TabId};
use json_viewer::tree_index::{self, TreeJob};
use json_viewer::ui;
use json_viewer::runner;
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{Env, Runtime};
use serde_json::Value;

const SIZE: (f32, f32) = (1280.0, 800.0);
const SCALE: f32 = 1.0;

/// `fold_index` keeps one process-wide store (fold states, pending reveals
/// keyed by view base, hover, LRU) and `line_index` one process-wide snapshot
/// LRU. The app is single-threaded, but libtest runs these tests in parallel
/// inside one process: a concurrent app render can consume another test's
/// pending reveal or evict a warmed revision. Serialize every test that
/// renders the app.
static FOLD_STORE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    FOLD_STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The small document from the task's sketch: nested objects, an array,
/// scalars of every kind.
const SMALL: &str = r#"{
  "scripts": {
    "name": "simple-json-viewer",
    "build": "cargo build",
    "items": [12, "x", true, null]
  },
  "n": -4.5,
  "flag": true,
  "nothing": null
}"#;

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("headless");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless")
}

fn ensure_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create target dir");
    }
}

/// A fresh headless runtime over the real app view.
fn app_runtime(state: AppState) -> Runtime<AppState> {
    let mut rt = Runtime::with_text(
        state,
        ui::view,
        SIZE,
        Env::new(true, SCALE),
        headless::text_system(),
    );
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    rt
}

/// Draws `rt` to `path` (size × scale physical pixels) and returns the tree.
fn draw(rt: &mut Runtime<AppState>, gpu: &Gpu, path: &Path) -> Value {
    ensure_dir(path);
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
    let tree = element_tree(rt);
    assert!(
        std::fs::metadata(path).expect("PNG missing").len() > 100,
        "{} is empty",
        path.display()
    );
    tree
}

/// Renders straight to a PNG through the headless helper.
fn render_png(state: AppState, name: &str) -> Value {
    let path = target_dir().join(name);
    ensure_dir(&path);
    headless::render_and_dump(state, SIZE, SCALE, &path, &path.with_extension("json"))
        .unwrap_or_else(|error| panic!("render {name} failed: {error:#}"))
}

/// A state with `doc` in the active tab and the tree open (index built
/// synchronously: tests run without an app handle).
fn tree_state(doc: &str) -> AppState {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, doc.to_string());
    state.update(Msg::TreeOpen);
    assert!(
        state.tree.get(&id).is_some_and(|t| t.is_ready()),
        "sync build produced the index"
    );
    state
}

fn active_tab(state: &AppState) -> TabId {
    state.active
}

/// Expands the first `count` container rows of the active tab's tree.
fn expand_first_containers(state: &mut AppState, count: usize) {
    let id = active_tab(state);
    let nodes: Vec<u32> = {
        let tree = state.tree.get(&id).expect("tree state");
        let index = tree.index.as_ref().expect("index");
        tree.rows
            .iter()
            .filter(|row| index.is_container(row.node) && !row.expanded)
            .take(count)
            .map(|row| row.node)
            .collect()
    };
    for node in nodes {
        state.update(Msg::TreeExpand { tab: id, node });
    }
}

/// Selects the first visible *leaf* row whose value text contains `needle`
/// (containers' value slices are whole subtrees, which would match anything
/// inside them).
fn select_row_containing(state: &mut AppState, needle: &str) {
    let id = active_tab(state);
    let input = state.tab(id).expect("tab").input.clone();
    let tree = state.tree.get(&id).expect("tree state");
    let index = tree.index.as_ref().expect("index");
    let node = tree
        .rows
        .iter()
        .map(|row| row.node)
        .find(|&node| {
            !index.is_container(node) && index.value_text(&input, node).contains(needle)
        })
        .expect("a leaf row containing the needle");
    state.update(Msg::TreeSelect { tab: id, node });
}

/// The visible tree rows' labels, in display order (for order assertions).
/// Virtual rows carry the key the `key` fn returned (`tree-row-<node>`).
fn row_labels(tree: &Value) -> Vec<String> {
    tree["elements"]
        .as_array()
        .map(|elements| {
            elements
                .iter()
                .filter(|element| {
                    element["key"]
                        .as_str()
                        .is_some_and(|key| key.starts_with("tree-row-"))
                })
                .filter_map(|element| element["label"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A models.dev api.json-shaped document: `entries` top-level keys, deep
/// nesting, mixed scalar types, minified onto a single line. Indices are
/// unpadded on purpose: byte order (`alpha_10` < `alpha_2`) differs from
/// natural order, which is what the sorted view demonstrates.
fn api_shaped_doc(entries: usize) -> String {
    let names = ["alpha", "Bravo", "charlie", "Delta", "echo", "fox-trot", "golf", "Hotel"];
    let mut doc = String::with_capacity(entries * 400 + 32);
    doc.push('{');
    for i in 0..entries {
        if i > 0 {
            doc.push(',');
        }
        let name = names[i % names.len()];
        let verified = if i % 3 == 0 { "true" } else { "false" };
        let deprecated = if i % 5 == 0 { "true" } else { "false" };
        let timeout_ms = 2500 + i;
        doc.push_str(&format!(
            r#""{name}_{i}":{{
                "name":"Service {i} of {entries}",
                "count":{i},
                "verified":{verified},
                "notes":null,
                "meta":{{
                    "hosts":["api{i}.example.com","cdn{i}.example.com"],
                    "timeout_ms":{timeout_ms},
                    "openapi":"3.1.0"
                }},
                "categories":{{
                    "primary":{{
                        "endpoints":["/v1/list","/v1/get/{{id}}"],
                        "deprecated":{deprecated}
                    }}
                }}
            }}"#
        ));
    }
    doc.push('}');
    // Minified single line: collapse the structural whitespace.
    let mut minified = String::with_capacity(doc.len());
    let bytes = doc.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_string = !in_string;
        }
        if !in_string && (b == b'\n' || b == b' ' || b == b'\t') {
            i += 1;
            continue;
        }
        minified.push(b as char);
        i += 1;
    }
    minified
}

// ── PNG evidence ───────────────────────────────────────────────────────────

/// Small document, expanded two levels, with a selection whose path shows in
/// the footer.
#[test]
fn tree_png_small_doc_expanded() {
    let _guard = serial();
    let mut state = tree_state(SMALL);
    expand_first_containers(&mut state, 2);
    select_row_containing(&mut state, "cargo build");
    let tree = render_png(state, "tree-small-expanded.png");
    let labels = row_labels(&tree);
    assert!(
        labels.contains(&"\"scripts\": {3}".to_string()),
        "scripts row missing: {labels:?}"
    );
    assert!(
        labels.contains(&"\"items\": [4]".to_string()),
        "items row missing: {labels:?}"
    );
    assert!(
        labels.contains(&"object {4}".to_string()),
        "root row label missing: {labels:?}"
    );
    let rendered = tree.to_string();
    assert!(
        rendered.contains("scripts → build"),
        "selected path missing from the footer"
    );
    assert!(rendered.contains("Copy path") && rendered.contains("Copy value"));
}

/// api.json shape: 326 top-level keys on one minified line, collapsed.
#[test]
fn tree_png_api_shaped_collapsed() {
    let _guard = serial();
    let doc = api_shaped_doc(326);
    let state = tree_state(&doc);
    let tree = render_png(state, "tree-api-collapsed.png");
    let labels = row_labels(&tree);
    assert!(labels.len() >= 60, "only {} rows mounted", labels.len());
    assert_eq!(
        labels[0], "object {326}",
        "root row shows the top-level key count: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.starts_with("\"alpha_0\":")),
        "first key row missing: {labels:?}"
    );
}

/// Same document partially expanded, scrolled so the expansion is visible.
#[test]
fn tree_png_api_shaped_partially_expanded() {
    let _guard = serial();
    let doc = api_shaped_doc(326);
    let mut state = tree_state(&doc);
    expand_first_containers(&mut state, 3);
    select_row_containing(&mut state, "Service 1 of 326");
    let gpu = Gpu::headless();
    let mut rt = app_runtime(state);
    // Reveal the selected row through its virtual key.
    let node = {
        let tree = rt.state.tree.get(&rt.state.active).unwrap();
        tree.selected.as_ref().map(|sel| sel.node)
    };
    if let Some(node) = node {
        rt.update(|_, tx| {
            tx.instant();
            tx.scroll_into_view(format!("tree-row-{node}"));
        });
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);
    }
    let tree = draw(&mut rt, &gpu, &target_dir().join("tree-api-partial.png"));
    let labels = row_labels(&tree);
    assert!(
        labels.iter().any(|label| label.contains("Service 1 of 326")),
        "expanded member row missing: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.starts_with("\"Bravo_1\":")),
        "expanded parent row missing: {labels:?}"
    );
    let rendered = tree.to_string();
    assert!(
        rendered.contains("Bravo_1 → name"),
        "footer path missing: {rendered}"
    );
}

/// Filter active: only matching nodes and their ancestors, with the match
/// count in the toolbar — and the filter field showing the active query,
/// not its placeholder.
#[test]
fn tree_png_filter_active() {
    let _guard = serial();
    let doc = api_shaped_doc(326);
    let mut state = tree_state(&doc);
    state.update(Msg::TreeFilterChanged {
        tab: state.active,
        query: "Bravo_".into(),
    });
    let tree = state.tree.get(&state.active).unwrap();
    assert!(tree.filter_matched.unwrap_or(0) >= 2, "expected matches");
    let tree_json = render_png(state, "tree-api-filter.png");
    let rendered = tree_json.to_string();
    assert!(
        rendered.contains("match"),
        "match count label missing"
    );
    // The shared filter field displays the active tab's filter text.
    let field_text = tree_json["elements"]
        .as_array()
        .and_then(|elements| {
            elements
                .iter()
                .find(|element| element["key"] == "tree-filter")
                .and_then(|element| element["text"].as_str())
        })
        .expect("tree-filter element");
    assert_eq!(field_text, "Bravo_", "filter field shows {field_text:?}");
    // Every mounted key row is a Bravo_* member or its ancestors/children.
    assert!(rendered.contains("\"Bravo_"), "filtered rows missing");
    assert!(
        !rendered.contains("\"alpha_0\":"),
        "unmatched key still visible under the filter"
    );
}

/// Sort A→Z on: the same rows, in case-insensitive *natural* order
/// (`alpha_2` before `alpha_10`), view-only.
#[test]
fn tree_png_sorted() {
    let _guard = serial();
    let doc = api_shaped_doc(326);
    let mut state = tree_state(&doc);
    let before: Vec<u32> = state.tree[&state.active]
        .rows
        .iter()
        .map(|r| r.node)
        .collect();
    state.update(Msg::TreeSortToggled { tab: state.active });
    let after: Vec<u32> = state.tree[&state.active].rows.iter().map(|r| r.node).collect();
    assert_eq!(after.len(), before.len(), "sort must not add or drop rows");
    let tree = render_png(state, "tree-api-sorted.png");
    let labels = row_labels(&tree);
    // Object-member labels are `"key": value`; pull the keys out.
    let keys: Vec<String> = labels
        .iter()
        .filter_map(|label| {
            let rest = label.strip_prefix('"')?;
            let end = rest.find('"')?;
            Some(rest[..end].to_owned())
        })
        .collect();
    let mut sorted_window = keys.clone();
    sorted_window.sort_by(|a, b| tree_index::natural_cmp(a, b));
    assert!(
        !keys.is_empty() && keys == sorted_window,
        "mounted rows are not in natural A→Z order: {keys:?}"
    );
    // Byte order would put alpha_104 before alpha_8; natural order must not.
    let alpha_keys: Vec<&String> = keys.iter().filter(|k| k.starts_with("alpha_")).collect();
    if alpha_keys.len() >= 2 {
        let first = alpha_keys[0].trim_start_matches("alpha_");
        assert_eq!(
            first.parse::<u32>().ok(),
            Some(0),
            "natural order starts at the first key, not a byte-order pick: {keys:?}"
        );
    }
}

/// The tree mounts on a broken document with a parse alert, not a crash.
#[test]
fn tree_png_invalid_document() {
    let _guard = serial();
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "{\"broken\": tru".into());
    state.update(Msg::TreeOpen);
    let rendered = render_png(state, "tree-invalid.png").to_string();
    assert!(
        rendered.contains("Cannot show tree"),
        "parse alert missing: {rendered}"
    );
}

/// Switching between Code and Tree keeps both views alive in one frame set.
#[test]
fn tree_png_switches_with_code_view() {
    let _guard = serial();
    let mut state = tree_state(SMALL);
    let id = state.active;
    // Tree → Output → Tree again.
    state.update(Msg::SwitchView(json_viewer::state::TabView::Output));
    assert!(!state.tree[&id].open);
    let code = render_png(state.clone(), "tree-code-output.png").to_string();
    assert!(code.contains("output-text"), "code output view missing");
    state.update(Msg::TreeOpen);
    assert!(state.tree[&id].open);
    let tree = render_png(state, "tree-after-switch-back.png").to_string();
    assert!(tree.contains("tree-toolbar"), "tree did not come back");
}

/// Search-result click while the tree is open: the hit's node is revealed
/// and selected, and the tree stays open (no jump to the Output view).
#[test]
fn tree_search_hit_click_reveals_and_selects() {
    let _guard = serial();
    let mut state = tree_state(SMALL);
    let id = state.active;
    state.apply_search_results(
        id,
        vec![SearchHit {
            path: "scripts → build".into(),
            key: Some("build".into()),
            value_preview: r#""cargo build""#.into(),
            line: Some(5),
        }],
    );
    let mut rt = app_runtime(state);

    // Click the search result through the real view.
    let (x, y) = rt
        .find("search-hit-0")
        .expect("search result is on screen");
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);

    // Pump the runner until the reveal lands (expand → select → scroll).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut revealed = false;
    while !revealed {
        rt.update_if(|state, tx| runner::poll(state, 0.0, tx));
        rt.frame(1.0 / 60.0);
        let selected = rt
            .state
            .tree
            .get(&rt.state.active)
            .and_then(|tree| tree.selected.clone());
        revealed = selected.as_ref().is_some_and(|sel| sel.path == "scripts → build");
        assert!(Instant::now() < deadline, "timed out waiting for the reveal");
    }
    rt.settle_all();
    rt.frame(0.0);

    // The tab stayed in tree mode with the node selected and revealed.
    assert!(rt.state.tree[&rt.state.active].open, "left tree mode");
    let node = rt
        .state
        .tree
        .get(&rt.state.active)
        .and_then(|tree| tree.selected.as_ref().map(|sel| sel.node))
        .expect("selection");
    let rendered = element_tree(&rt).to_string();
    assert!(
        rendered.contains(&format!("tree-row-{node}")),
        "the hit's row is not mounted after the reveal"
    );
    assert!(
        rendered.contains("scripts → build"),
        "footer path missing after the reveal"
    );

    // PNG evidence.
    let gpu = Gpu::headless();
    draw(&mut rt, &gpu, &target_dir().join("tree-hit-reveal.png"));
}

// ── Big-document evidence (release, --ignored) ────────────────────────────

/// 100 MB of valid JSON, one minified line per record inside a root array
/// (`models.dev`-class: many records, deep nesting).
fn hundred_mb_doc() -> String {
    let mut doc = String::with_capacity(100 * 1024 * 1024 + 64);
    doc.push_str("{\"records\":[");
    let record = r#"{"id":IDX,"unicode":"日本語テスト 🚀 café","numbers":{"float_one":1.0,"sci":1e5,"neg":-42.5,"score":SCOPE},"flags":[true,false,null],"nested":{"level1":{"level2":{"tags":["alpha","beta","omega"]}}},"payload":"PAYLOAD"}"#;
    let payload = "x".repeat(180);
    let mut i = 0usize;
    while doc.len() < 100 * 1024 * 1024 {
        if i > 0 {
            doc.push(',');
        }
        let chunk = record
            .replace("IDX", &i.to_string())
            .replace("SCOPE", &format!("{:.2}", i as f64 / 100.0))
            .replace("PAYLOAD", &payload);
        doc.push_str(&chunk);
        i += 1;
    }
    doc.push_str("]}");
    doc
}

/// 100 MB document: index build + expand-all + deep scroll + filter timings,
/// and the scrolled-mid-tree PNG.
#[test]
#[ignore]
fn tree_100mb_perf_evidence() {
    let _guard = serial();
    let gpu = Gpu::headless();
    let cancel = AtomicBool::new(false);

    // ── Build the index off the UI thread (as the app would) ────────────
    let doc = hundred_mb_doc();
    println!("DOC_BYTES={}", doc.len());
    let built = Instant::now();
    let index = tree_index::TreeIndex::build(&doc).expect("100 MB parse");
    let build_ms = built.elapsed().as_secs_f64() * 1000.0;
    let nodes = index.node_count();
    println!(
        "TREE_INDEX_BUILD_100MB_MS={build_ms:.0} NODES={nodes} INDEX_BYTES={} BYTES_PER_NODE={}",
        nodes * tree_index::TreeIndex::BYTES_PER_NODE,
        tree_index::TreeIndex::BYTES_PER_NODE
    );
    assert!(nodes > 1_000_000, "expected millions of nodes, got {nodes}");

    // ── Expand-all strategy: off-thread job, cancelable ─────────────────
    let expanded = Instant::now();
    let outcome = tree_index::run_job(&index, &doc, TreeJob::ExpandAll, false, &cancel)
        .expect("expand-all ran to completion");
    let expand_ms = expanded.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TREE_EXPAND_ALL_100MB_MS={expand_ms:.0} ROWS={} ROWS_BYTES={} BYTES_PER_ROW={}",
        outcome.rows.len(),
        outcome.rows.len() * std::mem::size_of::<tree_index::TreeRow>(),
        std::mem::size_of::<tree_index::TreeRow>()
    );
    assert_eq!(outcome.rows.len(), nodes, "expand-all shows every node");

    // ── Filter on the full 100 MB ───────────────────────────────────────
    let filtered = Instant::now();
    let outcome = tree_index::run_job(
        &index,
        &doc,
        TreeJob::Filter("日本語テスト".into()),
        false,
        &cancel,
    )
    .expect("filter is not cancelled here");
    let (rows, matched) = (outcome.rows, outcome.matched.unwrap_or(0));
    let filter_ms = filtered.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TREE_FILTER_100MB_MS={filter_ms:.0} MATCHED={matched} FILTER_ROWS={}",
        rows.len()
    );
    assert!(matched > 100_000, "expected the unicode needle everywhere");

    // ── Frame time with deep expansion: real view, virtual rows ─────────
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, doc);
    state.update(Msg::TreeOpen);
    state.update(Msg::TreeExpandAll { tab: id });
    let tree = state.tree.get(&id).unwrap();
    let total_rows = tree.rows.len();
    println!("UI_ROWS_AFTER_EXPAND_ALL={total_rows}");
    let mid_node = tree.rows[total_rows / 2].node;

    let mut rt = app_runtime(state);
    rt.update(|_, tx| {
        tx.instant();
        tx.scroll_into_view(format!("tree-row-{mid_node}"));
    });
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    // The reveal must have mounted the target row (checked before the wheel
    // loop scrolls the viewport on).
    let after_reveal = element_tree(&rt).to_string();
    assert!(
        after_reveal.contains(&format!("tree-row-{mid_node}")),
        "the revealed row was not mounted after scroll_into_view"
    );

    let mut frame_ms = Vec::with_capacity(60);
    for _ in 0..60 {
        let start = Instant::now();
        rt.wheel_xy(640.0, 400.0, 0.0, -120.0);
        rt.frame(1.0 / 60.0);
        frame_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let avg = frame_ms.iter().sum::<f64>() / frame_ms.len() as f64;
    let p95 = frame_ms[(frame_ms.len() as f64 * 0.95) as usize];
    let max = frame_ms.last().copied().unwrap_or_default();
    println!(
        "TREE_FRAME_DEEP_EXPANSION_AVG_MS={avg:.2} P95_MS={p95:.2} MAX_MS={max:.2}"
    );
    assert!(
        p95 < 100.0,
        "frames while scrolling a fully expanded 100 MB tree exceed the budget: p95 {p95:.2} ms"
    );

    // The mid-tree PNG: still deep inside the expanded document.
    let tree_json = draw(&mut rt, &gpu, &target_dir().join("tree-100mb-mid.png"));
    let rendered = tree_json.to_string();
    assert!(
        rendered.contains("tree-row-"),
        "no tree rows are mounted mid-document"
    );
    assert!(
        rendered.contains("tree-toolbar"),
        "tree toolbar not visible"
    );
}

/// 6.9 MB single-line api.json-shaped document: index + expand + filter
/// timings (the smaller of the task's two size classes).
#[test]
#[ignore]
fn tree_7mb_perf_evidence() {
    let _guard = serial();
    let doc = api_shaped_doc(19_000);
    let mb = doc.len() as f64 / (1024.0 * 1024.0);
    println!("API_DOC_BYTES={} ({mb:.2} MiB single line)", doc.len());

    let built = Instant::now();
    let index = tree_index::TreeIndex::build(&doc).expect("parse");
    let build_ms = built.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TREE_INDEX_BUILD_7MB_MS={build_ms:.1} NODES={}",
        index.node_count()
    );

    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let outcome = tree_index::run_job(&index, &doc, TreeJob::ExpandAll, false, &cancel)
        .expect("expand-all");
    println!(
        "TREE_EXPAND_ALL_7MB_MS={:.1} ROWS={}",
        started.elapsed().as_secs_f64() * 1000.0,
        outcome.rows.len()
    );

    let started = Instant::now();
    let outcome = tree_index::run_job(&index, &doc, TreeJob::Filter("Bravo_".into()), false, &cancel)
        .expect("filter");
    println!(
        "TREE_FILTER_7MB_MS={:.1} MATCHED={}",
        started.elapsed().as_secs_f64() * 1000.0,
        outcome.matched.unwrap_or(0)
    );

    // The UI path end to end: open the tree on this document, expand all.
    // 323k nodes are over the sync limit, so the reducer routes through the
    // job machinery (which runs inline without an app handle).
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, doc);
    state.update(Msg::TreeOpen);
    assert!(state.tree[&id].is_ready());
    state.update(Msg::TreeExpandAll { tab: id });
    assert!(
        state.tree[&id].rows.len() > 300_000,
        "expand-all rows missing: {}",
        state.tree[&id].rows.len()
    );
    assert!(!state.tree[&id].job_pending);

    // A fully expanded frame must stay cheap: only the mounted window builds.
    let gpu = Gpu::headless();
    let mut rt = app_runtime(state);
    let start = Instant::now();
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    println!(
        "TREE_FRAME_FULLY_EXPANDED_7MB_MS={:.1}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let tree = draw(&mut rt, &gpu, &target_dir().join("tree-7mb-expanded.png"));
    assert!(tree.to_string().contains("tree-row-"));
}
