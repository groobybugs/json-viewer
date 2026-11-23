//! Search query field and the result list.

use rune::{col, row, text, tok, Cx, El, ScrollSpec};
use rune_kit::{Button, ButtonVariant, Kit};

use crate::state::{AppState, Msg};

/// Search box. Typing emits [`Msg::SearchChanged`]; the reducer decides when a
/// search actually starts (two characters, matching the old debounce guard).
pub fn search_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let field = rune::text_input(&state.search_edit, tok::type_::BODY)
        .key("search")
        .label("Search in JSON")
        .w_full()
        .padding(8.0)
        .fill(theme.card)
        .color(theme.text)
        .placeholder("Search in JSON...", theme.text_secondary)
        .caret(theme.text, tok::accent(0, true))
        .on_edit(|state: &mut AppState| &mut state.search_edit)
        .on_input_change(|state: &mut AppState, _| {
            let query = state.search_edit.text.clone();
            state.dispatch(Msg::SearchChanged(query));
        });
    row().w_full().child(field)
}

/// Results for the active tab, capped to a scrollable panel.
pub fn search_results(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let Some(tab) = state.active_tab() else {
        return col();
    };
    if tab.search_results.is_empty() {
        return col();
    }
    let kit = Kit::neutral(true);
    let heading = row()
        .w_full()
        .gap(8.0)
        .child(
            text(format!("Search Results ({})", tab.search_results.len()))
                .font(tok::type_::LABEL)
                .color(theme.text_secondary),
        )
        .child(rune::spacer())
        .child(
            Button::new("Clear")
                .variant(ButtonVariant::GhostFaint)
                .on_press(|state: &mut AppState| {
                    state.search_edit.replace_text("");
                    state.dispatch(Msg::SearchChanged(String::new()));
                })
                .build(&kit),
        );
    let mut list = col().w_full().gap(2.0);
    for hit in &tab.search_results {
        list = list.child(
            col()
                .w_full()
                .padding(6.0)
                .fill(theme.card)
                .child(
                    text(hit.path.clone())
                        .font(tok::type_::MONO_CAPTION)
                        .color(theme.text_secondary),
                )
                .child(
                    text(hit.value_preview.clone())
                        .font(tok::type_::BODY)
                        .color(theme.text),
                ),
        );
    }
    col()
        .w_full()
        .max_h(240.0)
        .min_h(0.0)
        .gap(4.0)
        .fill(theme.bg)
        .child(heading)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        )
}
