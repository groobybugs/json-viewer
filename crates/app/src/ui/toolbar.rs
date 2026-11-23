//! Document actions: format, minify, copy, export, open, clear.

use rune::{row, Cx, El};
use rune_kit::{Button, ButtonVariant, Kit};

use crate::state::{AppState, Msg, TabView};

/// Action row for the active tab. Buttons disable themselves when there is
/// nothing to act on, matching the original React toolbar.
pub fn toolbar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = Kit::neutral(true);
    let tab = state.active_tab();
    let has_input = tab.is_some_and(|tab| !tab.input.trim().is_empty());
    let has_output = tab.is_some_and(|tab| !tab.output.is_empty());
    let processing = tab.is_some_and(|tab| tab.is_processing);

    let format = Button::new("Format")
        .variant(ButtonVariant::Contrast)
        .disabled(processing || !has_input)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Format))
        .build(&kit);
    let minify = Button::new("Minify")
        .disabled(processing || !has_input)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Minify))
        .build(&kit);
    let copy = Button::new("Copy")
        .disabled(processing || (!has_input && !has_output))
        .on_press(|state: &mut AppState| state.dispatch(Msg::Copy))
        .build(&kit);
    let export = Button::new("Export")
        .disabled(processing || (!has_input && !has_output))
        .on_press(|state: &mut AppState| state.dispatch(Msg::Export))
        .build(&kit);
    let open = Button::new("Open")
        .on_press(|state: &mut AppState| state.dispatch(Msg::OpenFile))
        .build(&kit);
    let clear = Button::new("Clear")
        .variant(ButtonVariant::Ghost)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Clear))
        .build(&kit);
    let input_view = Button::new("Input")
        .variant(if tab.is_some_and(|tab| tab.view == TabView::Input) {
            ButtonVariant::Ghost
        } else {
            ButtonVariant::GhostFaint
        })
        .on_press(|state: &mut AppState| state.dispatch(Msg::SwitchView(TabView::Input)))
        .build(&kit);
    let output_view = Button::new("Output")
        .variant(if tab.is_some_and(|tab| tab.view == TabView::Output) {
            ButtonVariant::Ghost
        } else {
            ButtonVariant::GhostFaint
        })
        .on_press(|state: &mut AppState| state.dispatch(Msg::SwitchView(TabView::Output)))
        .build(&kit);

    row()
        .w_full()
        .gap(6.0)
        .child(format)
        .child(minify)
        .child(copy)
        .child(export)
        .child(open)
        .child(clear)
        .child(rune::spacer())
        .child(input_view)
        .child(output_view)
}
