# M3-B Final Audit Report

## Step A — Requirements Verification

| ID | Requirement | Code (file:symbol) | Test | Verdict |
|---|---|---|---|---|
| M3-STREAM-01 | Shim registration/atomicity: 2 globals, exact descriptors/prototypes/`instanceof`, Blob-only methods, illegal construction/receivers `TypeError`, conflicts/non-extensible/feature-off leave no partial globals | `streams.rs:build_stream_specs`, `init_stream_prototype`, `init_reader_prototype`; `blob.rs:init_prototype` (M3-B members); `extension.rs:register`, `install_globals`, `rollback_globals` | `m3_blob_streams::stream_surface_descriptors_and_inheritance`, `::shim_constructors_and_receivers_reject_synchronously`, `::full_streams_api_absent`, `::global_conflict_leaves_no_partial_streams`, `::streams_shim_disabled_fails_before_global_mutation`, `::non_extensible_global_rejects_streams_atomically` | PASS |
| M3-STREAM-02 | Bounded core reader: chunk-size validation, one exact ordered chunk per call, O(chunk) memory | `blob.rs:BlobData::reader`, `BlobReader::read_next`, `::cancel` | `blob::tests::reader_rejects_zero_and_out_of_range_chunk_size`, `::reader_delivers_exact_chunks_in_order`, `::reader_splits_across_segments_preserving_order`, `::reader_never_exceeds_chunk_ceiling`, `::reader_empty_blob_yields_done_immediately`, `::reader_cancel_is_idempotent_and_isolated` | PASS |
| M3-STREAM-03 | Demand/FIFO/backpressure: nothing read before `read()`, one chunk per request, EOF repeats without reads; resolvers GC-safe in job captures | `streams.rs:reader_read`, `pump_one` | `::reads_are_pending_until_run_jobs_with_fifo_order`, `::eof_repeats_without_source_reads`, `::empty_blob_resolves_done_first_read`, `::second_chunk_not_read_before_second_demand`; `streams::tests::pending_read_survives_gc_with_exact_chunk`, `::two_queued_reads_survive_gc_fifo_exact`, `::gc_then_cancel_and_error_paths_settle` | PASS |
| M3-STREAM-04 | Byte packaging: exact concatenation, fresh offset-0 `Uint8Array` with independent backing, composed/sliced/File | `streams.rs:package_bytes_chunk` | `::byte_chunks_concatenate_exactly_with_fresh_backing`, `::sixteen_kib_boundary_yields_exact_chunks`, `::composed_and_sliced_blobs_stream_in_order`, `::two_streams_are_independent` | PASS |
| M3-STREAM-05 | EOF: `{value: undefined, done: true}`, repeated EOF, empty first-done | `streams.rs:pump_one` (EOF + decoder flush) | `::eof_repeats_without_source_reads`, `::empty_blob_resolves_done_first_read` | PASS |
| M3-STREAM-06 | Cancel/release/isolation: idempotent cancels, locked-cancel rejection, queued-done, release rules, sibling independence | `streams.rs:stream_cancel`, `reader_cancel`, `release_lock`, `cancel_shared` | `::cancel_before_first_read_resolves_done`, `::locked_stream_cancel_rejects_with_type_error`, `::reader_cancel_makes_queued_and_future_reads_done`, `::release_lock_with_queued_read_throws_without_state_change`, `::released_reader_read_throws_synchronously`, `::release_lock_then_new_reader_works`, `::two_streams_are_independent` | PASS |
| M3-STREAM-07 | Errors/bounds: short/long/source failures → same-realm plain `Error`, terminal replay, chunk-size bounds | `blob.rs:BlobReader` terminal error class; `streams.rs:stream_error_reason` | `blob::tests::reader_terminal_error_replays_without_new_reads`, `::reader_propagates_source_error_as_same_class`; `streams::tests::errored_stream_rejects_with_plain_error_not_range_error`, `::errored_stream_future_reads_reject_without_new_reads`; `m3_blob_streams::short_source_response_rejects_without_partial_chunk`, `::stream_core_error_rejects_plain_error_and_stays_terminal`, `::stream_error_path_rejects_with_plain_error_not_range_error`, `::chunk_size_config_bounds_rejected` | PASS |
| M3-STREAM-08 | Incremental UTF-8 decoder: split multibyte without early U+FFFD, every-boundary exactness, invalid flush, no spurious chunks, independent decoders | `streams.rs:Utf8Decoder::push`, `::flush`, `incomplete_tail_len` | `::text_stream_decodes_split_multibyte_without_early_replacement`, `::text_stream_split_at_every_boundary`, `::text_stream_invalid_flush_and_no_spurious_chunks` | PASS |
| M3-STREAM-09 | File inheritance and no accidental API | `streams.rs:stream`, `text_stream` via `require_blob` | `::shim_constructors_and_receivers_reject_synchronously` (File case), `::composed_and_sliced_blobs_stream_in_order` (File); `::full_streams_api_absent`; `guards::streams_shim_surface_is_bounded`, `::no_filereader_or_dom_surface` | PASS |

## Step B — Defect search findings and fixes

1. **M3-A `no_stream_*` test contradicted the new surface** — it asserted
   `stream`/`ReadableStream` are `undefined`. Fix: renamed to
   `no_filereader_or_dom_globals_appear` and asserts the M3-B surface
   exists while FileReader/DOM stay absent.
2. **`RefCell` double borrow in `reader_read`** — `pending.push_back`
   borrowed the cell while `shared.borrow().mode` was live. Fix: copy the
   `Copy` mode first, then push (would have panicked at runtime).
3. **Dead error-drain helper** — the first `pump_one` draft dropped queued
   resolvers without settling them. Fix: terminal error state replays per
   request in each request's own job; the placeholder was deleted.
4. **Stale decoder draft buffered invalid tails** — the first
   `incomplete_tail_len` treated lone continuations as incomplete.
   Fix: strict `is_valid_utf8_prefix` (only real leader prefixes buffer;
   max 3 bytes); invalid tails replace immediately via `from_utf8_lossy`.
5. **`chunk_size` mapping was `RangeError`** — order practice maps
   `ResourceLimit` to `RangeError`, but a bad chunk size is host
   misconfiguration, and the M3-B integration test pins `TypeError`.
   Fix: `create_stream` maps any `reader()` failure to synchronous
   `TypeError`; the test covers 0/16KiB-1/1MiB+1 and all three good bounds.
6. **Reaction chains need multiple `run_jobs()` passes** — FIFO/cancel
   assertions read `.then` side effects, which settle on later Boa passes
   than the chunk jobs. Fix: tests poll the JS-observable verdict across
   passes (no timers); production code unchanged.
7. **`cargo hack` found a dead field/statement** — the no-default-features
   combination flagged `streams_shim` unread and an unreachable branch.
   Fix: single `cfg!`-based availability condition keeps both live in every
   configuration; `cargo hack` is green.

## Step C — Independent audit notes

- **Dependency graph**: no new dependencies (`encoding_rs` deliberately
  not added; ADR-0016). `cargo deny check` exits 0 with
  `wildcards = "deny"` intact.
- **Core isolation**: `boa_fapi_core` has no Boa/JS/DOM/streams-shim/
  `Context`/`JsValue`/URL/fs/thread/runtime references (guards green);
  `BlobReader` takes no token/limits per call — chunk ceiling is
  snapshotted at `reader()` time.
- **Public APIs**: `BlobData` exposes exactly 11 methods
  (`blob_data_public_api_is_fixed` green); `BlobReader` exactly
  `read_next`/`cancel` (`blob_reader_public_api_is_fixed` green);
  bindings expose no new public Rust items (module scan covers
  `streams.rs`); `lib.rs` re-exports unchanged.
- **Reader arithmetic/allocation/exactness/O(chunk)**: chunk length is
  `min(chunk_size, remaining)`; per-segment `checked_add`, exact
  `chunk.len()` match, `checked_add` + capacity bound before append,
  release-enforced totals; one fallible `try_reserve_exact(chunk)` per
  call; no `materialize`, no readahead, no `as` on unbounded values.
- **GC/job captures**: `StreamShared` holds reader/decoder/queues/flags
  only; resolvers travel in each `PromiseJob` closure as traced
  `JsFunction`s; `StreamNative`/`ReaderNative` use
  `#[unsafe_ignore_trace]` on the `Rc` (no GC pointers inside);
  `#![deny(unsafe_code)]` holds; jobs never call `run_jobs()`.
- **Registration rollback**: build → preflight (5 names + extensibility)
  → install → rollback covers all four globals; `StreamsShimDisabled`
  precedes any mutation; re-registration rule (b) unchanged.
- **Brands/descriptors**: native-data brands only; direct `new` on both
  shim constructors is `TypeError`; methods writable/non-enumerable/
  configurable with exact `length`; `locked` is an enumerable/
  configurable getter; tags are `ReadableStream` /
  `ReadableStreamDefaultReader`.
- **Demand/FIFO**: `stream()`/`getReader()` read nothing; each `read()`
  enqueues exactly one job pumping exactly one `read_next()`; FIFO is the
  `VecDeque` order; terminal states settle without source reads.
- **Cancel/release/error**: cancels are idempotent and isolated;
  locked-stream cancel rejects `TypeError` via job; queued reads resolve
  done on cancel; `releaseLock` throws with queued reads and releases
  otherwise; errored streams replay plain-`Error` rejections without new
  reads (child-module JS-realm proof).
- **Decoder boundaries**: buffered prefix is at most 3 bytes of a strict
  leader prefix; complete input decodes via `from_utf8_lossy`; EOF flush
  emits one U+FFFD per leftover byte; empty flush never yields an empty
  `done:false` chunk; two streams never share decoder state.
- **Scope/docs/CI order**: no pipe/tee/BYOB/controller/strategy/
  transform/writable/decoder-ctor/reader-ctor/event/DOM member exists
  (bounded-surface guard + JS absence test); README/architecture state the
  bounded shim with explicit `run_jobs()` and absent full-Streams/M4/fs;
  CI runs the M3-B test after M3-A on both OS jobs.

## M3B-rework (R1–R3)

- **R1 root cause**: `PendingRead` stored `ResolvingFunctions` (`JsFunction`s)
  inside `StreamShared`, reachable through `#[unsafe_ignore_trace]`
  `Rc<RefCell<..>>` — pending resolvers were invisible to the GC while the
  `PromiseJob` captured only the `Rc`. Fix: shared cell keeps only Rust
  data (`reader`, `decoder`, `mode`, flags, `pending: usize` count,
  `next_seq`); each request is a `{seq, mode}` key and its resolvers live
  only in that request's `PromiseJob` closure, which the engine traces
  until settlement. Regression: `pending_read_survives_gc_with_exact_chunk`
  (stream+reader+pending read in JS globals, `boa_gc::force_collect()`
  before `run_jobs()`, exact chunk), `two_queued_reads_survive_gc_fifo_exact`
  (FIFO + exact chunks after GC), `gc_then_cancel_and_error_paths_settle`
  (pending+cancel and terminal error after GC, no lost resolver/panic/extra
  read).
- **R2 root cause**: a text chunk fully absorbed into the decoder's pending
  prefix resolved `{value: "", done: false}`, violating §4.4. Fix: `pump_one`
  coalesces whole chunks inside the current text request (`read_next_chunk`
  loop) until non-empty text, EOF flush, or error; memory stays
  O(chunk)+≤3 decoder bytes and the next request never starts early.
  Regression: `text_stream_decodes_split_multibyte_without_early_replacement`
  (16 KiB-1 ASCII + split emoji: first result non-empty, exact join, no
  empty `done:false`), plus the every-boundary/invalid-flush tests.
- **R3 root cause**: `FileApiLimits::validate()` accepted chunk sizes
  1..16383 and >1 MiB; the range was enforced only late in
  `BlobData::reader`. Fix: `validate()` rejects
  `default_chunk_size ∉ 16 KiB..=1 MiB` with existing typed
  `ResourceLimit(MaterializeBytes)` (single check reused by `reader`);
  `register` fail-fasts the structural bound before any global mutation.
  Regression: `chunk_size_below_minimum_rejected`,
  `chunk_size_above_maximum_rejected`, `chunk_size_bounds_accepted`,
  `invalid_limits_reject_registration_before_any_global`,
  `chunk_size_config_bounds_rejected` (bounds stream exact bytes).
- **Rework audit**: GC captures (resolvers only in traced job closures;
  shared cell provably GC-pointer-free), FIFO/cancel after GC, UTF-8
  boundary coalescing, limits validation at `validate()` + `register`,
  guards (core API fixed, bounded shim surface, no M4), scope and truthful
  docs re-verified; no new defects found.

## Step D — Final validation (post-fix)

All work order §7 commands re-run after the last production change; every
command exited 0. Exact commands, exit codes and the coverage table are
recorded in `docs/m3b-validation.md`.

## Audit conclusion

- All M3-STREAM-01..09 requirements verified with code and test evidence.
- 7 defects found during the audit pass and fixed with regression tests.
- M3B-rework: 3 findings (R1–R3) fixed with regression tests above.
- 267 workspace tests pass; boa_fapi line coverage 89.86% (threshold 85%).
- No masked failures, skips, exclusions, or changed acceptance criteria.
