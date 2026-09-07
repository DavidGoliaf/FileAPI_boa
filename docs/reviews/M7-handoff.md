# M7 Handoff — WPT harness and hardening

Branch: `task/m7`, base `bc742df200c1aa1bc8f9cee63b0c20c37737d64c`.
Status: implementation complete, local validation green on Windows;
external CI (Ubuntu + Windows) NOT RUN YET at handoff time.

## Implemented

- Harness (`boa_fapi_wpt`, zero new deps): `manifest.rs` (strict
  schema-1 loader + self-contained JSON parser), `harness.rs`
  (testharness prelude), `runner.rs` (per-file fresh `Context`, bounded
  `run_jobs()` pump, PASS/FAIL/TIMEOUT/NOTRUN), `report.rs`
  (deterministic JSON/JUnit + strict gate), `main.rs` (CLI
  `--manifest/--strict/--threads/--filter/--json/--junit/--timeout-ms`,
  SHA-256 verification, 8 MiB worker thread for Boa recursion).
- Corpus: 7 adapted files in `crates/boa_fapi_wpt/corpus/*.js` +
  `wpt-manifest.json` (pinned `0968c868…`, 38 subtests, all PASS;
  upstream blob SHAs recorded; file SHA-256 verified pre-run).
- Suites (`boa_fapi`): `appendix_a_acceptance` (12),
  `abort_races` (8), `abort_races_fs` (1, `fs`-gated),
  `hardening_hooks` (4: differential/fuzz/bench/leak, bounded).
- Docs: `docs/wpt.md`, `docs/spec-delta.md`, `CHANGELOG.md`,
  `QUESTIONS.md` (supply decision), `README.md` (M7 scope),
  `docs/architecture.md` (layer 4), `docs/spec-matrix.md`
  (M7-WPT-01..05, M7-RACE-01..03, M7-HARD-01..03, M7-DOC-01),
  `docs/DECISIONS.md` (ADR-0033, zero new deps), `docs/m7-validation.md`.
- CI: M7 test steps + strict WPT run with JSON/JUnit artifacts + SBOM
  artifact in `ci.yml`; scheduled `nightly.yml` (hardening, visible
  failures, best-effort miri skip with reason).

## Deviations

None normative. Deliberate scoped choices (all documented): adapted
subset instead of full upstream pass (browser capabilities out of
scope — exact gaps, never wildcard PASS); `expect_used` re-allowed
crate-wide in `boa_fapi_wpt` (unit-test fixtures only — pinned by a
comment, no production use); `main.rs` worker thread for Boa stack
depth (platform parity, not semantics).

## ADR

ADR-0033 (zero new production dependencies for the harness) — see
`docs/DECISIONS.md`.

## Demo commands

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture
cargo test --package boa_fapi --test abort_races -- --nocapture
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo llvm-cov --workspace --all-features --fail-under-lines 80
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

`cargo test --workspace --no-default-features` stays NOT RUN by the
accepted M6 design; powerset coverage is `cargo hack`.

## Exact commit / CI links

- Implementation commit: _to be filled after commit_.
- CI run: _to be filled after push_ (Ubuntu + Windows, M7 sequence).
- After handoff: next stage NOT started.
