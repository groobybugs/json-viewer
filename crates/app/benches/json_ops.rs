//! Timing harness for `json_ops` on large documents.
//!
//! ```sh
//! cargo bench -p json-viewer --bench json_ops
//! ```
//!
//! The same table is printed by the ignored library test:
//! `cargo test --release -p json-viewer json_ops -- --ignored --nocapture`.

use std::time::Instant;

use json_viewer::json_ops::{self, JsonAction};

/// Builds a valid top-level array of records close to `target_bytes`.
fn make_fixture(target_bytes: usize) -> String {
    const RECORD: &str = concat!(
        r#"{"id":12345,"name":"widget-äöü-日本","price":19.99,"#,
        r#""tags":["a","b",null,true],"nested":{"x":1e5,"y":[1,2,3],"ok":false},"#,
        r#""text":"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor."}"#
    );
    let mut out = String::with_capacity(target_bytes + RECORD.len() + 2);
    out.push('[');
    while out.len() + RECORD.len() + 2 <= target_bytes {
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(RECORD);
    }
    out.push(']');
    out
}

fn measure(label: &str, mb: f64, mut run: impl FnMut() -> bool) -> f64 {
    let started = Instant::now();
    let ok = run();
    let seconds = started.elapsed().as_secs_f64();
    assert!(ok, "{label} produced an unexpected result");
    println!(
        "{label:<22}{mb:>10.1}{:>12.1}{:>12.1}",
        seconds * 1000.0,
        mb / seconds
    );
    seconds
}

fn main() {
    println!("{:<22}{:>10}{:>12}{:>12}", "case", "input MB", "ms", "MB/s");
    for target_mb in [10usize, 50, 100] {
        let fixture = make_fixture(target_mb << 20);
        let mb = fixture.len() as f64 / (1024.0 * 1024.0);
        println!("--- fixture {mb:.1} MiB ({target_mb} MB target)");

        let mut format_len = 0usize;
        measure("format", mb, || {
            let out = json_ops::process(&fixture, JsonAction::Format).unwrap();
            format_len = out.len();
            format_len > fixture.len()
        });

        let mut minified = String::new();
        measure("minify", mb, || {
            minified = json_ops::process(&fixture, JsonAction::Minify).unwrap();
            minified == fixture
        });

        let invalid = &fixture[..fixture.len() - 1];
        measure("invalid-at-end", mb, || {
            let err = json_ops::process(invalid, JsonAction::Minify).unwrap_err();
            err.line == 1 && err.column > 0
        });

        println!("format output: {format_len} bytes");
    }
}
