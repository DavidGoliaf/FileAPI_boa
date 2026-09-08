# Changelog

## M8 — release closure and observability (task/m8, unreleased)

- New optional default-off `tracing` feature in `boa_fapi` (only new
  dependency: `tracing = "0.1"`): one terminal
  `boa_fapi::file_api.operation` event per completion with exactly
  `operation`, `size`, `duration_ms`, `chunk_count`, `result_class`,
  `environment_hash`. No JS API change, no job/error/lifetime change.
- New `m8_observability` (5 tests, feature-gated) + `m8_feature_off`
  (1 test) suites: allow-list, result classes, secrecy, ordering/stale
  suppression, feature-off surface.
- Release gates: per-package coverage (`boa_fapi_core` ≥ 85%,
  `boa_fapi` ≥ 80%), Linux/macOS/Windows CI, `wasm32-unknown-unknown`
  memory-only checks, `cargo package` gate; crate READMEs,
  canonical repository URL, `LICENSE-MIT`.
- No new JS API: M1–M7 surface, capabilities and error mapping are
  unchanged; full WHATWG/DOM/Streams/Workers/Fetch/File System Access
  remain out of scope.

## M7 — WPT harness and hardening (task/m7, unreleased)

- New `boa_fapi_wpt` CLI harness: strict manifest runs over an adapted
  pinned WPT subset (7 files / 38 subtests) with SHA-256 verification,
  minimal testharness prelude, explicit `run_jobs()` pumping,
  PASS/FAIL/TIMEOUT/NOTRUN statuses and deterministic JSON/JUnit
  reports. Normative command:
  `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict`.
- New JS integration suites in `boa_fapi`: `appendix_a_acceptance`
  (12 tests, observable M1–M6 surface) and `abort_races` (8 tests) plus
  `abort_races_fs` (1 test, `fs`-gated filesystem race).
- New bounded hardening hooks (`hardening_hooks`, 4 tests): Node
  differential smoke, fixed-seed decode fuzz, bench smoke, leak/repeat
  counts. Nightly scheduled workflow for extended fuzz/miri-style runs.
- No JS API change: M1–M6 surface, capabilities and error mapping are
  unchanged; full DOM/HTML/Workers/Fetch/full Streams remain out of scope.
