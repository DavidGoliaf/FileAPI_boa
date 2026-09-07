# M5 final audit

## Scope

Capability-based filesystem `File` (`boa_fapi_fs` + `fs` feature),
snapshot validation, host limits, lifecycle shutdown. No Blob URL,
structured clone, Workers runtime, DOM/HTML, WPT harness, or M6 surface.

## Findings and fixes

1. **`blob.rs` move-after-use** — `snapshot_for_segments(&segments)` after
   moving `segments` into `Self`. Fix: compute `let snapshot` before the
   struct literal (3 sites). Re-validated with `cargo test -p boa_fapi_core`.
2. **`SnapshotState` non-exhaustive matches** — downstream `match` on the
   extended enum needs a wildcard. Fix: `_ =>` arms mapping unknown
   future variants to safe defaults (size 0 / `SnapshotChanged`). ✅
3. **Windows `MetadataExt::file_index` unstable on 1.91** (`windows_by_handle`).
   Fix: Windows identity mixes stable `file_attributes` + `creation_time` +
   size + mtime; NTFS file-id limitation documented in ADR-0025 and covered
   by the labelled cfg test. ✅
4. **Guards `filereader_and_dom_surface_is_bounded`** — the old forbidden
   `"fs"` substring matched the new `#[cfg(feature = "fs")]` shutdown
   checks. Fix: guard updated to allow only `shutdown`/`ShutdownFlag` lines
   in `filereader.rs`; `lifecycle.rs` added to the no-public-items guard.
   No M4-A behavior changed (M4-A suite green without edits). ✅
5. **`BlobData::materialize` doc-link** — `filereader_sync.rs` doc comment
   tripped the `dom.rs`/`filereader.rs` guard's `BlobData::materialize`
   substring rule. Verified: only a doc comment in the sync module (which
   the guard does not scan); production code calls the method through
   existing paths. No change needed. ✅
6. **Limits `validate()` ordering** — custom tight limits must keep
   `sync <= materialize <= blob` and `chunk <= materialize`; tests use
   consistent triples. ✅
7. **Clippy `op_ref`/`collapsible_if`/empty-range** — fixed at the flagged
   sites; `slot_count` kept with `#[allow(dead_code)]` (registry
   introspection helper). ✅
8. **Rustdoc `-Dwarnings`** — broken intra-doc links fixed with fully
   qualified paths; private-type link in `file_from_resource` docs
   de-linked. ✅
9. **Retrospective bug-find (order §10)** — scope boundaries (no M6
   surface: guards green), `unsafe`/panic/unwrap/expect (workspace lints
   + scanner guards green), capability lifetime (close idempotent, reads
   fail after), snapshot replacement race (pre+post checks per range),
   short-read handling (exact-length, no partial), symlink/junction
   policy (no path input at all), leakage (negative JS-error tests +
   message-surface test), atomic registration/feature guards/rollback
   (powerset green), shutdown idempotency (repeated-shutdown test), no
   late callbacks (pending-job tests assert `pending`/`AbortError` and
   empty/error+loadend event sets), exact limits (`==`/`+1` covered in fs
   units + JS boundary tests), platform test truthfulness (cfg-scoped
   with explicit limitation labels). No unresolved items.

## Traceability

`docs/spec-matrix.md` M5-FS-01..M5-FS-10; ADRs 0024–0027 in
`docs/DECISIONS.md`; CI runs Ubuntu + Windows with the two new M5 jobs.
