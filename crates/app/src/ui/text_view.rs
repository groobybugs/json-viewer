//! Virtualized text component.
//!
//! Rune's [`TextEdit`] owns editable text and the caret; for read-only output
//! we build only the lines in the requested window. `first_line` and
//! `visible_lines` are the virtualization contract: a caller tracking the
//! scroll viewport can pass the visible range, so a 100k-line document costs
//! only the rows it shows.
//!
//! ```ignore
//! TextView::read_only(&tab.output)
//!     .first_line(scroll_line)
//!     .visible_lines(40)
//!     .line_numbers(true)
//!     .build("output")
//! ```

use std::ops::Range;

use rune::{col, oklch, row, text, tok, El, TextEdit};

/// How the text is edited.
pub enum TextViewMode<'a> {
    /// Editable through Rune's text input. The caret and selection live in
    /// `TextEdit`; the app syncs `TextEdit::text` back into its model in
    /// `on_input_change`.
    Editable(&'a TextEdit),
    /// Read-only text, windowed by line index.
    ReadOnly,
}

/// A windowed text area.
pub struct TextView<'a> {
    text: &'a str,
    mode: TextViewMode<'a>,
    first_line: usize,
    visible_lines: usize,
    line_numbers: bool,
    query: Option<&'a str>,
}

impl<'a> TextView<'a> {
    /// Creates an editable view over `text`.
    ///
    /// `edit` must be the [`TextEdit`] stored in app state; Rune routes edits
    /// back to it through `.on_edit`.
    pub fn editable(text: &'a str, edit: &'a TextEdit) -> Self {
        Self {
            text,
            mode: TextViewMode::Editable(edit),
            first_line: 0,
            visible_lines: 40,
            line_numbers: false,
            query: None,
        }
    }

    /// Creates a read-only view.
    pub fn read_only(text: &'a str) -> Self {
        Self {
            text,
            mode: TextViewMode::ReadOnly,
            first_line: 0,
            visible_lines: 40,
            line_numbers: false,
            query: None,
        }
    }

    /// First visible line (0-based). Clamped to the document length.
    pub fn first_line(mut self, line: usize) -> Self {
        self.first_line = line;
        self
    }

    /// Number of rows to build. [`TextView::build`] renders exactly this many
    /// lines, never the whole document.
    pub fn visible_lines(mut self, lines: usize) -> Self {
        self.visible_lines = lines;
        self
    }

    /// Draws a right-aligned line-number gutter in read-only mode.
    pub fn line_numbers(mut self, on: bool) -> Self {
        self.line_numbers = on;
        self
    }

    /// Tints lines containing `query` (case-insensitive) in read-only mode.
    /// A future syntax/highlight pass can replace this with spans.
    pub fn highlight(mut self, query: &'a str) -> Self {
        self.query = Some(query);
        self
    }

    /// Number of lines; empty text counts as one empty line.
    pub fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    /// Visible line range, clamped to the document.
    pub fn visible_range(&self) -> Range<usize> {
        let count = self.line_count();
        let start = self.first_line.min(count);
        let end = start.saturating_add(self.visible_lines).min(count);
        start..end
    }

    /// Returns line `index` (0-based), or `None` past the end.
    pub fn line(&self, index: usize) -> Option<&'a str> {
        self.text.split('\n').nth(index)
    }

    /// Builds the Rune element. `key` must be unique in the window.
    ///
    /// Editable mode emits a multi-line text input; read-only mode emits one
    /// row per line in [`TextView::visible_range`].
    pub fn build(&self, key: &str) -> El {
        let theme = tok::theme(true);
        match &self.mode {
            TextViewMode::Editable(edit) => rune::text_input(edit, mono())
                .key(key.to_string())
                .label("JSON input")
                .w_full()
                .min_h(240.0)
                .padding(12.0)
                .fill(theme.card)
                .color(theme.text)
                .caret(theme.text, tok::accent(0, true)),
            TextViewMode::ReadOnly => {
                let theme = tok::theme(true);
                let output_ink = oklch(0.78, 0.13, 150.0);
                let highlight_ink = tok::accent(0, true);
                let mut lines = col()
                    .key(key.to_string())
                    .w_full()
                    .padding(12.0)
                    .fill(theme.bg);
                for index in self.visible_range() {
                    let line = self.line(index).unwrap_or_default();
                    let mut content = row().w_full().gap(12.0);
                    if self.line_numbers {
                        content = content.child(
                            text(format!("{:>5}", index + 1))
                                .font(mono())
                                .color(theme.text_secondary),
                        );
                    }
                    let tinted = self.query.is_some_and(|query| {
                        !query.is_empty() && line.to_lowercase().contains(&query.to_lowercase())
                    });
                    content = content.child(text(line).font(mono()).color(if tinted {
                        highlight_ink
                    } else {
                        output_ink
                    }));
                    lines = lines.child(content);
                }
                lines
            }
        }
    }
}

fn mono() -> tok::TextStyle {
    tok::TextStyle::new(12.0, 400.0).mono()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_clamps_to_the_document() {
        let view = TextView::read_only("a\nb\nc")
            .first_line(1)
            .visible_lines(5);
        assert_eq!(view.line_count(), 3);
        assert_eq!(view.visible_range(), 1..3);
        assert_eq!(view.line(1), Some("b"));
        assert_eq!(view.line(9), None);
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        let view = TextView::read_only("");
        assert_eq!(view.line_count(), 1);
        assert_eq!(view.line(0), Some(""));
    }

    #[test]
    fn first_line_past_the_end_is_empty() {
        let view = TextView::read_only("a\nb").first_line(10).visible_lines(5);
        assert_eq!(view.visible_range(), 2..2);
    }
}
