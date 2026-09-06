# Architecture

## Layer model

```text
M1 core (data and algorithms, no Boa)
  → M2 boa_fapi (Web IDL bindings, brands, GC integration)
  → future host adapters (FS / DOM / Streams / URL)
```

### Layer 1: `boa_fapi_core` (M1)

Platform-independent crate containing:

- **ByteSource trait** — abstraction over immutable byte data. This is the boundary through which future file-backed sources will check snapshot state and cancellation. In M1, only `MemorySource` (in-memory `Bytes` wrapper) exists.
- **BlobData** — segmented immutable byte storage. Stores `Vec<BlobSegment>`, each holding an `Arc<dyn ByteSource>` with offset and length. Slicing reuses `Arc` pointers without copying payload. Segmentation stays private: bindings compose blobs through `concat_shared`/`push_shared`, read bytes through `read_all`, and prove sharing through `shares_sources_with`/`first_segment_shares_source_with`.
- **Resource limits** — `FileApiLimits` struct with validation, enforcing size, count, and concurrency constraints.
- **Error model** — `FileApiError` enum covering not-found, permission, cancellation, range, and resource limit errors.
- **MIME normalization** — `normalize_blob_type()` implementing File API spec: ASCII lowercase, reject non-printable.
- **Line endings** — `convert_line_endings_to_native()` normalizing CR/LF/CRLF to target ending.
- **Cancellation** — `CancellationToken` for cooperative cancellation of read operations.

### Layer 2: `boa_fapi` (M2)

Boa bindings for the M2 surface. Module responsibilities:

- `extension.rs` — `FileApiExtension`/builder, atomic registration
  (build → preflight → install → rollback), re-registration rule
  (`AlreadyRegistered`), and the host-facing `FileApiHandle`
  (`blob_from_bytes`, `file_from_bytes`, `file_list`).
- `webidl.rs` — the single conversion layer: `DOMString`, `USVString`,
  `[Clamp] long long`, `long long`, `unsigned long`, the `BlobPart` union
  (USVString / BufferSource / Blob / File) and options-bag parsing.
- `brand.rs` — the only brand gates (`require_blob`, `require_file`,
  `require_file_list`) based on native data types, not JS-visible state.
- `blob.rs` / `file.rs` / `file_list.rs` — native data (`BlobNative`,
  `FileNative`, `FileListNative`), constructors, getters, `slice`.
- `clock.rs` — injectable `Clock` for `File.lastModified` defaults.
- `error.rs` — `RegisterError` and core-error → JS-error mapping.

Registration is atomic: constructor/prototype objects are built first, all
own global names and global extensibility are preflighted, `globalThis` is
changed only afterwards, and a failed install rolls back so none of the
three globals remains installed. `FileList` has no public constructor.

Native data owns `Arc<BlobData>` plus immutable Rust strings/numbers only;
it contains no `JsObject`/`JsValue`/`Context` and is GC-safe through the
`boa_gc` derive with `#[unsafe_ignore_trace]` on non-GC fields.

Not implemented (M3+): promise-returning reads, streams, FileReader, DOM
events, blob URLs, structured clone.

### Layer 3: Host adapters (future M5+)

Platform-specific implementations:
- `boa_fapi_fs` — filesystem-backed `ByteSource` with snapshot checking
- `boa_fapi_wpt` — WPT test harness
- Future: DOM adapter, Streams adapter, URL store

## ByteSource as boundary

`ByteSource` is the key abstraction boundary. It defines:
- `len()` — total byte count
- `snapshot()` — returns `SnapshotState` (for M1/M2, only `Memory`)
- `read_range()` — reads a byte range with cancellation support

Future filesystem implementations will check snapshot stability on each read and return `SnapshotChanged` if the file was modified. M1's `MemorySource` always returns `SnapshotState::Memory` and never changes after construction.

## Dependency graph

```text
boa_fapi_core (no external runtime deps beyond bytes/thiserror)
    └── bytes, thiserror

boa_fapi → boa_fapi_core + boa_engine + boa_gc + bytes + thiserror
boa_fapi_fs (future) → boa_fapi_core + std::fs
boa_fapi_wpt (future) → boa_fapi_core + test harness
```
