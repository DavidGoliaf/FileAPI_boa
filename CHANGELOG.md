# Changelog

## Unreleased — M9-R1 compatibility and accepted audit follow-up

### Compatibility changes (accepted ADR-0048–0051)

- `File.name` preserves the converted USVString verbatim, including `/`,
  instead of replacing `/` with `:`. Host display names and clone
  round-trips also preserve slashes; callers must supply a safe display
  name, not a secret host path. No basename is inferred.
- While LOADING, `FileReader.abort()` dispatches `abort` and conditional
  `loadend` synchronously before returning, not through a later job.
  Handlers can reenter immediately; a restarted read suppresses the old
  `loadend`. EMPTY/DONE abort remains silent and clears `result`.
- A task/microtask boundary after `loadstart` lets queued promise reactions
  run before the first chunk is applied.
- Both `FileReader` and `FileReaderSync` use
  `data:application/octet-stream;base64,<payload>` when `Blob.type` is empty.
  The output is 24 bytes longer than the former `data:;base64,<payload>`;
  its 37-byte prefix counts toward `max_data_url_output`, including for an
  empty Blob. Previously accepted near-limit reads can now fail with
  `QuotaExceededError`; equality with the full output limit still succeeds.
  This is accepted pinned-WPT change-control, not a wholesale WD update.

### Accepted audit fixes — locally validated; external CI pending

- Clean up FileReader state tables at shutdown, including reader roots
  and deferred state at the `loadstart` boundary.
- Check shutdown immediately after the `abort` handler, before dispatching
  `loadend` or enqueueing a listener error.
- In `package_data_url`, check the full output length against the quota
  before allocating the base64 payload.
- Stop after shutdown inside intermediate or final `progress`: do not
  restore cleared state or publish `DONE`/`result` after shutdown.
- Reconcile current specification, host-name and clone-name documentation
  with the accepted behavior. Local validation is recorded in the M9F
  handoff; external CI for these uncommitted changes remains pending.

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
