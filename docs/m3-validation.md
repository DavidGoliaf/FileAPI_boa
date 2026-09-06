# M3-A Validation Report

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
| 3 | `cargo test --workspace --all-features` | 0 | PASS (232 tests: 22 boa_fapi unit, 10 boa_fapi guards, 51 M2 JS integration, 17 M3-A JS integration, 1 boa_fapi doc, 131 core: 48+25+7+17+16+19) |
| 4 | `cargo test --package boa_fapi --test m2_blob_file_filelist` | 0 | PASS (51 tests, all executing JS in a real Boa `Context`) |
| 5 | `cargo test --package boa_fapi --test m3_promise_blob_reads` | 0 | PASS (17 tests, every read settled via `context.run_jobs()`) |
| 6 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 7 | `cargo test --package boa_fapi --doc` | 0 | PASS (1 doc test) |
| 8 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (91.40% lines, threshold 85%) |
| 9 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 10 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db` | 0 | PASS (advisory DB fetched from GitHub) |
| 11 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok) |
| 12 | `git diff --check` | 0 | PASS (CRLF warnings only) |

## Coverage Summary (boa_fapi)

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           221                12    94.57%          10                 0   100.00%         143                 4    97.20%           0                 0         -
brand.rs                           45                 2    95.56%           4                 0   100.00%          37                 2    94.59%           0                 0         -
clock.rs                           25                 8    68.00%           3                 1    66.67%          12                 3    75.00%           0                 0         -
error.rs                           28                14    50.00%           4                 2    50.00%          19                11    42.11%           0                 0         -
extension.rs                      248                33    86.69%          19                 1    94.74%         165                14    91.52%           0                 0         -
file.rs                           157                 8    94.90%          12                 0   100.00%         114                 3    97.37%           0                 0         -
file_list.rs                      124                 7    94.35%           5                 0   100.00%          88                 6    93.18%           0                 0         -
promise_read.rs                   173                32    81.50%          13                 4    69.23%         105                16    84.76%           0                 0         -
webidl.rs                         632                71    88.77%          41                 4    90.24%         387                33    91.47%           0                 0         -
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            1653               187    88.69%         111                12    89.19%        1070                92    91.40%           0                 0         -
```

## Notes

- Every M3-A integration test registers the extension into a fresh
  `boa_engine::Context`, executes real JavaScript, and settles reads only
  via `context.run_jobs()`; the pending-then-settled order is asserted in JS.
- Core `BlobData` public API is exactly the 9 M1/M2 methods plus bounded
  `materialize` (guard `blob_data_public_api_is_fixed`); no segments,
  unbounded reads, or identity probes are public.
- `MaterializeBytes` limit rejects as `RangeError`; other read failures
  reject as plain `Error` (no `DOMException` before M4).
- No new dependencies were added for M3-A.
