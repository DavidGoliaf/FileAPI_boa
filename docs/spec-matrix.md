# M1 Specification Traceability Matrix

Public `BlobData` API is exactly the M1 contract (`empty`, `from_segments`, `size`,
`media_type`, `snapshot`, `segment_count`, `slice`). Tests that need internals
(content of a blob or its segments) live in the `#[cfg(test)]` module beside
`src/blob.rs` and read the private fields through normal child-module privacy;
integration suites live in `crates/boa_fapi_core/tests/`.

| ID | Normative rule | Code | Test |
|---|---|---|---|
| M1-CORE-01 | core does not depend on Boa | `crates/boa_fapi_core/Cargo.toml` — only `bytes` + `thiserror` | `guards_tests::core_cargo_toml_no_forbidden_dependencies` |
| M1-CORE-02 | ByteSource range/cancel contract | `crates/boa_fapi_core/src/source/mod.rs` — `ByteSource` trait; `crates/boa_fapi_core/src/source/memory.rs` — `MemorySource` impl | `source_tests::*` (19 tests: full/empty/prefix/middle/suffix range, start>end, end>len, u64::MAX, cancel before read, clone sees cancel, shares allocation) |
| M1-CORE-03 | immutable memory source | `crates/boa_fapi_core/src/source/memory.rs` — `MemorySource::new`, `read_range` returns `Bytes::slice` | `source_tests::memory_source_result_shares_allocation` (exact pointer equality via `Bytes::as_ptr()`), `blob::tests::memory_source_immutable_after_new`, `blob::tests::blob_data_immutable_repeated_reads` |
| M1-CORE-04 | Blob type normalization | `crates/boa_fapi_core/src/mime.rs` — `normalize_blob_type()` | `mime_tests::*` (16 tests: empty, ASCII lowercase, mixed case, spaces preserved, DEL/LF/CR/NUL/non-ASCII/emoji give empty, printable ASCII all preserved, property tests) |
| M1-CORE-05 | native line endings | `crates/boa_fapi_core/src/endings.rs` — `convert_line_endings_to_native()` | `endings_tests::*` (25 tests: both targets × empty/no-endings/CR/LF/CRLF/CRCR/LFLF/CRLFCR/start/end/Unicode, property: no standalone CR, count preserved) |
| M1-CORE-06 | segment validation and checked arithmetic | `crates/boa_fapi_core/src/blob.rs` — `BlobData::from_segments()` | `blob::tests::from_segments_*` (13 tests: single/multiple sources, offsets, zero-length filtered, zero-length invalid offset rejected, invalid offset/end, offset overflow, size at/over limit, count at/over limit, normalized MIME) |
| M1-CORE-07 | File API slice semantics | `crates/boa_fapi_core/src/blob.rs` — `BlobData::slice()` | `blob::tests::slice_*` (19 tests: None/None, 0/size, positive beyond, negative -1/-size/<-size, i64::MIN/MAX, end<start, empty, crosses 1/2/all segments, override type normal/invalid/none, original unchanged, `Arc::ptr_eq` sharing, fast path) + `blob::tests::slice_matches_reference` (property) |
| M1-CORE-08 | limits validation | `crates/boa_fapi_core/src/limits.rs` — `FileApiLimits::validate()` | `limits_tests::*` (17 tests: exact defaults, each zero limit, each invalid relation, boundary OK) |
| M1-CORE-09 | safe Rust/no panics in public paths | `crates/boa_fapi_core/src/lib.rs` — `#![deny(unsafe_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]`; root `Cargo.toml` `[workspace.lints.clippy]` denies `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented` for every workspace member | `guards_tests::production_source_no_unwrap_expect_panic` (scanner strips `#[cfg(test)]` module bodies; scanner covered by `strip_test_modules_*` self-tests), `guards_tests::public_api_no_path_types` |
