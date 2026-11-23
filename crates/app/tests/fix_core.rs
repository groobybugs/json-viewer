//! rp-fix-core regression evidence: editor-core and state fixes from the
//! final review (C2, M4, L1, H2, H3, M3) plus the UTF-8 byte-offset audit.
//!
//! Run: `cargo test -p json-viewer --test fix_core`
//!
//! The reducer-level regressions live in the module test blocks of
//! `editor/state.rs`, `editor/buffer.rs` and `state.rs`; this file drives
//! the real runner flow (job spawn → inbox → poll) for the async findings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use json_viewer::state::{AppState, Effect, Msg, TabView};
use json_viewer::{headless, runner};
use rune::{col, text, Cx, El, Env, Runtime, Tx};

fn io_view(state: &AppState, _cx: &mut Cx<AppState>) -> El {
    let status = state.status.clone().unwrap_or_else(|| "ready".into());
    col()
        .w_full()
        .h_full()
        .child(text(status).key("status").font(rune::tok::type_::LABEL))
}

fn runtime(state: AppState) -> Runtime<AppState> {
    let mut rt = Runtime::new(state, io_view, (400.0, 300.0), Env::new(true, 1.0));
    rt.poll = Some(runner::poll);
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
        "timed out: status={:?} inputs={:?}",
        rt.state.status,
        rt.state.tabs.iter().map(|t| (t.id, t.input.len())).collect::<Vec<_>>()
    );
}

/// The document a Format effect will process, whether it takes the classic
/// `Arc<str>` copy or the O(1) rope snapshot.
fn format_input(effect: &Effect) -> String {
    match effect {
        Effect::SpawnJson { input, .. } => input.to_string(),
        Effect::MaterializeRope { input, .. } => input.to_string(),
        other => panic!("expected a JSON job effect, got {other:?}"),
    }
}

#[test]
fn dropped_file_lands_in_the_requesting_tab() {
    // M3: the destination must be the tab that was active when the drop was
    // requested, not whichever tab is active when the read completes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.json");
    std::fs::write(&path, r#"{"dropped":true}"#).unwrap();

    let mut state = AppState::default();
    let a = state.active;
    let b = state.add_tab();
    state.set_active(a);

    let effects = state.update(Msg::FileDropped(path));
    let mut tx = Tx::default();
    runner::execute(&mut state, &mut tx, effects);
    // The user switches tabs while the read is in flight.
    state.update(Msg::SelectTab(b));

    let started = Instant::now();
    loop {
        runner::poll(&mut state, 0.0, &mut tx);
        let landed = state.tab(a).is_some_and(|t| !t.input.is_empty())
            || state.tab(b).is_some_and(|t| !t.input.is_empty());
        if landed {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(8), "read never landed");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        state.tab(a).unwrap().input,
        r#"{"dropped":true}"#,
        "the drop must land in the tab that requested it"
    );
    assert!(
        state.tab(b).unwrap().input.is_empty(),
        "the newly active tab was overwritten: {:?}",
        state.tab(b).unwrap().input
    );
}

#[test]
fn opened_file_lands_in_the_requesting_tab() {
    // M3 (open-dialog path): the tab is captured when the dialog is
    // requested; switching tabs before the read completes must not redirect
    // the file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opened.json");
    std::fs::write(&path, r#"{"opened":true}"#).unwrap();

    let mut state = AppState::default();
    let a = state.active;
    let b = state.add_tab();
    state.set_active(a);
    let mut rt = runtime(state);

    rt.state.dispatch(Msg::OpenFile);
    rt.frame(1.0 / 60.0);
    let (kind, _) = rt.file_dialog().expect("open dialog requested");
    assert_eq!(kind, rune::FileDialogKind::Open);
    // Switch tabs while the dialog is open, then answer it.
    rt.state.set_active(b);
    rt.resolve_file_dialog(Some(path)).expect("answer dialog");

    pump_until(&mut rt, |state| {
        state.tab(a).is_some_and(|t| !t.input.is_empty())
            || state.tab(b).is_some_and(|t| !t.input.is_empty())
    });
    assert_eq!(
        rt.state.tab(a).unwrap().input,
        r#"{"opened":true}"#,
        "the open must land in the tab that requested it"
    );
    assert!(
        rt.state.tab(b).unwrap().input.is_empty(),
        "the newly active tab was overwritten: {:?}",
        rt.state.tab(b).unwrap().input
    );
}

#[test]
fn format_after_external_replace_uses_the_new_text() {
    // H2 end-to-end: a dirty rope from an earlier edit must not feed Format
    // after an external replacement (file load) while viewing Output.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "[1]".into());
    // One keystroke: the rope is now dirty (" [1]") ahead of the model.
    json_viewer::editor::state::with(
        id,
        &state.tab(id).unwrap().input,
        state.tab(id).unwrap().input_revision,
        |ed| {
            ed.insert(" ", json_viewer::editor::EditKind::Typing);
        },
    );
    state.update(Msg::SwitchView(TabView::Output));
    // A file load replaces the input while the Output view is showing.
    state.update_input(id, "[2]".into());
    let effects = state.update(Msg::Format);
    assert_eq!(effects.len(), 1);
    assert_eq!(
        format_input(&effects[0]),
        "[2]",
        "Format must process the replacement text, not the stale rope"
    );
}

#[test]
fn search_effects_carry_the_input_revision() {
    // H3 plumbing: the revision the search/format was started for travels
    // with the effect so late results can be gated (mirrors SpawnValidation).
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, r#"{"alpha":1}"#.into());
    let revision = state.tab(id).unwrap().input_revision;
    let effects = state.update(Msg::SearchChanged("al".into()));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Search { input_revision, .. }] if *input_revision == revision
    ));
    let effects = state.update(Msg::Format);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SpawnJson { input_revision, .. }] if *input_revision == revision
    ));
}

#[test]
fn superseded_search_results_never_land() {
    // H3 through the real runner: two searches race; only the latest
    // query's hits may appear, whichever order the walks finish in.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, r#"{"old":1,"new":2}"#.into());
    let mut tx = Tx::default();
    let effects = state.update(Msg::SearchChanged("old".into()));
    runner::execute(&mut state, &mut tx, effects);
    let effects = state.update(Msg::SearchChanged("new".into()));
    runner::execute(&mut state, &mut tx, effects);

    let started = Instant::now();
    loop {
        runner::poll(&mut state, 0.0, &mut tx);
        let tab = state.tab(id).unwrap();
        if !tab.is_processing && !tab.search_results.is_empty() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(8), "search never landed");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Let any late stale delivery arrive; it must change nothing.
    for _ in 0..10 {
        runner::poll(&mut state, 0.0, &mut tx);
        std::thread::sleep(Duration::from_millis(5));
    }
    let tab = state.tab(id).unwrap();
    assert_eq!(tab.search_query, "new");
    assert!(
        tab.search_results.iter().all(|hit| hit.path == "new"),
        "stale hits landed: {:?}",
        tab.search_results
    );
    assert!(!tab.search_results.is_empty());
}

#[test]
fn utf8_document_renders_and_edits_without_panics() {
    // Audit end-to-end: emoji, combining marks, CJK and CRLF through the
    // real editor view — render, word selection, vertical motion.
    let doc = "{\"emoji\":\"🚀\",\"accent\":\"café\",\"cjk\":\"中文\",\"comb\":\"e\u{301}\"}\r\n[1,2]";
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, doc.into());
    let mut rt = Runtime::with_text(
        state,
        editor_view,
        (1280.0, 800.0),
        Env::new(true, 1.0),
        headless::text_system(),
    );
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);

    // Double-click the emoji (C2 through the real view path).
    let emoji_off = doc.find('🚀').unwrap();
    json_viewer::editor::state::with(
        id,
        &rt.state.tab(id).unwrap().input,
        rt.state.tab(id).unwrap().input_revision,
        |ed| ed.select_word(emoji_off),
    );
    rt.frame(0.0);
    let sel = json_viewer::editor::state::with(
        id,
        &rt.state.tab(id).unwrap().input,
        rt.state.tab(id).unwrap().input_revision,
        |ed| ed.selection(),
    );
    assert_eq!(sel, Some(emoji_off..emoji_off + 4));

    // Vertical motion across the CRLF line keeps a char goal column (L1).
    json_viewer::editor::state::with(
        id,
        &rt.state.tab(id).unwrap().input,
        rt.state.tab(id).unwrap().input_revision,
        |ed| {
            ed.set_caret(emoji_off + 4, false); // after 🚀, char col 14
            ed.move_caret(json_viewer::editor::state::Move::Down, false, 10);
            let caret = ed.caret;
            assert_eq!(
                ed.buffer.char_to_byte(ed.buffer.byte_to_char(caret)),
                caret,
                "caret left a char boundary"
            );
        },
    );
    rt.frame(0.0);
}

fn editor_view(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let tab = state.active_tab().expect("active tab");
    let base = format!("editor-{}", tab.id);
    col()
        .key(format!("document-scroll-{}-input", tab.id))
        .w_full()
        .h_full()
        .scroll(rune::ScrollSpec { stick_bottom: false })
        .child(json_viewer::editor::view::build(state, cx, tab, &base))
}

#[test]
fn mid_char_error_column_renders_without_panics() {
    // Audit: the error underline indexes the display string at the error
    // column; a column inside a multibyte char must snap, not split it.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "a🚀b".into());
    state.tab_mut(id).unwrap().validation = Some(json_viewer::state::ValidationResult::Invalid(
        json_viewer::json_ops::JsonError::new("unexpected token", 1, 3), // byte 3: inside 🚀
    ));
    let mut rt = Runtime::with_text(
        state,
        editor_view,
        (1280.0, 800.0),
        Env::new(true, 1.0),
        headless::text_system(),
    );
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    rt.settle_all();
    rt.frame(0.0);
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.key.as_deref() == Some("editor-1-error-dot-0")),
        "error gutter dot missing"
    );
}

#[test]
fn ime_select_snaps_to_char_boundaries() {
    // Audit: Ime::Select sets anchor/caret directly from the IME range.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "a🚀b".into());
    let mut rt = Runtime::with_text(
        state,
        editor_view,
        (1280.0, 800.0),
        Env::new(true, 1.0),
        headless::text_system(),
    );
    rt.frame(0.0);
    let ime = rune::Ime::Select(2..4); // inside the emoji
    json_viewer::editor::keys::on_ime(&mut rt.state, &ime, &mut Tx::default());
    let (anchor, caret) = json_viewer::editor::state::with(
        id,
        &rt.state.tab(id).unwrap().input,
        rt.state.tab(id).unwrap().input_revision,
        |ed| (ed.anchor, ed.caret),
    );
    for off in [anchor.unwrap(), caret] {
        json_viewer::editor::state::with(
            id,
            &rt.state.tab(id).unwrap().input,
            rt.state.tab(id).unwrap().input_revision,
            |ed| {
                assert_eq!(
                    ed.buffer.char_to_byte(ed.buffer.byte_to_char(off)),
                    off,
                    "IME selection offset {off} is not a char boundary"
                );
            },
        );
    }
}

#[test]
fn stale_rope_never_feeds_the_search_job() {
    // H2 (search consumer): same stale-rope window as Format, via Search.
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, r#"{"old":1}"#.into());
    json_viewer::editor::state::with(
        id,
        &state.tab(id).unwrap().input,
        state.tab(id).unwrap().input_revision,
        |ed| {
            ed.insert(" ", json_viewer::editor::EditKind::Typing);
        },
    );
    state.update_input(id, r#"{"new":2}"#.into());
    let effects = state.update(Msg::SearchChanged("ne".into()));
    assert_eq!(effects.len(), 1);
    let input = match &effects[0] {
        Effect::Search { input, .. } => input.to_string(),
        Effect::MaterializeRope { input, .. } => input.to_string(),
        other => panic!("expected a search effect, got {other:?}"),
    };
    assert_eq!(input, r#"{"new":2}"#);
}

#[test]
fn file_loaded_legacy_message_still_targets_the_active_tab() {
    // API compat: the original FileLoaded message (used by tests/bins)
    // keeps its active-tab semantics; the runner now sends FileLoadedFor.
    let mut state = AppState::default();
    let a = state.active;
    let b = state.add_tab();
    state.set_active(b);
    state.update(Msg::FileLoaded {
        path: PathBuf::from("/tmp/legacy.json"),
        content: "[1]".into(),
    });
    assert!(state.tab(a).unwrap().input.is_empty());
    assert_eq!(state.tab(b).unwrap().input, "[1]");
}
