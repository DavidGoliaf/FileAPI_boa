# M7 Handoff — WPT harness and hardening (rework F1–F11 + rework-2 F12–F17 applied)

Branch: `task/m7`, base `bc742df200c1aa1bc8f9cee63b0c20c37737d64c`.
Status: rework complete per `docs/reviews/M7-rework.md` (base `b50c60e`)
and `docs/reviews/M7-rework-2.md` (base `aed9a0f`); local validation
green on Windows; external CI (Ubuntu + Windows) AWAITING OWNER
VERIFICATION — no CI run is claimed here.

## Implemented

- Harness (`boa_fapi_wpt`, zero new deps): `manifest.rs` (strict
  schema-1 loader + self-contained JSON parser + `corpus_root`/path
  policy), `harness.rs` (testharness prelude with `record_once`/
  `run_step`), `runner.rs` (per-file fresh `Context`, bounded
  `run_jobs()` pump, PASS/FAIL/TIMEOUT/NOTRUN), `report.rs`
  (deterministic JSON/JUnit + enum-compared strict gate), `main.rs`
  (CLI `--manifest/--strict/--threads/--filter/--json/--junit/--timeout-ms`,
  SHA-256 verification, isolated `--worker-file` child processes with
  wall kill, 8 MiB thread for Boa recursion).
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
scope — exact gaps, never wildcard PASS); `--worker-file` internal
worker protocol (validated file index only, no shell/command string);
`run_file` kept as the library mapping while strict CLI uses the
isolated path (both documented in `docs/wpt.md`); per-test-module
`allow(clippy::expect_used)` in harness unit tests only (no production
use — workspace clippy green without blanket allow).

## ADR

ADR-0033 (zero new production dependencies for the harness) — see
`docs/DECISIONS.md`.

## Demo commands

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --package boa_fapi_wpt -- --nocapture
cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture
cargo test --package boa_fapi --test abort_races -- --nocapture
cargo test --package boa_fapi --test abort_races_fs -- --nocapture
cargo test --package boa_fapi --test hardening_hooks -- --nocapture
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --filter corpus/blob
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --filter corpus/blob
cargo doc --workspace --no-deps
git diff --check
```

Expected boundary results: full strict exit 0 (38 passed, 0 notrun,
0 unexpected); threads 2 exit 0 with SHA-256-identical JSON to
threads 1; non-strict filter exit 0 with the subset report; strict +
filter exit 2 (`--filter cannot be combined with --strict`).

`cargo test --workspace --no-default-features` stays NOT RUN by the
accepted M6 design; powerset coverage is `cargo hack`.

## Exact commit / CI links

- Implementation commit: _to be filled after commit_.
- CI: AWAITING OWNER VERIFICATION — no run URL/SHA is claimed here
  (CI must run on the final implementation commit on Ubuntu and Windows).
- After handoff: next stage NOT started.
