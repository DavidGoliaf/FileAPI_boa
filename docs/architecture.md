# Architecture

## Layer model

```text
M1 core (data and algorithms, no Boa)
  → M2 boa_fapi (Web IDL bindings, brands, GC integration)
  → M5/M6 host adapters (FS / URL / clone bridge)
  → M7 conformance (WPT harness + acceptance/race/hardening suites)
```

### Layer 1: `boa_fapi_core` (M1)

Platform-independent crate containing:

- **ByteSource trait** — abstraction over immutable byte data. This is the boundary through which future file-backed sources will check snapshot state and cancellation. In M1, only `MemorySource` (in-memory `Bytes` wrapper) exists.
- **BlobData** — segmented immutable byte storage. Stores `Vec<BlobSegment>`, each holding an `Arc<dyn ByteSource>` with offset and length. Slicing reuses `Arc` pointers without copying payload. Segmentation stays private: bindings compose blobs only through `concat_shared`/`push_shared` and read bytes only through the bounded M3 `materialize`; public reads of segments or source identity do not exist.
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

Not implemented (M7+): full DOM/HTML, full WHATWG Streams beyond the
shim, workers runtime, WPT harness.

### Layer 2f: `boa_fapi` Blob URL store + `URL` shim (M6)
`boa_fapi_core::blob_url` owns the Boa-free store; `boa_fapi::url_shim`
owns the JS namespace; `extension.rs` owns the wiring:

- `EnvironmentDescriptor { kind, serialized_origin, partition, nonce }`
  (explicit host config, never inferred) + comparable `EnvironmentKey`:
  only the origin is serialized into `blob:<origin>/<uuid-v4>`; the
  partition and nonce never leave the key (redacted even from `Debug`);
- `BlobUrlStore` (per-context `Arc`, `Mutex<HashMap>` + atomic `seq`,
  O(1); lock held only for the map op): `insert_capped` (quota-atomic,
  `Collision` never overwrites), `resolve` (full-key check before the
  `Arc<BlobData>`; malformed/unknown/revoked/foreign share one opaque
  class), `revoke` (idempotent silent no-op; handed-out `Arc` reads to
  completion), `clear()` at shutdown (tracked closer, all strong refs
  released, idempotent);
- `URL` is a namespace object (not a constructor, not WHATWG URL):
  `createObjectURL` (Blob-brand only, `length` 1) draws 16 CSPRNG bytes
  from the configured `UrlEntropySource` (production `OsEntropy` via
  `getrandom`, tests inject deterministic entropy), retries collisions
  bounded with fresh entropy, returns the string in the calling realm
  with no Boa job; `revokeObjectURL` (same shape) always returns
  `undefined`; failures are one network-error equivalent `TypeError`
  (never token/UUID/origin/existence/host detail); ServiceWorker forbids
  creation with no partial global change; `url-shim` off leaves the
  `URL` name untouched while host store ops keep working;
- `ResolvedBlob` (`Arc<BlobData>` + media type + checked length) is the
  host Fetch boundary: no network handler is registered by `boa-fapi`.

### Layer 2g: `boa_fapi` structured-clone bridge (M6, no `boa-idb`)
`boa_fapi_core::clone` owns the versioned encoding; `clone_bridge.rs`
owns the host bridge; `extension.rs` owns the entry points:

- layout `FCL1 | u32 version (= 1) | u32 tag (BLOB/FILE/FLST) | body`
  with `Cursor` checked arithmetic against `MAX_CLONE_BYTES` /
  `MAX_CLONE_STRING_BYTES` / `MAX_CLONE_FILES`: malformed/truncated/
  overflow/unknown-version/unknown-tag/trailing fail without panic or
  partial output; payloads carry materialized bytes + public metadata
  only (M1-normalized type, sanitized name, stored `lastModified`) —
  never paths, capabilities, OS handles or snapshot identities;
- `clone_blob`/`clone_file`/`clone_file_list` materialize through the
  existing checked path (`SourceFailed` typed, no partial payload;
  every list element brand-checked before output); `blob_from_clone`/
  `file_from_clone`/`file_list_from_clone` rebuild with a new immutable
  backing (kind mismatch fails before JS state);
- `CloneAdapter`/`CloneBridgeDescriptor` is the only `boa-idb` coupling
  (no dependency in any feature combination): version preflight before
  any `globalThis` mutation (`CloneBridgeIncompatible` + rollback),
  `NoBridge`/`Shutdown` before payload; no JS `structuredClone` global
  exists by design (host-side capability); `structured-clone` off keeps
  M1–M5 behavior with no partial surface.

### Layer 2b: `boa_fapi` promise reads (M9-B executor)

`io.rs` owns the explicit file I/O executor and completion bridge;
`promise_read.rs` owns the validation/submit/settle path for
`Blob.prototype.text()`, `arrayBuffer()`, and `bytes()`:

- brand/Web IDL validation, size preflight and quota reservation run on
  the Boa thread; failures settle the fresh pending promise through one
  Boa job without worker contact;
- the method submits a Send-only `FileIoTask` (shared `BlobData`,
  limits snapshot, cancellation token, bridge — no `JsValue`/`JsObject`/
  `Context`/paths) to the context `FileIoExecutor` and returns the
  pending `Promise` immediately;
- a worker materializes without Boa, pushes a Rust-only
  `FileIoCompletion` (bytes or typed `FileApiError`), and signals the
  `FileIoWake` hook; packaging (`String`/`ArrayBuffer`/`Uint8Array`)
  happens on the Boa thread after `poll_io`;
- `FileApiHandle::poll_io` (owner only; foreign contexts rejected)
  turns DTOs into Boa settlement jobs without calling user JS or holding
  the bridge mutex across Boa calls; `has_pending_io` reports
  outstanding work; `shutdown` cancels, clears, and forbids late
  settlement with exact-once quota release;
- the built-in `ThreadedFileIoExecutor` is a fixed pool with a bounded
  queue (thread-per-read without a limit is forbidden); tests inject a
  controlled manual executor. M9-C (FileReader) reuses the same bridge:
  one operation slot per read, one chunk request per drained completion
  (no readahead), FIFO within one reader, stale generations dropped
  before any JS mutation; the host may bound reader completions per
  `poll_io` (`set_poll_io_budget`, leftovers re-wake) so a busy reader
  cannot starve promise reads or other readers.

### Layer 2c: `boa_fapi` streams shim (M3-B surface, M9-D I/O)
`streams.rs` owns the branded `ReadableStream` shim:

- `Blob.prototype.stream()`/`textStream()` (Blob-brand only, inherited by
  `File`) create fresh unlocked streams that reserve one `IoBridge` slot
  and register a stream root; nothing is read until `reader.read()`;
- each `read()` creates one pending promise plus one FIFO demand slot and
  submits at most one bounded `StreamChunkTask` when nothing is in flight
  (no read-ahead): the worker reads exactly one `[loaded, loaded+chunk)`
  range off-thread through `read_blob_range` and pushes a Rust-only
  `StreamChunkCompletion` (chunk/EOF/typed error, never a JS object);
- `FileApiHandle::poll_io` drains stream completions FIFO per stream and
  turns each into at most one settlement Boa job (fresh `Uint8Array` or
  incremental UTF-8 string, `{value, done}` settlement, EOF with decoder
  flush and queued/future done, sticky terminal error/cancel states with
  stored mapped `DOMException` replay and no further source reads);
- at most one chunk is in flight per stream; chunk reservations are tracked
  in `IoBridge::chunk_reservations` so whole-blob promise FIFO never blocks
  behind live streams; every terminal path (EOF, error, cancel, shutdown)
  frees its quota exactly once via a shared terminal-EOF transition
  (`transition_stream_eof`: payload cursor cleared, operation root and
  payload removed, one `IoBridge::unreserve`, all before any Promise job is
  queued; idempotent, so a late completion can never double-release). After
  terminal EOF future `read()` calls resolve done without worker contact;
- `ReadableStream`/`ReadableStreamDefaultReader` constructors reject direct
  `new`; `getReader` locks, `releaseLock` unlocks only with no queued read
  and no in-flight I/O, stream/reader `cancel()` settle queued reads done
  synchronously on the calling stack (cancel promise resolves `undefined`
  through one Boa job) and make the in-flight chunk stale;
- GC/drop lifecycle (M9-D-R2): every stream epoch owns an explicit
  endpoint lease (`StreamShared::live_endpoints`/`live_readers` +
  `StreamLease { context, operation, generation, terminal, released }` —
  opaque ids only, no GC pointers). Stream/reader native data deregister
  their endpoint in both the GC finalizer and the Rust drop path
  (per-endpoint exactly-once claims, no `Rc::strong_count`): the
  deregistration only publishes a bounded `(context, operation,
  generation)` cleanup record into the context-pinned queue. `poll_io`
  validates each record (own context, live op root, same generation, not
  terminal, zero endpoints, empty queue, not in-flight, no live demand)
  and runs the single abandoned transition (token cancel, cursor clear, op
  root + payload removal, one conditional quota release, one `stream_read`
  event in the existing `cancelled` class — no JS event/error, no new
  telemetry class). Live read-promise demand outlives endpoint drops and
  settles first; post-settlement drains re-arm abandonment. All terminal
  paths (EOF/error/cancel/abandoned/shutdown) arbitrate one exactly-once
  ownership; `IoBridge::unreserve` is conditional-idempotent (repeat
  release of an unknown id never touches another slot); creation failures
  after `reserve()` roll back synchronously (no hidden slots);
- after M4-A stream errors reject with the central mapped `DOMException`
  (same mapping as promise reads and FileReader), not a plain `Error`.

### Layer 2d: `boa_fapi` DOM shim + `FileReader` (M4-A)
`dom.rs` owns the minimal branded DOM surface; `filereader.rs` owns the
asynchronous `FileReader` state machine:

- `dom-shim` Cargo feature (default on) +
  `FileApiExtensionBuilder::dom_shim(bool)` (default true): off returns
  typed `RegisterError::DomShimDisabled` before any `globalThis` mutation;
  the five globals install atomically with the M2/M3-B preflight/rollback;
- `EventTarget` (3 methods, tuple `(type, callback, capture)` dedupe,
  at-target dispatch only, listener exceptions never stop the remaining
  listeners), `Event` (7 readonly attributes + `preventDefault`/
  `stopImmediatePropagation`), `ProgressEvent` (inherits `Event`, 3
  readonly progress attributes), `DOMException` (inherits `Error`,
  readonly `name`/`message`, `[object DOMException]`, required names);
- `FileReader` inherits `EventTarget`: 5 async read methods, `EMPTY`/
  `LOADING`/`DONE` on constructor and prototype, readonly
  `readyState`/`result`/`error`, 6 writable `on*` handlers; initial state
  exactly `(EMPTY, null, null)`; `result` only `null`/DOMString/fresh
  `ArrayBuffer`; `error` only `null`/same-realm `DOMException`;
- `readAs*` validates synchronously, sets `(LOADING, null, null)`,
  reserves one `IoBridge` slot, and submits the first chunk request to
  the `FileIoExecutor` before returning — never a source read on the Boa
  thread (M9-C). A worker reads exactly one bounded `[offset, offset+len)`
  range off-thread (`FileReaderChunkTask::execute` → `read_blob_range`,
  panic-contained) and pushes a Rust-only `FileReaderChunkCompletion`
  (generation + chunk/EOF/typed error, never a JS object);
- `FileApiHandle::poll_io` drains reader chunks FIFO within one reader
  and turns each into at most one pump Boa job; the job dispatches
  `loadstart` on the first completion (including empty-blob EOF),
  packages bytes/text incrementally, emits throttled `progress` with a
  final `progress(loaded=total)` before `load`, then enqueues terminal
  `load`/`error` (+ conditional `loadend`); at most one next chunk
  request is submitted per drained chunk;
- `abort()` bumps the generation, cancels the worker token, releases the
  bridge slot exactly once, and queues `abort` (+ conditional `loadend`);
  completions of older generations (after abort/restart/shutdown) are
  dropped in `poll_io` with no JS mutation, event, telemetry, or second
  release; quota mirrors the bridge (`active` counter) with the same
  65th-reader `SecurityError` fast path;
- `readAsText` decodes incrementally through `encoding_rs` (replacement,
  split sequences, BOM); `readAsDataURL` checks `max_data_url_output`
  with checked arithmetic before allocation; memory stays O(chunk + final
  result); M3 promise/stream errors migrated once to the same central
  `DOMException` mapping; the four packagers live in the shared private
  `package.rs` so sync/async representations cannot diverge.

### Layer 2e: `boa_fapi` worker-only `FileReaderSync` (M4-B)
`filereader_sync.rs` owns the synchronous binding; `package.rs` owns the
packaging shared with the async reader:

- `FileApiEnvironment` (`Window` default, `DedicatedWorker`,
  `SharedWorker`, `ServiceWorker`) is explicit host config stored in the
  registered specs and handle; only the two worker descriptors build and
  install the single `FileReaderSync` global (atomically, with the same
  preflight/rollback contract); `Window`/`ServiceWorker` leave the name
  untouched, and the descriptor is never inferred from threads or
  callbacks;
- `FileReaderSync` is a stateless brand with exactly four prototype
  methods (`length` 1 each) plus the tag — no state, no `abort`, no
  handlers, no Promise/EventTarget surface; every call runs to completion
  on the calling stack with no jobs enqueued and no async-quota contact;
- fixed preflight order (brand → argument → label → sync-size limit →
  source read), `size > max_sync_read_bytes` as `QuotaExceededError`,
  checked data-URL length before reads/allocation, bounded
  `BlobData::materialize`, central `DOMException` mapping, no partial
  result;
- explicitly still omitted: `FileReaderSync` in window/service workers,
  filesystem-backed sources, snapshot validation, blob URLs, structured
  clone, full DOM/Workers runtime, WPT harness, M5.

### Layer 3: Host adapters (M5 filesystem)

`boa_fapi_fs` owns the capability-based filesystem source; `boa_fapi`
(`fs` feature, default on) owns the host import and the lifecycle:

- `boa_fapi_core::policy` — Boa-free boundary: `HostResourceId`,
  `FileOpenRequest`, `FileGrant`, `FileResource`, `FileResourceOpener`,
  `FileAccessPolicy`, `DenyAllPolicy`. No location/handle/secret in any
  type, method, or error.
- `boa_fapi_core::snapshot` — `FileSnapshot { identity, size, mtime }`
  plus `SnapshotState::Memory | Filesystem(FileSnapshot)`; `BlobData`
  derives its blob-level snapshot from its segments (first filesystem
  snapshot wins; the per-source check is the security boundary).
- `boa_fapi_fs::FsRegistry` — owns already-open read-only handles by
  opaque id; captures the import snapshot with safe `Metadata` APIs;
  the `Mutex` guards only the slot map and is never held across I/O
  (handles are cloned via `try_clone` under a short lock; metadata and
  positional reads run on the clone after the lock drops). `close`
  removes the slot (handle drops immediately), `close_all` drops every
  slot, `on_shutdown`/`run_closers` fire one-shot closers outside the
  lock.
- `boa_fapi_fs::FileSource` / `HostFileSource` — `ByteSource` over a
  registered slot, Unix-only (`PermissionDenied` elsewhere): cancel →
  checked arithmetic → live-vs-import snapshot → policy hook →
  positional read → exact-length check → post-read confirm;
  `open_copy_on_import` (every platform; closes the consumed registration
  on success, limit refusal, allocation failure, and read error — one
  copy per registration) is the enforced fallback for weak platforms and
  untrusted JS.
- `boa_fapi_fs::policy` — `DenyRawPathPolicy` (default deny),
  `RegistryPolicy` (live-slot approval + per-read revalidation; refuses
  `authorize_open` off-Unix), `RootConfinedPolicy` (open-handle identity
  only, never string prefix).
- `boa_fapi::FileApiHandle::file_from_resource(registry, Arc<dyn
  FileResource>, display_name, options, context)` — validates live==
  import plus the weak-platform gate before any JS object, preflights
  `max_blob_size`, tracks the registry for shutdown `close_all`, wraps in
  `ArcResourceSource` (shutdown-aware, per-read snapshot checks),
  attaches only the display name (`/` → `:`, no basename). Signature
  adaptation (`registry` + `Arc` vs target `&dyn`) recorded in ADR-0024.
- `boa_fapi::lifecycle` — `ShutdownFlag` (closed bit + cancellation +
  tracked closers) in `RegisteredSpecs`/handle/every fs import;
  `FileApiHandle::shutdown` runs all closers exactly once (`close_all`
  per tracked registry → OS handles drop **at shutdown**), cancels
  pending work, and makes late jobs settle nothing. Blob URL store and
  structured-clone lifetime stay M6 extension points.
- No JS path API exists: no raw-path import, no directory enumeration,
  no location/identity in JS errors, tracing, blob URLs, or artifacts.

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

boa_fapi → boa_fapi_core + boa_engine + boa_gc + bytes + thiserror + encoding_rs + base64 + getrandom (+ boa_fapi_fs with `fs`)
boa_fapi_fs → boa_fapi_core + bytes + thiserror + std::fs
boa_fapi_wpt → boa_fapi + boa_engine + thiserror (M7 harness: manifest, runner, reports, CLI; zero new deps)
```

### Layer 5: `boa_fapi` terminal-only `tracing` observer (M8)

`observability.rs` (compiled only with the default-off `tracing` feature)
is a terminal-only observer with no JS/IO coupling: one
`boa_fapi::file_api.operation` event per completion (`promise_read`,
`stream_read`, `filereader_read`, `filereader_sync`, `fs_read`,
`blob_url_create`, `blob_url_resolve`, `clone_encode`, `clone_decode`)
carrying exactly `operation`, `size`, `duration_ms`, `chunk_count`,
`result_class`, `environment_hash`. Timing uses a Rust monotonic clock for
`duration_ms` only; the environment hash covers the existing internal key
with a safe-Rust hasher. Stale FileReader completions and shutdown late
completions emit nothing. Feature-off builds contain no timer/trace call
on production paths. M8 adds no runtime-interoperability layer: no new
globals, handles, adapters, threads, or JS callbacks.

### Layer 4: `boa_fapi_wpt` conformance harness (M7)

The harness adds no JS API and changes no binding: it drives the
accepted M1–M6 surface from adapted WPT files in fresh Boa `Context`s.

- `manifest.rs` — strict `wpt-manifest.json` loader (schema 1, pinned
  commit hex, per-file SHA-256, exact subtest entries; unknown status,
  wildcards, missing gap fields and expired `review_by` are load
  errors, never silent `NOTRUN`);
- `harness.rs` — minimal testharness prelude (`test`/`async_test`/
  `promise_test`, assertions, `done`/`step`, cleanup) injected as JS
  source (no upstream fetch, no DOM);
- `runner.rs` — per-file fresh `Context` + `FileApiExtension`, bounded
  `run_jobs()` pump (64 passes + wall guard, no sleep), one terminal
  `PASS`/`FAIL`/`TIMEOUT` per subtest, `NOTRUN` rows from manifest gaps,
  `blob:`-scrubbed details, unexpected entries as explicit FAIL rows;
- `report.rs` — deterministic JSON/JUnit (manifest order, fixed keys,
  no timestamps/paths/secrets) plus the `strict_pass` gate;
- `main.rs` — CLI (`--manifest/--strict/--threads/--filter/--json/`
  `--junit/--timeout-ms`), SHA-256 verification before execution,
  self-contained SHA-256 (FIPS 180-4) and UTC-date review clock, 8 MiB
  worker thread for deeply recursive Boa evaluation (identical behavior
  on every platform).
