//! Document actions: format, minify, copy, export, open, clear.

use rune::{row, spacer, text, tok, Cx, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Spinner};

use super::{kit, BORDER, MUTED, TEXT, WINDOW_BG};
use crate::state::{AppState, Msg, TabView};

static FORMAT_ICON: Icon = Icon {
    name: "json-viewer-format",
    view: 16.0,
    body: r#"<rect x="2.5" y="2.5" width="11" height="11" rx="1.5" stroke="currentColor"/><path d="M8 5.5V10.5M5.5 8H10.5" stroke="currentColor" stroke-linecap="round"/>"#,
};
static MINIFY_ICON: Icon = Icon {
    name: "json-viewer-minify",
    view: 16.0,
    body: r#"<rect x="2.5" y="2.5" width="11" height="11" rx="1.5" stroke="currentColor"/><path d="M5.5 8H10.5" stroke="currentColor" stroke-linecap="round"/>"#,
};
static EXPORT_ICON: Icon = Icon {
    name: "json-viewer-export",
    view: 16.0,
    body: r#"<path d="M8 2.5V10M5 7L8 10L11 7M3 11.5V13.5H13V11.5" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static OPEN_ICON: Icon = Icon {
    name: "json-viewer-open",
    view: 16.0,
    body: r#"<path d="M8 10V2.5M5 5.5L8 2.5L11 5.5M3 10.5V13.5H13V10.5" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// View switch (Input | Output | Tree) and action row for the active tab.
/// Actions that would race a worker or have no content are disabled; busy
/// state stays visible.
pub fn toolbar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = kit();
    let tab = state.active_tab();
    let has_input = tab.is_some_and(|tab| !tab.input.trim().is_empty());
    let has_output = tab.is_some_and(|tab| !tab.output.is_empty());
    let has_content = tab
        .is_some_and(|tab| !tab.input.is_empty() || !tab.output.is_empty() || tab.error.is_some());
    let processing = tab.is_some_and(|tab| tab.is_processing);
    // The tree is a third per-tab display mode, next to Input/Output (see
    // AGENTS.md): the tree's open flag lives in `AppState::tree`, not `Tab`,
    // so `TabView` (persisted) stays a two-variant enum.
    let tree_open = tab.is_some_and(|tab| {
        state
            .tree
            .get(&tab.id)
            .is_some_and(|tree| tree.open)
    });

    let input_view = Button::new("Input")
        .variant(if tab.is_some_and(|tab| tab.view == TabView::Input) {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::Ghost
        })
        .control_size(ControlSize::Regular)
        .on_press(|state: &mut AppState| state.dispatch(Msg::SwitchView(TabView::Input)))
        .build(&kit)
        .key("input-view")
        .focus_selected(tab.is_some_and(|tab| tab.view == TabView::Input));
    let output_view = Button::new("Output")
        .variant(if tab.is_some_and(|tab| tab.view == TabView::Output) {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::Ghost
        })
        .control_size(ControlSize::Regular)
        .on_press(|state: &mut AppState| state.dispatch(Msg::SwitchView(TabView::Output)))
        .build(&kit)
        .key("output-view")
        .role("tab")
        .focus_selected(tab.is_some_and(|tab| tab.view == TabView::Output));
    let tree_view = Button::new("Tree")
        .variant(if tree_open {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::Ghost
        })
        .control_size(ControlSize::Regular)
        .on_press(|state: &mut AppState| state.dispatch(Msg::TreeOpen))
        .build(&kit)
        .key("tree-view")
        .role("tab")
        .focus_selected(tree_open);
    let switcher = row()
        .role("tablist")
        .focus_group()
        .gap(2.0)
        .padding(2.0)
        .radius(6.0)
        .fill(WINDOW_BG)
        .child(input_view.role("tab"))
        .child(output_view.role("tab"))
        .child(tree_view.role("tab"));

    let format = Button::new("Format")
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Regular)
        .icon(&FORMAT_ICON)
        .shortcut("ctrl+shift+f")
        .disabled(processing || !has_input)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Format))
        .build(&kit)
        .key("format");
    let minify = Button::new("Minify")
        .control_size(ControlSize::Regular)
        .icon(&MINIFY_ICON)
        .disabled(processing || !has_input)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Minify))
        .build(&kit)
        .key("minify");
    let copy = Button::new("Copy")
        .control_size(ControlSize::Regular)
        .icon(kit.glyph.copy)
        .disabled(processing || !has_output)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Copy))
        .build(&kit)
        .key("copy");
    let export = Button::new("Export")
        .control_size(ControlSize::Regular)
        .icon(&EXPORT_ICON)
        .disabled(processing || !has_output)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Export))
        .build(&kit)
        .key("export");
    let open = Button::new("Open")
        .control_size(ControlSize::Regular)
        .icon(&OPEN_ICON)
        .shortcut("ctrl+o")
        .on_press(|state: &mut AppState| state.dispatch(Msg::OpenFile))
        .build(&kit)
        .key("open");
    let clear = Button::new("Clear")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Regular)
        .icon(kit.glyph.clear)
        .disabled(processing || !has_content)
        .on_press(|state: &mut AppState| state.dispatch(Msg::Clear))
        .build(&kit)
        .key("clear");

    let actions = row()
        .gap(6.0)
        .center_y()
        .child(format)
        .child(minify)
        .child(copy)
        .child(export)
        .child(open)
        .child(clear);
    let mut bar = row()
        .w_full()
        .h(52.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], BORDER)
        .child(switcher);
    if processing {
        bar = bar.child(
            row()
                .gap(6.0)
                .center_y()
                .child(
                    Spinner::new()
                        .size(14.0)
                        .color(TEXT)
                        .build(&kit)
                        .key("processing-spinner"),
                )
                .child(
                    text("Processing…")
                        .key("processing-label")
                        .font(tok::type_::LABEL)
                        .color(MUTED),
                ),
        );
    }
    bar.child(spacer()).child(actions)
}
