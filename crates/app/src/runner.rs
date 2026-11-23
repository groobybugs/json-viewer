//! Effect runner: drains messages, applies pure reducers and executes effects.
//!
//! Rune's `AppSpec::poll` is a plain function pointer, so the inbox and job
//! handles live in thread-local storage. `poll` runs once at the start of each
//! frame. Messages pushed by view handlers are applied, their effects are
//! executed, and completed background jobs wake the next frame through the
//! channel.
//!
//! This is deliberately a *stub-quality* runner: native dialogs and clipboard
//! writes still block the UI thread. Move them to a worker (or Rune's async
//! executor) when those modules get their real implementation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};

use rune::Tx;

use crate::json_ops::{self, JobHandle};
use crate::state::{AppState, Effect, Msg, TabId};
use crate::{io, persist, search};

struct Inbox {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

thread_local! {
    static INBOX: RefCell<Option<Inbox>> = const { RefCell::new(None) };
    static JOBS: RefCell<HashMap<TabId, JobHandle>> = RefCell::new(HashMap::new());
}

fn with_inbox<R>(f: impl FnOnce(&mut Inbox) -> R) -> R {
    INBOX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let (tx, rx) = mpsc::channel();
            *slot = Some(Inbox { tx, rx });
        }
        f(slot.as_mut().expect("inbox was just initialized"))
    })
}

fn sender() -> Sender<Msg> {
    with_inbox(|inbox| inbox.tx.clone())
}

/// Rune `poll` hook. Applies every queued message (view messages first, then
/// completed background jobs) and returns whether state changed.
pub fn poll(state: &mut AppState, _clock: f64, _tx: &mut Tx) -> bool {
    let mut changed = false;
    loop {
        let mut messages = state.take_pending();
        let completed: Vec<Msg> = with_inbox(|inbox| inbox.rx.try_iter().collect());
        if messages.is_empty() && completed.is_empty() {
            break;
        }
        messages.extend(completed);
        for message in messages {
            let effects = state.update(message);
            changed = true;
            execute(state, effects);
        }
    }
    changed
}

/// Executes effects returned by [`AppState::update`].
///
/// Background JSON jobs post [`Msg::JsonDone`] to the inbox. Search runs on a
/// detached thread; later workers should add a cancel handle per tab.
pub fn execute(state: &mut AppState, effects: Vec<Effect>) {
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
            Effect::Copy(text) => match io::copy_to_clipboard(&text) {
                Ok(()) => state.dispatch(Msg::Status("Copied to clipboard".into())),
                Err(error) => state.dispatch(Msg::Status(format!("Copy failed: {error}"))),
            },
            Effect::Export {
                default_name,
                content,
            } => {
                if let Some(path) = io::pick_export_path(&default_name) {
                    match io::write_file(&path, &content) {
                        Ok(()) => {
                            state.dispatch(Msg::Status(format!("Exported {}", path.display())))
                        }
                        Err(error) => {
                            state.dispatch(Msg::Status(format!("Export failed: {error}")))
                        }
                    }
                }
            }
            Effect::Open => {
                if let Some((path, content)) = io::pick_open_file() {
                    state.dispatch(Msg::FileLoaded { path, content });
                }
            }
            Effect::ReadDroppedFile(path) => match io::on_file_dropped(&path) {
                Ok(content) => state.dispatch(Msg::FileLoaded { path, content }),
                Err(error) => state.dispatch(Msg::Status(format!("Drop failed: {error}"))),
            },
        }
    }
}
