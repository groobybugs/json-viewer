//! Rune view layer.
//!
//! [`view`] is the function pointer handed to `AppSpec`/`Runtime`. Each
//! submodule owns one region of the window; see the ownership map in
//! `AGENTS.md`. Views are cheap-to-rebuild descriptions: handlers only queue
//! [`crate::state::Msg`] values, they never do I/O.

pub mod search_bar;
pub mod shell;
pub mod tabs;
pub mod text_view;
pub mod toolbar;
pub mod tree_view;

use rune::{col, Color, Cx, El, KeyEvent, Tx};
use rune_kit::{Kit, PaletteSeed};

use crate::state::{AppState, Msg};

/// Window title, also asserted by the headless smoke test.
pub const TITLE: &str = "JSON Viewer";

// Rune colours are linear sRGB. These are the linear values of the familiar
// Tailwind slate/blue hex palette used by the original React app.
pub(crate) const WINDOW_BG: Color = Color {
    r: 0.00334654,
    g: 0.00604883,
    b: 0.0144438,
    a: 1.0,
}; // #0b1220
pub(crate) const PANEL_BG: Color = Color {
    r: 0.00802319,
    g: 0.0159963,
    b: 0.0343398,
    a: 1.0,
}; // #162234
pub(crate) const EDITOR_BG: Color = Color {
    r: 0.00477695,
    g: 0.00856813,
    b: 0.0231534,
    a: 1.0,
}; // #0f172a
pub(crate) const BORDER: Color = Color {
    r: 0.0331048,
    g: 0.0528606,
    b: 0.0908417,
    a: 1.0,
}; // #334155
pub(crate) const TEXT: Color = Color {
    r: 0.760525,
    g: 0.806952,
    b: 0.871367,
    a: 1.0,
}; // #e2e8f0
pub(crate) const MUTED: Color = Color {
    r: 0.296138,
    g: 0.366253,
    b: 0.47932,
    a: 1.0,
}; // #94a3b8
pub(crate) const ACCENT: Color = Color {
    r: 0.116971,
    g: 0.376262,
    b: 0.955973,
    a: 1.0,
}; // #60a5fa
pub(crate) const SUCCESS: Color = Color {
    r: 0.0684782,
    g: 0.730461,
    b: 0.215861,
    a: 1.0,
}; // #4ade80
pub(crate) const DANGER: Color = Color {
    r: 0.938686,
    g: 0.165132,
    b: 0.165132,
    a: 1.0,
}; // #f87171
pub(crate) const DANGER_BG: Color = Color {
    r: 0.0356013,
    g: 0.0109601,
    b: 0.0193824,
    a: 1.0,
}; // #351b26

pub(crate) fn kit() -> Kit {
    Kit::from_palette(PaletteSeed {
        bg: EDITOR_BG,
        fg: TEXT,
        accent: ACCENT,
    })
}

/// Builds the whole window.
///
/// The dark theme is forced (the original app was dark-only). The document
/// card grows into all space left by the header, search, results and status.
pub fn view(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let document = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(PANEL_BG)
        .radius(10.0)
        .border(1.0, BORDER)
        .clip_rounded(10.0)
        .child(toolbar::toolbar(state, cx))
        .child(tabs::tab_content(state, cx));
    let body = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(search_bar::search_bar(state, cx))
        .child(tabs::tab_bar(state, cx))
        .child(search_bar::search_results(state, cx))
        .child(document);
    let header = shell::header(state, cx);
    crate::io::drop_zone(shell::shell(state, cx, header, body)).on_key(handle_shortcut)
}

/// Window-level keyboard shortcuts, including startup before any control has focus.
pub fn on_key(state: &mut AppState, event: &KeyEvent, tx: &mut Tx) -> bool {
    handle_shortcut(state, event, tx)
}

fn handle_shortcut(state: &mut AppState, event: &KeyEvent, tx: &mut Tx) -> bool {
    let msg = if event.is_command('t') && !event.mods.shift {
        Some(Msg::AddTab)
    } else if event.is_command('w') && !event.mods.shift {
        Some(Msg::CloseTab(state.active))
    } else if event.is_command('o') && !event.mods.shift {
        Some(Msg::OpenFile)
    } else if event.is_command('f') && event.mods.shift {
        Some(Msg::Format)
    } else {
        None
    };
    if let Some(msg) = msg {
        if !tx.key_repeat() {
            state.dispatch(msg);
        }
        return true;
    }
    if event.is_command('f') && !event.mods.shift {
        tx.focus("search", true);
        return true;
    }
    false
}
