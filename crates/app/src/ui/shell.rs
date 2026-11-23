//! Page frame, header and status line.

use rune::{col, row, spacer, text, tok, Cx, El};
use rune_kit::{Button, ButtonVariant, Kit};

use crate::state::{AppState, Msg};
use crate::ui;

/// Draws the window background, padding, header and body.
pub fn shell(state: &AppState, cx: &mut Cx<AppState>, header: El, body: El) -> El {
    let theme = tok::theme(true);
    col()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .padding(16.0)
        .gap(12.0)
        .child(header)
        .child(body)
        .child(status_bar(state, cx))
}

/// Title row with the import and new-tab actions.
pub fn header(_state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let kit = Kit::neutral(true);
    let title = text(ui::TITLE).font(tok::type_::TITLE).color(theme.text);
    let import = Button::new("Import JSON")
        .on_press(|state: &mut AppState| state.dispatch(Msg::OpenFile))
        .build(&kit);
    let new_tab = Button::new("New JSON")
        .variant(ButtonVariant::Contrast)
        .on_press(|state: &mut AppState| state.dispatch(Msg::AddTab))
        .build(&kit);
    row()
        .w_full()
        .gap(8.0)
        .child(title)
        .child(spacer())
        .child(import)
        .child(new_tab)
}

/// Transient status line (`None` renders no text).
pub fn status_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let label = state.status.clone().unwrap_or_default();
    text(label)
        .font(tok::type_::LABEL)
        .color(theme.text_secondary)
}
