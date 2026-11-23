//! Headless smoke test: renders the placeholder to `target/headless/smoke.png`
//! and writes/asserts the element tree.

use std::path::PathBuf;

use json_viewer::headless;
use json_viewer::state::AppState;

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

#[test]
fn placeholder_renders_to_png_and_contains_the_title() {
    let directory = target_dir().join("headless");
    let png = directory.join("smoke.png");
    let tree_path = directory.join("smoke-tree.json");

    let tree = headless::render_and_dump(
        AppState::default(),
        headless::DEFAULT_SIZE,
        1.0,
        &png,
        &tree_path,
    )
    .expect("headless render failed");

    let metadata = std::fs::metadata(&png).expect("PNG was not written");
    assert!(metadata.len() > 0, "PNG is empty");
    assert!(tree_path.exists(), "element tree was not written");

    let rendered = tree.to_string();
    assert!(
        rendered.contains("JSON Viewer"),
        "element tree did not contain the window title: {rendered}"
    );
    assert!(
        tree["elements"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "element tree has no elements"
    );
}
