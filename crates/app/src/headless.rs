//! Headless rendering used by tests and the `--headless` CLI mode.
//!
//! Rune's own `--snap` and `--tree` flags work on the binary, but tests need a
//! reusable function. This module follows the same primitives Rune's tests
//! use: `Gpu::headless()`, an offscreen `Rgba8Unorm` target, the normal
//! `Renderer`, then CPU readback and `write_png`.
//!
//! The element tree is Rune's accessibility dump plus per-element geometry, so
//! assertions can look for visible text such as `JSON Viewer`.

use std::path::Path;

use anyhow::Result;
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::text::TextSystem;
use rune::{Env, Runtime};
use serde_json::{json, Value};

use crate::state::AppState;
use crate::ui;

/// Default logical window size in points.
pub const DEFAULT_SIZE: (f32, f32) = (1280.0, 800.0);
/// Default device scale (Rune's headless default is 2.0).
pub const DEFAULT_SCALE: f32 = 2.0;

// `AppSpec::fonts` / `TextSystem::new` is all-or-nothing on native: a non-empty
// `fonts` REPLACES Rune's embedded Inter faces. Keep it empty so Rune's own
// Inter (the `embedded-fonts` default feature) is used.
/// No bundled faces; `TextSystem::new(&[], ..)` registers Rune's embedded Inter.
pub static APP_FONTS: &[&[u8]] = &[];
/// IBM Plex Mono (fontconfig) primary, `ui-monospace` as fixed-pitch fallback.
pub const APP_FAMILIES: (&str, &str) = ("Inter", "IBM Plex Mono, ui-monospace");

/// A text system with the app's registered faces, for `Runtime::with_text`.
pub fn text_system() -> TextSystem {
    TextSystem::new(APP_FONTS, APP_FAMILIES)
}

/// Builds a headless runtime, runs two frames and settles animations.
pub fn runtime(state: AppState, size: (f32, f32), scale: f32) -> Runtime<AppState> {
    let mut runtime = Runtime::with_text(state, ui::view, size, Env::new(true, scale), text_system());
    runtime.set_srgb_blend(true);
    runtime.frame(0.0);
    runtime.settle_all();
    runtime.frame(0.0);
    runtime
}

/// Renders the view to `path` and returns the element tree.
///
/// The target is `size * scale` physical pixels.
pub fn render_png(state: AppState, size: (f32, f32), scale: f32, path: &Path) -> Result<Value> {
    let width = (size.0 * scale).round().max(1.0) as u32;
    let height = (size.1 * scale).round().max(1.0) as u32;
    let mut runtime = runtime(state, size, scale);

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
    write_png(path, width, height, &pixels)?;
    Ok(element_tree(&runtime))
}

/// Returns the element tree as JSON: Rune's accessibility tree plus one entry
/// per element with role, key, text, label and geometry.
pub fn element_tree(runtime: &Runtime<AppState>) -> Value {
    let elements: Vec<Value> = runtime
        .elements()
        .iter()
        .map(|element| {
            json!({
                "path": &element.path,
                "key": &element.key,
                "role": element.role,
                "label": &element.label,
                "text": &element.text,
                "rect": element.rect,
                "visible": element.visible,
                "focusable": element.focusable,
                "focused": element.focused,
            })
        })
        .collect();
    json!({
        "accessibility": runtime.accessibility_json(),
        "elements": elements,
    })
}

/// Writes an element tree to `path`.
pub fn write_tree(tree: &Value, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(tree)?)?;
    Ok(())
}

/// Renders the placeholder view to `png_path`, dumps the element tree to
/// `tree_path`, and returns the tree.
pub fn render_and_dump(
    state: AppState,
    size: (f32, f32),
    scale: f32,
    png_path: &Path,
    tree_path: &Path,
) -> Result<Value> {
    let tree = render_png(state, size, scale, png_path)?;
    write_tree(&tree, tree_path)?;
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_tree_contains_the_window_title() {
        // Building the tree needs no GPU; keep this test cheap.
        let runtime = runtime(AppState::default(), DEFAULT_SIZE, 1.0);
        let tree = element_tree(&runtime).to_string();
        assert!(
            tree.contains("JSON Viewer"),
            "tree did not contain the title"
        );
    }
}
