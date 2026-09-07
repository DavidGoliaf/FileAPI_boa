# M3-A Rework — bounded materialization and real non-limit rejection proof

| Field | Value |
|---|---|
| Status | `REWORK REQUIRED` |
| Branch | Continue on `task/m3`; do not start M3-B/M4 |
| Base | `d2e1ccabe7473e7dc1273011cf725ae0df393afd` |
| Scope authority | Only the two findings, regression tests, evidence refresh and validation below |

## Immutable rules

1. Do not modify `TZ_boa_fapi_FileAPI.md`, `tasks/03_TASK_PROMISE_BLOB_READS.md`, M1/M2 tasks, thresholds, acceptance criteria, matrix IDs or CI semantics.
2. Do not hide either finding through `#[ignore]`, feature gating, mock-only replacement, lint/coverage reduction, exclusion, production test hook, false PASS or documentation-only change.
3. Keep safe Rust, core Boa-free, no raw segment/source/identity public API and no new dependency unless an ADR is written before adding it.
4. After the final code fix, run a new audit. Fix every defect found there with a regression test, then restart audit and validation. Do not change requirements to avoid a finding.

## P1 — `BlobData::materialize` trusts invalid `ByteSource` output

### Finding

`crates/boa_fapi_core/src/blob.rs` appends the `Bytes` returned by `ByteSource::read_range` without checking its length. `BlobData::from_segments` validates the requested range against `ByteSource::len()`, but the public trait does not mechanically force an implementation to return exactly that range.

An oversized response can make `Vec::extend_from_slice` allocate beyond the preflighted `max_materialize_bytes`; a short response succeeds with truncated bytes in release builds because the final size check is only `debug_assert_eq!`. This violates the exact-range, checked/fallible allocation and no-partial-result contract.

### Required implementation

For every segment, before append:

1. Convert `seg.len` using `usize::try_from`; failure returns `FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes)`.
2. Require `chunk.len() == expected_segment_len`; mismatch returns existing typed `FileApiError::InvalidRange`.
3. Use `checked_add` for next output length and require it not to exceed the already checked exact capacity. On failure return typed error before append.
4. Append only after checks. Any error discards local buffer and returns `Err`, never partial `Bytes`.
5. Replace debug-only final size assertion with a release-enforced typed invariant (or make it structurally impossible to violate). No production `assert!`, `expect`, `unwrap` or implicit capacity growth.

Keep normal memory-source byte order, cancellation semantics and the exact materialization-limit boundary.

### Required regression tests

In a `#[cfg(test)]` core child module create controlled `ByteSource` implementations:

* declared valid length but response one byte **shorter** than requested → `Err(InvalidRange)`;
* declared valid length but response one byte **longer** than requested → `Err(InvalidRange)` and no output growth past `max_materialize_bytes`;
* retain multi-segment exact output, cancel-before/between and exact/over-limit coverage.

No private production hook, no weakened public API guard.

## P2 — claimed non-limit Promise rejection is not tested

### Finding

`non_limit_core_error_rejects_with_plain_error` configures zero materialization limit and reads an empty Blob. It explicitly expects fulfillment. Therefore it does not exercise a non-limit `FileApiError`, does not prove `reject_with` plain-`Error` branch, and contradicts its name plus M3 trace/audit claim.

### Required implementation and proof

1. Remove or rename the misleading test; do not retain a false trace claim.
2. Add a `#[cfg(test)]` child-module test in `promise_read.rs` through the real `read_promise` → `PromiseJob` → `Context::run_jobs()` path. Its controlled `ByteSource` returns non-limit `FileApiError::Cancelled` or `InvalidRange`.
3. Observe the JS Promise and prove: pending before `run_jobs`; rejected after it; reason `instanceof Error`; reason not `instanceof RangeError`; no fulfilled bytes/string/ArrayBuffer result; original BlobData unchanged.
4. Keep the existing `ResourceLimit(MaterializeBytes)` → `RangeError` integration test and prove the mappings are distinct.

Do not add public arbitrary-`ByteSource` host construction, public test accessor or DOMException merely for testing.

## Documentation and audit refresh

1. Correct `docs/spec-matrix.md`, `docs/m3-final-audit.md`, `docs/m3-validation.md` and `docs/reviews/M3-handoff.md` to state actual tests and results. Remove the false non-limit claim.
2. Record both findings, fixes and exact regression names in the final audit; retain existing `M3-READ` IDs.
3. CI status is checked by the customer before resubmission for acceptance. The executor must not invent a run URL/ID or claim remote CI PASS. In `M3-handoff.md` it records `CI: awaiting customer verification` unless the customer has explicitly supplied the verified result.

## Required final validation and acceptance

After the final production change run M3-A §7 in this exact order:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

Record exact local exit code and PASS/FAIL/BLOCKED. Before resubmission, the customer independently verifies that CI is green on Windows and Ubuntu for the final commit; that external confirmation is required for acceptance but is not an executor-produced artifact. Before commit perform the mandatory retrospective bug-find pass, update the handoff and stop for independent review. Do not start M3-B, M4 or unrelated cleanup.
