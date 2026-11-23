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
//!
//! [`Effect::Persist`] is debounced (~500ms after the last change) and saved on
//! a single `json-viewer-persist` thread. [`on_close`] / [`on_quit`] flush the
//! latest snapshot before the process can exit. `rune::run` does not take those
//! hooks; wire them with `App::new(spec).on_close(on_close).on_quit(on_quit)`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rune::Tx;

use crate::io::{self, CopyQueued};
use crate::json_ops::{self, JobHandle};
#[cfg(not(test))]
use crate::persist;
use crate::search;
use crate::state::{AppState, Effect, Msg, Tab, TabId};

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
    /// Key of a row to reveal on the next idle poll. The view for the
    /// requested document must be mounted first, so the reveal waits one poll
    /// after the message that switched views.
    static PENDING_REVEAL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Queues a one-shot [`Tx::scroll_into_view`] for a row that is about to
/// mount, for example a search hit in the Output view.
pub fn queue_reveal(key: String) {
    PENDING_REVEAL.with(|slot| *slot.borrow_mut() = Some(key));
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

/// How long after the last [`Effect::Persist`] before a save is started.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);
/// Bytes of tab text copied on the UI thread per poll.
///
/// A full `Vec<Tab>` clone of a 100MB string measured 37.7ms on this machine,
/// over the 30ms budget, so the hot path never clones the document in one
/// shot. One 4MB step measured 2.3ms.
const PERSIST_CHUNK: usize = 4 * 1024 * 1024;

type SaveHook = Arc<dyn Fn(&[Tab]) -> Result<(), String> + Send + Sync>;

struct PersistJob {
    tabs: Vec<Tab>,
    gen: u64,
}

struct PersistAck {
    gen: u64,
    error: Option<String>,
}

struct PersistSlot {
    job: Option<PersistJob>,
    stop: bool,
}

struct PersistShared {
    slot: Mutex<PersistSlot>,
    wake: Condvar,
    save: Mutex<SaveHook>,
}

#[derive(Clone, Copy)]
enum CopyField {
    Input,
    Output,
}

struct CopyCursor {
    dest: Vec<Tab>,
    tab: usize,
    field: CopyField,
    offset: usize,
    gen: u64,
}

enum CopyStep {
    Continue,
    Done,
    Restart,
}

struct PersistCtrl {
    shared: Arc<PersistShared>,
    ack_rx: Receiver<PersistAck>,
    join: Option<JoinHandle<()>>,
    dirty: bool,
    last_change: Instant,
    debounce: Duration,
    /// Bumped on every [`Effect::Persist`]. An in-flight chunked copy restarts
    /// when this moves, so a newer edit wins.
    edit_gen: u64,
    copy: Option<CopyCursor>,
    next_gen: u64,
    acked: u64,
}

thread_local! {
    static PERSIST: RefCell<Option<PersistCtrl>> = const { RefCell::new(None) };
    static QUIT_FLUSHED: Cell<bool> = const { Cell::new(false) };
}

impl Drop for PersistCtrl {
    fn drop(&mut self) {
        {
            let mut slot = self
                .shared
                .slot
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            slot.stop = true;
        }
        self.shared.wake.notify_one();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn builtin_save(tabs: &[Tab]) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = tabs;
        Err("unit test persisted without a save hook".into())
    }
    #[cfg(not(test))]
    {
        persist::save_tabs(tabs).map_err(|err| err.to_string())
    }
}

fn persist_worker(shared: Arc<PersistShared>, ack_tx: Sender<PersistAck>) {
    loop {
        let job = {
            let mut slot = shared.slot.lock().unwrap_or_else(|err| err.into_inner());
            loop {
                if let Some(job) = slot.job.take() {
                    break job;
                }
                if slot.stop {
                    return;
                }
                slot = shared
                    .wake
                    .wait(slot)
                    .unwrap_or_else(|err| err.into_inner());
            }
        };
        let hook = shared
            .save
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let error = hook(&job.tabs).err();
        if ack_tx
            .send(PersistAck {
                gen: job.gen,
                error,
            })
            .is_err()
        {
            return;
        }
    }
}

fn spawn_persist() -> PersistCtrl {
    let shared = Arc::new(PersistShared {
        slot: Mutex::new(PersistSlot {
            job: None,
            stop: false,
        }),
        wake: Condvar::new(),
        save: Mutex::new(Arc::new(builtin_save)),
    });
    let (ack_tx, ack_rx) = mpsc::channel();
    let worker_shared = Arc::clone(&shared);
    let join = std::thread::Builder::new()
        .name("json-viewer-persist".into())
        .spawn(move || persist_worker(worker_shared, ack_tx))
        .expect("spawn persist worker");
    PersistCtrl {
        shared,
        ack_rx,
        join: Some(join),
        dirty: false,
        last_change: Instant::now(),
        debounce: PERSIST_DEBOUNCE,
        edit_gen: 0,
        copy: None,
        next_gen: 1,
        acked: 0,
    }
}

fn blank_snapshot(tab: &Tab) -> Tab {
    let mut copy = Tab::new(tab.id, tab.title.clone());
    copy.error = tab.error.clone();
    copy.is_processing = tab.is_processing;
    copy.view = tab.view;
    copy.search_query = tab.search_query.clone();
    copy.search_results = tab.search_results.clone();
    copy.input.reserve(tab.input.len());
    copy.output.reserve(tab.output.len());
    copy
}

fn begin_copy(state: &AppState, gen: u64) -> CopyCursor {
    CopyCursor {
        dest: state.tabs.iter().map(blank_snapshot).collect(),
        tab: 0,
        field: CopyField::Input,
        offset: 0,
        gen,
    }
}

/// Copies at most [`PERSIST_CHUNK`] bytes from `state` into `cursor`.
fn step_copy(cursor: &mut CopyCursor, state: &AppState) -> CopyStep {
    if cursor.dest.len() != state.tabs.len() {
        return CopyStep::Restart;
    }
    let mut budget = PERSIST_CHUNK;
    while budget > 0 && cursor.tab < state.tabs.len() {
        let src = match cursor.field {
            CopyField::Input => state.tabs[cursor.tab].input.as_str(),
            CopyField::Output => state.tabs[cursor.tab].output.as_str(),
        };
        if cursor.offset > src.len() {
            return CopyStep::Restart;
        }
        let take = (src.len() - cursor.offset).min(budget);
        if take > 0 {
            let end = cursor.offset + take;
            let piece = &src[cursor.offset..end];
            match cursor.field {
                CopyField::Input => cursor.dest[cursor.tab].input.push_str(piece),
                CopyField::Output => cursor.dest[cursor.tab].output.push_str(piece),
            }
            cursor.offset = end;
            budget -= take;
        }
        if cursor.offset == src.len() {
            cursor.offset = 0;
            match cursor.field {
                CopyField::Input => cursor.field = CopyField::Output,
                CopyField::Output => {
                    cursor.field = CopyField::Input;
                    cursor.tab += 1;
                }
            }
        }
    }
    if cursor.tab >= state.tabs.len() {
        CopyStep::Done
    } else {
        CopyStep::Continue
    }
}

fn with_persist<R>(f: impl FnOnce(&mut PersistCtrl) -> R) -> R {
    PERSIST.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(spawn_persist());
        }
        f(slot.as_mut().expect("persist worker was just started"))
    })
}

fn report_persist_error(state: &mut AppState, error: &str) {
    let text = format!("Could not save tabs: {error}");
    state.status = Some(text.clone());
    state.dispatch(Msg::Status(text));
}

fn enqueue_persist(ctrl: &mut PersistCtrl, tabs: Vec<Tab>) -> u64 {
    let gen = ctrl.next_gen;
    ctrl.next_gen += 1;
    {
        let mut slot = ctrl
            .shared
            .slot
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        // One waiting snapshot. A newer one replaces it; the worker saves that.
        slot.job = Some(PersistJob { tabs, gen });
    }
    ctrl.shared.wake.notify_one();
    gen
}

fn apply_ack(ctrl: &mut PersistCtrl, state: &mut AppState, ack: PersistAck) -> bool {
    ctrl.acked = ctrl.acked.max(ack.gen);
    if let Some(error) = ack.error {
        report_persist_error(state, &error);
        return true;
    }
    false
}

fn drain_acks(ctrl: &mut PersistCtrl, state: &mut AppState) -> bool {
    let mut changed = false;
    while let Ok(ack) = ctrl.ack_rx.try_recv() {
        changed |= apply_ack(ctrl, state, ack);
    }
    changed
}

fn finish_copy(ctrl: &mut PersistCtrl, cursor: CopyCursor) {
    let _ = enqueue_persist(ctrl, cursor.dest);
}

fn note_persist(tx: &mut Tx) {
    with_persist(|ctrl| {
        ctrl.edit_gen = ctrl.edit_gen.wrapping_add(1);
        ctrl.dirty = true;
        ctrl.last_change = Instant::now();
        ctrl.copy = None;
        tx.wake_after(ctrl.debounce);
    });
}

fn start_or_continue_copy(ctrl: &mut PersistCtrl, state: &AppState, tx: &mut Tx) {
    if ctrl
        .copy
        .as_ref()
        .is_some_and(|cursor| cursor.gen != ctrl.edit_gen)
    {
        ctrl.copy = None;
    }
    if ctrl.copy.is_none() {
        if !ctrl.dirty {
            return;
        }
        let elapsed = ctrl.last_change.elapsed();
        if elapsed < ctrl.debounce {
            tx.wake_after(ctrl.debounce - elapsed);
            return;
        }
        ctrl.dirty = false;
        ctrl.copy = Some(begin_copy(state, ctrl.edit_gen));
    }
    let step = {
        let cursor = ctrl.copy.as_mut().expect("copy cursor");
        step_copy(cursor, state)
    };
    match step {
        CopyStep::Continue => tx.wake_after(Duration::ZERO),
        CopyStep::Restart => {
            ctrl.copy = None;
            ctrl.dirty = true;
            ctrl.last_change = Instant::now();
            tx.wake_after(ctrl.debounce);
        }
        CopyStep::Done => {
            if let Some(cursor) = ctrl.copy.take() {
                finish_copy(ctrl, cursor);
            }
        }
    }
}

fn drive_persist(state: &mut AppState, tx: &mut Tx) -> bool {
    let mut changed = false;
    PERSIST.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(ctrl) = slot.as_mut() else {
            return;
        };
        changed = drain_acks(ctrl, state);
        if ctrl.copy.is_some() || ctrl.dirty {
            start_or_continue_copy(ctrl, state, tx);
        }
    });
    changed
}

/// Blocks until the latest tabs are saved. Used by [`on_close`] / [`on_quit`].
fn flush_persist(state: &mut AppState) {
    with_persist(|ctrl| {
        ctrl.dirty = false;
        ctrl.copy = None;
        // Close/quit must finish before return. Chunk so we never call the
        // 35ms full-document clone; the loop still runs to completion here.
        let mut cursor = begin_copy(state, ctrl.edit_gen);
        loop {
            match step_copy(&mut cursor, state) {
                CopyStep::Continue => {}
                CopyStep::Done => break,
                CopyStep::Restart => {
                    cursor = begin_copy(state, ctrl.edit_gen);
                }
            }
        }
        let gen = enqueue_persist(ctrl, cursor.dest);
        let deadline = Instant::now() + Duration::from_secs(60);
        while ctrl.acked < gen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                report_persist_error(state, "persist flush timed out");
                return;
            }
            match ctrl.ack_rx.recv_timeout(remaining) {
                Ok(ack) => {
                    let _ = apply_ack(ctrl, state, ack);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report_persist_error(state, "persist flush timed out");
                    return;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    report_persist_error(state, "persist worker stopped");
                    return;
                }
            }
        }
    });
}

/// Rune [`App::on_close`](rune::App::on_close). Saves the current tabs before
/// the window goes, ignoring the debounce. `rune::run` does not register this;
/// `main` must build the app with `App::new(spec).on_close(on_close).on_quit(on_quit)`.
pub fn on_close(state: &mut AppState, tx: &mut Tx) {
    let _ = tx;
    flush_persist(state);
}

/// Rune [`App::on_quit`](rune::App::on_quit). Same flush as [`on_close`].
/// Windows may end the process when this returns, so the save is finished first.
pub fn on_quit(state: &mut AppState, tx: &mut Tx) {
    on_close(state, tx);
}

#[cfg(test)]
fn install_persist_save(hook: impl Fn(&[Tab]) -> Result<(), String> + Send + Sync + 'static) {
    with_persist(|ctrl| {
        ctrl.dirty = false;
        ctrl.copy = None;
        ctrl.debounce = PERSIST_DEBOUNCE;
        {
            let mut slot = ctrl
                .shared
                .slot
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            slot.job = None;
        }
        *ctrl
            .shared
            .save
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Arc::new(hook);
    });
}

#[cfg(test)]
fn set_persist_debounce(debounce: Duration) {
    with_persist(|ctrl| ctrl.debounce = debounce);
}

/// Rune `poll` hook. Applies every queued message (view messages first, then
/// completed background jobs) and returns whether state changed.
pub fn poll(state: &mut AppState, _clock: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    let mut applied = false;
    loop {
        let mut messages = state.take_pending();
        let completed: Vec<Msg> = with_inbox(|inbox| inbox.rx.try_iter().collect());
        let io_done = take_io_messages();
        if messages.is_empty() && completed.is_empty() && io_done.is_empty() {
            break;
        }
        applied = true;
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
    // Editor caret auto-scroll (rp-editor): the reducer marks the caret
    // line; scroll minimally to keep it inside the viewport.
    if reveal_editor_caret(state, tx) {
        changed = true;
    }
    if !applied {
        if let Some(key) = PENDING_REVEAL.with(|slot| slot.borrow_mut().take()) {
            tx.instant();
            if let Some((id, line)) = parse_editor_row_key(&key) {
                // A rope-editor row: it may not be mounted (far jump), so
                // scroll by offset and put the caret on the validation
                // error this reveal came from.
                reveal_editor_line(state, tx, id, Some(line));
            } else if let Some(id) = key
                .strip_prefix("editor-")
                .and_then(|rest| rest.parse::<u64>().ok())
            {
                reveal_editor_line(state, tx, id, None);
            } else {
                tx.scroll_into_view(key);
            }
            changed = true;
        }
    }
    if io_pending() {
        tx.wake_after(Duration::from_millis(16));
    }
    if rune::quitting() && !QUIT_FLUSHED.with(|flag| flag.replace(true)) {
        flush_persist(state);
        changed = true;
    }
    if drive_persist(state, tx) {
        changed = true;
    }
    changed
}

/// Parses `editor-{id}-row-{line}` reveal keys (the rope editor's rows use
/// the same key shape as `TextView::row_key`).
fn parse_editor_row_key(key: &str) -> Option<(u64, usize)> {
    let rest = key.strip_prefix("editor-")?;
    let (id, line) = rest.split_once("-row-")?;
    Some((id.parse().ok()?, line.parse().ok()?))
}

/// Scrolls the document viewport so the editor's caret line stays visible
/// (minimal scroll; nothing when it already is). Returns whether it moved.
fn reveal_editor_caret(state: &mut AppState, tx: &mut Tx) -> bool {
    let id = state.active;
    let input_view = state
        .tab(id)
        .is_some_and(|tab| tab.view == crate::state::TabView::Input);
    if !input_view {
        return false;
    }
    let Some((line, scroll_y, viewport_h)) = crate::editor::state::take_reveal(id) else {
        return false;
    };
    let row_h = crate::ui::text_view::ROW_HEIGHT;
    let y = line as f32 * row_h;
    let target = if y < scroll_y {
        Some(y)
    } else if viewport_h > 0.0 && y + row_h > scroll_y + viewport_h {
        Some(y + row_h - viewport_h)
    } else {
        None
    };
    match target {
        Some(target) => {
            tx.instant();
            tx.scroll_to_y(format!("document-scroll-{id}"), target.max(0.0));
            true
        }
        None => false,
    }
}

/// Reveals an editor line by scroll offset (works for unmounted rows) and
/// places the caret on the tab's validation error, if any. `line` is
/// 0-based; `None` centers the error line (or just focuses the container).
fn reveal_editor_line(state: &mut AppState, tx: &mut Tx, id: u64, line: Option<usize>) {
    let Some(tab) = state.tab(id) else {
        return;
    };
    let error = tab.validation_error().map(|e| (e.line, e.column));
    let viewport_h = crate::editor::state::with(id, &tab.input, tab.input_revision, |ed| {
        if let Some((err_line, err_col)) = error {
            let line_range = ed.buffer.line_byte_range(err_line.saturating_sub(1));
            let col = (err_col.saturating_sub(1)).min(line_range.end - line_range.start);
            let offset = line_range.start + col;
            ed.set_caret(offset, false);
            ed.reveal = None;
        }
        ed.viewport_h
    });
    let target_line = line.or_else(|| error.map(|(l, _)| l.saturating_sub(1)));
    if let Some(target_line) = target_line {
        let row_h = crate::ui::text_view::ROW_HEIGHT;
        let y = target_line as f32 * row_h;
        let centered = if viewport_h > 0.0 { y - viewport_h * 0.5 } else { y };
        tx.scroll_to_y(format!("document-scroll-{id}"), centered.max(0.0));
    } else {
        tx.scroll_into_view(format!("editor-{id}"));
    }
}

/// Executes effects returned by [`AppState::update`].
///
/// Background JSON jobs post [`Msg::JsonDone`] or [`Msg::ValidationDone`] to
/// the inbox. Search runs on a detached thread. Open, export, copy and
/// dropped-file reads return before any dialog or disk I/O finishes.
pub fn execute(state: &mut AppState, tx: &mut Tx, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::SpawnJson { tab, input, action } => {
                let tx = sender();
                let revision = crate::line_index::next_revision();
                let handle = json_ops::spawn_process(input, action, move |result| {
                    // Build the full line index off the UI thread from the
                    // worker-owned output before the reducer publishes it.
                    if let Ok(output) = &result {
                        crate::line_index::prewarm_off_thread(Arc::from(output.as_str()), revision);
                    }
                    let _ = tx.send(Msg::JsonDone {
                        tab,
                        revision,
                        result,
                    });
                });
                JOBS.with(|jobs| {
                    let mut jobs = jobs.borrow_mut();
                    if let Some(mut previous) = jobs.remove(&tab) {
                        previous.cancel();
                    }
                    jobs.insert(tab, handle);
                });
            }
            Effect::SpawnValidation {
                tab,
                input,
                input_revision,
            } => {
                let tx = sender();
                let handle = json_ops::spawn_validate(input, move |result| {
                    let _ = tx.send(Msg::ValidationDone {
                        tab,
                        input_revision,
                        result,
                    });
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
            Effect::Persist => note_persist(tx),
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
    use std::sync::{Arc, Mutex};
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

    fn wait_saves(saved: &Arc<Mutex<Vec<String>>>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while saved.lock().unwrap().len() < n {
            if Instant::now() > deadline {
                panic!("saves={:?}", saved.lock().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn debounce_coalesces_rapid_persists_into_one_save() {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&saved);
        install_persist_save(move |tabs| {
            record.lock().unwrap().push(
                tabs.first()
                    .map(|tab| tab.input.clone())
                    .unwrap_or_default(),
            );
            Ok(())
        });
        set_persist_debounce(Duration::from_secs(3600));
        let mut state = AppState::default();
        let mut tx = Tx::default();
        for i in 0..8 {
            state.update_input(state.active, format!("v{i}"));
            execute(&mut state, &mut tx, vec![Effect::Persist]);
        }
        assert!(
            !poll(&mut state, 0.0, &mut tx) || saved.lock().unwrap().is_empty(),
            "poll must not block, and must not save before the debounce"
        );
        assert!(
            saved.lock().unwrap().is_empty(),
            "N rapid Persist effects saved early: {:?}",
            saved.lock().unwrap()
        );
        set_persist_debounce(Duration::ZERO);
        let started = Instant::now();
        poll(&mut state, 0.0, &mut tx);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "debounced kick blocked the UI thread"
        );
        wait_saves(&saved, 1);
        assert_eq!(saved.lock().unwrap().as_slice(), ["v7"]);
        poll(&mut state, 0.0, &mut tx);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(saved.lock().unwrap().len(), 1, "coalesced save ran twice");
    }

    #[test]
    fn persist_save_returns_before_save_tabs_finishes() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (done_tx, done_rx) = mpsc::channel();
        let ui = std::thread::spawn(move || {
            install_persist_save(move |_tabs| {
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = release_rx.lock().unwrap().recv();
                Ok(())
            });
            set_persist_debounce(Duration::ZERO);
            let mut state = AppState::default();
            let mut tx = Tx::default();
            let started = Instant::now();
            execute(&mut state, &mut tx, vec![Effect::Persist]);
            poll(&mut state, 0.0, &mut tx);
            let _ = done_tx.send(started.elapsed());
        });
        let elapsed = done_rx
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_else(|_| {
                let _ = release_tx.send(());
                panic!("persist poll blocked on save_tabs");
            });
        assert!(
            elapsed < Duration::from_millis(200),
            "save did not return immediately: {elapsed:?}"
        );
        assert!(
            started_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "worker never entered save"
        );
        let _ = release_tx.send(());
        ui.join().unwrap();
    }

    #[test]
    fn close_flushes_a_debounced_snapshot() {
        let saved = Arc::new(Mutex::new(0u32));
        let count = Arc::clone(&saved);
        install_persist_save(move |_tabs| {
            *count.lock().unwrap() += 1;
            Ok(())
        });
        set_persist_debounce(Duration::from_secs(3600));
        let mut state = AppState::default();
        let mut tx = Tx::default();
        state.update_input(state.active, "flush-me".into());
        execute(&mut state, &mut tx, vec![Effect::Persist]);
        poll(&mut state, 0.0, &mut tx);
        assert_eq!(
            *saved.lock().unwrap(),
            0,
            "debounce should still be holding"
        );
        on_close(&mut state, &mut tx);
        assert_eq!(*saved.lock().unwrap(), 1, "close must flush one save");
    }

    #[test]
    fn persist_failure_sets_status() {
        install_persist_save(|_| Err("disk full".into()));
        set_persist_debounce(Duration::ZERO);
        let mut state = AppState::default();
        let mut tx = Tx::default();
        execute(&mut state, &mut tx, vec![Effect::Persist]);
        poll(&mut state, 0.0, &mut tx);
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.status.as_deref() != Some("Could not save tabs: disk full") {
            if Instant::now() > deadline {
                panic!("status={:?}", state.status);
            }
            std::thread::sleep(Duration::from_millis(5));
            poll(&mut state, 0.0, &mut tx);
        }
    }

    #[test]
    fn hundred_mb_snapshot_step_is_under_30ms() {
        let mut tabs = vec![crate::state::Tab::new(1, "big")];
        tabs[0].input = "x".repeat(100 * 1024 * 1024);
        let started = Instant::now();
        let cloned = tabs.clone();
        let clone_ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!("PERSIST_CLONE_MS={clone_ms:.3}");
        assert_eq!(cloned[0].input.len(), 100 * 1024 * 1024);
        drop(cloned);
        drop(tabs);

        let saved = Arc::new(Mutex::new(0usize));
        let record = Arc::clone(&saved);
        install_persist_save(move |tabs| {
            *record.lock().unwrap() = tabs.first().map(|tab| tab.input.len()).unwrap_or(0);
            Ok(())
        });
        set_persist_debounce(Duration::ZERO);
        let mut state = AppState::default();
        state.active_tab_mut().unwrap().input = "x".repeat(100 * 1024 * 1024);
        let mut tx = Tx::default();
        execute(&mut state, &mut tx, vec![Effect::Persist]);
        let started = Instant::now();
        poll(&mut state, 0.0, &mut tx);
        let step = started.elapsed();
        eprintln!("PERSIST_STEP_MS={:.3}", step.as_secs_f64() * 1000.0);
        assert!(
            step < Duration::from_millis(30),
            "snapshot step blocked the UI thread: {step:?} (full clone was {clone_ms:.3} ms)"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while *saved.lock().unwrap() != 100 * 1024 * 1024 {
            if Instant::now() > deadline {
                panic!("saved len={}", saved.lock().unwrap());
            }
            poll(&mut state, 0.0, &mut tx);
        }
    }
}
