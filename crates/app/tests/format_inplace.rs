//! rp-format-inplace evidence: Format/Minify rewrite the Input document in
//! place as ONE undo transaction in the rope editor.
//!
//! Behavioural tests drive the real worker flow (`Msg::Format` →
//! `Effect::MaterializeRope`/`SpawnJson` → `Msg::JsonDone` through
//! `runner::poll`) in a headless runtime with the real `ui::view`:
//!
//! - Format/Minify replace Input; one Ctrl+Z restores the exact previous text
//!   and caret; Ctrl+Shift+Z / Ctrl+Y redo.
//! - A stale result (input edited while the job ran) and invalid JSON never
//!   touch Input; an identical result is a no-op (no revision bump, no undo
//!   step).
//! - Non-ASCII/emoji documents round-trip byte-exactly.
//! - The rewritten Input is what persist snapshots (`runner::copied_tabs`),
//!   and that snapshot round-trips through a real `TabsStore` in a temp dir.
//!   (`persist::save_tabs` is a no-op under the test harness, so no test can
//!   or needs to write the real profile.)
//! - PNG evidence: `target/headless/format-inplace-*.png`.
//!
//! Ignored release benches (`--ignored --nocapture`, run each in its own
//! process for a clean VmHWM):
//!
//! - `format_inplace_100mb_bench`: 100 MB minified Format — UI-thread apply
//!   ms, first Output frame, keystroke p95 right after (budget: within 15% of
//!   the E5 1.21 ms), one-step undo, VmHWM before/after.
//! - `format_inplace_baseline_bench`: 100 MB already-formatted Format
//!   (unchanged-output path, no Input rewrite) — the "before" RSS reference.
//!
//! ```sh
//! cargo test --release -p json-viewer --test format_inplace \
//!     format_inplace_100mb_bench -- --ignored --nocapture --exact
//! cargo test --release -p json-viewer --test format_inplace \
//!     format_inplace_baseline_bench -- --ignored --nocapture --exact
//! ```

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use json_viewer::editor::state as editor_state;
use json_viewer::editor::state::EditorAction;
use json_viewer::editor::EditKind;
use json_viewer::json_ops::JsonAction;
use json_viewer::state::{AppState, Msg, TabView};
use json_viewer::{headless, json_ops, persist, runner};
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};
use rune::{Key, KeyEvent, Mods, Runtime};

const SCALE: f32 = 1.0;
const MIB: usize = 1024 * 1024;
/// Keystroke p95 budget: within 15% of the E5 rope-editor 1.21 ms at 100 MB.
const KEY_P95_BUDGET_MS: f64 = 1.21 * 1.15;
/// One 60 Hz frame for the UI-thread apply step.
const APPLY_BUDGET_MS: f64 = 16.0;

/// `fold_index`/`line_index` keep process-wide stores; serialize the tests.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn proc_status_mb(field: &str) -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

fn focus_editor(rt: &mut Runtime<AppState>, id: u64) {
    rt.update(|_, tx| {
        tx.focus(format!("editor-{id}"), true);
    });
    rt.frame(0.0);
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.key.as_deref() == Some(format!("editor-{id}").as_str()) && e.focused),
        "rope editor did not take focus"
    );
}

fn place_caret(rt: &mut Runtime<AppState>, id: u64, caret: usize) {
    rt.update(|s, tx| {
        let tab = s.tab(id).expect("tab");
        let line = editor_state::with(id, &tab.input, tab.input_revision, |ed| {
            ed.caret = caret.min(ed.buffer.len_bytes());
            ed.anchor = None;
            ed.reveal = None;
            ed.set_horizontal_window(240);
            ed.buffer.byte_to_line(ed.caret)
        });
        tx.instant();
        tx.scroll_to_y(
            format!("document-scroll-{id}-input"),
            (line as f32 * 16.0 - 200.0).max(0.0),
        );
    });
    rt.frame(0.0);
    rt.frame(0.0);
}

/// One keystroke sample: key + dispatch frame + poll (reducer apply) + render
/// frame — the E5 methodology.
fn key_ms(rt: &mut Runtime<AppState>, key: Key, mods: Mods) -> f64 {
    let t = Instant::now();
    rt.key(KeyEvent { key, mods });
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    ms(t)
}

/// A chord sample (Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y) with the same methodology.
fn chord_ms(rt: &mut Runtime<AppState>, event: KeyEvent) -> f64 {
    let t = Instant::now();
    rt.key(event);
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    ms(t)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent { key: Key::Command(c), mods: Mods { ctrl: true, ..Mods::default() } }
}

fn rope_text(rt: &Runtime<AppState>, id: u64) -> String {
    let tab = rt.state.tab(id).expect("tab");
    editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.to_string())
}

fn rope_len(rt: &Runtime<AppState>, id: u64) -> usize {
    let tab = rt.state.tab(id).expect("tab");
    editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.len_bytes())
}

fn rope_caret(rt: &Runtime<AppState>, id: u64) -> usize {
    let tab = rt.state.tab(id).expect("tab");
    editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.caret)
}

fn dispatch(rt: &mut Runtime<AppState>, msg: Msg) {
    rt.update(|s, tx| {
        s.dispatch(msg);
        runner::poll(s, 0.0, tx);
    });
    rt.frame(0.0);
}

/// Pumps the real worker flow until the job lands; returns the duration of
/// the `runner::poll` call that applied `Msg::JsonDone` (the UI-thread apply).
fn pump_format(rt: &mut Runtime<AppState>, id: u64, deadline_secs: u64) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        let t = Instant::now();
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        let applied = ms(t);
        let done = rt
            .state
            .tab(id)
            .is_some_and(|tab| !tab.is_processing && tab.view == TabView::Output);
        if done {
            rt.frame(0.0);
            return applied;
        }
        rt.frame(1.0 / 60.0);
        assert!(Instant::now() < deadline, "format job never completed");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Pumps until the tab's background work is done (any outcome).
fn pump_idle(rt: &mut Runtime<AppState>, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        rt.frame(1.0 / 60.0);
        if rt.state.tab(id).is_some_and(|tab| !tab.is_processing) {
            return;
        }
        assert!(Instant::now() < deadline, "job never completed");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn draw_png(rt: &mut Runtime<AppState>, name: &str) {
    rt.settle_all();
    rt.frame(0.0);
    let (w, h) = headless::DEFAULT_SIZE;
    let gpu = Gpu::headless();
    let format = OFFSCREEN_FORMAT.remove_srgb_suffix();
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(true);
    let target = Offscreen::with_format(&gpu.device, w as u32, h as u32, format);
    renderer.prepare(
        &gpu.device,
        &gpu.queue,
        &mut rt.text.atlas,
        &rt.instances,
        (w as u32, h as u32),
    );
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&mut encoder, &target.view, rt.clear_color());
    target.copy(&mut encoder);
    renderer.submit(&gpu.queue, encoder);
    let pixels = target.read(&gpu.device);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/headless")
        .join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_png(&path, w as u32, h as u32, &pixels).expect("write format-inplace PNG");
}

/// Single-line minified ASCII JSON, grown to ~`target` bytes (E5 fixture).
fn minified_fixture(target: usize) -> String {
    let mut out = String::with_capacity(target + 4096);
    out.push_str("{\"items\":[");
    let mut i = 0usize;
    while out.len() < target.saturating_sub(200) {
        if i > 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"id\":{i},\"name\":\"item-{i:07}\",\
             \"description\":\"bench record {i:07} with some plain ascii text\",\
             \"tags\":[\"alpha\",\"beta\"],\"value\":{}.{}}}",
            i % 1000,
            i % 100
        )
        .expect("write fixture");
        i += 1;
    }
    out.push_str("]}");
    assert!(!out.contains('\n'), "minified fixture must be one line");
    out
}

// ── Behaviour ──────────────────────────────────────────────────────────────

#[test]
fn format_replaces_input_and_one_undo_restores_text_and_caret() {
    let _guard = serial();
    let original = "{\"b\":2,\"a\":[1,2,3]}";
    let formatted = json_ops::process(original, JsonAction::Format).expect("format fixture");
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    place_caret(&mut rt, id, 5);

    dispatch(&mut rt, Msg::Format);
    let _apply_ms = pump_format(&mut rt, id, 30);

    let tab = rt.state.tab(id).unwrap();
    assert_eq!(tab.output, formatted);
    assert_eq!(tab.view, TabView::Output, "Format still switches to Output");
    assert_eq!(rope_text(&rt, id), formatted, "Input must hold the result");
    // The persist snapshot copies the rewritten (dirty) rope, not the cache.
    let copied = runner::copied_tabs(&rt.state);
    assert_eq!(copied[0].input, formatted);

    // Back to Input: one Ctrl+Z restores the exact original and caret.
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    focus_editor(&mut rt, id);
    chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_text(&rt, id), original);
    assert_eq!(rope_caret(&rt, id), 5);

    // Ctrl+Shift+Z redoes the format.
    let redo = KeyEvent {
        key: Key::Command('z'),
        mods: Mods { ctrl: true, shift: true, ..Mods::default() },
    };
    chord_ms(&mut rt, redo);
    assert_eq!(rope_text(&rt, id), formatted);

    // Ctrl+Z then Ctrl+Y also redoes.
    chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_text(&rt, id), original);
    chord_ms(&mut rt, ctrl('y'));
    assert_eq!(rope_text(&rt, id), formatted);
}

#[test]
fn minify_replaces_input_and_one_undo_restores_it() {
    let _guard = serial();
    let original = "{\n  \"b\": 2,\n  \"a\": [1, 2, 3]\n}";
    let minified = json_ops::process(original, JsonAction::Minify).expect("minify fixture");
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    place_caret(&mut rt, id, 8);

    dispatch(&mut rt, Msg::Minify);
    pump_format(&mut rt, id, 30);
    assert_eq!(rt.state.tab(id).unwrap().output, minified);
    assert_eq!(rope_text(&rt, id), minified);

    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    focus_editor(&mut rt, id);
    chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_text(&rt, id), original);
    assert_eq!(rope_caret(&rt, id), 8);
    chord_ms(&mut rt, ctrl('y'));
    assert_eq!(rope_text(&rt, id), minified);
}

#[test]
fn stale_result_leaves_input_untouched() {
    let _guard = serial();
    // Deterministic at the reducer level: the job was started for the old
    // revision, the user types while it runs, the late result must not land.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "{\"a\":1}".into());
    let tab = state.tab(id).unwrap();
    editor_state::with(id, &tab.input, tab.input_revision, |_| {});
    let effects = state.update(Msg::Format);
    assert!(!effects.is_empty());
    let old_revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::EditorEdit {
        tab: id,
        action: EditorAction::Insert("x".into(), EditKind::Typing),
    });
    let typed_revision = state.tab(id).unwrap().input_revision;
    assert_ne!(typed_revision, old_revision);
    let effects = state.update(Msg::JsonDone {
        tab: id,
        revision: json_viewer::line_index::next_revision(),
        input_revision: old_revision,
        result: Ok("{\n  \"a\": 1\n}".into()),
    });
    assert!(effects.is_empty(), "stale result must not persist or spawn");
    let tab = state.tab(id).unwrap();
    assert!(tab.output.is_empty());
    assert_eq!(tab.view, TabView::Input);
    assert_eq!(tab.input_revision, typed_revision);
    let text = editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.to_string());
    assert_eq!(text, "x{\"a\":1}", "stale result rewrote Input");
}

#[test]
fn invalid_json_leaves_input_untouched() {
    let _guard = serial();
    let original = "{\"a\": oops}";
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    let revision = rt.state.tab(id).unwrap().input_revision;

    dispatch(&mut rt, Msg::Format);
    pump_idle(&mut rt, id);

    let tab = rt.state.tab(id).unwrap();
    assert!(tab.error.is_some(), "invalid input must report an error");
    assert!(tab.output.is_empty());
    assert_eq!(tab.view, TabView::Input);
    assert_eq!(tab.input_revision, revision);
    assert_eq!(rope_text(&rt, id), original);
    let can_undo = editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.can_undo());
    assert!(!can_undo, "a failed job must not add an undo step");
}

#[test]
fn non_ascii_and_emoji_document_round_trips() {
    let _guard = serial();
    let original = "{ \"emoji\" : \"🚀\", \"中文\" : \"café\", \"e\u{301}\" : \"x\" }";
    let formatted = json_ops::process(original, JsonAction::Format).expect("unicode fixture");
    assert_ne!(original, formatted);
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    // Caret on the emoji's first byte (a char boundary).
    let caret = original.find('🚀').expect("emoji in fixture");
    place_caret(&mut rt, id, caret);

    dispatch(&mut rt, Msg::Format);
    pump_format(&mut rt, id, 30);
    assert_eq!(rope_text(&rt, id), formatted);

    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    focus_editor(&mut rt, id);
    chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_text(&rt, id), original, "undo must restore exact bytes");
    assert_eq!(rope_caret(&rt, id), caret);
    chord_ms(&mut rt, ctrl('y'));
    assert_eq!(rope_text(&rt, id), formatted);
}

#[test]
fn formatting_already_formatted_input_is_a_noop() {
    let _guard = serial();
    let formatted = json_ops::process("{\"a\":1}", JsonAction::Format).expect("fixture");
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, formatted.clone());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    let revision = rt.state.tab(id).unwrap().input_revision;

    dispatch(&mut rt, Msg::Format);
    pump_format(&mut rt, id, 30);

    let tab = rt.state.tab(id).unwrap();
    assert_eq!(tab.output, formatted);
    assert_eq!(tab.input_revision, revision, "unchanged output must not bump the revision");
    assert_eq!(rope_text(&rt, id), formatted);
    let can_undo = editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.buffer.can_undo());
    assert!(!can_undo, "unchanged output must not add an undo step");
}

#[test]
fn persist_snapshot_of_the_rewritten_input_round_trips_through_the_store() {
    let _guard = serial();
    let original = "{\"a\":1,\"b\":[1,2,3]}";
    let formatted = json_ops::process(original, JsonAction::Format).expect("fixture");
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);

    dispatch(&mut rt, Msg::Format);
    pump_format(&mut rt, id, 30);

    // The debounced save copies exactly this snapshot (the runner's unit
    // tests prove the debounce/worker path; `save_tabs` itself is a no-op
    // under the test harness, so the real profile can never be touched).
    // Prove the snapshot carries the rewritten Input and survives the real
    // store format in a temp dir.
    let copied = runner::copied_tabs(&rt.state);
    assert_eq!(copied[0].input, formatted);
    assert_eq!(copied[0].output, formatted);
    let dir = tempfile::tempdir().expect("temp dir");
    let store = persist::TabsStore::new(dir.path());
    store.save(&copied).expect("save snapshot");
    let loaded = store.load().expect("load snapshot");
    assert_eq!(loaded[0].input, formatted);

    // Let the debounce elapse and the chunked copy run: no persist error may
    // surface, and the snapshot must still be the rewritten document.
    std::thread::sleep(Duration::from_millis(600));
    for _ in 0..20 {
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        rt.frame(1.0 / 60.0);
    }
    assert!(
        rt.state.status.is_none(),
        "persist reported an error: {:?}",
        rt.state.status
    );
    assert_eq!(runner::copied_tabs(&rt.state)[0].input, formatted);
}

#[test]
fn png_evidence_input_after_format_and_after_undo() {
    let _guard = serial();
    let original = "{\"name\":\"🚀 launch\",\"nested\":{\"a\":[1,2,3]},\"ok\":true}";
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, original.into());
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);

    dispatch(&mut rt, Msg::Format);
    pump_format(&mut rt, id, 30);
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    focus_editor(&mut rt, id);
    draw_png(&mut rt, "format-inplace-input.png");

    chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_text(&rt, id), original);
    draw_png(&mut rt, "format-inplace-undo.png");
}

// ── Release benches (ignored) ──────────────────────────────────────────────

#[test]
#[ignore]
fn format_inplace_100mb_bench() {
    let _guard = serial();
    let input = minified_fixture(100 * MIB);
    let bytes = input.len();
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input);
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    let mid = bytes / 2;
    place_caret(&mut rt, id, mid);

    let hwm_before = proc_status_mb("VmHWM:");
    let rss_before = proc_status_mb("VmRSS:");
    let revision_before = rt.state.tab(id).unwrap().input_revision;

    dispatch(&mut rt, Msg::Format);
    let apply_ms = pump_format(&mut rt, id, 180);
    let t = Instant::now();
    rt.frame(1.0 / 60.0);
    let first_output_frame_ms = ms(t);
    assert!(
        rt.state.tab(id).unwrap().input_revision > revision_before,
        "format must bump the input revision"
    );

    // Let the debounced persist chunk-copy finish so the keystroke samples
    // below measure the editor, not the (pre-existing) persist machinery.
    std::thread::sleep(Duration::from_millis(600));
    for _ in 0..200 {
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        rt.frame(1.0 / 60.0);
    }
    let hwm_after = proc_status_mb("VmHWM:");
    let rss_after = proc_status_mb("VmRSS:");

    // Back to Input: ONE Ctrl+Z right after the replace restores the exact
    // original document and the pre-format caret. (The undo entry respects
    // the 64 MiB history cap, so later edits evict it at this size — the cap
    // semantics are covered by buffer.rs module tests.)
    dispatch(&mut rt, Msg::SwitchView(TabView::Input));
    focus_editor(&mut rt, id);
    let formatted_len = rope_len(&rt, id);
    let undo_ms = chord_ms(&mut rt, ctrl('z'));
    assert_eq!(rope_len(&rt, id), bytes, "format undo must restore the original document");
    assert_eq!(rope_caret(&rt, id), mid, "format undo must restore the caret");
    let redo_ms = chord_ms(&mut rt, ctrl('y'));
    assert_eq!(rope_len(&rt, id), formatted_len, "redo must re-apply the format");

    // Keystroke p95 right after the replace (E5 methodology, 20 chars per
    // position).
    let mut samples = Vec::new();
    let mut expected = formatted_len;
    for caret in [0usize, formatted_len / 2, formatted_len] {
        place_caret(&mut rt, id, caret);
        for _ in 0..20 {
            let d = key_ms(&mut rt, Key::Text("x".into()), Mods::default());
            samples.push(d);
            assert!(d < 120_000.0, "keystroke watchdog");
        }
        expected += 20;
        assert_eq!(rope_len(&rt, id), expected, "typed chars lost");
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let key_p95_ms = percentile(&samples, 0.95);
    let key_median_ms = percentile(&samples, 0.5);

    // Capture the Input view at the mid-file caret (undo the typing runs
    // first; the format step itself may already be evicted by the cap).
    for _ in 0..3 {
        chord_ms(&mut rt, ctrl('z'));
    }
    assert_eq!(rope_len(&rt, id), formatted_len, "typing undos must restore the formatted doc");
    place_caret(&mut rt, id, formatted_len / 2);
    draw_png(&mut rt, "format-inplace-100mb-input.png");

    let hwm_end = proc_status_mb("VmHWM:");
    drop(rt);

    println!("\n## rp-format-inplace — 100 MB minified Format (release, scale 1, 1280x800, real ui::view)\n");
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!("| Input MiB | {:.2} |", bytes as f64 / MIB as f64);
    println!("| Formatted MiB | {:.2} |", formatted_len as f64 / MIB as f64);
    println!("| Apply (UI-thread poll) ms | {apply_ms:.2} |");
    println!("| First Output frame ms | {first_output_frame_ms:.1} |");
    println!("| Keystroke median ms | {key_median_ms:.2} |");
    println!("| Keystroke p95 ms | {key_p95_ms:.2} |");
    println!("| Keystroke p95 budget ms | {KEY_P95_BUDGET_MS:.3} |");
    println!("| Format undo ms | {undo_ms:.2} |");
    println!("| Format redo ms | {redo_ms:.2} |");
    println!(
        "| VmHWM before/after/end MiB | {} / {} / {} |",
        hwm_before.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
        hwm_after.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
        hwm_end.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
    );
    println!(
        "| VmRSS before/after MiB | {} / {} |",
        rss_before.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
        rss_after.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
    );

    assert!(
        apply_ms < APPLY_BUDGET_MS,
        "apply blocked the UI thread for {apply_ms:.2} ms (budget {APPLY_BUDGET_MS})"
    );
    assert!(
        key_p95_ms <= KEY_P95_BUDGET_MS,
        "keystroke p95 {key_p95_ms:.2} ms exceeds budget {KEY_P95_BUDGET_MS:.3} ms"
    );
}

#[test]
#[ignore]
fn format_inplace_baseline_bench() {
    let _guard = serial();
    // Already-formatted input: the worker detects identical output, so Input
    // is NOT rewritten — the "before" behaviour and its peak-RSS reference.
    // (The E5 pretty fixture is editor-only, not parseable JSON; canonicalize
    // the valid minified fixture instead.)
    let minified = minified_fixture(100 * MIB);
    let input = json_ops::process(&minified, JsonAction::Format).expect("canonicalize 100 MB");
    drop(minified);
    let bytes = input.len();
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input);
    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, SCALE);
    focus_editor(&mut rt, id);
    place_caret(&mut rt, id, bytes / 2);

    let hwm_before = proc_status_mb("VmHWM:");
    let revision_before = rt.state.tab(id).unwrap().input_revision;
    dispatch(&mut rt, Msg::Format);
    let apply_ms = pump_format(&mut rt, id, 180);
    assert_eq!(
        rt.state.tab(id).unwrap().input_revision, revision_before,
        "identical output must be a no-op for Input"
    );
    std::thread::sleep(Duration::from_millis(600));
    for _ in 0..200 {
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        rt.frame(1.0 / 60.0);
    }
    let hwm_after = proc_status_mb("VmHWM:");
    drop(rt);

    println!("\n## rp-format-inplace baseline — 100 MB already-formatted Format (no Input rewrite)\n");
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!("| Input MiB | {:.2} |", bytes as f64 / MIB as f64);
    println!("| Apply (UI-thread poll) ms | {apply_ms:.2} |");
    println!(
        "| VmHWM before/after MiB | {} / {} |",
        hwm_before.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
        hwm_after.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
    );
}
