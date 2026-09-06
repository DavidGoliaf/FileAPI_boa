# Architecture

## Layer model

```text
M1 core (data and algorithms, no Boa)
  → future boa_fapi (Web IDL / GC / jobs)
  → future host adapters (FS / DOM / Streams / URL)
```

### Layer 1: `boa_fapi_core` (M1)

Platform-independent crate containing:

- **ByteSource trait** — abstraction over immutable byte data. This is the boundary through which future file-backed sources will check snapshot state and cancellation. In M1, only `MemorySource` (in-memory `Bytes` wrapper) exists.
- **BlobData** — segmented immutable byte storage. Stores `Vec<BlobSegment>`, each holding an `Arc<dyn ByteSource>` with offset and length. Slicing reuses `Arc` pointers without copying payload.
- **Resource limits** — `FileApiLimits` struct with validation, enforcing size, count, and concurrency constraints.
- **Error model** — `FileApiError` enum covering not-found, permission, cancellation, range, and resource limit errors.
- **MIME normalization** — `normalize_blob_type()` implementing File API spec: ASCII lowercase, reject non-printable.
- **Line endings** — `convert_line_endings_to_native()` normalizing CR/LF/CRLF to target ending.
- **Cancellation** — `CancellationToken` for cooperative cancellation of read operations.

### Layer 2: `boa_fapi` (future M2+)

Will provide Web IDL bindings connecting `boa_fapi_core` types to Boa's JavaScript engine:
- `Blob` constructor and `Blob.prototype` methods
- `File` constructor
- `FileReader`
- GC integration for JS-visible objects

### Layer 3: Host adapters (future M5+)

Platform-specific implementations:
- `boa_fapi_fs` — filesystem-backed `ByteSource` with snapshot checking
- `boa_fapi_wpt` — WPT test harness
- Future: DOM adapter, Streams adapter, URL store

## ByteSource as boundary

`ByteSource` is the key abstraction boundary. It defines:
- `len()` — total byte count
- `snapshot()` — returns `SnapshotState` (in M1, only `Memory`)
- `read_range()` — reads a byte range with cancellation support

Future filesystem implementations will check snapshot stability on each read and return `SnapshotChanged` if the file was modified. M1's `MemorySource` always returns `SnapshotState::Memory` and never changes after construction.

## Dependency graph

```text
boa_fapi_core (no external runtime deps)
    └── bytes, thiserror

boa_fapi (future) → boa_fapi_core + boa_engine
boa_fapi_fs (future) → boa_fapi_core + std::fs
boa_fapi_wpt (future) → boa_fapi_core + test harness
```
