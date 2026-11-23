//! Regressions from the final review: search grammar, output reveal and restore.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use json_viewer::json_ops::{self, JsonAction};
use json_viewer::state::{AppState, Msg, Tab, TabView};
use json_viewer::{headless, line_index, search, tree_index, ui};
use rune::{Env, Runtime};

static GLOBALS: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    GLOBALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn search_accepts_the_same_extreme_numbers_surrogates_and_depth_as_validate() {
    let cases = [
        (
            "unpaired surrogate sibling",
            r#"{"target":"yes","other":"\uD800"}"#.to_owned(),
            "target",
        ),
        (
            "large exponent sibling",
            r#"{"target":"yes","other":1e400}"#.to_owned(),
            "target",
        ),
        (
            "large integer",
            r#"{"other":18446744073709551616123}"#.to_owned(),
            "18446744073709551616123",
        ),
        (
            "deep nesting",
            format!("{{\"target\":{}0{}}}", "[".repeat(150), "]".repeat(150)),
            "target",
        ),
    ];

    for (name, input, needle) in cases {
        assert!(json_ops::validate(&input).is_ok(), "{name} must be valid");
        let hits = search::search(
            &input,
            needle,
            search::SearchOptions::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(hits.len(), 1, "{name} returned no compatible search hit");
        assert!(hits[0].path.contains("target") || name == "large integer");
    }
}

#[test]
fn minified_search_hit_maps_to_the_existing_single_output_row() {
    let _guard = serial();
    let input = r#"{"a":1,"target":"needle"}"#;
    let output = json_ops::process(input, JsonAction::Minify).expect("minify fixture");
    let hits = search::search(
        input,
        "target",
        search::SearchOptions::default(),
        &AtomicBool::new(false),
    );
    assert_eq!(hits[0].line, Some(3));
    assert_eq!(output.lines().count(), 1);

    let mut state = AppState::default();
    let tab = state.active;
    state.update_input(tab, input.into());
    assert!(state.apply_json_result(tab, Ok(output)));
    state.set_view(tab, TabView::Input);
    state.apply_search_results(tab, hits);

    let mut runtime = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    runtime.frame(0.0);
    let (x, y) = runtime.find("search-hit-0").expect("search hit is visible");
    runtime.pointer_move(x, y);
    runtime.pointer_down(x, y);
    runtime.pointer_up(x, y);
    runtime.frame(0.0);

    assert_eq!(runtime.state.pending[0], Msg::SwitchView(TabView::Output));
    assert_eq!(
        runtime.state.pending.get(1),
        Some(&Msg::Status("target · line 1".into())),
        "a pretty-only row number cannot be revealed in minified output"
    );
}

#[test]
fn restored_large_output_gets_a_complete_line_index() {
    let _guard = serial();
    const LINES: usize = 4_500_000;
    let mut output = "x\n".repeat(LINES - 1);
    output.push('x');
    assert!(output.len() >= line_index::LARGE_DOCUMENT_BYTES);

    let mut restored = Tab::new(41, "restored");
    restored.output = output;
    restored.output_revision = 0;
    restored.view = TabView::Output;
    let actual_lines = restored.output.lines().count();
    let mut state = AppState::default();
    state.update(Msg::TabsLoaded(vec![restored]));
    line_index::prewarm_restored_output(&state.tabs[0].output);
    let mut runtime = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    runtime.frame(0.0);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = {
            let tab = runtime.state.active_tab().expect("restored tab");
            assert_eq!(tab.output_revision, 0);
            line_index::snapshot(&tab.output)
        };
        if !snapshot.is_partial() {
            assert_eq!(snapshot.line_count(), actual_lines);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored output stayed on the 64 KiB prefix"
        );
        runtime.frame(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
#[ignore = "generates a 100 MB fixture; run with cargo test --release -- --ignored --nocapture"]
fn restored_100mb_first_output_frame_stays_under_16ms() {
    let _guard = serial();
    let mut restored = Tab::new(42, "restored-100mb");
    restored.output = format!("{{\"data\":\"{}\"}}", "x".repeat(100 * 1024 * 1024));
    restored.output_revision = 0;
    restored.view = TabView::Output;
    let mut state = AppState::default();
    state.update(Msg::TabsLoaded(vec![restored]));

    let mut runtime = Runtime::with_text(
        AppState::default(),
        ui::view,
        headless::DEFAULT_SIZE,
        Env::new(true, 1.0),
        headless::text_system(),
    );
    runtime.set_srgb_blend(true);
    // Warm process-wide font/layout caches, then measure the restored Output
    // mount in a running app rather than Rune's one-time startup work.
    runtime.frame(0.0);
    runtime.settle_all();
    runtime.frame(0.0);
    // Match main::boot, which starts the job after loading persisted tabs and
    // before the window's first Output frame.
    line_index::prewarm_restored_output(&state.tabs[0].output);
    runtime.update(|current, _| *current = state);
    let started = Instant::now();
    runtime.frame(0.0);
    let first_frame_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("RESTORED_100MB_FIRST_OUTPUT_FRAME_MS={first_frame_ms:.3}");
    assert!(
        first_frame_ms < 16.0,
        "restored first Output frame blocked the UI thread: {first_frame_ms:.3} ms"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let complete = {
            let tab = runtime.state.active_tab().expect("restored tab");
            !line_index::snapshot(&tab.output).is_partial()
        };
        if complete {
            break;
        }
        assert!(Instant::now() < deadline, "restored prewarm did not finish");
        runtime.frame(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn tree_path_resolution_prefers_an_exact_key_over_separator_splitting() {
    let input = r#"{"a → target":"needle","a":{"target":0}}"#;
    let index = tree_index::TreeIndex::build(input).expect("tree index");
    let hits = search::search(
        input,
        "needle",
        search::SearchOptions::default(),
        &AtomicBool::new(false),
    );
    let exact = index.first_child(0).expect("root's exact-key child");
    assert_eq!(
        index.node_for_path(input, &hits[0].path),
        Some(exact),
        "the hit for the root key must not select the nested a/target branch"
    );
}

#[test]
fn tree_rejects_invalid_escapes_like_validate() {
    for input in [r#"{"label":"\q"}"#, r#"{"label":"\u12G4"}"#] {
        assert!(json_ops::validate(input).is_err());
        assert!(
            tree_index::TreeIndex::build(input).is_err(),
            "tree accepted a document Validate rejects: {input}"
        );
    }
    assert!(tree_index::TreeIndex::build(r#"{"label":"\uD800"}"#).is_ok());
}
