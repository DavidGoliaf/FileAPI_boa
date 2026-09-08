# M4-B Handoff — worker-only `FileReaderSync`

## What was built

- Environment capability: public `FileApiEnvironment` (`Window` default,
  `DedicatedWorker`, `SharedWorker`, `ServiceWorker`) with explicit
  `FileApiExtensionBuilder::environment()`; stored in the registered
  specs and handle (`FileApiHandle::environment()`), never inferred.
  Only worker descriptors build, preflight, install, and roll back the
  single `FileReaderSync` global (atomic, same fail-fast contract);
  `Window`/`ServiceWorker` leave the name untouched.
- Sync binding: new `crates/boa_fapi/src/filereader_sync.rs` — stateless
  brand, `new`-only constructor, exactly four prototype methods
  (`length` 1 each) plus the tag; no state, no `abort`, no handlers, no
  Promise/EventTarget/jobs. Fixed preflight order (brand → argument →
  label → sync-size → read), `size > max_sync_read_bytes` as
  `QuotaExceededError`, checked data-URL length before reads/allocation,
  bounded `BlobData::materialize`, central `DOMException` mapping, no
  partial result, async quota untouched.
- Shared packaging: new private `crates/boa_fapi/src/package.rs`
  (`TextEncoding`/`resolve_label`/`IncrementalDecoder`/`decode_text`/
  `package_binary_string`/`data_url_len`/`package_data_url`) used by both
  readers; the async `filereader.rs` was refactored onto it with zero
  M4-A test edits and a fully green M4-A suite.
- Tests: `crates/boa_fapi/tests/m4_filereader_sync.rs` (21 tests covering
  all 10 §7 groups, incl. capability-absent name preservation and the
  throwing-label preflight-order regression test); `filereader_sync::tests` (5 child-module proofs:
  short/long/failing sources, zero-read preflight, packaging parity);
  guards (`sync_surface_is_bounded`, `no_out_of_scope_surface`, updated
  re-export list).
- Docs/trace: `M4B-FRS-01..08` in `docs/spec-matrix.md`; ADR-0022/0023;
  README + `docs/architecture.md` (worker-only capability, descriptor,
  memory-only boundary, omitted M5+); CI runs the M4-B test after M4-A
  on both OS.
- Deps: none added.

## Base / commit

- Base: M4-A final commit `88fe487`; branch `task/m4b`.
- Implementation + validation/audit results in this handoff describe the
  final commit of this branch (resolved via `git log`); no production
  code, tests, or validation results changed after the green run except
  this handoff file.

## Demo commands (work order §9, in order)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture
cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
cargo test --package boa_fapi --test m4_filereader_async -- --nocapture
cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

All local commands pass except `cargo deny check`, which exits 1 because
the advisory database cannot be fetched in this environment; recorded in
`docs/m4b-validation.md` (coverage 89.95% lines). Green CI never rewrites
this local result. Final audit trace and findings:
`docs/m4b-final-audit.md`.

## Matrix / ADR

- Matrix: `M4B-FRS-01..08` with `file:symbol` and test names; M4-FR-10
  row updated to the renamed guards. No existing rows replaced.
- ADR-0022: worker environment descriptor and sync registration.
- ADR-0023: shared sync/async packaging boundary (no new dependencies).

## Coverage

- Workspace: 352 tests green (42 boa_fapi unit incl. 5 sync, 13 guards,
  51 M2 JS integration, 33 M4-A JS integration, 21 M4-B JS integration,
  28 M3-B JS integration, 16 M3-A JS integration, 1 doc, 146 core);
  `boa_fapi` line coverage 89.95% (threshold 85%, see
  `docs/m4b-validation.md`). All M4-A tests green without edits.

## CI

CI (verified via the GitHub API): run `34101779438` (`CI #11`, push of
`a17c3d8` on `task/m4b`) completed with conclusion `success` —
`M4-B validation (windows-latest)` job `101677622816` green and
`M4-B validation (ubuntu-latest)` job `101677622527` green, every step
green on both OS:
[run](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34101779438),
[Windows job](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34101779438/job/101677622816),
[Ubuntu job](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34101779438/job/101677622527).

## Findings / fixes

See `docs/m4b-final-audit.md` §G (6 items, all resolved): verbatim
packaging extraction with M4-A regression proof, two powerset `cfg`
fixes, the `progress:5` test-expectation fix, the re-export guard
update, and the guard rename with matrix update. Each fix re-ran the
affected suites green; the audit was repeated until no unresolved item
remained.

Acceptance-blocker fixes (post-`51e6eda`, all resolved): the
`readAsText` label conversion moved after the brand/argument checks with
the `throwing_label_is_converted_after_brand_and_argument_checks`
regression test; `docs/spec-matrix.md` M4-B rows rewritten without
control characters or wrapped lines (`git diff --check` exit 0);
`docs/DECISIONS.md` trailing blank line removed; `cargo deny check`
recorded as BLOCKED with the exact local-run reason while the verified CI
run `34101779438` (success on both OS) is recorded with URLs.

## Explicitly omitted (separate future orders)

`FileReaderSync` in window/service workers, filesystem-backed sources,
snapshot validation, blob URLs, structured clone, full DOM/Workers
runtime, WPT harness, M5. The negative guards
(`window_and_service_worker_never_expose_sync`,
`no_filesystem_url_clone_or_full_dom_surface`,
`guards::no_out_of_scope_surface`) prove their absence.

## Deviations

None. No changes to `TZ_boa_fapi_FileAPI.md`, the M4-B order,
thresholds, CI workflow semantics beyond the appended M4-B test step and
the M4-B job rename, trace IDs, or acceptance criteria.
