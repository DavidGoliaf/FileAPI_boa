//! Per-test Boa execution: fresh `Context`, File API registration,
//! prelude injection, adapted file evaluation, bounded job pumping.
//!
//! Each manifest file runs in its own `Context` (isolation by
//! construction): a file failure, timeout or panic-equivalent cannot leak
//! jobs or globals into the next file. One terminal status per test/subtest
//! is derived from recorded harness results: `PASS` only when the named
//! subtest recorded exactly one passing entry, `FAIL` on a failing entry
//! or a JS evaluation error, `TIMEOUT` when the bounded pump budget is
//! exhausted while `async_test`/`promise_test` entries are still pending.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiExtension, FileApiHandle};
use thiserror::Error;

use crate::harness;
use crate::manifest::{ExpectedStatus, ManifestFile};

/// Terminal per-subtest outcome (before expectation comparison).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActualStatus {
    /// The subtest recorded exactly one passing entry.
    Pass,
    /// A recorded entry failed, JS evaluation threw, or a job errored.
    Fail,
    /// Async entries were still pending after the bounded pump budget.
    Timeout,
    /// Expected capability gap with a clean file evaluation.
    NotRun,
}

impl ActualStatus {
    /// Renders the canonical status token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Timeout => "TIMEOUT",
            Self::NotRun => "NOTRUN",
        }
    }
}

/// One executed subtest row.
#[derive(Debug, Clone)]
pub struct SubtestResult {
    /// Test ID from the manifest.
    pub test: String,
    /// Subtest name from the manifest.
    pub subtest: String,
    /// Observed terminal status.
    pub actual: ActualStatus,
    /// Expected terminal status.
    pub expected: ExpectedStatus,
    /// Short detail (failure text or gap reason; secrets stripped).
    pub detail: String,
    /// Trace row, e.g. `M7-WPT-01`.
    pub trace: String,
    /// Elapsed wall time in milliseconds (informational only, never part
    /// of the strict comparison).
    pub elapsed_ms: u64,
}

/// One executed file row.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// Manifest file path.
    pub path: String,
    /// Upstream path (provenance).
    pub upstream_path: String,
    /// WPT group.
    pub group: String,
    /// Subtest rows in manifest order.
    pub subtests: Vec<SubtestResult>,
}

/// Runner errors (launch failures, never silent `NOTRUN`).
#[derive(Debug, Error)]
pub enum RunError {
    /// Registration of the File API extension failed.
    #[error("extension registration failed")]
    Register,
    /// Prelude injection failed.
    #[error("harness prelude failed")]
    Prelude,
    /// Adapted file evaluation threw at top level.
    #[error("adapted file evaluation failed")]
    FileEval,
    /// Reading back harness results failed.
    #[error("result readback failed")]
    Readback,
}

/// Deterministic clock for WPT runs (no wall-clock reads in JS time).
#[derive(Debug)]
struct WptClock {
    millis: i64,
}

impl Clock for WptClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

/// Execution options for one file.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Per-subtest pump budget in `run_jobs()` passes.
    pub max_pump_passes: usize,
    /// Wall-clock guard per file (bounded timeout, manifest-driven).
    pub file_timeout: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            max_pump_passes: 64,
            file_timeout: Duration::from_secs(30),
        }
    }
}

/// Strips secret-looking material from detail text (defense in depth:
/// adapted files never emit secrets, but recorded messages pass through
/// this scrubber before reports).
///
/// Rules (F9): every `blob:` occurrence is replaced up to the next
/// whitespace/quote; `file://`, HTTP(S) URLs with path/query, Windows
/// drive paths, UNC paths and absolute Unix paths become fixed
/// placeholders; control characters except tab/newline/CR are dropped;
/// output is capped at 480 Unicode scalars / 48 tokens without splitting
/// a UTF-8 sequence; unclassifiable input becomes `<redacted-error>`.
#[must_use]
pub fn scrub_detail(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for chunk in text.split_whitespace().take(48) {
        tokens.push(scrub_token(chunk));
    }
    let mut out = tokens.join(" ");
    // Drop control characters except \t \n \r (XML 1.0 validity + report
    // hygiene); count in Unicode scalars and truncate on a char boundary.
    out = out
        .chars()
        .filter(|c| !c.is_control() || *c == '\t' || *c == '\n' || *c == '\r')
        .take(480)
        .collect();
    if out.trim().is_empty() && !text.trim().is_empty() {
        return "<redacted-error>".to_owned();
    }
    out
}

/// Scrubs one whitespace-delimited token.
///
/// `blob:` is redacted together with any trailing `)`/`,`/`;`/`]`/`}`
/// punctuation (the URL ends at whitespace or a quote); the punctuation
/// itself is preserved so `url(blob:u)` keeps its closing paren.
fn scrub_token(chunk: &str) -> String {
    // `blob:` anywhere inside the token (prefix or punctuation-adjacent)
    // redacts to the next whitespace/quote.
    if let Some(index) = chunk.find("blob:") {
        let before = &chunk[..index];
        let mut tail = &chunk[index + "blob:".len()..];
        // Strip one trailing quote/paren/comma/semicolon/bracket set from
        // the redacted span, then re-append it verbatim.
        let mut suffix = String::new();
        while tail.ends_with([')', ',', ';', ']', '}', '"', '\'']) {
            let cut = tail.len() - 1;
            suffix.insert(0, tail.as_bytes()[cut] as char);
            tail = &tail[..cut];
        }
        let _ = &tail;
        return format!("{before}blob:<redacted>{suffix}");
    }
    if chunk.starts_with("file://") {
        return "file:<redacted>".to_owned();
    }
    if chunk.starts_with("http://") || chunk.starts_with("https://") {
        // Keep scheme + host, redact path/query.
        let rest = &chunk[chunk.find("://").map(|i| i + 3).unwrap_or(0)..];
        let host = rest.split('/').next().unwrap_or("");
        let scheme = if chunk.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        return format!("{scheme}://{host}<redacted-path>");
    }
    if chunk.starts_with("\\\\") {
        return "<redacted-unc-path>".to_owned();
    }
    if chunk.len() > 2 && chunk.as_bytes()[1] == b':' {
        return "<redacted-drive-path>".to_owned();
    }
    if chunk.starts_with('/') {
        return "<redacted-abs-path>".to_owned();
    }
    chunk.to_owned()
}

/// Registers the File API extension into a fresh context.
fn fresh_context() -> Result<(Context, FileApiHandle), RunError> {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(WptClock {
            millis: 1_700_000_000_000,
        }))
        .build()
        .register(&mut context)
        .map_err(|_| RunError::Register)?;
    Ok((context, handle))
}

/// Reads a JS string evaluation, mapping failure to [`RunError::Readback`].
fn eval_string(context: &mut Context, source: &str) -> Result<String, RunError> {
    let value = context
        .eval(Source::from_bytes(source))
        .map_err(|_| RunError::Readback)?;
    value
        .to_string(context)
        .map(|s| s.to_std_string_lossy().to_string())
        .map_err(|_| RunError::Readback)
}

/// Reads a JS integer evaluation, mapping failure to [`RunError::Readback`].
///
/// `to_number` may invoke user `valueOf`/`toString`; the probe reads a
/// plain number property on the harness object, so no user code runs here.
fn eval_u64(context: &mut Context, source: &str) -> Result<u64, RunError> {
    let value = context
        .eval(Source::from_bytes(source))
        .map_err(|_| RunError::Readback)?;
    let number = value.to_number(context).map_err(|_| RunError::Readback)?;
    if !number.is_finite() || number < 0.0 {
        return Err(RunError::Readback);
    }
    // `as u64` saturates instead of wrapping; the caller additionally
    // clamps to 10 000 entries, so hostile lengths cannot allocate.
    Ok(number as u64)
}

/// Executes one manifest file and maps recorded entries to subtest rows.
///
/// The file runs in a fresh `Context`; the prelude installs first, then
/// the adapted source. `run_jobs()` is pumped up to `max_pump_passes`
/// passes (or `file_timeout` wall guard): entries recorded by then decide
/// `PASS`/`FAIL`; still-expected-but-unrecorded async entries decide
/// `TIMEOUT`. Subtests expected `NOTRUN` are never executed for status:
/// they are reported `NOTRUN` with the manifest gap reason when the file
/// itself evaluated cleanly.
pub fn run_file(
    file: &ManifestFile,
    source_text: &str,
    options: &RunOptions,
) -> Result<FileResult, RunError> {
    let started = Instant::now();
    let (context, _handle) = &mut fresh_context()?;
    let context: &mut Context = context;
    context
        .eval(Source::from_bytes(&harness::prelude_source(&file.path)))
        .map_err(|_| RunError::Prelude)?;
    let file_result = context.eval(Source::from_bytes(source_text));
    let file_error = file_result.err().map(|e| format!("{e:?}"));
    // Bounded pump: explicit job passes, no sleep, wall guard as backstop.
    // A job error is FAIL for every unsettled row (never a silent TIMEOUT
    // substitution): the flag below poisons all pending rows of this file.
    let mut job_failed = false;
    let mut passes = 0;
    while passes < options.max_pump_passes {
        if started.elapsed() > options.file_timeout {
            break;
        }
        if context.run_jobs().is_err() {
            job_failed = true;
            break;
        }
        passes += 1;
    }
    // Read back recorded entries: `pass|name|message` per index.
    // The count is clamped (10 000) and every per-index read is fallible:
    // a hostile file redefining the probe between reads degrades to fewer
    // entries (possibly TIMEOUT), never to a panic or an unbounded loop.
    let count = match eval_u64(context, harness::results_probe_source()) {
        Ok(n) => n.min(10_000),
        Err(_) => 0,
    };
    let mut recorded: Vec<(bool, String, String)> = Vec::new();
    for index in 0..count {
        // `entry` sources are built from an integer index only — no file
        // or manifest text is interpolated into evaluated JS.
        let Ok(entry) = eval_string(context, &harness::result_entry_source(index as usize)) else {
            break;
        };
        let mut parts = entry.splitn(3, '|');
        let pass = parts.next() == Some("1");
        let name = parts.next().unwrap_or("").to_owned();
        let message = parts.next().unwrap_or("").to_owned();
        recorded.push((pass, name, message));
    }
    // Index recorded entries by name (first entry wins; duplicates fail
    // the subtest explicitly instead of silently passing).
    let mut by_name: BTreeMap<String, Vec<(bool, String)>> = BTreeMap::new();
    for (pass, name, message) in recorded {
        by_name.entry(name).or_default().push((pass, message));
    }
    let expected_names: BTreeSet<String> =
        file.subtests.iter().map(|s| s.subtest.clone()).collect();
    let mut subtests = Vec::new();
    for sub in &file.subtests {
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if sub.expected == ExpectedStatus::NotRun {
            // Capability gap: clean evaluation reports actual NOTRUN with
            // the manifest reason; any top-level throw is FAIL instead.
            if let Some(error) = file_error.as_ref() {
                subtests.push(SubtestResult {
                    test: sub.test.clone(),
                    subtest: sub.subtest.clone(),
                    actual: ActualStatus::Fail,
                    expected: sub.expected,
                    detail: scrub_detail(error),
                    trace: sub.trace.clone(),
                    elapsed_ms,
                });
            } else {
                subtests.push(SubtestResult {
                    test: sub.test.clone(),
                    subtest: sub.subtest.clone(),
                    actual: ActualStatus::NotRun,
                    expected: sub.expected,
                    detail: scrub_detail(&format!("notrun: {}", sub.reason)),
                    trace: sub.trace.clone(),
                    elapsed_ms,
                });
            }
            continue;
        }
        match by_name.get(&sub.subtest) {
            Some(entries) if entries.len() == 1 && entries[0].0 => subtests.push(SubtestResult {
                test: sub.test.clone(),
                subtest: sub.subtest.clone(),
                actual: ActualStatus::Pass,
                expected: sub.expected,
                detail: String::new(),
                trace: sub.trace.clone(),
                elapsed_ms,
            }),
            Some(entries) if entries.len() == 1 => subtests.push(SubtestResult {
                test: sub.test.clone(),
                subtest: sub.subtest.clone(),
                actual: ActualStatus::Fail,
                expected: sub.expected,
                detail: scrub_detail(&entries[0].1),
                trace: sub.trace.clone(),
                elapsed_ms,
            }),
            Some(_) => subtests.push(SubtestResult {
                test: sub.test.clone(),
                subtest: sub.subtest.clone(),
                actual: ActualStatus::Fail,
                expected: sub.expected,
                detail: scrub_detail("duplicate harness entries"),
                trace: sub.trace.clone(),
                elapsed_ms,
            }),
            None => {
                // No entry recorded: a top-level file error or a job error
                // is FAIL for the row; otherwise the async entry never
                // settled → TIMEOUT. Readback degradation (fewer entries)
                // lands here as TIMEOUT, never as an empty vector.
                if job_failed {
                    subtests.push(SubtestResult {
                        test: sub.test.clone(),
                        subtest: sub.subtest.clone(),
                        actual: ActualStatus::Fail,
                        expected: sub.expected,
                        detail: scrub_detail("job error during pump"),
                        trace: sub.trace.clone(),
                        elapsed_ms,
                    });
                } else if let Some(error) = file_error.as_ref() {
                    subtests.push(SubtestResult {
                        test: sub.test.clone(),
                        subtest: sub.subtest.clone(),
                        actual: ActualStatus::Fail,
                        expected: sub.expected,
                        detail: scrub_detail(error),
                        trace: sub.trace.clone(),
                        elapsed_ms,
                    });
                } else {
                    subtests.push(SubtestResult {
                        test: sub.test.clone(),
                        subtest: sub.subtest.clone(),
                        actual: ActualStatus::Timeout,
                        expected: sub.expected,
                        detail: scrub_detail("no harness entry after pump budget"),
                        trace: sub.trace.clone(),
                        elapsed_ms,
                    });
                }
            }
        }
    }
    // Top-level evaluation throw fails every row of the file: no PASS
    // row may survive when the same adapted file threw at top level.
    if file_error.is_some() {
        for sub in subtests.iter_mut() {
            if sub.actual == ActualStatus::Pass {
                sub.actual = ActualStatus::Fail;
                sub.detail = scrub_detail("top-level file evaluation failed");
            }
        }
    }
    // Unexpected extra entries (recorded names outside the manifest) are
    // surfaced as file-level FAIL rows so strict mode breaks on them.
    // `unexpected:*` rows always expect PASS, so `strict_pass` (which
    // requires actual PASS for PASS-expected rows) fails on them.
    for name in by_name.keys() {
        if !expected_names.contains(name) {
            subtests.push(SubtestResult {
                test: file
                    .subtests
                    .first()
                    .map(|s| s.test.clone())
                    .unwrap_or_default(),
                subtest: format!("unexpected:{name}"),
                actual: ActualStatus::Fail,
                expected: ExpectedStatus::Pass,
                detail: scrub_detail("recorded entry without manifest expectation"),
                trace: file
                    .subtests
                    .first()
                    .map(|s| s.trace.clone())
                    .unwrap_or_default(),
                elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            });
        }
    }
    Ok(FileResult {
        path: file.path.clone(),
        upstream_path: file.upstream_path.clone(),
        group: file.group.clone(),
        subtests,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestSubtest, load_manifest};

    fn manifest_file(status: &str) -> ManifestFile {
        let sha = "e".repeat(64);
        let text = format!(
            "{{\"schema_version\": 1, \"source\": {{\"repository\": \"https://github.com/web-platform-tests/wpt\", \"commit\": \"0968c868d8095217d18d86b34c7f21dccae58768\", \"license\": \"BSD-3-Clause\"}}, \"corpus_root\": \"crates/boa_fapi_wpt/corpus\", \"default_timeout_ms\": 5000, \"files\": [{{\"path\": \"corpus/a.js\", \"upstream_path\": \"FileAPI/blob/a.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/blob\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t\", \"subtest\": \"s\", \"status\": \"{status}\", \"reason\": \"needs X\", \"capability\": \"c\", \"owner\": \"o\", \"review_by\": \"2099-01-01\", \"trace\": \"M7-WPT-05\"}}]}}, {{\"path\": \"corpus/b.js\", \"upstream_path\": \"FileAPI/file/b.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/file\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t2\", \"subtest\": \"s2\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}}]}}, {{\"path\": \"corpus/c.js\", \"upstream_path\": \"FileAPI/filelist-section/c.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/filelist-section\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t3\", \"subtest\": \"s3\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}}]}}, {{\"path\": \"corpus/d.js\", \"upstream_path\": \"FileAPI/reading-data-section/d.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/reading-data-section\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t4\", \"subtest\": \"s4\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}}]}}, {{\"path\": \"corpus/e.js\", \"upstream_path\": \"FileAPI/FileReader/e.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/FileReader\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t5\", \"subtest\": \"s5\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}}]}}, {{\"path\": \"corpus/f.js\", \"upstream_path\": \"FileAPI/BlobURL/f.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"FileAPI/BlobURL\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"t6\", \"subtest\": \"s6\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}}]}}]}}"
        );
        load_manifest(&text, "2026-09-08")
            .expect("load")
            .files
            .remove(0)
    }

    #[test]
    fn passing_file_maps_to_pass() {
        let file = manifest_file("PASS");
        let result = run_file(
            &file,
            "test(function() { assert_true(true); }, 's');",
            &RunOptions::default(),
        )
        .expect("run");
        let row = result
            .subtests
            .iter()
            .find(|s| s.subtest == "s")
            .expect("row s");
        assert_eq!(row.actual, ActualStatus::Pass);
    }

    #[test]
    fn failing_assertion_maps_to_fail() {
        let file = manifest_file("PASS");
        let result = run_file(
            &file,
            "test(function() { assert_true(false, 'nope'); }, 's');",
            &RunOptions::default(),
        )
        .expect("run");
        let row = result
            .subtests
            .iter()
            .find(|s| s.subtest == "s")
            .expect("row s");
        assert_eq!(row.actual, ActualStatus::Fail);
    }

    #[test]
    fn scrubber_redacts_blob_urls() {
        assert_eq!(
            scrub_detail("saw blob:https://x/uuid here"),
            "saw blob:<redacted> here"
        );
    }

    #[test]
    fn scrubber_redacts_paths_and_controls() {
        // `blob:` mid-token (punctuation-adjacent) redacts to whitespace,
        // preserving one trailing punctuation mark verbatim.
        assert_eq!(
            scrub_detail("url(blob:abc123) end"),
            "url(blob:<redacted>) end"
        );
        // URL / drive / UNC / absolute Unix paths become placeholders.
        assert_eq!(scrub_detail("at file:///tmp/x.js"), "at file:<redacted>");
        assert_eq!(
            scrub_detail("at https://host/a?b=1"),
            "at https://host<redacted-path>"
        );
        assert_eq!(
            scrub_detail("at C:\\Users\\x\\f.js"),
            "at <redacted-drive-path>"
        );
        assert_eq!(
            scrub_detail("at \\\\host\\share\\f"),
            "at <redacted-unc-path>"
        );
        assert_eq!(scrub_detail("at /home/user/f.js"), "at <redacted-abs-path>");
        // Control characters (except tab/newline/CR) are dropped; output
        // stays valid UTF-8 without mid-sequence slicing.
        assert_eq!(scrub_detail("a\u{1}b"), "ab");
        // Whitespace-only input carries no information: empty is honest
        // (callers emit `<redacted-error>` only for non-empty input that
        // scrubbed to nothing).
        assert_eq!(scrub_detail("   "), "");
        // 480-scalar cap keeps full code points (é is 1 scalar, 2 bytes).
        let long = "é".repeat(600);
        assert_eq!(scrub_detail(&long).chars().count(), 480);
    }

    #[test]
    fn manifest_subtest_shape_is_stable() {
        let sub = ManifestSubtest {
            test: "t".to_owned(),
            subtest: "s".to_owned(),
            timeout_ms: 1,
            expected: ExpectedStatus::Pass,
            reason: String::new(),
            capability: String::new(),
            owner: String::new(),
            review_by: String::new(),
            trace: String::new(),
        };
        assert_eq!(sub.timeout_ms, 1);
    }
}
