# boa-fapi

A Rust implementation of the [File API](https://www.w3.org/TR/FileAPI/) for the [Boa](https://github.com/boa-dev/boa) JavaScript engine.

## Workspace structure

| Crate | Purpose |
|---|---|
| `boa_fapi_core` | Platform-independent data model and algorithms (no Boa dependency) |
| `boa_fapi` | Boa bindings: `Blob`, `File`, `FileList` (M2), promise reads (M3-A), streams shim (M3-B), DOM shim + async `FileReader` (M4-A); Web IDL conversions, brands, GC-safe native data |
| `boa_fapi_fs` | Future filesystem-backed sources (M5+) |
| `boa_fapi_wpt` | Future WPT test harness (M7+) |

## Current scope (M1–M4-A)

- **M1 (`boa_fapi_core`)**: immutable byte sources, segmented blobs, File API slice semantics, MIME type normalization, and line ending conversion.
- **M2 (`boa_fapi`)**: registration of `Blob`, `File` and host-created `FileList` into a real `boa_engine::Context` — constructors with correct `name`/`length`/descriptors, `Symbol.toStringTag`, non-forgeable internal brands, `File.prototype → Blob.prototype` inheritance, Web IDL conversions (`DOMString`, `USVString`, `[Clamp] long long`, `long long`, `unsigned long`), Blob parts (USVString, BufferSource snapshot copy, Blob/File zero-copy composition), injectable `Clock` for `File.lastModified`, and atomic registration with rollback.
- **M3-A (`boa_fapi`)**: memory-backed promise reads `Blob.prototype.text()`, `arrayBuffer()`, `bytes()` (inherited by `File`). Each call returns a pending `Promise` immediately; materialization (bounded by `max_materialize_bytes`), packaging, and settlement happen only in a Boa `PromiseJob` after the embedder calls `context.run_jobs()`. Failures reject with the mapped `DOMException` (see M4-A mapping below):

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
- **M4-A (`boa_fapi`)**: minimal self-contained DOM shim plus asynchronous
  `FileReader` for memory-backed `Blob`/`File` only. Installed globals:
  `EventTarget` (listener registration, at-target dispatch),
  `Event`/`ProgressEvent` (readonly attributes), `DOMException`
  (`InvalidStateError`, `NotReadableError`, `AbortError`,
  `EncodingError`, `SecurityError`, `NotFoundError`,
  `QuotaExceededError`; core failures map centrally onto these names), and
  `FileReader` (inherits `EventTarget`; `readAsArrayBuffer`,
  `readAsBinaryString`, `readAsText`, `readAsDataURL`, `abort`;
  `EMPTY`/`LOADING`/`DONE`; readonly `readyState`/`result`/`error`; six
  writable `on*` handlers). Every read enqueues FileReading jobs that are
  delivered only through the ordinary `context.run_jobs()` cycle — the
  embedder must drain jobs explicitly, exactly as for M3-A/M3-B:

```rust
use boa_engine::{Context, Source};
use boa_fapi::FileApiExtension;

let extension = FileApiExtension::builder().build();
let context = &mut Context::default();
extension.register(context).expect("registration failed");
context
    .eval(Source::from_bytes(
        "globalThis.reader = new FileReader(); \
         globalThis.reader.onload = function () { globalThis.text = this.result; }; \
         globalThis.reader.readAsText(new Blob(['abc']));",
    ))
    .expect("evaluation failed");
// Nothing has fired yet: events dispatch from queued FileReading jobs.
context.run_jobs().expect("jobs failed");
```

  Boundary: memory-backed sources only; `progress` is throttled to one
  event per 50 ms of the injected `Clock` (one per chunk when chunks are
  rarer). Disable the whole shim with
  `FileApiExtensionBuilder::dom_shim(false)` or `--no-default-features`
  (registration then fails with `RegisterError::DomShimDisabled` before
  any `globalThis` mutation, since no host DOM adapter exists).

Not yet implemented (M4-B–M7): `FileReaderSync`, worker environments,
filesystem-backed sources, blob URLs, structured clone, full DOM/HTML,
full WHATWG Streams (`pipeTo`, `tee`, BYOB, transformers), WPT harness.
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
`FileReader` events likewise fire only through `context.run_jobs()`:
`cargo test --package boa_fapi --test m4_filereader_async`.
