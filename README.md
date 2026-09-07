# boa-fapi

A Rust implementation of the [File API](https://www.w3.org/TR/FileAPI/) for the [Boa](https://github.com/boa-dev/boa) JavaScript engine.

## Workspace structure

| Crate | Purpose |
|---|---|
| `boa_fapi_core` | Platform-independent data model and algorithms (no Boa dependency) |
| `boa_fapi` | Boa bindings: `Blob`, `File`, `FileList` (M2), promise reads (M3-A), streams shim (M3-B), DOM shim + async `FileReader` (M4-A), worker-only sync `FileReaderSync` (M4-B), `fs` host import + shutdown (M5); Web IDL conversions, brands, GC-safe native data |
| `boa_fapi_fs` | Capability-based filesystem-backed `ByteSource` with snapshot validation (M5) |
| `boa_fapi_wpt` | Future WPT test harness (M7+) |

## Current scope (M1–M5)

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
- **M4-B (`boa_fapi`)**: worker-only synchronous `FileReaderSync` for
  memory-backed `Blob`/`File`. The host selects the environment
  explicitly with
  `FileApiExtensionBuilder::environment(FileApiEnvironment::DedicatedWorker
  | SharedWorker)` (default `Window` installs nothing new); only the two
  worker descriptors install the normative `FileReaderSync`
  (`readAsArrayBuffer`, `readAsBinaryString`, `readAsText`,
  `readAsDataURL`, each fully synchronous, no `Promise`/events/jobs).
  Every call preflights brand, size (`size > max_sync_read_bytes` throws
  `QuotaExceededError`), and the checked data-URL length before any
  source read or output allocation, then materializes through the bounded
  core path and packages with the same helpers as the async reader, so
  sync/async representations cannot diverge. Source failures throw the
  same mapped `DOMException` with no partial result; the async
   `max_concurrent_reads_per_global` quota is never touched.
- **M5 (`boa_fapi_fs` + `boa_fapi` `fs` feature)**: capability-based
  filesystem `File`. The host opens a read-only resource before any JS
  exists, registers it with `boa_fapi_fs::FsRegistry` (opaque id only —
  no location crosses the boundary), and imports it with
  `FileApiHandle::file_from_resource(registry, resource, display_name,
  options, context)` (Unix live-handle path; Windows / non-Unix hosts
  use enforced `open_copy_on_import` + `file_from_bytes` because no
  strong handle identity exists there). JS observes only content and the
  explicit display name. Every Unix `read_range` validates cancellation,
  checked arithmetic, and the live opaque snapshot (identity + size +
  mtime) against the import snapshot before reading (the registry mutex
  guards only the slot map and is never held across I/O — handles are
  cloned via `try_clone` first), verifies exact bytes afterwards, and
  fails with `SnapshotChanged`/`NotFound`/`FileLocked`/
  `PermissionDenied`/`InvalidRange` (JS: `NotReadableError`, no location
  detail, no partial bytes). Host-controlled `FileApiHandle::shutdown`
  runs every tracked registry's `close_all` (OS handles drop
  immediately), cancels pending filesystem work, rejects new operations,
  and lets late jobs settle nothing after context destruction. Disable
  with `--no-default-features` (memory API and registration keep
  working; no partial filesystem surface).

Not yet implemented (M6–M7): worker environments beyond the descriptor,
blob URLs, structured clone, full DOM/HTML,
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
`FileReaderSync` returns synchronously with no jobs involved:
`cargo test --package boa_fapi --test m4_filereader_sync`.
Filesystem capability/snapshot units (no Boa):
`cargo test --package boa_fapi_fs --all-features`.
Host `file_from_resource` integration (real JS + temp files + shutdown):
`cargo test --package boa_fapi --test m5_file_fs`.
