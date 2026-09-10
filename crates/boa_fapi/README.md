# `boa_fapi`

Pipeline role: Boa bindings M2–M6 over `boa_fapi_core` — `Blob`, `File`,
`FileList`, promise reads, streams shim, DOM shim with async `FileReader`,
worker-only sync `FileReaderSync`, `fs` host import with shutdown, Blob URL
store with `URL` shim, structured-clone bridge. Web IDL conversions,
brands, GC-safe native data.

Explicit jobs loop (no background runtime): every async settlement runs
only after the embedder drives the M9-B/M9-C/M9-D host loop:

```text
wait for FileIoWake or other host event
handle.poll_io(&mut context)
context.run_jobs()
repeat until host and File API queues are quiescent
```

One `run_jobs()` without `poll_io` is not required to wait for OS I/O;
no automatic integration with an arbitrary Boa `JobQueue` is claimed.
Promise reads (`text()`/`arrayBuffer()`/`bytes()`) submit a Send-only
task to the configured `FileIoExecutor` (default: bounded pool, 4
workers / 128 queued) and settle only through a Boa job after `poll_io`.
Async `FileReader.readAs*` submits one chunk request per drained
completion through the same executor (no readahead, FIFO within one
reader) and dispatches events only from `poll_io` pump jobs; the host may
bound reader completions per `poll_io` with
`handle.set_poll_io_budget(Some(n))` for fairness. Stream `read()` submits
at most one bounded chunk request per demand (at most one in flight per
stream, no readahead) through `submit_stream` and settles only through a
Boa job after `poll_io`: one completion settles one demand, EOF runs a
shared terminal transition (state cleared, quota released exactly once,
before any Promise job is queued) and then resolves queued/future reads
done, source errors reject with the stored mapped
`DOMException` and perform no further reads.

Host File/capability boundary: filesystem `File` enters only via
`FileApiHandle::file_from_resource` with a pre-opened registry slot; JS
observes content plus the explicit display name, never a path. Shutdown via
`FileApiHandle::shutdown` clears URL state, closes tracked registries,
cancels pending work; late jobs settle nothing.

Feature table (all default-on except `tracing`):

| Feature | Default | Effect when off |
|---|---|---|
| `streams-shim` | on | `register` fails (`StreamsShimDisabled`) before globals change |
| `dom-shim` | on | `register` fails (`DomShimDisabled`); no `FileReader`/`FileReaderSync` |
| `fs` | on | no `file_from_resource`; memory API unchanged |
| `url-shim` | on | no `URL` global; host store ops keep working |
| `structured-clone` | on | no bridge surface; M1–M5 unchanged |
| `tracing` | off | no dependency, no events, no JS/API/job change |

Optional `tracing` (default off): terminal-only observer emitting one
`boa_fapi::file_api.operation` event per completion with exactly
`operation`, `size`, `duration_ms`, `chunk_count`, `result_class`,
`environment_hash`. No bytes, paths, URLs, handles, origins, partitions,
or error text ever enter telemetry.

Run:

```sh
cargo test --package boa_fapi --all-features
cargo test --package boa_fapi --test m9_promise_io -- --nocapture
cargo test --package boa_fapi --test m9_filereader_io -- --nocapture
cargo test --package boa_fapi --test m9_stream_io -- --nocapture
cargo test --package boa_fapi --test m8_observability --all-features -- --nocapture
```

Wasm memory-only gate (lib, no default features):

```sh
cargo check --target wasm32-unknown-unknown --package boa_fapi --no-default-features --lib
```

Limits: no full DOM/HTML, no Workers runtime, no Fetch, no full Streams
(`pipeTo`, `tee`, BYOB), no File System Access write API.

See top-level `README.md` and `TZ_boa_fapi_FileAPI.md`.
