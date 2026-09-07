# M3-A Rework 2 — JS proof of rejection type and truthful handoff

| Field | Value |
|---|---|
| Status | `REWORK REQUIRED` |
| Branch | Continue on `task/m3` from `568de992ad839bd32a6a91f790a3876862ab873d` |
| Scope | Only the two P2 findings, regression tests and evidence refresh below |

## Immutable rules

1. Do not change `TZ_boa_fapi_FileAPI.md`, `tasks/03_TASK_PROMISE_BLOB_READS.md`, M1/M2 orders, thresholds, acceptance criteria, matrix IDs, CI workflow semantics or test expectations.
2. Do not hide findings with ignore attributes, feature gates, coverage/lint reduction, exclusions, production test hooks, documentation-only edits, false PASS or changed requirements.
3. Keep safe Rust, core Boa-free and public `BlobData` API fixed. Do not add arbitrary-source host constructors, public test accessors, DOMException or M3-B/M4 API.
4. After both fixes, make a separate audit; fix every defect it finds with a regression test and restart audit plus validation.

## P2-A — non-limit rejection type is not proven in JavaScript

### Finding

`promise_read::tests::non_limit_error_rejects_with_plain_error_not_range_error` reads only `reason.name == "Error"` from Rust and uses a different `Context`. A writable `name` property does not prove prototype identity, so it does not prove `instanceof Error` and `!(instanceof RangeError)` in the originating realm.

### Required correction

1. Keep the existing test-only `FailingSource` and real `read_promise` → `PromiseJob` → `Context::run_jobs()` path. Do not replace it with direct `reject_with` testing or public hooks.
2. In the same Context, attach a JavaScript rejection handler before jobs run. Its observable result must prove `error instanceof Error`, `!(error instanceof RangeError)`, `error.name === "Error"` and `error.message === "blob read failed"`.
3. Prove pending before `run_jobs()` and the JS handler result after it; prove no fulfilled text/bytes/ArrayBuffer value and unchanged input `BlobData` metadata.
4. Retain the separate materialization-limit path and prove in the same JS-realm style that its reason is `instanceof RangeError`; prove the mappings are distinct.
5. The test is allowed only in `promise_read.rs` `#[cfg(test)]` child module. Normal integration tests remain real JS Context tests.

## P2-B — `M3-handoff.md` contradicts the final rework result

### Finding

The handoff correctly says `CI: awaiting customer verification`, but later says `232` tests, `91.40%` coverage and "see the Actions run". Final validation/audit say `234` tests and `92.29%`. The artifact is internally inconsistent and implies agent-produced CI evidence.

### Required correction

1. Update every handoff value to actual final validation results (currently expected: 234 workspace tests and 92.29% `boa_fapi` line coverage; use newer factual values if the final run differs).
2. Delete the obsolete "see Actions"/agent-run-ID wording.
3. Keep exactly `CI: awaiting customer verification` unless the customer explicitly supplies verified CI results.
4. Make `docs/m3-validation.md`, `docs/m3-final-audit.md`, `docs/spec-matrix.md` and `M3-handoff.md` agree on counts, coverage, regression names, commit scope and CI responsibility.

## Required final evidence

1. Update final audit with both P2 fixes and exact test names.
2. Update M3-READ-06 to identify the real JS realm assertions for both plain `Error` and `RangeError` mappings.
3. After the final production change, rerun the full M3-A §7 validation sequence unchanged: fmt, strict clippy, workspace/M2/M3 tests, strict docs, doc tests, llvm-cov ≥85%, cargo-hack, cargo-deny fetch/check and `git diff --check`. Record each actual exit as PASS/FAIL/BLOCKED.
4. The executor must not claim remote CI status. The customer verifies Windows and Ubuntu CI before requesting the next acceptance review.

Before commit make the mandatory retrospective bug-find pass. Commit on `task/m3`, update `M3-handoff.md`, then stop for independent review. Do not start M3-B, M4 or unrelated cleanup.
