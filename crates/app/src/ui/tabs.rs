//! Tab strip and the active document.

use rune::{col, oklch, row, text, tok, Cx, El, ScrollSpec};
use rune_kit::{Button, ButtonVariant, Kit};

use crate::state::{AppState, Msg, TabView};
use crate::ui::text_view::TextView;

/// Horizontal tab strip. The last remaining tab has no close button, matching
/// the original UI.
pub fn tab_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = Kit::neutral(true);
    let mut strip = row().w_full().gap(2.0);
    for tab in &state.tabs {
        let active = tab.id == state.active;
        let id = tab.id;
        let select = Button::new(tab.title.clone())
            .variant(if active {
                ButtonVariant::Contrast
            } else {
                ButtonVariant::Ghost
            })
            .on_press(move |state: &mut AppState| state.dispatch(Msg::SelectTab(id)))
            .build(&kit)
            .key(format!("tab-{id}"));
        strip = strip.child(select);
        if state.tabs.len() > 1 {
            let close = Button::new("×")
                .variant(ButtonVariant::GhostFaint)
                .on_press(move |state: &mut AppState| state.dispatch(Msg::CloseTab(id)))
                .build(&kit)
                .key(format!("close-{id}"));
            strip = strip.child(close);
        }
    }
    strip
}

/// Input or output for the active tab, plus its error line.
pub fn tab_content(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let Some(tab) = state.active_tab() else {
        return col().w_full().grow(1.0);
    };
    let body = match tab.view {
        TabView::Input => {
            let edit = state.editor(tab.id).expect("every tab has an editor");
            TextView::editable(&tab.input, edit)
                .line_numbers(true)
                .build(&format!("editor-{}", tab.id))
                .on_edit(|state: &mut AppState| {
                    let id = state.active;
                    state
                        .editors
                        .get_mut(&id)
                        .expect("active tab has an editor")
                })
                .on_input_change(|state: &mut AppState, _| state.sync_active_editor())
        }
        TabView::Output => TextView::read_only(&tab.output)
            .line_numbers(true)
            .visible_lines(200)
            .build("output-text"),
    };
    let scroller = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body);

    let mut content = col().w_full().grow(1.0).min_h(0.0).gap(8.0).child(scroller);
    if let Some(error) = &tab.error {
        let error = text(format!("Invalid JSON: {error}"))
            .font(tok::type_::LABEL)
            .color(oklch(0.62, 0.19, 25.0))
            .padding(8.0)
            .fill(theme.card);
        content = content.child(error);
    }
    content
}
