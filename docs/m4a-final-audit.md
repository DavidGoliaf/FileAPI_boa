# M4-A Final Audit (bug-find pass + rework re-audit)

Performed after the first complete green local run, before the
retrospective and commit, and repeated after the rework fixes in
`docs/reviews/M4A-rework.md` (findings R1–R6 below). Each requirement
cites exact `file:symbol`, evidence (normal/error/boundary/race), and
verdict. Findings were fixed, affected validation repeated, and evidence
updated until no unresolved item remained.

## A. `M4-DOM-*` rows

- **M4-DOM-01** — `extension.rs:FileApiExtension::register`,
  `error.rs:RegisterError::DomShimDisabled`.
  Evidence: `m4_filereader_async.rs::dom_shim_disabled_fails_before_global_mutation`
  (typed error, no globals), `::non_extensible_global_rejects_dom_atomically`,
  `::each_new_global_conflicts_atomically` (all five names, rollback).
  Verdict: PASS.
- **M4-DOM-02** — `dom.rs:build_dom_specs`, `filereader.rs:build_filereader_specs`.
  Evidence: `::dom_globals_have_exact_descriptors_and_prototypes`,
  `::dom_constructors_have_exact_name_and_length`,
  `::filereader_surface_descriptors_and_initial_state`,
  `::filereader_constants_are_readonly`. Verdict: PASS.
- **M4-DOM-03** — `dom.rs:add_event_listener`, `remove_event_listener`,
  `dispatch_event`, `invoke_event`.
  Evidence: `::event_target_listener_dedupe_removal_order_and_isolation`
  (tuple dedupe incl. capture flag, order, exception isolation with
  synchronous rethrow after all listeners). Verdict: PASS.
- **M4-DOM-04** — `dom.rs:event_*_getter`, `event_prevent_default`,
  `event_stop_immediate`.
  Evidence: `::event_and_progress_event_attributes_are_exact` (identity of
  target/currentTarget, flags, return of `dispatchEvent`, cancelable
  semantics). Verdict: PASS.
- **M4-DOM-05** — `dom.rs:dom_exception_constructor`, `map_core_error`,
  `construct_exception`.
  Evidence: `::dom_exception_names_and_error_inheritance` (all seven names,
  `instanceof` both, defaults), plus migrated `promise_read::tests::*` and
  `streams::tests::*` (same-realm `DOMException` with `Error` inheritance).
  Verdict: PASS.

## B. `M4-FR-*` rows

- **M4-FR-01** — `filereader.rs:filereader_constructor`,
  `init_filereader_prototype`, `init_filereader_constants`.
  Evidence: surface/descriptor/initial-state tests + brand checks
  (`::filereader_construction_and_receiver_brand_checks`: `new`-only,
  borrowed/forged receivers, missing/non-Blob args leave state untouched).
  Verdict: PASS.
- **M4-FR-02** — `filereader.rs:read_as_array_buffer`, `finish_at_eof`.
  Evidence: `::read_as_array_buffer_is_exact_and_fresh` (empty, exact,
  NUL/high-byte, composed/sliced/File, fresh backing, independence).
  Verdict: PASS.
- **M4-FR-03** — `filereader.rs:read_as_binary_string`.
  Evidence: `::read_as_binary_string_preserves_nuls_and_high_bytes`.
  Verdict: PASS.
- **M4-FR-04** — `filereader.rs:read_as_text`, `IncrementalDecoder`,
  `resolve_label`.
  Evidence: `::read_as_text_utf8_bom_replacement_and_split_boundaries`
  (UTF-8/BOM/replacement, 16 KiB-edge split €, windows-1252,
  unknown-label `EncodingError` fail-fast with DONE+error synchronously).
  Verdict: PASS.
- **M4-FR-05** — `filereader.rs:read_as_data_url`, `finish_at_eof`.
  Evidence: `::read_as_data_url_exact_packaging` (four exact cases, no
  whitespace), `::data_url_quota_boundary` (`==limit` ok / `+1` fail-fast
  `QuotaExceededError`). Verdict: PASS.
- **M4-FR-06** — `filereader.rs:run_pump`, `run_dispatch`,
  `dispatch_event_now`.
  Evidence: `::empty_blob_full_event_sequence_is_exact` (exact 4-event
  order with readyState/target/flags per handler),
  `::multichunk_sequence_has_final_progress_before_load` (final progress
  `loaded=total` precedes `load`),
  `::progress_throttle_uses_injected_clock` (frozen clock → 1
  intermediate + final; single-chunk slow-chunk exception). One pump uses
  exactly one clock sample (`finish_at_eof` takes the pump tick; rework
  finding 4). Verdict: PASS.
- **M4-FR-07** — `filereader.rs:start_read`, `run_dispatch`.
  Evidence: `::second_read_while_loading_throws_invalid_state`
  (same-realm `InvalidStateError`, first read intact),
  `::reentrant_load_starts_new_read_and_suppresses_old_loadend` (exact
  8-event log; old `loadend` suppressed, new operation intact),
  `::reentrant_error_handler_starts_new_read` (reentrant `error` case),
  `::abort_handler_restart_after_mid_chunk_abort` (reentrant `abort`
  case). Verdict: PASS.
- **M4-FR-08** — `filereader.rs:abort`, generation guards, `QueueHolder`.
  Evidence: `::abort_before_first_job_emits_only_abort_loadend`,
  `::abort_between_chunks_suppresses_stale_events`,
  `::abort_in_empty_or_done_state_is_silent` (EMPTY + DONE),
  `::stale_completion_after_new_operation_is_noop` (abort→new read: stale
  jobs emit nothing, new result intact),
  `::gc_survives_queued_filereader_jobs` (`force_collect()` before jobs),
  plus `filereader::tests::loadstart_abort_performs_no_source_read`
  (zero post-abort source reads),
  `::loadstart_abort_then_restart_emits_only_new_operation`,
  `::progress_abort_freezes_source_reads` (reads frozen at one chunk).
  Verdict: PASS.
- **M4-FR-09** — `filereader.rs:fail_operation`, `fail_fast`,
  `release_slot`.
  Evidence: quota test `::concurrent_read_quota_recovers_after_success_error_abort`
  (64 ok, 65th `SecurityError` via normal path, recovery),
  `::data_url_quota_boundary`, plus
  `filereader::tests::short_source_response_fails_as_not_readable_error`,
  `::long_source_response_fails_as_not_readable_error`,
  `::failing_source_fails_as_not_readable_error` (each: DONE, null
  result, `NotReadableError`, exactly `error` then `loadend`, quota
  recovery under single-slot limits) and
  `::error_handler_restart_suppresses_old_loadend` (reentrant error).
  Verdict: PASS.
- **M4-FR-10** — `promise_read.rs:reject_with`,
  `streams.rs:stream_error_reason`, guards.
  Evidence: `::m3_promise_rejections_are_dom_exceptions_with_fixed_mapping`,
  `::bounded_operation_sequences_match_pure_model` (162-scenario
  enumerated corpus vs pure model, real JS per scenario),
  `::excluded_m4b_apis_are_absent`;
  `guards::filereader_and_dom_surface_is_bounded`,
  `::no_filereader_sync_or_out_of_scope_surface`. Verdict: PASS.

## C. Re-reads (registration, descriptors, GC, jobs)

- Registration/rollback re-read: build → limits `validate()` → DOM +
  FileReader specs → preflight (extensibility + all 10 names) → install →
  rollback on failure; marker inserted last. No path installs partial
  globals. — PASS.
- Descriptors/brands re-read: every new method/getter/constant checked
  against Web IDL attributes by tests; forged receivers (`{}`,
  `Object.create(proto)`, cross-brand) all give synchronous `TypeError`. —
  PASS.
- GC traces re-read: `EventTargetNative.listeners` (`Vec<ListEntry>` with
  `JsFunction`) and `FileReaderNative.listeners` are traced via derive;
  `EventNative`/`ProgressEventNative` targets are `Option<JsObject>`
  (traced); `result`/`error`/`PumpState` payloads are plain Rust data
  (`#[unsafe_ignore_trace]`); jobs own the reader `JsObject` by value in
  the capture. `force_collect()` probes green. No untraced JS data in
  native state. — PASS.
- Job captures re-read: every `enqueue_reading_job` closure owns
  `FileReadingJob` (reader + generation + owned pump/dispatch state) and
  the realm; no `Context`, no borrowed data, no `run_jobs()` inside jobs,
  no JS called from source completion (events dispatch inside jobs or
  queued dispatch jobs). Stale generations checked at pump, finish, fail,
  and every dispatch entry. — PASS.

## D. Event ordering re-check

- Empty/multichunk/error/abort/reentrant cases re-run: first/last/final
  progress exact; 50 ms boundaries exact on step clocks; source error path
  (`fail_operation`) carries no partial result; all four packagers with
  checked arithmetic (Data-URL double-checked at start and finish). —
  PASS.

## E. Test quality

- Mutation spot-checks (verified by temporarily breaking the
  implementation and re-running the suites):
  - `decode_to_string` with zero-capacity output produced empty text —
    caught by the first text test, fixed with `with_capacity`.
  - Generic-job queue dropped `loadstart`/`progress` at the LOADING gate
    (successor finished first) — caught by event-order tests, fixed by
    synchronous in-job dispatch for non-terminal events.
  - `timeStamp` clock reads perturbed the throttle — caught by the step
    clock, fixed by reusing the pump tick.
  - Removing the LOADING guard fails
    `bounded_operation_sequences_match_pure_model` on the missing
    `throw:InvalidStateError` comparison — the corpus is not vacuous.
  - Removing the `loadstart` generation recheck fails
    `filereader::tests::loadstart_abort_performs_no_source_read` and
    `::loadstart_abort_then_restart_emits_only_new_operation` on the
    source-read counter — the stale-race tests are not vacuous.
  - Each fix re-ran the affected suites green.
- No test inspects private state or calls implementation helpers: all
  assertions read JS-visible objects/events after real `run_jobs()`
  (the counting sources observe only how many times the implementation
  calls the public `ByteSource::read_range` contract). —
  PASS.

## F. Out-of-scope / forbidden / stale-claim sweep

- `FileReaderSync`, workers, `fs`/paths, blob URLs, structured clone, URL
  shim, full DOM/HTML, WPT harness: absent (negative guards green;
  `extension.rs` wiring is the only allowed `FileReader` mention outside
  the two new modules). — PASS.
- Forbidden escapes: no `unsafe`, no `unwrap`/`expect`/`panic` in
  production (guards green), no `#[ignore]`/reduced corpus, no blanket
  allow. — PASS.
- Stale M3 claims: `spec-matrix.md` M3-READ-06/M3-STREAM-07 updated to the
  DOMException mapping; `architecture.md` promise/stream sections updated;
  old test names (`*_with_range_error`, `*_plain_error_*`) renamed to the
  mapped assertions. — PASS.
- False evidence: validation/audit/handoff state only actual final data
  (commands + exit codes above; coverage 89.79% lines); local and
  independent `cargo deny check` attempts are BLOCKED before advisory
  evaluation by the unavailable RustSec database fetch; final CI is
  recorded with the verified workflow and OS job links; no TODO/FIXME
  added. — PASS.

## G. Findings and fixes (all resolved)

1. `decode_to_string` requires spare capacity — fixed with
   `with_capacity`; text tests green.
2. Generic-queue FIFO dropped queued non-terminal events — fixed with
   synchronous in-job `loadstart`/`progress` dispatch; event-order tests
   green.
3. `create_progress_event` clock reads perturbed throttle — fixed by
   passing the pump tick as `time_stamp`; step-clock tests green.
4. `encoding_rs` BOM sniffing vs fail-fast unknown labels — fixed with
   `for_label_no_replacement` + manual single UTF-8 BOM strip; label
   tests green.
5. `cargo deny` rejected `BSD-3-Clause` — fixed by allow-listing it
   (+ ADR-0020 note); the current local check is blocked before advisory
   evaluation by the database fetch, while the final CI workflow is green
   (see §F).
6. `cargo hack` powerset failed without `dom-shim` — fixed with
   `cfg`-gated pre-M4 fallbacks in `promise_read.rs`/`streams.rs`;
   powerset green.
7. Docs build linked a private module — fixed doc wording; rustdoc green.

## H. Rework findings R1–R6 (`docs/reviews/M4A-rework.md`, all resolved)

1. Truthful validation evidence — `docs/m4a-validation.md`,
   `docs/m4a-final-audit.md` (§F, this section), and
   `docs/reviews/M4A-handoff.md` record both local `cargo deny check`
   attempts as BLOCKED with the exact advisory-DB fetch reason; they do not
   claim a local advisory result, and now record the owner-verified final CI
   workflow and both OS job links.
2. README updated — installed M4-A surface, explicit
   `context.run_jobs()` contract with examples, memory-backed-only
   boundary, `dom-shim` capability, and explicitly omitted M4-B
   features; no full-DOM/filesystem/`FileReaderSync`/blob-URL/clone/WPT
   claims (also removed a duplicated M1–M3-A bullet block).
3. Stale-generation read race closed — `filereader.rs:run_pump`
   rechecks the generation after `loadstart` and `progress` dispatches
   and before source reads, successor enqueue, packaging, slot release,
   and event emission; `finish_at_eof` rechecks after the final-progress
   dispatch. Covered by `filereader::tests::loadstart_abort_performs_no_source_read`
   (zero post-abort source reads),
   `::loadstart_abort_then_restart_emits_only_new_operation`,
   `::progress_abort_freezes_source_reads`,
   `::error_handler_restart_suppresses_old_loadend`, plus the
   integration reentrant `error`/`abort` tests.
4. Extra Clock read removed — `finish_at_eof` takes the pump's `now`
   tick; one pump uses exactly one clock sample. The throttle tests
   guard the contract (a second sample would shift the step-clock
   schedule and fail the exact progress counts).
5. Source-failure coverage added — `filereader::tests` proves
   JS-visible `DONE`/`null`/`NotReadableError`, exactly
   `error`-then-`loadend`, no partial result, and quota recovery for
   short, long, and failing sources (unreachable via public
   constructors, hence the child-module pattern already used by
   `promise_read::tests`/`streams::tests`; no production hook), plus
   reentrant `error`/`abort` cases; the integration suite adds
   `reentrant_error_handler_starts_new_read` and
   `abort_handler_restart_after_mid_chunk_abort` through public
   triggers.
6. Real property/model test —
   `m4_filereader_async.rs::bounded_operation_sequences_match_pure_model`
   now runs a 162-scenario enumerated corpus (9 sync prefixes × 3
   follow-ups × 6 handler modes) with real JS per scenario against a
   pure generation/terminal model; every comparison is exact, and the
   corpus provably covers all terminal kinds, sync throws, replacement,
   and stale completions. Mutation probes above confirm the assertions
   fail on divergence.

No unresolved items remain.
