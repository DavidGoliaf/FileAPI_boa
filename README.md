# boa-fapi

A Rust implementation of the [File API](https://www.w3.org/TR/FileAPI/) for the [Boa](https://github.com/boa-dev/boa) JavaScript engine.

## Workspace structure

| Crate | Purpose |
|---|---|
| `boa_fapi_core` | Platform-independent data model and algorithms (no Boa dependency) |
| `boa_fapi` | Boa bindings: `Blob`, `File`, `FileList` (M2), promise reads (M3-A); Web IDL conversions, brands, GC-safe native data |
| `boa_fapi_fs` | Future filesystem-backed sources (M5+) |
| `boa_fapi_wpt` | Future WPT test harness (M7+) |

## Current scope (M1–M3-B)

- **M1 (`boa_fapi_core`)**: immutable byte sources, segmented blobs, File API slice semantics, MIME type normalization, and line ending conversion.
- **M2 (`boa_fapi`)**: registration of `Blob`, `File` and host-created `FileList` into a real `boa_engine::Context` — constructors with correct `name`/`length`/descriptors, `Symbol.toStringTag`, non-forgeable internal brands, `File.prototype → Blob.prototype` inheritance, Web IDL conversions (`DOMString`, `USVString`, `[Clamp] long long`, `long long`, `unsigned long`), Blob parts (USVString, BufferSource snapshot copy, Blob/File zero-copy composition), injectable `Clock` for `File.lastModified`, and atomic registration with rollback.
- **M3-A (`boa_fapi`)**: memory-backed promise reads `Blob.prototype.text()`, `arrayBuffer()`, `bytes()` (inherited by `File`). Each call returns a pending `Promise` immediately; materialization (bounded by `max_materialize_bytes`), packaging, and settlement happen only in a Boa `PromiseJob` after the embedder calls `context.run_jobs()`:

- **M1 (`boa_fapi_core`)**: immutable byte sources, segmented blobs, File API slice semantics, MIME type normalization, and line ending conversion.
- **M2 (`boa_fapi`)**: registration of `Blob`, `File` and host-created `FileList` into a real `boa_engine::Context` — constructors with correct `name`/`length`/descriptors, `Symbol.toStringTag`, non-forgeable internal brands, `File.prototype → Blob.prototype` inheritance, Web IDL conversions (`DOMString`, `USVString`, `[Clamp] long long`, `long long`, `unsigned long`), Blob parts (USVString, BufferSource snapshot copy, Blob/File zero-copy composition), injectable `Clock` for `File.lastModified`, and atomic registration with rollback.
- **M3-A (`boa_fapi`)**: memory-backed promise reads `Blob.prototype.text()`, `arrayBuffer()`, `bytes()` (inherited by `File`). Each call returns a pending `Promise` immediately; materialization (bounded by `max_materialize_bytes`), packaging, and settlement happen only in a Boa `PromiseJob` after the embedder calls `context.run_jobs()`:

```rust
use boa_engine::{Context, Source};
use boa_fapi::FileApiExtension;

let extension = FileApiExtension::builder().build();
let context = &mut Context::default();
extension.register(context).expect("registration failed");
context
    .eval(Source::from_bytes(
        "globalThis.result = 'pending'; \
         new Blob(['abc']).text().then(v => { globalThis.result = v; });",
    ))
    .expect("evaluation failed");
// Nothing has settled yet: the read runs in a queued Boa job.
context.run_jobs().expect("jobs failed");
```

- **M3-B (`boa_fapi`)**: demand-driven `Blob.prototype.stream()` (fresh
  offset-0 `Uint8Array` chunks) and `textStream()` (incremental UTF-8
  strings, split-safe) via a branded `ReadableStream` shim with
  `ReadableStreamDefaultReader`. Chunks come strictly from `read()` demand
  through Boa jobs (`context.run_jobs()`), one chunk per request, with
  backpressure, FIFO, EOF, cancellation, and isolated errors. Disable with
  `FileApiExtensionBuilder::streams_shim(false)` or `--no-default-features`.

Not yet implemented (M4–M7): FileReader, DOMException/EventTarget, blob URLs, structured clone, filesystem-backed sources, WPT harness. Full WHATWG Streams (`pipeTo`, `tee`, BYOB, transformers) is out of scope: the shim implements only the M3-B surface above.
## Building

```sh
cargo build --workspace --all-features
```

## Testing

```sh
cargo test --workspace --all-features
```

The M2 integration tests execute real JavaScript (`new Blob`, `new File`,
`instanceof`, borrowed getters, `slice`, `FileList.item`) inside a Boa
`Context`:
`cargo test --package boa_fapi --test m2_blob_file_filelist`.
Promise reads additionally require driving the Boa job queue
(`context.run_jobs()`):
`cargo test --package boa_fapi --test m3_promise_blob_reads`.
Streams likewise settle only through `context.run_jobs()`:
`cargo test --package boa_fapi --test m3_blob_streams`.
