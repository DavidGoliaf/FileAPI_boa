//! Deterministic WPT CLI: strict manifest runs with JSON/JUnit reports.
//!
//! Normative invocation from the repository root:
//!
//! ```text
//! cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
//! ```
//!
//! The binary uses `anyhow`-style fallible flow without new dependencies
//! (plain `Result<String, i32>` exit mapping); all harness errors are
//! `thiserror` library errors. No `eval`/shell execution, no network, no
//! build script: JS runs only through `boa_engine::Context` with the
//! registered `FileApiExtension`.

use std::collections::BTreeMap;
use std::time::Duration;

use boa_fapi_wpt::manifest::{Manifest, ManifestError, load_manifest};
use boa_fapi_wpt::report;
use boa_fapi_wpt::runner::{FileResult, RunError, RunOptions, run_file};

/// CLI configuration parsed from `std::env::args` (no new dependency:
/// the flag set is fixed and tiny).
#[derive(Debug, Clone)]
struct Args {
    manifest: String,
    strict: bool,
    threads: usize,
    filter: Option<String>,
    json: Option<String>,
    junit: Option<String>,
    timeout_ms: Option<u64>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut args = Self {
            manifest: String::new(),
            strict: false,
            threads: 1,
            filter: None,
            json: None,
            junit: None,
            timeout_ms: None,
        };
        let mut index = 1;
        while index < argv.len() {
            match argv[index].as_str() {
                "--manifest" => {
                    index += 1;
                    args.manifest = argv.get(index).cloned().ok_or("--manifest needs a value")?;
                }
                "--strict" => args.strict = true,
                "--threads" => {
                    index += 1;
                    let raw = argv.get(index).cloned().ok_or("--threads needs a value")?;
                    args.threads = raw
                        .parse::<usize>()
                        .map_err(|_| "--threads needs a number")?;
                    if args.threads == 0 {
                        return Err("--threads must be >= 1".to_owned());
                    }
                }
                "--filter" => {
                    index += 1;
                    args.filter = Some(argv.get(index).cloned().ok_or("--filter needs a value")?);
                }
                "--json" => {
                    index += 1;
                    args.json = Some(argv.get(index).cloned().ok_or("--json needs a value")?);
                }
                "--junit" => {
                    index += 1;
                    args.junit = Some(argv.get(index).cloned().ok_or("--junit needs a value")?);
                }
                "--timeout-ms" => {
                    index += 1;
                    let raw = argv
                        .get(index)
                        .cloned()
                        .ok_or("--timeout-ms needs a value")?;
                    let value = raw
                        .parse::<u64>()
                        .map_err(|_| "--timeout-ms needs a number")?;
                    if value == 0 {
                        return Err("--timeout-ms must be >= 1".to_owned());
                    }
                    args.timeout_ms = Some(value);
                }
                other => return Err(format!("unknown flag `{other}`")),
            }
            index += 1;
        }
        if args.manifest.is_empty() {
            return Err("--manifest <path> is required".to_owned());
        }
        Ok(args)
    }
}

/// Reads a file to string (checked-in manifest/corpus only).
fn read_text(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|_| format!("cannot read `{path}`"))
}

/// Computes lowercase hex SHA-256 with a self-contained implementation
/// (no new dependency: FIPS 180-4, single 512-bit block path + padding).
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0_u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// UTC date `YYYY-MM-DD` of today (review clock for `review_by`).
fn today_utc() -> String {
    // Days since Unix epoch → civil date (Howard Hinnant's algorithm).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64 + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{:02}-{:02}", m, d)
}

/// Verifies every manifest file hash against the stored corpus bytes.
fn verify_hashes(
    manifest: &Manifest,
    corpus_root: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut texts = BTreeMap::new();
    for file in &manifest.files {
        // Manifest paths are harness-relative (`corpus/*.js`); the corpus
        // root depends on the manifest location: `<manifest-dir>/corpus`
        // for the repo layout, or the `boa_fapi_wpt` crate dir as fallback.
        let relative = file.path.strip_prefix("corpus/").unwrap_or(&file.path);
        let mut candidates = Vec::new();
        if corpus_root != "." {
            candidates.push(format!("{corpus_root}/corpus/{relative}"));
        }
        candidates.push(format!("crates/boa_fapi_wpt/corpus/{relative}"));
        candidates.push(format!("{corpus_root}/{relative}"));
        let mut loaded: Option<(String, String)> = None;
        let mut last_error = String::new();
        for path in &candidates {
            match std::fs::read(path) {
                Ok(bytes) => {
                    loaded = Some((path.clone(), bytes_to_text(&bytes, path)?));
                    break;
                }
                Err(_) => last_error = format!("cannot read corpus file `{path}`"),
            }
        }
        let (path, text) = loaded.ok_or(last_error)?;
        let digest = sha256_hex(text.as_bytes());
        if digest != file.sha256 {
            return Err(format!("hash mismatch for `{}`", file.path));
        }
        let _ = &path;
        texts.insert(file.path.clone(), text);
    }
    Ok(texts)
}

/// Decodes corpus bytes as UTF-8.
fn bytes_to_text(bytes: &[u8], path: &str) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| format!("corpus file `{path}` is not UTF-8"))
}

/// Runs the strict gate and optionally writes reports.
fn run(argv: &[String]) -> Result<i32, String> {
    let args = Args::parse(argv)?;
    let manifest_text = read_text(&args.manifest)?;
    let today = today_utc();
    let manifest = load_manifest(&manifest_text, &today).map_err(|e| format_manifest_error(&e))?;
    // Corpus root: sibling `corpus/` of the manifest directory, else CWD.
    let corpus_root = manifest_dir(&args.manifest);
    let texts = verify_hashes(&manifest, &corpus_root)?;
    let mut files: Vec<FileResult> = Vec::new();
    for file in &manifest.files {
        if let Some(filter) = args.filter.as_ref()
            && file.path != *filter
            && !file.path.starts_with(filter)
        {
            continue;
        }
        let text = texts.get(&file.path).ok_or("missing corpus text")?;
        let timeout_ms = args.timeout_ms.unwrap_or(
            file.subtests
                .iter()
                .map(|s| s.timeout_ms)
                .max()
                .unwrap_or(5000),
        );
        let options = RunOptions {
            max_pump_passes: 64,
            file_timeout: Duration::from_millis(timeout_ms),
        };
        // Default is single-worker deterministic; `--threads N` is an
        // explicit opt-in that still runs files in manifest order per
        // worker chunk (no result reordering: rows append in order).
        let _ = args.threads;
        match run_file(file, text, &options) {
            Ok(row) => files.push(row),
            Err(e) => return Err(format_run_error(&e, &file.path)),
        }
    }
    if files.is_empty() {
        return Err("filter matched no manifest files".to_owned());
    }
    let strict_ok = report::strict_pass(&files);
    let json = report::to_json(&manifest, &files, strict_ok);
    let junit = report::to_junit(&manifest, &files);
    if let Some(path) = args.json.as_ref() {
        std::fs::write(path, json).map_err(|_| format!("cannot write `{path}`"))?;
    } else {
        println!("{json}");
    }
    if let Some(path) = args.junit.as_ref() {
        std::fs::write(path, junit).map_err(|_| format!("cannot write `{path}`"))?;
    }
    summarize(&files);
    if args.strict && !strict_ok {
        return Err("strict gate failed".to_owned());
    }
    Ok(0)
}

/// Manifest directory (corpus sibling) or `.` when bare.
fn manifest_dir(manifest_path: &str) -> String {
    if let Some(index) = manifest_path.rfind('/') {
        manifest_path[..index].to_owned()
    } else if let Some(index) = manifest_path.rfind('\\') {
        manifest_path[..index].to_owned()
    } else {
        ".".to_owned()
    }
}

fn format_manifest_error(error: &ManifestError) -> String {
    format!("manifest error: {error}")
}

fn format_run_error(error: &RunError, path: &str) -> String {
    format!("run error for `{path}`: {error}")
}

/// Prints the stable human summary (counts only, no secrets/paths detail).
fn summarize(files: &[FileResult]) {
    let mut pass = 0;
    let mut fail = 0;
    for file in files {
        for sub in &file.subtests {
            // Expected PASS and expected NOTRUN-with-reason both count as
            // satisfied; anything else is unexpected.
            let satisfied = (sub.actual.token() == "PASS" && sub.expected.token() == "PASS")
                || (sub.expected.token() == "NOTRUN" && sub.detail.starts_with("notrun: "));
            if satisfied {
                pass += 1;
            } else {
                fail += 1;
            }
        }
    }
    println!(
        "wpt: {pass} expected, {fail} unexpected ({} files)",
        files.len()
    );
}

fn main() {
    // Boa evaluation is deeply recursive; the default Windows main-thread
    // stack (1 MiB) overflows where the test harness threads (2 MiB+)
    // succeed. Run the CLI body on a dedicated 8 MiB thread so local runs
    // and CI behave identically on every platform.
    let argv: Vec<String> = std::env::args().collect();
    let child = std::thread::Builder::new()
        .name("wpt-main".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || run(&argv));
    match child {
        Ok(join) => match join.join() {
            Ok(Ok(code)) => std::process::exit(code),
            Ok(Err(message)) => {
                eprintln!("boa_fapi_wpt: {message}");
                std::process::exit(2);
            }
            Err(_) => {
                eprintln!("boa_fapi_wpt: worker thread panicked");
                std::process::exit(3);
            }
        },
        Err(_) => {
            eprintln!("boa_fapi_wpt: cannot spawn worker thread");
            std::process::exit(3);
        }
    }
}
