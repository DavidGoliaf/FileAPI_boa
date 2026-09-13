//! M9E-R1 gate remediation integration tests (M9E-R1-01…07).
//!
//! Each test builds a self-contained fixture (manifest, inventory,
//! expectations, corpus, upstream tree) in a temp directory, invokes the
//! real CLI binary and asserts the exit code plus the JSON/JUnit verdict.
//! No network, no shell, no dependency on the pinned checkout.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use boa_fapi_wpt::hash::sha256_hex;
use boa_fapi_wpt::manifest::Json;

const REPOSITORY: &str = "https://github.com/web-platform-tests/wpt";
const COMMIT: &str = "0968c868d8095217d18d86b34c7f21dccae58768";
const REVIEW_BY: &str = "2099-01-01";
const EXCLUDED: &str = "FileAPI/reading-data-section/excluded.any.js";

/// Six direct files covering all mandatory groups.
const FILES: [(&str, &str, &str, &str); 6] = [
    ("a.js", "FileAPI/blob/a.any.js", "FileAPI/blob", "s1"),
    ("b.js", "FileAPI/file/b.any.js", "FileAPI/file", "s2"),
    (
        "c.js",
        "FileAPI/filelist-section/c.any.js",
        "FileAPI/filelist-section",
        "s3",
    ),
    (
        "d.js",
        "FileAPI/reading-data-section/d.any.js",
        "FileAPI/reading-data-section",
        "s4",
    ),
    ("e.js", "FileAPI/root/e.any.js", "FileAPI/root", "s5"),
    ("f.js", "FileAPI/url/f.any.js", "FileAPI/url", "s6"),
];

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Baseline,
    DefectFirst,
    DuplicateResult,
    ExtraResult,
    MissingResult,
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExclusionEdit {
    Present,
    Removed,
    FakePath,
    Duplicate,
    ExecutedPath,
    InvalidCapability,
    ExpiredReview,
}

/// One materialized fixture directory.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn build(name: &str, case: Case, edit: ExclusionEdit) -> Self {
        let root = unique_dir(name);
        std::fs::create_dir_all(root.join("corpus")).unwrap();
        std::fs::create_dir_all(root.join("upstream")).unwrap();
        let mut manifest_files = Vec::new();
        let mut expectation_rows = Vec::new();
        let mut inventory_entries = Vec::new();
        for (index, (corpus, upstream, group, subtest)) in FILES.iter().enumerate() {
            let test = upstream.rsplit('/').next().unwrap();
            let content = match (case, index) {
                (Case::DefectFirst, 0) => {
                    "test(function () { assert_true(false, 'defect'); }, 's1');".to_owned()
                }
                (Case::DuplicateResult, 0) => {
                    "test(function () { assert_true(true); }, 's1');\ntest(function () { assert_true(false, 'contradictory'); }, 's1');"
                        .to_owned()
                }
                (Case::ExtraResult, 0) => {
                    "test(function () { assert_true(true); }, 's1');\ntest(function () { assert_true(true); }, 'EXTRA');"
                        .to_owned()
                }
                (Case::MissingResult, 0) => "// no harness entry recorded".to_owned(),
                _ => format!("test(function () {{ assert_true(true); }}, '{subtest}');"),
            };
            write_file(&root.join("corpus").join(corpus), &content);
            write_file(&root.join("upstream").join(upstream), &content);
            let hash = sha256_hex(content.as_bytes());
            let mut subtests = vec![if case == Case::DefectFirst && index == 0 {
                manifest_subtest(test, subtest, "FAIL", "open defect", "M9E-R1-01")
            } else {
                manifest_subtest(test, subtest, "PASS", "", "")
            }];
            if case == Case::Dynamic && index == 0 {
                subtests.push(manifest_subtest(
                    test,
                    "DYNAMIC: matrix",
                    "NOTRUN",
                    "dynamic title matrix",
                    "QUESTIONS.md Q1-Q3",
                ));
            }
            manifest_files.push(manifest_file(
                corpus, upstream, group, &hash, "direct", &subtests,
            ));
            expectation_rows.push(expectation_row(
                upstream,
                test,
                subtest,
                if case == Case::DefectFirst && index == 0 {
                    "FAIL"
                } else {
                    "PASS"
                },
                if case == Case::DefectFirst && index == 0 {
                    "open defect"
                } else {
                    ""
                },
                if case == Case::DefectFirst && index == 0 {
                    "M9E-R1-01"
                } else {
                    ""
                },
                "direct",
            ));
            if case == Case::Dynamic && index == 0 {
                expectation_rows.push(expectation_row(
                    upstream,
                    test,
                    "DYNAMIC: matrix",
                    "NOTRUN",
                    "dynamic title matrix",
                    "QUESTIONS.md Q1-Q3",
                    "direct",
                ));
            }
            inventory_entries.push(inventory_entry(upstream, &content));
        }
        // One excluded upstream path (never executed).
        let excluded_content = "// excluded inventory path\n";
        write_file(&root.join("upstream").join(EXCLUDED), excluded_content);
        inventory_entries.push(inventory_entry(EXCLUDED, excluded_content));
        match edit {
            ExclusionEdit::Removed => {}
            ExclusionEdit::Duplicate => {
                expectation_rows.push(exclusion_row(EXCLUDED, "navigation", REVIEW_BY));
                expectation_rows.push(exclusion_row(EXCLUDED, "navigation", REVIEW_BY));
            }
            ExclusionEdit::FakePath => {
                expectation_rows.push(exclusion_row(
                    "FileAPI/nope/gone.any.js",
                    "navigation",
                    REVIEW_BY,
                ));
            }
            ExclusionEdit::ExecutedPath => {
                expectation_rows.push(exclusion_row(
                    "FileAPI/blob/a.any.js",
                    "navigation",
                    REVIEW_BY,
                ));
            }
            ExclusionEdit::InvalidCapability => {
                expectation_rows.push(exclusion_row(EXCLUDED, "bogus-capability", REVIEW_BY));
            }
            ExclusionEdit::ExpiredReview => {
                expectation_rows.push(exclusion_row(EXCLUDED, "navigation", "2000-01-01"));
            }
            ExclusionEdit::Present => {
                expectation_rows.push(exclusion_row(EXCLUDED, "navigation", REVIEW_BY));
            }
        }
        write_text(
            &root.join("wpt-manifest.json"),
            &manifest_json(&manifest_files),
        );
        write_text(
            &root.join("expectations.json"),
            &expectations_json(&expectation_rows),
        );
        write_text(
            &root.join("wpt-inventory.json"),
            &inventory_json(&inventory_entries),
        );
        Fixture { root }
    }

    /// Runs the CLI in `mode` (`--strict`/`--check-expectations`/`--smoke`).
    fn run(&self, mode: &str, extra: &[&str]) -> (i32, String) {
        let out = self.root.join("out.json");
        let junit = self.root.join("out.xml");
        let mut command = Command::new(env!("CARGO_BIN_EXE_boa_fapi_wpt"));
        command
            .arg("--manifest")
            .arg(self.root.join("wpt-manifest.json"))
            .arg("--json")
            .arg(&out)
            .arg("--junit")
            .arg(&junit);
        if mode != "--smoke" {
            command
                .arg("--expectations")
                .arg(self.root.join("expectations.json"))
                .arg("--upstream-root")
                .arg(self.root.join("upstream"));
        }
        command.arg(mode);
        for argument in extra {
            command.arg(argument);
        }
        let status = command.status().unwrap();
        let code = status.code().unwrap_or(-1);
        let json = std::fs::read_to_string(&out).unwrap_or_default();
        let junit_text = std::fs::read_to_string(&junit).unwrap_or_default();
        let _ = junit_text;
        (code, json)
    }

    /// Appends a byte to one upstream file to force a hash mismatch.
    fn mutate_upstream(&self) {
        let path = self.root.join("upstream/FileAPI/blob/a.any.js");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"// mutation\n");
        std::fs::write(&path, bytes).unwrap();
    }
}

fn unique_dir(name: &str) -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "boa-fapi-wpt-r1-{name}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn write_text(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

fn manifest_subtest(test: &str, subtest: &str, status: &str, reason: &str, issue: &str) -> String {
    let (capability, classification) = if status == "NOTRUN" {
        ("blob-url", "unsupported-host-capability")
    } else {
        ("blob-constructor", "supported")
    };
    format!(
        "{{\"test\":\"{test}\",\"subtest\":\"{subtest}\",\"status\":\"{status}\",\"reason\":\"{reason}\",\"capability\":\"{capability}\",\"owner\":\"m9e\",\"review_by\":\"{REVIEW_BY}\",\"trace\":\"M9E-WPT-03\",\"classification\":\"{classification}\",\"spec_section\":\"\",\"issue\":\"{issue}\"}}"
    )
}

fn manifest_file(
    corpus: &str,
    upstream: &str,
    group: &str,
    hash: &str,
    provenance: &str,
    subtests: &[String],
) -> String {
    let blob = "0".repeat(40);
    format!(
        "{{\"path\":\"corpus/{corpus}\",\"upstream_path\":\"{upstream}\",\"upstream_blob_sha\":\"{blob}\",\"upstream_sha256\":\"{hash}\",\"sha256\":\"{hash}\",\"group\":\"{group}\",\"capability\":\"blob-constructor\",\"provenance\":\"{provenance}\",\"subtests\":[{}]}}",
        subtests.join(",")
    )
}

fn expectation_row(
    upstream: &str,
    test: &str,
    subtest: &str,
    status: &str,
    reason: &str,
    issue: &str,
    adapter: &str,
) -> String {
    let (capability, classification) = if status == "NOTRUN" {
        ("blob-url", "unsupported-host-capability")
    } else {
        ("blob-constructor", "supported")
    };
    format!(
        "{{\"upstream_path\":\"{upstream}\",\"test\":\"{test}\",\"subtest\":\"{subtest}\",\"status\":\"{status}\",\"classification\":\"{classification}\",\"capability\":\"{capability}\",\"reason\":\"{reason}\",\"owner\":\"m9e\",\"review_by\":\"{REVIEW_BY}\",\"trace\":\"M9E-WPT-03\",\"spec_section\":\"\",\"issue\":\"{issue}\",\"adapter\":\"{adapter}\"}}"
    )
}

fn exclusion_row(path: &str, capability: &str, review_by: &str) -> String {
    let test = path.rsplit('/').next().unwrap();
    format!(
        "{{\"upstream_path\":\"{path}\",\"test\":\"{test}\",\"subtest\":\"file-level exclusion\",\"status\":\"NOTRUN\",\"classification\":\"unsupported-host-capability\",\"capability\":\"{capability}\",\"reason\":\"requires {capability}\",\"owner\":\"m9e\",\"review_by\":\"{review_by}\",\"trace\":\"M9E-WPT-03\",\"spec_section\":\"\",\"issue\":\"QUESTIONS.md Q1-Q3\",\"adapter\":\"\"}}"
    )
}

fn inventory_entry(path: &str, content: &str) -> String {
    let blob = "0".repeat(40);
    let hash = sha256_hex(content.as_bytes());
    format!(
        "{{\"path\":\"{path}\",\"blob_sha\":\"{blob}\",\"size\":{},\"sha256\":\"{hash}\"}}",
        content.len()
    )
}

fn manifest_json(files: &[String]) -> String {
    format!(
        "{{\"schema_version\":2,\"source\":{{\"repository\":\"{REPOSITORY}\",\"commit\":\"{COMMIT}\",\"license\":\"BSD-3-Clause\"}},\"corpus_root\":\"corpus\",\"default_timeout_ms\":60000,\"files\":[{}]}}",
        files.join(",")
    )
}

fn expectations_json(rows: &[String]) -> String {
    format!(
        "{{\"schema_version\":1,\"repository\":\"{REPOSITORY}\",\"commit\":\"{COMMIT}\",\"review_by_default\":\"{REVIEW_BY}\",\"expectations\":[{}]}}",
        rows.join(",")
    )
}

fn inventory_json(entries: &[String]) -> String {
    format!(
        "{{\"schema_version\":1,\"repository\":\"{REPOSITORY}\",\"commit\":\"{COMMIT}\",\"scope\":\"FileAPI/\",\"file_count\":{},\"generated_by\":\"test fixture\",\"inventory_sha256\":\"{}\",\"files\":[{}]}}",
        entries.len(),
        "a".repeat(64),
        entries.join(",")
    )
}

fn json_field<'a>(value: &'a Json, name: &str) -> &'a Json {
    value.field(name).unwrap()
}

fn json_u64(value: &Json, name: &str) -> u64 {
    json_field(value, name).as_u64().unwrap()
}

fn json_bool(value: &Json, name: &str) -> bool {
    match json_field(value, name) {
        Json::Bool(value) => *value,
        other => panic!("expected bool for {name}, got {other:?}"),
    }
}

fn json_str<'a>(value: &'a Json, name: &str) -> &'a str {
    json_field(value, name).as_str().unwrap()
}

fn parse(json: &str) -> Json {
    boa_fapi_wpt::manifest::parse_json(json).unwrap()
}

/// R1-05: the canonical report accounts for every inventory path and every
/// expectation id exactly once; exclusions are visible and in the totals.
#[test]
fn r1_05_complete_accounting_and_reporting() {
    let fixture = Fixture::build("r1-05", Case::Baseline, ExclusionEdit::Present);
    let (code, json) = fixture.run("--strict", &[]);
    assert_eq!(code, 0, "baseline must be release green: {json}");
    let root = parse(&json);
    assert_eq!(json_str(&root, "mode"), "WPT_STRICT");
    assert!(json_bool(&root, "expectations_match"));
    assert!(json_bool(&root, "release_green"));
    assert!(!json.contains("strict_pass"), "legacy strict_pass removed");
    let inventory = json_field(&root, "inventory");
    assert_eq!(json_u64(inventory, "total"), 7);
    assert_eq!(json_u64(inventory, "executed_direct"), 6);
    assert_eq!(json_u64(inventory, "executed_adapted"), 0);
    assert_eq!(json_u64(inventory, "excluded"), 1);
    assert_eq!(json_u64(inventory, "unaccounted"), 0);
    let results = json_field(&root, "results");
    assert_eq!(json_u64(results, "total"), 7);
    assert_eq!(json_u64(results, "unique"), 7);
    assert_eq!(json_u64(results, "upstream_pass"), 6);
    assert_eq!(json_u64(results, "smoke_pass"), 0);
    assert_eq!(json_u64(results, "defects"), 0);
    assert_eq!(json_u64(results, "notrun"), 1);
    assert_eq!(json_u64(results, "unexpected"), 0);
    let exclusions = json_field(&root, "exclusions").as_arr().unwrap();
    assert_eq!(exclusions.len(), 1);
    assert_eq!(json_str(&exclusions[0], "path"), EXCLUDED);
    assert_eq!(json_str(&exclusions[0], "capability"), "navigation");
    // JUnit totals match the canonical results.
    let junit = std::fs::read_to_string(fixture.root.join("out.xml")).unwrap();
    assert!(junit.contains("tests=\"7\""));
    assert!(junit.contains("failures=\"0\""));
    assert!(junit.contains("skipped=\"1\""));
    assert!(junit.contains("file-level-exclusions"));
}

/// R1-01: one expected FAIL matches (`expectations_match`), but `--strict`
/// is red; `--check-expectations` is diagnostically successful.
#[test]
fn r1_01_expected_fail_blocks_release() {
    let fixture = Fixture::build("r1-01", Case::DefectFirst, ExclusionEdit::Present);
    let (check_code, check_json) = fixture.run("--check-expectations", &[]);
    assert_eq!(
        check_code, 0,
        "check-expectations must exit 0: {check_json}"
    );
    let check = parse(&check_json);
    assert_eq!(json_str(&check, "mode"), "WPT_CHECK_EXPECTATIONS");
    assert!(json_bool(&check, "expectations_match"));
    assert!(!json_bool(&check, "release_green"));
    assert_eq!(json_str(&check, "exit_reason"), "release_defects");
    let (strict_code, strict_json) = fixture.run("--strict", &[]);
    assert_ne!(strict_code, 0, "strict must be red with a defect");
    let strict = parse(&strict_json);
    assert!(json_bool(&strict, "expectations_match"));
    assert!(!json_bool(&strict, "release_green"));
    assert_eq!(
        json_u64(json_field(&strict, "release_blockers"), "defects"),
        1
    );
    assert_eq!(json_str(&strict, "exit_reason"), "release_defects");
}

/// R1-02: duplicate, extra and missing result rows are drift and non-zero.
#[test]
fn r1_02_result_identity_drift_is_non_zero() {
    for (name, case) in [
        ("dup", Case::DuplicateResult),
        ("extra", Case::ExtraResult),
        ("missing", Case::MissingResult),
    ] {
        let fixture = Fixture::build(&format!("r1-02-{name}"), case, ExclusionEdit::Present);
        let (code, json) = fixture.run("--strict", &[]);
        assert_ne!(code, 0, "{name} must be non-zero: {json}");
        let root = parse(&json);
        assert!(!json_bool(&root, "expectations_match"));
        assert_eq!(json_str(&root, "exit_reason"), "expectation_drift");
        // A duplicate result never inflates the ordinary totals as if it
        // were a second independent test.
        if case == Case::DuplicateResult {
            assert_eq!(json_u64(json_field(&root, "results"), "unique"), 7);
        }
    }
}

/// R1-03: the DYNAMIC tracker id already in the manifest is not added a
/// second time; threads 1/2 produce byte-identical JSON.
#[test]
fn r1_03_dynamic_is_not_double_counted() {
    let fixture = Fixture::build("r1-03", Case::Dynamic, ExclusionEdit::Present);
    let (code, json) = fixture.run("--strict", &[]);
    assert_eq!(code, 0, "dynamic-only baseline is release green: {json}");
    let root = parse(&json);
    let results = json_field(&root, "results");
    assert_eq!(json_u64(results, "total"), 8);
    assert_eq!(json_u64(results, "unique"), 8);
    assert_eq!(json.matches("\"DYNAMIC: matrix\"").count(), 1);
    let (_, t2) = fixture.run("--strict", &["--threads", "2"]);
    assert_eq!(json, t2, "threads 1/2 JSON must be byte-identical");
}

/// R1-04: inventory/exclusion bijection mutations are all non-zero.
#[test]
fn r1_04_bijection_mutations_are_non_zero() {
    for edit in [
        ExclusionEdit::Removed,
        ExclusionEdit::FakePath,
        ExclusionEdit::Duplicate,
        ExclusionEdit::ExecutedPath,
        ExclusionEdit::InvalidCapability,
        ExclusionEdit::ExpiredReview,
    ] {
        let fixture = Fixture::build(&format!("r1-04-{edit:?}"), Case::Baseline, edit);
        let (code, json) = fixture.run("--strict", &[]);
        assert_ne!(code, 0, "{edit:?} must be non-zero: {json}");
        let root = parse(&json);
        let reason = json_str(&root, "exit_reason");
        assert!(
            reason == "integrity" || reason == "expectation_drift",
            "{edit:?} unexpected reason {reason}"
        );
    }
}

/// R1-06: exit 0 is impossible while the run is not green, and a green run
/// has zero blockers.
#[test]
fn r1_06_exit_and_verdict_are_consistent() {
    for (name, case) in [("green", Case::Baseline), ("red", Case::DefectFirst)] {
        let fixture = Fixture::build(&format!("r1-06-{name}"), case, ExclusionEdit::Present);
        let (code, json) = fixture.run("--strict", &[]);
        let root = parse(&json);
        let green = json_bool(&root, "release_green");
        if green {
            assert_eq!(code, 0, "{name}: green must exit 0");
            assert_eq!(
                json_u64(json_field(&root, "release_blockers"), "defects"),
                0
            );
        } else {
            assert_ne!(code, 0, "{name}: red must exit non-zero");
        }
    }
}

/// R1-07: mutated upstream bytes are an integrity failure, not a silent
/// NOTRUN.
#[test]
fn r1_07_mutated_upstream_is_integrity_failure() {
    let fixture = Fixture::build("r1-07", Case::Baseline, ExclusionEdit::Present);
    fixture.mutate_upstream();
    let (code, json) = fixture.run("--strict", &[]);
    assert_ne!(code, 0, "mutated upstream must be non-zero: {json}");
    let root = parse(&json);
    assert_eq!(json_str(&root, "exit_reason"), "integrity");
    assert!(!json_bool(&root, "release_green"));
}
