//! JSON Viewer binary.
//!
//! Windowed run:
//! `cargo run -p json-viewer`
//!
//! Rune's headless tooling also works directly:
//! `cargo run -p json-viewer -- --snap target/headless/app.png --tree target/headless/tree.json`
//!
//! This crate's own headless mode renders the placeholder, writes the PNG and
//! dumps the element tree:
//! `cargo run -p json-viewer -- --headless target/headless`

#![allow(dead_code)]

use std::path::PathBuf;

use json_viewer::headless;
use json_viewer::state::{AppState, Msg};
use json_viewer::{persist, runner, ui};
use rune::AppSpec;

/// Initial state: load persisted tabs when available, otherwise one blank tab.
fn boot() -> AppState {
    let mut state = AppState::default();
    match persist::load_tabs() {
        Ok(tabs) if !tabs.is_empty() => {
            state.update(Msg::TabsLoaded(tabs));
        }
        Ok(_) => {}
        Err(error) => {
            state.status = Some(format!("Could not load tabs: {error}"));
        }
    }
    state
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(index) = args.iter().position(|arg| arg == "--headless") {
        let directory = args
            .get(index + 1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/headless"));
        let png = directory.join("placeholder.png");
        let tree = directory.join("placeholder-tree.json");
        match headless::render_and_dump(
            AppState::default(),
            headless::DEFAULT_SIZE,
            headless::DEFAULT_SCALE,
            &png,
            &tree,
        ) {
            Ok(_) => {
                println!("rendered {}", png.display());
                println!("element tree {}", tree.display());
            }
            Err(error) => {
                eprintln!("headless render failed: {error:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mut spec = AppSpec::new("JSON Viewer", headless::DEFAULT_SIZE, boot, ui::view);
    spec.poll = Some(runner::poll);
    spec.on_key = Some(ui::on_key);
    rune::run(spec);
}
