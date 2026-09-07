# M4-A Validation Report (rework revalidation)

## Environment

- **Date**: 2026-09-08 (rework revalidation; initial report 2026-09-07)
- **Toolchain**: rustc 1.91.0 (edition 2024)
- **OS**: Windows (win32)
- **Shell**: PowerShell 7+
- **Boa**: boa_engine 0.22.0, boa_gc 0.22.0 (locked in `Cargo.lock`)
- **New deps**: encoding_rs 0.8.35, base64 0.22.1 (locked in `Cargo.lock`)

## Validation Commands (work order §7 + rework order)

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (324 tests: 37 boa_fapi unit, 12 boa_fapi guards, 51 M2 JS integration, 33 M4-A JS integration, 28 M3-B JS integration, 16 M3-A JS integration, 1 boa_fapi doc, 146 core: 58+25+8+20+16+19) |
| 4 | `cargo test --package boa_fapi --test m2_blob_file_filelist` | 0 | PASS (51 tests, all executing JS in a real Boa `Context`) |
| 5 | `cargo test --package boa_fapi --test m3_promise_blob_reads` | 0 | PASS (16 tests, every read settled via `context.run_jobs()`; rejections now mapped `DOMException`) |
| 6 | `cargo test --package boa_fapi --test m3_blob_streams` | 0 | PASS (28 tests, every chunk settled via `context.run_jobs()`; errors now mapped `DOMException`) |
| 7 | `cargo test --package boa_fapi --test m4_filereader_async` | 0 | PASS (33 tests, every event observed after `context.run_jobs()`) |
| 8 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 9 | `cargo test --package boa_fapi --doc` | 0 | PASS (1 doc test) |
| 10 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (89.79% lines, threshold 85%) |
| 11 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (all feature combinations, incl. `--no-default-features`) |
| 12 | `cargo deny check` | 1 independent / 0 local | BLOCKED — the independent acceptance run exited 1 because the RustSec advisory database could not be fetched from GitHub. The local revalidation passes only against the locally available database and is not claimed as acceptance evidence; no advisory result, CI run, URL, or run ID is claimed. |
| 13 | `git diff --check` | 0 | PASS (CRLF warnings only) |

## Coverage Summary (boa_fapi)

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           225                12    94.67%          10                 0   100.00%         151                 4    97.35%           0                 0         -
brand.rs                           45                 2    95.56%           4                 0   100.00%          37                 2    94.59%           0                 0         -
clock.rs                           25                 8    68.00%           3                 1    66.67%          12                 3    75.00%           0                 0         -
dom.rs                           1045               175    83.25%          53                 3    94.34%         783               104    86.72%           0                 0         -
error.rs                           28                14    50.00%           4                 2    50.00%          19                11    42.11%           0                 0         -
extension.rs                      416                56    86.54%          38                 6    84.21%         308                30    90.26%           0                 0         -
file.rs                           157                 8    94.90%          12                 0   100.00%         114                 3    97.37%           0                 0         -
file_list.rs                      124                7    94.35%           5                 0   100.00%          88                 6    93.18%           0                 0         -
filereader.rs                    1637               168    89.74%         103                19    81.55%        1292               104    91.95%           0                 0         -
promise_read.rs                   329                34    89.67%          25                 5    80.00%         226                19    91.59%           0                 0         -
streams.rs                       1066               206    80.68%          55                12    78.18%         715               103    85.59%           0                 0         -
webidl.rs                         632                71    88.77%          41                 4    90.24%         387                33    91.47%           0                 0         -
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            5729               761    86.72%         353                52    85.27%        4132               422    89.79%           0                 0         -
```

## Notes

- Every M4-A integration test registers the extension into a fresh
  `boa_engine::Context`, executes real JavaScript, and pumps the Boa job
  queue explicitly with `context.run_jobs()`; `loadstart`/`progress` fire
  synchronously inside their FileReading pump job (still within the task
  source), terminal `load`/`error`/`abort` (+ conditional `loadend`) go
  through queued dispatch jobs. No test calls JS from source completion,
  calls `run_jobs()` from a job, or inspects private internals.
- Throttle uses the injected `Clock` (fake/step clocks in tests; never
  system time); one pump uses exactly one clock sample — the event
  `timeStamp` reuses the pump's tick, so no extra clock read perturbs the
  50 ms accounting (rework finding 4; a step-clock assertion guards the
  single-sample contract).
- Stale-generation rechecks stand after every reentrant non-terminal
  dispatch (`loadstart`, `progress`, final progress) and before source
  reads, successor enqueue, packaging, slot release, and event emission
  (rework finding 3; counting-source tests prove zero post-abort reads).
- M3 promise/stream errors migrated exactly once to the central M4-A
  `DOMException` mapping (`ResourceLimit` → `QuotaExceededError`); without
  the `dom-shim` feature the pre-M4 `RangeError`/plain-`Error` mapping
  still applies (proven by `cargo hack` powerset check).
- `deny.toml` gained `BSD-3-Clause` for `encoding_rs`
  (`(Apache-2.0 OR MIT) AND BSD-3-Clause`); `base64` needs no change
  (MIT OR Apache-2.0).
- `cargo deny check` is BLOCKED (see row 12): the independent acceptance
  run exited 1 on the advisory-database fetch; the local exit 0 is not
  acceptance evidence.
- CI: `awaiting customer verification` — the owner checks green Windows
  and Ubuntu runs for the final commit before acceptance; no run URL/ID is
  claimed here.
