# M3-A Final Audit Report

## Step A — Requirements Verification

| ID | Requirement | Code (file:symbol) | Test | Verdict |
|---|---|---|---|---|
| M3-READ-01 | `text`/`arrayBuffer`/`bytes` on `Blob.prototype` only (name/length/descriptors, writable/non-enumerable/configurable); inherited by `File`; brand violations are synchronous `TypeError` | `blob.rs:init_prototype` (M3 members); `promise_read.rs:text`, `::array_buffer`, `::bytes` via `require_blob` | `m3_promise_blob_reads::read_methods_live_on_blob_prototype_with_correct_descriptors`, `::file_inherits_read_methods_without_own_copies`, `::brand_violations_throw_synchronously_without_promise` | PASS |
| M3-READ-02 | Pending `Promise` immediately; settlement only through Boa jobs; FIFO order | `promise_read.rs:read_promise` (`JsPromise::new_pending` + `PromiseJob` via `enqueue_job`); `::settle_read` | `::text_returns_pending_promise_settled_by_run_jobs`, `::empty_blob_read_stays_pending_until_run_jobs`, `::two_concurrent_reads_settle_fifo` | PASS |
| M3-READ-03 | `text()` UTF-8 replacement decode (invalid/truncated → U+FFFD) | `promise_read.rs:package_bytes` (`String::from_utf8_lossy`) | `::text_decodes_ascii_and_multibyte`, `::text_replaces_invalid_utf8`, `::text_reads_composed_and_sliced_blobs` | PASS |
| M3-READ-04 | `arrayBuffer()` exact bytes in a fresh independent `ArrayBuffer` | `promise_read.rs:package_bytes` (`JsArrayBuffer::new` + copy) | `::array_buffer_returns_exact_bytes_in_fresh_buffer`, `::array_buffer_results_are_independent` | PASS |
| M3-READ-05 | `bytes()` fresh offset-0 `Uint8Array` over an independent buffer | `promise_read.rs:package_bytes` (`JsUint8Array::from_iter`) | `::bytes_returns_uint8array_with_offset_zero`, `::bytes_results_are_independent` | PASS |
| M3-READ-06 | Materialization limit → pending `Promise` rejected as `RangeError`; `size == limit` ok, `limit + 1` rejects; blob stays usable; non-limit core error → plain `Error` | `blob.rs:BlobData::materialize`; `promise_read.rs:reject_with` | `::over_materialize_limit_rejects_with_range_error`, `::materialize_limit_boundary`; `promise_read::tests::non_limit_error_rejects_with_plain_error_not_range_error`, `::non_limit_rejection_mapping_is_distinct_from_limit_mapping`; core `blob::tests::materialize_*` (9 unit tests) | PASS |
| M3-READ-07 | `File` reads use exactly the Blob-brand path | `promise_read.rs` via `brand::require_blob` (accepts `FileNative`) | `::file_inherits_read_methods_without_own_copies`, `::text_decodes_ascii_and_multibyte` (File case) | PASS |
| M3-READ-08 | Bounded core materialize: order, empty, exact/over limit, short/long source responses, cancel paths, checked offsets | `blob.rs:BlobData::materialize` | `blob::tests::materialize_multi_segment_order`, `::materialize_empty_blob`, `::materialize_exactly_at_limit`, `::materialize_one_over_limit_rejected_before_allocation`, `::materialize_rejects_short_source_response`, `::materialize_rejects_long_source_response`, `::materialize_cancelled_before_first_read`, `::materialize_cancelled_between_segments`, `::materialize_checked_offset_failure` | PASS |

## Step B — Defect search findings and fixes

1. **M2 descriptor expectation for `slice`** — M2 fixed `slice` as
   enumerable, but Web IDL methods must be non-enumerable like the three new
   read methods. Fix: `slice` registered non-enumerable; M2
   `prototype_member_descriptors` and `constructor_descriptors_and_metadata`
   updated to assert the corrected descriptors. Regression covered by the
   same tests.
2. **`JsPromise::new` settles synchronously** — the executor form resolves
   before returning, violating the pending-first rule. Fix: use
   `JsPromise::new_pending` and settle only inside the enqueued `PromiseJob`.
3. **Async-IIFE eval returns a promise, not a bool** — `assert_eval`-style
   helpers cannot observe `await` results directly. Fix: `assert_async_body`
   drives `run_jobs()` and asserts the outer promise fulfills with `true`
   via `JsPromise::state()`.
4. **`BytesMut::try_reserve_exact` does not exist** — the `bytes` 1.x API
   only offers `Vec::try_reserve_exact`. Fix: `materialize` accumulates into
   a `Vec<u8>` with fallible reservation, then freezes into `Bytes`.
5. **Guard over-matched `materialize`** — the M2-era forbidden list still
   contained `pub fn materialize(` after the M3 order authorized it. Fix:
   guard allows exactly the 10 fixed methods; comment documents the M3
   exception.
6. **Rework P1: `materialize` trusted invalid source output** — returned
   `Bytes` were appended without length checks, so an oversized response
   could grow past `max_materialize_bytes` and a short one silently
   truncated in release (`debug_assert_eq!` only). Fix: `usize::try_from`
   on `seg.len`, exact `chunk.len()` match → `InvalidRange`, `checked_add`
   + capacity bound before append, release-enforced final length check.
   Regression tests: `materialize_rejects_short_source_response`,
   `materialize_rejects_long_source_response` (controlled `ByteSource`,
   child module only).
7. **Rework P2: false non-limit rejection claim** — the integration test
   named `non_limit_core_error_rejects_with_plain_error` read an empty blob
   under a zero limit and expected fulfillment, proving nothing about the
   plain-`Error` branch. Fix: misleading test removed; real proof added as
   `promise_read::tests::non_limit_error_rejects_with_plain_error_not_range_error`
   plus `::non_limit_rejection_mapping_is_distinct_from_limit_mapping`,
   both through `read_promise` → `PromiseJob` → `run_jobs()` with a
   failing controlled source (`Cancelled`) and limit/`Error` name/message
   assertions.

## Step C — Independent audit notes

- **Dependency graph**: no new dependencies for M3-A
  (`cargo metadata` shows only `boa_engine`/`boa_gc`/`bytes`/`thiserror` in
  `boa_fapi`; core still `bytes` + `thiserror` only). `cargo deny check`
  exits 0 with `wildcards = "deny"` intact.
- **Core isolation**: `boa_fapi_core` sources contain no Boa/JS/DOM/URL/fs
  types (guards `core_source_has_no_boa_types`,
  `core_cargo_toml_has_no_boa_dependencies` green); `materialize` takes
  `&FileApiLimits` + `&CancellationToken` only.
- **Public APIs**: core exposes exactly 10 methods
  (`blob_data_public_api_is_fixed` green); bindings expose no new public
  Rust items (`internal_binding_modules_expose_no_public_items` covers
  `promise_read.rs`); `lib.rs` re-exports unchanged.
- **Capture/GC ownership**: `ReadRequest` holds `Arc<BlobData>`,
  `FileApiLimits`, `ReadMode` only; resolvers travel in the `PromiseJob`
  closure as traced `JsFunction`s; no `Context`/`JsObject`/callback stored
  in core or native DTOs; `#![deny(unsafe_code)]` holds.
- **Settlement order/realm**: `PromiseJob::with_realm` with the calling
  realm; `enqueue_job` before returning the pending promise; no
  `Promise.resolve` shortcut, no resolver call on the JS stack, no
  `run_jobs()` inside the job; `.then` reactions run on later Boa passes
  (FIFO proven by `two_concurrent_reads_settle_fifo`).
- **Packaging**: `text` via `from_utf8_lossy`; `arrayBuffer` via fresh
  `JsArrayBuffer::new` + copy; `bytes` via `JsUint8Array::from_iter`
  (offset 0, own buffer). Independence proven by mutation tests.
- **Conversions/allocation**: `materialize` checks the limit before
  allocation, `usize::try_from` + `try_reserve_exact` map failure to
  `ResourceLimit(MaterializeBytes)`; `ArrayBuffer` length comes from
  `bytes.len()` (already bounded by the limit); no `as` casts on unbounded
  values, no panic, no partial resolve.
- **Limits/cancellation/rejection**: limit → `RangeError`, other core
  failures → plain `Error` without body detail, engine packaging failure →
  opaque value; boundary `size == limit` / `limit + 1` covered per method;
  cancellation before/first/between segments covered in core unit tests.
- **No accidental scope**: `stream`, `textStream`, `FileReader`,
  `FileReaderSync`, `EventTarget`, `DOMException`, `ReadableStream` absent
  as globals/members (JS test + `guards::no_stream_filereader_or_dom_surface`
  production scan).
- **Docs claims**: README/architecture state memory-backed promise reads
  with explicit `run_jobs()` and list streams/FileReader/DOMException/fs as
  absent; spec matrix rows reference real `file:symbol` pairs and existing
  tests; CI runs the M3-A test after the M2 test on both OS jobs.

## Step D — Final validation (post-fix)

All work order §7 commands re-run after the last production change; every
command exited 0. Exact commands, exit codes and the coverage table are
recorded in `docs/m3-validation.md`.

## Audit conclusion

- All M3-READ-01..08 requirements verified with code and test evidence.
- 7 defects found during the audit pass and fixed with regression tests
  (5 initial + 2 rework findings).
- 234 workspace tests pass; boa_fapi line coverage 92.29% (threshold 85%).
- No masked failures, skips, exclusions, or changed acceptance criteria.
