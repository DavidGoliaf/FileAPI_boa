# M2 Validation Report

## Environment

- **Date**: 2026-09-06
- **Toolchain**: rustc 1.91.0 (edition 2024)
- **OS**: Windows (win32)
- **Shell**: PowerShell 7+
- **Boa**: boa_engine 0.22.0, boa_gc 0.22.0 (locked in `Cargo.lock`)

## Validation Commands (work order §8)

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (207 tests: 22 boa_fapi unit, 9 boa_fapi guards, 51 M2 JS integration, 1 boa_fapi doc, 124 core: 41+25+6+17+16+19) |
| 4 | `cargo test --package boa_fapi --test m2_blob_file_filelist` | 0 | PASS (51 tests, all executing JS in a real Boa `Context`) |
| 5 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 6 | `cargo test --package boa_fapi --doc` | 0 | PASS (1 doc test) |
| 7 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85` | 0 | PASS (92.61% lines, threshold 85%) |
| 8 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 9 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db` | 0 | PASS (advisory DB fetched from GitHub) |
| 10 | `$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok) |
| 11 | `git diff --check` | 0 | PASS (CRLF warnings only) |

## Coverage Summary (boa_fapi)

```
Filename                      Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blob.rs                           211                12    94.31%          10                 0   100.00%         131                 4    96.95%           0                 0         -
brand.rs                           45                 2    95.56%           4                 0   100.00%          37                 2    94.59%           0                 0         -
clock.rs                           25                 8    68.00%           3                 1    66.67%          12                 3    75.00%           0                 0         -
error.rs                           21                 7    66.67%           3                 1    66.67%          13                 5    61.54%           0                 0         -
extension.rs                      248                33    86.69%          19                 1    94.74%         165                14    91.52%           0                 0         -
file.rs                           157                 8    94.90%          12                 0   100.00%         114                 3    97.37%           0                 0         -
file_list.rs                      124                 7    94.35%           5                 0   100.00%          88                 6    93.18%           0                 0         -
webidl.rs                         632                71    88.77%          41                 4    90.24%         387                33    91.47%           0                 0         -
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
TOTAL                            1463               148    89.88%          97                 7    92.78%         947                70    92.61%           0                 0         -
```

## Notes

- Every M2 integration test registers the extension into a fresh
  `boa_engine::Context` and evaluates real JavaScript; Rust-only unit tests
  additionally verify byte content and `Arc` sharing through the
  `#[cfg(test)]` child module (`src/tests.rs`), as permitted by work order §6.
- Raw segments are not part of any public Rust API: `boa_fapi_core` exposes
  only `concat_shared`/`push_shared`/`read_all` plus the identity probes
  `shares_sources_with`/`first_segment_shares_source_with`
  (ADR-0010); `PartsCollector` owns a single `BlobData`.
- `deny.toml` gained the `Zlib` license (foldhash via the Boa dependency
  tree); `[bans] wildcards` stays `deny` — the workspace-internal
  `boa_fapi_core` dependency pins both `path` and `version`, and
  `allow-wildcard-paths` is only a backstop for path-only dev edges
  (ADR-0003).
- `Cargo.lock` is tracked in Git (removed from `.gitignore`), so a fresh
  clone reproduces the locked Boa 0.22.x tree.
