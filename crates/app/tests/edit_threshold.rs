//! Measurement harness: where does Rune's native `TextEdit` stop being
//! smooth for the JSON input editor?
//!
//! Builds a headless `Runtime` with the app's real view composition and one
//! tab holding a generated ASCII JSON document. A test-local copy of the
//! native `rune::text_input` branch bypasses both read-only gates, preserving
//! the historical minified measurements after rp-fold added its line-length
//! gate, and measures in release:
//!
//! - load (`AppState::update_input`) and time to first frame with the editor
//!   mounted (`Runtime::with_text` + first `frame`),
//! - per-keystroke cost (`Runtime::key` + `runner::poll` + `frame`) for 20
//!   chars typed at the start, middle and end,
//! - caret move + instant scroll to the middle, and a 1 KB paste,
//! - a manual-timer cost breakdown (Rune before-clone, model insert + undo
//!   snapshot, app `sync_active_editor`, `update_input`, idle frame),
//! - peak RSS (`/proc/self/status` VmHWM, process-wide and cumulative).
//!
//! Sizes: pretty-printed 0.5/1/2/5/10/25/50 MiB plus single-line minified
//! 2/10 MiB. A 1 MiB control run uses the real `ui::view` (below the 5 MiB
//! `EDITABLE_LIMIT_BYTES`, so its editable path is the shipped one) to show
//! the forced-editable harness matches the real app.
//!
//! `frame` is the CPU commit/layout/instance work; GPU draw is not included
//! (the recommendation budgets are about input latency, not presentation).
//!
//! Run with:
//! `cargo test --release -p json-viewer --test edit_threshold -- --ignored --nocapture`

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use json_viewer::state::{AppState, TabView};
use json_viewer::ui::text_view::{TextView, LARGE_FILE_BANNER};
use json_viewer::ui::{search_bar, shell, tabs, toolbar};
use json_viewer::{headless, io, runner, ui};
use rune::{col, icon, row, text, tok, Color, Cx, El, Env, Key, KeyEvent, Mods, Runtime, TextEdit, TextStyle};
use rune_kit::glyphs;

const SCALE: f32 = 1.0;
const MIB: usize = 1024 * 1024;
/// Keystroke p95 budget from the task: one 60 Hz frame.
const KEY_BUDGET_MS: f64 = 16.0;
/// First-frame budget from the task.
const FIRST_FRAME_BUDGET_MS: f64 = 250.0;
/// A single keystroke slower than this aborts the remaining samples for the
/// size (the measurement itself must not run for hours).
const SAMPLE_WATCHDOG_MS: f64 = 120_000.0;
/// Above this first-frame time, type 5 chars per position instead of 20.
const ADAPTIVE_FIRST_FRAME_MS: f64 = 20_000.0;

// Linear sRGB values copied from `ui/mod.rs` (pub(crate) there) so the forced
// editable view paints exactly like the real one.
const PANEL_BG: Color = Color { r: 0.00802319, g: 0.0159963, b: 0.0343398, a: 1.0 };
const EDITOR_BG: Color = Color { r: 0.00477695, g: 0.00856813, b: 0.0231534, a: 1.0 };
const BORDER: Color = Color { r: 0.0331048, g: 0.0528606, b: 0.0908417, a: 1.0 };
const TEXT: Color = Color { r: 0.760525, g: 0.806952, b: 0.871367, a: 1.0 };
const MUTED: Color = Color { r: 0.296138, g: 0.366253, b: 0.47932, a: 1.0 };
const DANGER: Color = Color { r: 0.938686, g: 0.165132, b: 0.165132, a: 1.0 };
const DANGER_BG: Color = Color { r: 0.0356013, g: 0.0109601, b: 0.0193824, a: 1.0 };

/// `fold_index` keeps one process-wide store (fold states, pending reveals
/// keyed by view base, hover, LRU) and `line_index` one process-wide snapshot
/// LRU. The app is single-threaded, but libtest runs these tests in parallel
/// inside one process: a concurrent Output render can consume another test's
/// pending reveal or evict a warmed revision. Serialize the benches.
static FOLD_STORE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    FOLD_STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `ui::view` with the input editor's read-only gates lifted. Everything else
/// — shell, toolbar, tab strip, search, handlers, scroll viewport — is the
/// real composition; only `tab_content` mounts Rune's native input directly.
fn bench_view(state: &AppState, cx: &mut Cx<AppState>) -> El {
    let document = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(PANEL_BG)
        .radius(10.0)
        .border(1.0, BORDER)
        .clip_rounded(10.0)
        .child(toolbar::toolbar(state, cx))
        .child(bench_tab_content(state));
    let body = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(search_bar::search_bar(state, cx))
        .child(tabs::tab_bar(state, cx))
        .child(search_bar::search_results(state, cx))
        .child(document);
    let header = shell::header(state, cx);
    io::drop_zone(shell::shell(state, cx, header, body)).on_key(ui::on_key)
}

/// Copy of `tabs::tab_content` whose input is the same native text field used
/// by `TextView`'s editable branch. Direct construction is intentional: the
/// shipped long-line gate cannot be overridden by `editable_limit`, and this
/// historical bench must still measure pathological minified input.
fn bench_tab_content(state: &AppState) -> El {
    let Some(tab) = state.active_tab() else {
        return col().w_full().grow(1.0).fill(EDITOR_BG);
    };
    let body = match tab.view {
        TabView::Input => {
            let edit = state.editor(tab.id).expect("every tab has an editor");
            let theme = tok::theme(true);
            rune::text_input(edit, TextStyle::new(12.0, 400.0).mono())
                .key(format!("editor-{}", tab.id))
                .label("JSON input")
                .w_full()
                .min_h(240.0)
                .padding(12.0)
                .fill(theme.card)
                .color(theme.text)
                .caret(theme.text, tok::accent(0, true))
                .placeholder("Paste your JSON here or drop a JSON file…", MUTED)
                .fill(EDITOR_BG)
                .padding(16.0)
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .on_edit(|state: &mut AppState| {
                    let id = state.active;
                    state
                        .editors
                        .get_mut(&id)
                        .expect("active tab has an editor")
                })
                .on_input_change(|state: &mut AppState, _| state.sync_active_editor())
                .on_input_key(ui::on_key)
        }
        TabView::Output => TextView::read_only(&tab.output)
            .revision(tab.output_revision)
            .line_numbers(true)
            .visible_lines(200)
            .highlight(&tab.search_query)
            .build("output-text")
            .w_full()
            .grow(1.0)
            .fill(EDITOR_BG),
    };
    let scroller = col()
        .key(format!("document-scroll-{}", tab.id))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(EDITOR_BG)
        .scroll_xy()
        .child(body);

    let mut content = col().w_full().grow(1.0).min_h(0.0).fill(EDITOR_BG);
    if let Some(error) = &tab.error {
        content = content.child(
            row()
                .key("error-alert")
                .role("alert")
                .w_full()
                .gap(10.0)
                .padding(12.0)
                .center_y()
                .fill(DANGER_BG)
                .border_sides([0.0, 0.0, 1.0, 0.0], DANGER.alpha(0.6))
                .child(icon(&glyphs::CIRCLE_X, 18.0).color(DANGER).a11y_hidden())
                .child(
                    col()
                        .gap(2.0)
                        .child(
                            text("Parse error")
                                .font(TextStyle::new(12.0, 600.0))
                                .color(DANGER),
                        )
                        .child(
                            text(format!("Invalid JSON: {error}"))
                                .key("error-message")
                                .font(tok::type_::BODY)
                                .color(TEXT),
                        ),
                ),
        );
    }
    content.child(scroller)
}

/// Pretty-printed ASCII JSON array of records, ~170 bytes each, grown to
/// `target` bytes (within one record). 2-space indent like `json_ops::Format`.
fn pretty_fixture(target: usize) -> String {
    let mut out = String::with_capacity(target + 4096);
    out.push_str("{\n  \"items\": [\n");
    let mut i = 0usize;
    while out.len() < target.saturating_sub(300) {
        if i > 0 {
            out.push_str(",\n");
        }
        write!(
            out,
            "    {{\n      \"id\": {i},\n      \"name\": \"item-{i:07}\",\n      \
             \"description\": \"bench record {i:07} with some plain ascii text\",\n      \
             \"tags\": [\"alpha\", \"beta\"],\n      \"value\": {}.{}}}\n    }}",
            i % 1000,
            i % 100
        )
        .expect("write fixture");
        i += 1;
    }
    out.push_str("\n  ]\n}");
    out
}

/// Single-line minified ASCII JSON, same records, grown to `target` bytes.
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

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.total_cmp(b));
    percentile(samples, 0.5)
}

/// `/proc/self/status` field in MiB (VmHWM is the process-wide high-water
/// mark, so it is cumulative across the sizes measured in this process).
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

struct Breakdown {
    /// `TextEdit.text.clone()` — Rune's change-detection before-clone per key.
    text_clone_ms: f64,
    /// Full `TextEdit::clone()` as `text_input` does per view build.
    edit_clone_ms: f64,
    /// `TextEdit::display()` — display string construction per view build.
    display_ms: f64,
    /// First `insert` on a fresh editor: model edit + full undo snapshot.
    insert_first_ms: f64,
    /// Second consecutive `insert`: joins the open typing run (no snapshot).
    insert_join_ms: f64,
    /// App-side `sync_active_editor`: clone + compare (+ assign when differs).
    sync_ms: f64,
    /// `update_input` with changed text: clone + `replace_text` (newline scan,
    /// per-line blocks vec, history reset). The `Msg::InputChanged` path.
    update_input_ms: f64,
    /// A frame with no input at all: view rebuild + layout with warm caches.
    idle_frame_ms: f64,
}

struct VariantResult {
    label: String,
    view: &'static str,
    bytes: usize,
    lines: usize,
    load_ms: f64,
    first_frame_ms: f64,
    settle_ms: f64,
    chars_per_pos: usize,
    key_median_ms: f64,
    key_p95_ms: f64,
    key_max_ms: f64,
    per_pos_median: Vec<(&'static str, f64)>,
    caret_scroll_ms: f64,
    paste_ms: f64,
    aborted: bool,
    breakdown: Breakdown,
    vmhwm_mb: Option<f64>,
    vmrss_mb: Option<f64>,
}

fn bench_variant(label: &str, text: String, forced: bool) -> VariantResult {
    let bytes = text.len();
    let lines = text.bytes().filter(|b| *b == b'\n').count() + 1;

    // Load through the app's own reducer path (Msg::FileLoaded semantics).
    let mut state = AppState::default();
    let id = state.active;
    let t = Instant::now();
    state.update_input(id, text);
    let load_ms = ms(t);

    let view: fn(&AppState, &mut Cx<AppState>) -> El = if forced { bench_view } else { ui::view };
    let t = Instant::now();
    let mut rt = Runtime::with_text(
        state,
        view,
        headless::DEFAULT_SIZE,
        Env::new(true, SCALE),
        headless::text_system(),
    );
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    let first_frame_ms = ms(t);
    let t = Instant::now();
    rt.settle_all();
    rt.frame(0.0);
    let settle_ms = ms(t);

    let editor_key = format!("editor-{id}");
    assert!(
        !rt
            .elements()
            .iter()
            .any(|e| e.text.as_deref() == Some(LARGE_FILE_BANNER)),
        "{label}: editable view fell back to the read-only banner"
    );

    // Keyboard focus without geometry: the editor may be taller than the window.
    rt.update(|_, tx| {
        tx.focus(editor_key.clone(), true);
    });
    rt.frame(0.0);
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.key.as_deref() == Some(editor_key.as_str()) && e.focused),
        "{label}: editor did not take keyboard focus"
    );

    let chars_per_pos = if first_frame_ms > ADAPTIVE_FIRST_FRAME_MS { 5 } else { 20 };
    let mut samples: Vec<f64> = Vec::new();
    let mut per_pos_median = Vec::new();
    let mut aborted = false;
    let mut expected_len = rt.state.active_tab().expect("active tab").input.len();
    for (pos, caret) in [
        ("start", 0usize),
        ("middle", expected_len / 2),
        ("end", expected_len),
    ] {
        rt.update(|s, _| {
            s.editors.get_mut(&id).expect("editor").set_caret(caret);
        });
        rt.frame(0.0);
        let mut pos_samples = Vec::new();
        for k in 0..chars_per_pos {
            let ch = char::from(b'a' + (k % 26) as u8);
            let t = Instant::now();
            rt.key(KeyEvent {
                key: Key::Text(ch.to_string()),
                mods: Mods::default(),
            });
            rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
            rt.frame(1.0 / 60.0);
            let d = ms(t);
            pos_samples.push(d);
            samples.push(d);
            if d > SAMPLE_WATCHDOG_MS {
                aborted = true;
                break;
            }
        }
        expected_len += pos_samples.len();
        let tab = rt.state.active_tab().expect("active tab");
        assert_eq!(
            tab.input.len(),
            expected_len,
            "{label}: typed chars did not land in the model at {pos}"
        );
        assert_eq!(
            rt.state.active_editor().expect("editor").text.len(),
            expected_len,
            "{label}: editor and model diverged at {pos}"
        );
        per_pos_median.push((pos, median(&mut pos_samples)));
        if aborted {
            break;
        }
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let key_median_ms = percentile(&samples, 0.5);
    let key_p95_ms = percentile(&samples, 0.95);
    let key_max_ms = samples.last().copied().unwrap_or(f64::NAN);

    // Caret move + instant scroll to the middle of the document.
    let mid = rt.state.active_tab().expect("active tab").input.len() / 2;
    let editor_h = rt
        .elements()
        .iter()
        .find(|e| e.key.as_deref() == Some(editor_key.as_str()))
        .map(|e| e.rect[3])
        .unwrap_or(0.0);
    let t = Instant::now();
    rt.update(|s, tx| {
        s.editors.get_mut(&id).expect("editor").set_caret(mid);
        tx.scroll_to_y(format!("document-scroll-{id}"), (editor_h * 0.5 - 200.0).max(0.0));
    });
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    let caret_scroll_ms = ms(t);

    // Paste 1 KB at the middle caret.
    let before = rt.state.active_tab().expect("active tab").input.len();
    let t = Instant::now();
    rt.paste("x".repeat(1024));
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    let paste_ms = ms(t);
    assert_eq!(
        rt.state.active_tab().expect("active tab").input.len(),
        before + 1024,
        "{label}: paste did not land"
    );

    // ── Cost breakdown (manual timers on the live document) ──────────────
    let edit = rt.state.active_editor().expect("editor");
    let t = Instant::now();
    let _text = edit.text.clone();
    let text_clone_ms = ms(t);
    let t = Instant::now();
    let cloned = edit.clone();
    let edit_clone_ms = ms(t);
    let t = Instant::now();
    let _display = cloned.display();
    let display_ms = ms(t);
    drop(cloned);
    drop(_text);

    let mut scratch = TextEdit::new(rt.state.active_tab().expect("active tab").input.clone());
    let t = Instant::now();
    scratch.insert("x");
    let insert_first_ms = ms(t);
    let t = Instant::now();
    scratch.insert("y");
    let insert_join_ms = ms(t);
    drop(scratch);

    let mut sync_ms = 0.0;
    rt.update(|s, _| {
        let t = Instant::now();
        s.sync_active_editor();
        sync_ms = ms(t);
    });
    let mut update_input_ms = 0.0;
    rt.update(|s, _| {
        let mut txt = s.editors.get(&id).expect("editor").text.clone();
        txt.push('z');
        let t = Instant::now();
        s.update_input(id, txt);
        update_input_ms = ms(t);
    });

    rt.settle_all();
    let t = Instant::now();
    rt.frame(1.0 / 60.0);
    let idle_frame_ms = ms(t);

    let vmhwm_mb = proc_status_mb("VmHWM:");
    let vmrss_mb = proc_status_mb("VmRSS:");
    drop(rt);

    VariantResult {
        label: label.to_string(),
        view: if forced { "forced" } else { "real" },
        bytes,
        lines,
        load_ms,
        first_frame_ms,
        settle_ms,
        chars_per_pos,
        key_median_ms,
        key_p95_ms,
        key_max_ms,
        per_pos_median,
        caret_scroll_ms,
        paste_ms,
        aborted,
        breakdown: Breakdown {
            text_clone_ms,
            edit_clone_ms,
            display_ms,
            insert_first_ms,
            insert_join_ms,
            sync_ms,
            update_input_ms,
            idle_frame_ms,
        },
        vmhwm_mb,
        vmrss_mb,
    }
}

fn passes(r: &VariantResult) -> bool {
    !r.aborted && r.key_p95_ms < KEY_BUDGET_MS && r.first_frame_ms < FIRST_FRAME_BUDGET_MS
}

fn recommend(results: &[VariantResult], prefix: &str) -> String {
    let mut passing: Vec<&VariantResult> = results
        .iter()
        .filter(|r| r.label.starts_with(prefix) && r.view == "forced" && passes(r))
        .collect();
    passing.sort_by_key(|r| r.bytes);
    let failing: Vec<&VariantResult> = results
        .iter()
        .filter(|r| r.label.starts_with(prefix) && r.view == "forced" && !passes(r))
        .collect();
    match (passing.last(), failing.first()) {
        (Some(best), Some(worst)) => format!(
            "{prefix}: largest passing size {:.1} MiB (p95 {:.1} ms, first frame {:.0} ms); \
             next tested size {:.1} MiB fails (p95 {:.1} ms, first frame {:.0} ms). \
             Keep the threshold at or below {:.1} MiB.",
            best.bytes as f64 / MIB as f64,
            best.key_p95_ms,
            best.first_frame_ms,
            worst.bytes as f64 / MIB as f64,
            worst.key_p95_ms,
            worst.first_frame_ms,
            best.bytes as f64 / MIB as f64,
        ),
        (Some(best), None) => format!(
            "{prefix}: all tested sizes pass; largest is {:.1} MiB (p95 {:.1} ms, first frame {:.0} ms).",
            best.bytes as f64 / MIB as f64,
            best.key_p95_ms,
            best.first_frame_ms,
        ),
        (None, Some(worst)) => format!(
            "{prefix}: even {:.1} MiB fails (p95 {:.1} ms, first frame {:.0} ms); threshold must go lower.",
            worst.bytes as f64 / MIB as f64,
            worst.key_p95_ms,
            worst.first_frame_ms,
        ),
        (None, None) => format!("{prefix}: no results"),
    }
}

#[test]
#[ignore]
fn edit_threshold_bench() {
    let _guard = serial();
    let mut results = Vec::new();

    // Control: the forced native path at 1 MiB. (The real `ui::view` now
    // mounts the rope editor at every size — see `rope_editor_bench` — so
    // the historical control uses the same forced native composition as
    // the other rows.)
    results.push(bench_variant("pretty 1 control", pretty_fixture(MIB), true));
    for mb in [0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0] {
        let target = (mb * MIB as f64) as usize;
        results.push(bench_variant(&format!("pretty {mb}"), pretty_fixture(target), true));
    }
    for mb in [2.0, 10.0] {
        let target = (mb * MIB as f64) as usize;
        results.push(bench_variant(
            &format!("minified {mb}"),
            minified_fixture(target),
            true,
        ));
    }

    println!("\n## Rune native TextEdit — edit threshold bench (release, scale 1, 1280x800)\n");
    println!(
        "Keystroke = `Runtime::key` + `runner::poll` + `Runtime::frame(1/60)` (CPU only, no GPU draw). \
         {} chars per position unless noted. VmHWM is process-wide/cumulative.\n",
        results[0].chars_per_pos
    );
    println!("| Variant | View | MiB | Lines | Load ms | First frame ms | Settle ms | Key median ms | Key p95 ms | Key max ms | Caret+scroll ms | Paste 1KB ms | VmHWM MiB | VmRSS MiB |");
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for r in &results {
        println!(
            "| {}{} | {} | {:.2} | {} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} | {:.1} | {:.1} | {} | {} |",
            r.label,
            if r.aborted { " (watchdog-aborted)" } else { "" },
            r.view,
            r.bytes as f64 / MIB as f64,
            r.lines,
            r.load_ms,
            r.first_frame_ms,
            r.settle_ms,
            r.key_median_ms,
            r.key_p95_ms,
            r.key_max_ms,
            r.caret_scroll_ms,
            r.paste_ms,
            r.vmhwm_mb.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
            r.vmrss_mb.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
        );
    }

    println!("\n### Per-position keystroke medians (ms)\n");
    println!("| Variant | Start | Middle | End | Chars/pos |");
    println!("| --- | ---: | ---: | ---: | ---: |");
    for r in &results {
        let get = |pos: &str| {
            r.per_pos_median
                .iter()
                .find(|(p, _)| *p == pos)
                .map(|(_, v)| format!("{v:.2}"))
                .unwrap_or_else(|| "-".into())
        };
        println!(
            "| {} | {} | {} | {} | {} |",
            r.label,
            get("start"),
            get("middle"),
            get("end"),
            r.chars_per_pos
        );
    }

    println!("\n### Cost breakdown per keystroke (ms, manual timers)\n");
    println!(
        "Typing path: Rune clones `edit.text` before the edit to detect change, applies the insert \
         (undo snapshot on the first key of a typing run), calls `on_input_change` → \
         `sync_active_editor` (clone + compare), then the frame rebuilds the view (`text_input` \
         clones the TextEdit and re-shapes). No search re-run and no `Effect::Persist` on typing: \
         `Msg::InputChanged` is not dispatched by the editor and its reducer returns no effects.\n"
    );
    println!("| Variant | Key median | text.clone (Rune before) | insert first (undo snap) | insert join | sync_active_editor | TextEdit.clone (view) | display() | Idle frame | Residual* | update_input† |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for r in &results {
        let b = &r.breakdown;
        let residual = r.key_median_ms - b.text_clone_ms - b.insert_join_ms - b.sync_ms;
        println!(
            "| {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            r.label,
            r.key_median_ms,
            b.text_clone_ms,
            b.insert_first_ms,
            b.insert_join_ms,
            b.sync_ms,
            b.edit_clone_ms,
            b.display_ms,
            b.idle_frame_ms,
            residual,
            b.update_input_ms,
        );
    }
    println!(
        "\n*Residual = key median − (before-clone + joined insert + sync): shaping, layout, diff, \
         instance build. Negative means warm-cache frames beat the cold component timers.\n\
         †`update_input` with changed text (the hypothetical `Msg::InputChanged` path): clone + \
         `replace_text` (newline scan + per-line blocks vec + history reset). Not on the typing hot path.\n"
    );

    println!("\n### Recommendation (p95 keystroke < {KEY_BUDGET_MS} ms, first frame < {FIRST_FRAME_BUDGET_MS} ms)\n");
    println!("- {}", recommend(&results, "pretty"));
    println!("- {}", recommend(&results, "minified"));
    let control = &results[0];
    let forced_1 = results
        .iter()
        .find(|r| r.label == "pretty 1")
        .expect("pretty 1 result");
    println!(
        "\nControl check: forced native at 1 MiB (first run, warm-up) — first frame {:.1} ms, key median {:.2} ms; \
         forced native at 1 MiB (second run) — first frame {:.1} ms, key median {:.2} ms. \
         The shipped input editor is now the rope editor (`rope_editor_bench`).",
        control.first_frame_ms,
        control.key_median_ms,
        forced_1.first_frame_ms,
        forced_1.key_median_ms
    );
}

// ── rp-editor E5: rope-editor performance proof ──────────────────────────
//
// The real `ui::view` (rope editor, no gates) at 1/10/100 MB pretty and the
// 6.9 MB single line. Keystroke sample = `Runtime::key` + frame (handler
// dispatch) + `runner::poll` (reducer applies) + frame (render): the full
// input-to-rendered-state CPU cost, conservative against the native
// harness's single-frame sample. Targets: p95 keystroke < 16 ms and first
// frame < 250 ms for every size.

use json_viewer::editor::state as editor_state;
use rune::render::{write_png, Gpu, Offscreen, Renderer, OFFSCREEN_FORMAT};

struct RopeResult {
    label: String,
    bytes: usize,
    lines: usize,
    load_ms: f64,
    first_frame_ms: f64,
    settle_ms: f64,
    key_median_ms: f64,
    key_p95_ms: f64,
    key_max_ms: f64,
    per_pos_median: Vec<(&'static str, f64)>,
    caret_scroll_ms: f64,
    paste_1kb_ms: f64,
    paste_10mb_ms: f64,
    undo_10mb_ms: f64,
    select_all_delete_ms: f64,
    vmhwm_mb: Option<f64>,
    vmrss_mb: Option<f64>,
}

impl RopeResult {
    fn passes(&self) -> bool {
        self.key_p95_ms < KEY_BUDGET_MS && self.first_frame_ms < FIRST_FRAME_BUDGET_MS
    }
}

fn rope_buffer_len(rt: &Runtime<AppState>) -> usize {
    let tab = rt.state.active_tab().expect("active tab");
    editor_state::with(tab.id, &tab.input, tab.input_revision, |ed| ed.buffer.len_bytes())
}

fn rope_place_caret(rt: &mut Runtime<AppState>, id: u64, caret: usize) {
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
        tx.scroll_to_y(format!("document-scroll-{id}"), (line as f32 * 16.0 - 200.0).max(0.0));
    });
    rt.frame(0.0);
    rt.frame(0.0);
}

fn rope_key(rt: &mut Runtime<AppState>, key: Key, mods: Mods) -> f64 {
    let t = Instant::now();
    rt.key(KeyEvent { key, mods });
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    ms(t)
}

fn rope_paste(rt: &mut Runtime<AppState>, text: String) -> f64 {
    let t = Instant::now();
    rt.paste(text);
    rt.frame(1.0 / 60.0);
    rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
    rt.frame(1.0 / 60.0);
    ms(t)
}

fn draw_rope_png(rt: &mut Runtime<AppState>, name: &str) {
    rt.settle_all();
    rt.frame(0.0);
    let (w, h) = headless::DEFAULT_SIZE;
    let gpu = Gpu::headless();
    let format = OFFSCREEN_FORMAT.remove_srgb_suffix();
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(true);
    let target = Offscreen::with_format(&gpu.device, w as u32, h as u32, format);
    renderer.prepare(&gpu.device, &gpu.queue, &mut rt.text.atlas, &rt.instances, (w as u32, h as u32));
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&mut encoder, &target.view, rt.clear_color());
    target.copy(&mut encoder);
    renderer.submit(&gpu.queue, encoder);
    let pixels = target.read(&gpu.device);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/headless")
        .join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_png(&path, w as u32, h as u32, &pixels).expect("write rope PNG");
}

fn rope_bench_variant(label: &str, text: String) -> RopeResult {
    let bytes = text.len();
    let lines = text.bytes().filter(|b| *b == b'\n').count() + 1;
    let single_line = lines == 1;

    let mut state = AppState::default();
    let id = state.active;
    let t = Instant::now();
    state.update_input(id, text);
    let load_ms = ms(t);

    let t = Instant::now();
    let mut rt = Runtime::with_text(
        state,
        ui::view,
        headless::DEFAULT_SIZE,
        Env::new(true, SCALE),
        headless::text_system(),
    );
    rt.set_srgb_blend(true);
    rt.frame(0.0);
    let first_frame_ms = ms(t);
    let t = Instant::now();
    rt.settle_all();
    rt.frame(0.0);
    let settle_ms = ms(t);

    rt.update(|_, tx| {
        tx.focus(format!("editor-{id}"), true);
    });
    rt.frame(0.0);
    assert!(
        rt.elements().iter().any(|e| e.key.as_deref() == Some(format!("editor-{id}").as_str()) && e.focused),
        "{label}: rope editor did not take focus"
    );

    let mut samples: Vec<f64> = Vec::new();
    let mut per_pos_median = Vec::new();
    let mut expected = bytes;
    for (pos, caret) in [("start", 0usize), ("middle", bytes / 2), ("end", bytes)] {
        rope_place_caret(&mut rt, id, caret);
        let mut pos_samples = Vec::new();
        for _ in 0..20 {
            let d = rope_key(&mut rt, Key::Text("x".into()), Mods::default());
            pos_samples.push(d);
            samples.push(d);
            assert!(d < SAMPLE_WATCHDOG_MS, "{label}: keystroke watchdog at {pos}");
        }
        expected += pos_samples.len();
        assert_eq!(rope_buffer_len(&rt), expected, "{label}: typed chars lost at {pos}");
        per_pos_median.push((pos, median(&mut pos_samples)));
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let key_median_ms = percentile(&samples, 0.5);
    let key_p95_ms = percentile(&samples, 0.95);
    let key_max_ms = samples.last().copied().unwrap_or(f64::NAN);

    // Evidence PNG: mid-file caret with a word selection (100 MB row), or
    // editing at column 3,000,000 (single-line row).
    if single_line {
        rope_place_caret(&mut rt, id, 3_000_000);
        for _ in 0..3 {
            rope_key(&mut rt, Key::Text("x".into()), Mods::default());
        }
        expected += 3;
        draw_rope_png(&mut rt, "rope-single-line-col-3m.png");
    } else {
        let mid = rope_buffer_len(&rt) / 2;
        rope_place_caret(&mut rt, id, mid);
        rt.update(|s, _| {
            let tab = s.tab(id).expect("tab");
            editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.select_word(mid));
        });
        rt.frame(0.0);
        draw_rope_png(&mut rt, "rope-100mb-middle-selection.png");
        rt.update(|s, _| {
            let tab = s.tab(id).expect("tab");
            editor_state::with(id, &tab.input, tab.input_revision, |ed| ed.anchor = None);
        });
        rt.frame(0.0);
    }

    // Caret move + instant scroll to the middle.
    let mid = rope_buffer_len(&rt) / 2;
    let t = Instant::now();
    rope_place_caret(&mut rt, id, mid);
    let caret_scroll_ms = ms(t);

    // Paste 1 KB, then 10 MB, then undo the large paste.
    let paste_1kb_ms = rope_paste(&mut rt, "x".repeat(1024));
    expected += 1024;
    assert_eq!(rope_buffer_len(&rt), expected, "{label}: 1 KB paste lost");
    let payload = "x".repeat(10 * MIB);
    let paste_10mb_ms = rope_paste(&mut rt, payload);
    expected += 10 * MIB;
    assert_eq!(rope_buffer_len(&rt), expected, "{label}: 10 MB paste lost");
    let undo_10mb_ms = rope_key(
        &mut rt,
        Key::Command('z'),
        Mods { cmd: true, ..Mods::default() },
    );
    expected -= 10 * MIB;
    assert_eq!(rope_buffer_len(&rt), expected, "{label}: large-paste undo wrong");

    // Select-all + delete.
    rope_key(&mut rt, Key::Command('a'), Mods { cmd: true, ..Mods::default() });
    let select_all_delete_ms = rope_key(&mut rt, Key::Backspace, Mods::default());
    assert_eq!(rope_buffer_len(&rt), 0, "{label}: select-all+delete wrong");

    let vmhwm_mb = proc_status_mb("VmHWM:");
    let vmrss_mb = proc_status_mb("VmRSS:");
    drop(rt);

    RopeResult {
        label: label.to_string(),
        bytes,
        lines,
        load_ms,
        first_frame_ms,
        settle_ms,
        key_median_ms,
        key_p95_ms,
        key_max_ms,
        per_pos_median,
        caret_scroll_ms,
        paste_1kb_ms,
        paste_10mb_ms,
        undo_10mb_ms,
        select_all_delete_ms,
        vmhwm_mb,
        vmrss_mb,
    }
}

#[test]
#[ignore]
fn rope_editor_bench() {
    let _guard = serial();
    // Error-marker evidence through the real app view.
    {
        let input = "{\n  \"a\": 1,\n  \"b\": oops,\n  \"c\": 3\n}";
        let mut state = AppState::default();
        let id = state.active;
        state.update_input(id, input.into());
        let revision = state.tab(id).unwrap().input_revision;
        let error = json_viewer::json_ops::validate(input).expect_err("invalid fixture");
        state.update(json_viewer::state::Msg::ValidationDone {
            tab: id,
            input_revision: revision,
            result: Err(error),
        });
        let mut rt = Runtime::with_text(
            state,
            ui::view,
            headless::DEFAULT_SIZE,
            Env::new(true, SCALE),
            headless::text_system(),
        );
        rt.set_srgb_blend(true);
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);
        draw_rope_png(&mut rt, "rope-error-marker.png");
    }

    let mut results = Vec::new();
    for mb in [1.0, 10.0, 100.0] {
        let target = (mb * MIB as f64) as usize;
        results.push(rope_bench_variant(&format!("pretty {mb}"), pretty_fixture(target)));
    }
    results.push(rope_bench_variant("single-line 6.9", minified_fixture(6_900_000)));

    println!("\n## Rope editor (rp-editor E5) — release, scale 1, 1280x800, real ui::view\n");
    println!(
        "Keystroke = `Runtime::key` + frame (dispatch) + `runner::poll` (apply) + frame (render), \
         20 chars per position. Targets: p95 < {KEY_BUDGET_MS} ms, first frame < {FIRST_FRAME_BUDGET_MS} ms. \
         VmHWM is process-wide/cumulative.\n"
    );
    println!("| Variant | MiB | Lines | Load ms | First frame ms | Settle ms | Key median ms | Key p95 ms | Key max ms | Caret+scroll ms | Paste 1KB ms | Paste 10MB ms | Undo 10MB ms | SelAll+Del ms | VmHWM MiB | VmRSS MiB | Pass |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |");
    for r in &results {
        println!(
            "| {} | {:.2} | {} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} |",
            r.label,
            r.bytes as f64 / MIB as f64,
            r.lines,
            r.load_ms,
            r.first_frame_ms,
            r.settle_ms,
            r.key_median_ms,
            r.key_p95_ms,
            r.key_max_ms,
            r.caret_scroll_ms,
            r.paste_1kb_ms,
            r.paste_10mb_ms,
            r.undo_10mb_ms,
            r.select_all_delete_ms,
            r.vmhwm_mb.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
            r.vmrss_mb.map(|v| format!("{v:.0}")).unwrap_or_else(|| "n/a".into()),
            if r.passes() { "YES" } else { "NO" },
        );
    }
    println!("\n### Per-position keystroke medians (ms)\n");
    println!("| Variant | Start | Middle | End |");
    println!("| --- | ---: | ---: | ---: |");
    for r in &results {
        let get = |pos: &str| {
            r.per_pos_median
                .iter()
                .find(|(p, _)| *p == pos)
                .map(|(_, v)| format!("{v:.2}"))
                .unwrap_or_else(|| "-".into())
        };
        println!("| {} | {} | {} | {} |", r.label, get("start"), get("middle"), get("end"));
    }

    for r in &results {
        assert!(
            r.passes(),
            "{}: p95 {:.2} ms (budget {KEY_BUDGET_MS}), first frame {:.1} ms (budget {FIRST_FRAME_BUDGET_MS})",
            r.label, r.key_p95_ms, r.first_frame_ms
        );
    }
}
