# M7 validation

Date: 2026-09-08. Branch `task/m7`, base `bc742df200c1aa1bc8f9cee63b0c20c37737d64c`.
Local platform: Windows (weak-identity target — live-handle tests are Unix-only by construction).

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 11 M6-URL-JS, 6 M6-clone-JS, 12 appendix-A, 8 abort-races, 1 abort-races-fs, 4 hardening-hooks, 58+14+25+8+20+16+19 core incl. 14 M6-core, 12 wpt-harness, 9 fs, 1 doc) |
| 4 | `cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture` | 0 | PASS (12) |
| 5 | `cargo test --package boa_fapi --test abort_races -- --nocapture` | 0 | PASS (8) |
| 6 | `cargo test --package boa_fapi --test abort_races_fs -- --nocapture` | 0 | PASS (1; Unix live path `#[cfg(unix)]`, copy path elsewhere) |
| 7 | `cargo test --package boa_fapi --test hardening_hooks -- --nocapture` | 0 | PASS (4: differential/fuzz/bench/leak, bounded) |
| 8 | `cargo test --package boa_fapi_wpt -- --nocapture` | 0 | PASS (12) |
| 9 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict` | 0 | PASS (38 expected, 0 unexpected, 7 files; strict_pass=true) |
| 10 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --json target/wpt-report.json --junit target/wpt-report.xml` | 0 | PASS (artifacts written to `target/`, never committed) |
| 11 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 12 | `cargo test --package boa_fapi --doc` | 0 | PASS (1) |
| 13 | `cargo llvm-cov --workspace --all-features --fail-under-lines 80` | 0 | PASS (TOTAL 80.72% lines; harness lib covered, `main.rs` CLI covered only via strict runs) |
| 14 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (20/20 incl. `fs`/`url-shim`/`structured-clone` on/off + wpt crate) |
| 15 | `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; only pre-existing duplicate-version warnings; zero new deps — SBOM confirms) |
| 16 | `cargo tree --workspace --all-features --prefix none > target/sbom.txt` | 0 | PASS (SBOM artifact, never committed) |
| 17 | `git diff --check` | 0 | PASS |

## Feature matrix

| Configuration | Result |
|---|---|
| `--all-features` | PASS (full workspace incl. 25 M7 tests + 12 harness units + 38 strict subtests) |
| `--no-default-features` (`cargo test`) | NOT RUN by design (accepted M6 decision: shim-required suites fail without default features; `cargo hack --feature-powerset` proves every combination compiles) |
| WPT `--filter` / `--threads` / `--timeout-ms` | PASS (diagnostic subset keeps strict semantics; default single-worker deterministic) |

## Bounded hooks evidence (seed / duration / baseline / result)

- `differential_smoke`: fixed corpus `new Blob(['abc'])` → `3:`; Node v24.19.0 present → match PASS; without node → SKIP with reason (never fake PASS).
- `fuzz_decode_bounded`: seed `0x9E3779B97F4A7C15`, 512 cases, 64 KiB cap, <1s → explored=512, no panic.
- `bench_smoke`: clone×5 + resolve×200 on 256 KiB blob → informational medians printed, completion asserted, no absolute-path baseline published.
- `leak_repeat`: 8× create/revoke/shutdown cycles → counts return to 0, fresh contexts isolated.
- Nightly: `.github/workflows/nightly.yml` (scheduled fuzz/miri-shape, visible failures, best-effort miri skip with reason).

## M6 regression

All M1–M6 suites green without modification (only additive M7 files).
`--no-default-features` stance unchanged from the accepted M6 decision.

## CI

- External CI (GitHub Actions, Ubuntu + Windows) status at handoff time: **NOT RUN YET** — local validation above is on Windows; Unix-only paths (`#[cfg(unix)]` live-handle/snapshot/fs-race) execute in the Ubuntu job. The implementation commit and the CI run link will be recorded in `docs/reviews/M7-handoff.md` after push.
- Reports/SBOM are CI artifacts (`target/wpt-report.json`, `target/wpt-report.xml`, `target/sbom.txt`), never committed.
