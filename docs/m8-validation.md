# M8 validation — release closure and observability

Date: 2026-09-08. Branch `task/m8`, base `92192927d8bf9d6ebf3ed4e6ca60c9966e3c4dff` (M7 baseline).
Local platform: Windows x86_64 MSVC (weak-identity target — live-handle tests are Unix-only by construction).
Toolchain: cargo 1.91.0, rustc 1.91.0, `wasm32-unknown-unknown` std installed.
Commit SHA: recorded in `docs/reviews/M8-handoff.md` at commit time (working tree below).

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 11 M6-URL-JS, 6 M6-clone-JS, 5 m8-observability, 12 appendix-A, 8 abort-races, 1 abort-races-fs, 4 hardening-hooks, 58+14+25+8+20+16+19 core incl. 14 M6-core, 22 wpt-harness, 9 fs, 1 doc) |
| 4 | `cargo test --package boa_fapi --test m8_feature_off` | 0 | PASS (1; feature-off surface guard) |
| 5 | `cargo test --package boa_fapi_wpt -- --nocapture` | 0 | PASS (18 library + 4 binary) |
| 6 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict` | 0 | PASS (38 passed, 0 notrun, 0 unexpected, 7 files; same corpus hash as M7) |
| 7 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2` | 0 | PASS (isolated workers, same strict pass) |
| 8 | `cargo test --package boa_fapi --test m8_observability --all-features -- --nocapture` | 0 | PASS (5: allow-list, result classes, secrecy, ordering/stale, feature-on surface) |
| 9 | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` | 0 | PASS (TOTAL 89.94% lines) |
| 10 | `cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 80` | 0 | PASS (TOTAL 87.20% lines) |
| 11 | `cargo llvm-cov --workspace --all-features --fail-under-lines 80` | 0 | PASS (TOTAL 81.08% lines) |
| 12 | `cargo check --target wasm32-unknown-unknown --package boa_fapi_core` | 0 | PASS (memory-only core) |
| 13 | `cargo check --target wasm32-unknown-unknown --package boa_fapi --no-default-features --lib` | FAIL | BLOCKER Q4: `getrandom v0.4.3` via `boa_engine` has no wasm32 backend without `wasm_js` (see `QUESTIONS.md`) |
| 14 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (27 checks incl. `tracing` off/on; pre-existing dead-code warnings only in non-default combos) |
| 15 | `cargo deny fetch db` + `cargo deny check` | 0 | PASS (advisories/bans/licenses/sources ok; only pre-existing duplicate-version warnings) |
| 16 | `cargo package --workspace --all-features` | 0 | PASS (4 crates packaged to `target/`; archives not committed) |
| 17 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 18 | `git diff --check` | 0 | PASS |

## Package coverage totals (line cover, `cargo llvm-cov --summary-only`)

- `boa_fapi_core` (`--all-features`): TOTAL 89.94% lines (gate 85) — exit 0.
- `boa_fapi` (`--all-features`): TOTAL 87.20% lines (gate 80) — exit 0.
- workspace (`--all-features`): TOTAL 81.08% lines (gate 80) — exit 0.

## WASM check

- `boa_fapi_core` on `wasm32-unknown-unknown`: PASS.
- `boa_fapi --no-default-features --lib` on `wasm32-unknown-unknown`: FAIL with transitive `getrandom 0.4.3` backend error (pre-existing M6 `boa_engine` dependency, not introduced by M8). Blocker recorded in `QUESTIONS.md` Q4 per §2.8. No workaround applied.

## Package check

- `cargo package --workspace --all-features` (with `--allow-dirty --no-verify` locally; CI runs the clean form): exit 0 for all four crates.
- `LICENSE-MIT` present at root; `cargo package -p boa_fapi_core --list` and `-p boa_fapi --list` include the new `README.md` / `observability.rs` / `m8_*` suites (`LICENSE-MIT` inclusion follows the workspace `license = "MIT"` packaging path; no archive committed).
- Per-crate packaging notes: `boa_fapi`/`boa_fapi_wpt` emit the pre-existing "manifest has no documentation, homepage or repository" warning (unchanged from M7 baseline; no new package metadata beyond the canonical repository URL).

## Feature matrix

| Configuration | Result |
|---|---|
| `--all-features` (incl. `tracing` on) | PASS (full workspace + 5 M8 collector tests) |
| default features (`tracing` off) | PASS (`m8_feature_off` 1 test; M1–M7 suites unchanged) |
| `--no-default-features` (`boa_fapi` lib check) | compiles with pre-existing dead-code warnings only (host platforms) |
| `cargo hack --feature-powerset --depth 2` | PASS (tracing off/on both covered) |

## WPT result

Strict run: 38 passed, 0 notrun, 0 unexpected (7 files) — identical corpus and counts to the M7 baseline; no report committed.

## No-leak evidence

`m8_observability::tracing_never_leaks_sensitive_values` asserts the serialized collector output contains none of: `display_name` with `..`/slash/drive/Unicode, fake URL/UUID, origin/partition-derived strings, snapshot markers, error-like text, or blob bodies. `docs/security.md` records the M8 telemetry secrecy rule.

## External CI

- CI workflow updated (`.github/workflows/ci.yml`): matrix `windows-latest`/`ubuntu-latest`/`macos-14`, wasm memory-only steps, per-package coverage gates, `m8_observability` step, `cargo package` step.
- External CI run URLs: recorded in `docs/reviews/M8-handoff.md` after push (local validation above is Windows-only; Ubuntu/macOS/Windows jobs are AWAITING CI at handoff time).
