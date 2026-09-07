# M4-B Validation Report

## Environment

- **Date**: 2026-09-08
- **Toolchain**: rustc 1.91.0 (edition 2024)
- **OS**: Windows (win32)
- **Shell**: PowerShell 7+
- **Boa**: boa_engine 0.22.0, boa_gc 0.22.0 (locked in `Cargo.lock`)
- **Deps**: no new dependencies (`encoding_rs` 0.8.35, `base64` 0.22.1 reused)

## Validation Commands (work order §9)

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (352 tests: 42 boa_fapi unit incl. 5 sync, 13 boa_fapi guards, 51 M2 JS integration, 33 M4-A JS integration, 21 M4-B JS integration, 28 M3-B JS integration, 16 M3-A JS integration, 1 boa_fapi doc, 146 core: 58+25+8+20+16+19) |
| 4 | `cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture` | 0 | PASS (51 tests) |
| 5 | `cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture` | 0 | PASS (16 tests) |
| 6 | `cargo test --package boa_fapi --test m3_blob_streams -- --nocapture` | 0 | PASS (28 tests) |
| 7 | `cargo test --package boa_fapi --test m4_filereader_async -- --nocapture` | 0 | PASS (33 tests, unchanged and green) |
| 8 | `cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture` | 0 | PASS (21 tests, every result returned synchronously) |
| 9 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 10 | `cargo test --package boa_fapi --doc` | 0 | PASS (1 doc test) |
| 11 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (89.95% lines, threshold 85%) |
| 12 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (all feature combinations, incl. `--no-default-features`; no warnings) |
| 13 | `cargo deny check` | 1 acceptance / 0 local | BLOCKED — the acceptance run exited 1 because the RustSec advisory database was unavailable in that environment. The local revalidation passes only against the locally available database and is not claimed as acceptance evidence; no advisory result is claimed here. The verified CI run below executed `cargo deny fetch db` + `cargo deny check` green on both OS. |
| 14 | `git diff --check` | 0 | PASS (CRLF warnings only) |

## Coverage Summary (boa_fapi)

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           225                12    94.67%          10                 0   100.00%         151                 4    97.35%           0                 0         -
brand.rs                           45                 2    95.56%           4                 0   100.00%          37                 2    94.59%           0                 0         -
clock.rs                           25                 8    68.00%           3                 1    66.67%          12                 3    75.00%           0                 0         -
dom.rs                           1045               175    83.25%          53                 3    94.34%         783               104    86.72%           0                 0         -
error.rs                           28                14    50.00%           4                 2    50.00%          19                11    42.11%           0                 0         -
extension.rs                      490                77    84.29%          45                 9    80.00%         367                49    86.65%           0                 0         -
file.rs                           157                 8    94.90%          12                 0   100.00%         114                 3    97.37%           0                 0         -
file_list.rs                      124                7    94.35%           5                 0   100.00%          88                 6    93.18%           0                 0         -
filereader.rs                    1520               164    89.21%          93                19    79.57%        1209                92    92.39%           0                 0         -
filereader_sync.rs                517                42    91.88%          40                 8    80.00%         382                22    94.24%           0                 0         -
package.rs                        149                12    91.95%          12                 1    91.67%          99                10    89.90%           0                 0         -
promise_read.rs                   329                34    89.67%          25                 5    80.00%         226                19    91.59%           0                 0         -
streams.rs                       1066               206    80.68%          55                12    78.18%         715               103    85.59%           0                 0         -
webidl.rs                         632                71    88.77%          41                 4    90.24%         387                33    91.47%           0                 0         -
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            6352               832    86.90%         402                64    84.08%        4589               461    89.95%           0                 0         -
```

## Notes

- Every M4-B integration test uses a fresh real `boa_engine::Context`
  with an explicit environment descriptor and asserts a synchronous
  JS-visible return or same-realm `DOMException` throw; sync results
  never touch `context.run_jobs()`, and the no-jobs test proves delivery
  never depends on the queue.
- The M4-A async surface is byte-for-byte behavior-preserving under the
  `package.rs` extraction: the full M4-A suite (33 integration + 37
  unit + model corpus) passes unchanged, with zero M4-A test edits.
- `cargo deny check` records its factual acceptance result (exit 1,
  BLOCKED, advisory DB unavailable there). The local exit 0 and the
  green CI deny steps below are supporting evidence only, never a
  rewrite of the acceptance result.
- CI (verified via the GitHub API, not invented): run `34100515645`
  (`CI #10`, push of `51e6eda` on `task/m4b`) completed with conclusion
  `success` — `M4-B validation (windows-latest)` job `101673651515`
  green and `M4-B validation (ubuntu-latest)` job `101673651739`
  green, every step green on both OS including `cargo deny fetch db`,
  `cargo deny check`, and `git diff --check`:
  [run](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34100515645),
  [Windows job](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34100515645/job/101673651515),
  [Ubuntu job](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34100515645/job/101673651739).
  That run predates the acceptance-blocker fixes in this report; the
  current tip additionally awaits its own CI verification by the owner.
