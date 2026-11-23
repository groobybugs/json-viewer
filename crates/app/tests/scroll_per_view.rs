//! Per-(tab, view) scroll position evidence.
//!
//! Run: `cargo test -p json-viewer --test scroll_per_view -- --nocapture`

use std::path::PathBuf;

use json_viewer::json_ops::{self, JsonAction};
use json_viewer::state::{AppState, Msg, TabView};
use json_viewer::{headless, runner};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{Runtime, Tx};

const SIZE: (f32, f32) = (1280.0, 800.0);

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn fixture(marker: &str) -> String {
    let mut text = String::from("{\n");
    for i in 0..180 {
        text.push_str(&format!("  \"{marker}-{i:03}\": {i},\n"));
    }
    text.push_str("  \"done\": true\n}\n");
    text
}

fn state_with_two_tabs() -> (AppState, u64, u64) {
    let mut state = AppState::default();
    let first = state.active;
    let first_input = fixture("first");
    state.update_input(first, first_input.clone());
    assert!(state.apply_json_result(first, json_ops::process(&first_input, JsonAction::Format)));
    state.set_view(first, TabView::Input);

    let second = state.add_tab();
    let second_input = fixture("second");
    state.update_input(second, second_input.clone());
    assert!(state.apply_json_result(second, json_ops::process(&second_input, JsonAction::Format)));
    state.set_view(second, TabView::Input);
    state.set_active(first);
    (state, first, second)
}

fn settle(rt: &mut Runtime<AppState>) {
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
}

fn dispatch(rt: &mut Runtime<AppState>, msg: Msg) {
    rt.update(|state, tx| {
        state.dispatch(msg);
        runner::poll(state, 0.0, tx);
    });
    settle(rt);
}

fn scroll(rt: &mut Runtime<AppState>, key: String, y: f32) {
    rt.update(|_, tx: &mut Tx| {
        tx.instant();
        tx.scroll_to_y(key, y);
    });
    settle(rt);
}

fn row_y(rt: &Runtime<AppState>, key: &str) -> f32 {
    rt.elements()
        .iter()
        .find(|element| element.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("expected {key} to be mounted"))
        .rect[1]
}

fn assert_restored(rt: &Runtime<AppState>, key: &str, expected_y: f32) {
    let actual_y = row_y(rt, key);
    assert!(
        (actual_y - expected_y).abs() < 0.01,
        "{key} should return to y={expected_y}, got y={actual_y}"
    );
}

fn draw(rt: &mut Runtime<AppState>, name: &str) {
    let directory = target_dir().join("headless/scroll-per-view");
    std::fs::create_dir_all(&directory).expect("create evidence directory");
    let path = directory.join(format!("{name}.png"));
    let (width, height) = (SIZE.0 as u32, SIZE.1 as u32);
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
    write_png(&path, width, height, &pixels).expect("write PNG");
    assert!(path.metadata().expect("PNG metadata").len() > 0);
    let tree = headless::element_tree(rt);
    headless::write_tree(&tree, &directory.join(format!("{name}-tree.json"))).expect("write tree");
    println!("wrote {}", path.display());
}

#[test]
fn input_and_output_offsets_survive_view_and_tab_switches() {
    let (state, first, second) = state_with_two_tabs();
    let mut rt = headless::runtime(state, SIZE, 1.0);

    let first_input_row = format!("editor-{first}-row-10");
    let second_input_row = format!("editor-{second}-row-15");
    scroll(&mut rt, format!("document-scroll-{first}-input"), 160.0);
    let first_input_y = row_y(&rt, &first_input_row);
    draw(&mut rt, "first-input-scrolled");

    dispatch(&mut rt, Msg::SwitchView(TabView::Output));
    scroll(&mut rt, format!("document-scroll-{first}-output"), 480.0);
    let first_output_y = row_y(&rt, "output-text-row-30");
    draw(&mut rt, "first-output-scrolled");

    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    assert_restored(&rt, &first_input_row, first_input_y);
    dispatch(&mut rt, Msg::SwitchView(TabView::Output));
    assert_restored(&rt, "output-text-row-30", first_output_y);

    dispatch(&mut rt, Msg::SelectTab(second));
    scroll(&mut rt, format!("document-scroll-{second}-input"), 240.0);
    let second_input_y = row_y(&rt, &second_input_row);
    dispatch(&mut rt, Msg::SwitchView(TabView::Output));
    scroll(&mut rt, format!("document-scroll-{second}-output"), 640.0);
    let second_output_y = row_y(&rt, "output-text-row-40");

    dispatch(&mut rt, Msg::SelectTab(first));
    assert_restored(&rt, "output-text-row-30", first_output_y);
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    assert_restored(&rt, &first_input_row, first_input_y);
    draw(&mut rt, "first-input-restored");

    dispatch(&mut rt, Msg::SelectTab(second));
    assert_restored(&rt, "output-text-row-40", second_output_y);
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    assert_restored(&rt, &second_input_row, second_input_y);
    draw(&mut rt, "second-input-restored");
}

#[test]
fn tree_offset_survives_code_view_switch() {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, fixture("tree"));
    state.update(Msg::TreeOpen);
    assert!(state.tree.get(&id).is_some_and(|tree| tree.is_ready()));
    let mut rt = headless::runtime(state, SIZE, 1.0);

    scroll(&mut rt, format!("document-scroll-{id}-tree"), 1_100.0);
    let tree_y = row_y(&rt, "tree-row-50");
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    dispatch(&mut rt, Msg::TreeOpen);
    assert_restored(&rt, "tree-row-50", tree_y);
    draw(&mut rt, "tree-restored");
}
