//! Validation parser, reducer, worker and rendered-panel evidence.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use json_viewer::json_ops::{self, JsonAction, JsonError, ValidationInfo};
use json_viewer::state::{AppState, Effect, Msg, TabView, ValidationResult};
use json_viewer::ui::validation_panel::error_snippet;
use json_viewer::{headless, runner, ui};
use rune::{Key, KeyEvent, Mods, Tx};
use serde_json::Value;

const MIB: usize = 1024 * 1024;
const SIZE: (f32, f32) = (1280.0, 800.0);

fn assert_error(input: &str, line: usize, column: usize, message: &str) -> JsonError {
    let error = json_ops::validate(input).expect_err("fixture must be invalid");
    assert_eq!((error.line, error.column), (line, column), "{error:?}");
    assert!(
        error.message.contains(message),
        "expected {message:?} in {error:?}"
    );
    error
}

#[test]
fn missing_comma_has_exact_position_and_human_message() {
    assert_error(
        "{\n  \"a\": 1\n  \"b\": 2\n}",
        3,
        3,
        "Expected ',' or '}' after property value",
    );
}

#[test]
fn trailing_comma_points_at_the_unexpected_close() {
    assert_error("[1,\n]", 2, 1, "Unexpected token ']'");
    assert_error("{\"a\":1,}", 1, 8, "Expected string property name");
}

#[test]
fn unclosed_brace_and_bracket_point_at_eof() {
    let object = "{\"a\": [1, 2]";
    assert_error(object, 1, object.len() + 1, "Unexpected end of JSON input");
    let array = "[{\"a\": 1}";
    assert_error(array, 1, array.len() + 1, "Unexpected end of JSON input");
}

#[test]
fn unterminated_string_points_at_eof() {
    let input = "{\"a\":\"abc";
    assert_error(input, 1, input.len() + 1, "Unexpected end of JSON input");
}

#[test]
fn bad_escape_points_at_the_escape_letter() {
    assert_error(r#"{"a":"\q"}"#, 1, 8, "Invalid escape sequence");
    assert_error(r#"{"a":"\u12G4"}"#, 1, 11, "Invalid unicode escape");
}

#[test]
fn invalid_literal_points_at_first_mismatch() {
    assert_error(r#"{"a": truX}"#, 1, 10, "Invalid literal, expected 'true'");
    assert_error("[true, falZe]", 1, 11, "Invalid literal, expected 'false'");
}

#[test]
fn unexpected_identifier_points_at_identifier_start() {
    let input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3\n}";
    let error = assert_error(input, 3, 8, "Unexpected token 'o'");
    let source_line = input.lines().nth(error.line - 1).unwrap();
    assert_eq!(&source_line[error.column - 1..], "oops,");
}

#[test]
fn garbage_after_document_has_exact_line_and_column() {
    assert_error(
        "{\"a\":1}\n true",
        2,
        2,
        "Unexpected token after top-level JSON value",
    );
}

#[test]
fn valid_statistics_include_bytes_lines_depth_and_values() {
    let input = "{\"a\":[1,true,null,{\"b\":\"x\"}],\"c\":{}}";
    let info = json_ops::validate(input).expect("valid fixture");
    assert_eq!(
        info,
        ValidationInfo {
            bytes: input.len(),
            lines: 1,
            max_depth: 3,
            value_count: 8,
        }
    );
    let multiline = "{\n  \"a\": 1\n}\n";
    assert_eq!(json_ops::validate(multiline).unwrap().lines, 4);
}

#[test]
fn large_single_line_error_has_exact_column_and_bounded_snippet() {
    let mut input = valid_fixture(6 * MIB);
    input.pop(); // closing }
    input.pop(); // closing ]
    input.push_str(",truX]}");
    let error_offset = input.len() - 3;
    let error = assert_error(
        &input,
        1,
        error_offset + 1,
        "Invalid literal, expected 'true'",
    );
    assert!(error.column > 6 * MIB);

    let snippet = error_snippet(&input, &error);
    assert!(snippet.line.starts_with('…'));
    assert!(snippet.line.contains("truX"));
    assert!(snippet.line.len() < 90, "snippet must stay bounded");
    assert_eq!(snippet.caret, format!("{}^", " ".repeat(41)));
}

#[test]
fn validation_effect_is_revisioned_and_does_not_replace_output() {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, r#"{"a":1}"#.into());
    state.tab_mut(id).unwrap().output = "old output".into();
    let revision = state.tab(id).unwrap().input_revision;
    let effects = state.update(Msg::Validate);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SpawnValidation { tab, input_revision, .. }]
            if *tab == id && *input_revision == revision
    ));
    assert!(state.tab(id).unwrap().is_processing);

    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: revision,
        result: Ok(ValidationInfo {
            bytes: 7,
            lines: 1,
            max_depth: 1,
            value_count: 2,
        }),
    });
    let tab = state.tab(id).unwrap();
    assert_eq!(tab.output, "old output");
    assert!(!tab.is_processing);
    assert!(matches!(tab.validation, Some(ValidationResult::Valid(_))));
}

#[test]
fn edits_dismiss_validation_and_stale_results_are_ignored() {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "{}".into());
    let old_revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: old_revision,
        result: Ok(json_ops::validate("{}").unwrap()),
    });
    assert!(state.tab(id).unwrap().validation.is_some());

    state.update_input(id, "[1]".into());
    assert_eq!(state.tab(id).unwrap().validation, None);
    let new_revision = state.tab(id).unwrap().input_revision;
    state.set_processing(id, true);
    assert!(!state.apply_validation_result(
        id,
        old_revision,
        Ok(json_ops::validate("{}").unwrap()),
    ));
    assert_eq!(state.tab(id).unwrap().input_revision, new_revision);
    assert_eq!(state.tab(id).unwrap().validation, None);
    assert!(!state.tab(id).unwrap().is_processing);
}

#[test]
fn format_and_minify_failures_retain_structured_error_for_the_panel() {
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, "[1 2]".into());
    let error = json_ops::process("[1 2]", JsonAction::Format).unwrap_err();
    assert!(!state.apply_json_result(id, Err(error.clone())));
    assert_eq!(state.tab(id).unwrap().validation_error(), Some(&error));
    assert_eq!(state.tab(id).unwrap().error, Some(error.to_string()));
}

#[test]
fn ctrl_shift_v_queues_validate() {
    let mut state = AppState::default();
    let event = KeyEvent {
        key: Key::Command('v'),
        mods: Mods {
            cmd: true,
            shift: true,
            ..Mods::default()
        },
    };
    assert!(ui::on_key(&mut state, &event, &mut Tx::default()));
    assert_eq!(state.take_pending(), vec![Msg::Validate]);
}

#[test]
fn validation_runs_off_the_calling_thread() {
    let mut state = AppState::default();
    state.update_input(state.active, r#"{"worker":true}"#.into());
    run_validation(&mut state);
    assert!(matches!(
        state.active_tab().unwrap().validation,
        Some(ValidationResult::Valid(_))
    ));
}

#[test]
fn headless_valid_alert_png() {
    let mut state = AppState::default();
    state.update_input(state.active, "{\n  \"a\": [1, true]\n}".into());
    run_validation(&mut state);
    let tree = render(state, "valid-alert");
    let message = element(&tree, "validation-success-message")["text"]
        .as_str()
        .expect("success text");
    assert!(message.starts_with("Valid JSON · "), "{message}");
    assert!(message.contains("3 lines · depth 2"), "{message}");
    assert_eq!(element(&tree, "validation-success")["role"], "alert");
}

#[test]
fn headless_format_error_panel_png_and_go_to_error() {
    let input = "{\n  \"a\": 1\n  \"b\": 2\n}";
    let error = json_ops::process(input, JsonAction::Format).unwrap_err();
    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input.into());
    state.apply_json_result(id, Err(error));

    let tree = render(state.clone(), "error-small");
    let message = element(&tree, "error-message")["text"]
        .as_str()
        .expect("error message");
    assert_eq!(
        message,
        "Invalid JSON: Expected ',' or '}' after property value"
    );
    assert!(
        !message.contains("at line"),
        "position shown twice: {message}"
    );
    assert_eq!(element(&tree, "error-location")["text"], "Line 3, column 3");
    assert!(element(&tree, "error-previous-line")["text"]
        .as_str()
        .is_some_and(|text| text.starts_with("2 │") && text.contains("\"a\": 1")));
    assert!(element(&tree, "error-snippet")["text"]
        .as_str()
        .is_some_and(|text| text.starts_with("3 │") && text.contains("\"b\": 2")));
    assert_eq!(element(&tree, "error-caret")["text"], "  │   ^");
    assert!(!tree.to_string().contains("Previous line"));

    let mut runtime = headless::runtime(state, SIZE, 1.0);
    click(&mut runtime, "go-to-error");
    for _ in 0..4 {
        runtime.update_if(|state, tx| runner::poll(state, 0.0, tx));
        runtime.frame(1.0 / 60.0);
    }
    assert_eq!(runtime.state.active_tab().unwrap().view, TabView::Input);
    let expected = input.find("\"b\"").unwrap();
    assert_eq!(runtime.state.editor(id).unwrap().focus, expected);
}

#[test]
fn validate_error_marker_and_go_to_error_use_the_real_position() {
    let input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3\n}";
    let error = json_ops::validate(input).expect_err("invalid fixture");
    assert_eq!((error.line, error.column), (3, 8));

    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input.into());
    let revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: revision,
        result: Err(error),
    });

    let tree = render(state.clone(), "error-marker-panel");
    assert_eq!(element(&tree, "error-location")["text"], "Line 3, column 8");
    assert_eq!(
        element(&tree, "editor-1-error-label")["text"],
        "line 3, column 8"
    );
    assert!(element(&tree, "editor-1-error-dot")["key"] == "editor-1-error-dot");

    let mut runtime = headless::runtime(state, SIZE, 1.0);
    click(&mut runtime, "go-to-error");
    for _ in 0..4 {
        runtime.update_if(|state, tx| runner::poll(state, 0.0, tx));
        runtime.frame(1.0 / 60.0);
    }
    assert_eq!(runtime.state.active_tab().unwrap().view, TabView::Input);
    assert_eq!(
        runtime.state.editor(id).unwrap().focus,
        input.find("oops").unwrap()
    );
    assert!(runtime.find("editor-1-error-dot").is_some());
}

#[test]
fn go_to_error_reveals_the_error_line_in_the_rope_editor() {
    let mut input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3,\n  \"pad\": \"".to_owned();
    input.push_str(&"x".repeat(6 * MIB));
    input.push_str("\"\n}\n");
    let error = json_ops::validate(&input).expect_err("invalid fixture");
    assert_eq!((error.line, error.column), (3, 8));
    let expected_caret = input.find("oops").unwrap();

    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input);
    let revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: revision,
        result: Err(error),
    });
    // rp-editor: the 6 MB single-line input is editable in the rope editor;
    // there is no read-only banner anymore.
    let mut runtime = headless::runtime(state, SIZE, 1.0);
    assert!(!headless::element_tree(&runtime)
        .to_string()
        .contains("Very long line"));

    click(&mut runtime, "go-to-error");
    for _ in 0..8 {
        runtime.update_if(|state, tx| runner::poll(state, 0.0, tx));
        runtime.settle_all();
        runtime.frame(1.0 / 60.0);
    }
    assert_eq!(runtime.state.active_tab().unwrap().view, TabView::Input);
    // The error line is revealed and marked, and the caret sits on it.
    assert!(runtime.find("editor-1-row-2").is_some());
    assert!(runtime.find("editor-1-error-dot-2").is_some());
    let tab = runtime.state.active_tab().unwrap();
    let caret = json_viewer::editor::state::with(tab.id, &tab.input, tab.input_revision, |ed| {
        ed.caret
    });
    assert_eq!(caret, expected_caret);
}

#[test]
fn headless_large_single_line_error_panel_png() {
    let mut input = valid_fixture(6 * MIB);
    input.pop();
    input.pop();
    input.push_str(",truX]}");
    let error_offset = input.len() - 3;
    let error = json_ops::validate(&input).unwrap_err();
    assert_eq!((error.line, error.column), (1, error_offset + 1));

    let mut state = AppState::default();
    let id = state.active;
    state.update_input(id, input);
    let revision = state.tab(id).unwrap().input_revision;
    state.update(Msg::ValidationDone {
        tab: id,
        input_revision: revision,
        result: Err(error),
    });
    let tree = render(state, "error-large-single-line");
    assert_eq!(
        element(&tree, "error-location")["text"],
        format!("Line 1, column {}", error_offset + 1)
    );
    let snippet = element(&tree, "error-snippet")["text"]
        .as_str()
        .expect("large error snippet");
    assert!(snippet.len() < 100);
    assert!(snippet.starts_with("1 │"));
    assert!(snippet.contains("truX"));
    assert_eq!(
        element(&tree, "error-caret")["text"].as_str().unwrap(),
        format!("  │ {}^", " ".repeat(41))
    );
}

#[test]
#[ignore = "release timing evidence; run with --release --ignored --nocapture"]
fn release_timing_validate_7_and_100_mb() {
    for mib in [7usize, 100] {
        let fixture = valid_fixture(mib * MIB);
        for run in 1..=3 {
            let started = Instant::now();
            let info = json_ops::validate(&fixture).expect("valid timing fixture");
            let elapsed = started.elapsed();
            assert_eq!(info.bytes, fixture.len());
            println!(
                "validate {mib} MiB run {run}: {:.3} ms ({:.1} MiB/s)",
                elapsed.as_secs_f64() * 1000.0,
                mib as f64 / elapsed.as_secs_f64()
            );
        }
    }
}

fn run_validation(state: &mut AppState) {
    let effects = state.update(Msg::Validate);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SpawnValidation { .. }]
    ));
    runner::execute(state, &mut Tx::default(), effects);
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.active_tab().unwrap().validation.is_none() {
        runner::poll(state, 0.0, &mut Tx::default());
        assert!(Instant::now() < deadline, "validation worker timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn valid_fixture(target_bytes: usize) -> String {
    let mut output = String::with_capacity(target_bytes + 128);
    output.push_str("{\"models\":[");
    let mut index = 0usize;
    while output.len() < target_bytes {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"id\":{index},\"name\":\"model-{index:06}\",\"tags\":[\"alpha\",\"beta\"],\"ok\":true}}"
        )
        .expect("write fixture");
        index += 1;
    }
    output.push_str("]}");
    output
}

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn render(state: AppState, name: &str) -> Value {
    let directory = target_dir().join("headless/validate");
    std::fs::create_dir_all(&directory).expect("create validation evidence directory");
    let png = directory.join(format!("{name}.png"));
    let tree_path = directory.join(format!("{name}-tree.json"));
    let tree = headless::render_and_dump(state, SIZE, 1.0, &png, &tree_path)
        .unwrap_or_else(|error| panic!("render {name} failed: {error:#}"));
    assert!(png.metadata().expect("PNG metadata").len() > 0);
    assert!(tree_path.exists());
    tree
}

fn element<'a>(tree: &'a Value, key: &str) -> &'a Value {
    tree["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|element| element["key"] == key))
        .unwrap_or_else(|| panic!("missing element {key}: {tree}"))
}

fn click(runtime: &mut rune::Runtime<AppState>, key: &str) {
    let (x, y) = runtime.find(key).unwrap_or_else(|| panic!("missing {key}"));
    runtime.pointer_move(x, y);
    runtime.pointer_down(x, y);
    runtime.pointer_up(x, y);
    runtime.frame(1.0 / 60.0);
}
