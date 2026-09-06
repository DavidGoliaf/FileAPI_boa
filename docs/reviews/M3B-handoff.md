# M3-B Handoff — Blob streams: `stream()`, `textStream()`

## What was built

- Core: bounded `BlobReader` in `crates/boa_fapi_core/src/blob.rs`
  (`BlobData::reader` validates `default_chunk_size` to `16 KiB..=1 MiB`;
  `read_next` yields at most one exact ordered chunk per call with O(chunk)
  memory, checked arithmetic, exact source-output match, sticky terminal
  error class; `cancel` idempotent and isolated). 8 unit tests; guards fix
  11 `BlobData` + 2 `BlobReader` methods.
- Bindings: new isolated `crates/boa_fapi/src/streams.rs` — branded
  `ReadableStream` shim + `ReadableStreamDefaultReader` (2 globals,
  5 methods, 1 accessor, 2 tags; direct `new` is `TypeError`),
  `Blob.prototype.stream()`/`textStream()` (Blob-brand only, File
  inherits), one `PromiseJob` per `read()`, fresh `Uint8Array`/incremental
  UTF-8 delivery, EOF with decoder flush, idempotent cancels, lock rules,
  plain-`Error` terminal rejections. Hand-written `Utf8Decoder`, no
  `encoding_rs`. Cargo feature `streams-shim` (default on) +
  `FileApiExtensionBuilder::streams_shim(bool)`; off returns typed
  `RegisterError::StreamsShimDisabled` before any mutation.
- Tests: `crates/boa_fapi/tests/m3_blob_streams.rs` (28 tests, all through
  `context.run_jobs()`); `streams::tests` (5 proofs: 2 JS-realm error
  proofs + 3 deterministic-`force_collect` GC proofs); guards
  (`streams_shim_surface_is_bounded`, `no_filereader_or_dom_surface`,
  module scan covers `streams.rs`).
- Docs/trace: `M3-STREAM-01..09` in `docs/spec-matrix.md`;
  ADR-0014/0015/0016; README + architecture (bounded shim, `run_jobs()`,
  absent full-Streams/M4/fs); CI runs the M3-B test after M3-A on both OS.

## Base / commit

- Base: M3-A commit `59a8932`; branch `task/m3b`.
- Implementation baseline (code freeze): `fb46cff` (initial M3-B),
  `13b8582` (M3B-rework R1–R3), `829b26b` (final implementation commit:
  full `validate()` at `register()`). Validation/audit/coverage results in
  this handoff describe this baseline.
- Documentation-only handoff fixes after the baseline (`040211d`,
  `8648a11`, and any later doc-only commits) change this handoff text
  only — no production code, tests, or validation results. The current
  HEAD is resolved via `git log`; it is not part of the implementation
  baseline.

## Demo commands (work order §7, in order)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
cargo test --package boa_fapi --test m3_blob_streams
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

All exit 0; recorded in `docs/m3b-validation.md` (coverage 89.81% lines).
Final audit trace and findings: `docs/m3b-final-audit.md`.

## Matrix / ADR

- Matrix: `M3-STREAM-01..09` with `file:symbol`, normal/error/boundary tests.
- ADR-0014: bounded `BlobReader` vs materialization.
- ADR-0015: capability-checked shim + atomic registration.
- ADR-0016: hand-written incremental UTF-8 decoder, no `encoding_rs`.
- No new dependencies, so no dependency ADR.

## Coverage

- Workspace: 267 tests green (30 boa_fapi unit incl. 5 stream proofs,
  11 guards, 51 M2 JS integration, 28 M3-B JS integration, 16 M3-A JS
  integration, 1 doc, 130 core); `boa_fapi` line coverage 89.81%
  (threshold 85%, see `docs/m3b-validation.md`).

## CI

`CI: awaiting customer verification` — the customer checks green Windows
and Ubuntu runs for the final commit before acceptance; no run URL/ID is
claimed here.

## Findings / fixes

See `docs/m3b-final-audit.md` Step B (7 items) plus the `M3B-rework`
section (R1–R3 and the R1/R2/R3 follow-ups in this commit): GC-safe
resolver ownership (resolvers only in traced job captures; 3
deterministic-`force_collect` regression tests), no empty text chunks
(in-request coalescing loop), chunk-size range in
`FileApiLimits::validate()` with full `validate()` at `register()` (no
carve-out; M2/M3-A fixtures migrated to fully valid configs without
weakened assertions).

## Deviations

None. No changes to `TZ_boa_fapi_FileAPI.md`, the M3-B/M1/M2/M3-A orders,
thresholds, CI workflow semantics beyond the appended M3-B test step, trace
IDs, or acceptance criteria.
