//! Page frame, header and status line.

use rune::{col, el, icon, row, spacer, text, tok, Cx, El, TextStyle};
use rune_kit::{glyphs, Button, ButtonVariant, ControlSize};

use super::{kit, ACCENT, BORDER, MUTED, SUCCESS, TEXT, WINDOW_BG};
use crate::state::{AppState, Msg};

/// Draws the window background, padding, header and body.
pub fn shell(state: &AppState, cx: &mut Cx<AppState>, header: El, body: El) -> El {
    col()
        .w_full()
        .h_full()
        .fill(WINDOW_BG)
        .padding(24.0)
        .center_y()
        .child(
            col()
                .w_full()
                .h_full()
                .max_w(1180.0)
                .gap(16.0)
                .child(header)
                .child(body)
                .child(status_bar(state, cx)),
        )
}

/// Title row with the import and new-tab actions.
pub fn header(_state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = kit();
    let brand = row()
        .gap(12.0)
        .center_y()
        .child(
            el().size(40.0, 40.0)
                .center()
                .radius(10.0)
                .fill(ACCENT.alpha(0.14))
                .border(1.0, ACCENT.alpha(0.35))
                .child(icon(&glyphs::FILE_CODE, 22.0).color(ACCENT).a11y_hidden()),
        )
        .child(
            col()
                .gap(1.0)
                .child(
                    text("Simple JSON Viewer")
                        .key("app-title")
                        .role("heading")
                        .level(1)
                        .font(tok::type_::TITLE)
                        .color(TEXT),
                )
                .child(
                    text("A fast, local JSON workspace")
                        .font(tok::type_::LABEL)
                        .color(MUTED),
                ),
        );
    let import = Button::new("Import JSON")
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Large)
        .shortcut("ctrl+o")
        .on_press(|state: &mut AppState| state.dispatch(Msg::OpenFile))
        .build(&kit)
        .key("import-json");
    let new_tab = Button::new("+  New JSON")
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Large)
        .shortcut("ctrl+t")
        .on_press(|state: &mut AppState| state.dispatch(Msg::AddTab))
        .build(&kit)
        .key("new-tab");
    row()
        .w_full()
        .gap(12.0)
        .center_y()
        .child(brand)
        .child(spacer())
        .child(import)
        .child(new_tab)
}

/// Status and document summary, always visible at the foot of the window.
pub fn status_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let active = state.active_tab();
    let processing = active.is_some_and(|tab| tab.is_processing);
    let status = state.status.clone().unwrap_or_else(|| {
        if processing {
            "Processing JSON…".into()
        } else {
            "Ready".into()
        }
    });
    let tabs = state.tabs.len();
    let lines = active.map_or(0, |tab| tab.input.lines().count());
    row()
        .key("status-bar")
        .w_full()
        .h(25.0)
        .pt(8.0)
        .gap(8.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], BORDER.alpha(0.65))
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(if processing { ACCENT } else { SUCCESS })
                .a11y_hidden(),
        )
        .child(
            text(status)
                .key("status-text")
                .role("status")
                .font(TextStyle::new(11.0, 500.0))
                .color(TEXT),
        )
        .child(spacer())
        .child(
            text("Ctrl+F search  ·  Ctrl+Shift+F format")
                .font(tok::type_::MONO_CAPTION)
                .color(MUTED),
        )
        .child(
            text(format!(
                "{tabs} tab{}  ·  {lines} line{}",
                if tabs == 1 { "" } else { "s" },
                if lines == 1 { "" } else { "s" }
            ))
            .key("document-summary")
            .font(tok::type_::MONO_CAPTION)
            .color(MUTED),
        )
}
