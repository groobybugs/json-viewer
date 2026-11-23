//! Deterministic JSON fixture generator for QA and performance benchmarks.
//!
//! Writes 1/10/50/100 MB fixtures plus a minified single-line variant and an
//! invalid-at-end variant into `target/fixtures/` (never committed).

use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn default_out_dir() -> PathBuf {
    let target = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("fixtures")
}

fn long_string(rng: &mut Rng, len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_- ";
    let mut out = String::with_capacity(len + 16);
    for _ in 0..len {
        let idx = (rng.next_u64() as usize) % ALPHABET.len();
        out.push(ALPHABET[idx] as char);
    }
    out.push_str(" café αβγ");
    out
}

fn write_record(
    w: &mut BufWriter<File>,
    rng: &mut Rng,
    index: usize,
    pretty: bool,
    comma_before: bool,
) -> std::io::Result<usize> {
    let tag = match index % 4 {
        0 => "alpha",
        1 => "target_needle",
        2 => "gamma",
        _ => "delta",
    };
    let payload = long_string(rng, 480);
    let score = (rng.next_u64() % 10_000) as f64 / 100.0;
    let prefix = if comma_before { "," } else { "" };

    let chunk = if pretty {
        format!(
            "{prefix}\n    {{\n      \"id\": {index},\n      \"unicode\": \"日本語テスト 🚀 café naïve — αβγ #{index}\",\n      \"numbers\": {{\n        \"float_one\": 1.0,\n        \"sci\": 1e5,\n        \"huge_u64\": 18446744073709551615,\n        \"huge_i64\": 9223372036854775807,\n        \"js_unsafe\": 9007199254740993,\n        \"neg\": -42.5,\n        \"score\": {score:.2}\n      }},\n      \"flags\": [true, false, null],\n      \"nested\": {{\n        \"level1\": {{\n          \"level2\": {{\n            \"tags\": [\"{tag}\", \"beta\", \"omega\"]\n          }}\n        }}\n      }},\n      \"payload\": \"{payload}\"\n    }}"
        )
    } else {
        format!(
            "{prefix}{{\"id\":{index},\"unicode\":\"日本語テスト 🚀 café naïve — αβγ #{index}\",\"numbers\":{{\"float_one\":1.0,\"sci\":1e5,\"huge_u64\":18446744073709551615,\"huge_i64\":9223372036854775807,\"js_unsafe\":9007199254740993,\"neg\":-42.5,\"score\":{score:.2}}},\"flags\":[true,false,null],\"nested\":{{\"level1\":{{\"level2\":{{\"tags\":[\"{tag}\",\"beta\",\"omega\"]}}}}}},\"payload\":\"{payload}\"}}"
        )
    };
    w.write_all(chunk.as_bytes())?;
    Ok(chunk.len())
}

fn generate_fixture(
    path: &Path,
    target_bytes: usize,
    pretty: bool,
    invalid_at_end: bool,
) -> std::io::Result<u64> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let mut rng = Rng::new(0x517c_c1b7_2722_0a95 ^ (target_bytes as u64));

    let header = if pretty {
        "{\n  \"meta\": {\n    \"generator\": \"gen_fixtures\",\n    \"version\": 1,\n    \"target_bytes\": "
    } else {
        "{\"meta\":{\"generator\":\"gen_fixtures\",\"version\":1,\"target_bytes\":"
    };
    w.write_all(header.as_bytes())?;
    let meta_tail = if pretty {
        format!("{target_bytes}\n  }},\n  \"records\": [")
    } else {
        format!("{target_bytes}}},\"records\":[")
    };
    w.write_all(meta_tail.as_bytes())?;

    let mut written = header.len() + meta_tail.len();
    let mut index = 0usize;
    while written < target_bytes {
        written += write_record(&mut w, &mut rng, index, pretty, index > 0)?;
        index += 1;
    }

    let footer = match (pretty, invalid_at_end) {
        (true, false) => "\n  ]\n}\n",
        (false, false) => "]}",
        (true, true) => "\n  ],\n  \"broken\": }\n",
        (false, true) => "],\"broken\":}",
    };
    w.write_all(footer.as_bytes())?;
    w.flush()?;
    Ok(fs::metadata(path)?.len())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let out_dir = if let Some(pos) = args.iter().position(|a| a == "--out") {
        args.get(pos + 1)
            .map(PathBuf::from)
            .expect("--out requires a directory path")
    } else {
        default_out_dir()
    };

    fs::create_dir_all(&out_dir)?;

    let specs = [
        ("fixture-1mb.json", 1_000_000, true, false),
        ("fixture-10mb.json", 10_000_000, true, false),
        ("fixture-50mb.json", 50_000_000, true, false),
        ("fixture-100mb.json", 100_000_000, true, false),
        ("fixture-10mb-minified.json", 10_000_000, false, false),
        ("fixture-10mb-invalid-end.json", 10_000_000, true, true),
    ];

    for (name, bytes, pretty, invalid_end) in specs {
        let path = out_dir.join(name);
        let actual = generate_fixture(&path, bytes, pretty, invalid_end)?;
        println!(
            "wrote {} ({:.2} MiB)",
            path.display(),
            actual as f64 / (1024.0 * 1024.0)
        );
    }

    Ok(())
}
