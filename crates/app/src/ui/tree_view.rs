//! Tree explorer view: toolbar (expand/collapse/sort/filter), the virtualized
//! node list and the selection footer.
//!
//! The list is an [`El::virtual_rows`] stack over the tab's flattened
//! [`crate::tree_index::TreeRow`] list: only the mounted window is built, so
//! a fully expanded 100 MB document (millions of rows) commits in time
//! independent of the row count — `virtual_rows_version` is the
//! `(index_revision, rows_gen)` pair, so a commit never re-keys the whole
//! list. The toolbar and footer are `sticky_top`/`sticky_bottom` children of
//! the same scroll viewport the rows live in, so they stay pinned while the
//! rows scroll.
//!
//! Handlers carry **node ids**, never row indices: rows move while a message
//! sits in the queue, node ids are stable for the life of the index.

use std::borrow::Cow;

use rune::{col, el, oklch, row, spacer, text, tok, Color, El, GrowthSpec, TextStyle};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Spinner};

use super::{kit, ACCENT, BORDER, DANGER, DANGER_BG, EDITOR_BG, MUTED, TEXT, WINDOW_BG};
use crate::state::{AppState, Msg, TabId};
use crate::tree_index::{NodeKind, TreeUi};

/// Estimated height of one tree row; unvisited virtual rows use this.
pub const ROW_HEIGHT: f32 = 22.0;
/// Indentation pitch per depth level (guide line + gap).
const GUIDE_PITCH: f32 = 14.0;
/// Levels past which a row stops drawing guides and pads instead.
const MAX_GUIDES: u16 = 48;
/// Value text is elided to this many bytes per row.
const VALUE_DISPLAY_BYTES: usize = 160;

/// The app's monospace style (the mono slot is IBM Plex Mono; see AGENTS.md).
fn mono() -> TextStyle {
    tok::TextStyle::new(12.0, 400.0).mono()
}

/// Type ink for one node's value.
fn kind_ink(kind: NodeKind) -> Color {
    match kind {
        NodeKind::String => oklch(0.82, 0.12, 145.0),
        NodeKind::Number => oklch(0.84, 0.13, 65.0),
        NodeKind::Bool => oklch(0.76, 0.15, 320.0),
        NodeKind::Null => MUTED,
        NodeKind::Object | NodeKind::Array => oklch(0.66, 0.02, 260.0),
    }
}

/// Object-key ink, the same family the code view uses for keys.
fn key_ink() -> Color {
    oklch(0.72, 0.14, 250.0)
}

/// Cuts `s` to `max` bytes on a char boundary, with an ellipsis.
fn elide(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The whole tree document area for `tab`: sticky toolbar, the virtualized
/// node list, sticky selection footer. Called from `tabs::tab_content` when
/// the tab's tree is open.
pub fn tree_body(state: &AppState, tab: TabId) -> El {
    let tree = state.tree.get(&tab);
    col()
        .key(format!("tree-body-{tab}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(EDITOR_BG)
        .child(tree_toolbar(state, tab, tree).sticky_top(0.0))
        .child(tree_list(state, tab, tree))
        .child(tree_footer(tab, tree).sticky_bottom(0.0))
}

/// Expand all / Collapse all / Sort A→Z / filter box / status, pinned.
fn tree_toolbar(state: &AppState, tab: TabId, tree: Option<&TreeUi>) -> El {
    let kit = kit();
    let ready = tree.is_some_and(|t| t.is_ready());
    let filter_active = tree.is_some_and(|t| t.filter_active());
    let busy = tree.is_some_and(|t| t.job_pending);
    let sort_on = tree.is_some_and(|t| t.sort_keys);

    let expand_all = Button::new("Expand all")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .disabled(!ready || filter_active || busy)
        .on_press(move |state: &mut AppState| {
            state.dispatch(Msg::TreeExpandAll { tab });
        })
        .build(&kit)
        .key("tree-expand-all");
    let collapse_all = Button::new("Collapse all")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .disabled(!ready || filter_active || busy)
        .on_press(move |state: &mut AppState| {
            state.dispatch(Msg::TreeCollapseAll { tab });
        })
        .build(&kit)
        .key("tree-collapse-all");
    let sort = Button::new("Sort A→Z")
        .variant(if sort_on {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::Ghost
        })
        .control_size(ControlSize::Small)
        .disabled(!ready || busy)
        .on_press(move |state: &mut AppState| {
            state.dispatch(Msg::TreeSortToggled { tab });
        })
        .build(&kit)
        .key("tree-sort");

    let field = rune::text_input(&state.tree_filter_edit, tok::type_::BODY)
        .key("tree-filter")
        .label("Filter tree")
        .single_line()
        .min_w(140.0)
        .grow(1.0)
        .fill(EDITOR_BG)
        .color(TEXT)
        .placeholder("Filter keys or values…", MUTED)
        .caret(ACCENT, ACCENT)
        .on_edit(|state: &mut AppState| &mut state.tree_filter_edit)
        .on_input_change(|state: &mut AppState, _| {
            let query = state.tree_filter_edit.text.clone();
            let tab = state.active;
            state.dispatch(Msg::TreeFilterChanged { tab, query });
        })
        .on_input_key(super::handle_shortcut);

    let mut filter_row = row().grow(1.0).min_w(0.0).gap(4.0).child(field);
    if !state.tree_filter_edit.text.is_empty() {
        filter_row = filter_row.child(
            IconButton::new(kit.glyph.clear)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label("Clear filter")
                .on_press(|state: &mut AppState| {
                    state.tree_filter_edit.replace_text("");
                    let tab = state.active;
                    state.dispatch(Msg::TreeFilterChanged {
                        tab,
                        query: String::new(),
                    });
                })
                .build(&kit)
                .key("tree-clear-filter"),
        );
    }

    // Status: job spinner, filter match count, or the row count.
    let status = if busy {
        let label = tree.map(|t| t.job_kind).unwrap_or("Working…");
        row()
            .gap(6.0)
            .center_y()
            .child(
                Spinner::new()
                    .size(13.0)
                    .color(MUTED)
                    .build(&kit)
                    .key("tree-job-spinner"),
            )
            .child(text(label).font(tok::type_::LABEL).color(MUTED))
            .key("tree-job-status")
    } else if filter_active {
        let matched = tree.and_then(|t| t.filter_matched).unwrap_or(0);
        text(if matched == 0 {
            "No matches".into()
        } else {
            format!("{matched} match{}", if matched == 1 { "" } else { "es" })
        })
        .key("tree-filter-count")
        .font(TextStyle::new(12.0, 500.0))
        .color(if matched == 0 { MUTED } else { TEXT })
    } else {
        let rows = tree.map_or(0, |t| t.rows.len());
        text(format!("{rows} rows"))
            .key("tree-row-count")
            .font(tok::type_::LABEL)
            .color(MUTED)
    };

    row()
        .key("tree-toolbar")
        .w_full()
        .h(44.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .fill(WINDOW_BG)
        .border_sides([0.0, 0.0, 1.0, 0.0], BORDER)
        .child(expand_all)
        .child(collapse_all)
        .child(sort)
        .child(filter_row)
        .child(status)
}

/// The node list, or whatever stands in for it (spinner, error, hint).
fn tree_list(_state: &AppState, tab: TabId, tree: Option<&TreeUi>) -> El {
    let Some(tree) = tree else {
        return placeholder("Building tree…", true);
    };
    if tree.building {
        return placeholder("Building tree…", true);
    }
    if let Some(error) = &tree.error {
        return tree_error(error);
    }
    let Some(index) = tree.index.as_deref() else {
        return placeholder("Building tree…", true);
    };
    let _ = index;
    if tree.rows.is_empty() {
        if tree.filter_active() {
            return placeholder(&format!("No matches for “{}”", tree.filter), false);
        }
        // An index only exists for non-empty input; this is defensive.
        return placeholder("Nothing to explore — paste JSON in the Input view.", false);
    }
    let count = tree.rows.len();
    let version = tree.rows_version();
    col()
        .key(format!("tree-rows-{tab}"))
        .w_full()
        .growth(GrowthSpec::default())
        .role("tree")
        .focus_group()
        .virtual_rows::<AppState>(
            count,
            ROW_HEIGHT,
            move |s: &AppState, i| Cow::Owned(row_key(s, tab, i)),
            move |s: &AppState, i| render_row(s, tab, i),
        )
        .virtual_rows_version(version)
}

/// Virtual-row key: stable per node, so splices keep rows (and focus) where
/// they are.
fn row_key(s: &AppState, tab: TabId, i: usize) -> String {
    s.tree
        .get(&tab)
        .and_then(|tree| tree.rows.get(i))
        .map(|row| format!("tree-row-{}", row.node))
        .unwrap_or_else(|| format!("tree-row-empty-{i}"))
}

/// Builds visible row `i`. Reads the live state (never a captured index):
/// rows move, node ids do not.
fn render_row(s: &AppState, tab: TabId, i: usize) -> El {
    let kit = kit();
    let Some(tree) = s.tree.get(&tab) else {
        return el();
    };
    let Some(entry) = tree.rows.get(i) else {
        return el();
    };
    let Some(index) = tree.index.as_deref() else {
        return el();
    };
    let input = s.tab(tab).map(|t| t.input.as_str()).unwrap_or("");
    let kind = index.kind(entry.node);
    let container = kind.is_container();
    let selected = tree
        .selected
        .as_ref()
        .is_some_and(|selection| selection.node == entry.node);

    // Key column: `"key":` for object members, `[index]:` for array
    // elements, nothing for the root.
    let key_part: Option<String> = if entry.depth == 0 {
        None
    } else {
        match index.key_text(input, entry.node) {
            Some(key) => Some(format!("{}:", crate::tree_index::display_string(&key))),
            None => Some(format!("[{}]:", entry.child_index)),
        }
    };

    // Value column: `{n}` / `[n]` for containers (bare for empty), the raw
    // token for scalars.
    let value_part = if container {
        let count = index.child_count(entry.node);
        if count == 0 {
            if kind == NodeKind::Object { "{}".into() } else { "[]".into() }
        } else if kind == NodeKind::Object {
            format!("{{{count}}}")
        } else {
            format!("[{count}]")
        }
    } else {
        elide(index.value_text(input, entry.node), VALUE_DISPLAY_BYTES)
    };

    // The full line, for the element tree / accessibility (and the tests).
    let line = match &key_part {
        Some(key) => format!("{key} {value_part}"),
        None => {
            if container {
                format!("{} {value_part}", kind.display())
            } else {
                value_part.clone()
            }
        }
    };

    // Indentation guides, then the arrow (or its placeholder).
    let mut content = row().gap(7.0).center_y();
    for _ in 0..entry.depth.min(MAX_GUIDES) {
        content = content
            .child(el().w(1.0).h(14.0).fill(BORDER.alpha(0.7)))
            .child(el().w(GUIDE_PITCH - 1.0));
    }
    if entry.depth > MAX_GUIDES {
        content = content.child(el().w((entry.depth - MAX_GUIDES) as f32 * GUIDE_PITCH));
    }
    let node = entry.node;
    let arrow = if container {
        let glyph = if entry.expanded { "▾" } else { "▸" };
        let toggle = if entry.expanded { "Collapse" } else { "Expand" };
        let expanded = entry.expanded;
        let arrow_label = match &key_part {
            Some(key) => format!("{toggle} {key}"),
            None => format!("{toggle} {}", kind.display()),
        };
        text(glyph)
            .w(14.0)
            .color(if expanded { TEXT } else { MUTED })
            .role("button")
            .label(arrow_label)
            .focusable(true)
            .on_press(move |state: &mut AppState| {
                state.dispatch(if expanded {
                    Msg::TreeCollapse { tab, node }
                } else {
                    Msg::TreeExpand { tab, node }
                });
            })
            .key(format!("tree-toggle-{node}"))
    } else {
        el().w(14.0)
    };
    content = content.child(arrow);
    if let Some(key) = &key_part {
        content = content.child(
            text(key.clone())
                .font(mono())
                .color(if index.key_text(input, entry.node).is_some() {
                    key_ink()
                } else {
                    MUTED
                })
                .selectable(true),
        );
    }
    content = content.child(
        text(value_part)
            .font(mono())
            .color(kind_ink(kind))
            .selectable(true),
    );

    let row_el = row()
        .key(format!("tree-node-{node}"))
        .role("treeitem")
        .label(line)
        .focusable(true)
        .focus_selected(selected)
        .w_full()
        .h(ROW_HEIGHT)
        .px(12.0)
        .center_y()
        .on_press(move |state: &mut AppState| {
            state.dispatch(Msg::TreeSelect { tab, node });
        })
        .child(content);
    let row_el = if selected {
        row_el
            .fill(ACCENT.alpha(0.14))
            .border_sides([2.0, 0.0, 0.0, 0.0], ACCENT)
    } else {
        row_el
    };
    kit.interactive(row_el)
}

/// Selected node's path plus Copy path / Copy value, pinned to the bottom.
fn tree_footer(tab: TabId, tree: Option<&TreeUi>) -> El {
    let kit = kit();
    let selection = tree.and_then(|t| t.selected.as_ref());
    let path_el = match selection {
        Some(selection) => text(selection.path.clone())
            .font(mono())
            .color(TEXT)
            .key("tree-selected-path")
            .selectable(true),
        None => text("Select a node to see its path")
            .font(tok::type_::LABEL)
            .color(MUTED),
    };
    let copy_path = Button::new("Copy path")
        .variant(ButtonVariant::Contrast)
        .control_size(ControlSize::Small)
        .disabled(selection.is_none())
        .on_press(|state: &mut AppState| state.dispatch(Msg::TreeCopyPath))
        .build(&kit)
        .key("tree-copy-path");
    let copy_value = Button::new("Copy value")
        .variant(ButtonVariant::Contrast)
        .control_size(ControlSize::Small)
        .disabled(selection.is_none())
        .on_press(|state: &mut AppState| state.dispatch(Msg::TreeCopyValue))
        .build(&kit)
        .key("tree-copy-value");
    row()
        .key(format!("tree-footer-{tab}"))
        .w_full()
        .h(32.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .fill(WINDOW_BG)
        .border_sides([0.0, 1.0, 0.0, 0.0], BORDER)
        .child(text("Path").font(tok::type_::LABEL).color(MUTED))
        .child(path_el)
        .child(spacer())
        .child(copy_path)
        .child(copy_value)
}

/// Centered stand-in while building, or for hints.
fn placeholder(label: &str, spinner: bool) -> El {
    let kit = kit();
    let mut content = row().gap(10.0).center_y();
    if spinner {
        content = content.child(
            Spinner::new()
                .size(16.0)
                .color(MUTED)
                .build(&kit)
                .key("tree-building-spinner"),
        );
    }
    content = content.child(text(label).font(tok::type_::BODY).color(MUTED));
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .child(content)
}

/// Parse failure: the tree needs valid JSON.
fn tree_error(error: &str) -> El {
    use rune::icon;
    use rune_kit::glyphs;
    row()
        .key("tree-error-alert")
        .role("alert")
        .w_full()
        .gap(10.0)
        .padding(12.0)
        .center_y()
        .fill(DANGER_BG)
        .border_sides([0.0, 0.0, 1.0, 0.0], DANGER.alpha(0.6))
        .child(icon(&glyphs::CIRCLE_X, 18.0).color(DANGER).a11y_hidden())
        .child(
            col().gap(2.0).child(
                text("Cannot show tree")
                    .font(TextStyle::new(12.0, 600.0))
                    .color(DANGER),
            ).child(
                text(error)
                    .key("tree-error-message")
                    .font(tok::type_::BODY)
                    .color(TEXT),
            ),
        )
}
