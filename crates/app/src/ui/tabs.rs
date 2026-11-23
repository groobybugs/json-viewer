//! Tab strip and the active document.

use rune::{col, el, row, text, Color, Cx, El, TextStyle};
use rune_kit::{ControlSize, IconButton, IconButtonVariant};

use super::{kit, ACCENT, BORDER, MUTED, PANEL_BG, TEXT};
use crate::state::{AppState, Msg, TabView};
use crate::ui::text_view::TextView;

static ADD_ICON: rune::Icon = rune::Icon {
    name: "json-viewer-add-tab",
    view: 16.0,
    body: r#"<path d="M8 3.5V12.5M3.5 8H12.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
};

/// Key of the document viewport currently mounted by the real UI.
pub(crate) fn active_scroll_key(state: &AppState) -> Option<String> {
    let tab = state.active_tab()?;
    let tree_open = state.tree.get(&tab.id).is_some_and(|tree| tree.open);
    Some(document_scroll_key(tab.id, tab.view, tree_open))
}

/// Browser-style tabs connect the active document to its card. The trailing
/// add button stays reachable while the tab row itself scrolls horizontally.
pub fn tab_bar(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let kit = kit();
    let closable = state.tabs.len() > 1;
    let mut strip = row().h(39.0).gap(4.0).shrink().min_w(0.0).scroll_x();
    for tab in &state.tabs {
        let active = tab.id == state.active;
        let id = tab.id;
        let title = tab.title.clone();
        let select = row()
            .key(format!("tab-{id}"))
            .role("tab")
            .label(title.clone())
            .focusable(true)
            .focus_selected(active)
            .grow(1.0)
            .min_w(0.0)
            .h_full()
            .pl(11.0)
            .center_y()
            .on_press(move |state: &mut AppState| state.dispatch(Msg::SelectTab(id)))
            .child(
                text(title.clone())
                    .key(format!("tab-title-{id}"))
                    .font(TextStyle::new(12.0, 500.0))
                    .color(if active { TEXT } else { MUTED })
                    .group_color(TEXT)
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate()
                    .help_when_truncated(title.clone())
                    .a11y_hidden(),
            );
        let mut content = row().w_full().grow(1.0).min_h(0.0).center_y().child(select);
        if closable {
            content = content.child(
                IconButton::new(kit.glyph.close)
                    .variant(IconButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .label(format!("Close {}", tab.title))
                    .on_press(move |state: &mut AppState| state.dispatch(Msg::CloseTab(id)))
                    .build(&kit)
                    .key(format!("close-{id}"))
                    .group_opacity(if active { 1.0 } else { 0.0 }, 1.0),
            );
        }
        let mut item = col()
            .key(format!("tab-item-{id}"))
            .w(148.0)
            .h(if active { 39.0 } else { 38.0 })
            .corners([8.0, 8.0, 0.0, 0.0])
            .hover_group()
            .fill(if active { PANEL_BG } else { Color::TRANSPARENT })
            .hover(if active {
                Color::TRANSPARENT
            } else {
                kit.c.hover
            })
            .child(
                el().w_full()
                    .h(2.0)
                    .fill(if active { ACCENT } else { Color::TRANSPARENT })
                    .a11y_hidden(),
            )
            .child(content.pr(4.0));
        if active {
            // No bottom edge: the tab and card read as one surface. Paint above
            // the card's top ring at their one-point overlap.
            item = item
                .border_sides([0.0, 1.0, 0.0, 1.0], BORDER)
                .paint_order(1);
        }
        strip = strip.child(item);
    }
    let add = IconButton::new(&ADD_ICON)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Regular)
        .label("New JSON tab")
        .shortcut("ctrl+t")
        .on_press(|state: &mut AppState| state.dispatch(Msg::AddTab))
        .build(&kit)
        .key("add-tab");
    let has_results = state
        .active_tab()
        .is_some_and(|tab| !tab.search_results.is_empty());
    row()
        .key("tab-strip")
        .w_full()
        .h(38.0)
        .gap(4.0)
        .items_start()
        // Shell removes the region gap when no results intervene. The active
        // item's extra point overlaps and hides the card's top border.
        .mt(if has_results { 0.0 } else { 12.0 })
        .child(strip.role("tablist").focus_group())
        .child(row().h(38.0).center().child(add))
}

/// The stable viewport key for one tab's Input, Output or Tree display.
pub(crate) fn document_scroll_key(id: u64, view: TabView, tree_open: bool) -> String {
    let view = if tree_open {
        "tree"
    } else {
        match view {
            TabView::Input => "input",
            TabView::Output => "output",
        }
    };
    format!("document-scroll-{id}-{view}")
}

/// Input or output for the active tab, plus its validation alert/panel.
pub fn tab_content(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let Some(tab) = state.active_tab() else {
        return col().w_full().grow(1.0).fill(PANEL_BG);
    };
    // The tree explorer is a third display mode held outside `TabView` (its
    // open flag lives in `AppState::tree`); when it is on, it replaces the
    // code text area entirely.
    let body = if state.tree.get(&tab.id).is_some_and(|tree| tree.open) {
        super::tree_view::tree_body(state, tab.id)
    } else {
        match tab.view {
            TabView::Input => {
                // The rope editor (rp-editor): editable at every size, no
                // native TextEdit and no read-only size/long-line gates.
                crate::editor::view::build(state, cx, tab, &format!("editor-{}", tab.id))
            }
            TabView::Output => TextView::read_only(&tab.output)
                .revision(tab.output_revision)
                .line_numbers(true)
                .visible_lines(200)
                .highlight(&tab.search_query)
                .build("output-text")
                .w_full()
                .grow(1.0)
                .fill(PANEL_BG),
        }
    };
    let scroll_key = document_scroll_key(
        tab.id,
        tab.view,
        state.tree.get(&tab.id).is_some_and(|tree| tree.open),
    );
    let offsets = state.scroll_offsets.clone();
    let offset_key = scroll_key.clone();
    let scroller = col()
        .key(scroll_key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(PANEL_BG)
        .scroll_xy()
        .on_scroll_offset(move |at| {
            let mut offsets = offsets.borrow_mut();
            if let Some(offset) = offsets.get_mut(offset_key.as_str()) {
                *offset = at.offset[1];
            } else {
                offsets.insert(offset_key.clone(), at.offset[1]);
            }
        })
        .child(body);

    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(PANEL_BG)
        .child(super::validation_panel::validation_panel(state, cx))
        .child(scroller)
}
