# Spec delta — WD 23.08.2026 vs pinned WPT reference

Reference: upstream `web-platform-tests/wpt`
`0968c868d8095217d18d86b34c7f21dccae58768` (`master` 2026-09-07).
Normative base stays WD 23.08.2026 per TZ §1.6; only behaviorally
verifiable divergences observed through the adapted M7 run are listed,
followed by the M9-E direct-run findings. No latest-draft change is
adopted without a separate decision.

## Verified divergences (adapted run, all PASS)

1. `Blob` constructor accepts non-Array sequences only as ordinary
   `Array` objects (M2 contract: sequence conversion is Array-checked).
   Upstream `Blob-constructor.any.js` exercises generic `@@iterator`,
   `String` objects and `FrozenArray<MessagePort>` shapes — adapted out
   as capability gaps of the binding contract, not failures. No normative
   change: M2 behavior is the fixed contract.
2. `Blob.slice(start, end)` clamps `end > size` to `size` (span
   `11 - 3 = 8` for `slice(3, 100)` on 11 bytes) — verified against
   M1 `BlobData::slice` and M2 `slice_boundaries`. Matches the spec
   relative-range algorithm; no divergence.
3. `slice()` with absent `contentType` yields empty `type` (M2-BLOB-06);
   explicit `contentType` is MIME-normalized. Upstream type-table cases
   pass through the same M1 `normalize_blob_type`. No divergence.
4. `File` requires `(fileBits, fileName)`; `lastModified` defaults to
   the injected host clock (deterministic in harness). Upstream
   `lastModifiedDate` alias is historical-only (`historical.https.html`
   is a NOTRUN gap). No divergence.
5. Promise reads (`text`/`arrayBuffer`/`bytes`) settle only after
   explicit `run_jobs()` pumping — host-driven job contract, not a
   browser microtask timing claim. Adapted `promise_test` cases pass
   through the pump. No divergence.
6. `URL.revokeObjectURL()` requires its argument (missing → `TypeError`)
   per Web IDL, then revokes silently — verified in M6 (`R2`) and the
   adapted `bloburl-create-revoke` file. Matches the IDL signature;
   no divergence.

## Change control (M9-A): `readAsText` MIME `charset` step

ТЗ §6.4 lists label → UTF-8 → BOM → U+FFFD. The File API WD 23.08.2026
packaging-data steps add one intermediate step: when no explicit label
is given (or it is empty), the Blob MIME type `charset` parameter is
tried before the UTF-8 default; an unrecognized MIME charset falls back
to UTF-8 (never to `EncodingError`); the BOM then overrides whichever
fallback won, per the Encoding Standard. This file records the
extension as change control: it narrows no normative requirement, it
only makes the MIME-type input observable that the WD already
references. `FileReader` and `FileReaderSync` share the single
`package::resolve_text_encoding` selector, so both emit identical
strings. MIME charset extraction is gated by a successful type/subtype parse;
malformed individual parameters are skipped as required by the WHATWG MIME
parser, so later valid parameters remain visible. Quoted values may contain
semicolons, terminate with their accumulated value at EOF, and suffix text
after a closing quote is ignored to the next separator.

## Superseded by M9-A (sequence contract)

Item 1 above described the M2 array-only contract. M9-A replaces it
with the normative Web IDL `sequence<BlobPart>` conversion (generic
`@@iterator`, boxed `String`, `TypedArray`-as-sequence; primitive
strings stay conversion errors). The item is kept for history; the
current contract is `crates/boa_fapi/tests/m9_webidl_conformance.rs`
(`M9A-IDL-01`/`M9A-IDL-02`) and the M9-A handoff.

## Change control (M9-A acceptance remediation)

The explicit label uses Encoding Standard `get an encoding` with only
leading/trailing ASCII whitespace removed. Failure of that lookup falls
through to the MIME `charset` parameter and then UTF-8; the readers do not
turn this fallback into a read error. MIME parsing validates type/subtype and
parameters, supports quoted values including EOF termination, and uses the
first duplicate parameter.
The decoder keeps the Encoding Standard BOM authority and must consume all
input/output across `OutputFull` and EOF flushes. Web IDL constructor
conversion completes before `NewTarget.prototype` is read. Sequence phase 1
uses a checked conservative size lower bound; exact accounting still occurs
after options and `endings`.

## NOTRUN gaps (not divergences)

HTML input UI, navigation, Fetch dereference, MediaSource, real browser
Window/Worker orchestration and the WPT server have no binding surface
in M1–M6 and are recorded as exact capability gaps, not behavior
differences. No new normative requirement is introduced by M7.

## M9-E direct-run findings (36 upstream files, 417 executable subtests)

- Sync `abort()` dispatches `abort`+`loadend` back-to-back on the calling
  stack (TZ §7.3). Pinned `fileReader.any.js` ("FileReader States --
  abort") asserts exactly this (handler runs before `abort()` returns);
  the pre-existing M4/M9-C suites pin it
  (`abort_before_first_job_emits_only_abort_loadend`,
  `loadstart_abort_then_restart_emits_only_new_operation`). The M9-E gate
  does NOT re-verify this path through `fileReader.any.js`: that upstream
  row is a recorded open defect (see below) because the M9-C executor
  protocol queues the abort terminal through `poll_io`/`run_jobs` and the
  `unreached_func` reassignment lands first. No product change in M9-E;
  the defect is harness-observable only.
- `filereader_result` "result is null during loadstart/progress" holds by
  construction: packaging publishes only at EOF (`finish_at_eof`), so
  every non-terminal continuation observes null. The 8 `progress`-matrix
  rows that need browser microtask interleaving between dispatch and
  packaging are exact `worker-runtime` NOTRUN exclusions, not failures.
- `url-format` origin/parse rows need the WHATWG URL constructor plus
  `location.origin`; the URL shim stays create/revoke-only by M6 design,
  so the 3 rows are exact `navigation` NOTRUN exclusions.
- `readAsDataURL` for empty-type Blobs: pinned upstream
  (`filereader_readAsDataURL.any.js`, two rows) expects
  `data:application/octet-stream;base64,...`, but the crate contract
  (M4, pinned by `m4_filereader_async::read_as_data_url_exact_packaging`
  and `m4_filereader_sync::sync_data_url_exact_packaging`) emits the Blob
  type verbatim (`data:;base64,...`). Recorded as open defects (2);
  changing the packaging would break the M4 suites and is out of scope
  for M9-E.
- `File` name `dummy/foo`: pinned upstream (`File-constructor.any.js`,
  "No replacement when using special character in fileName") expects the
  slash verbatim, but the normative File API replaces every U+002F with
  U+003A and the crate (M2, `normalize_file_name`) emits `dummy:foo`.
  Recorded as an open defect (1); the product follows the spec, not the
  upstream row.
- Open defects (5 total, all recorded `supported`/`FAIL`, release-red):
  `filereader_abort.any.js :: Aborting after read` — the test's own
  `.then()` continuation re-arms `wait_for(['abort','loadend'])` and
  calls `abort()` a second time after the sync dispatch already delivered
  the pair; the harness observes a phantom second pair (`2 !== 1`).
  Product behavior (exactly one pair per `abort()`) matches TZ §7.3;
  the fix needs upstream EventWatcher queue semantics. Plus the
  `fileReader.any.js` sync-abort row and the two `readAsDataURL`
  empty-type rows and the `File` slash row above. See
  `docs/reviews/M9E-handoff.md` §3.
