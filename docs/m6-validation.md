# M6 validation

Date: 2026-09-08. Branch `task/m6`, base `e23e0721e885561deda52c211075ed389dfd3cca`.
Local platform: Windows (weak-identity target — live-handle tests are Unix-only by construction).

## Commands (all exit 0 unless noted)

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (42 boa_fapi unit, 13 guards, 51 M2, 28 M3-B, 16 M3-A, 33 M4-A, 21 M4-B, 15 M5-JS, 9 M6-URL-JS, 5 M6-clone-JS, 58+11+25+8+20+16+19 core incl. 11 M6-core, 9 fs, 1 doc) |
| 4 | `cargo test --package boa_fapi_fs --all-features -- --nocapture` | 0 | PASS (9 on Windows; Unix live tests `#[cfg(unix)]`, run in Linux CI) |
| 5 | `cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture` | 0 | PASS (51) |
| 6 | `cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture` | 0 | PASS (16) |
| 7 | `cargo test --package boa_fapi --test m3_blob_streams -- --nocapture` | 0 | PASS (28) |
| 8 | `cargo test --package boa_fapi --test m4_filereader_async -- --nocapture` | 0 | PASS (33) |
| 9 | `cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture` | 0 | PASS (21) |
| 10 | `cargo test --package boa_fapi --test m5_file_fs -- --nocapture` | 0 | PASS (15 on Windows; Unix live tests `#[cfg(unix)]`, run in Linux CI) |
| 11 | `cargo test --package boa_fapi_core --test blob_url -- --nocapture` | 0 | PASS (11: 7 URL + 4 clone) |
| 12 | `cargo test --package boa_fapi --test m6_blob_url -- --nocapture` | 0 | PASS (9) |
| 13 | `cargo test --package boa_fapi --test m6_structured_clone -- --nocapture` | 0 | PASS (5; Unix fs-safety test `#[cfg(unix)]`, runs in Linux CI) |
| 14 | `cargo test --workspace --no-default-features` | — | MIXED (pre-existing M5 baseline behavior: unit suites requiring shims fail with `StreamsShimDisabled`/`DomShimDisabled` without default features — verified identical on the M5 base via `git stash`; all feature-gated integration suites compile and run) |
| 15 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS (17/17 incl. `fs`/`url-shim`/`structured-clone` on/off) |
| 16 | `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) | 0 | PASS |
| 17 | `cargo test --package boa_fapi --doc` | 0 | PASS (1) |
| 18 | `cargo llvm-cov --workspace --all-features` | 0 | TOTAL 86.48% lines (threshold 80): `blob_url.rs` 93.22%, `clone.rs` 88.28%, `url_shim.rs` 94.38%, `extension.rs` deltas covered by M6 suites |
| 19 | `cargo deny check` | 0 | PASS (advisories ok, bans ok, licenses ok, sources ok; local DB fetch succeeded, network available; `getrandom` 0.3 MIT OR Apache-2.0 within allow-list) |
| 20 | `git diff --check` | 0 | PASS |

## Feature matrix

| Configuration | Result |
|---|---|
| `--all-features` | PASS (full workspace incl. 25 M6 tests) |
| `--no-default-features` | MIXED, pre-existing: same 15 unit failures as the M5 base (shim-required suites); powerset `cargo hack` green proves every combination compiles |
| `url-shim = false` (`url_shim(false)`) | PASS (`url_environment_gating` feature-off case: no `URL` global, M1–M5 intact) |
| `structured-clone = false` | PASS (`clone_feature_off_keeps_m1_m5`: no `structuredClone` global, M1–M5 intact) |
| `url-shim`/`structured-clone` features off at compile time | PASS (`cargo hack` powerset green; `create_url_for_specs` has an inline no-shim path) |

## URL/clone fixtures

- UUID: `format_uuid_v4([0xAB; 16])` → `abababab-abab-4bab-abab-abababababab` (v4 bits pinned).
- Store URL: `blob:https://example.com/<uuid>`; opaque: `blob:null/<uuid>` (key still carries partition + nonce).
- Clone bytes: `FCL1 | u32 v1 | u32 tag (BLOB 0x424C_4F42 / FILE 0x4649_4C45 / FLST 0x464C_5354) | le-lengths + bytes + UTF-8 + i64`; same-version fixture in `blob_url.rs::clone_same_version_fixture`.
- Failure classes: `Malformed`/`Unavailable` share one display (`blob URL is not available`); JS dereference failure is one `TypeError`; clone failures are typed `CloneError` generics (no bytes/names/paths).

## Leak/shutdown evidence

- `url_descriptor_debug_redacts_partition`, `url_failures_share_one_opaque_class`, `same_origin_partitions_isolate` (partition + nonce, foreign === missing).
- `url_revoke_keeps_live_reads_and_clear_releases` + `url_shutdown_lifetime` (revoke keeps handed-out `Arc`; `clear()` at shutdown releases all strong refs; repeated shutdown idempotent; no late jobs).
- `clone_filesystem_safety_unix` (cfg unix): changed source → `SourceFailed`, no partial payload, no path/capability in the error.
- Guards: `no_out_of_scope_surface` (bounded URL methods only in `url_shim.rs`/`extension.rs`, no `structuredClone` global, no `MediaSource`/`boa-idb`), `public_api_exposes_no_paths_or_mutable_bytes` (no `boa-idb` dep in any manifest, no `use boa_idb`).

## Trace rows

See `docs/spec-matrix.md` M6-URL-01..06, M6-CLONE-01..05, M6-REG-01 with exact `file:symbol`, test names, and commands.

## Coverage

`cargo llvm-cov --workspace --all-features` TOTAL 86.48% lines (threshold 80%).
New modules: `blob_url.rs` 93.22%, `clone.rs` 88.28%, `url_shim.rs` 94.38%.
`boa_fapi_fs` policy/source lines below threshold are pre-existing M5 Unix-only paths (covered in Linux CI).

## CI

- External CI (GitHub Actions, Ubuntu + Windows) status at handoff time: **NOT RUN YET** — local validation above is on Windows; Unix-only tests (`#[cfg(unix)]` live-handle/snapshot/clone-fs-safety) execute in the Ubuntu job. The implementation commit and the CI run link will be recorded in `docs/reviews/M6-handoff.md` after push.
- Windows receives no weak identity or security fallback: live-handle imports stay refused off-Unix per the M5 contract (unchanged).
