//! Platform I/O: native file dialogs, file reads and the clipboard.
//!
//! Dialogs and the clipboard are blocking calls and must run on the UI/runner
//! thread in the scaffold. `json_ops` and `search` are the only background
//! workers for now.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why a file or clipboard operation failed.
#[derive(Debug, Error)]
pub enum IoError {
    /// Reading or writing a file failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The clipboard backend failed (no display, denied, unsupported).
    #[error("clipboard error: {0}")]
    Clipboard(String),
    /// The dropped file is not JSON.
    #[error("not a JSON file: {}", .0.display())]
    NotJson(PathBuf),
}

/// Reads a JSON file into a `String`.
///
/// The extension check mirrors `FileDropzone.tsx`: `.json` only, compared
/// case-insensitively. UTF-8 is required.
pub fn read_json_file(path: &Path) -> Result<String, IoError> {
    let is_json = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    if !is_json {
        return Err(IoError::NotJson(path.to_path_buf()));
    }
    Ok(fs::read_to_string(path)?)
}

/// Opens the native open dialog for a JSON file and reads it.
///
/// Returns `None` when the user cancels or the file cannot be read. Use
/// [`read_json_file`] when the error matters (for example after a drop).
pub fn pick_open_file() -> Option<(PathBuf, String)> {
    let path = rfd::FileDialog::new()
        .set_title("Open JSON")
        .add_filter("JSON", &["json"])
        .pick_file()?;
    let content = read_json_file(&path).ok()?;
    Some((path, content))
}

/// Opens the native save dialog for exporting JSON.
///
/// Returns `None` when the user cancels.
pub fn pick_export_path(default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Export JSON")
        .set_file_name(default_name)
        .add_filter("JSON", &["json"])
        .save_file()
}

/// Writes `content` to `path`, replacing an existing file.
pub fn write_file(path: &Path, content: &str) -> Result<(), IoError> {
    fs::write(path, content)?;
    Ok(())
}

/// Copies plain text to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<(), IoError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| IoError::Clipboard(error.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|error| IoError::Clipboard(error.to_string()))
}

/// Entry point for a file dropped on the window: validates and reads it.
pub fn on_file_dropped(path: &Path) -> Result<String, IoError> {
    read_json_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_json_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.JSON");
        fs::write(&path, r#"{"a":1}"#).unwrap();
        assert_eq!(on_file_dropped(&path).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn rejects_non_json_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "{}").unwrap();
        assert!(matches!(on_file_dropped(&path), Err(IoError::NotJson(_))));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(matches!(on_file_dropped(&path), Err(IoError::Io(_))));
    }

    #[test]
    fn write_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_file(&path, "{\"ok\":true}").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "{\"ok\":true}");
    }
}
