# M5 handoff

- Base: `f5404de6a105c52dc128e686e18d92b376603bd4` (accepted M4-B final).
- Branch: `task/m5`.
- Status: acceptance requested (second submission, after R1–R3 rework).
  No M6 started.

## What was built

Capability-based filesystem `File`, snapshot validation, host limits,
and lifecycle shutdown (order `M5-FS-FILE-SECURITY`):

- `boa_fapi_core`: `FileSnapshot` + `SnapshotState::Filesystem`, `policy`
  module (`HostResourceId`, `FileOpenRequest`, `FileGrant`,
  `FileResource`, `FileResourceOpener`, `FileAccessPolicy`,
  `DenyAllPolicy`); `BlobData` blob-level snapshot derivation.
- `boa_fapi_fs`: `FsRegistry`/`RegisteredResource` (opaque id only;
  `close` removes the slot, `close_all` drops all handles,
  `on_shutdown`/`run_closers` one-shot closers, `live_slot_count`
  proofs; mutex guards only the map, I/O runs on `try_clone`d handles
  after unlock), `identity` (`platform_has_strong_identity() ==
  cfg!(unix)`; Unix dev+ino; no weak fallback), `FileSource` (Unix-only;
  `new_for_copy` backs the universal `open_copy_on_import`, which closes
  the live handle eagerly) / `HostFileSource`, `DenyRawPathPolicy` /
  `RegistryPolicy` (refuses `authorize_open` off-Unix) /
  `RootConfinedPolicy`, `open_copy_on_import`.
- `boa_fapi` (`fs` feature, default on): `blob::data_from_fs_source`
  (preflight `max_blob_size`), `file::native_from_data` (display name
  only), `FileApiHandle::file_from_resource(registry, Arc<dyn
  FileResource>, ...)` (validates live==import plus the weak-platform
  gate before any JS object; tracks the registry for shutdown; `Arc`
  adaptation of the target `&dyn` shape recorded in ADR-0024),
  `FileApiHandle::shutdown` (form chosen over
  `FileApiExtension::shutdown`, ADR-0026), `lifecycle::ShutdownFlag`
  (closed bit + cancellation + tracked closers, exactly-once) shared by
  specs/handle/every import, closed-state checks in promise settlement,
  FileReader pump/dispatch, stream pumps, and the fs adapter; handle
  entry-point rejects after shutdown.
- Tests: `boa_fapi_fs` units (`fs_tests`: Unix-only live tests
  `#[cfg(unix)]`, universal refusal/copy tests everywhere), `boa_fapi`
  JS integration (`m5_file_fs`: platform-mandated host path —
  live-handle on Unix, `copy_on_import` elsewhere; per-platform
  shutdown-handle proofs); guards extended minimally (M5 shutdown
  exception documented inline).
- Docs: `README.md`, `docs/architecture.md`, `docs/security.md` (new),
  `docs/host-integration.md` (new, both host paths),
  `docs/DECISIONS.md` (ADR-0024–0027, R1–R3 corrections),
  `docs/spec-matrix.md` (M5-FS-01..10, R1–R3 rows), `docs/m5-validation.md`,
  `docs/m5-final-audit.md` (R1–R3 section); CI
  (`.github/workflows/ci.yml`) gains the `boa_fapi_fs` and M5
  host-integration jobs on Ubuntu + Windows.

## Post-handoff review blockers R1–R3 (fixed in this submission)

- **R1 — shutdown now drops OS handles immediately.** `close` removes the
  slot, `close_all` drops all slots, shutdown drains tracked closers
  exactly once before cancelling work. The old "close flips a flag"
  behavior and the handoff claim are replaced by `live_slot_count`
  proofs.
- **R2 — enforced `copy_on_import`-or-deny off-Unix.** The Windows
  attributes/mtime fallback is gone (it could not detect same-metadata
  replacement). `FileSource::new`, `RegistryPolicy::authorize_open`, and
  `file_from_resource` refuse filesystem-backed live imports off-Unix;
  hosts use `open_copy_on_import` (live handle closed eagerly).
- **R3 — no global lock across I/O.** `live_snapshot`/`read_at` clone via
  `try_clone` under a short lock; all metadata/byte I/O runs after
  unlock (Unix positional reads on the clone; independent cursor
  elsewhere).

## Omitted (M6+ scope, untouched)

Blob URL store/structured-clone lifetime (extension points reserved in
`lifecycle.rs`), full Workers runtime, DOM/HTML, WPT harness.

## How to run the demo commands

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi_fs --all-features -- --nocapture
cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture
cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
cargo test --package boa_fapi --test m4_filereader_async -- --nocapture
cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture
cargo test --package boa_fapi --test m5_file_fs -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

All green locally (see `docs/m5-validation.md` for the exact counts;
`cargo deny check` exit 0 with network-available advisory DB).

## Deviations

None from the order's normative requirements. Signature adaptation
(`(registry, Arc<dyn FileResource>)` vs target `&dyn`) and lifecycle
form (`FileApiHandle::shutdown` vs target `FileApiExtension::shutdown`)
are allowed adaptations recorded in ADR-0024/ADR-0026. Platform gating
(`cfg!(unix)` strong identity; enforced copy-or-deny elsewhere) is the
order §3.1-mandated behavior, recorded in ADR-0025.

## Audit findings

See `docs/m5-final-audit.md` (initial findings + R1–R3 section, all
fixed and re-validated; no unresolved items).

## Honest CI status

- Final code commit SHA: `11c8675743d37b77c3520fa3d91731afc7fa341d`
- CI run: https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34125651525
  — **success** on `ubuntu-latest` and `windows-latest` (full M5 order §9
  sequence, including `boa_fapi_fs` + M5 host-integration jobs).

Local: all commands exit 0 on this branch (`docs/m5-validation.md`).
Two earlier pushes failed Linux-only clippy lints (`map_io` by-value,
`Write` import scope, dead helper) that Windows clippy did not flag;
all three fixed, re-validated locally, and green in the final CI run
above. Unix-only live-handle tests executed in the Ubuntu job;
Windows executed the refusal + copy-fallback tests.
