//! End-to-end release performance harness for 100 MB JSON workloads.
//!
//! Run with:
//! `cargo test --release -p json-viewer --test perf -- --ignored --nocapture`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use json_viewer::json_ops::{self, JsonAction};
use json_viewer::search::{self, SearchOptions};
use json_viewer::state::{AppState, Msg, TabView};
use json_viewer::{headless, runner};

fn target_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn ensure_100mb_fixture() -> PathBuf {
    let path = target_dir().join("fixtures").join("fixture-100mb.json");
    if !path.exists() {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["run", "--release", "-p", "json-viewer", "--bin", "gen_fixtures"])
            .status()
            .expect("invoke gen_fixtures");
        assert!(status.success(), "gen_fixtures failed");
    }
    path
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[test]
#[ignore]
fn perf_100mb_end_to_end() {
    let fixture_path = ensure_100mb_fixture();
    let bytes = std::fs::metadata(&fixture_path)
        .expect("stat fixture-100mb.json")
        .len();
    let mb = bytes as f64 / (1024.0 * 1024.0);

    // 1. Load 100 MB from disk into AppState
    let t0 = Instant::now();
    let input = std::fs::read_to_string(&fixture_path).expect("read fixture-100mb.json");
    let mut state = AppState::default();
    state.update(Msg::FileLoaded {
        path: fixture_path.clone(),
        content: input.clone(),
    });
    let load_dur = t0.elapsed();

    // 2. Format 100 MB
    let t0 = Instant::now();
    let formatted = json_ops::process(&input, JsonAction::Format).expect("format 100 MB");
    let format_dur = t0.elapsed();

    // 3. Minify 100 MB
    let t0 = Instant::now();
    let minified = json_ops::process(&input, JsonAction::Minify).expect("minify 100 MB");
    let minify_dur = t0.elapsed();

    // 4. Search 100 MB
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let hits = search::search(&input, "target_needle", SearchOptions::default(), &cancel);
    let search_dur = t0.elapsed();
    assert!(!hits.is_empty(), "expected hits for target_needle");

    // 5. Scroll output frame time (60 frames with wheel input on 100 MB output)
    let id = state.active;
    state.apply_json_result(id, Ok(formatted));
    state.update(Msg::SwitchView(TabView::Output));

    let mut rt = headless::runtime(state, headless::DEFAULT_SIZE, 1.0);
    let mut frame_ms = Vec::with_capacity(60);
    for _ in 0..60 {
        let f_start = Instant::now();
        rt.wheel_xy(640.0, 400.0, 0.0, -120.0);
        rt.update_if(|s, tx| runner::poll(s, 0.0, tx));
        rt.frame(1.0 / 60.0);
        frame_ms.push(f_start.elapsed().as_secs_f64() * 1000.0);
    }
    frame_ms.sort_by(|a, b| a.total_cmp(b));
    let avg_frame = frame_ms.iter().sum::<f64>() / frame_ms.len() as f64;
    let p95_frame = percentile(&frame_ms, 0.95);
    let max_frame = *frame_ms.last().unwrap_or(&0.0);

    println!("\n### Rust + Rune 100 MB Performance ({mb:.2} MiB, minified {:.2} MiB)\n", minified.len() as f64 / (1024.0 * 1024.0));
    println!("| Operation | Duration / Frame Time | Notes |");
    println!("| --- | ---: | --- |");
    println!("| Load 100 MB | {:.2} ms | `fs::read_to_string` + `Msg::FileLoaded` |", load_dur.as_secs_f64() * 1000.0);
    println!("| Format 100 MB | {:.2} ms | `json_ops::process(..., Format)` |", format_dur.as_secs_f64() * 1000.0);
    println!("| Minify 100 MB | {:.2} ms | `json_ops::process(..., Minify)` |", minify_dur.as_secs_f64() * 1000.0);
    println!("| Search 100 MB | {:.2} ms | `search::search` ({} hits, capped at 1000) |", search_dur.as_secs_f64() * 1000.0, hits.len());
    println!(
        "| Scroll Output (60 frames) | avg {avg_frame:.2} ms / p95 {p95_frame:.2} ms / max {max_frame:.2} ms | `wheel_xy` + `frame(1/60)` |"
    );
    let _: Duration = load_dur;
}
