//! Platform I/O: native file dialogs, file reads and the clipboard.
//!
//! The frozen helpers [`pick_open_file`], [`pick_export_path`] and
//! [`copy_to_clipboard`] still block the caller. The runner must not use them
//! on the UI thread. It asks Rune for dialogs with [`open_dialog_options`] /
//! [`export_dialog_options`] (`Tx::open_file` / `Tx::save_file`, feature
//! `native-file-dialog`) and moves bytes with [`spawn_read_json`],
//! [`spawn_write_file`] and [`spawn_clipboard_copy`].
//!
//! Window drops go through [`drop_zone`]. rp-shell wraps its root element with
//! that function; see `NOTES-rp-io.md`. Wayland in winit 0.30 reports no file
//! drags, so the handlers only run on macOS, Windows, X11 and headless scripts.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::thread::{self, JoinHandle};

use rune::tok;
use rune::{el, text, Color, El, Tx};
use thiserror::Error;

use crate::state::{AppState, Msg};

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

/// Overlay copy while a JSON drag is over the window. Matches `FileDropzone.tsx`.
pub const DROP_HINT: &str = "Drop your JSON file here";
/// Overlay copy while a non-JSON drag is over the window.
pub const DROP_REJECT_HINT: &str = "Only .json files can be dropped";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropHover {
    None,
    Accept,
    Reject,
}

thread_local! {
    static DROP_HOVER: Cell<DropHover> = const { Cell::new(DropHover::None) };
}

fn set_drop_hover(hover: DropHover) {
    DROP_HOVER.with(|slot| slot.set(hover));
}

/// Whether a file drag is currently over the drop zone.
///
/// Hover cannot live on [`AppState`] (that struct is frozen), so the view
/// reads this thread-local. The hover handler also writes `status`, which
/// forces the shell to rebuild and pick the overlay up.
pub fn drop_hovering() -> bool {
    DROP_HOVER.with(|slot| slot.get() != DropHover::None)
}

/// `true` when `path` ends in `.json`, compared without case.
///
/// The old drop zone also accepted a `application/json` MIME type. A path has
/// no MIME, so the extension is the whole check.
pub fn is_json_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

/// UTF-8 BOM. `fs::read_to_string` keeps it as U+FEFF, which the JSON parser
/// rejects as an unexpected token at column 1.
const UTF8_BOM: char = '\u{feff}';

/// Path → status fragment for a file whose leading BOM was just stripped.
/// The read runs on a worker thread; the UI thread consumes the note from
/// [`display_name`] when it builds `Opened {name}`.
static BOM_NOTES: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn note_bom_stripped(path: &Path) {
    let note = format!("{} (UTF-8 BOM stripped)", file_name(path));
    BOM_NOTES
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(path.to_path_buf(), note);
}

/// File name used in status lines, falling back to the full path.
///
/// If [`read_json_file`] just stripped a leading UTF-8 BOM from this path, the
/// returned name notes that so the existing `Opened {name}` status line shows
/// it. The note is one-shot.
pub fn display_name(path: &Path) -> String {
    if let Some(note) = BOM_NOTES
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(path)
    {
        return note;
    }
    file_name(path)
}

/// Status line for a non-JSON path. Same text for a rejected drop and a read.
pub fn not_json_status(path: &Path) -> String {
    format!("Not a JSON file: {}", display_name(path))
}

/// Status line for a failed read.
pub fn read_error_status(path: &Path, error: &IoError) -> String {
    match error {
        IoError::NotJson(rejected) => not_json_status(rejected),
        other => format!("Could not read {}: {other}", display_name(path)),
    }
}

/// Reads a JSON file into a `String`.
///
/// The extension check mirrors `FileDropzone.tsx`: `.json` only, compared
/// case-insensitively. UTF-8 is required. This blocks the caller; the runner
/// calls it on [`spawn_read_json`]'s worker.
pub fn read_json_file(path: &Path) -> Result<String, IoError> {
    if !is_json_path(path) {
        return Err(IoError::NotJson(path.to_path_buf()));
    }
    let text = fs::read_to_string(path)?;
    Ok(match text.strip_prefix(UTF8_BOM) {
        Some(stripped) => {
            note_bom_stripped(path);
            stripped.to_owned()
        }
        None => text,
    })
}

/// Opens the native open dialog for a JSON file and reads it.
///
/// **Blocking.** The runner must not call this on the UI thread; it uses
/// [`open_dialog_options`] with `Tx::open_file` and [`spawn_read_json`].
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
/// **Blocking.** The runner uses [`export_dialog_options`] with `Tx::save_file`
/// instead. Returns `None` when the user cancels.
pub fn pick_export_path(default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Export JSON")
        .set_file_name(default_name)
        .add_filter("JSON", &["json"])
        .save_file()
}

/// Options for Rune's async Open dialog (`Tx::open_file`).
pub fn open_dialog_options() -> rune::FileDialogOptions {
    rune::FileDialogOptions {
        title: "Open JSON".into(),
        ..rune::FileDialogOptions::default()
    }
    .filter("JSON", &["json"])
}

/// Options for Rune's async Save dialog (`Tx::save_file`).
pub fn export_dialog_options(default_name: &str) -> rune::FileDialogOptions {
    let file_name = if default_name.is_empty() {
        "export.json".to_owned()
    } else {
        default_name.to_owned()
    };
    rune::FileDialogOptions {
        title: "Export JSON".into(),
        file_name: Some(file_name),
        ..rune::FileDialogOptions::default()
    }
    .filter("JSON", &["json"])
}

/// Writes `content` to `path`, replacing an existing file.
///
/// Blocks the caller. The runner calls it on [`spawn_write_file`]'s worker.
pub fn write_file(path: &Path, content: &str) -> Result<(), IoError> {
    fs::write(path, content)?;
    Ok(())
}

/// Copies plain text to the system clipboard via arboard.
///
/// **Blocking.** Prefer [`queue_rune_copy`] from a Rune poll or handler.
/// [`spawn_clipboard_copy`] is the off-thread fallback when no [`Tx`] exists.
pub fn copy_to_clipboard(text: &str) -> Result<(), IoError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| IoError::Clipboard(error.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|error| IoError::Clipboard(error.to_string()))
}

/// Queues a clipboard write on Rune's shell. Does not block and has no error
/// callback; the shell skips the write in headless runs that have no memory
/// clipboard.
pub fn queue_rune_copy(tx: &mut Tx, text: &str) {
    tx.copy(text.to_owned());
}

/// How [`start_copy`] delivered the text.
#[derive(Debug)]
pub enum CopyQueued {
    /// [`Tx::copy`] queued. The shell writes after this frame.
    Rune,
    /// No `Tx`: the caller must run [`spawn_clipboard_copy`].
    NeedsArboard,
}

/// Prefers Rune's clipboard. `None` means the arboard worker fallback.
pub fn start_copy(tx: Option<&mut Tx>, text: &str) -> CopyQueued {
    match tx {
        Some(tx) => {
            queue_rune_copy(tx, text);
            CopyQueued::Rune
        }
        None => CopyQueued::NeedsArboard,
    }
}

/// Entry point for a file dropped on the window: validates and reads it.
///
/// Blocks the caller. The runner validates on a worker via [`spawn_read_json`].
pub fn on_file_dropped(path: &Path) -> Result<String, IoError> {
    read_json_file(path)
}

/// Reads `path` on `json-viewer-read` and invokes `on_done` there.
///
/// Returns as soon as the thread is spawned. A 100 MB file does not block the
/// caller. `on_done` runs on the worker, so it must send a [`Msg`] rather than
/// touch UI state.
pub fn spawn_read_json(
    path: PathBuf,
    on_done: impl FnOnce(Result<String, IoError>) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("json-viewer-read".into())
        .spawn(move || on_done(read_json_file(&path)))
        .expect("spawn JSON read thread")
}

/// Writes `content` on `json-viewer-write` and invokes `on_done` there.
///
/// Returns as soon as the thread is spawned.
pub fn spawn_write_file(
    path: PathBuf,
    content: String,
    on_done: impl FnOnce(Result<(), IoError>) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("json-viewer-write".into())
        .spawn(move || on_done(write_file(&path, &content)))
        .expect("spawn JSON write thread")
}

/// Copies `text` with arboard on `json-viewer-clipboard`.
///
/// Fallback for [`start_copy`] when no Rune [`Tx`] is available. Returns as
/// soon as the thread is spawned; `on_done` runs on the worker.
pub fn spawn_clipboard_copy(
    text: String,
    on_done: impl FnOnce(Result<(), IoError>) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("json-viewer-clipboard".into())
        .spawn(move || on_done(copy_to_clipboard(&text)))
        .expect("spawn clipboard thread")
}

fn hover_status(state: &mut AppState, hover: DropHover) {
    set_drop_hover(hover);
    let hint = match hover {
        DropHover::Accept => Some(DROP_HINT),
        DropHover::Reject => Some(DROP_REJECT_HINT),
        DropHover::None => None,
    };
    match hint {
        Some(text) => state.status = Some(text.to_owned()),
        None if state.status.as_deref() == Some(DROP_HINT)
            || state.status.as_deref() == Some(DROP_REJECT_HINT) =>
        {
            state.status = None;
        }
        None => {}
    }
}

fn drop_highlight(reject: bool) -> El {
    let theme = tok::theme(true);
    let label = if reject { DROP_REJECT_HINT } else { DROP_HINT };
    let border = if reject {
        Color {
            r: 0.75,
            g: 0.22,
            b: 0.18,
            a: 1.0,
        }
    } else {
        Color {
            r: 0.18,
            g: 0.45,
            b: 0.95,
            a: 1.0,
        }
    };
    el().key("drop-highlight")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(theme.bg.alpha(0.9))
        .border(2.0, border)
        .center()
        .child(
            text(label)
                .key("drop-hint")
                .font(tok::type_::TITLE)
                .color(theme.text),
        )
}

/// Makes `el` the window drop target.
///
/// rp-shell calls this on the root element (see `NOTES-rp-io.md`). Accepted
/// files are `.json` only; the first one is queued as [`Msg::FileDropped`].
/// Anything else sets a status line and does not start a read. A drag paints
/// an absolute overlay (`drop-highlight`) without taking pointer events away
/// from the buttons under the zone: drop targets do not hit-test as presses.
///
/// The wrapper sets key `json-drop-zone`. Set a different key after this call
/// if the shell needs one; hover still works.
pub fn drop_zone(el: El) -> El {
    let hover = DROP_HOVER.with(|slot| slot.get());
    let mut zone = el
        .key("json-drop-zone")
        .on_drop_files(|state: &mut AppState, files: &[PathBuf], _tx: &mut Tx| {
            hover_status(state, DropHover::None);
            if let Some(path) = files.first() {
                state.dispatch(Msg::FileDropped(path.clone()));
            }
        })
        .accepts_files(is_json_path)
        .on_drop_rejected(|state: &mut AppState, files: &[PathBuf], _tx: &mut Tx| {
            hover_status(state, DropHover::None);
            let path = files
                .first()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("file"));
            let text = not_json_status(&path);
            state.status = Some(text.clone());
            state.dispatch(Msg::Status(text));
        })
        .on_drop_hover(|state: &mut AppState, over: bool, _tx: &mut Tx| {
            hover_status(
                state,
                if over {
                    DropHover::Accept
                } else {
                    DropHover::None
                },
            );
        })
        .on_drop_rejected_hover(|state: &mut AppState, over: bool, _tx: &mut Tx| {
            hover_status(
                state,
                if over {
                    DropHover::Reject
                } else {
                    DropHover::None
                },
            );
        });
    if hover != DropHover::None {
        zone = zone
            .border(
                2.0,
                if hover == DropHover::Reject {
                    Color {
                        r: 0.75,
                        g: 0.22,
                        b: 0.18,
                        a: 1.0,
                    }
                } else {
                    Color {
                        r: 0.18,
                        g: 0.45,
                        b: 0.95,
                        a: 1.0,
                    }
                },
            )
            .child(drop_highlight(hover == DropHover::Reject));
    }
    zone
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

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
        assert_eq!(not_json_status(&path), "Not a JSON file: notes.txt");
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

    #[test]
    fn export_round_trip_returns_before_the_write_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let handle = spawn_write_file(path.clone(), r#"{"ok":true}"#.into(), move |result| {
            let _ = tx.send(result);
        });
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "export write blocked the caller"
        );
        rx.recv_timeout(Duration::from_secs(5))
            .expect("export msg")
            .unwrap();
        assert_eq!(read_json_file(&path).unwrap(), r#"{"ok":true}"#);
        let bad = dir.path().join("notes.txt");
        fs::write(&bad, "{}").unwrap();
        assert!(matches!(read_json_file(&bad), Err(IoError::NotJson(_))));
        handle.join().unwrap();
    }

    #[test]
    fn spawn_read_returns_immediately_for_a_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.json");
        fs::write(&path, "[1,2,3]").unwrap();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let handle = spawn_read_json(path, move |result| {
            let _ = tx.send(result);
        });
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            "[1,2,3]"
        );
        handle.join().unwrap();
    }

    #[test]
    fn arboard_fallback_returns_immediately() {
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let handle = spawn_clipboard_copy("json-viewer-probe".into(), move |result| {
            let _ = tx.send(result.map(|_| ()));
        });
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "clipboard fallback blocked the caller"
        );
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => {
                let _ = result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Wayland clipboard servers can stall. The contract under test
                // is that the call itself returned; do not join a stuck worker.
                return;
            }
            Err(error) => panic!("{error}"),
        }
        let _ = handle.join();
    }

    #[test]
    fn start_copy_without_tx_selects_arboard() {
        assert!(matches!(start_copy(None, "x"), CopyQueued::NeedsArboard));
    }

    #[test]
    fn dialog_options_request_json() {
        let open = open_dialog_options();
        assert_eq!(open.title, "Open JSON");
        assert_eq!(open.filters, [("JSON".into(), vec!["json".into()])]);
        let export = export_dialog_options("JSON-1.json");
        assert_eq!(export.title, "Export JSON");
        assert_eq!(export.file_name.as_deref(), Some("JSON-1.json"));
        assert_eq!(export.filters, [("JSON".into(), vec!["json".into()])]);
    }

    #[test]
    fn fifo_read_returns_before_bytes_are_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "mkfifo"
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_path = path.clone();
        let caller = thread::spawn(move || {
            let started = Instant::now();
            let handle = spawn_read_json(worker_path, move |result| {
                let _ = done_tx.send(result.map(|text| text.len()));
            });
            ready_tx.send(started.elapsed()).unwrap();
            handle
        });
        let elapsed = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| {
                let _ = fs::File::create(&path);
                panic!("spawn_read_json blocked the caller");
            });
        assert!(
            elapsed < Duration::from_millis(500),
            "read did not return immediately: {elapsed:?}"
        );
        let mut file = fs::File::create(&path).unwrap();
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..(1024 * 1024 / chunk.len()) {
            file.write_all(&chunk).unwrap();
        }
        drop(file);
        let len = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("read result")
            .unwrap();
        assert_eq!(len, 1024 * 1024);
        caller.join().unwrap().join().unwrap();
    }
}
