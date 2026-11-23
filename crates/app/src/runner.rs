//! Effect runner: drains messages, applies pure reducers and executes effects.
//!
//! Rune's `AppSpec::poll` is a plain function pointer, so the inbox and job
//! handles live in thread-local storage. `poll` runs once at the start of each
//! frame. Messages pushed by view handlers are applied, their effects are
//! executed, and completed background jobs wake the next frame through
//! `Tx::wake_after` while an I/O job is still in flight.
//!
//! Open, export, copy and dropped-file reads do not block this thread. Dialogs
//! are Rune `Tx::open_file` / `Tx::save_file` (feature `native-file-dialog`).
//! Bytes move on `json-viewer-read` / `json-viewer-write`. Copy prefers
//! `Tx::copy`; [`crate::io::spawn_clipboard_copy`] is the arboard fallback when
//! [`crate::io::start_copy`] is given no `Tx`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use rune::Tx;

use crate::io::{self, CopyQueued};
use crate::json_ops::{self, JobHandle};
use crate::state::{AppState, Effect, Msg, TabId};
use crate::{persist, search};

struct Inbox {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    io_tx: Sender<Msg>,
    io_rx: Receiver<Msg>,
}

thread_local! {
    static INBOX: RefCell<Option<Inbox>> = const { RefCell::new(None) };
    static JOBS: RefCell<HashMap<TabId, JobHandle>> = RefCell::new(HashMap::new());
    /// I/O jobs spawned on this UI thread that have not yet delivered a `Msg`.
    static IO_JOBS: Cell<usize> = const { Cell::new(0) };
}

fn with_inbox<R>(f: impl FnOnce(&mut Inbox) -> R) -> R {
    INBOX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let (tx, rx) = mpsc::channel();
            let (io_tx, io_rx) = mpsc::channel();
            *slot = Some(Inbox {
                tx,
                rx,
                io_tx,
                io_rx,
            });
        }
        f(slot.as_mut().expect("inbox was just initialized"))
    })
}

fn sender() -> Sender<Msg> {
    with_inbox(|inbox| inbox.tx.clone())
}

fn io_sender() -> Sender<Msg> {
    with_inbox(|inbox| inbox.io_tx.clone())
}

fn track_io_job() {
    IO_JOBS.with(|jobs| jobs.set(jobs.get() + 1));
}

fn take_io_messages() -> Vec<Msg> {
    let messages: Vec<Msg> = with_inbox(|inbox| inbox.io_rx.try_iter().collect());
    if !messages.is_empty() {
        IO_JOBS.with(|jobs| jobs.set(jobs.get().saturating_sub(messages.len())));
    }
    messages
}

fn io_pending() -> bool {
    IO_JOBS.with(|jobs| jobs.get() > 0)
}

fn spawn_read(path: std::path::PathBuf) {
    track_io_job();
    let tx = io_sender();
    let reported = path.clone();
    io::spawn_read_json(path, move |result| {
        let msg = match result {
            Ok(content) => Msg::FileLoaded {
                path: reported,
                content,
            },
            Err(error) => Msg::Status(io::read_error_status(&reported, &error)),
        };
        let _ = tx.send(msg);
    });
}

fn spawn_write(path: std::path::PathBuf, content: String) {
    track_io_job();
    let tx = io_sender();
    let reported = path.clone();
    io::spawn_write_file(path, content, move |result| {
        let msg = match result {
            Ok(()) => Msg::Status(format!("Exported {}", reported.display())),
            Err(error) => Msg::Status(format!("Export failed: {error}")),
        };
        let _ = tx.send(msg);
    });
}

fn queue_arboard_copy(text: String) {
    track_io_job();
    let tx = io_sender();
    io::spawn_clipboard_copy(text, move |result| {
        let msg = match result {
            Ok(()) => Msg::Status("Copied to clipboard".into()),
            Err(error) => Msg::Status(format!("Copy failed: {error}")),
        };
        let _ = tx.send(msg);
    });
}

fn on_open_picked(state: &mut AppState, result: rune::FileDialogResult, tx: &mut Tx) {
    match result {
        Ok(Some(path)) => {
            state.dispatch(Msg::Status(format!(
                "Reading {}...",
                io::display_name(&path)
            )));
            spawn_read(path);
            tx.wake_after(Duration::from_millis(16));
        }
        Ok(None) => {}
        Err(error) => {
            state.dispatch(Msg::Status(format!("Could not open file: {error}")));
            tx.wake_after(Duration::from_millis(16));
        }
    }
}

fn on_export_picked(
    state: &mut AppState,
    result: rune::FileDialogResult,
    tx: &mut Tx,
    content: String,
) {
    match result {
        Ok(Some(path)) => {
            state.dispatch(Msg::Status(format!(
                "Exporting {}...",
                io::display_name(&path)
            )));
            spawn_write(path, content);
            tx.wake_after(Duration::from_millis(16));
        }
        Ok(None) => {}
        Err(error) => {
            state.dispatch(Msg::Status(format!("Could not export file: {error}")));
            tx.wake_after(Duration::from_millis(16));
        }
    }
}

/// Rune `poll` hook. Applies every queued message (view messages first, then
/// completed background jobs) and returns whether state changed.
pub fn poll(state: &mut AppState, _clock: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    loop {
        let mut messages = state.take_pending();
        let completed: Vec<Msg> = with_inbox(|inbox| inbox.rx.try_iter().collect());
        let io_done = take_io_messages();
        if messages.is_empty() && completed.is_empty() && io_done.is_empty() {
            break;
        }
        messages.extend(completed);
        messages.extend(io_done);
        for message in messages {
            let opened = match &message {
                Msg::FileLoaded { path, .. } => Some(io::display_name(path)),
                _ => None,
            };
            let effects = state.update(message);
            changed = true;
            execute(state, tx, effects);
            if let Some(name) = opened {
                state.dispatch(Msg::Status(format!("Opened {name}")));
            }
        }
    }
    if io_pending() {
        tx.wake_after(Duration::from_millis(16));
    }
    changed
}

/// Executes effects returned by [`AppState::update`].
///
/// Background JSON jobs post [`Msg::JsonDone`] to the inbox. Search runs on a
/// detached thread. Open, export, copy and dropped-file reads return before
/// any dialog or disk I/O finishes.
pub fn execute(state: &mut AppState, tx: &mut Tx, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::SpawnJson { tab, input, action } => {
                let tx = sender();
                let handle = json_ops::spawn_process(input, action, move |result| {
                    let _ = tx.send(Msg::JsonDone { tab, result });
                });
                JOBS.with(|jobs| {
                    let mut jobs = jobs.borrow_mut();
                    if let Some(mut previous) = jobs.remove(&tab) {
                        previous.cancel();
                    }
                    jobs.insert(tab, handle);
                });
            }
            Effect::Search { tab, input, query } => {
                let tx = sender();
                std::thread::Builder::new()
                    .name("json-viewer-search".into())
                    .spawn(move || {
                        let cancel = AtomicBool::new(false);
                        let hits = search::search(
                            &input,
                            &query,
                            search::SearchOptions::default(),
                            &cancel,
                        );
                        let _ = tx.send(Msg::SearchDone { tab, hits });
                    })
                    .expect("spawn search worker thread");
            }
            Effect::Persist => {
                if let Err(error) = persist::save_tabs(&state.tabs) {
                    state.dispatch(Msg::Status(format!("Could not save tabs: {error}")));
                }
            }
            Effect::Copy(text) => match io::start_copy(Some(tx), &text) {
                CopyQueued::Rune => {
                    state.dispatch(Msg::Status("Copied to clipboard".into()));
                }
                CopyQueued::NeedsArboard => {
                    queue_arboard_copy(text);
                    tx.wake_after(Duration::from_millis(16));
                }
            },
            Effect::Export {
                default_name,
                content,
            } => {
                let options = io::export_dialog_options(&default_name);
                tx.save_file(options, move |state: &mut AppState, result, tx| {
                    on_export_picked(state, result, tx, content);
                });
            }
            Effect::Open => {
                tx.open_file(
                    io::open_dialog_options(),
                    |state: &mut AppState, result, tx| {
                        on_open_picked(state, result, tx);
                    },
                );
            }
            Effect::ReadDroppedFile(path) => {
                state.dispatch(Msg::Status(format!(
                    "Reading {}...",
                    io::display_name(&path)
                )));
                spawn_read(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use rune::{col, text, Cx, El, Env, Runtime};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn io_view(state: &AppState, _cx: &mut Cx<AppState>) -> El {
        let status = state.status.clone().unwrap_or_else(|| "ready".into());
        io::drop_zone(
            col()
                .w_full()
                .h_full()
                .child(text(status).key("status").font(rune::tok::type_::LABEL)),
        )
    }

    fn runtime() -> Runtime<AppState> {
        let mut rt = Runtime::new(
            AppState::default(),
            io_view,
            (400.0, 300.0),
            Env::new(true, 1.0),
        );
        rt.poll = Some(poll);
        rt.frame(0.0);
        rt
    }

    fn pump_until(rt: &mut Runtime<AppState>, mut done: impl FnMut(&AppState) -> bool) {
        let started = Instant::now();
        for i in 0..400 {
            rt.frame(1.0 / 60.0);
            if done(&rt.state) {
                return;
            }
            if i > 2 {
                std::thread::sleep(Duration::from_millis(5));
            }
            if started.elapsed() > Duration::from_secs(8) {
                break;
            }
        }
        panic!(
            "timed out: status={:?} input_len={}",
            rt.state.status,
            rt.state
                .active_tab()
                .map(|tab| tab.input.len())
                .unwrap_or(0)
        );
    }

    /// Applies one Rune script event of the shape `--script` accepts.
    fn apply_script_event(rt: &mut Runtime<AppState>, event: &serde_json::Value) {
        if event.get("drag_files").and_then(|spec| spec.as_str()) == Some("leave") {
            rt.drag_files_leave();
            return;
        }
        if let Some(spec) = event.get("drop_files").or_else(|| event.get("drag_files")) {
            let paths = spec["paths"]
                .as_array()
                .expect("paths")
                .iter()
                .map(|path| PathBuf::from(path.as_str().expect("path string")))
                .collect();
            let at = spec["at"].as_array().map(|at| {
                (
                    at[0].as_f64().unwrap_or(0.0) as f32,
                    at[1].as_f64().unwrap_or(0.0) as f32,
                )
            });
            if event.get("drop_files").is_some() {
                rt.drop_files(paths, at);
            } else {
                rt.drag_files(paths, at);
            }
            return;
        }
        if event.get("type").and_then(|kind| kind.as_str()) == Some("file-dialog") {
            let path = event
                .get("path")
                .and_then(|path| path.as_str())
                .map(PathBuf::from);
            rt.resolve_file_dialog(path).expect("answer file dialog");
        }
    }

    #[test]
    fn hundred_mb_drop_read_returns_immediately_and_delivers_msg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.json");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success());

        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let effect_path = path.clone();
        let ui = std::thread::spawn(move || {
            let mut state = AppState::default();
            let mut tx = Tx::default();
            let started = Instant::now();
            execute(
                &mut state,
                &mut tx,
                vec![Effect::ReadDroppedFile(effect_path)],
            );
            ready_tx.send(started.elapsed()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let mut tx = Tx::default();
                poll(&mut state, 0.0, &mut tx);
                let len = state.active_tab().map(|tab| tab.input.len()).unwrap_or(0);
                if len == 100 * 1024 * 1024 {
                    done_tx.send(Ok((len, state.status.clone()))).unwrap();
                    return;
                }
                if Instant::now() > deadline {
                    done_tx
                        .send(Err(format!("status={:?} len={len}", state.status)))
                        .unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let elapsed = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| {
                let _ = std::fs::File::create(&path);
                panic!("ReadDroppedFile blocked the UI thread");
            });
        assert!(
            elapsed < Duration::from_secs(1),
            "100 MB read did not return immediately: {elapsed:?}"
        );

        let mut file = std::fs::File::create(&path).unwrap();
        let chunk = vec![b'a'; 1024 * 1024];
        for _ in 0..100 {
            file.write_all(&chunk).unwrap();
        }
        drop(file);

        let (len, status) = done_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("FileLoaded msg")
            .expect("read failed");
        assert_eq!(len, 100 * 1024 * 1024);
        assert_eq!(status.as_deref(), Some("Opened big.json"));
        ui.join().unwrap();
    }

    #[test]
    fn headless_script_drops_json_and_rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("dropped.json");
        let text_path = dir.path().join("notes.txt");
        std::fs::write(&json_path, r#"{"dropped":true}"#).unwrap();
        std::fs::write(&text_path, "nope").unwrap();

        let mut rt = runtime();
        let hover = serde_json::json!({
            "t": 0.0,
            "drag_files": {"paths": [json_path], "at": [24.0, 24.0]}
        });
        apply_script_event(&mut rt, &hover);
        rt.frame(1.0 / 60.0);
        assert!(io::drop_hovering(), "drag hover highlight");
        assert_eq!(rt.state.status.as_deref(), Some(io::DROP_HINT));
        assert!(
            rt.elements().iter().any(|element| {
                element.key.as_deref() == Some("drop-highlight")
                    || element.text.as_deref() == Some(io::DROP_HINT)
            }),
            "overlay missing: {:?}",
            rt.elements()
                .iter()
                .map(|element| { (element.key.clone(), element.text.clone(), element.role,) })
                .collect::<Vec<_>>()
        );

        let script = serde_json::json!([{
            "t": 0.1,
            "drop_files": {"paths": [json_path], "at": [24.0, 24.0]}
        }]);
        apply_script_event(&mut rt, &script[0]);
        pump_until(&mut rt, |state| {
            state
                .active_tab()
                .is_some_and(|tab| tab.input == r#"{"dropped":true}"#)
        });
        assert_eq!(
            rt.state.status.as_deref(),
            Some("Opened dropped.json"),
            "FileLoaded should land in the active tab"
        );

        let reject = serde_json::json!({
            "t": 0.2,
            "drop_files": {"paths": [text_path], "at": [24.0, 24.0]}
        });
        apply_script_event(&mut rt, &reject);
        rt.frame(1.0 / 60.0);
        assert_eq!(
            rt.state.status.as_deref(),
            Some("Not a JSON file: notes.txt")
        );
        assert_eq!(
            rt.state.active_tab().map(|tab| tab.input.as_str()),
            Some(r#"{"dropped":true}"#),
            "a rejected drop must not replace the document"
        );
    }

    #[test]
    fn headless_open_and_export_round_trip_through_scripted_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("in.json");
        let exported = dir.path().join("out.json");
        std::fs::write(&source, r#"{"n":1}"#).unwrap();

        let mut rt = runtime();
        let clip = rune::clipboard::memory();
        rt.use_clipboard(&clip);

        rt.state.dispatch(Msg::OpenFile);
        rt.frame(1.0 / 60.0);
        let (kind, options) = rt.file_dialog().expect("open dialog requested");
        assert_eq!(kind, rune::FileDialogKind::Open);
        assert_eq!(options.title, "Open JSON");
        let answer = serde_json::json!({"type": "file-dialog", "path": source});
        apply_script_event(&mut rt, &answer);
        pump_until(&mut rt, |state| {
            state
                .active_tab()
                .is_some_and(|tab| tab.input == r#"{"n":1}"#)
        });

        rt.state.dispatch(Msg::Copy);
        rt.frame(1.0 / 60.0);
        let copied = clip.text().map(|text| text.to_string());
        assert_eq!(copied.as_deref(), Some(r#"{"n":1}"#));
        assert_eq!(rt.state.status.as_deref(), Some("Copied to clipboard"));

        rt.state.dispatch(Msg::Export);
        rt.frame(1.0 / 60.0);
        let (kind, options) = rt.file_dialog().expect("save dialog requested");
        assert_eq!(kind, rune::FileDialogKind::Save);
        assert_eq!(options.file_name.as_deref(), Some("JSON-1.json"));
        rt.resolve_file_dialog(Some(exported.clone())).unwrap();
        pump_until(&mut rt, |state| {
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("out.json"))
        });
        assert_eq!(
            std::fs::read_to_string(&exported).unwrap(),
            r#"{"n":1}"#,
            "export round trip"
        );
    }
}
