//! Renders the documentation screenshot at `docs/screenshot.png`.
//!
//! Run from the repository root:
//! `cargo run -p json-viewer --bin screenshot`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use json_viewer::headless;
use json_viewer::json_ops::{self, JsonAction};
use json_viewer::search::{self, SearchOptions};
use json_viewer::state::{AppState, Msg, TabView};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::Runtime;

fn formatted(input: &str) -> String {
    json_ops::process(input, JsonAction::Format).expect("screenshot fixture is valid JSON")
}

fn main() {
    let config = r#"{
        "name": "simple-json-viewer",
        "version": "1.0.0",
        "private": true,
        "scripts": {
            "dev": "vite",
            "build": "vite build",
            "test": "vitest"
        },
        "dependencies": {
            "react": "^18.3.1",
            "react-dom": "^18.3.1"
        },
        "viewer": {
            "theme": "dark",
            "fontSize": 13,
            "wordWrap": false
        }
    }"#;
    let data = r#"{
        "users": [
            { "name": "Ada Lovelace", "role": "admin", "active": true },
            { "name": "Grace Hopper", "role": "maintainer", "active": true },
            { "name": "Alan Turing", "role": "viewer", "active": false }
        ],
        "total": 3
    }"#;

    let config_output = formatted(config);
    let data_output = formatted(data);

    let mut state = AppState::default();
    let first = state.active;
    state.tabs[0].title = "app-config.json".into();
    state.tabs[0].input = config.into();
    state.apply_json_result(first, Ok(config_output.clone()));

    let cancel = AtomicBool::new(false);
    let hits = search::search(
        &config_output,
        "name",
        SearchOptions::default(),
        &cancel,
    );
    state.tabs[0].search_query = "name".into();
    state.update(Msg::SearchDone {
        tab: first,
        hits,
    });

    let second = state.add_tab();
    state.tabs[1].title = "users.json".into();
    state.tabs[1].input = data.into();
    state.apply_json_result(second, Ok(data_output));
    state.active = first;
    state.update(Msg::SwitchView(TabView::Output));

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshot.png");
    render(state, &path);

    let size = std::fs::metadata(&path).expect("screenshot missing").len();
    println!("wrote {} ({size} bytes)", path.display());
    assert!(size < 300 * 1024, "docs/screenshot.png must stay under 300 KB");
}

fn render(state: AppState, path: &std::path::Path) {
    let (points_w, points_h) = headless::DEFAULT_SIZE;
    let scale = headless::DEFAULT_SCALE;
    let width = (points_w * scale).round().max(1.0) as u32;
    let height = (points_h * scale).round().max(1.0) as u32;

    let mut runtime: Runtime<AppState> =
        headless::runtime(state, headless::DEFAULT_SIZE, scale);
    runtime.set_srgb_blend(true);
    runtime.frame(0.0);
    runtime.settle_all();
    runtime.frame(0.0);

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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create docs directory");
    }
    write_png(path, width, height, &pixels).expect("write screenshot PNG");
}
