# M1 Final Audit Report

## Step A — Requirements Verification

| ID | Requirement | Code (file:symbol) | Test | Verdict | Notes |
|---|---|---|---|---|---|
| M1-CORE-01 | core does not depend on Boa | `crates/boa_fapi_core/Cargo.toml` — only `bytes`, `thiserror` deps | `guards_tests::core_cargo_toml_no_forbidden_dependencies` | PASS | |
| M1-CORE-02 | ByteSource range/cancel contract | `src/source/mod.rs` — `ByteSource` trait; `src/source/memory.rs` — `MemorySource` impl | `source_tests::*` (19 tests) | PASS | |
| M1-CORE-03 | immutable memory source | `src/source/memory.rs` — `Bytes::slice` in `read_range` | `source_tests::memory_source_result_shares_allocation`, `blob_tests::blob_data_immutable_repeated_reads` | PASS | |
| M1-CORE-04 | Blob type normalization | `src/mime.rs` — `normalize_blob_type()` | `mime_tests::*` (16 tests) | PASS | |
| M1-CORE-05 | native line endings | `src/endings.rs` — `convert_line_endings_to_native()` | `endings_tests::*` (25 tests) | PASS | |
| M1-CORE-06 | segment validation and checked arithmetic | `src/blob.rs:from_segments()` — validates ALL segments before filtering zero-length; checked_add for totals | `blob_tests::from_segments_*` (12 tests including `from_segments_zero_length_invalid_offset_rejected`) | PASS | |
| M1-CORE-07 | File API slice semantics | `src/blob.rs:slice()` | `blob_tests::slice_*` (19 tests) + `guards_tests::slice_matches_reference` | PASS | |
| M1-CORE-08 | limits validation | `src/limits.rs:validate()` | `limits_tests::*` (17 tests) | PASS | |
| M1-CORE-09 | safe Rust/no panics in public paths | `src/lib.rs` — `#![deny(unsafe_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]`; root `Cargo.toml` `[workspace.lints.clippy]` denies `unwrap_used`/`expect_used`/`panic`/`todo`/`unimplemented` for all members | `guards_tests::production_source_no_unwrap_expect_panic`, `guards_tests::public_api_no_path_types`, `guards_tests::strip_test_modules_*` | PASS | |

## Step B — Defect Search

### Dependency audit
- `boa_fapi_core/Cargo.toml`: Only `bytes` and `thiserror` in deps, `proptest` in dev-deps. ✅

### Public API audit
- No `Path`, `PathBuf`, file descriptors, `JsValue`, or mutable payload access. ✅
- Public `BlobData` API is exactly the M1 contract: `empty`, `from_segments`, `size`, `media_type`, `snapshot`, `segment_count`, `slice`. No `read_all`, no `materialize`, no `segment_source_ptr`, no segments accessor. ✅
- Tests that need internals read the private fields from the `#[cfg(test)]` module beside `src/blob.rs` (child-module privacy); integration suites use only the contract API. ✅

### Arithmetic audit
- `from_segments()`: validates ALL segments (including zero-length) before filtering. ✅
- Uses `checked_add` for offset+len and total size. ✅
- `slice()`: uses `saturating_sub` and `unsigned_abs()`. ✅

### from_segments validation order
- Issue found: zero-length segments with invalid offsets were discarded before validation.
- Fix: moved validation loop before the filter. Now all segments are validated regardless of length. ✅

### Lint configuration
- Issue found: workspace lints set `unwrap_used = "allow"` which contradicts the task requirement.
- Fix: root `Cargo.toml` `[workspace.lints.clippy]` sets `unwrap_used = "deny"`, `expect_used = "deny"`, `panic = "deny"` (plus `todo`/`unimplemented` already denied); all members inherit via `[lints] workspace = true`. Test crates carry a documented test-only allowlist (`#![allow(...)]` at the top of each `tests/*.rs`), which is the allowlist for tests required by work order §6.7. ✅

### Guard test completeness
- Issue found: guard test only checked `PathBuf` and `std::fs::`, missing `Path`, file descriptors, `JsValue`.
- Fix: expanded to check `PathBuf`, `std::path::Path`, `std::fs::`, `FileDesc`, `RawFd`, `RawHandle`, `JsValue`, `JsString`, `JsObject`. ✅

### cargo deny
- Issue found: `cargo deny check` fails when advisory DB path is read-only (default `~/.cargo/` location).
- Fix: `deny.toml` sets `db-path = "target/cargo-deny-advisories"` (writable project-local path). Validation command uses `$env:CARGO_DENY_DB_PATH` to override. ✅

### Production source audit
- No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` in non-test production code. ✅
- `#![deny(unsafe_code)]` in lib.rs. ✅

### M1 boundary audit
- No JS bindings, DOM, filesystem, stream, URL, WPT, or hidden global state. ✅

### Documentation audit
- All docs updated with real references. ✅

## Step C — Issues Found and Fixed

1. **`from_segments()` validation order** — Zero-length segments with invalid offsets were filtered before validation. Fix: validate all segments first, then filter.
2. **`read_all()` OOM risk** — Public method allocated up to 2 GiB without limit. Fix: removed; M1 contract defines no content-read public API (a limited read arrives with its contract in a later milestone).
3. **Lint configuration** — Workspace lints allowed `unwrap`/`expect`/`panic`. Fix: strict `[workspace.lints.clippy]` deny set in root `Cargo.toml`; test-only allowlist in `tests/*.rs` per §6.7.
4. **Guard test incomplete** — Only checked `PathBuf`/`std::fs::`. Fix: expanded to check `Path`, file descriptors, `JsValue`, `JsString`, `JsObject`.
5. **cargo deny read-only DB** — Default advisory DB path may be read-only. Fix: `db-path` in `deny.toml` points to `target/cargo-deny-advisories`.

## Rework — Review Blockers (2026-09-06)

| # | Blocker | Fix |
|---|---|---|
| 1 | Public `segment_source_ptr(index)` panicked on out-of-range `self.segments[index]` | Method removed from the API. Pointer sharing is proven in tests via `Arc::ptr_eq` on the segments (work order §6.6), from the `#[cfg(test)]` module beside `src/blob.rs`. |
| 2 | Public `materialize(max_bytes, ..)` ignored `FileApiLimits::max_materialize_bytes` (caller could pass `u64::MAX`); API absent from the M1 contract | Method removed from the API. `BlobData` public API is now exactly the M1 contract; `max_materialize_bytes` remains enforced by `FileApiLimits::validate()` (M1-CORE-08). Tests read blob content via child-module access to private fields, bounded by `blob.size()`. |
| 3 | `#[allow(dead_code)]` in production code (`blob.rs:131`) | Removed together with the unused `segments()` accessor; no `allow` attributes remain in production code (`crates/boa_fapi_core/src/`). |
| 4 | Root `[workspace.lints.clippy]` still allowed `unwrap`/`expect`/`panic` | Root `Cargo.toml` now denies `unwrap_used`, `expect_used`, `panic` workspace-wide; `cargo clippy --workspace --all-targets --all-features -- -D warnings` enforces it. Test-only allowlist documented in each `tests/*.rs` header. |
| 5 | `memory_source_result_shares_allocation` checked only length/bytes, not shared allocation | Strengthened with exact pointer equality via `Bytes::as_ptr()`: a full-range read must return a `Bytes` over the very same allocation. |

### Rework validation

All commands of work order §8 re-run from the root on 2026-09-06, exit code 0: fmt check, clippy `-D warnings`, 120 tests (37 `blob::tests` unit tests incl. the property test, 19 source, 25 endings, 16 mime, 17 limits, 6 guards incl. 3 scanner self-tests), rustdoc `-Dwarnings`, doc-tests, `cargo llvm-cov` 97.84% lines (threshold 85%), `cargo hack check --feature-powerset --depth 2`, `cargo deny fetch db` + `cargo deny check` (both exit 0 in an available network; an earlier reviewer run of these two steps hit external GitHub advisory DB unavailability — network condition, not a code defect), `git diff --check`. Details in `docs/m1-validation.md`.

## Step D — Final Validation (Post-Fix)

| # | Command | Exit Code | Result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 0 | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 0 | PASS |
| 3 | `cargo test --workspace --all-features` | 0 | PASS (119 tests, 0 failed) |
| 4 | `cargo doc --workspace --no-deps` (RUSTDOCFLAGS='-Dwarnings') | 0 | PASS |
| 5 | `cargo test --package boa_fapi_core --doc` | 0 | PASS |
| 6 | `cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85` | 0 | PASS (95.22%) |
| 7 | `cargo hack check --feature-powerset --depth 2` | 0 | PASS |
| 8 | `cargo deny fetch db` + `cargo deny check` | 0 | PASS (with CARGO_DENY_DB_PATH) |
| 9 | `git diff --check` | 0 | PASS |

## Audit Conclusion

- All M1-CORE-01…09 requirements verified with specific code and test evidence.
- 5 defects found and fixed during audit; 5 review blockers found and fixed during rework (see above); no remaining open issues.
- 120 tests passing after rework; 97.84% line coverage (threshold: 85%).
- No masked failures, skips, or changed acceptance criteria.
