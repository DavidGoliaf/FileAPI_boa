# Changelog

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
