# M9-B Handoff — I/O executor and Promise-based Blob reads

Branch: `task/m9b`, base: accepted M9-A head.
Status: CI GREEN locally on this tree (see demo commands). Handoff is
evidence, not acceptance.

## Built

- `crates/boa_fapi/src/io.rs` (new): `FileIoExecutor`/`FileIoWake`
  (`Send + Sync + 'static`, no Boa, no user JS), Send-only `FileIoTask`/
  `FileIoCompletion` (no `JsValue`/`JsObject`/`Context`/realm/paths in
  types or `Debug`), opaque `FileApiContextId`/`FileIoOperationId` (never
  reused), per-context `IoBridge` (quota `max_concurrent_reads_per_global`,
  bounded completion queue, cancellation tokens; mutex only for
  bookkeeping, never across I/O/Boa/JS), `ThreadedFileIoExecutor` (fixed
  workers + bounded queue; thread-per-read without a limit forbidden),
  `NoopWake`, typed `FileIoSubmitError`/`PollIoError`. Worker panics are
  contained to a stable `NotReadableError`.
- `FileApiExtensionBuilder::io_executor/io_wake` + per-registration bridge
  (`RegisteredSpecs::io/context_id`, shutdown-tracked); `FileApiHandle::
  poll_io/has_pending_io/is_shutdown(public)` — owner-only `poll_io`,
  foreign contexts rejected without state change, DTOs → Boa jobs only, no
  user JS under a mutex, exact-once quota release, shutdown drops late
  completions with no jobs/telemetry/JS.
- `promise_read.rs`: Boa-thread brand/IDL validation + size preflight +
  quota reservation, pending `Promise` first, filesystem task to the
  executor; memory and filesystem share the path (no sync `materialize`
  fallback); settlement only through one Boa job after `poll_io`
  (`settle_completion` + `PendingReads` table); submit/queue-full/worker-
  loss/shutdown settle through the central `DOMException` mapping
  (`TooManyReads` → `SecurityError`, worker loss → `NotReadableError`).
- Tests: `crates/boa_fapi/tests/m9_promise_io.rs` (17, controlled manual
  executor + counting wake, no `sleep` oracle): pending-before-I/O with a
  usable Boa thread, `run_jobs`-alone never settles (behavioural blocking-
  source guard with a gated worker thread), foreign `poll_io` rejection,
  completion-before-`poll_io` runs no JS, success/error/shutdown/queue-
  full/worker-loss exact-once quota, 65-read limit + recovery, snapshot
  error with no partial bytes, panic containment, byte-exact
  memory/filesystem parity, verbatim host `poll_io`/`run_jobs` loop.
- Pre-existing suites migrated to the documented host loop (`poll_io` +
  `run_jobs`): `m3_promise_blob_reads` (16), `m5_file_fs` (15),
  `abort_races`/`abort_races_fs`, `appendix_a_acceptance` (promise path),
  `m4_filereader_async` (promise regression), `m8_observability`
  (promise paths), `m3_blob_streams` (the one body awaiting `text()`).
  Streams/FileReader scheduling itself is untouched (M9-C/M9-D scope).
- Docs: `docs/host-integration.md` (verbatim loop, executor/wake config),
  `crates/boa_fapi/README.md` (loop + executor), `docs/architecture.md`
  (Layer 2b executor), `docs/spec-matrix.md` (M9B-IO-01…04, M9B-HOST-01),
  `docs/DECISIONS.md` (ADR-0042, no new dependencies). Guards updated:
  `io.rs` is the single allowed `std::thread` site; public I/O surface
  asserted.
- No new dependencies (`cargo-deny` clean, allow-list unchanged).
- Rework after acceptance: `poll_io` is now strictly non-blocking; the WPT
  runner uses a generation-counted `FileIoWake` plus a bounded condition
  variable wait, rather than a fixed CPU-spin budget. `m9_promise_io` runs
  under the exact acceptance command (17 tests, no feature gate), and the
  operation-id `u64::MAX` boundary is covered by an `io.rs` unit test.
  The legacy M8 feature-off guard now drives the same documented host loop;
  the `promise_read` unit helper has a bounded worker-scheduling guard, so
  it does not assume a fixed number of immediate polls is portable.

## Timing-independent evidence (blocking source not on the Boa thread)

- `m9_promise_io::blocking_source_never_runs_inside_boa_job`: a host
  `ByteSource` blocks on a test gate on a worker thread while the Boa
  thread runs unrelated jobs (`jobRan === 'yes'`) and the promise stays
  `pending` through repeated `run_jobs()` without `poll_io`; after the
  gate opens, the documented loop settles `fulfilled:11`. A synchronous
  Boa-job `materialize` fallback would hang the Boa thread or settle
  without `poll_io` — neither happens.
- `m9_promise_io::pending_promise_before_blocking_io_boa_thread_stays_usable`:
  with every request held by the manual executor, the promise returns
  pending, `executor.pending() === 1`, unrelated eval + `Promise.resolve`
  jobs run, completion before `poll_io` runs no JS, and `poll_io` settles
  exactly 1 job.
- Pre-existing behaviour preserved: `cargo test --workspace --all-features`
  green (all suites), `m9_promise_io` 17/17 green.
- Feature combinations: `cargo hack check --feature-powerset --depth 2`
  green; the advanced immutable `BlobData` host constructors do not require
  a test-only Cargo feature and compile in every supported feature set.

## Deviations

None. FileReader/Streams stay on their current scheduling (M9-C/M9-D);
`FileReaderSync` unchanged except the shared packaging already accepted
in M9-A.

## Demo commands (this tree, Windows)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_promise_io -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

Then stop (do not start M9-C).
