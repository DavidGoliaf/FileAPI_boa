# M5 final audit

## Scope

Capability-based filesystem `File` (`boa_fapi_fs` + `fs` feature),
snapshot validation, host limits, lifecycle shutdown. No Blob URL,
structured clone, Workers runtime, DOM/HTML, WPT harness, or M6 surface.

## Findings and fixes (initial implementation)

1. **`blob.rs` move-after-use** — `snapshot_for_segments(&segments)` after
   moving `segments` into `Self`. Fix: compute `let snapshot` before the
   struct literal (3 sites). Re-validated with `cargo test -p boa_fapi_core`.
2. **`SnapshotState` non-exhaustive matches** — downstream `match` on the
   extended enum needs a wildcard. Fix: `_ =>` arms mapping unknown
   future variants to safe defaults (size 0 / `SnapshotChanged`). ✅
3. **Windows `MetadataExt::file_index` unstable on 1.91** (`windows_by_handle`).
   Initial fix (weak attributes/mtime fallback) **rejected on review**:
   a fallback that cannot detect same-metadata replacement violates
   order §3.1. Final fix: enforced `copy_on_import`-or-deny (finding R2
   below). ✅
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

## Review blockers R1–R3 (post-handoff review, fixed before final commit)

- **R1 — shutdown did not release OS handles.** `FsRegistry::close` only
  flipped a `closed` bit; the `std::fs::File` stayed in the `HashMap`
  until registry destruction, contradicting the order and the handoff
  claim. Fix: `close` removes the slot (handle drops immediately),
  `close_all` drops every slot, `on_shutdown`/`run_closers` provide the
  one-shot closer primitive; `ShutdownFlag` tracks one closer per
  `file_from_resource` registry (`track(move || registry.close_all())`);
  `FileApiHandle::shutdown` drains closers exactly once before cancelling
  work. Proven by `live_slot_count` assertions
  (`fs_tests::close_removes_slot_and_drops_handle`,
  `::close_all_drops_every_handle`,
  `::shutdown_closers_run_once_outside_lock`,
  `m5_file_fs.rs::shutdown_releases_live_handles` [unix],
  `::shutdown_releases_handles_and_rejects_late_use`,
  `::post_shutdown_source_reads_fail`). ✅
- **R2 — Windows weak-identity fallback.** `file_attributes` +
  `creation_time` + size + mtime cannot detect same-metadata
  replacement, so offering it as a live-read identity violates order
  §3.1 ("обязана выбрать `copy_on_import` либо отказать в импорте").
  Fix: **enforced** `copy_on_import`-or-deny — `platform_has_strong_
  identity() == cfg!(unix)`; `FileSource::new`,
  `RegistryPolicy::authorize_open`, and `file_from_resource` refuse
  filesystem-backed live imports off-Unix with `PermissionDenied`;
  `open_copy_on_import` (via `new_for_copy`, closing the live handle
  eagerly) is the mandated fallback. No weak live reads anywhere.
  Proven by `weak_platform_direct_import_is_refused` (all platforms)
  and `weak_platform_copy_reports_no_location_detail` (not-unix). ✅
- **R3 — global mutex held across I/O.** `read_at`/`live_snapshot` ran
  metadata reads and byte reads while holding the registry `Mutex`,
  violating the order's no-global-lock-during-slow-I/O rule (and the
  non-Unix path even did seek+read under a write lock). Fix: both clone
  the handle via `try_clone` under a short lock, then run all I/O on the
  clone after unlock (Unix: lock-free positional reads on the clone;
  other platforms: independent cursor on the per-call clone). The old
  `closed: bool` flag is gone (removal is the close); `Slot` keeps only
  the handle + import snapshot. ✅

## Retrospective bug-find (order §10, re-run after R1–R3)

Scope boundaries (no M6 surface: guards green), `unsafe`/panic/unwrap/
expect (workspace lints + scanner guards green), capability lifetime
(close removes the slot immediately, idempotent; reads after fail
`NotFound`), snapshot replacement race on Unix (pre+post checks per
range; weak platforms have no live reads by construction), short-read
handling (exact-length, no partial), symlink/junction policy (no
location input at all; off-Unix refusal), leakage (negative JS-error
tests + message-surface test per platform), atomic registration/feature
guards/rollback (powerset green), shutdown idempotency + immediate
handle release (`live_slot_count` proofs), no late callbacks
(pending-job tests assert `pending`/`AbortError` and empty/error+loadend
event sets), exact limits (`==`/`+1` covered in fs units + JS boundary
tests), platform test truthfulness (Unix-only live tests `#[cfg(unix)]`;
 weak-platform tests assert refusal + copy semantics — never a silent
pass). No unresolved items.

## Post-review fixes R4–R5 (second review round, fixed before final commit)

- **R4 — copy leaked the handle on failure paths.** `open_copy_on_import`
  closed the slot only on success; `max_bytes` refusal, allocation
  failure, and read errors returned early with the slot still live until
  registry destruction. Fix: split into `open_copy_inner` (pure copy
  logic) + an outer wrapper that unconditionally `close`s the consumed
  registration on every exit (idempotent, so the success-path close is
  covered too). Documented consume semantics: one copy per registration.
- **R5 — the +1 test never exercised the limit.** The second
  `open_copy_on_import` call reused the already-consumed slot, so it
  observed `NotFound` instead of `ResourceLimit`. Fix: all three copy
  boundary tests re-register a fresh slot before the `+1` probe and
  assert `Err(ResourceLimit(_))` plus `live_slot_count() == 0`
  afterwards (`copy_bounds_and_content_everywhere`,
  `copy_on_import_bounds_and_content`,
  `failed_copy_releases_handle_on_every_path` — the last also pins the
  R4 release-on-refusal behavior). ✅

## Traceability

`docs/spec-matrix.md` M5-FS-01..M5-FS-10 (R1–R3 rows updated with the
new symbols and test names); ADRs 0024–0027 in `docs/DECISIONS.md`
(R1–R3 corrections applied); CI runs Ubuntu + Windows with the two new
M5 jobs, on the final commit SHA (see handoff).
