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
    /// A recorded entry failed or JS evaluation threw.
    Fail,
    /// Async entries were still pending after the pump budget.
    Timeout,
}

impl ActualStatus {
    /// Renders the canonical status token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Timeout => "TIMEOUT",
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
#[must_use]
pub fn scrub_detail(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(512));
    for chunk in text.split_whitespace().take(48) {
        if chunk.starts_with("blob:") {
            out.push_str("blob:<redacted>");
        } else {
            out.push_str(chunk);
        }
        out.push(' ');
    }
    let trimmed = out.trim_end().to_owned();
    if trimmed.len() > 480 {
        trimmed[..480].to_owned()
    } else {
        trimmed
    }
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
    let mut passes = 0;
    while passes < options.max_pump_passes {
        if started.elapsed() > options.file_timeout {
            break;
        }
        if context.run_jobs().is_err() {
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
            // Capability gap: report NOTRUN-equivalent as expected status
            // with the manifest reason; the file must still have evaluated
            // cleanly (top-level throw fails the row explicitly).
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
                    actual: ActualStatus::Fail,
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
                // No entry recorded: top-level file error is FAIL,
                // otherwise the async entry never settled → TIMEOUT.
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
mod tests {
    use super::*;
    use crate::manifest::{ManifestSubtest, load_manifest};

    fn manifest_file(status: &str) -> ManifestFile {
        let text = format!(
            r#"{{"schema_version": 1, "source": {{"repository": "https://github.com/web-platform-tests/wpt", "commit": "0968c868d8095217d18d86b34c7f21dccae58768", "license": "BSD-3-Clause"}}, "default_timeout_ms": 5000, "files": [{{"path": "corpus/a.js", "upstream_path": "FileAPI/blob/a.any.js", "upstream_blob_sha": "43c29ada4d5455410ab40c79c5982de2b973d2ba", "sha256": "{}", "group": "FileAPI/blob", "capability": "blob", "subtests": [{{"test": "t", "subtest": "s", "status": "{status}", "reason": "needs X", "capability": "c", "owner": "o", "review_by": "2099-01-01", "trace": "M7-WPT-05"}}]}}]}}"#,
            "e".repeat(64)
        );
        load_manifest(&text, "2026-09-08")
            .expect("load")
            .files
            .pop()
            .expect("file")
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
        assert_eq!(result.subtests.len(), 1);
        assert_eq!(result.subtests[0].actual, ActualStatus::Pass);
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
        assert_eq!(result.subtests[0].actual, ActualStatus::Fail);
    }

    #[test]
    fn scrubber_redacts_blob_urls() {
        assert_eq!(
            scrub_detail("saw blob:https://x/uuid here"),
            "saw blob:<redacted> here"
        );
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
