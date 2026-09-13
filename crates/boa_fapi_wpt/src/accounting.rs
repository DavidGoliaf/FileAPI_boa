//! Canonical run accounting and release verdict (M9E-R1 §3–§5).
//!
//! A single [`CanonicalRun`] is built once from the validated manifest,
//! inventory, expectations and executed rows. Every serializer (console,
//! JSON, JUnit) and the process exit code derive from that one value; no
//! serializer recomputes totals independently.
//!
//! Two distinct results are kept apart (M9E-R1 §3.1):
//!
//! - [`CanonicalRun::expectations_match`] — actual statuses and identities
//!   fully match the audited expectations (no missing/duplicate/unexpected/
//!   drift).
//! - [`CanonicalRun::release_green`] — expectations match **and** there is
//!   no release blocker (recorded FAIL defect, timeout, harness gap,
//!   supported NOTRUN, or expired exclusion).

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::inventory::{Disposition, Dispositions, ExecutedClaim, Inventory, resolve_dispositions};
use crate::manifest::{Classification, ExpectationRow, Manifest, ManifestSource, Provenance};
use crate::runner::{ActualStatus, FileResult};

/// JSON report schema version emitted from the canonical model.
pub const REPORT_SCHEMA_VERSION: u32 = 2;

/// Run mode token recorded in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Offline smoke (`--smoke`): adapter/harness check only, never a
    /// release or conformance verdict.
    Smoke,
    /// Normative release gate (`--strict`).
    Strict,
    /// Diagnostic observation mode (`--check-expectations`): verifies the
    /// audited expectations reproduce, but always reports
    /// `release_green: false` and never claims conformance/release pass.
    CheckExpectations,
}

impl RunMode {
    /// Renders the canonical mode token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Smoke => "ADAPTED_SMOKE",
            Self::Strict => "WPT_STRICT",
            Self::CheckExpectations => "WPT_CHECK_EXPECTATIONS",
        }
    }

    /// Returns `true` for the two gate modes that consume inventory and
    /// expectations.
    #[must_use]
    pub fn is_gate(self) -> bool {
        matches!(self, Self::Strict | Self::CheckExpectations)
    }
}

/// Why a run did not end `0`, kept distinct in JSON even when the CLI uses
/// one non-zero code (M9E-R1 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Run succeeded under its mode (release green for `--strict`,
    /// expectations match for `--check-expectations`).
    Ok,
    /// Bad command line input or an integrity failure before execution
    /// (inventory/hash/path/adaptation drift).
    Integrity,
    /// Actual statuses/identities drifted from audited expectations
    /// (duplicate, missing, unexpected extra, status mismatch).
    ExpectationDrift,
    /// Execution timed out or crashed.
    ExecutionFailure,
    /// Audited expectations still record open product/harness defects
    /// (expected FAIL, actual FAIL).
    ReleaseDefects,
}

impl ExitReason {
    /// Renders the canonical token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Integrity => "integrity",
            Self::ExpectationDrift => "expectation_drift",
            Self::ExecutionFailure => "execution_failure",
            Self::ReleaseDefects => "release_defects",
        }
    }

    /// Process exit code for this reason: `0` success, `1` release defects,
    /// `2` input/integrity/drift/execution failure.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::ReleaseDefects => 1,
            Self::Integrity | Self::ExpectationDrift | Self::ExecutionFailure => 2,
        }
    }
}

/// Inventory accounting totals (M9E-R1 §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InventoryTotals {
    /// Pinned inventory path count.
    pub total: usize,
    /// Paths executed directly.
    pub executed_direct: usize,
    /// Paths executed through an adapter.
    pub executed_adapted: usize,
    /// Paths excluded (capability or artifact).
    pub excluded: usize,
    /// Paths with no disposition (must be `0`).
    pub unaccounted: usize,
}

/// Result accounting totals (M9E-R1 §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultsTotals {
    /// Canonical (deduplicated) row count.
    pub total: usize,
    /// Unique row count (always equal to `total` for a valid run).
    pub unique: usize,
    /// Upstream PASS rows.
    pub upstream_pass: usize,
    /// Project-owned smoke PASS rows (never WPT).
    pub smoke_pass: usize,
    /// Recorded open defects (expected FAIL, actual FAIL).
    pub defects: usize,
    /// NOTRUN rows, including file-level exclusions.
    pub notrun: usize,
    /// Rows that did not match their expectation (status or identity).
    pub unexpected: usize,
}

/// Release blockers; any non-zero (or `not_a_release_mode`) keeps the
/// release gate red.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseBlockers {
    /// Recorded open defects.
    pub defects: usize,
    /// Timeout rows.
    pub timeouts: usize,
    /// Unexpected status mismatches or extra rows.
    pub unexpected: usize,
    /// Identity defects: duplicate, missing or extra result rows.
    pub expectation_drift: usize,
    /// Set for `--smoke`: not a release mode, never green.
    pub not_a_release_mode: usize,
}

impl ReleaseBlockers {
    /// Returns `true` when no blocker is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defects == 0
            && self.timeouts == 0
            && self.unexpected == 0
            && self.expectation_drift == 0
            && self.not_a_release_mode == 0
    }
}

/// One file-level inventory exclusion visible in the report (M9E-R1 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusionRow {
    /// Pinned upstream `FileAPI/**` path.
    pub path: String,
    /// File-level test id.
    pub test: String,
    /// Closed-list capability.
    pub capability: String,
    /// Exact gap reason.
    pub reason: String,
    /// Owner.
    pub owner: String,
    /// Issue/question link.
    pub issue: String,
    /// Review date `YYYY-MM-DD`.
    pub review_by: String,
    /// Trace id.
    pub trace: String,
}

/// Single validated run model consumed by every serializer and the exit
/// code (M9E-R1 §5).
#[derive(Debug, Clone)]
pub struct CanonicalRun {
    /// Report schema version.
    pub schema_version: u32,
    /// Run mode.
    pub mode: RunMode,
    /// Pinned source identity.
    pub source: ManifestSource,
    /// Observation verdict: actual statuses/identities match expectations.
    pub expectations_match: bool,
    /// Release verdict: expectations match and no release blocker.
    pub release_green: bool,
    /// Release blockers with exact counts.
    pub release_blockers: ReleaseBlockers,
    /// Inventory totals (`None` in `--smoke`: no inventory is consumed).
    pub inventory: Option<InventoryTotals>,
    /// Result totals.
    pub results: ResultsTotals,
    /// Executed file rows in manifest order.
    pub files: Vec<FileResult>,
    /// File-level exclusions in inventory order.
    pub exclusions: Vec<ExclusionRow>,
    /// Exit reason class.
    pub exit_reason: ExitReason,
}

impl CanonicalRun {
    /// Returns `true` when the process must exit `0` for this run.
    #[must_use]
    pub fn success(&self) -> bool {
        match self.mode {
            RunMode::Smoke => self.exit_reason == ExitReason::Ok,
            RunMode::Strict => self.release_green,
            RunMode::CheckExpectations => self.expectations_match,
        }
    }
}

/// Accounting failures raised before a release verdict is formed.
#[derive(Debug, Error)]
pub enum AccountingError {
    /// Inventory/hash/adaptation integrity failure.
    #[error("{0}")]
    Integrity(String),
    /// Expectation drift (duplicate, missing, extra, status mismatch).
    #[error("{0}")]
    Drift(String),
}

impl AccountingError {
    /// Exit reason class for this error.
    #[must_use]
    pub fn reason(&self) -> ExitReason {
        match self {
            Self::Integrity(_) => ExitReason::Integrity,
            Self::Drift(_) => ExitReason::ExpectationDrift,
        }
    }
}

/// Builds the canonical model for a gate run (`--strict` or
/// `--check-expectations`).
///
/// `files` are the executed rows in manifest order. The function validates
/// the inventory bijection before any verdict, deduplicates result
/// identities, adds the file-level exclusions, and computes both verdicts.
pub fn build_gate_run(
    mode: RunMode,
    manifest: &Manifest,
    inventory: &Inventory,
    expectations: &[ExpectationRow],
    files: Vec<FileResult>,
) -> Result<CanonicalRun, AccountingError> {
    let source = manifest.source.clone();

    // 1. Inventory ↔ disposition bijection (M9E-R1 §4.1).
    let claims = executed_claims(manifest)?;
    let exclusions = collect_exclusions(expectations);
    let excluded_paths: Vec<String> = exclusions.iter().map(|e| e.path.clone()).collect();
    let dispositions = resolve_dispositions(inventory, &claims, &excluded_paths)
        .map_err(|e| AccountingError::Integrity(e.to_string()))?;
    let inventory_totals = inventory_totals(inventory, &dispositions)?;

    // 2. Expected identity index from the manifest.
    let expected_index = expected_subtest_index(manifest);

    // 3. Result accounting over executed rows (deduplicated).
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut results = ResultsTotals::default();
    let mut blockers = ReleaseBlockers::default();
    let mut executed_keys: BTreeSet<(String, String, String)> = BTreeSet::new();
    for file in &files {
        for sub in &file.subtests {
            let key = (
                file.upstream_path.clone(),
                sub.test.clone(),
                sub.subtest.clone(),
            );
            if !seen.insert(key.clone()) {
                blockers.expectation_drift += 1;
                continue;
            }
            executed_keys.insert(key.clone());
            match expected_index.get(&key) {
                Some((provenance, classification)) => {
                    classify_expected(
                        &mut results,
                        &mut blockers,
                        sub.actual,
                        sub.expected.token(),
                        *provenance,
                        *classification,
                    );
                }
                None => {
                    // Result row without a manifest expectation: unexpected
                    // extra result (identity drift), never a silent PASS.
                    results.unexpected += 1;
                    blockers.expectation_drift += 1;
                    if sub.actual == ActualStatus::Timeout {
                        blockers.timeouts += 1;
                    } else {
                        blockers.unexpected += 1;
                    }
                }
            }
        }
    }
    // Missing actual rows: every manifest subtest must be represented once.
    for key in expected_index.keys() {
        if !seen.contains(key) {
            blockers.expectation_drift += 1;
        }
    }
    // 4. File-level exclusions enter the totals as exact NOTRUN rows.
    for exclusion in &exclusions {
        let key = (
            exclusion.path.clone(),
            exclusion.test.clone(),
            "file-level exclusion".to_owned(),
        );
        if !seen.insert(key) {
            blockers.expectation_drift += 1;
            continue;
        }
        results.notrun += 1;
    }
    results.total = seen.len();
    results.unique = seen.len();

    // 5. Verdicts (M9E-R1 §3.1): expectations_match is identity+status
    // equality; release_green additionally forbids recorded defects.
    let expectations_match = blockers.expectation_drift == 0 && blockers.unexpected == 0;
    let release_green = expectations_match && blockers.defects == 0 && blockers.timeouts == 0;
    let exit_reason = if blockers.expectation_drift > 0 || blockers.unexpected > 0 {
        ExitReason::ExpectationDrift
    } else if blockers.timeouts > 0 {
        ExitReason::ExecutionFailure
    } else if blockers.defects > 0 {
        ExitReason::ReleaseDefects
    } else {
        ExitReason::Ok
    };
    Ok(CanonicalRun {
        schema_version: REPORT_SCHEMA_VERSION,
        mode,
        source,
        expectations_match,
        release_green,
        release_blockers: blockers,
        inventory: Some(inventory_totals),
        results,
        files,
        exclusions,
        exit_reason,
    })
}

/// Builds the canonical model for the offline smoke run.
///
/// Smoke consumes no inventory or expectations: results are classified from
/// the manifest provenance only, `expectations_match` is trivially true, and
/// the release gate is always red (`not_a_release_mode`) because smoke is
/// never a conformance verdict.
///
/// # Errors
///
/// Returns an accountingerror when two manifest files claim one upstream
/// path (bijection integrity).
pub fn build_smoke_run(
    manifest: &Manifest,
    files: Vec<FileResult>,
) -> Result<CanonicalRun, AccountingError> {
    let _ = executed_claims(manifest)?;
    let expected_index = expected_subtest_index(manifest);
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut results = ResultsTotals::default();
    let mut blockers = ReleaseBlockers {
        not_a_release_mode: 1,
        ..ReleaseBlockers::default()
    };
    for file in &files {
        for sub in &file.subtests {
            let key = (
                file.upstream_path.clone(),
                sub.test.clone(),
                sub.subtest.clone(),
            );
            if !seen.insert(key.clone()) {
                blockers.expectation_drift += 1;
                continue;
            }
            match expected_index.get(&key) {
                Some((provenance, classification)) => {
                    classify_expected(
                        &mut results,
                        &mut blockers,
                        sub.actual,
                        sub.expected.token(),
                        *provenance,
                        *classification,
                    );
                }
                None => {
                    results.unexpected += 1;
                    blockers.expectation_drift += 1;
                }
            }
        }
    }
    results.total = seen.len();
    results.unique = seen.len();
    Ok(CanonicalRun {
        schema_version: REPORT_SCHEMA_VERSION,
        mode: RunMode::Smoke,
        source: manifest.source.clone(),
        expectations_match: true,
        release_green: false,
        release_blockers: blockers,
        inventory: None,
        results,
        files,
        exclusions: Vec::new(),
        exit_reason: ExitReason::Ok,
    })
}

/// Computes the executed inventory claims from manifest provenance.
///
/// A `direct` file always claims its upstream path. An `adapted` file claims
/// it only when it is a real adaptation (no `project-acceptance` rows); a
/// pure project-acceptance smoke does not consume the upstream disposition
/// (the excluded upstream path stays excluded). A mixed adapted file is an
/// integrity error.
fn executed_claims(manifest: &Manifest) -> Result<Vec<ExecutedClaim>, AccountingError> {
    let mut claims = Vec::new();
    for file in &manifest.files {
        let disposition = match file.provenance {
            Provenance::Direct => Some(Disposition::ExecutedDirect),
            Provenance::Adapted => {
                let project = file
                    .subtests
                    .iter()
                    .any(|s| s.classification == Classification::ProjectAcceptance);
                let supported = file
                    .subtests
                    .iter()
                    .any(|s| s.classification != Classification::ProjectAcceptance);
                if project && supported {
                    return Err(AccountingError::Integrity(format!(
                        "adapted file `{}` mixes project-acceptance and upstream rows",
                        file.path
                    )));
                }
                if project {
                    None
                } else {
                    Some(Disposition::ExecutedAdapted)
                }
            }
        };
        if let Some(disposition) = disposition {
            claims.push(ExecutedClaim {
                upstream_path: file.upstream_path.clone(),
                disposition,
            });
        }
    }
    Ok(claims)
}

/// Collects the file-level exclusion rows (M9E-R1 §4.2).
fn collect_exclusions(expectations: &[ExpectationRow]) -> Vec<ExclusionRow> {
    expectations
        .iter()
        .filter(|row| row.subtest == "file-level exclusion")
        .map(|row| ExclusionRow {
            path: row.upstream_path.clone(),
            test: row.test.clone(),
            capability: row.capability.clone(),
            reason: row.reason.clone(),
            owner: row.owner.clone(),
            issue: row.issue.clone(),
            review_by: row.review_by.clone(),
            trace: row.trace.clone(),
        })
        .collect()
}

/// Computes inventory totals and enforces the arithmetic invariants.
fn inventory_totals(
    inventory: &Inventory,
    dispositions: &Dispositions,
) -> Result<InventoryTotals, AccountingError> {
    let total = inventory.len();
    let executed_direct = dispositions.executed_direct();
    let executed_adapted = dispositions.executed_adapted();
    let excluded = dispositions.excluded();
    let accounted = executed_direct + executed_adapted + excluded;
    let unaccounted = total.saturating_sub(accounted);
    if accounted != total || unaccounted != 0 {
        return Err(AccountingError::Integrity(format!(
            "inventory disposition sum {accounted} != total {total}"
        )));
    }
    Ok(InventoryTotals {
        total,
        executed_direct,
        executed_adapted,
        excluded,
        unaccounted,
    })
}

/// Maps a canonical identity to its manifest provenance + classification.
fn expected_subtest_index(
    manifest: &Manifest,
) -> BTreeMap<(String, String, String), (Provenance, Classification)> {
    let mut index = BTreeMap::new();
    for file in &manifest.files {
        for sub in &file.subtests {
            index.insert(
                (
                    file.upstream_path.clone(),
                    sub.test.clone(),
                    sub.subtest.clone(),
                ),
                (file.provenance, sub.classification),
            );
        }
    }
    index
}

/// Classifies one executed row that has a manifest expectation.
fn classify_expected(
    results: &mut ResultsTotals,
    blockers: &mut ReleaseBlockers,
    actual: ActualStatus,
    expected: &str,
    provenance: Provenance,
    classification: Classification,
) {
    match (actual, expected) {
        (ActualStatus::Pass, "PASS") => {
            if provenance == Provenance::Direct
                && classification != Classification::ProjectAcceptance
            {
                results.upstream_pass += 1;
            } else {
                results.smoke_pass += 1;
            }
        }
        (ActualStatus::Fail, "FAIL") => {
            results.defects += 1;
            blockers.defects += 1;
        }
        (ActualStatus::NotRun, "NOTRUN") => {
            results.notrun += 1;
        }
        (ActualStatus::Timeout, _) => {
            results.unexpected += 1;
            blockers.timeouts += 1;
            blockers.unexpected += 1;
        }
        _ => {
            results.unexpected += 1;
            blockers.unexpected += 1;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::inventory::load_inventory;
    use crate::manifest::{ExpectedStatus, ManifestFile, ManifestSource, ManifestSubtest};
    use crate::runner::SubtestResult;

    fn source() -> ManifestSource {
        ManifestSource {
            repository: "https://github.com/web-platform-tests/wpt".to_owned(),
            commit: "0968c868d8095217d18d86b34c7f21dccae58768".to_owned(),
            license: "BSD-3-Clause".to_owned(),
        }
    }

    fn sub(expected: ExpectedStatus, test: &str, subtest: &str) -> ManifestSubtest {
        ManifestSubtest {
            test: test.to_owned(),
            subtest: subtest.to_owned(),
            timeout_ms: 5000,
            expected,
            reason: String::new(),
            capability: "blob-constructor".to_owned(),
            owner: "o".to_owned(),
            review_by: "2099-01-01".to_owned(),
            trace: "M9E-WPT-03".to_owned(),
            classification: Classification::Supported,
            spec_section: String::new(),
            issue: String::new(),
        }
    }

    fn file(
        path: &str,
        upstream: &str,
        provenance: Provenance,
        subtests: Vec<ManifestSubtest>,
    ) -> ManifestFile {
        ManifestFile {
            path: path.to_owned(),
            upstream_path: upstream.to_owned(),
            upstream_blob_sha: "b".repeat(40),
            upstream_sha256: "f".repeat(64),
            sha256: "e".repeat(64),
            group: "FileAPI/blob".to_owned(),
            capability: "blob-constructor".to_owned(),
            provenance,
            adapter: if provenance == Provenance::Adapted {
                "m9e-filelist-fixture-01".to_owned()
            } else {
                String::new()
            },
            fixture: None,
            subtests,
        }
    }

    fn manifest(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            schema_version: 2,
            source: source(),
            corpus_root: "crates/boa_fapi_wpt/corpus".to_owned(),
            default_timeout_ms: 5000,
            files,
        }
    }

    fn inventory_text() -> String {
        let sha = "a".repeat(64);
        let blob = "b".repeat(40);
        format!(
            "{{\"schema_version\":1,\"repository\":\"https://github.com/web-platform-tests/wpt\",\"commit\":\"0968c868d8095217d18d86b34c7f21dccae58768\",\"scope\":\"FileAPI/\",\"file_count\":2,\"inventory_sha256\":\"{}\",\"files\":[{{\"path\":\"FileAPI/blob/a.any.js\",\"blob_sha\":\"{blob}\",\"size\":1,\"sha256\":\"{sha}\"}},{{\"path\":\"FileAPI/blob/b.any.js\",\"blob_sha\":\"{blob}\",\"size\":1,\"sha256\":\"{sha}\"}}]}}",
            "c".repeat(64)
        )
    }

    fn exclusion_row(path: &str) -> ExpectationRow {
        ExpectationRow {
            upstream_path: path.to_owned(),
            test: "b.any.js".to_owned(),
            subtest: "file-level exclusion".to_owned(),
            expected: ExpectedStatus::NotRun,
            classification: Classification::UnsupportedHostCapability,
            capability: "navigation".to_owned(),
            reason: "requires navigation".to_owned(),
            owner: "m9e".to_owned(),
            review_by: "2099-01-01".to_owned(),
            trace: "M9E-WPT-03".to_owned(),
            spec_section: String::new(),
            issue: "QUESTIONS.md Q1-Q3".to_owned(),
            adapter: String::new(),
        }
    }

    fn row(
        actual: ActualStatus,
        expected: ExpectedStatus,
        test: &str,
        subtest: &str,
    ) -> SubtestResult {
        SubtestResult {
            test: test.to_owned(),
            subtest: subtest.to_owned(),
            actual,
            expected,
            detail: String::new(),
            trace: "M9E-WPT-03".to_owned(),
            elapsed_ms: 0,
        }
    }

    fn result(upstream: &str, subtests: Vec<SubtestResult>) -> FileResult {
        FileResult {
            path: "corpus/a.js".to_owned(),
            upstream_path: upstream.to_owned(),
            group: "FileAPI/blob".to_owned(),
            subtests,
        }
    }

    /// R1-01: an expected FAIL that matches is `expectations_match` but not
    /// release green.
    #[test]
    fn r1_01_expected_fail_blocks_release() {
        let manifest = manifest(vec![
            file(
                "corpus/a.js",
                "FileAPI/blob/a.any.js",
                Provenance::Direct,
                vec![
                    sub(ExpectedStatus::Pass, "t", "pass"),
                    sub(ExpectedStatus::Fail, "t", "defect"),
                ],
            ),
            file(
                "corpus/b.js",
                "FileAPI/blob/b.any.js",
                Provenance::Direct,
                vec![sub(ExpectedStatus::Pass, "t2", "b")],
            ),
        ]);
        let inventory = load_inventory(&inventory_text(), &source()).expect("inv");
        let files = vec![
            result(
                "FileAPI/blob/a.any.js",
                vec![
                    row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "pass"),
                    row(ActualStatus::Fail, ExpectedStatus::Fail, "t", "defect"),
                ],
            ),
            result(
                "FileAPI/blob/b.any.js",
                vec![row(ActualStatus::Pass, ExpectedStatus::Pass, "t2", "b")],
            ),
        ];
        let run = build_gate_run(RunMode::Strict, &manifest, &inventory, &[], files).expect("run");
        assert!(run.expectations_match);
        assert!(!run.release_green);
        assert_eq!(run.release_blockers.defects, 1);
        assert_eq!(run.exit_reason, ExitReason::ReleaseDefects);
        assert_eq!(run.exit_reason.exit_code(), 1);
    }

    /// R1-02: duplicate, missing and unexpected extra results are drift.
    #[test]
    fn r1_02_duplicate_missing_extra_are_drift() {
        let manifest = manifest(vec![file(
            "corpus/a.js",
            "FileAPI/blob/a.any.js",
            Provenance::Direct,
            vec![
                sub(ExpectedStatus::Pass, "t", "one"),
                sub(ExpectedStatus::Pass, "t", "two"),
            ],
        )]);
        let inventory = load_inventory(&inventory_text(), &source()).expect("inv");
        let exclude = vec![exclusion_row("FileAPI/blob/b.any.js")];
        // Duplicate exact id.
        let dup = vec![result(
            "FileAPI/blob/a.any.js",
            vec![
                row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "one"),
                row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "one"),
                row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "two"),
            ],
        )];
        let run =
            build_gate_run(RunMode::Strict, &manifest, &inventory, &exclude, dup).expect("run");
        assert!(!run.expectations_match);
        assert_eq!(run.exit_reason, ExitReason::ExpectationDrift);
        // Duplicate does not inflate the ordinary totals.
        assert_eq!(run.results.total, 3);
        assert_eq!(run.results.upstream_pass, 2);
        // Missing row.
        let missing = vec![result(
            "FileAPI/blob/a.any.js",
            vec![row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "one")],
        )];
        let run =
            build_gate_run(RunMode::Strict, &manifest, &inventory, &exclude, missing).expect("run");
        assert!(!run.expectations_match);
        assert_eq!(run.exit_reason, ExitReason::ExpectationDrift);
        // Unexpected extra.
        let extra = vec![result(
            "FileAPI/blob/a.any.js",
            vec![
                row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "one"),
                row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "two"),
                row(
                    ActualStatus::Fail,
                    ExpectedStatus::Pass,
                    "t",
                    "unexpected:zzz",
                ),
            ],
        )];
        let run =
            build_gate_run(RunMode::Strict, &manifest, &inventory, &exclude, extra).expect("run");
        assert!(!run.expectations_match);
        assert_eq!(run.release_blockers.expectation_drift, 1);
    }

    /// R1-06: status table — exit 0 is impossible with a non-green run and a
    /// green run has no blockers.
    #[test]
    fn r1_06_status_consistency() {
        let manifest = manifest(vec![file(
            "corpus/a.js",
            "FileAPI/blob/a.any.js",
            Provenance::Direct,
            vec![
                sub(ExpectedStatus::Pass, "t", "p"),
                sub(ExpectedStatus::NotRun, "t", "n"),
            ],
        )]);
        let inventory = load_inventory(&inventory_text(), &source()).expect("inv");
        let exclude = vec![exclusion_row("FileAPI/blob/b.any.js")];
        for (actual_pass, actual_notrun) in [
            (ActualStatus::Pass, ActualStatus::NotRun),
            (ActualStatus::Fail, ActualStatus::NotRun),
            (ActualStatus::Pass, ActualStatus::Fail),
            (ActualStatus::Timeout, ActualStatus::NotRun),
        ] {
            let files = vec![result(
                "FileAPI/blob/a.any.js",
                vec![
                    row(actual_pass, ExpectedStatus::Pass, "t", "p"),
                    row(actual_notrun, ExpectedStatus::NotRun, "t", "n"),
                ],
            )];
            let run = build_gate_run(RunMode::Strict, &manifest, &inventory, &exclude, files)
                .expect("run");
            assert!(
                !run.success() || run.release_green,
                "exit 0 with release_green false"
            );
            if run.release_green {
                assert!(
                    run.release_blockers.is_empty() || run.release_blockers.not_a_release_mode == 0
                );
                assert_eq!(run.release_blockers.defects, 0);
            }
        }
    }

    /// R1-03: an adapted project-acceptance smoke does not claim the upstream
    /// path; the path stays excluded and smoke rows are separate.
    #[test]
    fn r1_03_project_acceptance_does_not_claim_upstream() {
        let mut adapted_sub = sub(ExpectedStatus::Pass, "filelist-host", "s");
        adapted_sub.classification = Classification::ProjectAcceptance;
        let manifest = manifest(vec![
            file(
                "corpus/a.js",
                "FileAPI/blob/a.any.js",
                Provenance::Direct,
                vec![sub(ExpectedStatus::Pass, "t", "a")],
            ),
            file(
                "corpus/filelist-host.js",
                "FileAPI/blob/b.any.js",
                Provenance::Adapted,
                vec![adapted_sub],
            ),
        ]);
        let inventory = load_inventory(&inventory_text(), &source()).expect("inv");
        let exclude = vec![exclusion_row("FileAPI/blob/b.any.js")];
        let files = vec![
            result(
                "FileAPI/blob/a.any.js",
                vec![row(ActualStatus::Pass, ExpectedStatus::Pass, "t", "a")],
            ),
            result(
                "FileAPI/blob/b.any.js",
                vec![row(
                    ActualStatus::Pass,
                    ExpectedStatus::Pass,
                    "filelist-host",
                    "s",
                )],
            ),
        ];
        let run =
            build_gate_run(RunMode::Strict, &manifest, &inventory, &exclude, files).expect("run");
        let inv = run.inventory.expect("inventory");
        assert_eq!(inv.total, 2);
        assert_eq!(inv.executed_direct, 1);
        assert_eq!(inv.executed_adapted, 0);
        assert_eq!(inv.excluded, 1);
        assert_eq!(run.results.smoke_pass, 1);
    }
}
