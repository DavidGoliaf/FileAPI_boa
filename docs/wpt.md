# WPT conformance gate (M9-E)

Pinned source, inventory, two run modes, CLI, status model, expectations
rules, capability taxonomy, adaptation fidelity, FileList fixture, report
examples and limitations. M7 history stays in `docs/reviews/M7-handoff.md`;
this file is normative for the M9-E gate.

## Pinned source, inventory and integrity

- Upstream: `https://github.com/web-platform-tests/wpt`, commit
  `0968c868d8095217d18d86b34c7f21dccae58768` (`master` on 2026-09-07),
  license `BSD-3-Clause` (upstream `LICENSE.md`).
- Inventory: `wpt-inventory.json` (schema 1, scope `FileAPI/`, 115 files),
  generated deterministically from the pinned git tree without checkout
  and without network at verify time:
  `python tools/gen-wpt-inventory.py`
  (`TEMP/wpt-upstream/.git` over pinned `FETCH_HEAD` via `git cat-file`).
  `inventory_sha256`:
  `5d121cd1b7789843a4685184b18ef13028ec128870b49472322046706e569a3f`.
- Stored corpus: `crates/boa_fapi_wpt/corpus/*.js` holds the 36 raw pinned
  `.any.js` upstream files byte-identically (`direct` provenance) plus one
  handwritten project-owned FileList fixture (`filelist-host.js`,
  `adapted` provenance, never presented as WPT). Corpus bytes are
  materialized by `python tools/gen-m9e-gate.py` from the pinned git tree
  (hand-audited titles in `tools/upstream-titles.json`); no full upstream
  page outside `FileAPI/**` is stored.
- `wpt-manifest.json` (schema 2, 37 files / 417 subtests) stores per file
  `upstream_path`, `upstream_blob_sha` (provenance only, never evidence),
  `upstream_sha256` (raw-content evidence, verified against
  `--upstream-root`), `sha256` of the stored corpus bytes, `provenance`
  (`direct` or `adapted` + `adapter` id), optional `fixture`, and the exact
  subtest list. The harness verifies corpus hashes before execution and
  the strict gate verifies every manifest `upstream_sha256` against raw
  `--upstream-root` bytes plus the full inventory set — any hash,
  inventory, or adaptation drift is a launch error, never `NOTRUN`.
- Full upstream pass is **not** claimed where host capabilities are
  missing: those rows are exact `NOTRUN` exclusions (see below), never
  `PASS`.

## Two modes, different claims

Fast offline smoke for development (adapter/harness check only):

```powershell
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
```

It runs the same corpus but the summary and reports are always labelled
`ADAPTED_SMOKE`, never `WPT conformance`.

Normative gate:

```powershell
cargo run --package boa_fapi_wpt -- `
  --manifest wpt-manifest.json `
  --expectations expectations.json `
  --upstream-root <PINNED_WPT_CHECKOUT> `
  --strict
```

`--strict` exits non-zero on hash/inventory/adaptation drift, missing or
duplicate result, unexpected PASS/FAIL/TIMEOUT/NOTRUN, expired exclusion,
or a supported capability wrongly marked NOTRUN. A recorded open defect
(expected FAIL, actual FAIL) satisfies the strict comparison (exit 0,
`strict_pass: true`) and keeps only the release gate red (stderr note,
`release_green: false`). The summary always shows
upstream PASS, adapted smoke PASS, open defects, and exclusions
separately. `--strict` requires `--expectations` plus `--upstream-root`;
`--smoke` takes neither; the two modes are exclusive.

## Manifest schema (`schema_version: 2`)

```text
{
  "schema_version": 2,
  "source": { "repository", "commit" (40 lowercase hex), "license" },
  "corpus_root": "crates/boa_fapi_wpt/corpus",
  "default_timeout_ms": u64 1..=300000,
  "files": [{
    "path": "corpus/*.js", "upstream_path": "FileAPI/**",
    "upstream_blob_sha": hex40, "upstream_sha256": hex64, "sha256": hex64,
    "group": "FileAPI/<group>", "capability": "<cap>",
    "provenance": "direct" | "adapted", "adapter"?: "<id>",
    "fixture"?: "filelist",
    "subtests": [{
      "test": "<id>", "subtest": "<name>",
      "status": "PASS" | "FAIL" | "NOTRUN",
      "timeout_ms"?: u64 1..=300000,
      "reason" (required unless PASS), "capability", "owner",
      "review_by": "YYYY-MM-DD", "trace": "M9E-*",
      "classification": "supported" | "unsupported-host-capability"
        | "harness-gap" | "project-acceptance",
      "spec_section": "<section>", "issue": "<link>"
    }]
  }]
}
```

Rules: exact test/subtest entries, no wildcards (`*` rejected at load),
no file-only expectations (every file lists ≥1 subtest), unknown status
is a load error, expired `review_by` fails the load (UTC date), empty
file list is a load error, duplicate adapted paths, duplicate
test/subtest pairs, and duplicate subtest names inside one file are load
errors. `NOTRUN`/`FAIL` without `reason`/`capability`/`owner`/
`review_by`/`trace` is a load error — gaps and defects are never
free-form comments. `supported` is never `NOTRUN`; expected `FAIL` needs
an open-defect `issue` link (`supported` + `FAIL` satisfies the strict
comparison but keeps the release gate red); `harness-gap` breaks the
release gate (load error); `unsupported-host-capability` needs a
closed-list capability, never `browser-only`. Paths: `file.path` is a
logical name (`corpus/<name>.js` only — no traversal, absolute form,
drive prefix, backslash, NUL/controls or non-`.js` suffix); resolution
joins `corpus_root` (manifest-relative) via `Path`, canonicalizes,
requires strict containment and rejects any symlink in root or
candidate; case collisions between two inventory paths resolving to one
filesystem path are a launch error. Hashes run over raw bytes after
UTF-8 validation.

## CLI

```text
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --smoke
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --expectations expectations.json --upstream-root <PINNED_WPT> --strict
  [--threads N] [--filter <exact-or-prefix>] [--json <path>] [--junit <path>]
  [--timeout-ms <N>]
```

- `--threads N`: every file runs in an isolated child process of the same
  executable (`--worker-file <index>`, validated file ID only — never an
  arbitrary path or shell string), killed at the wall deadline
  (kill → `TIMEOUT`); results re-sort by manifest index, so
  `--threads 2` JSON is byte-identical to `--threads 1` for the same
  manifest (verified by SHA-256 in validation). No `Context`/`JsObject`
  crosses workers; one file's failure never drops another file's row.
- `--filter`: diagnostic subset without changing strict-gate semantics
  (empty match is a launch error). `--strict` + `--filter` and `--smoke`
  + `--filter` are launch errors: the full run without filter is the only
  gate.
- `--timeout-ms`: overrides per-subtest pump budget, range 1..=300000
  (manifest `timeout_ms` and `default_timeout_ms` share the range);
  each file additionally runs under the wall deadline in its worker.
- Missing manifest, bad schema version, hash mismatch, inventory drift,
  unknown status, incomplete expectation, or manifest/expectations drift
  → launch error (exit 2), never `NOTRUN`.

## Status model

Per test/subtest exactly one terminal status (enum-compared, never
detail-prefix-matched):

- `PASS` — the named subtest recorded exactly one passing harness entry;
- `FAIL` — a failing entry, a top-level JS throw (fails every row of the
  file — no PASS row survives it), a duplicate entry, a job error during
  the pump, or a recorded name without manifest expectation. Expected
  `FAIL` (open defect with an `issue` link) satisfies the strict
  comparison but never releases (see `release_green`);
- `TIMEOUT` — async entries still pending after the bounded pump budget
  (64 `run_jobs()` passes + wall guard, no sleep), a killed worker past
  its wall deadline, or worker output overflow/corruption. No expectation
  can expect `TIMEOUT`;
- `NOTRUN` — actual status for `NOTRUN`-expected rows when the file
  evaluated cleanly, with detail `notrun: <manifest reason>` (a
  top-level throw turns the row into `FAIL` instead).

Strict gate (`--strict`, non-zero on): `PASS` expects only actual `PASS`;
`FAIL` expects only actual `FAIL`; `NOTRUN` expects only actual `NOTRUN`;
anything else breaks strict. New tests, subtests, unexpected PASS, or
unexpected FAIL/TIMEOUT break the gate.

Timeout is enforced by the CLI process boundary: each file runs in an
isolated child process of the same executable, killed at the wall
deadline (kill → `TIMEOUT` with non-zero worker detail); the child owns
its `Context`, so after the process exits no live Boa job can mutate
later tests. The library `run_file` keeps its bounded pump budget for
unit use. No panic/crash path is silent: runner errors are launch
errors or documented FAIL rows.

## Expectations and capability accounting (`expectations.json`)

496 exact rows (schema 1, same pinned repository/commit as the manifest):
374 `PASS`/`supported`, 5 `FAIL`/`supported` (open defects, see below), 6
`PASS`/`project-acceptance` (FileList fixture, never WPT), 111
`NOTRUN`/`unsupported-host-capability`. Every relevant upstream
test/subtest has one row with `upstream_path`, test/subtest id, expected
status, closed-list `capability`, exact reason, owner, issue/question
link, `review_by`, `adapter` (`direct`, or the fixture adapter id), trace
ID, and normative section. Wildcards and file-level catch-alls are
forbidden; the gate resolves manifest rows against expectations rows by
exact `(test, subtest)` id and rejects any drift (status, reason,
capability, owner, review, trace, classification, section, issue).
`DYNAMIC:` tracker rows cover dynamic-title matrices that never execute
(`url-with-fetch`/`url-with-xhr`, non-ASCII Blob type matrix).

Capability allow-list (never `browser-only`):
`html-file-input`, `navigation`, `fetch`, `mediasource`,
`worker-runtime`, `network-wpt-server`, plus the crate's own implemented
capabilities (`blob-constructor`, `blob-slice`, `promise-reads`,
`file-constructor`, `filereader`, `filelist`, `blob-url`).

Open defects (5, all `supported`/`FAIL`, release-red):

1. `filereader_abort.any.js :: Aborting after read` (capability
   `filereader`, issue `docs/reviews/M9E-handoff.md §3`): the test's own
   `.then()` continuation re-arms `wait_for(['abort','loadend'])` and
   calls `abort()` a second time after the sync dispatch already
   delivered the pair; the harness observes a phantom second pair
   (`2 !== 1`). The product dispatches exactly one `abort`+`loadend`
   pair per `abort()` (TZ §7.3); fixing the double-delivery observation
   needs upstream EventWatcher queue semantics.
2. `fileReader.any.js :: FileReader States -- abort`: upstream requires
   the `abort` handler to run before `abort()` returns (sync dispatch),
   but the M9-C executor protocol queues the abort terminal through
   `poll_io`/`run_jobs`, so the `unreached_func` reassignment lands
   first. Same terminal state, different delivery turn; no product
   change in M9-E.
3. `filereader_readAsDataURL.any.js :: readAsDataURL result for Blob
   with unspecified MIME type` and `:: readAsDataURL result for empty
   Blob`: upstream expects `data:application/octet-stream;base64,...`
   for empty-type Blobs, but the crate contract (M4, pinned by the
   `m4_filereader_async`/`m4_filereader_sync` data-URL suites) emits the
   type verbatim (`data:;base64,...`).
4. `File-constructor.any.js :: No replacement when using special
   character in fileName`: upstream expects `dummy/foo` verbatim, but
   the normative File API replaces every `/` with `:` and the crate
   (M2 `normalize_file_name`) emits `dummy:foo`.

Recorded as open defects — strict comparison accepts the actual FAILs,
the release gate stays red.

## Adaptation fidelity (`.any.js` direct)

Upstream `.any.js` files execute byte-identically (raw pinned bytes,
`direct` provenance) under a minimal pinned testharness compatibility
layer (`test`/`async_test`/`promise_test`, `EventWatcher` + `wait_for`,
`assert_*`, `format_value`, `setup`, `test_blob`/`test_blob_binary`,
`TextEncoder` UTF-8 subset, `garbageCollect` no-op hook). No upstream
`testharness.js` fetch, no DOM, no network. Fidelity rules: no custom
`@@iterator`/ToString/exception-order/race case is removed, no rewritten
scenario is renamed to an upstream subtest, no assertion is shortened for
Boa compatibility, no PASS rests on a merely similar local test. Where a
single assertion needs a missing platform capability, that subtest is an
exact `NOTRUN` (never a rewritten simpler oracle). Untitled tests inside
title-computing loops take their exact runtime title from the
`__wpt_next_title` FIFO (pre-filled in manifest order); an empty FIFO is
FAIL, never an invented id. `async_test` `done()` after completion is a
no-op (upstream `Test.done()` past COMPLETE returns silently); steps
after settle are dropped. `EventWatcher.wait_for` keeps the pinned
ordered-sequence semantics (`['abort','loadend']` waits for the
sequence, not the first) with late-subscribe from fired-event history
(single-string waits resolve at once when already fired; array waits arm
live so stale history never fabricates a pair) and drops non-head
intermediates (empty-blob `progress` skip).

Host loop per file (M9-B/M9-C/M9-D): fresh `Context`, prelude, optional
fixture, upstream source evaluation, then the bounded `poll_io` +
`run_jobs()` pump (wake-gated, wall-guarded). Promise/FileReader/Streams
tests settle only through this loop — never via a synchronous source
read inside a job.

## Host fixtures and FileList (M9-E §6)

The runner owns one per-file fixture hook executed on the Boa thread
before any test JS runs. The single declared fixture `filelist` (file
`corpus/filelist-host.js`, `project-acceptance`, adapter
`m9e-filelist-fixture-01`):

1. creates two distinct `File` objects through the public host API
   (`FileApiHandle::file_from_bytes`: `a.txt` = `a`, `b.txt` = `bb`);
2. builds a real `FileList` via `FileApiHandle::file_list`;
3. injects only that object under the reserved `globalThis.__wpt_file_list`
   binding (brand-proofed to `[object FileList]`, install fails otherwise);
4. the project-owned JS asserts `length`, identity/order, `item()`,
   indexed access, out-of-range behavior, descriptors (`length` own data
   property, read-only), the indexed-getter iteration contract
   (`Symbol.iterator === Array.prototype.values`, no
   `entries`/`keys`/`values`/`forEach`), and the absence of a public
   constructor.

A plain Array or a pair of Files never satisfies the fixture. Iterator
methods beyond the indexed getter are not added: the pinned W3C FileList
IDL declares none.

## Minimum upstream set (M9-E §7)

Supported/direct coverage holds the full applicable subtests of the
pinned files for: Blob constructor, BlobPart conversion, slicing, MIME
and promise reads; File constructor/name/type/lastModified; FileList
host-created surface (project-owned fixture, see above); FileReader
states, read methods, encoding, events and abort; Blob URL
create/revoke/isolation for the Fetch/navigation-free subset
(`Generated Blob URLs are unique`, both `starts with "blob:"` rows;
origin/parse rows are exact `navigation` NOTRUN — the URL shim is
create/revoke-only by M6 design); serialization-adjacent cases through
the existing clone bridge stay covered by `m6_structured_clone`.
M9-A regression cases (iterable outer sequence, primitive/object
BlobPart fallback, throwing conversion/IteratorClose, unknown encoding
fallback) stay covered by `m9_webidl_conformance` and keep their
`PROJECT_ACCEPTANCE` classification where no pinned upstream subtest
exists — never presented as WPT.

## Trace IDs

| Trace ID | Requirement |
|---|---|
| `M9E-WPT-01` | pinned full FileAPI inventory and raw upstream hashes |
| `M9E-WPT-02` | direct/adapted provenance and fidelity gate |
| `M9E-WPT-03` | exact expectations and capability accounting |
| `M9E-WPT-04` | real FileList host fixture |
| `M9E-WPT-05` | async host poll integration in runner |
| `M9E-WPT-06` | deterministic JSON/JUnit summary separating WPT and smoke |

## Reports

- JSON (`--json`, else stdout): `schema_version`, `mode`
  (`ADAPTED_SMOKE`/`WPT_STRICT`), `source`, `summary` (`upstream_pass`,
  `smoke_pass`, `defects`, `exclusions`, `unexpected`), `strict_pass`,
  `files[]` in manifest order with per-subtest
  `test`/`subtest`/`actual`/`expected`/`trace`/`detail`. Keys fixed
  order; no timestamps, random IDs or absolute paths in the compared
  section (`elapsed_ms` is intentionally omitted from reports).
- JUnit (`--junit`): `<testsuites>` with per-file `<testsuite>`
  (`failures` counts only `FAIL`/`TIMEOUT`, plus a `skipped` count) and
  per-subtest `<testcase>`; `NOTRUN` rows carry `<skipped>` without
  `<failure>`, `FAIL`/`TIMEOUT` rows carry `<failure>`.
- Detail scrubber: every `blob:` occurrence (prefix- or
  punctuation-adjacent, trailing `)`/`,`/`;` preserved) → `blob:<redacted>`;
  `file://` → `file:<redacted>`; HTTP(S) → scheme+host plus
  `<redacted-path>`; drive/UNC/absolute-Unix → fixed placeholders;
  controls except tab/newline/CR dropped (XML 1.0 validity); 480 Unicode
  scalars / 48 tokens on char boundaries; unclassifiable input →
  `<redacted-error>`. XML escape additionally drops XML 1.0 illegal chars.

Example (truncated):

```json
{"schema_version":1,"mode":"WPT_STRICT","source":{"repository":"https://github.com/web-platform-tests/wpt","commit":"0968c868…","license":"BSD-3-Clause"},"summary":{"upstream_pass":374,"smoke_pass":6,"defects":5,"exclusions":35,"unexpected":0},"strict_pass":true,"files":[{"path":"corpus/blob-slice.js","upstream_path":"FileAPI/blob/Blob-slice.any.js","group":"FileAPI/blob","subtests":[{"test":"Blob-slice.any.js","subtest":"no-argument Blob slice","actual":"PASS","expected":"PASS","trace":"M9E-WPT-02","detail":""}]}]}
```

## Limitations

- Conformance is claimed for the 36 directly executed upstream `.any.js`
  files (417 executable subtests: 374 upstream PASS, 5 recorded open
  defects, 32 exclusions) plus 3 `DYNAMIC:` tracker rows, plus 79
  file-level inventory exclusions (exact `NOTRUN` rows in
  `expectations.json`, never executed); the rest of `FileAPI/**` needs
  browser capabilities out of scope (Fetch, navigation incl. WHATWG URL
  parsing, workers, WPT server).
- Every CLI file execution is an isolated worker with a wall deadline and
  kill boundary. `N = 1` runs workers sequentially; `N > 1` schedules them
  concurrently, with manifest-order output (byte-identical JSON for the
  same manifest).
- Wall-clock appears only as the CLI process-boundary deadline and the
  internal pump guard; reports carry no timing comparisons.
- Known open defects (5, release-red): the `filereader_abort` phantom
  second pair, the `fileReader.any.js` sync-abort delivery turn, the two
  empty-type `readAsDataURL` rows, and the `File` slash row (see
  Expectations above and `docs/spec-delta.md`). The strict gate accepts
  the recorded FAILs; the release gate stays red.
