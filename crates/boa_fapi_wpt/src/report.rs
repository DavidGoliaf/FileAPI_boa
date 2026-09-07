//! Deterministic JSON/JUnit report serialization.
//!
//! Reports are stable by construction: files and subtests serialize in
//! manifest order, object keys are emitted in fixed order, and the
//! comparison-relevant section carries no timestamps, no random IDs and
//! no absolute paths. `elapsed_ms` is informational per subtest (never
//! compared by strict mode).

use std::fmt::Write;

use crate::manifest::Manifest;
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

/// Escapes a string for XML attribute/text output.
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
            c => out.push(c),
        }
    }
    out
}

/// Serializes the strict run report as deterministic JSON.
#[must_use]
pub fn to_json(manifest: &Manifest, files: &[FileResult], strict_pass: bool) -> String {
    let mut out = String::new();
    out.push_str("{\"schema_version\":1,");
    out.push_str(&format!(
        "\"source\":{{\"repository\":\"{}\",\"commit\":\"{}\",\"license\":\"{}\"}},",
        json_escape(&manifest.source.repository),
        json_escape(&manifest.source.commit),
        json_escape(&manifest.source.license)
    ));
    out.push_str(&format!("\"strict_pass\":{strict_pass},\"files\":["));
    for (fi, file) in files.iter().enumerate() {
        if fi > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"upstream_path\":\"{}\",\"group\":\"{}\",\"subtests\":[",
            json_escape(&file.path),
            json_escape(&file.upstream_path),
            json_escape(&file.group)
        ));
        for (si, sub) in file.subtests.iter().enumerate() {
            if si > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"test\":\"{}\",\"subtest\":\"{}\",\"actual\":\"{}\",\"expected\":\"{}\",\"trace\":\"{}\",\"detail\":\"{}\"}}",
                json_escape(&sub.test),
                json_escape(&sub.subtest),
                sub.actual.token(),
                sub.expected.token(),
                json_escape(&sub.trace),
                json_escape(&sub.detail)
            ));
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

/// Serializes the strict run report as deterministic JUnit XML.
#[must_use]
pub fn to_junit(manifest: &Manifest, files: &[FileResult]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>");
    for file in files {
        let failures = file
            .subtests
            .iter()
            .filter(|s| s.actual != ActualStatus::Pass || s.expected.token() == "NOTRUN")
            .count();
        out.push_str(&format!(
            "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\">",
            xml_escape(&file.path),
            file.subtests.len(),
            failures
        ));
        for sub in &file.subtests {
            out.push_str(&format!(
                "<testcase classname=\"{}\" name=\"{}\">",
                xml_escape(&sub.test),
                xml_escape(&sub.subtest)
            ));
            match sub.actual {
                ActualStatus::Pass if sub.expected.token() == "PASS" => {}
                _ => {
                    out.push_str(&format!(
                        "<failure message=\"actual={} expected={}\">{}</failure>",
                        sub.actual.token(),
                        sub.expected.token(),
                        xml_escape(&sub.detail)
                    ));
                }
            }
            out.push_str("</testcase>");
        }
        out.push_str("</testsuite>");
    }
    out.push_str("</testsuites>");
    let _ = &manifest.source.commit;
    out
}

/// Strict gate: `true` only when every subtest matches its expectation.
///
/// `PASS`-expected rows must be actually `PASS`; `NOTRUN`-expected rows
/// are satisfied by the harness `NOTRUN` report (recorded as expected
/// with the gap reason — any `FAIL`/`TIMEOUT` there breaks strict).
/// Unexpected extra rows always break strict.
#[must_use]
pub fn strict_pass(files: &[FileResult]) -> bool {
    files.iter().flat_map(|f| &f.subtests).all(|s| {
        (s.expected.token() == "PASS" && s.actual == ActualStatus::Pass)
            || (s.expected.token() == "NOTRUN" && s.detail.starts_with("notrun: "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExpectedStatus;
    use crate::runner::SubtestResult;

    fn row(actual: ActualStatus, expected: ExpectedStatus, detail: &str) -> SubtestResult {
        SubtestResult {
            test: "t".to_owned(),
            subtest: "s".to_owned(),
            actual,
            expected,
            detail: detail.to_owned(),
            trace: "M7-WPT-04".to_owned(),
            elapsed_ms: 0,
        }
    }

    #[test]
    fn strict_gate_needs_exact_match() {
        let pass = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(ActualStatus::Pass, ExpectedStatus::Pass, "")],
        }];
        assert!(strict_pass(&pass));
        let fail = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(ActualStatus::Fail, ExpectedStatus::Pass, "x")],
        }];
        assert!(!strict_pass(&fail));
        let gap = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(
                ActualStatus::Fail,
                ExpectedStatus::NotRun,
                "notrun: needs X",
            )],
        }];
        assert!(strict_pass(&gap));
    }

    #[test]
    fn json_and_junit_escape_deterministically() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(xml_escape("a<b"), "a&lt;b");
    }
}
