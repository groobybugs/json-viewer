//! Validation success and source-context error panels.

use rune::{col, icon, row, spacer, text, tok, Cx, El, TextStyle, Tx};
use rune_kit::{glyphs, Button, ButtonVariant, ControlSize};

use super::{kit, DANGER, DANGER_BG, EDITOR_BG, MUTED, SUCCESS, TEXT};
use crate::json_ops::{JsonError, ValidationInfo};
use crate::state::{AppState, Msg, TabView, ValidationResult};
use crate::ui::text_view::{TextView, EDITABLE_LIMIT_BYTES};

const CONTEXT_BYTES: usize = 40;

/// Bounded source excerpt displayed by the error panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorSnippet {
    /// The complete previous line, or a bounded prefix when it is very long.
    pub previous_line: Option<String>,
    /// About 80 bytes around the error, with ellipses for elided source.
    pub line: String,
    /// Monospace whitespace followed by `^` at the error column.
    pub caret: String,
}

/// Extracts source context without cloning the offending line.
///
/// Columns produced by `json_ops` are 1-based UTF-8 byte columns. The window
/// is measured in bytes but snapped to character boundaries, which keeps a
/// column in the millions cheap to display on a minified document.
pub fn error_snippet(input: &str, error: &JsonError) -> ErrorSnippet {
    let range = line_range(input, error.line);
    let previous_line = if range.start == 0 {
        None
    } else {
        let previous_end = range.start - 1;
        let previous_start = input[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        Some(truncate_line(
            input[previous_start..previous_end]
                .strip_suffix('\r')
                .unwrap_or(&input[previous_start..previous_end]),
        ))
    };
    let source_line = input[range.clone()]
        .strip_suffix('\r')
        .unwrap_or(&input[range]);
    let offset = floor_boundary(
        source_line,
        error.column.saturating_sub(1).min(source_line.len()),
    );
    let start = floor_boundary(source_line, offset.saturating_sub(CONTEXT_BYTES));
    let end = floor_boundary(
        source_line,
        offset.saturating_add(CONTEXT_BYTES).min(source_line.len()),
    );
    let left_elided = start > 0;
    let right_elided = end < source_line.len();
    let mut line = String::with_capacity(end - start + 2);
    if left_elided {
        line.push('…');
    }
    line.push_str(&source_line[start..end]);
    if right_elided {
        line.push('…');
    }
    let caret_column = source_line[start..offset].chars().count() + usize::from(left_elided);
    let caret = format!("{}^", " ".repeat(caret_column));
    ErrorSnippet {
        previous_line,
        line,
        caret,
    }
}

/// Draws the active tab's latest validation result, or its generic error.
pub fn validation_panel(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let Some(tab) = state.active_tab() else {
        return col();
    };
    match &tab.validation {
        Some(ValidationResult::Valid(info)) => success_alert(info),
        Some(ValidationResult::Invalid(error)) => error_panel(&tab.input, error),
        None => tab.error.as_deref().map_or_else(col, generic_error),
    }
}

fn success_alert(info: &ValidationInfo) -> El {
    row()
        .key("validation-success")
        .role("alert")
        .w_full()
        .gap(10.0)
        .padding(12.0)
        .center_y()
        .fill(SUCCESS.alpha(0.08))
        .border_sides([0.0, 0.0, 1.0, 0.0], SUCCESS.alpha(0.45))
        .child(icon(&glyphs::CHECK, 18.0).color(SUCCESS).a11y_hidden())
        .child(
            text(format!(
                "Valid JSON · {} · {} lines · depth {}",
                format_size(info.bytes),
                grouped(info.lines),
                info.max_depth
            ))
            .key("validation-success-message")
            .font(tok::type_::BODY)
            .color(SUCCESS),
        )
}

fn error_panel(input: &str, error: &JsonError) -> El {
    let kit = kit();
    let snippet = error_snippet(input, error);
    let line = error.line;
    let column = error.column;
    let go = Button::new("Go to error")
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Small)
        .on_press_tx(move |state: &mut AppState, tx: &mut Tx| {
            reveal_error(state, tx, line, column);
        })
        .build(&kit)
        .key("go-to-error");
    let mut source = col()
        .key("error-source")
        .w_full()
        .gap(2.0)
        .padding(8.0)
        .radius(6.0)
        .fill(EDITOR_BG);
    if let Some(previous) = snippet.previous_line {
        source = source
            .child(text("Previous line").font(tok::type_::LABEL).color(MUTED))
            .child(
                text(previous)
                    .key("error-previous-line")
                    .font(TextStyle::new(12.0, 400.0).mono())
                    .color(MUTED),
            );
    }
    source = source
        .child(
            text(snippet.line)
                .key("error-snippet")
                .font(TextStyle::new(12.0, 400.0).mono())
                .color(TEXT),
        )
        .child(
            text(snippet.caret)
                .key("error-caret")
                .font(TextStyle::new(12.0, 400.0).mono())
                .color(DANGER),
        );

    col()
        .key("error-alert")
        .role("alert")
        .w_full()
        .gap(8.0)
        .padding(12.0)
        .fill(DANGER_BG)
        .border_sides([0.0, 0.0, 1.0, 0.0], DANGER.alpha(0.6))
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
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
                )
                .child(spacer())
                .child(
                    text(format!("line {line}, column {column}"))
                        .key("error-location")
                        .font(tok::type_::LABEL)
                        .color(DANGER),
                )
                .child(go),
        )
        .child(source)
}

fn generic_error(error: &str) -> El {
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
            text(format!("Invalid JSON: {error}"))
                .key("error-message")
                .font(tok::type_::BODY)
                .color(TEXT),
        )
}

fn reveal_error(state: &mut AppState, tx: &mut Tx, line: usize, column: usize) {
    let id = state.active;
    let Some(tab) = state.active_tab() else {
        return;
    };
    let large = tab.input.len() > EDITABLE_LIMIT_BYTES;
    let offset = error_offset(&tab.input, line, column);
    if let Some(edit) = state.editors.get_mut(&id) {
        edit.set_caret(offset);
    }
    state.dispatch(Msg::SwitchView(TabView::Input));
    let editor_key = format!("editor-{id}");
    if large {
        // Large inputs use TextView's read-only virtual rows with this base key.
        crate::runner::queue_reveal(TextView::row_key(&editor_key, line.saturating_sub(1)));
    } else {
        // Native TextEdit reveals its caret when focused; this also scrolls the
        // document viewport to the editor after the Input view mounts.
        tx.focus_when_shown(editor_key.clone(), false);
        crate::runner::queue_reveal(editor_key);
    }
    state.dispatch(Msg::Status(format!("line {line}, column {column}")));
}

fn error_offset(input: &str, line: usize, column: usize) -> usize {
    let range = line_range(input, line);
    let start = range.start;
    let source_line = input[range.clone()]
        .strip_suffix('\r')
        .unwrap_or(&input[range]);
    start + floor_boundary(source_line, column.saturating_sub(1).min(source_line.len()))
}

fn line_range(input: &str, line: usize) -> std::ops::Range<usize> {
    let mut start = 0usize;
    for _ in 1..line.max(1) {
        let Some(newline) = input[start..].find('\n') else {
            return input.len()..input.len();
        };
        start += newline + 1;
    }
    let end = input[start..]
        .find('\n')
        .map_or(input.len(), |newline| start + newline);
    start..end
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn truncate_line(line: &str) -> String {
    if line.len() <= CONTEXT_BYTES * 2 {
        return line.to_string();
    }
    let end = floor_boundary(line, CONTEXT_BYTES * 2);
    format!("{}…", &line[..end])
}

fn format_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}
