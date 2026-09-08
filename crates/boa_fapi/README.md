# `boa_fapi`

Pipeline role: Boa bindings M2–M6 over `boa_fapi_core` — `Blob`, `File`,
`FileList`, promise reads, streams shim, DOM shim with async `FileReader`,
worker-only sync `FileReaderSync`, `fs` host import with shutdown, Blob URL
store with `URL` shim, structured-clone bridge. Web IDL conversions,
brands, GC-safe native data.

Explicit jobs loop (no background runtime): every async settlement runs
only after the embedder drains `context.run_jobs()`.

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
cargo test --package boa_fapi --test m8_observability --all-features -- --nocapture
```

Wasm memory-only gate (lib, no default features):

```sh
cargo check --target wasm32-unknown-unknown --package boa_fapi --no-default-features --lib
```

Limits: no full DOM/HTML, no Workers runtime, no Fetch, no full Streams
(`pipeTo`, `tee`, BYOB), no File System Access write API.

See top-level `README.md` and `TZ_boa_fapi_FileAPI.md`.
