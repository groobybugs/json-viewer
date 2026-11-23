//! Tab strip and the active document.

use rune::{col, el, icon, row, text, tok, Cx, El, ScrollSpec, TextStyle};
use rune_kit::{glyphs, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant};

use super::{kit, ACCENT, BORDER, DANGER, DANGER_BG, EDITOR_BG, MUTED, TEXT};
use crate::state::{AppState, Msg, TabView};
use crate::ui::text_view::TextView;

/// Horizontal tab strip. The last remaining tab has no close button, matching
/// the original UI.
pub fn tab_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = kit();
    let mut strip = row().w_full().gap(6.0).pb(7.0).scroll_x();
    for tab in &state.tabs {
        let active = tab.id == state.active;
        let id = tab.id;
        let select = Button::new(tab.title.clone())
            .variant(if active {
                ButtonVariant::Neutral
            } else {
                ButtonVariant::Ghost
            })
            .control_size(ControlSize::Large)
            .on_press(move |state: &mut AppState| state.dispatch(Msg::SelectTab(id)))
            .build(&kit)
            .key(format!("tab-{id}"))
            .role("tab")
            .focus_selected(active);
        let mut item = row().gap(2.0).center_y().child(select);
        if state.tabs.len() > 1 {
            item = item.child(
                IconButton::new(kit.glyph.close)
                    .variant(IconButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .label(format!("Close {}", tab.title))
                    .on_press(move |state: &mut AppState| state.dispatch(Msg::CloseTab(id)))
                    .build(&kit)
                    .key(format!("close-{id}")),
            );
        }
        if active {
            item = item.border_sides([0.0, 0.0, 2.0, 0.0], ACCENT);
        }
        strip = strip.child(item.key(format!("tab-item-{id}")));
    }
    col()
        .w_full()
        .child(strip.role("tablist").focus_group())
        .child(el().w_full().h(1.0).fill(BORDER.alpha(0.8)))
}

/// Input or output for the active tab, plus a prominent parse-error alert.
pub fn tab_content(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let Some(tab) = state.active_tab() else {
        return col().w_full().grow(1.0).fill(EDITOR_BG);
    };
    let body = match tab.view {
        TabView::Input => {
            let edit = state.editor(tab.id).expect("every tab has an editor");
            TextView::editable(&tab.input, edit)
                .line_numbers(true)
                .build(&format!("editor-{}", tab.id))
                .placeholder("Paste your JSON here or drop a JSON file…", MUTED)
                .fill(EDITOR_BG)
                .padding(16.0)
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .on_edit(|state: &mut AppState| {
                    let id = state.active;
                    state
                        .editors
                        .get_mut(&id)
                        .expect("active tab has an editor")
                })
                .on_input_change(|state: &mut AppState, _| state.sync_active_editor())
                .on_input_key(super::handle_shortcut)
        }
        TabView::Output => TextView::read_only(&tab.output)
            .line_numbers(true)
            .visible_lines(200)
            .highlight(&tab.search_query)
            .build("output-text")
            .w_full()
            .grow(1.0)
            .fill(EDITOR_BG),
    };
    let scroller = col()
        .key(format!("document-scroll-{}", tab.id))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(EDITOR_BG)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_x()
        .child(body);

    let mut content = col().w_full().grow(1.0).min_h(0.0).fill(EDITOR_BG);
    if let Some(error) = &tab.error {
        content = content.child(
            row()
                .key("error-alert")
                .role("alert")
                .w_full()
                .gap(10.0)
                .padding(12.0)
                .center_y()
                .fill(DANGER_BG)
                .border_sides([0.0, 0.0, 1.0, 0.0], DANGER.alpha(0.6))
                .child(icon(&glyphs::CIRCLE_X, 18.0).color(DANGER).a11y_hidden())
                .child(
                    col()
                        .gap(2.0)
                        .child(
                            text("Parse error")
                                .font(TextStyle::new(12.0, 600.0))
                                .color(DANGER),
                        )
                        .child(
                            text(format!("Invalid JSON: {error}"))
                                .key("error-message")
                                .font(tok::type_::BODY)
                                .color(TEXT),
                        ),
                ),
        );
    }
    content.child(scroller)
}
