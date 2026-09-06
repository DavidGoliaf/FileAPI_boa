# M1 Validation Report

## Environment

- **Date**: 2026-09-06
- **Toolchain**: rustc 1.91.0 (edition 2024)
- **OS**: Windows (win32)
- **Shell**: PowerShell 7+

## Validation Commands

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (116 tests) |
| 4 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 5 | `cargo test --package boa_fapi_core --doc` | 0 | PASS (0 doc tests) |
| 6 | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` | 0 | PASS (96.09% line coverage) |
| 7 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 8 | `cargo deny fetch db` + `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok) |
| 9 | `git diff --check` | N/A | BLOCKED — not a Git repository; all other checks pass |

## Coverage Summary

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover
---------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           198                18    90.91%          11                 1    90.91%         146                11    92.47%
cancellation.rs                    11                 0   100.00%           3                 0   100.00%          11                 0   100.00%
endings.rs                         29                 0   100.00%           1                 0   100.00%          20                 0   100.00%
limits.rs                          45                 0   100.00%           2                 0   100.00%          68                 0   100.00%
mime.rs                            11                 0   100.00%           1                 0   100.00%           7                 0   100.00%
source\memory.rs                   31                 0   100.00%           4                 0   100.00%          26                 0   100.00%
source\mod.rs                       3                 0   100.00%           1                 0   100.00%           3                 0   100.00%
---------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                             328                18    94.51%          23                 1    95.65%         281                11    96.09%
```
