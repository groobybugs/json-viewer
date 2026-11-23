//! Regression tests for rp-fix-io (C1, H1, L2, crash-mid-save).
//!
//! These use only the public API so they compile against the pre-fix crate
//! and fail on the bugs. Crash snapshots are built by hand so each save step
//! is loadable without depending on a live interrupt hook.

use std::fs;
use std::path::{Path, PathBuf};

use json_viewer::state::{AppState, Msg, Tab};
use json_viewer::{io, json_ops, persist, runner};

#[test]
fn save_tabs_from_the_test_harness_does_not_touch_the_profile() {
    let path = persist::data_path();
    let before = fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    persist::save_tabs(&AppState::default().tabs).unwrap();
    let after = fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    assert_eq!(
        before, after,
        "test harness wrote the real profile at {}",
        path.display()
    );
}

fn tab(id: u64, title: &str, input: &str) -> Tab {
    let mut tab = Tab::new(id, title);
    tab.input = input.into();
    tab
}

fn payload(dir: &Path, id: u64) -> PathBuf {
    dir.join("tabs.d").join(format!("{id}.json"))
}

/// Pre-create every plausible index temp name as a directory so the next
/// `atomic_write` of `tabs.json` fails (EISDIR / IsADirectory) without
/// touching the committed index. Payload temps use a different prefix.
fn sabotage_index_writes(dir: &Path) {
    let pid = std::process::id();
    // Wider than any seq this process can reach: parallel tests share TEMP_SEQ.
    for seq in 0..8192 {
        let path = dir.join(format!("tabs.json.{pid}.{seq}.tmp"));
        let _ = fs::create_dir(&path);
    }
}

#[test]
fn c1_input_chunk_boundary_does_not_panic() {
    let mut state = AppState::default();
    // Probe fixture: '🚀' straddles byte 4_194_304 (PERSIST_CHUNK).
    let input = format!("\"{}🚀\"", "a".repeat((4 << 20) - 2));
    assert!(
        !input.is_char_boundary(4 * 1024 * 1024),
        "fixture must straddle the 4 MiB chunk edge"
    );
    state.update(Msg::InputChanged(input.clone()));
    // `on_close` would save through `persist::save_tabs` (this binary is not
    // `cfg(test)`) and overwrite the real profile. The copy is the panic site.
    let copied = runner::copied_tabs(&state);
    assert_eq!(copied[0].input, input);
}

#[test]
fn c1_output_chunk_boundary_does_not_panic() {
    let mut state = AppState::default();
    // 'é' is 2 bytes and starts at 4_194_303, so 4 MiB lands inside it.
    let output = format!("{}é", "b".repeat((4 << 20) - 1));
    assert!(!output.is_char_boundary(4 * 1024 * 1024));
    state.tabs[0].output = output.clone();
    let copied = runner::copied_tabs(&state);
    assert_eq!(copied[0].output, output);
}

#[test]
fn h1_failed_index_write_keeps_closed_tab_payload() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    let a = tab(1, "A", "alpha");
    let b = tab(2, "B", "beta");
    store.save(&[a.clone(), b]).unwrap();
    let index_before = fs::read(store.path()).unwrap();

    sabotage_index_writes(dir.path());
    let saved = store.save(std::slice::from_ref(&a));
    assert!(
        saved.is_err(),
        "index write must fail when its temp path is a directory, got {saved:?}"
    );
    assert_eq!(
        fs::read(store.path()).unwrap(),
        index_before,
        "failed index write changed the committed index"
    );
    assert!(
        payload(dir.path(), 2).exists(),
        "closed-tab payload was deleted before the index commit"
    );
    let loaded = store
        .load()
        .expect("store must stay loadable after a failed index write");
    assert_eq!(loaded.len(), 2, "load lost a tab: {loaded:?}");
    assert_eq!(loaded[0].input, "alpha");
    assert_eq!(loaded[1].input, "beta");
}

#[test]
fn h1_unreadable_payload_does_not_fail_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    let a = tab(1, "A", "recoverable work");
    let b = tab(2, "B", "doomed");
    store.save(&[a, b]).unwrap();
    fs::write(payload(dir.path(), 2), "{").unwrap();

    let loaded = store
        .load()
        .expect("one corrupt payload must not fail every tab");
    assert_eq!(loaded.len(), 1, "good tab was dropped: {loaded:?}");
    assert_eq!(loaded[0].id, 1);
    assert_eq!(loaded[0].input, "recoverable work");
    let warnings = persist::load_warnings(store.path());
    assert!(
        warnings.iter().any(|w| w.contains("tab 2")),
        "skipped payload was not surfaced: {warnings:?}"
    );
    assert!(
        payload(dir.path(), 1).exists(),
        "readable payload was removed during load"
    );
}

#[test]
fn h1_missing_payload_does_not_fail_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store
        .save(&[tab(1, "A", "keep-me"), tab(2, "B", "gone")])
        .unwrap();
    fs::remove_file(payload(dir.path(), 2)).unwrap();

    let loaded = store
        .load()
        .expect("a missing payload must not fail every tab");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].input, "keep-me");
}

#[test]
fn h1_boot_fallback_save_does_not_destroy_recoverable_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store
        .save(&[tab(1, "A", "recoverable work"), tab(2, "B", "also-work")])
        .unwrap();
    // Corrupt index: load fails, boot starts from a blank tab, the next save
    // of that fallback must not overwrite or delete the payloads.
    fs::write(store.path(), "{not json").unwrap();
    assert!(store.load().is_err(), "corrupt index should still fail load");

    let a_before = fs::read(payload(dir.path(), 1)).unwrap();
    let b_before = fs::read(payload(dir.path(), 2)).unwrap();
    let index_before = fs::read(store.path()).unwrap();

    let _ = store.save(&AppState::default().tabs);

    assert_eq!(
        fs::read(payload(dir.path(), 1)).unwrap(),
        a_before,
        "fallback save overwrote a recoverable payload"
    );
    assert_eq!(
        fs::read(payload(dir.path(), 2)).unwrap(),
        b_before,
        "fallback save overwrote or removed a recoverable payload"
    );
    assert_eq!(
        fs::read(store.path()).unwrap(),
        index_before,
        "fallback save replaced an unreadable index"
    );
}

#[test]
fn l2_utf8_bom_is_stripped_and_noted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bom.json");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(br#"{"ok":true}"#);
    fs::write(&path, bytes).unwrap();

    let text = io::read_json_file(&path).expect("BOM file should load");
    assert_eq!(text, r#"{"ok":true}"#, "leading U+FEFF was kept: {text:?}");
    assert!(
        json_ops::validate(&text).is_ok(),
        "stripped file should validate, got {:?}",
        json_ops::validate(&text)
    );
    let noted = io::display_name(&path);
    assert!(
        noted.contains("BOM"),
        "status name should note the stripped BOM, got {noted:?}"
    );
}

#[test]
fn l2_bom_note_reaches_opened_status() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bom.json");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend(br#"{"ok":true}"#);
    fs::write(&path, bytes).unwrap();
    let content = io::read_json_file(&path).unwrap();
    let mut state = AppState::default();
    state.dispatch(Msg::FileLoaded {
        path: path.clone(),
        content,
    });
    let mut tx = rune::Tx::default();
    runner::poll(&mut state, 0.0, &mut tx);
    let status = state.status.clone().unwrap_or_default();
    assert!(
        status.contains("UTF-8 BOM stripped"),
        "opened status should note the BOM, got {status:?}"
    );
    assert_eq!(
        state.active_tab().map(|tab| tab.input.as_str()),
        Some(r#"{"ok":true}"#)
    );
}

#[test]
fn crash_after_payload_rename_before_first_index_is_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store
        .save(&[tab(1, "A", "alpha-work"), tab(2, "B", "beta-work")])
        .unwrap();
    // Crash before the index commit of a first save: payloads exist, index does not.
    fs::remove_file(store.path()).unwrap();

    let loaded = store.load().expect("missing index with payloads must load");
    assert_eq!(loaded.len(), 2, "payloads lost when index was missing: {loaded:?}");
    assert_eq!(loaded[0].input, "alpha-work");
    assert_eq!(loaded[1].input, "beta-work");
}

#[test]
fn crash_after_index_tmp_before_rename_is_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store.save(&[tab(1, "A", "from-tmp")]).unwrap();
    let index_bytes = fs::read(store.path()).unwrap();
    fs::remove_file(store.path()).unwrap();
    // Complete temp (synced, not yet renamed). Name matches unique_temp's shape.
    fs::write(dir.path().join("tabs.json.1.7.tmp"), index_bytes).unwrap();

    let loaded = store
        .load()
        .expect("a complete index temp must be loadable when the index is missing");
    assert_eq!(loaded.len(), 1, "index temp was ignored: {loaded:?}");
    assert_eq!(loaded[0].input, "from-tmp");
}

#[test]
fn crash_after_payload_tmp_before_rename_keeps_old_and_recovers_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    let a = tab(1, "A", "old-alpha");
    let b = tab(2, "B", "old-beta");
    store.save(&[a.clone(), b]).unwrap();

    // In-progress rewrite of A: temp holds new bytes, destination still old.
    let mut a_new = a.clone();
    a_new.input = "new-alpha".into();
    fs::write(
        dir.path().join("tabs.d").join("1.json.9.3.tmp"),
        serde_json::to_vec(&a_new).unwrap(),
    )
    .unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(
        loaded[0].input, "old-alpha",
        "an uncommitted payload temp must not replace the committed file"
    );

    // Destination missing (crash after creating the temp, before the first
    // rename of a new payload): the complete temp is the only copy.
    let b_bytes = fs::read(payload(dir.path(), 2)).unwrap();
    fs::remove_file(payload(dir.path(), 2)).unwrap();
    fs::write(dir.path().join("tabs.d").join("2.json.4.1.tmp"), b_bytes).unwrap();
    let loaded = store
        .load()
        .expect("missing payload with a complete temp must load");
    assert_eq!(loaded.len(), 2, "payload temp was not recovered: {loaded:?}");
    assert_eq!(loaded[1].input, "old-beta");
}

#[test]
fn crash_after_index_rename_before_deletion_keeps_closed_payload() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store
        .save(&[tab(1, "A", "stay"), tab(2, "B", "closed-but-present")])
        .unwrap();
    // Committed index no longer names B; B's payload has not been deleted yet.
    // Hash is not checked on load.
    fs::write(
        store.path(),
        r#"{"version":1,"tabs":[{"id":1,"hash":1}]}"#,
    )
    .unwrap();

    let loaded = store.load().expect("committed index must load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].input, "stay");
    let b = fs::read_to_string(payload(dir.path(), 2)).unwrap();
    assert!(
        b.contains("closed-but-present"),
        "closed payload was destroyed before deletion finished: {b}"
    );
}

#[test]
fn crash_after_deletion_loads_remaining_tabs() {
    let dir = tempfile::tempdir().unwrap();
    let store = persist::TabsStore::new(dir.path());
    store
        .save(&[tab(1, "A", "stay"), tab(2, "B", "removed")])
        .unwrap();
    fs::write(
        store.path(),
        r#"{"version":1,"tabs":[{"id":1,"hash":1}]}"#,
    )
    .unwrap();
    fs::remove_file(payload(dir.path(), 2)).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].input, "stay");
}
