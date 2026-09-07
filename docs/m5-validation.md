# M5 validation

Date: 2026-09-07 (rework after R1–R3 review). Branch `task/m5`, base `f5404de6a105c52dc128e686e18d92b376603bd4`.
Local platform: Windows (weak-identity target — live-handle tests are Unix-only by construction).

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 58+25+8+20+16+19 core, 9 fs, 1 doc) |
| 4 | `cargo test --package boa_fapi_fs --all-features -- --nocapture` | 0 | PASS (9 on Windows: refusal + copy (consume semantics, `ResourceLimit` on fresh slot) + close/closer + deny tests; Unix live tests `#[cfg(unix)]`, run in Linux CI) |
| 5 | `cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture` | 0 | PASS (51) |
| 6 | `cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture` | 0 | PASS (16) |
| 7 | `cargo test --package boa_fapi --test m3_blob_streams -- --nocapture` | 0 | PASS (28) |
| 8 | `cargo test --package boa_fapi --test m4_filereader_async -- --nocapture` | 0 | PASS (33) |
| 9 | `cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture` | 0 | PASS (21) |
| 10 | `cargo test --package boa_fapi --test m5_file_fs -- --nocapture` | 0 | PASS (15 on Windows: copy-path integration + refusal + shutdown-handle proofs; Unix live tests `#[cfg(unix)]`, run in Linux CI) |
| 11 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 12 | `cargo test --package boa_fapi --doc` | 0 | PASS (1) |
| 13 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (TOTAL 85.29% lines) |
| 14 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (11/11 incl. `fs` on/off) |
| 15 | `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; local DB fetch succeeded, network available) |
| 16 | `git diff --check` | 0 | PASS |

## R1–R3 rework evidence

- R1 (shutdown drops handles): `fs_tests::close_removes_slot_and_drops_handle`,
  `::close_all_drops_every_handle`, `::shutdown_closers_run_once_outside_lock`
  (all platforms) + `m5_file_fs.rs::shutdown_releases_live_handles` (unix) —
  `live_slot_count` reaches 0 at shutdown.
- R2 (enforced copy-or-deny): `fs_tests::weak_platform_direct_import_is_refused`
  (all platforms) + `m5_file_fs.rs::weak_platform_copy_reports_no_location_detail`
  (not-unix) + `RegistryPolicy`/`FileSource::new`/`file_from_resource` gates.
- R3 (no lock across I/O): `capability.rs::cloned_handle` (`try_clone` under a
  short lock; Unix positional reads / independent cursor elsewhere) — covered by
  every fs/fs-JS test executing reads; no direct lock-hold probe exists by
  construction (the mutex is private), the guarantee is structural + clippy-clean.

## Trace rows

See `docs/spec-matrix.md` M5-FS-01..M5-FS-10 with exact `file:symbol`, test names, and commands.

## Coverage

`cargo llvm-cov --package boa_fapi --all-features` TOTAL 85.29% lines (threshold 85%).
`boa_fapi_fs` units: 9 tests on Windows (Unix live tests run in Linux CI);
`boa_fapi` M5-JS integration: 15 tests on Windows (Unix live tests run in Linux CI).
M2/M3/M4-A/M4-B regression suites green (memory behavior unchanged).

## Post-review fixes R4–R5

- R4 (copy handle lifecycle): `open_copy_on_import` closes the live handle
  on **every** exit (success, `max_bytes` refusal, allocation failure,
  read error) via the `open_copy_inner` + unconditional `close` wrapper;
  each copy consumes its registration (re-register for another copy).
- R5 (+1 test honesty): `copy_bounds_and_content_everywhere`,
  `copy_on_import_bounds_and_content`, and
  `failed_copy_releases_handle_on_every_path` re-register a fresh slot
  before the `+1` probe and assert `Err(ResourceLimit(_))` plus
  `live_slot_count() == 0` — the old second-call-`NotFound` shape is gone.

## CI

- Final code commit: `11c8675743d37b77c3520fa3d91731afc7fa341d`.
- CI run: https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34125651525
  — **success** on Ubuntu + Windows (full order §9 sequence on the final SHA).
- Unix-only tests (`#[cfg(unix)]` live-handle/snapshot tests) executed in the
  Ubuntu job; Windows executed the refusal + copy-fallback tests.
