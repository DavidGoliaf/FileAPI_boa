# M3-B Validation Report

## Environment

- **Date**: 2026-09-06
- **Toolchain**: rustc 1.91.0 (edition 2024)
- **OS**: Windows (win32)
- **Shell**: PowerShell 7+
- **Boa**: boa_engine 0.22.0, boa_gc 0.22.0 (locked in `Cargo.lock`)

## Validation Commands (work order §7)

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (267 tests: 30 boa_fapi unit, 11 boa_fapi guards, 51 M2 JS integration, 28 M3-B JS integration, 16 M3-A JS integration, 1 boa_fapi doc, 130 core: 58+25+8+20+16+19) |
| 4 | `cargo test --package boa_fapi --test m2_blob_file_filelist` | 0 | PASS (51 tests, all executing JS in a real Boa `Context`) |
| 5 | `cargo test --package boa_fapi --test m3_promise_blob_reads` | 0 | PASS (16 tests, every read settled via `context.run_jobs()`) |
| 6 | `cargo test --package boa_fapi --test m3_blob_streams` | 0 | PASS (28 tests, every chunk settled via `context.run_jobs()`) |
| 7 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 8 | `cargo test --package boa_fapi --doc` | 0 | PASS (1 doc test) |
| 9 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (89.86% lines, threshold 85%) |
| 10 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (all feature combinations, incl. `--no-default-features`) |
| 11 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db` | 0 | PASS (advisory DB fetched from GitHub) |
| 12 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok) |
| 13 | `git diff --check` | 0 | PASS (CRLF warnings only) |

## Coverage Summary (boa_fapi)

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           225                12    94.67%          10                 0   100.00%         151                 4    97.35%           0                 0         -
brand.rs                           45                 2    95.56%           4                 0   100.00%          37                 2    94.59%           0                 0         -
clock.rs                           25                 8    68.00%           3                 1    66.67%          12                 3    75.00%           0                 0         -
error.rs                           28                 7    75.00%           4                 1    75.00%          19                 5    73.68%           0                 0         -
extension.rs                      292                37    87.33%          20                 1    95.00%         214                18    91.59%           0                 0         -
file.rs                           157                 8    94.90%          12                 0   100.00%         114                 3    97.37%           0                 0         -
file_list.rs                      124                7    94.35%           5                 0   100.00%          88                 6    93.18%           0                 0         -
promise_read.rs                   308                34    88.96%          22                 5    77.27%         209                19    90.91%           0                 0         -
streams.rs                       1038               196    81.12%          51                11    78.43%         702               103    85.33%           0                 0         -
webidl.rs                         632                71    88.77%          41                 4    90.24%         387                33    91.47%           0                 0         -
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            2874               382    86.71%         172                23    86.63%        1933               196    89.86%           0                 0         -
```

## Notes

- Every M3-B integration test registers the extension into a fresh
  `boa_engine::Context`, executes real JavaScript, and pumps the Boa job
  queue explicitly with `context.run_jobs()`; multi-pass reaction chains
  poll the JS-observable verdict (no timers, no thread ordering).
- Core `BlobData` public API is exactly the 10 M1/M2/M3-A methods plus
  `reader`; `BlobReader` exposes exactly `read_next`/`cancel` (both guards
  green). No segments, positions, sources, or identity probes are public.
- Stream core failures reject with a same-realm plain `Error` (never
  `RangeError`, never `DOMException`); chunk-size misconfiguration fails
  synchronously as `TypeError`.
- No new dependencies were added for M3-B (`encoding_rs` not needed: the
  incremental decoder is hand-written, ADR-0016).
