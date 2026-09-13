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

use boa_fapi_wpt::accounting::{self, AccountingError, ExitReason, RunMode};
use boa_fapi_wpt::hash::sha256_hex;
use boa_fapi_wpt::inventory::{Inventory, load_inventory};
use boa_fapi_wpt::manifest::{Manifest, ManifestError, ManifestSource, load_manifest_for_mode};
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
    smoke: bool,
    check_expectations: bool,
    expectations: Option<String>,
    upstream_root: Option<String>,
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
            smoke: false,
            check_expectations: false,
            expectations: None,
            upstream_root: None,
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
                "--smoke" => args.smoke = true,
                "--check-expectations" => args.check_expectations = true,
                "--expectations" => {
                    index += 1;
                    args.expectations = Some(
                        argv.get(index)
                            .cloned()
                            .ok_or("--expectations needs a value")?,
                    );
                }
                "--upstream-root" => {
                    index += 1;
                    args.upstream_root = Some(
                        argv.get(index)
                            .cloned()
                            .ok_or("--upstream-root needs a value")?,
                    );
                }
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
        // Mode rules (M9-E §2, M9E-R1 §3.2): `--smoke` is the offline
        // developer path (schema 1 accepted, reported as ADAPTED_SMOKE, no
        // release terminology); `--strict` is the release gate and
        // `--check-expectations` is the diagnostic observation mode. All
        // three are mutually exclusive, and the two gate modes require
        // `--expectations` plus `--upstream-root`.
        let modes = [args.smoke, args.strict, args.check_expectations]
            .into_iter()
            .filter(|set| *set)
            .count();
        if modes == 0 {
            return Err("one of --smoke, --strict or --check-expectations is required".to_owned());
        }
        if modes > 1 {
            return Err(
                "--smoke, --strict and --check-expectations are mutually exclusive".to_owned(),
            );
        }
        if (args.strict || args.check_expectations) && args.expectations.is_none() {
            return Err("--strict/--check-expectations need --expectations <path>".to_owned());
        }
        if (args.strict || args.check_expectations) && args.upstream_root.is_none() {
            return Err("--strict/--check-expectations need --upstream-root <dir>".to_owned());
        }
        if args.smoke && (args.expectations.is_some() || args.upstream_root.is_some()) {
            return Err("--smoke takes no --expectations/--upstream-root".to_owned());
        }
        Ok(args)
    }

    /// Gate mode token for this invocation (smoke otherwise).
    fn mode(&self) -> RunMode {
        if self.smoke {
            RunMode::Smoke
        } else if self.check_expectations {
            RunMode::CheckExpectations
        } else {
            RunMode::Strict
        }
    }
}

/// Reads a file to string (checked-in manifest/corpus only).
fn read_text(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|_| format!("cannot read `{path}`"))
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
    let manifest = match load_manifest_for_mode(&manifest_text, &today, false) {
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
            let json = report::worker_json(&row);
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
    use boa_fapi_wpt::manifest::parse_json;
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
    // Order-tolerant row matching with explicit extras (M9-E): the worker
    // serializes rows in manifest order, but the parent must not turn a
    // row-count/order skew into silent corruption. Match by exact (test,
    // subtest) id, accept any order; surface harness-recorded ids outside
    // the manifest as explicit `unexpected:<name>` FAIL rows (runner §5.3
    // contract) instead of degrading the whole file to TIMEOUT. Missing
    // ids, duplicate ids, or unknown status tokens stay corruption.
    let mut by_id: std::collections::BTreeMap<(String, String), &boa_fapi_wpt::manifest::Json> =
        std::collections::BTreeMap::new();
    for sub_json in subtests.iter() {
        let test = sub_json.field("test")?.as_str()?;
        let subtest = sub_json.field("subtest")?.as_str()?;
        if by_id
            .insert((test.to_owned(), subtest.to_owned()), sub_json)
            .is_some()
        {
            return None;
        }
    }
    let mut rows = Vec::new();
    for expected in file.subtests.iter() {
        let key = (expected.test.clone(), expected.subtest.clone());
        let sub_json = by_id.remove(&key)?;
        let actual = sub_json.field("actual")?.as_str()?;
        let detail = sub_json.field("detail")?.as_str().unwrap_or("");
        let test = expected.test.as_str();
        let subtest = expected.subtest.as_str();
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
            expected: expected.expected,
            detail: scrub_detail(detail),
            trace: expected.trace.clone(),
            elapsed_ms: 0,
        });
    }
    // Leftover worker ids are harness-recorded extras: one explicit FAIL
    // row each (never silent, never whole-file TIMEOUT).
    let mut extras: Vec<((String, String), String)> = Vec::new();
    for ((test, subtest), sub_json) in by_id {
        if !subtest.starts_with("unexpected:") {
            return None;
        }
        let detail = sub_json.field("detail")?.as_str().unwrap_or("").to_owned();
        match sub_json.field("actual")?.as_str()? {
            "FAIL" => extras.push(((test, subtest), detail)),
            _ => return None,
        }
    }
    extras.sort();
    for ((test, subtest), detail) in extras {
        rows.push(SubtestResult {
            test,
            subtest,
            actual: ActualStatus::Fail,
            expected: boa_fapi_wpt::manifest::ExpectedStatus::Pass,
            detail: scrub_detail(&detail),
            trace: file
                .subtests
                .first()
                .map(|s| s.trace.clone())
                .unwrap_or_default(),
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
    // NOTE: a global `--timeout-ms` override REPLACES the per-file wall
    // deadline (diagnostic knob, never the gate default): the manifest's
    // own `timeout_ms`/`default_timeout_ms` bounds each worker otherwise.
    // A small global override with a heavy file (Blob-slice fans out
    // ~280 promise reads across one worker) kills the worker mid-pump
    // and reports whole-file TIMEOUT — that is the knob working, not a
    // product failure. The gate never passes `--timeout-ms`.
    let mut chunks: Vec<Vec<usize>> = vec![Vec::new(); slots];
    for (position, index) in selected.into_iter().enumerate() {
        chunks[position % slots].push(index);
    }
    // One OS thread per slot; each thread runs its chunk sequentially
    // through isolated child processes. No Context/JsObject crosses
    // threads (only validated file records by index).
    let manifest_path = args.manifest.clone();
    let timeout_override = args.timeout_ms;
    let manifest_owned = manifest.for_worker(
        manifest.source.clone(),
        manifest.corpus_root.clone(),
        manifest.default_timeout_ms,
        manifest.files.clone(),
    );
    let mut handles = Vec::new();
    for chunk in chunks {
        let manifest_path = manifest_path.clone();
        let manifest_owned = manifest_owned.for_worker(
            manifest_owned.source.clone(),
            manifest_owned.corpus_root.clone(),
            manifest_owned.default_timeout_ms,
            manifest_owned.files.clone(),
        );
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

/// Verifies the pinned upstream tree under `--upstream-root` (M9-E §3).
///
/// No shell, no network: only [`std::fs`] reads under the given root.
/// Checks, in order:
///
/// 1. `wpt-inventory.json` (beside the manifest) pins the same
///    repository/commit as the manifest source;
/// 2. every inventory path resolves strictly inside the canonicalized
///    root (no absolute path, no `..` escape, no symlink escape — any
///    symlink on the candidate prefix is a launch error; case collisions
///    between two inventory paths resolving to one canonical path are a
///    launch error);
/// 3. the enumerated `FileAPI/**` file set under the root equals the
///    inventory set exactly (missing/extra file is a launch error);
/// 4. every manifest `upstream_path` is in the inventory and its raw
///    bytes hash to the manifest `upstream_sha256` (changed file is a
///    launch error; `upstream_blob_sha` is never evidence).
///
/// Error details carry only manifest-relative logical paths, never
/// absolute filesystem paths.
///
/// The inventory file is located beside `manifest_path` (the `--manifest`
/// argument); the upstream tree is enumerated with `std::fs` only.
fn verify_upstream_tree_impl(
    manifest: &Manifest,
    upstream_root: &str,
    manifest_path: Option<&str>,
) -> Result<Inventory, String> {
    use std::path::Path;
    let root = Path::new(upstream_root);
    let canonical_root = root
        .canonicalize()
        .map_err(|_| "cannot resolve --upstream-root".to_owned())?;
    // Inventory lives beside the manifest; fall back to CWD-relative.
    let inventory_text = match manifest_path {
        Some(path) => {
            let anchor = Path::new(path)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| Path::new(".").to_path_buf());
            std::fs::read_to_string(anchor.join("wpt-inventory.json"))
                .map_err(|_| "cannot read wpt-inventory.json".to_owned())?
        }
        None => std::fs::read_to_string("wpt-inventory.json")
            .map_err(|_| "cannot read wpt-inventory.json".to_owned())?,
    };
    let inventory = load_inventory(&inventory_text, &manifest.source)
        .map_err(|e| format!("inventory error: {e}"))?;
    verify_upstream_tree_with_inventory(manifest, &canonical_root, &inventory)?;
    Ok(inventory)
}

/// Pure inventory/tree comparison: enumerates `FileAPI/**` under the
/// canonical root and compares the set plus raw-content hashes against the
/// parsed [`Inventory`].
fn verify_upstream_tree_with_inventory(
    manifest: &Manifest,
    canonical_root: &std::path::Path,
    inventory: &Inventory,
) -> Result<(), String> {
    use std::collections::{BTreeMap, BTreeSet};
    // Case-collision guard: two inventory paths must not canonicalize to
    // one filesystem path (checked after enumeration below per actual
    // on-disk resolution).
    let mut on_disk: BTreeMap<String, String> = BTreeMap::new();
    let mut stack = vec![canonical_root.join("FileAPI")];
    while let Some(dir) = stack.pop() {
        // Symlink directories are rejected: traversal must stay inside.
        let dir_meta = std::fs::symlink_metadata(&dir)
            .map_err(|_| "cannot enumerate --upstream-root".to_owned())?;
        if dir_meta.file_type().is_symlink() {
            return Err("symlink escape in --upstream-root".to_owned());
        }
        let read =
            std::fs::read_dir(&dir).map_err(|_| "cannot enumerate --upstream-root".to_owned())?;
        for child in read {
            let child = child.map_err(|_| "cannot enumerate --upstream-root".to_owned())?;
            let file_type = child
                .file_type()
                .map_err(|_| "cannot enumerate --upstream-root".to_owned())?;
            if file_type.is_symlink() {
                return Err("symlink escape in --upstream-root".to_owned());
            }
            let path = child.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let canonical = path
                    .canonicalize()
                    .map_err(|_| "cannot enumerate --upstream-root".to_owned())?;
                if !canonical.starts_with(canonical_root) {
                    return Err("upstream escape in --upstream-root".to_owned());
                }
                let relative = canonical
                    .strip_prefix(canonical_root)
                    .map_err(|_| "upstream escape in --upstream-root".to_owned())?;
                let mut logical = relative.to_string_lossy().replace('\\', "/");
                if !logical.starts_with("FileAPI/") {
                    // Canonical root itself may be cased differently; the
                    // logical path is anchored at FileAPI.
                    logical = format!("FileAPI/{}", logical);
                }
                if on_disk
                    .insert(logical.clone(), canonical.to_string_lossy().into_owned())
                    .is_some()
                {
                    return Err(format!("case collision at `{logical}`"));
                }
            }
        }
    }
    let disk_set: BTreeSet<String> = on_disk.keys().cloned().collect();
    let inv_set: BTreeSet<String> = inventory.entries.iter().map(|e| e.path.clone()).collect();
    if disk_set != inv_set {
        let missing: Vec<&String> = inv_set.difference(&disk_set).collect();
        let extra: Vec<&String> = disk_set.difference(&inv_set).collect();
        if let Some(path) = missing.first() {
            return Err(format!("upstream file missing `{path}`"));
        }
        if let Some(path) = extra.first() {
            return Err(format!("upstream file extra `{path}`"));
        }
        return Err("upstream inventory drift".to_owned());
    }
    // Raw-content evidence for every manifest upstream file.
    for file in &manifest.files {
        let Some(entry) = inventory.get(&file.upstream_path) else {
            return Err(format!("upstream file missing `{}`", file.upstream_path));
        };
        if entry.sha256 != file.upstream_sha256 {
            return Err(format!("upstream hash drift for `{}`", file.upstream_path));
        }
        let candidate = canonical_root.join(&file.upstream_path);
        let bytes = std::fs::read(&candidate)
            .map_err(|_| format!("cannot read upstream file `{}`", file.upstream_path))?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(format!("upstream file too large `{}`", file.upstream_path));
        }
        let digest = sha256_hex(&bytes);
        if digest != file.upstream_sha256 {
            return Err(format!("upstream hash drift for `{}`", file.upstream_path));
        }
    }
    Ok(())
}

/// Gate inputs verified before any file executes.
struct GateInputs {
    inventory: Inventory,
    expectations: Vec<boa_fapi_wpt::manifest::ExpectationRow>,
}

/// A CLI failure with its distinct JSON exit reason (M9E-R1 §3.2).
struct CliFailure {
    reason: ExitReason,
    message: String,
}

impl CliFailure {
    fn new(reason: ExitReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl From<AccountingError> for CliFailure {
    fn from(error: AccountingError) -> Self {
        Self {
            reason: error.reason(),
            message: error.to_string(),
        }
    }
}

/// Emits a minimal failure JSON (when `--json` was requested) plus the
/// stderr message; the JSON keeps the distinct `exit_reason` class.
fn emit_failure(args: &Args, mode: RunMode, failure: &CliFailure, source: Option<&ManifestSource>) {
    if let Some(path) = args.json.as_ref() {
        let json = report::failure_json(mode, failure.reason, source, &failure.message);
        let _ = std::fs::write(path, json);
    }
    eprintln!("boa_fapi_wpt: {}", failure.message);
}

/// Verifies gate inputs (inventory + expectations) before execution.
fn prepare_gate_inputs(
    args: &Args,
    manifest: &Manifest,
    today: &str,
) -> Result<GateInputs, CliFailure> {
    let upstream_root = args.upstream_root.as_deref().ok_or_else(|| {
        CliFailure::new(ExitReason::Integrity, "--upstream-root <dir> is required")
    })?;
    let expectations_path = args.expectations.as_deref().ok_or_else(|| {
        CliFailure::new(ExitReason::Integrity, "--expectations <path> is required")
    })?;
    let inventory =
        verify_upstream_tree_impl(manifest, upstream_root, Some(args.manifest.as_str()))
            .map_err(|message| CliFailure::new(ExitReason::Integrity, message))?;
    let expectations_text = read_text(expectations_path)
        .map_err(|message| CliFailure::new(ExitReason::Integrity, message))?;
    let rows =
        boa_fapi_wpt::manifest::load_expectations(&expectations_text, today, &manifest.source)
            .map_err(|error| {
                CliFailure::new(
                    ExitReason::ExpectationDrift,
                    format!("expectations error: {error}"),
                )
            })?;
    boa_fapi_wpt::manifest::resolve_expectations(manifest, &rows).map_err(|error| {
        CliFailure::new(
            ExitReason::ExpectationDrift,
            format!("expectations error: {error}"),
        )
    })?;
    Ok(GateInputs {
        inventory,
        expectations: rows,
    })
}

/// Runs the requested mode and optionally writes reports.
fn run(argv: &[String]) -> Result<i32, String> {
    let args = Args::parse(argv)?;
    let mode = args.mode();
    let strict_effective = args.strict || args.check_expectations;
    // Filter is diagnostic-only: combining it with a gate would let the
    // gate pass on a subset while excluded files fail. Reject upfront.
    if strict_effective && args.filter.is_some() {
        return Err("--filter cannot be combined with --strict/--check-expectations".to_owned());
    }
    if args.smoke && args.filter.is_some() {
        return Err("--filter cannot be combined with --smoke".to_owned());
    }
    let manifest_text = match read_text(&args.manifest) {
        Ok(text) => text,
        Err(message) => {
            let failure = CliFailure::new(ExitReason::Integrity, message);
            emit_failure(&args, mode, &failure, None);
            return Ok(failure.reason.exit_code());
        }
    };
    let today = today_utc();
    let manifest = match load_manifest_for_mode(&manifest_text, &today, strict_effective) {
        Ok(manifest) => manifest,
        Err(error) => {
            let failure = CliFailure::new(ExitReason::Integrity, format_manifest_error(&error));
            emit_failure(&args, mode, &failure, None);
            return Ok(failure.reason.exit_code());
        }
    };
    // Mode/schema agreement (M9-E §2): smoke accepts schema 1 (legacy
    // adapted manifest) or schema 2 (runs the same corpus, still labelled
    // ADAPTED_SMOKE); a gate mode requires schema 2 plus inventory and
    // expectations.
    if args.smoke && manifest.schema_version() < boa_fapi_wpt::manifest::MIN_SMOKE_SCHEMA_VERSION {
        return Err("smoke needs manifest schema >= 1".to_owned());
    }
    if strict_effective && manifest.schema_version() != boa_fapi_wpt::manifest::SCHEMA_VERSION {
        return Err("strict needs manifest schema_version 2".to_owned());
    }
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
    let texts = match verify_hashes(&args.manifest, &manifest) {
        Ok(texts) => texts,
        Err(message) => {
            let failure = CliFailure::new(ExitReason::Integrity, message);
            emit_failure(&args, mode, &failure, Some(&manifest.source));
            return Ok(failure.reason.exit_code());
        }
    };
    // Gate inputs (M9-E §3/§4): inventory + expectations are verified
    // before any file executes. Failures keep a distinct JSON reason.
    let gate = if strict_effective {
        match prepare_gate_inputs(&args, &manifest, &today) {
            Ok(gate) => Some(gate),
            Err(failure) => {
                emit_failure(&args, mode, &failure, Some(&manifest.source));
                return Ok(failure.reason.exit_code());
            }
        }
    } else {
        None
    };
    // Every CLI file execution — including default `--threads 1` (F12) —
    // runs through the isolated worker path with the wall deadline: the
    // in-process pump guard cannot interrupt a hung `run_jobs()`, so only
    // the process boundary is a hard timeout. `threads` selects only the
    // number of concurrent children; rows always re-sort by manifest
    // index. `run_file` stays the library mapping for unit tests.
    let slots = args.threads.min(manifest.files.len().max(1));
    let mut files = match run_files_parallel(&args, &manifest, slots, &texts) {
        Ok(files) => files,
        Err(message) => {
            let failure = CliFailure::new(ExitReason::ExecutionFailure, message);
            emit_failure(&args, mode, &failure, Some(&manifest.source));
            return Ok(failure.reason.exit_code());
        }
    };
    if files.is_empty() {
        return Err("filter matched no manifest files".to_owned());
    }
    if let Some(gate) = gate.as_ref() {
        // Tracker rows (`DYNAMIC: ...` NOTRUN exclusions) attach to their
        // manifest file here so totals cover every gate id. A tracker id
        // already present in the manifest/result is never synthesized
        // twice (M9E-R1 §4.2.3). File-level exclusion rows are represented
        // separately by the canonical model, not per file.
        for file in files.iter_mut() {
            let Some(manifest_file) = manifest.files.iter().find(|f| f.path == file.path) else {
                continue;
            };
            file.subtests
                .extend(boa_fapi_wpt::manifest::tracker_subtests(
                    manifest_file,
                    &gate.expectations,
                ));
        }
    }
    // One canonical model is the single source for JSON, JUnit, console and
    // the exit code (M9E-R1 §5).
    let run = match gate {
        Some(gate) => match accounting::build_gate_run(
            mode,
            &manifest,
            &gate.inventory,
            &gate.expectations,
            files,
        ) {
            Ok(run) => run,
            Err(error) => {
                let failure = CliFailure::from(error);
                emit_failure(&args, mode, &failure, Some(&manifest.source));
                return Ok(failure.reason.exit_code());
            }
        },
        None => match accounting::build_smoke_run(&manifest, files) {
            Ok(run) => run,
            Err(error) => {
                let failure = CliFailure::from(error);
                emit_failure(&args, mode, &failure, Some(&manifest.source));
                return Ok(failure.reason.exit_code());
            }
        },
    };
    let json = report::to_json(&run);
    let junit = report::to_junit(&run);
    if let Some(path) = args.json.as_ref() {
        std::fs::write(path, json).map_err(|_| format!("cannot write `{path}`"))?;
    } else {
        println!("{json}");
    }
    if let Some(path) = args.junit.as_ref() {
        std::fs::write(path, junit).map_err(|_| format!("cannot write `{path}`"))?;
    }
    println!("{}", report::summary_line(&run));
    if mode == RunMode::Strict && !run.release_green {
        // Recorded open defects satisfy `expectations_match` but keep the
        // release gate red: M9-F must not ship with known FAIL. Stderr
        // note only — reports stay deterministic.
        eprintln!(
            "boa_fapi_wpt: note: release gate red: {} defect(s), {} timeout(s), {} unexpected, {} drift",
            run.release_blockers.defects,
            run.release_blockers.timeouts,
            run.release_blockers.unexpected,
            run.release_blockers.expectation_drift
        );
    }
    if run.success() {
        Ok(0)
    } else {
        Ok(run.exit_reason.exit_code())
    }
}

fn format_manifest_error(error: &ManifestError) -> String {
    format!("manifest error: {error}")
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
    use super::{Args, read_pipe};
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
            "--expectations",
            "expectations.json",
            "--upstream-root",
            "wpt-checkout",
            "--strict",
            "--threads",
            "2",
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
            assert!(!args.smoke);
            assert_eq!(args.expectations.as_deref(), Some("expectations.json"));
            assert_eq!(args.upstream_root.as_deref(), Some("wpt-checkout"));
            assert_eq!(args.threads, 2);
            assert_eq!(args.json.as_deref(), Some("target/report.json"));
            assert_eq!(args.junit.as_deref(), Some("target/report.xml"));
            assert_eq!(args.timeout_ms, Some(42));
        }
    }

    #[test]
    fn cli_parser_accepts_smoke_without_gate_inputs() {
        let argv = ["boa_fapi_wpt", "--manifest", "wpt-manifest.json", "--smoke"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let parsed = Args::parse(&argv);
        assert!(parsed.is_ok());
        if let Ok(parsed) = parsed {
            assert!(parsed.smoke);
            assert!(!parsed.strict);
        }
    }

    #[test]
    fn cli_parser_rejects_mode_conflicts() {
        // Smoke + strict are exclusive.
        let both = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--smoke".to_owned(),
            "--strict".to_owned(),
            "--expectations".to_owned(),
            "expectations.json".to_owned(),
            "--upstream-root".to_owned(),
            "wpt-checkout".to_owned(),
        ];
        assert!(Args::parse(&both).is_err());
        // Strict without gate inputs is rejected.
        let strict_only = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--strict".to_owned(),
        ];
        assert!(Args::parse(&strict_only).is_err());
        // Check-expectations without gate inputs is rejected.
        let check_only = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--check-expectations".to_owned(),
        ];
        assert!(Args::parse(&check_only).is_err());
        // Strict + check-expectations are exclusive.
        let strict_check = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--strict".to_owned(),
            "--check-expectations".to_owned(),
            "--expectations".to_owned(),
            "expectations.json".to_owned(),
            "--upstream-root".to_owned(),
            "wpt-checkout".to_owned(),
        ];
        assert!(Args::parse(&strict_check).is_err());
        // Smoke with gate inputs is rejected.
        let smoke_gate = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
            "--smoke".to_owned(),
            "--expectations".to_owned(),
            "expectations.json".to_owned(),
        ];
        assert!(Args::parse(&smoke_gate).is_err());
    }

    #[test]
    fn cli_parser_accepts_check_expectations_mode() {
        let argv = [
            "boa_fapi_wpt",
            "--manifest",
            "wpt-manifest.json",
            "--expectations",
            "expectations.json",
            "--upstream-root",
            "wpt-checkout",
            "--check-expectations",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let parsed = Args::parse(&argv);
        assert!(parsed.is_ok());
        if let Ok(parsed) = parsed {
            assert!(parsed.check_expectations);
            assert!(!parsed.strict);
            assert_eq!(parsed.mode().token(), "WPT_CHECK_EXPECTATIONS");
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
        // A bare manifest with no explicit mode is rejected (never a
        // silently mislabelled smoke/strict run).
        let no_mode = vec![
            "boa_fapi_wpt".to_owned(),
            "--manifest".to_owned(),
            "wpt-manifest.json".to_owned(),
        ];
        assert!(Args::parse(&no_mode).is_err());
    }
}
