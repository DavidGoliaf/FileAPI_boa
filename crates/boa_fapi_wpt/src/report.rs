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
///
/// `NOTRUN` rows (actual + expected) serialize as `<skipped>` without a
/// `<failure>` and without raising the suite `failures` count;
/// `FAIL`/`TIMEOUT` rows serialize as `<failure>`; unexpected rows always
/// break strict via [`strict_pass`].
#[must_use]
pub fn to_junit(manifest: &Manifest, files: &[FileResult]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>");
    for file in files {
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
        out.push_str(&format!(
            "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\">",
            xml_escape(&file.path),
            file.subtests.len(),
            failures,
            skipped
        ));
        for sub in &file.subtests {
            out.push_str(&format!(
                "<testcase classname=\"{}\" name=\"{}\">",
                xml_escape(&sub.test),
                xml_escape(&sub.subtest)
            ));
            match sub.actual {
                ActualStatus::Pass => {}
                ActualStatus::NotRun => {
                    out.push_str(&format!(
                        "<skipped message=\"{}\"/>",
                        xml_escape(&sub.detail)
                    ));
                }
                ActualStatus::Fail | ActualStatus::Timeout => {
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

/// Strict gate: enum-to-enum comparison, never detail-prefix matching.
///
/// - `PASS` expects only actual `PASS`;
/// - `NOTRUN` expects only actual `NOTRUN`;
/// - `FAIL`/`TIMEOUT` always break strict (no manifest status can expect
///   them — the loader only accepts `PASS`/`NOTRUN`).
#[must_use]
pub fn strict_pass(files: &[FileResult]) -> bool {
    files.iter().flat_map(|f| &f.subtests).all(|s| {
        (s.expected.token() == "PASS" && s.actual == ActualStatus::Pass)
            || (s.expected.token() == "NOTRUN" && s.actual == ActualStatus::NotRun)
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
        // A NOTRUN gap reported as actual FAIL (old mapping) breaks strict:
        // only actual NOTRUN satisfies expected NOTRUN.
        let gap_fail = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(
                ActualStatus::Fail,
                ExpectedStatus::NotRun,
                "notrun: needs X",
            )],
        }];
        assert!(!strict_pass(&gap_fail));
        let gap = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(
                ActualStatus::NotRun,
                ExpectedStatus::NotRun,
                "notrun: needs X",
            )],
        }];
        assert!(strict_pass(&gap));
        let timeout = vec![FileResult {
            path: "p".to_owned(),
            upstream_path: "u".to_owned(),
            group: "g".to_owned(),
            subtests: vec![row(ActualStatus::Timeout, ExpectedStatus::Pass, "t")],
        }];
        assert!(!strict_pass(&timeout));
    }

    #[test]
    fn json_and_junit_escape_deterministically() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(xml_escape("a<b"), "a&lt;b");
    }

    #[test]
    fn serializer_checks_quote_amp_nul_unicode_blob_and_paths() {
        // Quote/ampersand, NUL (dropped in XML), Unicode passthrough.
        assert_eq!(
            json_escape("q\"&\u{0}é"),
            "q\\\"&\u{0}é".replace('\u{0}', "\\u0000")
        );
        assert_eq!(xml_escape("q\"&\u{0}é"), "q&quot;&amp;é");
        // `blob:` inside a token and OS paths are scrubbed before reports.
        assert_eq!(
            crate::runner::scrub_detail("see (blob:uuid-1), C:\\a\\b.js and /tmp/x"),
            "see (blob:<redacted>), <redacted-drive-path> and <redacted-abs-path>"
        );
        // JSON stays valid: escaped detail round-trips through the parser.
        let escaped = json_escape("a\"b\\c");
        let wrapped = format!("{{\"d\": \"{escaped}\"}}");
        assert!(crate::manifest::parse_json(&wrapped).is_ok());
    }
}
