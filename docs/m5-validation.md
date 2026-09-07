# M5 validation

Date: 2026-09-07. Branch `task/m5`, base `f5404de6a105c52dc128e686e18d92b376603bd4`.

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 58+25+8+20+16+19 core, 17 fs, 1 doc) |
| 4 | `cargo test --package boa_fapi_fs --all-features -- --nocapture` | 0 | PASS (17) |
| 5 | `cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture` | 0 | PASS (51) |
| 6 | `cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture` | 0 | PASS (16) |
| 7 | `cargo test --package boa_fapi --test m3_blob_streams -- --nocapture` | 0 | PASS (28) |
| 8 | `cargo test --package boa_fapi --test m4_filereader_async -- --nocapture` | 0 | PASS (33) |
| 9 | `cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture` | 0 | PASS (21) |
| 10 | `cargo test --package boa_fapi --test m5_file_fs -- --nocapture` | 0 | PASS (15) |
| 11 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 12 | `cargo test --package boa_fapi --doc` | 0 | PASS (1) |
| 13 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (TOTAL 87.63% lines; `streams.rs` lowest at 81.01% but the gate is the package total) |
| 14 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (11/11 incl. `fs` on/off) |
| 15 | `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; local DB fetch succeeded, network available) |
| 16 | `git diff --check` | 0 | PASS |

## Trace rows

See `docs/spec-matrix.md` M5-FS-01..M5-FS-10 with exact `file:symbol`, test names, and commands.

## Coverage

`cargo llvm-cov --package boa_fapi --all-features` TOTAL 87.63% lines (threshold 85%).
`boa_fapi_fs` units: 17 tests; `boa_fapi` M5-JS integration: 15 tests.
M2/M3/M4-A/M4-B regression suites green (memory behavior unchanged).
