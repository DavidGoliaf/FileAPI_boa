# M4-B Final Audit (retrospective bug-find pass)

Performed after the final green local run, before commit, per `AGENTS.md`
and work order §10. Each area cites exact `file:symbol`, evidence, and
verdict. Findings were fixed, affected validation repeated, and evidence
updated until no unresolved item remained.

## A. Scope boundaries

- `extension.rs:FileApiEnvironment` + `register`/`install_globals`/
  `rollback_globals`: only `FileReaderSync` is added, and only for
  worker descriptors. No filesystem, URL, clone, full-DOM, WPT, or M5
  code exists (negative guards green:
  `guards::no_out_of_scope_surface`, `::sync_surface_is_bounded`).
  `TZ_boa_fapi_FileAPI.md`, M1–M4-A orders, matrix IDs, and thresholds
  untouched. — PASS.
- `filereader_sync.rs` surface is exactly the four normative methods
  (integration `::sync_prototype_has_only_four_methods` + guard scan).
  No `FileReaderSync` in `Window`/`ServiceWorker`, not even an
  `undefined` shim (capability matrix tests). — PASS.

## B. Panic / unsafe / unwrap / expect

- No `unsafe`, raw pointers, or fabricated references in the diff
  (`#![deny(unsafe_code)]` + `production_source_no_unwrap_expect_panic`
  green, covering the new modules). No background threads, Tokio,
  executors, or JS calls from source completion
  (`no_out_of_scope_surface` scans `tokio`/`std::thread`). — PASS.
- Fallible paths use checked arithmetic (`package::data_url_len`),
  `try_from`, and `JsResult` propagation; `materialize` reserves
  fallibly in core. No allocation before preflight. — PASS.

## C. Generation / source-read races and quota

- Sync reads hold no async quota and run no jobs, so no generation or
  race surface exists by construction; `sync_reads_do_not_consume_async_quota`
  proves 64 async slots usable alongside sync reads with the 65th still
  rejected. — PASS.
- Preflight order is fixed and documented
  (`read_bytes_sync`: brand → argument → label → sync-size → read);
  `rejected_sync_preflight_performs_zero_source_reads` proves zero
  `read_range` calls on rejection. — PASS.
- Async `filereader.rs` was touched only by the `package.rs`
  extraction (mechanical move + helper call sites); the full M4-A suite
  passes with zero test edits, proving no semantic divergence. — PASS.

## D. Descriptors, brands, rollback

- Constructor `name`/`length`/descriptors/tag exact
  (`::sync_surface_descriptors_and_tags`); `new`-only with sync
  `TypeError`; borrowed/forged receivers and bad arguments give sync
  `TypeError` (`::sync_construction_and_receiver_brand_checks`); stateless
  native brand unforgable from JS. — PASS.
- Rollback removes `FileReaderSync` only when the worker capability
  would have installed it (`rollback_globals` flag;
  `::sync_name_conflict_fails_atomically` proves atomicity). — PASS.

## E. Test quality and mutation probes

- Verified by temporarily breaking the implementation and re-running:
  - removing the sync-size preflight fails
    `m4_filereader_sync.rs::sync_size_limit_boundary` and
    `filereader_sync::tests::rejected_sync_preflight_performs_zero_source_reads`
    (boundary + zero-read assertions are not vacuous);
  - each fix re-ran the affected suites green before restore.
- No test inspects private state or calls implementation helpers: all
  assertions read JS-visible returns/throws after real calls; counting
  sources observe only the public `ByteSource::read_range` contract. —
  PASS.

## F. Document truthfulness

- Validation table records factual exits (deny: acceptance exit 1,
  BLOCKED, advisory DB unavailable there; the local exit 0 and the
  green CI deny steps are supporting evidence only, never a rewrite of
  the acceptance result); coverage is the measured 89.95% lines; no
  TODO/FIXME added; no `docs/security.md`, WPT, or filesystem docs
  created. Verified CI run `34100515645` for `51e6eda` (success, both
  OS, URLs in `docs/m4b-validation.md`); the current tip additionally
  awaits its own CI verification — no other run is claimed. — PASS.

## G. Findings and fixes (all resolved)

1. Shared-helper extraction risk (async divergence) — contained by
   moving code verbatim into `package.rs` and proving zero M4-A test
   edits with a fully green M4-A suite.
2. `sync_enabled` unused under `--no-default-features` powerset leg —
   fixed with `#[cfg]` gating.
3. `file_reader_sync_enabled` dead in non-`dom-shim` powerset legs —
   fixed with `#[cfg(feature = "dom-shim")]` on the method; powerset
   warning-free.
4. Test expectation bug (`progress:5` vs bare `progress` in the
   abort-restart log) — fixed to assert the real loaded value.
5. `lib.rs` re-export guard needed the new `FileApiEnvironment` member
   — guard updated, not weakened.
6. Old guard name `no_filereader_sync_or_out_of_scope_surface`
   contradicted the new legitimate surface — replaced by
   `sync_surface_is_bounded` + `no_out_of_scope_surface` with per-file
   allow rules; `docs/spec-matrix.md` M4-FR-10 row updated to the new
   names.
7. Acceptance blockers (post-`51e6eda`): `readAsText` label conversion
   ran before brand/argument checks — moved into the shared preamble
   after them, with the throwing-label regression test;
   `docs/spec-matrix.md` M4-B rows carried wrapped lines and control
   characters — rewritten cleanly (`git diff --check` exit 0);
   `docs/DECISIONS.md` trailing blank line removed; deny recorded as
   BLOCKED with the exact acceptance reason while verified CI run
   `34100515645` is recorded with URLs; counts updated to 352/21.

No unresolved items remain.
