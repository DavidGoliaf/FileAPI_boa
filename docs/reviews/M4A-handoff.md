# M4-A Handoff — DOM shim and async `FileReader` for memory Blob

## What was built

- DOM shim: new `crates/boa_fapi/src/dom.rs` — branded `EventTarget`
  (3 methods, tuple `(type, callback, capture)` dedupe, at-target dispatch
  only, first listener error rethrown after all listeners ran), `Event`
  (7 readonly attributes + `preventDefault`/`stopImmediatePropagation`),
  `ProgressEvent` (inherits `Event`, 3 readonly progress attributes),
  `DOMException` (inherits `Error`, readonly `name`/`message`,
  `[object DOMException]`, required names) plus the central
  `map_core_error` (NotFound→NotFoundError,
  UnsafeFile/TooManyReads/PermissionDenied→SecurityError,
  SnapshotChanged/FileLocked/InvalidRange/Internal→NotReadableError,
  ResourceLimit→QuotaExceededError, Cancelled→AbortError, no
  path/byte/source details). Cargo feature `dom-shim` (default on) +
  `FileApiExtensionBuilder::dom_shim(bool)`; off returns typed
  `RegisterError::DomShimDisabled` before any mutation.
- FileReader: new `crates/boa_fapi/src/filereader.rs` — `new FileReader()`
  (`new`-only, `name`/`length` exact), prototype inherits
  `EventTarget.prototype`, `Symbol.toStringTag="FileReader"`, 5 async read
  methods (`length` 1/1/1/1/0), readonly `readyState`/`result`/`error`, 6
  writable `on*` handlers, `EMPTY`/`LOADING`/`DONE` on constructor and
  prototype. Per-Context FIFO FileReading jobs over the Boa promise-job
  queue drained by `context.run_jobs()`; monotonic generations make stale
  completions strict no-ops; `max_concurrent_reads_per_global` quota with
  exact release; `loadstart`/`progress` dispatch inside their pump job,
  terminal `load`/`error`/`abort` (+ conditional `loadend`) through queued
  dispatch; 50 ms `progress` throttle on the injected `Clock`; incremental
  `encoding_rs` text decoding (replacement, split sequences, BOM);
  checked Data-URL packaging vs `max_data_url_output`; O(chunk + result)
  memory. Initial state exactly `(EMPTY, null, null)`.
- Migration: M3 promise reads and M3-B stream errors reject once with the
  same central mapped same-realm `DOMException` (pre-M4
  `RangeError`/plain-`Error` fallbacks kept behind `cfg(not(dom-shim))`
  for the powerset check). Affected tests/trace rows updated; no stale
  plain-`Error`/`RangeError` claim retained.
- Tests: `crates/boa_fapi/tests/m4_filereader_async.rs` (31 tests, all
  through `context.run_jobs()`); `promise_read::tests` + `streams::tests`
  re-proven on the mapped `DOMException`; guards
  (`filereader_and_dom_surface_is_bounded`,
  `no_filereader_sync_or_out_of_scope_surface`, updated
  `streams_shim_surface_is_bounded`).
- Docs/trace: `M4-DOM-01..05` + `M4-FR-01..10` in `docs/spec-matrix.md`
  (M3-READ-06/M3-STREAM-07 updated); ADR-0017..0021; README surface via
  `lib.rs` docs + `docs/architecture.md`; CI runs the M4-A test on both
  OS after M3-B.
- Deps: `encoding_rs = "0.8"` (0.8.35), `base64 = "0.22"` (0.22.1);
  `deny.toml` gained `BSD-3-Clause`.

## Base / commit

- Base: M3-B handoff commit `040211d`; branch `task/m4a`.
- Implementation + validation/audit results in this handoff describe the
  final commit of this branch (resolved via `git log`); no production
  code, tests, or validation results changed after the green run except
  this handoff file.

## Demo commands (work order §7, in order)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
cargo test --package boa_fapi --test m3_blob_streams
cargo test --package boa_fapi --test m4_filereader_async
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

All exit 0; recorded in `docs/m4a-validation.md` (coverage 88.26% lines).
Final audit trace and findings: `docs/m4a-final-audit.md`.

## Matrix / ADR

- Matrix: `M4-DOM-01..05`, `M4-FR-01..10` with `file:symbol`,
  normal/error/boundary/race tests; M3 rows updated to the DOMException
  mapping.
- ADR-0017: minimal DOM ownership/capability negotiation.
- ADR-0018: FileReading FIFO + generation lifetime.
- ADR-0019: result/error mapping.
- ADR-0020: `encoding_rs` 0.8 dependency.
- ADR-0021: `base64` 0.22 dependency.

## Coverage

- Workspace: 299 tests green (30 boa_fapi unit, 12 guards, 51 M2 JS
  integration, 31 M4-A JS integration, 28 M3-B JS integration, 16 M3-A JS
  integration, 1 doc, 130 core); `boa_fapi` line coverage 88.26%
  (threshold 85%, see `docs/m4a-validation.md`).

## CI

`CI: awaiting customer verification` — the customer checks green Windows
and Ubuntu runs for the final commit before acceptance; no run URL/ID is
claimed here.

## Findings / fixes

See `docs/m4a-final-audit.md` §G (7 items, all resolved): decoder output
capacity, generic-queue event loss → in-job non-terminal dispatch,
`timeStamp` clock perturbation, BOM/unknown-label handling, deny
`BSD-3-Clause`, `cfg`-gated pre-M4 fallbacks for the powerset check, and
the private-module rustdoc link. Each fix re-ran the affected suites
green; the audit was repeated until no unresolved item remained.

## Explicitly omitted (separate future orders)

`FileReaderSync`, workers, filesystem-backed sources, blob URLs,
structured clone, full DOM/HTML, full Streams, WPT harness. The negative
guards (`excluded_m4b_apis_are_absent`,
`no_filereader_sync_or_out_of_scope_surface`) prove their absence.

## Deviations

None. No changes to `TZ_boa_fapi_FileAPI.md`, the M4-A order, thresholds,
CI workflow semantics beyond the appended M4-A test step, trace IDs, or
acceptance criteria.
