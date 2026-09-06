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
| 3 | `cargo test --workspace --all-features` | 0 | PASS (119 tests) |
| 4 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 5 | `cargo test --package boa_fapi_core --doc` | 0 | PASS (0 doc tests) |
| 6 | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` | 0 | PASS (95.22% line coverage) |
| 7 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 8 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db` | 0 | PASS |
| 8b | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check` | 0 | PASS |
| 9 | `git diff --check` | 0 | PASS |

## Notes

- `cargo deny` requires `CARGO_DENY_DB_PATH` set to a writable path within the project (`target/cargo-deny-advisories`) to avoid read-only default advisory DB issues. The `deny.toml` sets `db-path` accordingly.
- Coverage: 95.22% line coverage (threshold: 85%).

## Coverage Summary

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover
-------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           205                21    89.76%          12                 2    83.33%         158                14    91.14%
cancellation.rs                    11                 0   100.00%           3                 0   100.00%          11                 0   100.00%
endings.rs                         29                 0   100.00%           1                 0   100.00%          20                 0   100.00%
limits.rs                          45                 0   100.00%           2                 0   100.00%          68                 0   100.00%
mime.rs                            11                 0   100.00%           1                 0   100.00%           7                 0   100.00%
source\memory.rs                   31                 0   100.00%           4                 0   100.00%          26                 0   100.00%
source\mod.rs                       3                 0   100.00%           1                 0   100.00%           3                 0   100.00%
-------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                             335                21    93.73%          24                 2    91.67%         293                14    95.22%
```

## Rework Validation (2026-09-06, after review blockers)

Re-run of all §8 commands after the review rework (public API reduced to the M1
contract, strict workspace lints, guard scanner hardening). Same environment.

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS (workspace lints now deny `unwrap`/`expect`/`panic`) |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (120 tests: 37 `blob::tests` unit incl. property, 19 source, 25 endings, 16 mime, 17 limits, 6 guards) |
| 4 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 5 | `cargo test --package boa_fapi_core --doc` | 0 | PASS (0 doc tests) |
| 6 | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` | 0 | PASS (97.84% line coverage) |
| 7 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 8 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db` | 0 | PASS (advisory DB fetched from GitHub) |
| 8b | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; pre-existing unmatched license allowance warning unchanged) |
| 9 | `git diff --check` | 0 | PASS (CRLF warnings only) |

Note: a reviewer re-run of steps 8 and 8b previously failed with exit code 1
because the GitHub advisory DB was unreachable from their network. That is an
external network condition, not a code defect. In an available network both
steps were re-executed exactly as listed above (fetch first, then check) and
both completed with exit code 0 on 2026-09-06.

## Rework Coverage Summary

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover
-------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                          1088                25    97.70%          51                 1    98.04%         467                13    97.22%
cancellation.rs                    11                 0   100.00%           3                 0   100.00%          11                 0   100.00%
endings.rs                         29                 0   100.00%           1                 0   100.00%          20                 0   100.00%
limits.rs                          45                 0   100.00%           2                 0   100.00%          68                 0   100.00%
mime.rs                            11                 0   100.00%           1                 0   100.00%           7                 0   100.00%
source\memory.rs                   31                 0   100.00%           4                 0   100.00%          26                 0   100.00%
source\mod.rs                       3                 0   100.00%           1                 0   100.00%           3                 0   100.00%
-------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            1218                25    97.95%          63                 1    98.41%         602                13    97.84%
```
