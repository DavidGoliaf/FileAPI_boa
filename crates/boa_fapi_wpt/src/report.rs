//! Deterministic JSON/JUnit/console serialization from the canonical run.
//!
//! Reports are stable by construction: files and subtests serialize in
//! manifest order, object keys are emitted in fixed order, and the
//! comparison-relevant section carries no timestamps, no random IDs and
//! no absolute paths. `elapsed_ms` is informational per subtest (never
//! compared by strict mode).
//!
//! Every serializer takes the one validated [`CanonicalRun`]: totals are
//! never recomputed independently (M9E-R1 §5).

use std::fmt::Write;

use crate::accounting::{
    CanonicalRun, ExitReason, InventoryTotals, ReleaseBlockers, ResultsTotals, RunMode,
};
use crate::manifest::ManifestSource;
use crate::runner::{ActualStatus, FileResult};

/// Escapes a string for JSON double-quoted output.
#[must_use]
pub fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Escapes a string for XML attribute/text output (XML 1.0 validity:
/// control characters except tab/newline/CR are dropped — they are
/// illegal even escaped; DEL is dropped as well).
#[must_use]
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {}
            c => out.push(c),
        }
    }
    out
}

/// Serializes the canonical run as deterministic JSON (schema 2).
#[must_use]
pub fn to_json(run: &CanonicalRun) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"schema_version\":{},\"mode\":\"{}\",\"source\":{{\"repository\":\"{}\",\"commit\":\"{}\",\"license\":\"{}\"}},",
        run.schema_version,
        run.mode.token(),
        json_escape(&run.source.repository),
        json_escape(&run.source.commit),
        json_escape(&run.source.license)
    );
    let _ = write!(
        out,
        "\"expectations_match\":{},\"release_green\":{},\"release_blockers\":{},",
        run.expectations_match,
        run.release_green,
        blockers_json(&run.release_blockers)
    );
    let _ = write!(out, "\"exit_reason\":\"{}\",", run.exit_reason.token());
    match run.inventory.as_ref() {
        Some(inventory) => {
            out.push_str("\"inventory\":");
            out.push_str(&inventory_json(inventory));
        }
        None => out.push_str("\"inventory\":null"),
    }
    out.push(',');
    out.push_str("\"results\":");
    out.push_str(&results_json(&run.results));
    out.push(',');
    out.push_str("\"files\":[");
    for (index, file) in run.files.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&file_json(file));
    }
    out.push(']');
    out.push_str(",\"exclusions\":[");
    for (index, exclusion) in run.exclusions.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"path\":\"{}\",\"test\":\"{}\",\"capability\":\"{}\",\"reason\":\"{}\",\"owner\":\"{}\",\"issue\":\"{}\",\"review_by\":\"{}\",\"trace\":\"{}\"}}",
            json_escape(&exclusion.path),
            json_escape(&exclusion.test),
            json_escape(&exclusion.capability),
            json_escape(&exclusion.reason),
            json_escape(&exclusion.owner),
            json_escape(&exclusion.issue),
            json_escape(&exclusion.review_by),
            json_escape(&exclusion.trace)
        );
    }
    out.push(']');
    out.push('}');
    out
}

/// Serializes one executed file entry (shared by canonical JSON and the
/// isolated-worker protocol).
#[must_use]
pub fn file_json(file: &FileResult) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"path\":\"{}\",\"upstream_path\":\"{}\",\"group\":\"{}\",\"subtests\":[",
        json_escape(&file.path),
        json_escape(&file.upstream_path),
        json_escape(&file.group)
    );
    for (index, sub) in file.subtests.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"test\":\"{}\",\"subtest\":\"{}\",\"actual\":\"{}\",\"expected\":\"{}\",\"trace\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(&sub.test),
            json_escape(&sub.subtest),
            sub.actual.token(),
            sub.expected.token(),
            json_escape(&sub.trace),
            json_escape(&sub.detail)
        );
    }
    out.push_str("]}");
    out
}

/// Serializes the isolated-worker protocol envelope (one file).
#[must_use]
pub fn worker_json(file: &FileResult) -> String {
    format!("{{\"files\":[{}]}}", file_json(file))
}

fn inventory_json(totals: &InventoryTotals) -> String {
    format!(
        "{{\"total\":{},\"executed_direct\":{},\"executed_adapted\":{},\"excluded\":{},\"unaccounted\":{}}}",
        totals.total,
        totals.executed_direct,
        totals.executed_adapted,
        totals.excluded,
        totals.unaccounted
    )
}

fn results_json(totals: &ResultsTotals) -> String {
    format!(
        "{{\"total\":{},\"unique\":{},\"upstream_pass\":{},\"smoke_pass\":{},\"defects\":{},\"notrun\":{},\"unexpected\":{}}}",
        totals.total,
        totals.unique,
        totals.upstream_pass,
        totals.smoke_pass,
        totals.defects,
        totals.notrun,
        totals.unexpected
    )
}

fn blockers_json(blockers: &ReleaseBlockers) -> String {
    format!(
        "{{\"defects\":{},\"timeouts\":{},\"unexpected\":{},\"expectation_drift\":{},\"not_a_release_mode\":{}}}",
        blockers.defects,
        blockers.timeouts,
        blockers.unexpected,
        blockers.expectation_drift,
        blockers.not_a_release_mode
    )
}

/// Serializes the canonical run as deterministic JUnit XML.
///
/// `NOTRUN` rows (including the synthetic file-level exclusion suite)
/// serialize as `<skipped>`; `FAIL`/`TIMEOUT` rows serialize as
/// `<failure>`. Aggregate `tests`/`failures`/`skipped` on `<testsuites>`
/// equal the canonical `results` totals.
#[must_use]
pub fn to_junit(run: &CanonicalRun) -> String {
    let mut out = String::new();
    let mut total_tests = 0usize;
    let mut total_failures = 0usize;
    let mut total_skipped = 0usize;
    let mut body = String::new();
    for file in &run.files {
        let failures = file
            .subtests
            .iter()
            .filter(|s| s.actual == ActualStatus::Fail || s.actual == ActualStatus::Timeout)
            .count();
        let skipped = file
            .subtests
            .iter()
            .filter(|s| s.actual == ActualStatus::NotRun)
            .count();
        total_tests += file.subtests.len();
        total_failures += failures;
        total_skipped += skipped;
        let _ = write!(
            body,
            "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\">",
            xml_escape(&file.path),
            file.subtests.len(),
            failures,
            skipped
        );
        for sub in &file.subtests {
            let _ = write!(
                body,
                "<testcase classname=\"{}\" name=\"{}\">",
                xml_escape(&sub.test),
                xml_escape(&sub.subtest)
            );
            match sub.actual {
                ActualStatus::Pass => {}
                ActualStatus::NotRun => {
                    let _ = write!(body, "<skipped message=\"{}\"/>", xml_escape(&sub.detail));
                }
                ActualStatus::Fail | ActualStatus::Timeout => {
                    let _ = write!(
                        body,
                        "<failure message=\"actual={} expected={}\">{}</failure>",
                        sub.actual.token(),
                        sub.expected.token(),
                        xml_escape(&sub.detail)
                    );
                }
            }
            body.push_str("</testcase>");
        }
        body.push_str("</testsuite>");
    }
    // Synthetic suite: file-level exclusions are visible with path,
    // capability, reason, owner, issue and review date (M9E-R1 §5).
    if !run.exclusions.is_empty() {
        total_tests += run.exclusions.len();
        total_skipped += run.exclusions.len();
        let _ = write!(
            body,
            "<testsuite name=\"file-level-exclusions\" tests=\"{}\" failures=\"0\" skipped=\"{}\">",
            run.exclusions.len(),
            run.exclusions.len()
        );
        for exclusion in &run.exclusions {
            let detail = format!(
                "path={} capability={} reason={} owner={} issue={} review_by={}",
                exclusion.path,
                exclusion.capability,
                exclusion.reason,
                exclusion.owner,
                exclusion.issue,
                exclusion.review_by
            );
            let _ = write!(
                body,
                "<testcase classname=\"file-level-exclusions\" name=\"{}\"><skipped message=\"{}\"/></testcase>",
                xml_escape(&exclusion.path),
                xml_escape(&detail)
            );
        }
        body.push_str("</testsuite>");
    }
    let _ = write!(
        out,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\">",
        run.mode.token(),
        total_tests,
        total_failures,
        total_skipped
    );
    out.push_str(&body);
    out.push_str("</testsuites>");
    out
}

/// One stable console summary line derived from the canonical run.
#[must_use]
pub fn summary_line(run: &CanonicalRun) -> String {
    let inventory = match run.inventory.as_ref() {
        Some(inventory) => format!(
            "inventory={}/{} ({} direct, {} adapted, {} excluded, {} unaccounted)",
            inventory.total - inventory.unaccounted,
            inventory.total,
            inventory.executed_direct,
            inventory.executed_adapted,
            inventory.excluded,
            inventory.unaccounted
        ),
        None => "inventory=n/a".to_owned(),
    };
    format!(
        "{}: expectations_match={} release_green={} exit_reason={} {} results={}/{} ({} upstream pass, {} smoke pass, {} defects, {} notrun, {} unexpected)",
        run.mode.token(),
        run.expectations_match,
        run.release_green,
        run.exit_reason.token(),
        inventory,
        run.results.unique,
        run.results.total,
        run.results.upstream_pass,
        run.results.smoke_pass,
        run.results.defects,
        run.results.notrun,
        run.results.unexpected
    )
}

/// Minimal failure report emitted when the run aborts before a canonical
/// model exists (integrity/drift): the JSON still carries a distinct
/// `exit_reason` (M9E-R1 §3.2).
#[must_use]
pub fn failure_json(
    mode: RunMode,
    reason: ExitReason,
    source: Option<&ManifestSource>,
    message: &str,
) -> String {
    let source = match source {
        Some(source) => format!(
            "{{\"repository\":\"{}\",\"commit\":\"{}\",\"license\":\"{}\"}}",
            json_escape(&source.repository),
            json_escape(&source.commit),
            json_escape(&source.license)
        ),
        None => "null".to_owned(),
    };
    format!(
        "{{\"schema_version\":{},\"mode\":\"{}\",\"source\":{},\"expectations_match\":false,\"release_green\":false,\"release_blockers\":{},\"exit_reason\":\"{}\",\"error\":\"{}\",\"inventory\":null,\"results\":{{\"total\":0,\"unique\":0,\"upstream_pass\":0,\"smoke_pass\":0,\"defects\":0,\"notrun\":0,\"unexpected\":0}},\"files\":[],\"exclusions\":[]}}",
        crate::accounting::REPORT_SCHEMA_VERSION,
        mode.token(),
        source,
        blockers_json(&ReleaseBlockers::default()),
        reason.token(),
        json_escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{
        CanonicalRun, ExitReason, InventoryTotals, ReleaseBlockers, ResultsTotals, RunMode,
    };
    use crate::manifest::{ExpectedStatus, ManifestSource};
    use crate::runner::{ActualStatus, FileResult, SubtestResult};

    fn run() -> CanonicalRun {
        CanonicalRun {
            schema_version: 2,
            mode: RunMode::Strict,
            source: ManifestSource {
                repository: "https://github.com/web-platform-tests/wpt".to_owned(),
                commit: "0968c868d8095217d18d86b34c7f21dccae58768".to_owned(),
                license: "BSD-3-Clause".to_owned(),
            },
            expectations_match: true,
            release_green: false,
            release_blockers: ReleaseBlockers {
                defects: 1,
                ..ReleaseBlockers::default()
            },
            inventory: Some(InventoryTotals {
                total: 2,
                executed_direct: 1,
                executed_adapted: 0,
                excluded: 1,
                unaccounted: 0,
            }),
            results: ResultsTotals {
                total: 2,
                unique: 2,
                upstream_pass: 1,
                smoke_pass: 0,
                defects: 1,
                notrun: 0,
                unexpected: 0,
            },
            files: vec![FileResult {
                path: "corpus/a.js".to_owned(),
                upstream_path: "FileAPI/blob/a.any.js".to_owned(),
                group: "FileAPI/blob".to_owned(),
                subtests: vec![
                    SubtestResult {
                        test: "t".to_owned(),
                        subtest: "p".to_owned(),
                        actual: ActualStatus::Pass,
                        expected: ExpectedStatus::Pass,
                        detail: String::new(),
                        trace: "M9E-WPT-03".to_owned(),
                        elapsed_ms: 0,
                    },
                    SubtestResult {
                        test: "t".to_owned(),
                        subtest: "d".to_owned(),
                        actual: ActualStatus::Fail,
                        expected: ExpectedStatus::Fail,
                        detail: "open defect".to_owned(),
                        trace: "M9E-WPT-03".to_owned(),
                        elapsed_ms: 0,
                    },
                ],
            }],
            exclusions: vec![crate::accounting::ExclusionRow {
                path: "FileAPI/blob/b.any.js".to_owned(),
                test: "b.any.js".to_owned(),
                capability: "navigation".to_owned(),
                reason: "requires navigation".to_owned(),
                owner: "m9e".to_owned(),
                issue: "QUESTIONS.md Q1-Q3".to_owned(),
                review_by: "2027-09-08".to_owned(),
                trace: "M9E-WPT-03".to_owned(),
            }],
            exit_reason: ExitReason::ReleaseDefects,
        }
    }

    #[test]
    fn json_carries_both_verdicts_and_totals() {
        let json = to_json(&run());
        assert!(json.contains("\"expectations_match\":true"));
        assert!(json.contains("\"release_green\":false"));
        assert!(json.contains("\"defects\":1"));
        assert!(json.contains("\"exit_reason\":\"release_defects\""));
        assert!(json.contains("\"total\":2"));
        assert!(json.contains("\"exclusions\":[{"));
        // Valid JSON: the harness parser accepts it.
        assert!(crate::manifest::parse_json(&json).is_ok());
    }

    #[test]
    fn junit_totals_match_canonical() {
        let run = run();
        let xml = to_junit(&run);
        assert!(xml.contains("tests=\"2\""));
        assert!(xml.contains("failures=\"1\""));
        assert!(xml.contains("skipped=\"1\""));
        assert!(xml.contains("file-level-exclusions"));
    }

    #[test]
    fn json_and_junit_escape_deterministically() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(xml_escape("a<b"), "a&lt;b");
        assert_eq!(
            json_escape("q\"&\u{0}é"),
            "q\\\"&\u{0}é".replace('\u{0}', "\\u0000")
        );
        assert_eq!(xml_escape("q\"&\u{0}é"), "q&quot;&amp;é");
        assert_eq!(
            crate::runner::scrub_detail("see (blob:uuid-1), C:\\a\\b.js and /tmp/x"),
            "see (blob:<redacted>), <redacted-drive-path> and <redacted-abs-path>"
        );
    }
}
