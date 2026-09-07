# M7 validation (incl. M7-rework F1–F11)

Date: 2026-09-08. Branch `task/m7`, base `bc742df200c1aa1bc8f9cee63b0c20c37737d64c`.
Local platform: Windows (weak-identity target — live-handle tests are Unix-only by construction).

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 11 M6-URL-JS, 6 M6-clone-JS, 12 appendix-A, 8 abort-races, 1 abort-races-fs, 4 hardening-hooks, 58+14+25+8+20+16+19 core incl. 14 M6-core, 15 wpt-harness, 9 fs, 1 doc) |
| 4 | `cargo check --workspace --all-features` | 0 | PASS |
| 5 | `cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture` | 0 | PASS (12) |
| 6 | `cargo test --package boa_fapi --test abort_races -- --nocapture` | 0 | PASS (8) |
| 7 | `cargo test --package boa_fapi --test abort_races_fs -- --nocapture` | 0 | PASS (1; Unix live path `#[cfg(unix)]`, copy path elsewhere) |
| 8 | `cargo test --package boa_fapi --test hardening_hooks -- --nocapture` | 0 | PASS (4: differential/fuzz/bench/leak, bounded) |
| 9 | `cargo test --package boa_fapi_wpt -- --nocapture` | 0 | PASS (18 library + 1 binary test) |
| 10 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict` | 0 | PASS (38 passed, 0 notrun, 0 unexpected, 7 files; strict_pass=true) |
| 11 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --threads 2 --json target/wpt-report.json --junit target/wpt-report.xml` | 0 | PASS (SHA-256 `4D5CEC74…9B9CD` — identical to `--threads 1`; artifacts written to `target/`, never committed) |
| 12 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --filter corpus/blob` | 0 | PASS (22 passed, 0 notrun, 0 unexpected, 3 files — diagnostic subset) |
| 13 | `cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict --filter corpus/blob` | 2 | EXPECTED LAUNCH ERROR (`--filter cannot be combined with --strict`) |
| 14 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 15 | `cargo test --package boa_fapi --doc` | 0 | PASS (1) |
| 16 | `cargo llvm-cov --workspace --all-features --fail-under-lines 80` | 0 | PASS (TOTAL 80.72% lines; harness lib covered, `main.rs` CLI covered only via strict runs) |
| 17 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (20/20 incl. `fs`/`url-shim`/`structured-clone` on/off + wpt crate) |
| 18 | `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; only pre-existing duplicate-version warnings; zero new deps — SBOM confirms) |
| 19 | `cargo tree --workspace --all-features --prefix none > target/sbom.txt` | 0 | PASS (SBOM artifact, never committed) |
| 20 | `git diff --check` | 0 | PASS |

## Feature matrix

| Configuration | Result |
|---|---|
| `--all-features` | PASS (full workspace incl. 25 M7 tests + 15 harness units + 38 strict subtests) |
| `--no-default-features` (`cargo test`) | NOT RUN by design (accepted M6 decision: shim-required suites fail without default features; `cargo hack --feature-powerset` proves every combination compiles) |
| WPT `--filter` (non-strict) / `--timeout-ms` | PASS (diagnostic subset keeps strict semantics) |
| WPT `--strict --threads 2` | PASS (isolated workers, SHA-256-identical JSON to `--threads 1`) |
| WPT `--strict --filter` | EXPECTED LAUNCH ERROR exit 2 (F1 — filter is diagnostic-only) |

## Bounded hooks evidence (seed / duration / baseline / result)

- `differential_smoke`: fixed corpus `new Blob(['abc'])` → `3:`; Node v24.19.0 present → match PASS; without node → SKIP with reason (never fake PASS).
- `fuzz_decode_bounded`: seed `0x9E3779B97F4A7C15`, 512 cases, 64 KiB cap, <1s → explored=512, no panic.
- `bench_smoke`: clone×5 + resolve×200 on 256 KiB blob → informational medians printed, completion asserted, no absolute-path baseline published.
- `leak_repeat`: 8× create/revoke/shutdown cycles → counts return to 0, fresh contexts isolated.
- Nightly: `.github/workflows/nightly.yml` (Ubuntu nightly install may SKIP explicitly, but a started Miri test failure fails the job; fixed-seed fuzz has timeout-minutes; Windows is compatible-only).

## Rework F1–F11 (review `docs/reviews/M7-rework.md`, base `b50c60e`)

- F1 (strict+filter): `--strict --filter` → exit 2 launch error before manifest read; filter-only stays diagnostic (22-subtest subset verified).
- F2 (paths): `corpus_root` manifest field + logical-path policy + `Path` join + canonicalize + strict containment + symlink rejection at every level; hashes over raw bytes; details carry only logical `file.path`. Traversal/absolute/backslash/NUL/suffix/symlink/outside-root fixtures → exit 2 (unit + boundary verified; Windows empty-parent CWD quirk fixed and documented in code).
- F3 (NOTRUN): `ActualStatus::NotRun` end to end — runner emits it, JSON prints actual `NOTRUN`, JUnit prints `<skipped>` without `<failure>` or `failures` count, summary counts it separately, strict compares enums (detail prefix retired).
- F4 (async errors): `record_once`/`safe_error`/`run_step` prelude (double-done/cleanup/throwing-callback FAIL, `PASS`-after-`FAIL` blocked); job-error flag and top-level-throw-poisons-file in runner; async assertion throw verified FAIL, never TIMEOUT.
- F5 (isolation): `--worker-file <index>` child processes via `current_exe` (validated ID only, no shell/command string), bounded stdout (2 MiB) / stderr (64 KiB), `Child::kill` at wall deadline → `TIMEOUT`; `run_file` kept as library mapping; worker stack inherits the 8 MiB thread (Windows overflow fixed).
- F6 (threads): `min(N, files)` slots, index-chunked isolated workers, `BTreeMap` re-sort — `--threads 2` JSON SHA-256-identical to `--threads 1` (hashes below).
- F7 (mapping): register/prelude/readback are CLI launch errors; file-eval is FAIL rows; timeout/crash/protocol corruption is TIMEOUT; NOTRUN remains an explicit capability gap; empty vectors are impossible and top-level throw poisons the file.
- F8 (manifest): required `corpus_root`, HTTPS WPT repository check, group allow-list + all-six gate, full per-subtest records, real calendar dates, duplicate-key rejection, path/name/meta/corpus/timeout limits, no absolute paths in schema errors.
- F9 (secrecy): scrubber covers mid-token `blob:` (punctuation-preserving), file/HTTP/drive/UNC/abs paths, controls (XML 1.0 validity), 480-scalar/48-token char-boundary cap, `<redacted-error>` fallback; serializer checks for quote/amp/NUL/Unicode/paths + JSON round-trip.
- F10 (lint): crate-wide `allow(clippy::expect_used)` removed; per-test-module allows only; workspace clippy green.
- F11 (CI): job renamed M7 validation, M7 order comment, full-manifest strict + threads-2 + pwsh JSON report check, artifacts after generation; nightly split Ubuntu(miri run)/Windows(note) with bounded fuzz + timeout-minutes and explicit SKIP reasons.

## Rework-2 F12–F17 (review `docs/reviews/M7-rework-2.md`, base `aed9a0f`)

- F12 (hard timeout for default CLI): every CLI file execution — including `--threads 1` — runs through `run_file_isolated` with wall kill; `run_file` stays library-only mapping. Boundary: full strict exit 0 (38 passed, 0 notrun, 0 unexpected); threads 1 and 2 both isolated with identical SHA-256 JSON (`4D5CEC74…9B9CD`).
- F13 (typed worker protocol): `WORKER-OK` (fully verified row) / `WORKER-TIMEOUT` (reserved) / `WORKER-ERROR register|prelude|readback|file-eval|protocol`; kill/crash/overflow/corruption → `TIMEOUT` (never PASS/NOTRUN); register/prelude/readback → CLI launch error exit 2; file-eval → FAIL rows; protocol checks include exit status, concurrent bounded pipe drains, single line, UTF-8, size, path/count/ID/expected/token match, and scrubbed detail.
- F14 (duplicate JSON keys): recursive rejection with `DuplicateJsonKey(key)` — root/source/files/subtests covered; only the key name revealed. Tests: root, source, subtest keys, mixed value types.
- F15 (embedded leakage): `blob:`/`file:`/HTTP(S)/drive/UNC/abs-Unix matched anywhere in the token (after `=`/`:`/`(`/`[`/`{`), punctuation-preserving; host kept for HTTP(S), fixed placeholders elsewhere; 480-scalar/48-token char-boundary cap; `<redacted-error>` fallback. Tests: `url=https://…`, `(https://…)`, `path=/tmp/x`, `(file:///tmp/x)`, drive-with-comma, UNC-in-parens, 480-scalar Unicode boundary, controls, JSON/XML round-trip.
- F16 (repository identity): exact canonical `https://github.com/web-platform-tests/wpt` only — `wpt-malicious`, extra path, query, fragment, userinfo, `http://`, trailing slash all rejected; canonical valid URL loads.
- F17 (subtest collision): per-file `seen_names` (variant A) — same subtest under two test IDs in one file → `DuplicateSubtest` load error; cross-file reuse stays allowed (both proven by tests); harness protocol unchanged (no composite-key format change).

## M6 regression

All M1–M6 suites green without modification (only additive M7 files).
`--no-default-features` stance unchanged from the accepted M6 decision.

## CI

- External CI (GitHub Actions, Ubuntu + Windows) status at handoff time: **NOT RUN YET** — local validation above is on Windows; Unix-only paths (`#[cfg(unix)]` live-handle/snapshot/fs-race) execute in the Ubuntu job. The implementation commit and the CI run link will be recorded in `docs/reviews/M7-handoff.md` after push.
- Reports/SBOM are CI artifacts (`target/wpt-report.json`, `target/wpt-report.xml`, `target/sbom.txt`), never committed.
