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
use boa_fapi_wpt::runner::{
    ActualStatus, FileResult, RunError, RunOptions, SubtestResult, run_file, scrub_detail,
};

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
///
/// Resolution uses only the manifest `corpus_root` (relative to the
/// manifest directory): the logical `file.path` is validated, joined via
/// [`std::path::Path`], canonicalized, and required to stay strictly
/// inside the canonical root; symlinks anywhere in the root or candidate
/// are launch errors. Hashes run over the raw bytes after UTF-8
/// validation (hash-then-decode would hash different bytes than executed
/// on non-UTF8 input). Error details carry only the manifest-relative
/// logical path, never an absolute path.
fn verify_hashes(
    manifest_path: &str,
    manifest: &Manifest,
) -> Result<BTreeMap<String, String>, String> {
    use boa_fapi_wpt::manifest::{MAX_CORPUS_BYTES, resolve_corpus_path};
    let mut texts = BTreeMap::new();
    for file in &manifest.files {
        let candidate = resolve_corpus_path(manifest_path, &manifest.corpus_root, &file.path)
            .map_err(|_| format!("invalid corpus path `{}`", file.path))?;
        let bytes = std::fs::read(&candidate)
            .map_err(|_| format!("cannot read corpus file `{}`", file.path))?;
        if bytes.len() > MAX_CORPUS_BYTES {
            return Err(format!("corpus file `{}` too large", file.path));
        }
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| format!("corpus file `{}` is not UTF-8", file.path))?;
        let digest = sha256_hex(&bytes);
        if digest != file.sha256 {
            return Err(format!("hash mismatch for `{}`", file.path));
        }
        texts.insert(file.path.clone(), text);
    }
    Ok(texts)
}

/// Internal worker mode: runs exactly one validated manifest file and
/// prints a single-line worker report to stdout.
///
/// Invocation (parent only, never user-facing): the same executable with
/// `--worker-file <index> --manifest <path> [--timeout-ms N]`. The worker
/// re-loads and re-validates the manifest (schema, hashes, paths),
/// resolves the file by manifest index (never by arbitrary path), runs it
/// in-process with the file timeout as wall guard, and prints exactly one
/// protocol line:
///
/// - `WORKER-OK <file-json>` — encoded [`FileResult`] as compact JSON;
/// - `WORKER-TIMEOUT <stable-detail>` — reserved for future kill paths
///   (currently the parent synthesizes timeout rows; the worker never
///   prints this itself);
/// - `WORKER-ERROR <error-code>` — typed runner failure. Allowed codes:
///   `register`, `prelude`, `readback`, `file-eval`, `protocol`.
///
/// Stdout carries exactly one protocol line (bounded, corpus-capped);
/// anything else on stdout is a protocol corruption. Stderr is inherited
/// for launch errors only (exit 2, not part of the protocol).
fn worker_main(argv: &[String]) -> i32 {
    let mut manifest_path: Option<String> = None;
    let mut index: Option<usize> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut cursor = 1;
    while cursor < argv.len() {
        match argv[cursor].as_str() {
            "--manifest" => {
                cursor += 1;
                manifest_path = argv.get(cursor).cloned();
            }
            "--worker-file" => {
                cursor += 1;
                index = argv.get(cursor).and_then(|raw| raw.parse::<usize>().ok());
            }
            "--timeout-ms" => {
                cursor += 1;
                timeout_ms = argv.get(cursor).and_then(|raw| raw.parse::<u64>().ok());
            }
            _ => {}
        }
        cursor += 1;
    }
    let (Some(manifest_path), Some(index)) = (manifest_path, index) else {
        eprintln!("boa_fapi_wpt: worker needs --manifest and --worker-file <index>");
        return 2;
    };
    let manifest_text = match read_text(&manifest_path) {
        Ok(text) => text,
        Err(message) => {
            eprintln!("boa_fapi_wpt: {message}");
            return 2;
        }
    };
    let today = today_utc();
    let manifest = match load_manifest(&manifest_text, &today) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("boa_fapi_wpt: {}", format_manifest_error(&error));
            return 2;
        }
    };
    let Some(file) = manifest.files.get(index) else {
        eprintln!("boa_fapi_wpt: worker file index out of range");
        return 2;
    };
    let texts = match verify_hashes(&manifest_path, &manifest) {
        Ok(texts) => texts,
        Err(message) => {
            eprintln!("boa_fapi_wpt: {message}");
            return 2;
        }
    };
    let Some(text) = texts.get(&file.path) else {
        eprintln!("boa_fapi_wpt: missing corpus text");
        return 2;
    };
    if timeout_ms.is_some_and(|t| t == 0 || t > boa_fapi_wpt::manifest::MAX_TIMEOUT_MS) {
        eprintln!("boa_fapi_wpt: --timeout-ms out of range 1..=300000");
        return 2;
    }
    let timeout = timeout_ms.unwrap_or(
        file.subtests
            .iter()
            .map(|s| s.timeout_ms)
            .max()
            .unwrap_or(5000),
    );
    let options = RunOptions {
        max_pump_passes: 64,
        file_timeout: Duration::from_millis(timeout),
    };
    match run_file(file, text, &options) {
        Ok(row) => {
            // Single-line worker report: compact JSON of the one FileResult.
            let json = report::to_json(&manifest, std::slice::from_ref(&row), true);
            // Bound stdout: one line, corpus-capped length.
            let mut line = json;
            line.retain(|c| c != '\n' && c != '\r');
            if line.len() > 2 * 1024 * 1024 {
                eprintln!("boa_fapi_wpt: worker output too large");
                return 2;
            }
            println!("WORKER-OK {line}");
            0
        }
        Err(error) => {
            // Typed worker error (F13): exit code alone never types the
            // failure — the protocol line does.
            let code = match error {
                RunError::Register => "register",
                RunError::Prelude => "prelude",
                RunError::Readback => "readback",
                RunError::FileEval => "file-eval",
            };
            println!("WORKER-ERROR {code}");
            1
        }
    }
}

/// Bounded worker output cap (2 MiB): overflow is a launch failure.
const MAX_WORKER_OUTPUT: u64 = 2 * 1024 * 1024;
const MAX_WORKER_STDERR: u64 = 64 * 1024;

/// Bounded capture collected concurrently with the child process.
struct PipeCapture {
    bytes: Vec<u8>,
    overflow: bool,
    read_failed: bool,
}

impl PipeCapture {
    fn failed() -> Self {
        Self {
            bytes: Vec::new(),
            overflow: false,
            read_failed: true,
        }
    }
}

/// Drains a worker pipe until EOF while retaining only the bounded prefix.
/// Continuing to drain after overflow prevents the child from blocking on a
/// full OS pipe before it can exit.
fn read_pipe<R: std::io::Read>(mut pipe: R, limit: u64) -> PipeCapture {
    let cap = limit as usize;
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut read_failed = false;
    let mut buffer = [0_u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let retained_limit = cap.saturating_add(1);
                if bytes.len() < retained_limit {
                    let keep = count.min(retained_limit - bytes.len());
                    bytes.extend_from_slice(&buffer[..keep]);
                }
                if bytes.len() > cap {
                    overflow = true;
                    bytes.truncate(retained_limit);
                }
            }
            Err(_) => {
                read_failed = true;
                break;
            }
        }
    }
    PipeCapture {
        bytes,
        overflow,
        read_failed,
    }
}

fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: R,
    limit: u64,
) -> std::thread::JoinHandle<PipeCapture> {
    std::thread::spawn(move || read_pipe(pipe, limit))
}

fn join_pipe_reader(reader: std::thread::JoinHandle<PipeCapture>) -> PipeCapture {
    match reader.join() {
        Ok(capture) => capture,
        Err(_) => PipeCapture::failed(),
    }
}

/// Runs one manifest file in an isolated child process of the same
/// executable, killing it at the wall deadline.
///
/// Returns the parsed [`FileResult`] on `WORKER-OK`, a synthetic `TIMEOUT`
/// row on kill / crash / truncated or corrupt output, or a CLI launch error
/// for typed register/prelude/readback failures. Stderr is bounded and
/// scrubbed; only the manifest-relative file path appears in errors.
fn run_file_isolated(
    exe: &std::path::Path,
    manifest_path: &str,
    index: usize,
    file: &boa_fapi_wpt::manifest::ManifestFile,
    timeout_ms: u64,
) -> Result<FileResult, String> {
    use std::time::Instant;
    let deadline = Duration::from_millis(timeout_ms.max(1));
    let mut command = std::process::Command::new(exe);
    command
        .arg("--worker-file")
        .arg(index.to_string())
        .arg("--manifest")
        .arg(manifest_path)
        .arg("--timeout-ms")
        .arg(timeout_ms.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Ok(timeout_row(file, "worker spawn failed"));
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(timeout_row(file, "worker stdout pipe missing"));
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(timeout_row(file, "worker stderr pipe missing"));
    };
    let stdout_reader = spawn_pipe_reader(stdout, MAX_WORKER_OUTPUT);
    let stderr_reader = spawn_pipe_reader(stderr, MAX_WORKER_STDERR);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return match finish_worker(status, stdout_reader, stderr_reader, file) {
                    WorkerOutcome::Row(row) => Ok(row),
                    WorkerOutcome::Launch(launch) => Err(format!("worker {} error", launch.class)),
                };
            }
            Ok(None) => {
                if started.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = join_pipe_reader(stdout_reader);
                    let _ = join_pipe_reader(stderr_reader);
                    return Ok(timeout_row(file, "worker wall deadline exceeded"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe_reader(stdout_reader);
                let _ = join_pipe_reader(stderr_reader);
                return Ok(timeout_row(file, "worker wait failed"));
            }
        }
    }
}

/// Reads a finished worker's bounded output and maps it to a row.
///
/// Protocol (F13), checked in order:
///
/// 1. exactly one non-empty stdout line, valid UTF-8, within the output
///    cap; anything else (empty, multi-line content before/after the
///    protocol line, overflow) is `TIMEOUT` / protocol corruption;
/// 2. `WORKER-OK <json>` → fully verified [`FileResult`] (path, subtest
///    count, test/subtest IDs, expected statuses, known actual tokens —
///    see [`worker_json_to_row`]);
/// 3. `WORKER-TIMEOUT <detail>` → synthetic `TIMEOUT` rows (reserved;
///    currently the parent synthesizes timeouts itself);
/// 4. `WORKER-ERROR <code>` → `register`/`prelude`/`readback` become a
///    CLI launch error (exit 2, F7 mapping); `file-eval` becomes FAIL
///    rows for the file; `protocol`/unknown codes become `TIMEOUT`;
/// 5. killed worker (wall deadline) → `TIMEOUT` rows, parent continues;
/// 6. crash/non-zero exit without a known token → `TIMEOUT` with
///    `worker non-zero exit` (never PASS, never NOTRUN).
///
/// The protocol line types the failure, and its exit status must agree with
/// the token before the result is accepted.
fn finish_worker(
    status: std::process::ExitStatus,
    stdout_reader: std::thread::JoinHandle<PipeCapture>,
    stderr_reader: std::thread::JoinHandle<PipeCapture>,
    file: &boa_fapi_wpt::manifest::ManifestFile,
) -> WorkerOutcome {
    let stdout = join_pipe_reader(stdout_reader);
    let stderr = join_pipe_reader(stderr_reader);
    if stdout.overflow
        || stdout.read_failed
        || stderr.overflow
        || stderr.read_failed
        || stdout.bytes.len() as u64 > MAX_WORKER_OUTPUT
    {
        return WorkerOutcome::Row(timeout_row(file, "worker output overflow"));
    }
    // UTF-8 and size before parse; non-UTF8 stdout is corruption.
    let text = match String::from_utf8(stdout.bytes) {
        Ok(text) => text,
        Err(_) => return WorkerOutcome::Row(timeout_row(file, "worker protocol corruption")),
    };
    let mut lines = text.split('\n');
    let line = lines.next().unwrap_or("");
    // Exactly one protocol line: allow only the single newline emitted by
    // `println!`; an additional blank line is protocol corruption too.
    let extra_line = lines.next();
    if line.is_empty() || extra_line.is_some_and(|rest| !rest.is_empty() || lines.next().is_some())
    {
        return WorkerOutcome::Row(timeout_row(file, "worker protocol corruption"));
    }
    if let Some(json) = line.strip_prefix("WORKER-OK ") {
        if !status.success() {
            return WorkerOutcome::Row(timeout_row(file, "worker non-zero exit"));
        }
        return match worker_json_to_row(json, file) {
            Some(row) => WorkerOutcome::Row(row),
            None => WorkerOutcome::Row(timeout_row(file, "worker protocol corruption")),
        };
    }
    if let Some(detail) = line.strip_prefix("WORKER-TIMEOUT ") {
        let _ = detail;
        if status.success() {
            return WorkerOutcome::Row(timeout_row(file, "worker protocol corruption"));
        }
        return WorkerOutcome::Row(timeout_row(file, "worker timeout"));
    }
    if let Some(code) = line.strip_prefix("WORKER-ERROR ") {
        if status.success() {
            return WorkerOutcome::Row(timeout_row(file, "worker protocol corruption"));
        }
        return map_worker_error(code.trim(), file);
    }
    // No known token: killed/crashed worker → TIMEOUT (never PASS/NOTRUN).
    if !status.success() {
        let hint = String::from_utf8_lossy(&stderr.bytes);
        let hint = hint.lines().next().unwrap_or("").trim();
        if !hint.is_empty() {
            return WorkerOutcome::Row(timeout_row(
                file,
                &format!("worker exit: {}", truncate_hint(hint)),
            ));
        }
        return WorkerOutcome::Row(timeout_row(file, "worker non-zero exit"));
    }
    WorkerOutcome::Row(timeout_row(file, "worker protocol corruption"))
}

/// Parent-side outcome of one isolated file run.
enum WorkerOutcome {
    /// A verified row for the file.
    Row(FileResult),
    /// A CLI-wide launch error (exit 2): register/prelude/readback.
    /// The message is reported by the caller; the payload form keeps the
    /// mapping table explicit at the type level.
    Launch(WorkerLaunch),
}

/// Typed launch payload for register/prelude/readback failures.
#[derive(Debug, Clone)]
struct WorkerLaunch {
    /// Stable failure class: `register`, `prelude` or `readback`.
    class: &'static str,
}

/// Maps a typed `WORKER-ERROR` line to a row or a launch error (F13 table).
fn map_worker_error(code: &str, file: &boa_fapi_wpt::manifest::ManifestFile) -> WorkerOutcome {
    match code {
        "register" | "prelude" | "readback" => WorkerOutcome::Launch(WorkerLaunch {
            class: match code {
                "register" => "register",
                "prelude" => "prelude",
                _ => "readback",
            },
        }),
        "file-eval" => {
            // FAIL rows for every expected subtest (F7 mapping); when rows
            // cannot be built (never here — the manifest guarantees ≥1
            // subtest), the caller degrades to a launch error.
            WorkerOutcome::Row(fail_row(file, "adapted file evaluation failed"))
        }
        "protocol" => WorkerOutcome::Row(timeout_row(file, "worker protocol corruption")),
        _ => WorkerOutcome::Row(timeout_row(file, "worker protocol corruption")),
    }
}

/// Builds a synthetic `FAIL` row for every expected subtest.
fn fail_row(file: &boa_fapi_wpt::manifest::ManifestFile, detail: &str) -> FileResult {
    FileResult {
        path: file.path.clone(),
        upstream_path: file.upstream_path.clone(),
        group: file.group.clone(),
        subtests: file
            .subtests
            .iter()
            .map(|s| SubtestResult {
                test: s.test.clone(),
                subtest: s.subtest.clone(),
                actual: ActualStatus::Fail,
                expected: s.expected,
                detail: scrub_detail(detail),
                trace: s.trace.clone(),
                elapsed_ms: 0,
            })
            .collect(),
    }
}

/// Extracts the single [`FileResult`] from a `WORKER-OK` JSON line.
///
/// The worker JSON envelopes exactly one file (`files[0]`); the row is
/// accepted only when its path matches the dispatched manifest file —
/// anything else is protocol corruption.
fn worker_json_to_row(
    json: &str,
    file: &boa_fapi_wpt::manifest::ManifestFile,
) -> Option<FileResult> {
    use boa_fapi_wpt::manifest::{ExpectedStatus, parse_json};
    let root = parse_json(json).ok()?;
    let files = root.field("files")?.as_arr()?;
    if files.len() != 1 {
        return None;
    }
    let entry = &files[0];
    let path = entry.field("path")?.as_str()?;
    if path != file.path {
        return None;
    }
    let subtests = entry.field("subtests")?.as_arr()?;
    if subtests.len() != file.subtests.len() {
        return None;
    }
    let mut rows = Vec::new();
    for (sub_json, expected) in subtests.iter().zip(file.subtests.iter()) {
        let test = sub_json.field("test")?.as_str()?;
        let subtest = sub_json.field("subtest")?.as_str()?;
        let actual = sub_json.field("actual")?.as_str()?;
        let detail = sub_json.field("detail")?.as_str().unwrap_or("");
        if test != expected.test || subtest != expected.subtest {
            return None;
        }
        let actual = match actual {
            "PASS" => ActualStatus::Pass,
            "FAIL" => ActualStatus::Fail,
            "TIMEOUT" => ActualStatus::Timeout,
            "NOTRUN" => ActualStatus::NotRun,
            _ => return None,
        };
        rows.push(SubtestResult {
            test: test.to_owned(),
            subtest: subtest.to_owned(),
            actual,
            expected: if expected.expected.token() == "PASS" {
                ExpectedStatus::Pass
            } else {
                ExpectedStatus::NotRun
            },
            detail: scrub_detail(detail),
            trace: expected.trace.clone(),
            elapsed_ms: 0,
        });
    }
    Some(FileResult {
        path: file.path.clone(),
        upstream_path: file.upstream_path.clone(),
        group: file.group.clone(),
        subtests: rows,
    })
}

/// Builds a synthetic `TIMEOUT` row for every expected subtest.
fn timeout_row(file: &boa_fapi_wpt::manifest::ManifestFile, detail: &str) -> FileResult {
    FileResult {
        path: file.path.clone(),
        upstream_path: file.upstream_path.clone(),
        group: file.group.clone(),
        subtests: file
            .subtests
            .iter()
            .map(|s| SubtestResult {
                test: s.test.clone(),
                subtest: s.subtest.clone(),
                actual: ActualStatus::Timeout,
                expected: s.expected,
                detail: scrub_detail(detail),
                trace: s.trace.clone(),
                elapsed_ms: 0,
            })
            .collect(),
    }
}

/// Truncates a worker stderr hint to a stable short detail.
fn truncate_hint(hint: &str) -> String {
    scrub_detail(&hint.chars().take(160).collect::<String>())
}

/// Runs manifest files across `slots` isolated worker processes.
///
/// Each slot owns a disjoint manifest-index chunk; every file — including
/// the default single-slot run (F12) — executes in the isolated child
/// path with the wall deadline + kill semantics. Results are re-sorted by
/// manifest index before serialization, so `--threads N` output is
/// byte-identical to `--threads 1` for the same manifest. One file's
/// failure never drops another file's row (`TIMEOUT`/`FAIL` rows are
/// synthesized instead); `strict` still fails on any unexpected row.
/// `texts` is accepted for API symmetry with the verification step and
/// intentionally unused: workers re-load and re-verify the corpus
/// themselves (never trust parent-supplied bytes across the boundary).
fn run_files_parallel(
    args: &Args,
    manifest: &Manifest,
    slots: usize,
    texts: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<FileResult>, String> {
    use std::collections::BTreeMap;
    let _ = texts;
    let exe = std::env::current_exe().map_err(|_| "cannot locate harness executable".to_owned())?;
    // Manifest-indexed work list (filter applies before chunking, so the
    // diagnostic subset keeps its manifest order in both modes).
    let mut selected: Vec<usize> = Vec::new();
    for (index, file) in manifest.files.iter().enumerate() {
        if let Some(filter) = args.filter.as_ref()
            && file.path != *filter
            && !file.path.starts_with(filter)
        {
            continue;
        }
        selected.push(index);
    }
    if selected.is_empty() {
        return Err("filter matched no manifest files".to_owned());
    }
    // Chunk round-robin across slots for stable assignment.
    let mut chunks: Vec<Vec<usize>> = vec![Vec::new(); slots];
    for (position, index) in selected.into_iter().enumerate() {
        chunks[position % slots].push(index);
    }
    // One OS thread per slot; each thread runs its chunk sequentially
    // through isolated child processes. No Context/JsObject crosses
    // threads (only validated file records by index).
    let manifest_path = args.manifest.clone();
    let timeout_override = args.timeout_ms;
    let manifest_owned = Manifest {
        source: manifest.source.clone(),
        corpus_root: manifest.corpus_root.clone(),
        default_timeout_ms: manifest.default_timeout_ms,
        files: manifest.files.clone(),
    };
    let mut handles = Vec::new();
    for chunk in chunks {
        let manifest_path = manifest_path.clone();
        let manifest_owned = Manifest {
            source: manifest_owned.source.clone(),
            corpus_root: manifest_owned.corpus_root.clone(),
            default_timeout_ms: manifest_owned.default_timeout_ms,
            files: manifest_owned.files.clone(),
        };
        let exe = exe.clone();
        handles.push(std::thread::spawn(
            move || -> Result<Vec<(usize, FileResult)>, String> {
                let mut rows: Vec<(usize, FileResult)> = Vec::new();
                for index in chunk {
                    let Some(file) = manifest_owned.files.get(index) else {
                        continue;
                    };
                    let timeout_ms = timeout_override.unwrap_or(
                        file.subtests
                            .iter()
                            .map(|s| s.timeout_ms)
                            .max()
                            .unwrap_or(5000),
                    );
                    rows.push((
                        index,
                        run_file_isolated(&exe, &manifest_path, index, file, timeout_ms)?,
                    ));
                }
                Ok(rows)
            },
        ));
    }
    let mut by_index: BTreeMap<usize, FileResult> = BTreeMap::new();
    let mut first_error: Option<String> = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(rows)) if first_error.is_none() => {
                for (index, row) in rows {
                    by_index.insert(index, row);
                }
            }
            Ok(Ok(_rows)) => {}
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(_) => {
                if first_error.is_none() {
                    first_error = Some("worker slot panicked".to_owned());
                }
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(by_index.into_values().collect())
}

/// Runs the strict gate and optionally writes reports.
fn run(argv: &[String]) -> Result<i32, String> {
    let args = Args::parse(argv)?;
    // Filter is diagnostic-only: combining it with --strict would let the
    // gate pass on a subset while excluded files fail. Reject upfront.
    if args.strict && args.filter.is_some() {
        return Err("--filter cannot be combined with --strict".to_owned());
    }
    let manifest_text = read_text(&args.manifest)?;
    let today = today_utc();
    let manifest = load_manifest(&manifest_text, &today).map_err(|e| format_manifest_error(&e))?;
    if args.threads == 0 {
        return Err("--threads must be >= 1".to_owned());
    }
    if args
        .timeout_ms
        .is_some_and(|t| t == 0 || t > boa_fapi_wpt::manifest::MAX_TIMEOUT_MS)
    {
        return Err("--timeout-ms out of range 1..=300000".to_owned());
    }
    // Hash/path validation first: every CLI path (sequential and
    // parallel) reuses the same validated texts.
    let texts = verify_hashes(&args.manifest, &manifest)?;
    // Every CLI file execution — including default `--threads 1` (F12) —
    // runs through the isolated worker path with the wall deadline: the
    // in-process pump guard cannot interrupt a hung `run_jobs()`, so only
    // the process boundary is a hard timeout. `threads` selects only the
    // number of concurrent children; rows always re-sort by manifest
    // index. `run_file` stays the library mapping for unit tests.
    let slots = args.threads.min(manifest.files.len().max(1));
    let files = run_files_parallel(&args, &manifest, slots, &texts)?;
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

fn format_manifest_error(error: &ManifestError) -> String {
    format!("manifest error: {error}")
}

/// Prints the stable human summary (counts only, no secrets/paths detail).
///
/// `NOTRUN` rows count separately from `PASS`/`FAIL`: a gap is never
/// reported as a pass.
fn summarize(files: &[FileResult]) {
    let mut pass = 0;
    let mut notrun = 0;
    let mut fail = 0;
    for file in files {
        for sub in &file.subtests {
            if sub.actual.token() == "PASS" && sub.expected.token() == "PASS" {
                pass += 1;
            } else if sub.actual.token() == "NOTRUN" && sub.expected.token() == "NOTRUN" {
                notrun += 1;
            } else {
                fail += 1;
            }
        }
    }
    println!(
        "wpt: {pass} passed, {notrun} notrun, {fail} unexpected ({} files)",
        files.len()
    );
}

fn main() {
    // `--worker-file` is the internal isolated mode (F5): run exactly one
    // validated file and exit with the worker protocol (no strict gate, no
    // reports). The flag is accepted anywhere in argv; anything else
    // follows the normal CLI path.
    //
    // Both paths run on the dedicated 8 MiB thread below: Boa evaluation
    // is deeply recursive and the default Windows main-thread stack
    // (1 MiB) overflows where the test harness threads (2 MiB+) succeed.
    // The worker inherits this protection by routing through the same
    // spawn (it returns the worker exit code instead of the strict gate).
    let raw: Vec<String> = std::env::args().collect();
    let is_worker = raw.iter().any(|a| a == "--worker-file");
    let argv: Vec<String> = std::env::args().collect();
    let child = std::thread::Builder::new()
        .name("wpt-main".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            if is_worker {
                worker_main(&argv)
            } else {
                match run(&argv) {
                    Ok(code) => code,
                    Err(message) => {
                        eprintln!("boa_fapi_wpt: {message}");
                        2
                    }
                }
            }
        });
    match child {
        Ok(join) => match join.join() {
            Ok(code) => std::process::exit(code),
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

#[cfg(test)]
mod tests {
    use super::{Args, read_pipe, sha256_hex};
    use std::io::Cursor;

    #[test]
    fn bounded_pipe_capture_retains_prefix_and_drains_input() {
        let capture = read_pipe(Cursor::new(b"123456789".to_vec()), 4);
        assert_eq!(capture.bytes, b"12345");
        assert!(capture.overflow);
        assert!(!capture.read_failed);
    }

    #[test]
    fn cli_parser_accepts_all_file_run_options() {
        let argv = [
            "boa_fapi_wpt",
            "--manifest",
            "wpt-manifest.json",
            "--strict",
            "--threads",
            "2",
            "--filter",
            "corpus/blob",
            "--json",
            "target/report.json",
            "--junit",
            "target/report.xml",
            "--timeout-ms",
            "42",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let parsed = Args::parse(&argv);
        assert!(parsed.is_ok());
        if let Ok(args) = parsed {
            assert_eq!(args.manifest, "wpt-manifest.json");
            assert!(args.strict);
            assert_eq!(args.threads, 2);
            assert_eq!(args.filter.as_deref(), Some("corpus/blob"));
            assert_eq!(args.json.as_deref(), Some("target/report.json"));
            assert_eq!(args.junit.as_deref(), Some("target/report.xml"));
            assert_eq!(args.timeout_ms, Some(42));
        }
    }

    #[test]
    fn cli_parser_rejects_invalid_or_missing_options() {
        let missing_manifest = vec!["boa_fapi_wpt".to_owned()];
        assert!(Args::parse(&missing_manifest).is_err());
        let invalid_threads = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--threads".to_owned(),
            "0".to_owned(),
        ];
        assert!(Args::parse(&invalid_threads).is_err());
        let unknown_flag = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--unknown".to_owned(),
        ];
        assert!(Args::parse(&unknown_flag).is_err());
    }

    #[test]
    fn sha256_helper_matches_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
