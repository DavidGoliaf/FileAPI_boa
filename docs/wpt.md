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
  "corpus_root": "crates/boa_fapi_wpt/corpus",
  "default_timeout_ms": u64 1..=300000,
  "files": [{
    "path": "corpus/*.js", "upstream_path": "FileAPI/**",
    "upstream_blob_sha": hex40, "sha256": hex64,
    "group": "FileAPI/<group>", "capability": "<cap>",
    "subtests": [{
      "test": "<id>", "subtest": "<name>", "status": "PASS" | "NOTRUN",
      "timeout_ms"?: u64 1..=300000,
      "reason" (required unless PASS), "capability", "owner",
      "review_by": "YYYY-MM-DD", "trace": "M7-*"
    }]
  }]
}
```

Rules: exact test/subtest entries, no wildcards (`*` rejected at load),
no file-only expectations (every file lists ≥1 subtest), unknown status
is a load error, expired `review_by` fails the load (UTC date), empty
file list is a load error, duplicate adapted paths and duplicate
test/subtest pairs are load errors. `NOTRUN` without `reason`/
`capability`/`owner`/`review_by`/`trace` is a load error — capability
gaps are never free-form comments. All six mandatory groups must be
present (`FileAPI/blob`, `FileAPI/file`, `FileAPI/filelist-section`,
`FileAPI/reading-data-section`, `FileAPI/FileReader`, `FileAPI/BlobURL`).
Paths: `file.path` is a logical name (`corpus/<name>.js` only — no
traversal, absolute form, drive prefix, backslash, NUL/controls or
non-`.js` suffix); resolution joins `corpus_root` (manifest-relative)
via `Path`, canonicalizes, requires strict containment and rejects any
symlink in root or candidate. Hashes run over raw bytes after UTF-8
validation.

## CLI

```text
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
  [--threads N] [--filter <exact-or-prefix>] [--json <path>] [--junit <path>]
  [--timeout-ms <N>]
```

- `--threads N`: `1` runs in-process in manifest order (deterministic);
  `N > 1` runs `min(N, files)` slots, each file in an isolated child
  process of the same executable (`--worker-file <index>`, validated
  file ID only — never an arbitrary path or shell string), killed at the
  wall deadline (kill → `TIMEOUT`); results re-sort by manifest index,
  so `--threads 2` JSON is byte-identical to `--threads 1` for the same
  manifest (verified by SHA-256 in validation). No `Context`/`JsObject`
  crosses workers; one file's failure never drops another file's row.
- `--filter`: diagnostic subset without changing strict-gate semantics
  (empty match is a launch error). `--strict` + `--filter` is a launch
  error (`--filter cannot be combined with --strict`): the full run
  without filter is the only CI gate.
- `--timeout-ms`: overrides per-subtest pump budget, range 1..=300000
  (manifest `timeout_ms` and `default_timeout_ms` share the range);
  each file additionally runs under the wall deadline in its worker.
- Missing manifest, bad schema version, hash mismatch, unknown status or
  incomplete expectation → launch error (exit 2), never `NOTRUN`.

## Status model

Per test/subtest exactly one terminal status (enum-compared, never
detail-prefix-matched):

- `PASS` — the named subtest recorded exactly one passing harness entry;
- `FAIL` — a failing entry, a top-level JS throw (fails every row of the
  file — no PASS row survives it), a duplicate entry, a job error during
  the pump, or a recorded name without manifest expectation;
- `TIMEOUT` — async entries still pending after the bounded pump budget
  (64 `run_jobs()` passes + wall guard, no sleep), a killed worker past
  its wall deadline, or worker output overflow/corruption;
- `NOTRUN` — actual status for `NOTRUN`-expected rows when the file
  evaluated cleanly, with detail `notrun: <manifest reason>` (a
  top-level throw turns the row into `FAIL` instead).

Strict gate (`--strict`, non-zero on): `PASS` expects only actual `PASS`;
`NOTRUN` expects only actual `NOTRUN`; `FAIL`/`TIMEOUT` always break
strict (no manifest status can expect them). New tests, subtests or
FAILs break CI.

Timeout is enforced by the CLI process boundary (F5): each file runs in
an isolated child process of the same executable, killed at the wall
deadline (kill → `TIMEOUT` with non-zero worker detail); the child owns
its `Context`, so after the process exits no live Boa job can mutate
later tests. The library `run_file` keeps its bounded pump budget for
unit use. No panic/crash path is silent: runner errors are launch
errors or documented FAIL rows (see the F7 mapping table in
`docs/m7-validation.md`).

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
  `test`/`subtest`/`actual`/`expected` (`NOTRUN` is a real actual
  status)/`trace`/`detail`. Keys fixed order; no timestamps, random IDs
  or absolute paths in the compared section (`elapsed_ms` is
  intentionally omitted from reports).
- JUnit (`--junit`): `<testsuites>` with per-file `<testsuite>`
  (`failures` counts only `FAIL`/`TIMEOUT`, plus a `skipped` count) and
  per-subtest `<testcase>`; `NOTRUN` rows carry `<skipped>` without
  `<failure>`, `FAIL`/`TIMEOUT` rows carry `<failure>`.
- Detail scrubber (F9): every `blob:` occurrence (prefix- or
  punctuation-adjacent, trailing `)`/`,`/`;` preserved) → `blob:<redacted>`;
  `file://` → `file:<redacted>`; HTTP(S) → scheme+host plus
  `<redacted-path>`; drive/UNC/absolute-Unix → fixed placeholders;
  controls except tab/newline/CR dropped (XML 1.0 validity); 480 Unicode
  scalars / 48 tokens on char boundaries; unclassifiable input →
  `<redacted-error>`. XML escape additionally drops XML 1.0 illegal chars.

Example (truncated):

```json
{"schema_version":1,"source":{"repository":"https://github.com/web-platform-tests/wpt","commit":"0968c868…","license":"BSD-3-Clause"},"strict_pass":true,"files":[{"path":"corpus/blob-slice.js","upstream_path":"FileAPI/blob/Blob-slice.any.js","group":"FileAPI/blob","subtests":[{"test":"blob-slice","subtest":"Blob slice positive range","actual":"PASS","expected":"PASS","trace":"M7-WPT-01","detail":""}]}]}
```

## Limitations

- Adapted subset only (7 files / 38 subtests); full `FileAPI/**` needs
  browser capabilities out of scope (Fetch, navigation, workers, server).
- Every CLI file execution is an isolated worker with a wall deadline and
  kill boundary. `N = 1` runs workers sequentially; `N > 1` schedules them
  concurrently, with manifest-order output (byte-identical JSON for the same
  manifest).
- Wall-clock appears only as the CLI process-boundary deadline and the
  internal pump guard; reports carry no timing comparisons.
