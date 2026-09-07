# M6 Handoff — Blob URL, environment isolation, structured-clone bridge

Branch: `task/m6`, base `e23e0721e885561deda52c211075ed389dfd3cca`.
Status: `ACCEPTED` after independent review rework R1–R4. The production
implementation and its regression evidence are in `38d7b25`; the final CI
workflow correction is in `d02e949`.

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
- Tests: `boa_fapi_core/tests/blob_url.rs` (14), `m6_blob_url.rs` (11),
  `m6_structured_clone.rs` (6); guards extended (bounded URL surface,
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

`cargo test --workspace --no-default-features` is intentionally not a CI
step: shim-required unit and JS integration suites register default shims
and fail without them (`StreamsShimDisabled`), a pre-existing M2/M3
behavior. Feature-combination coverage is green through
`cargo hack check --feature-powerset --depth 2` (17/17).

## Exact commit / CI links

- Implementation/rework commit: [`38d7b25f1cae07617e88e045a8c752c9b19af084`](https://github.com/DavidGoliaf/FileAPI_boa/commit/38d7b25f1cae07617e88e045a8c752c9b19af084).
- Final CI commit: [`d02e949595f706b405cf26bf397c7b1fce3f18f0`](https://github.com/DavidGoliaf/FileAPI_boa/commit/d02e949595f706b405cf26bf397c7b1fce3f18f0).
- CI run: [`34150738386`](https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34150738386) — Ubuntu + Windows, both green.
- After handoff: M7 NOT started.
