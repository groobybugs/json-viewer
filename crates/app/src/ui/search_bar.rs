//! Search query field and the selectable result list.

use rune::{col, icon, row, spacer, text, tok, Cx, El, ScrollSpec, TextStyle, Tx};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Spinner};

use super::{kit, ACCENT, BORDER, EDITOR_BG, MUTED, PANEL_BG, TEXT};
use crate::state::{AppState, Msg, TabView};
use crate::ui::text_view::TextView;

/// Search box. Typing emits [`Msg::SearchChanged`]; the reducer decides when a
/// search actually starts (two characters, matching the old debounce guard).
pub fn search_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = kit();
    let field = rune::text_input(&state.search_edit, tok::type_::BODY)
        .key("search")
        .label("Search in JSON")
        .single_line()
        .grow(1.0)
        .min_w(0.0)
        .fill(EDITOR_BG)
        .color(TEXT)
        .placeholder("Search keys and values…", MUTED)
        .caret(ACCENT, ACCENT)
        .on_edit(|state: &mut AppState| &mut state.search_edit)
        .on_input_change(|state: &mut AppState, _| {
            let query = state.search_edit.text.clone();
            state.dispatch(Msg::SearchChanged(query));
        })
        .on_input_key(super::handle_shortcut);
    let mut input = row()
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .h(38.0)
        .px(11.0)
        .gap(9.0)
        .center_y()
        .fill(EDITOR_BG)
        .radius(7.0)
        .border(1.0, BORDER)
        .child(icon(kit.glyph.search, 16.0).color(MUTED).a11y_hidden())
        .child(field);

    if !state.search_edit.text.is_empty() {
        input = input.child(
            IconButton::new(kit.glyph.clear)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label("Clear search")
                .on_press(|state: &mut AppState| {
                    state.search_edit.replace_text("");
                    state.dispatch(Msg::SearchChanged(String::new()));
                })
                .build(&kit)
                .key("clear-search"),
        );
    }

    let mut bar = row().w_full().gap(12.0).center_y().child(input);
    if let Some(tab) = state.active_tab() {
        if tab.is_processing && tab.search_query.len() >= 2 {
            bar = bar.child(
                row()
                    .gap(6.0)
                    .center_y()
                    .child(Spinner::new().size(14.0).color(ACCENT).build(&kit))
                    .child(text("Searching…").font(tok::type_::LABEL).color(TEXT)),
            );
        } else if !tab.search_query.is_empty() {
            let count = tab.search_results.len();
            bar = bar.child(
                text(if count == 0 {
                    "No matches".into()
                } else {
                    format!("{count} match{}", if count == 1 { "" } else { "es" })
                })
                .key("search-count")
                .font(TextStyle::new(12.0, 500.0))
                .color(if count == 0 { MUTED } else { TEXT }),
            );
        }
    }
    bar
}

/// Results for the active tab. Selecting a hit opens Output, scrolls its line
/// into view, and leaves a status message with the exact source location.
pub fn search_results(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let Some(tab) = state.active_tab() else {
        return col();
    };
    if tab.search_results.is_empty() {
        return col();
    }
    let kit = kit();
    let count = tab.search_results.len();
    let heading = row()
        .w_full()
        .h(42.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], BORDER)
        .child(
            text(format!("Search Results ({count})"))
                .key("search-results-heading")
                .font(TextStyle::new(13.0, 600.0))
                .color(TEXT),
        )
        .child(spacer())
        .child(
            Button::new("Clear")
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Small)
                .on_press(|state: &mut AppState| {
                    state.search_edit.replace_text("");
                    state.dispatch(Msg::SearchChanged(String::new()));
                })
                .build(&kit)
                .key("clear-results"),
        );
    let mut list = col().w_full();
    for (index, hit) in tab.search_results.iter().enumerate() {
        let status_path = hit.path.clone();
        let line = hit.line;
        let row_label = match line {
            Some(line) => format!("{}, line {line}", hit.path),
            None => hit.path.clone(),
        };
        let result = col()
            .key(format!("search-hit-{index}"))
            .role("button")
            .label(row_label)
            .focusable(true)
            .w_full()
            .padding(10.0)
            .gap(3.0)
            .fill(EDITOR_BG)
            .border_sides([0.0, 0.0, 1.0, 0.0], BORDER.alpha(0.55))
            .child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(
                        text(hit.path.clone())
                            .font(tok::type_::MONO_CAPTION)
                            .color(ACCENT),
                    )
                    .child(spacer())
                    .child(
                        text(line.map_or_else(
                            || "Jump to output".into(),
                            |line| format!("Line {line}"),
                        ))
                        .font(tok::type_::LABEL)
                        .color(MUTED),
                    ),
            )
            .child(
                text(hit.value_preview.clone())
                    .font(tok::type_::BODY)
                    .color(TEXT),
            )
            .on_press_tx(move |state: &mut AppState, tx: &mut Tx| {
                select_hit(state, tx, line, status_path.clone());
            });
        list = list.child(kit.interactive(result));
    }
    let height = (42.0 + count as f32 * 54.0).min(238.0);
    col()
        .key("search-results")
        .w_full()
        .h(height)
        .min_h(0.0)
        .fill(PANEL_BG)
        .radius(8.0)
        .border(1.0, BORDER)
        .clip_rounded(8.0)
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

fn select_hit(state: &mut AppState, tx: &mut Tx, line: Option<usize>, path: String) {
    state.dispatch(Msg::SwitchView(TabView::Output));
    if let Some(line) = line {
        // `hit.line` is 1-based in the formatted document; TextView rows are
        // 0-based. The reveal waits for the Output view to mount, then
        // `Tx::scroll_into_view` finds the virtual row by key.
        let row = TextView::row_key("output-text", line.saturating_sub(1));
        crate::runner::queue_reveal(row);
        state.dispatch(Msg::Status(format!("{path} · line {line}")));
    } else {
        tx.scroll_into_view("output-text");
        state.dispatch(Msg::Status(path));
    }
}
