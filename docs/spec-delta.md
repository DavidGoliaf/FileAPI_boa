# Spec delta — WD 23.08.2026 vs pinned WPT reference

Reference: upstream `web-platform-tests/wpt`
`0968c868d8095217d18d86b34c7f21dccae58768` (`master` 2026-09-07).
Normative base stays WD 23.08.2026 per TZ §1.6; only behaviorally
verifiable divergences observed through the adapted M7 run are listed.
No latest-draft change is adopted without a separate decision.

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
semicolons and suffix text after a closing quote is ignored to the next
separator.

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
parameters, supports quoted values, and uses the first duplicate parameter.
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
