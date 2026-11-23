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

use rune::{col, Cx, El};

use crate::state::AppState;

/// Window title, also asserted by the headless smoke test.
pub const TITLE: &str = "JSON Viewer";

/// Builds the whole window.
///
/// The dark theme is forced (the original app was dark-only) and the layout is
/// a vertical stack: header, toolbar, tab strip, search, document, status.
pub fn view(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let header = shell::header(state, cx);
    let body = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(toolbar::toolbar(state, cx))
        .child(tabs::tab_bar(state, cx))
        .child(search_bar::search_bar(state, cx))
        .child(search_bar::search_results(state, cx))
        .child(tabs::tab_content(state, cx));
    shell::shell(state, cx, header, body)
}
