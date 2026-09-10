# Host integration (M5 filesystem, M9-B/M9-C/M9-D I/O loop)

## Promise-read, FileReader and stream I/O loop (M9-B/M9-C/M9-D)

`Blob.prototype.text()`, `arrayBuffer()` and `bytes()` return a pending
`Promise` before any blocking read runs. Async `FileReader.readAs*`
likewise returns (with the reader in `LOADING`) before any chunk runs.
`ReadableStreamDefaultReader.read()` likewise returns a pending promise
(one FIFO demand slot) before any chunk runs. Filesystem work runs on a
`FileIoExecutor`; the host turns completions into Boa jobs with `poll_io`,
then settles them with `run_jobs()`. Repeat both until the host and the
File API queues are quiescent:

```text
wait for FileIoWake or other host event
handle.poll_io(&mut context)
context.run_jobs()
repeat until host and File API queues are quiescent
```

One `Context::run_jobs()` without `poll_io` is not required to wait for
OS I/O. No automatic integration with an arbitrary Boa `JobQueue` is
claimed: the host always drives `poll_io` explicitly. Inject the
executor and the wake hook through the builder:

```rust,no_run
use std::sync::Arc;
use boa_engine::Context;
use boa_fapi::{FileApiExtension, FileIoExecutor, FileIoWake, NoopWake, ThreadedFileIoExecutor};

let executor: Arc<dyn FileIoExecutor> = Arc::new(ThreadedFileIoExecutor::new(4, 128));
let wake: Arc<dyn FileIoWake> = Arc::new(NoopWake);
let mut context = Context::default();
let handle = FileApiExtension::builder()
    .io_executor(executor)
    .io_wake(wake)
    .build()
    .register(&mut context)
    .unwrap();
// ... JS calls text()/arrayBuffer()/bytes() ...
handle.poll_io(&mut context).unwrap();
context.run_jobs().unwrap();
```

The built-in executor is a fixed pool with a bounded queue
(thread-per-read without a limit is forbidden); `submit` (promise reads),
`submit_reader` (one FileReader chunk per request, no readahead, no
whole-blob accumulation) and `submit_stream` (one stream chunk per demand,
at most one in flight per stream, no readahead) never block and a full
queue surfaces as a typed resource error. `shutdown` cancels outstanding
work, clears queued completions, and forbids late settlement.

FileReader fairness: one drained chunk becomes at most one pump Boa job,
which submits at most one next chunk request. The host may bound reader
completions per `poll_io` with `handle.set_poll_io_budget(Some(n))`
(`None` drains everything): leftovers keep their FIFO position and
re-wake the host (`FileIoWake`), so a busy reader cannot starve promise
reads or other readers. Queue and active-operation counts stay bounded by
`FileApiLimits` (`max_concurrent_reads_per_global`, chunk ceiling).

Stream demand (M9-D): one drained chunk settles exactly one queued `read()`
and submits at most one next request when more demand waits — still at most
one in-flight chunk per stream. EOF runs a shared terminal transition
(payload cursor cleared, operation root and payload removed, quota released
exactly once — before any Promise job is queued) and then resolves every
queued read (and all future reads) as done; a text-tail EOF delivers the
decoder flush as the single final `{ value, done: false }` for its demand
and terminates the stream on the same transition. The first source error
rejects every queued and future read with the stored mapped `DOMException`
and performs no further source reads. `cancel()` settles queued reads done
synchronously and makes the in-flight chunk stale; `releaseLock()` refuses
with queued/in-flight demand so no completion is lost. Late completions
after any terminal path are strict no-ops (no JS mutation, no telemetry,
no second release).

## Opening and authorizing a resource

The host opens the file read-only **before** any JS `File` exists. Any
location handling (canonicalization, root confinement, symlink/junction
checks) happens in host code, outside `boa_fapi`:

```rust,no_run
use boa_fapi_fs::FsRegistry;

let registry = FsRegistry::new();
let file = std::fs::OpenOptions::new().read(true).open("data.bin").unwrap();
let resource = registry.register(file).unwrap();
```

Optional policy approval (default denies everything):

```rust,no_run
use boa_fapi_core::policy::FileOpenRequest;
use boa_fapi_fs::{FsRegistry, RegistryPolicy};
use boa_fapi_core::policy::FileAccessPolicy;

let registry = FsRegistry::new();
let policy = RegistryPolicy::new(registry.clone());
// `request.resource` is the opaque id from `resource.id()`; no location input exists.
// On Windows / non-Unix targets `authorize_open` refuses: use copy_on_import below.
```

## Passing an explicit display name

Unix (strong identity) — live-handle import:

```rust,no_run
use std::sync::Arc;
use boa_engine::{Context, Source};
use boa_fapi::{FileApiExtension, HostFileOptions};
use boa_fapi_fs::{FsRegistry, HostFileSource};

let registry = FsRegistry::new();
let mut context = Context::default();
let handle = FileApiExtension::builder().build().register(&mut context).unwrap();
// `resource` came from `registry.register(open_read_only_file)`.
let resource = registry.register(std::fs::File::open("data.bin").unwrap()).unwrap();
let adapter: Arc<dyn boa_fapi_core::policy::FileResource> =
    Arc::new(HostFileSource::new(&registry, &resource, None).unwrap());
let file = handle
    .file_from_resource(&registry, adapter, "report.txt", HostFileOptions::default(), &mut context)
    .unwrap();
```

Windows / other non-Unix targets (no strong identity) — enforced
`copy_on_import` fallback (immutable memory bytes; one copy consumes its
registration, and the live handle closes on every exit — success, limit
refusal, allocation failure, or read error; re-register for another
copy):

```rust,no_run
use boa_engine::Context;
use boa_fapi::{FileApiExtension, HostFileOptions};
use boa_fapi_fs::FsRegistry;

let registry = FsRegistry::new();
let mut context = Context::default();
let handle = FileApiExtension::builder().build().register(&mut context).unwrap();
let resource = registry.register(std::fs::File::open("data.bin").unwrap()).unwrap();
let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, 256 * 1024 * 1024).unwrap();
let file = handle
    .file_from_bytes(bytes, "report.txt", HostFileOptions::default(), &mut context)
    .unwrap();
```

`display_name` is the only name JS observes (`/` becomes `:`; no
basename is computed from host state). The import validates the live
snapshot before any JS object exists; a stale resource fails with no
partial state. Reads go through the async `FileReader`, worker
`FileReaderSync`, `text()`/`arrayBuffer()`/`bytes()`, `slice()`, and
`stream()` paths with the same error mapping (`NotReadableError`, no
location detail, no partial bytes).

## Configuring limits

`FileApiLimits` applies unchanged (`max_blob_size` preflighted at
import; `max_materialize_bytes`/`max_sync_read_bytes`/chunk/concurrency
quotas enforced on every read path with no filesystem bypass).

## Shutdown

```rust,no_run
# use boa_engine::Context;
# use boa_fapi::FileApiExtension;
# let mut context = Context::default();
# let handle = FileApiExtension::builder().build().register(&mut context).unwrap();
handle.shutdown(&mut context).unwrap();
```

Idempotent; rejects new host operations; runs every tracked registry's
`close_all` so OS handles drop **immediately** (not at registry
destruction); cancels pending filesystem work through the shared
cancellation token; late Boa jobs settle nothing after context
destruction. The examples above never print a location: display names in
outputs are placeholders only.
