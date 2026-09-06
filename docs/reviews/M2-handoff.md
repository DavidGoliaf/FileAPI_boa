# M2 Handoff — Boa bindings: `Blob`, `File`, `FileList`

## What was built

- M2 JS bindings in `crates/boa_fapi` (extension/brand/blob/file/file_list/
  webidl/clock/error) with atomic registration, native brand gates, Web IDL
  conversions, no-copy `Blob`/`File` composition, injectable `Clock`.
- Core composition without raw segments or test probes:
  `BlobData::concat_shared`/`push_shared` only, in
  `crates/boa_fapi_core/src/blob.rs`; `PartsCollector` owns one `BlobData`.
  No public byte reads, no source-identity probes; no-copy is proven in the
  `#[cfg(test)]` child module via private fields (M1 §6.6), and
  `boa_fapi/src/tests.rs` asserts only JS-observable state plus M1 metadata.
- Review fixes on top of the M2 feature set:
  - P1: removed public `BlobData::segments()`; no raw segments in public API.
  - P1 (round 2): removed public `read_all` (unbounded materialization
    bypassed `max_materialize_bytes`, out of M1 contract) and the
    `shares_sources_with` / `first_segment_shares_source_with` test
    accessors; added guard `blob_data_public_api_is_fixed` (9 methods).
  - P1: `deny.toml` back to `wildcards = "deny"`; internal dep pins
    `path + version`; `allow-wildcard-paths` is a backstop only.
  - P1: `Cargo.lock` removed from `.gitignore`, tracked in Git.
  - P2: `docs/m2-validation.md` re-recorded after the last change
    (92.61% lines); this handoff file created.

## How to run the demo commands

From a fresh clone with only a Rust toolchain (+ `cargo-deny`,
`cargo-llvm-cov`, `cargo-hack` for the full list), work order §8 in order:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

All exit 0. Recorded results and the coverage table: `docs/m2-validation.md`.
Final audit trace (M2-REG/WIDL/BLOB/FILE/FLIST/GC) and findings:
`docs/m2-final-audit.md`.

## Deviations

None. No changes to `TZ_boa_fapi_FileAPI.md`, the M2 work order, thresholds,
commands, matrix IDs, or acceptance criteria.

## DECISIONS.md entries

- ADR-0003 (updated): `boa_engine`/`boa_gc` 0.22.x; `Zlib` allow-listed;
  wildcards stay `deny` via pinned internal `path + version`.
- ADR-0004: brands as native data types via `downcast_ref`.
- ADR-0005: GC-safe natives via `#[unsafe_ignore_trace]` (no `unsafe`).
- ADR-0006: re-registration rule (b) `AlreadyRegistered`.
- ADR-0007: atomic registration build → preflight → install → rollback.
- ADR-0008: `BlobPart` union restricted to string/BufferSource/Blob/File.
- ADR-0009: injectable `Clock` for `File.lastModified`.
- ADR-0010 (rewritten): no public segment accessor, byte read, or identity
  probe; core exposes only `concat_shared`/`push_shared` beyond M1.
