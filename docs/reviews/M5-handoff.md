# M5 handoff

- Base: `f5404de6a105c52dc128e686e18d92b376603bd4` (accepted M4-B final).
- Branch: `task/m5`.
- Status: acceptance requested. No M6 started.

## What was built

Capability-based filesystem `File`, snapshot validation, host limits,
and lifecycle shutdown (order `M5-FS-FILE-SECURITY`):

- `boa_fapi_core`: `FileSnapshot` + `SnapshotState::Filesystem`, `policy`
  module (`HostResourceId`, `FileOpenRequest`, `FileGrant`,
  `FileResource`, `FileResourceOpener`, `FileAccessPolicy`,
  `DenyAllPolicy`); `BlobData` blob-level snapshot derivation.
- `boa_fapi_fs`: `FsRegistry`/`RegisteredResource` (opaque id only),
  `identity` (safe `Metadata` capture; Unix dev+ino, Windows
  attributes+creation-time, documented fallback), `FileSource`/
  `HostFileSource` (cancel → arithmetic → snapshot → policy → positional
  read → exact-length → post-read confirm), `DenyRawPathPolicy`/
  `RegistryPolicy`/`RootConfinedPolicy`, `open_copy_on_import`.
- `boa_fapi` (`fs` feature, default on): `blob::data_from_fs_source`
  (preflight `max_blob_size`), `file::native_from_data` (display name
  only), `FileApiHandle::file_from_resource(Arc<dyn FileResource>, ...)`
  (validates live==import before any JS object; `Arc` adaptation of the
  target `&dyn` shape recorded in ADR-0024), `FileApiHandle::shutdown`
  (form chosen over `FileApiExtension::shutdown`, ADR-0026),
  `lifecycle::ShutdownFlag` shared by specs/handle/every import, closed-
  state checks in promise settlement, FileReader pump/dispatch, stream
  pumps, and the fs adapter; handle entry-point rejects after shutdown.
- Tests: 17 `boa_fapi_fs` units (`fs_tests`), 15 `boa_fapi` JS
  integration (`m5_file_fs`); guards extended minimally (M5 shutdown
  exception documented inline).
- Docs: `README.md`, `docs/architecture.md`, `docs/security.md` (new),
  `docs/host-integration.md` (new), `docs/DECISIONS.md` (ADR-0024–0027),
  `docs/spec-matrix.md` (M5-FS-01..10), `docs/m5-validation.md`,
  `docs/m5-final-audit.md`; CI (`.github/workflows/ci.yml`) gains the
  `boa_fapi_fs` and M5 host-integration jobs on Ubuntu + Windows.

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
(`Arc<dyn FileResource>` vs target `&dyn`) and lifecycle form
(`FileApiHandle::shutdown` vs target `FileApiExtension::shutdown`) are
allowed adaptations recorded in ADR-0024/ADR-0026.

## Audit findings

See `docs/m5-final-audit.md` (9 findings, all fixed and re-validated;
no unresolved items).

## Honest CI status

Local: all commands exit 0 on this branch. CI (Ubuntu + Windows) must be
run on the final code-commit SHA after push; this handoff does not claim
CI results before that run.
