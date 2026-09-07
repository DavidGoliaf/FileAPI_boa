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

## NOTRUN gaps (not divergences)

HTML input UI, navigation, Fetch dereference, MediaSource, real browser
Window/Worker orchestration and the WPT server have no binding surface
in M1–M6 and are recorded as exact capability gaps, not behavior
differences. No new normative requirement is introduced by M7.
