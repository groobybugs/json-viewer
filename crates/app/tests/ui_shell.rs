//! UI-shell evidence: real headless PNGs, element assertions and keyboard input.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use json_viewer::headless;
use json_viewer::json_ops::JsonError;
use json_viewer::search::SearchHit;
use json_viewer::state::{AppState, Msg, TabView};
use rune::{Key, KeyEvent, Mods, Runtime};
use serde_json::Value;

const SIZE: (f32, f32) = (1280.0, 800.0);

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
const SAMPLE: &str = r#"{
  "name": "Ada Lovelace",
  "active": true,
  "skills": ["math", "programming"],
  "address": { "city": "London", "postcode": "N1" }
}"#;
const PRETTY: &str = r#"{
  "name": "Ada Lovelace",
  "active": true,
  "skills": [
    "math",
    "programming"
  ],
  "address": {
    "city": "London",
    "postcode": "N1"
  }
}"#;

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn render(state: AppState, name: &str) -> Value {
    render_at(state, SIZE, name)
}

fn render_at(state: AppState, size: (f32, f32), name: &str) -> Value {
    let directory = target_dir().join("headless/ui-shell");
    std::fs::create_dir_all(&directory).expect("create evidence directory");
    let png = directory.join(format!("{name}.png"));
    let tree_path = directory.join(format!("{name}-tree.json"));
    let tree = headless::render_and_dump(state, size, 1.0, &png, &tree_path)
        .unwrap_or_else(|error| panic!("render {name} failed: {error:#}"));
    assert!(png.metadata().expect("PNG metadata").len() > 0);
    assert!(tree_path.exists());
    tree
}

fn render_hovered(state: AppState, hover_key: &str, name: &str) -> Value {
    let mut runtime = headless::runtime(state, SIZE, 1.0);
    let (x, y) = runtime.find(hover_key).expect("hover target exists");
    runtime.pointer_move(x, y);
    runtime.frame(0.0);
    runtime.settle_all();
    runtime.frame(0.0);
    render_runtime(runtime, name)
}

fn render_runtime(mut runtime: Runtime<AppState>, name: &str) -> Value {
    use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};

    let directory = target_dir().join("headless/ui-shell");
    std::fs::create_dir_all(&directory).expect("create evidence directory");
    let png = directory.join(format!("{name}.png"));
    let tree_path = directory.join(format!("{name}-tree.json"));
    let (width, height) = (SIZE.0 as u32, SIZE.1 as u32);
    let gpu = Gpu::headless();
    let format = OFFSCREEN_FORMAT.remove_srgb_suffix();
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(true);
    let target = Offscreen::with_format(&gpu.device, width, height, format);
    renderer.prepare(
        &gpu.device,
        &gpu.queue,
        &mut runtime.text.atlas,
        &runtime.instances,
        (width, height),
    );
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&mut encoder, &target.view, runtime.clear_color());
    target.copy(&mut encoder);
    renderer.submit(&gpu.queue, encoder);
    let pixels = target.read(&gpu.device);
    write_png(&png, width, height, &pixels).expect("write hover PNG");
    let tree = headless::element_tree(&runtime);
    headless::write_tree(&tree, &tree_path).expect("write hover element tree");
    tree
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if value["key"] == key {
        return Some(value);
    }
    value["children"]
        .as_array()?
        .iter()
        .find_map(|child| find_key(child, key))
}

fn a11y<'a>(tree: &'a Value, key: &str) -> &'a Value {
    find_key(&tree["accessibility"], key)
        .unwrap_or_else(|| panic!("missing accessibility element {key}: {tree}"))
}

fn states(value: &Value) -> Vec<&str> {
    value["states"].as_array().map_or_else(Vec::new, |states| {
        states.iter().filter_map(Value::as_str).collect()
    })
}

fn assert_disabled(tree: &Value, key: &str, disabled: bool) {
    assert_eq!(
        states(a11y(tree, key)).contains(&"disabled"),
        disabled,
        "unexpected disabled state for {key}"
    );
}

fn element<'a>(tree: &'a Value, key: &str) -> &'a Value {
    tree["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|element| element["key"] == key))
        .unwrap_or_else(|| panic!("missing element {key}: {tree}"))
}

fn has_element(tree: &Value, key: &str) -> bool {
    tree["elements"]
        .as_array()
        .is_some_and(|elements| elements.iter().any(|element| element["key"] == key))
}

fn tab_count(tree: &Value) -> usize {
    tree["elements"].as_array().map_or(0, |elements| {
        elements
            .iter()
            .filter(|element| {
                element["key"]
                    .as_str()
                    .and_then(|key| key.strip_prefix("tab-"))
                    .is_some_and(|id| id.parse::<u64>().is_ok())
            })
            .count()
    })
}

fn document_state() -> AppState {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, SAMPLE.into());
    state
}

fn output_state() -> AppState {
    let mut state = document_state();
    let id = state.active;
    assert!(state.apply_json_result(id, Ok(PRETTY.into())));
    state.set_view(id, TabView::Output);
    state
}

fn hits() -> Vec<SearchHit> {
    vec![
        SearchHit {
            path: "name".into(),
            key: Some("name".into()),
            value_preview: "Ada Lovelace".into(),
            line: Some(2),
        },
        SearchHit {
            path: "address → city".into(),
            key: Some("city".into()),
            value_preview: "London".into(),
            line: Some(9),
        },
    ]
}

fn search_state() -> AppState {
    let mut state = output_state();
    let id = state.active;
    state.search_edit.replace_text("name");
    let _ = state.update(Msg::SearchChanged("name".into()));
    state.apply_search_results(id, hits());
    state.set_view(id, TabView::Input);
    state
}

fn assert_toolbar_present(tree: &Value) {
    for key in ["format", "minify", "copy", "export", "open", "clear"] {
        assert_eq!(a11y(tree, key)["role"], "button", "missing {key}");
    }
    assert_eq!(a11y(tree, "input-view")["name"], "Input");
    assert_eq!(a11y(tree, "output-view")["name"], "Output");
}

#[test]
fn six_shell_states_render_with_controls_and_layout() {
    let _guard = serial();
    let empty = render(AppState::default(), "01-empty-tab");
    assert_toolbar_present(&empty);
    assert_eq!(tab_count(&empty), 1);
    assert!(find_key(&empty["accessibility"], "close-1").is_none());
    assert_eq!(a11y(&empty, "add-tab")["name"], "New JSON tab");
    for key in ["format", "minify", "copy", "export", "clear"] {
        assert_disabled(&empty, key, true);
    }
    assert_disabled(&empty, "open", false);
    let editor = &element(&empty, "document-scroll-1")["rect"];
    assert!(
        editor[3].as_f64().unwrap() > 400.0,
        "document must fill remaining height: {editor}"
    );

    let mut three = AppState::default();
    let first = three.active;
    three.update_input(first, SAMPLE.into());
    let second = three.add_tab();
    three.update_input(second, r#"{"second": 2}"#.into());
    let third = three.add_tab();
    three.update_input(third, r#"[1, 2, 3]"#.into());
    three.set_active(second);
    let three = render(three, "02-three-tabs-input");
    assert_toolbar_present(&three);
    assert_eq!(tab_count(&three), 3);
    for id in [first, second, third] {
        assert_eq!(a11y(&three, &format!("tab-{id}"))["role"], "tab");
        assert!(find_key(&three["accessibility"], &format!("close-{id}")).is_some());
    }
    assert!(states(a11y(&three, "tab-2")).contains(&"selected"));
    assert!(!states(a11y(&three, "tab-1")).contains(&"selected"));
    assert_eq!(a11y(&three, "add-tab")["name"], "New JSON tab");
    assert_disabled(&three, "format", false);
    assert_disabled(&three, "minify", false);

    let output = render(output_state(), "03-output-view");
    assert!(states(a11y(&output, "output-view")).contains(&"selected"));
    assert!(output.to_string().contains("Ada Lovelace"));
    assert_disabled(&output, "copy", false);
    assert_disabled(&output, "export", false);

    let mut error_state = document_state();
    let id = error_state.active;
    error_state.apply_json_result(id, Err(JsonError::new("expected value", 3, 14)));
    let error = render(error_state, "04-error-alert");
    assert_eq!(a11y(&error, "error-alert")["role"], "alert");
    assert!(element(&error, "error-message")["text"]
        .as_str()
        .is_some_and(|text| text == "Invalid JSON: expected value"));
    assert_eq!(
        element(&error, "error-location")["text"],
        "Line 3, column 14"
    );

    let mut processing_state = output_state();
    let id = processing_state.active;
    processing_state.set_view(id, TabView::Input);
    processing_state.set_processing(id, true);
    let processing = render(processing_state, "05-processing-state");
    assert_eq!(
        a11y(&processing, "processing-spinner")["role"],
        "progress-indicator"
    );
    assert!(processing.to_string().contains("Processing…"));
    for key in ["format", "minify", "copy", "export", "clear"] {
        assert_disabled(&processing, key, true);
    }
    assert_disabled(&processing, "open", false);

    let search = render(search_state(), "06-search-results");
    assert!(element(&search, "search-results-heading")["text"]
        .as_str()
        .is_some_and(|text| text.contains("Search Results (2)")));
    assert!(element(&search, "search-count")["text"]
        .as_str()
        .is_some_and(|text| text.contains("2 matches")));
    assert_eq!(a11y(&search, "clear-results")["name"], "Clear");
    assert!(!states(a11y(&search, "clear-results")).contains(&"disabled"));
    assert_eq!(a11y(&search, "search-hit-0")["role"], "button");
    assert_eq!(a11y(&search, "search-hit-1")["role"], "button");
    assert!(search.to_string().contains("address → city"));
}

#[test]
fn toolbar_groups_fit_wide_and_narrow_windows() {
    let _guard = serial();
    let actions = [
        "validate", "format", "minify", "copy", "export", "open", "clear",
    ];
    for (size, name) in [
        ((1280.0, 800.0), "toolbar-output-1280"),
        ((1024.0, 700.0), "toolbar-output-1024"),
    ] {
        let tree = render_at(output_state(), size, name);
        for key in ["input-view", "output-view", "tree-view"]
            .into_iter()
            .chain(actions)
            .chain([
                "fold-all",
                "unfold-all",
                "fold-level-1",
                "fold-level-2",
                "fold-level-3",
            ])
        {
            let rect = &element(&tree, key)["rect"];
            let x = rect[0].as_f64().unwrap();
            let y = rect[1].as_f64().unwrap();
            let right = x + rect[2].as_f64().unwrap();
            let bottom = y + rect[3].as_f64().unwrap();
            assert!(x >= 0.0 && y >= 0.0, "{name}: {key} starts outside: {rect}");
            assert!(
                right <= size.0 as f64 && bottom <= size.1 as f64,
                "{name}: {key} clipped: {rect}"
            );
        }
        for pair in actions.windows(2) {
            let left = &element(&tree, pair[0])["rect"];
            let right = &element(&tree, pair[1])["rect"];
            assert!(
                left[0].as_f64().unwrap() + left[2].as_f64().unwrap() <= right[0].as_f64().unwrap(),
                "{name}: {} overlaps {}",
                pair[0],
                pair[1]
            );
        }
        let switch = &element(&tree, "tree-view")["rect"];
        let first_action = &element(&tree, "validate")["rect"];
        assert!(
            switch[0].as_f64().unwrap() + switch[2].as_f64().unwrap()
                <= first_action[0].as_f64().unwrap(),
            "{name}: view switch and actions overlap"
        );
        assert!(!has_element(&tree, "tree-toolbar"));
    }

    let input = render_at(document_state(), (1024.0, 700.0), "toolbar-input-1024");
    assert!(!has_element(&input, "fold-toolbar"));
    assert!(!has_element(&input, "tree-toolbar"));

    let mut tree_state = output_state();
    tree_state.update(Msg::TreeOpen);
    let tree = render_at(tree_state, (1024.0, 700.0), "toolbar-tree-1024");
    assert!(!has_element(&tree, "fold-toolbar"));
    assert!(has_element(&tree, "tree-toolbar"));
    assert!(states(a11y(&tree, "tree-view")).contains(&"selected"));
    assert!(!states(a11y(&tree, "output-view")).contains(&"selected"));
}

#[test]
fn tab_strip_handles_long_titles_overflow_and_hover() {
    let _guard = serial();
    let mut overflow_state = AppState::default();
    let first = overflow_state.active;
    let second = overflow_state.add_tab();
    overflow_state.tab_mut(second).unwrap().title =
        "A very long JSON document title that must truncate".into();
    for _ in 3..=12 {
        overflow_state.add_tab();
    }
    let active = overflow_state.tabs[5].id;
    overflow_state.set_active(active);
    overflow_state.update_input(active, SAMPLE.into());
    let overflow = render(overflow_state, "07-twelve-tabs-overflow");
    assert_eq!(tab_count(&overflow), 12);
    assert!(states(a11y(&overflow, &format!("tab-{active}"))).contains(&"selected"));
    let first_rect = &element(&overflow, &format!("tab-item-{first}"))["rect"];
    let last_rect = &element(&overflow, "tab-item-12")["rect"];
    assert!(
        last_rect[0].as_f64().unwrap() > first_rect[0].as_f64().unwrap() + 1000.0,
        "tabs should overflow the viewport: first={first_rect}, last={last_rect}"
    );
    let title_width = element(&overflow, "tab-title-2")["rect"][2]
        .as_f64()
        .unwrap();
    assert!(
        title_width <= 125.0,
        "long title escaped its tab: {title_width}"
    );
    let add_rect = &element(&overflow, "add-tab")["rect"];
    assert!(add_rect[0].as_f64().unwrap() + add_rect[2].as_f64().unwrap() <= SIZE.0.into());

    let mut hover_state = AppState::default();
    let first = hover_state.active;
    let second = hover_state.add_tab();
    hover_state.add_tab();
    hover_state.set_active(first);
    let hover = render_hovered(hover_state, "tab-2", "08-inactive-tab-hover");
    assert!(states(a11y(&hover, "tab-1")).contains(&"selected"));
    assert!(!states(a11y(&hover, "tab-2")).contains(&"selected"));
    assert!(find_key(&hover["accessibility"], &format!("close-{second}")).is_some());
}

fn command(key: char, shift: bool) -> KeyEvent {
    KeyEvent {
        key: Key::Command(key),
        mods: Mods {
            cmd: true,
            shift,
            ..Mods::default()
        },
    }
}

#[test]
fn window_shortcut_adds_a_tab_before_any_control_has_focus() {
    let _guard = serial();
    let mut runtime = headless::runtime(AppState::default(), SIZE, 1.0);
    // AppSpec installs this same function in main.rs; Runtime exposes the field
    // directly so the startup route can be exercised without opening a window.
    runtime.on_key = Some(json_viewer::ui::on_key);
    assert!(!runtime.elements().iter().any(|element| element.focused));

    runtime.key(command('t', false));
    runtime.frame(0.0);
    let messages = runtime.state.take_pending();
    assert_eq!(messages, vec![Msg::AddTab]);
    runtime.update(|state, _| {
        for message in messages {
            let _ = state.update(message);
        }
    });
    runtime.frame(0.0);
    assert_eq!(runtime.state.tabs.len(), 2);
}

#[test]
fn shortcuts_queue_messages_and_focus_search() {
    let _guard = serial();
    let mut runtime = headless::runtime(AppState::default(), SIZE, 1.0);
    runtime.update(|_, tx| tx.focus("editor-1", false));
    runtime.frame(0.0);

    runtime.key(command('t', false));
    runtime.frame(0.0);
    assert_eq!(runtime.state.pending, vec![Msg::AddTab]);
    runtime.state.take_pending();

    runtime.key(command('w', false));
    runtime.frame(0.0);
    assert_eq!(
        runtime.state.pending,
        vec![Msg::CloseTab(runtime.state.active)]
    );
    runtime.state.take_pending();

    runtime.key(command('o', false));
    runtime.frame(0.0);
    assert_eq!(runtime.state.pending, vec![Msg::OpenFile]);
    runtime.state.take_pending();

    runtime.key(command('f', true));
    runtime.frame(0.0);
    assert_eq!(runtime.state.pending, vec![Msg::Format]);
    runtime.state.take_pending();

    runtime.key(command('f', false));
    runtime.frame(0.0);
    assert!(runtime.state.pending.is_empty());
    assert!(runtime
        .elements()
        .iter()
        .any(|element| element.key.as_deref() == Some("search") && element.focused));

    // The search field uses the same shortcut dispatcher as the JSON editor.
    runtime.key(command('t', false));
    runtime.frame(0.0);
    assert_eq!(runtime.state.pending, vec![Msg::AddTab]);
}

#[test]
fn search_hit_selects_output_and_reports_its_line() {
    let _guard = serial();
    let mut runtime = headless::runtime(search_state(), SIZE, 1.0);
    let (x, y) = runtime.find("search-hit-1").expect("second hit is visible");
    runtime.pointer_down(x, y);
    runtime.pointer_up(x, y);
    runtime.frame(0.0);
    assert_eq!(runtime.state.pending[0], Msg::SwitchView(TabView::Output));
    assert_eq!(
        runtime.state.pending.get(1),
        Some(&Msg::Status("address → city · line 9".into()))
    );
}
