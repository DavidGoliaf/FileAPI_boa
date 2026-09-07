# M6 Handoff — Blob URL, environment isolation, structured-clone bridge

Branch: `task/m6`, base `e23e0721e885561deda52c211075ed389dfd3cca`.
Status: implementation complete, local validation green on Windows;
external CI (Ubuntu + Windows) NOT RUN YET at handoff time.

## Implemented

- Core (`boa_fapi_core`, Boa-free, no new deps): `blob_url.rs`
  (`EnvironmentDescriptor`/`EnvironmentKey`, `BlobUrlStore`,
  `ResolvedBlob`, `BlobUrlError`, `format_uuid_v4`/`format_blob_url`/
  `parse_blob_url`) + `clone.rs` (`FCL1`/v1/`SCF_*` encoding,
  `FileApiClonePayload`, `CloneError`, checked `Cursor`).
- Bindings (`boa_fapi`): `url_shim.rs` (`URL` namespace object, two
  static methods, oracle-free mapping), `clone_bridge.rs` (host
  `CloneAdapter` surface + fake reference bridge), `extension.rs`
  wiring (per-context store, `OsEntropy` via `getrandom`, builder
  `url_shim`/`structured_clone`/`origin`/`partition`/`nonce`/`entropy`/
  `clone_adapter`, `RegisterError::CloneBridgeIncompatible`, handle
  `create/resolve/revoke_blob_url` + `clone_*`/`blob_from_clone`/
  `file_from_clone`/`file_list_from_clone` + bridge encode/decode,
  unconditional `shutdown` with `store.clear()` closer).
- Tests: `boa_fapi_core/tests/blob_url.rs` (11), `m6_blob_url.rs` (9),
  `m6_structured_clone.rs` (5); guards extended (bounded URL surface,
  no `boa-idb` dep, no `structuredClone` global, no `MediaSource`);
  two M4 negative-guard lines updated for the normative M6 `URL`
  surface (only M1–M5 test edits in the diff).
- Docs: `spec-matrix.md` (M6-URL-01..06, M6-CLONE-01..05, M6-REG-01),
  `architecture.md` (layers 2f/2g), `security.md` (M6 section),
  `README.md` (M6 scope + test commands), `DECISIONS.md`
  (ADR-0028..0032: identity, `getrandom`, store lifetime, API
  compatibility, clone encoding), `m6-validation.md`, CI (M6 steps).

## Deviations

None normative. Deliberate choices (all ADR-recorded): `URL` as a
namespace object (not a constructor/WHATWG URL); `url-shim`/
`structured-clone` off = absent surface (no `UrlAdapter` error — host
adapter out of M6 scope); `FileApiHandle` kept over target
`FileApiExtension::shutdown` (equivalence recorded); `uuid` crate not
taken (15-line v4 formatter instead); `CloneAdapter` is the only
`boa-idb` coupling (no dependency); `M6-REG-01` powerset honest about
the pre-existing `--no-default-features` unit behavior (verified
identical on the M5 base).

## ADR

ADR-0028 (identity), ADR-0029 (`getrandom` 0.3), ADR-0030 (store
lifetime), ADR-0031 (API compatibility), ADR-0032 (clone encoding) —
see `docs/DECISIONS.md`.

## Demo commands

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi_core --test blob_url -- --nocapture
cargo test --package boa_fapi --test m6_blob_url -- --nocapture
cargo test --package boa_fapi --test m6_structured_clone -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo llvm-cov --workspace --all-features --fail-under-lines 80
cargo deny check
```

`cargo test --workspace --no-default-features` is MIXED by pre-existing
M5 design (shim-required unit suites fail without default features —
identical on the base); `cargo hack check --feature-powerset --depth 2`
is green for every combination.

## Exact commit / CI links

- Implementation commit: _to be filled after commit_.
- CI run: _to be filled after push_ (Ubuntu + Windows, M6 sequence).
- After handoff: M7 NOT started.
