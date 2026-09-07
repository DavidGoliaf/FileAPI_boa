# WPT harness (M7)

Pinned source, adaptation, CLI, status model, expectations rules,
capability taxonomy, report examples and limitations.

## Pinned source and integrity

- Upstream: `https://github.com/web-platform-tests/wpt`, commit
  `0968c868d8095217d18d86b34c7f21dccae58768` (`master` on 2026-09-07),
  license `BSD-3-Clause` (upstream `LICENSE.md`).
- Stored files are **adapted** JS cases in
  `crates/boa_fapi_wpt/corpus/*.js` — assertion subsets rewritten for
  supported capabilities, not verbatim copies of full copyrighted pages
  (upstream path + blob SHA recorded per file for provenance).
- `wpt-manifest.json` stores SHA-256 of each adapted file; the harness
  verifies hashes before execution — mismatch is a launch error, never
  `NOTRUN`. Full upstream pass is **not** claimed: conformance covers
  the adapted set only (see QUESTIONS.md Q1–Q3 for the supply decision).

## Manifest schema (`schema_version: 1`)

```text
{
  "schema_version": 1,
  "source": { "repository", "commit" (40 lowercase hex), "license" },
  "default_timeout_ms": u64 > 0,
  "files": [{
    "path": "corpus/*.js", "upstream_path": "FileAPI/**",
    "upstream_blob_sha": hex40, "sha256": hex64,
    "group": "FileAPI/<group>", "capability": "<cap>",
    "subtests": [{
      "test": "<id>", "subtest": "<name>", "status": "PASS" | "NOTRUN",
      "timeout_ms"?: u64,
      "reason" (required unless PASS), "capability", "owner",
      "review_by": "YYYY-MM-DD", "trace": "M7-*"
    }]
  }]
}
```

Rules: exact test/subtest entries, no wildcards (`*` rejected at load),
no file-only expectations (every file lists ≥1 subtest), unknown status
is a load error, expired `review_by` fails the load (UTC date), empty
file list is a load error. `NOTRUN` without `reason`/`capability`/
`owner`/`review_by`/`trace` is a load error — capability gaps are never
free-form comments.

## CLI

```text
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
  [--threads N] [--filter <exact-or-prefix>] [--json <path>] [--junit <path>]
  [--timeout-ms <N>]
```

- `--threads N`: accepted for interface parity; files always run
  sequentially in manifest order (determinism first — parallelism is not
  claimed; `N > 1` currently returns a launch error instead of silently
  ignoring the flag).
- `--filter`: diagnostic subset without changing strict-gate semantics
  (empty match is a launch error).
- `--timeout-ms`: overrides per-subtest pump budget; default from manifest.
- Missing manifest, bad schema version, hash mismatch, unknown status or
  incomplete expectation → launch error (exit 2), never `NOTRUN`.

## Status model

Per test/subtest exactly one terminal status:

- `PASS` — the named subtest recorded exactly one passing harness entry;
- `FAIL` — a failing entry, a top-level JS throw, a duplicate entry, or
  a recorded name without manifest expectation;
- `TIMEOUT` — async entries still pending after the bounded pump budget
  (64 `run_jobs()` passes + wall guard, no sleep);
- `NOTRUN` — reported for `NOTRUN`-expected rows with the manifest gap
  reason when the file evaluated cleanly (a top-level throw turns the
  row into `FAIL` instead).

Strict gate (`--strict`, non-zero on): every `PASS`-expected row is
actually `PASS`; every `NOTRUN`-expected row carries its `notrun: `
reason; no unexpected rows. New tests, subtests or FAILs break CI.

Timeout is a harness verdict, distinct from JS failure: the file runs in
a fresh `Context` per file, so a timeout cannot leave a live job that
mutates later tests. No panic/crash path is silent: runner errors are
launch errors.

## Adaptation (`.any.js` / HTML JS-only)

- Upstream `.any.js` cases run as plain scripts (no worker orchestration
  needed for the adapted subset); JS-only parts of HTML WPT are
  extracted as adapted files.
- Provided prelude: `test`/`async_test`/`promise_test`, `assert_true`/
  `assert_false`/`assert_equals`/`assert_not_equals`/
  `assert_array_equals`/`assert_throws_js`/`assert_throws_exactly`/
  `assert_unreached`, `format_value`, `done`/`step`/`step_func`,
  `add_cleanup`. No upstream `testharness.js` fetch, no DOM.
- Async settlement is observed through explicit `Context::run_jobs()`
  pumping (bounded passes); sync assertions record immediately.

## Capability taxonomy

`blob-constructor`, `blob-slice`, `file-constructor`, `filelist`,
`promise-reads`, `filereader`, `blob-url`. Anything needing HTML input
UI, navigation, Fetch, MediaSource, real browser Window/Worker
orchestration or the WPT server is a `NOTRUN` gap with exact
`reason` + `capability` + `owner` + `review_by` + `trace` — never a
generic `browser-only` comment, never a wildcard.

## Reports

- JSON (`--json`, else stdout): `schema_version`, `source`,
  `strict_pass`, `files[]` in manifest order with per-subtest
  `test`/`subtest`/`actual`/`expected`/`trace`/`detail`. Keys fixed
  order; no timestamps, random IDs or absolute paths in the compared
  section (`elapsed_ms` is intentionally omitted from reports).
- JUnit (`--junit`): `<testsuites>` with per-file `<testsuite>` and
  per-subtest `<testcase>`; non-passing rows carry `<failure>`.
- Detail scrubber redacts `blob:` URLs (`blob:<redacted>`) and truncates
  to 480 chars / 48 tokens.

Example (truncated):

```json
{"schema_version":1,"source":{"repository":"https://github.com/web-platform-tests/wpt","commit":"0968c868…","license":"BSD-3-Clause"},"strict_pass":true,"files":[{"path":"corpus/blob-slice.js","upstream_path":"FileAPI/blob/Blob-slice.any.js","group":"FileAPI/blob","subtests":[{"test":"blob-slice","subtest":"Blob slice positive range","actual":"PASS","expected":"PASS","trace":"M7-WPT-01","detail":""}]}]}
```

## Limitations

- Adapted subset only (7 files / 38 subtests); full `FileAPI/**` needs
  browser capabilities out of scope (Fetch, navigation, workers, server).
- Single-worker deterministic; `--threads` is interface parity only
  (sequential manifest order, `N > 1` is a launch error, not a speed
  claim).
- Wall-clock appears only as an internal pump guard; reports carry no
  timing comparisons.
