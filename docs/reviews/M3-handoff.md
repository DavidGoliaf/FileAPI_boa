# M3-A Handoff — Promise reads: `text()`, `arrayBuffer()`, `bytes()`

## What was built

- Core: bounded `BlobData::materialize(limits, cancel)` in
  `crates/boa_fapi_core/src/blob.rs` — limit check before allocation,
  fallible `usize` conversion + `try_reserve_exact`, cancellation before
  every segment read, checked `offset..offset+len`, no partial bytes.
  7 unit tests; guard `blob_data_public_api_is_fixed` now allows exactly
  10 methods.
- Bindings: new isolated `crates/boa_fapi/src/promise_read.rs`
  (`ReadMode`, `ReadRequest`, `read_promise`, `text`, `arrayBuffer`,
  `bytes`, `settle_read`, `reject_with`, `package_bytes`) plus
  `js_read_error` in `error.rs`. Pending `JsPromise::new_pending` +
  `PromiseJob` with realm via `Context::enqueue_job`; settlement only in
  the job. `blob.rs::init_prototype` registers the three methods
  (writable, non-enumerable, configurable, length 0); `File` inherits via
  `Blob.prototype`. `slice` corrected to non-enumerable per Web IDL.
- Tests: `crates/boa_fapi/tests/m3_promise_blob_reads.rs` (17 tests, every
  read settled via `context.run_jobs()`); guards extended
  (`promise_read.rs` in the no-public-items scan,
  `no_stream_filereader_or_dom_surface`).
- Docs/trace: `M3-READ-01..08` in `docs/spec-matrix.md`; ADR-0011/0012/0013;
  README + architecture (memory-only reads, explicit `run_jobs()`); CI runs
  the M3-A test after the M2 test on both OS jobs.

## Base / commit

- Base: M2 commit `5620dc466a2286cfb9924044f35fdad9aa363685`; branch `task/m3`.
- This handoff covers the single M3-A commit on `task/m3` (see `git log`).

## Demo commands (work order §7, in order)

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

All exit 0; recorded in `docs/m3-validation.md` (coverage 92.29% lines).
Final audit trace and findings: `docs/m3-final-audit.md`.

## CI

`CI: awaiting customer verification` — the customer checks green Windows
and Ubuntu runs for the final commit before acceptance; no run URL/ID is
claimed here.

## Matrix / ADR

- Matrix: `M3-READ-01..08` with `file:symbol`, normal/error/boundary tests.- ADR-0011: why `materialize` is the sole bounded byte operation.
- ADR-0012: why the Boa job queue is the only settlement mechanism.
- ADR-0013: why limit → `RangeError`, other failures → `Error`.
- No new dependencies, so no dependency ADR.

## Coverage / CI

- Workspace: 232 tests green; `boa_fapi` line coverage 91.40% (threshold 85%).
- CI: `m3a-validation` job on `windows-latest` + `ubuntu-latest` runs §7 in
  order, including the new M3-A test step. CI links/run IDs: see the Actions
  run for this commit on `task/m3`.

## Findings / fixes

See `docs/m3-final-audit.md` Step B (7 items): `slice` enumerability,
`JsPromise::new` vs `new_pending`, async-IIFE assertion helper,
`BytesMut::try_reserve_exact`, guard allow-list for `materialize`, plus the
two rework findings — strict source-output checks in `materialize`
(short/long regression tests) and the real non-limit `Error` rejection
proof replacing the misleading integration test.

## Deviations

None. No changes to `TZ_boa_fapi_FileAPI.md`, the M3-A/M1/M2 orders,
thresholds, CI workflow semantics, trace IDs, or acceptance criteria.
