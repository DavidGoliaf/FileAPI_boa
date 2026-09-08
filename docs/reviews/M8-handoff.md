# M8 Handoff — release closure and observability (tracing default-off)

Branch: `task/m8`, base `92192927d8bf9d6ebf3ed4e6ca60c9966e3c4dff` (M7 baseline).
Status: REWORK REQUIRED — wasm blocker Q4 open (see below). Handoff is evidence, not acceptance.
Implementation commit: PENDING (recorded here at commit time).

## Implemented

- Optional `tracing` (`boa_fapi`, default off; only new dep `tracing 0.1`):
  `src/observability.rs` (target `boa_fapi::file_api.operation`, six fixed
  fields, exact error mappings, opaque env hash, monotonic `duration_ms`);
  terminal call sites in `promise_read.rs`, `streams.rs` (+ `total_size`),
  `filereader.rs` (success/fail-fast/abort), `filereader_sync.rs`,
  `extension.rs` (URL create/resolve, clone encode/decode/bridge, `fs_read`
  in `ArcResourceSource`); no JS/API/job/error/lifetime change, no new
  public adapters, no JS calls from trace paths.
- Tests: `m8_observability` (5, feature-gated: allow-list, result classes,
  secrecy, ordering/stale suppression, feature-on surface) +
  `m8_feature_off` (1: optional-dep manifest + M1–M7 behavior without the
  feature). All guards green.
- CI (`.github/workflows/ci.yml` only): matrix
  `windows-latest`/`ubuntu-latest`/`macos-14`, kept M1–M7 steps + strict WPT
  order, added `m8_observability` test, Ubuntu wasm memory-only steps,
  per-package coverage gates (85/80) + workspace gate, `cargo package`
  step, unchanged deny fetch/check.
- Metadata/docs: canonical repository URL, `LICENSE-MIT`, four crate
  READMEs, `README.md` (M8 gates + tracing), `docs/architecture.md`
  (layer 5 observer), `docs/security.md` (telemetry secrecy),
  `docs/spec-matrix.md` (M8-REL-01..05, M8-SEC-01, M8-RACE-01),
  `CHANGELOG.md` (M8 entry, no new JS API), `docs/DECISIONS.md`
  (ADR-0034 tracing dep), `docs/m8-validation.md`,
  `QUESTIONS.md` Q4 wasm blocker.
- Historical M1–M7 audit/handoff/rework docs untouched.

## Deviations

- `ENGINE_ERROR_CLASS` const lives in `observability.rs` gated on
  `dom-shim` (avoids the `"error"` literal in `filereader_sync.rs` under
  the surface guard). No behavior impact.
- `tasks/09_TASK_WPT_HARDENING.md` / `tasks/10_TASK_RELEASE_CLOSURE.md`
  are untracked user files; left uncommitted per §2.9.

## Blocker

- Q4 (wasm): `cargo check --target wasm32-unknown-unknown --package
  boa_fapi --no-default-features --lib` fails on the pre-existing
  transitive `getrandom v0.4.3` via `boa_engine` (no wasm32 backend
  without `wasm_js`). `boa_fapi_core` wasm check passes. Recorded in
  `QUESTIONS.md` Q4 with command, output, and acceptance impact. No
  workaround applied (§2.8). DoD wasm item NOT met; external CI cannot
  turn it green without an owner decision.

## Demo commands (exit codes on Windows, this tree)

```powershell
git rev-parse HEAD # pending commit
git status --short --branch # 0 after commit
cargo fmt --all -- --check # 0
cargo clippy --workspace --all-targets --all-features -- -D warnings # 0
cargo test --workspace --all-features # 0
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict # 0 (38 passed, 0 unexpected)
cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85 # 0 (89.94% lines)
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 80 # 0 (87.20% lines)
cargo check --target wasm32-unknown-unknown --package boa_fapi_core # 0
cargo check --target wasm32-unknown-unknown --package boa_fapi --no-default-features --lib # FAIL (Q4 blocker)
cargo hack check --feature-powerset --depth 2 # 0 (27 checks)
cargo deny check # 0 (pre-existing duplicate-version warnings only)
cargo package --workspace --all-features # 0 (to target/, not committed)
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps # 0
git diff --check # 0
```

Coverage totals: core 89.94% lines (gate 85), boa_fapi 87.20% (gate 80),
workspace 81.08% (gate 80).
External CI run URLs (Ubuntu/macOS/Windows): AWAITING CI — no run claimed here.
Changed files: `.github/workflows/ci.yml`, `CHANGELOG.md`, `Cargo.lock`,
`Cargo.toml`, `QUESTIONS.md`, `README.md`, `LICENSE-MIT`,
`crates/boa_fapi/Cargo.toml`, four crate READMEs, `src/observability.rs`,
`src/{extension,promise_read,streams,filereader,filereader_sync,lib}.rs`,
`tests/{m8_observability,m8_feature_off}.rs`, `docs/{DECISIONS,architecture,
m8-validation,security,spec-matrix}.md`.
