# Security model (M5 filesystem + M6 URL/clone)

## Threat model

- **Arbitrary location access from JS.** Impossible by construction: no
  JS API takes a location; `file_from_resource` takes the owning
  registry plus an opaque `Arc<dyn FileResource>`. The default policy
  denies every import.
- **Traversal (`..`) / symlink / Windows junction / reparse-point
  escape / aliasing.** On Unix, verification happens on the open
  handle's opaque identity captured at registration (`dev`+`ino`); no
  `starts_with(root)` string check exists anywhere — there is no
  location string to compare. On Windows and other non-Unix targets
  live-handle imports are **refused outright** (`PermissionDenied`):
  without a strong identity the comparison cannot detect replacement,
  so no weak attributes/mtime fallback is offered — hosts must use
  `copy_on_import` or deny.
- **Replacement race (TOCTOU, Unix).** The import snapshot is captured
  on the open handle; every `read_range` checks live == import before
  reading and confirms identity after reading. Chunked consumers issue
  one range per chunk, so validation runs before the first chunk and on
  every new chunk boundary. `copy_on_import` is additionally the
  recommended mode for untrusted JS.
- **Global-lock I/O stalls.** The registry `Mutex` guards only the slot
  map and is never held across I/O: `live_snapshot`/`read_at` clone the
  handle via `try_clone` under a short lock, then run metadata reads and
  positional reads on the clone after the lock is dropped (Unix uses
  lock-free positional reads; other platforms use an independent cursor
  on the per-call clone).
- **Metadata disclosure.** Error messages are fixed generics
  (`map_core_error`); snapshots never contain locations or handle
  values; `display_name` is host-chosen verbatim; tests assert no
  separators, drive letters, content, or temp markers in JS-visible
  errors.
- **Lock / permission failures.** `map_io` maps OS contention to
  `FileLocked`, denial to `PermissionDenied`, disappearance to
  `NotFound`; JS observes only `NotReadableError`/`SecurityError`/
  `NotFoundError` with no OS string.
- **Shutdown race.** `ShutdownFlag` (closed bit + shared cancellation +
  tracked closers) is observed by host-handle entry points, promise
  settlement, FileReader pump/dispatch jobs, stream pumps, and every fs
  `read_range`. Shutdown runs every tracked registry's `close_all`, so
  OS handles are dropped **immediately** (proven by `live_slot_count`
  assertions, not by registry destruction). Late completions settle
  nothing; no callback runs after context destruction; no locations leak
  into queues or errors.

## Non-goals (M7+)

Full Workers runtime and the WPT harness are separate extension points
and are not implemented here.

## M8 — telemetry secrecy (optional `tracing`, default off)

- **Allow-list only.** Events carry exactly `operation` (nine fixed
  names), `size` (`u64`), `duration_ms` (`u64`), `chunk_count` (`u64`),
  `result_class` (`ok`, `cancelled`, `quota`, `not_found`, `permission`,
  `snapshot_changed`, `invalid_range`, `encoding`, `shutdown`, `error`),
  and `environment_hash` (`u64`). No `event` name field exists; the target
  `boa_fapi::file_api.operation` is the only event identity.
- **Never logged.** Bytes, bodies, `display_name`, absolute/canonical
  paths, OS handles, snapshot identities, full blob URLs, UUIDs, origins,
  partitions, nonces, error messages, and arbitrary `Debug` output never
  enter telemetry. `environment_hash` is an opaque safe-Rust hash of the
  existing internal key; sources are never logged separately.
- **No behavior change.** Tracing never alters JS surface, job ordering,
  error mapping, or lifetimes; stale/shutdown completions emit nothing.

## M6 — Blob URL isolation and clone payload secrecy

- **URL guessing/enumeration.** UUIDs are 128-bit OS CSPRNG
  (`getrandom`; counters/timestamps/PRNGs forbidden by contract, no
  fallback). Malformed, unknown, revoked and foreign-partition URLs
  share one externally observable class (identical display, no token,
  UUID, origin internals, existence bit or host metadata); `revoke` is
  a silent no-op for foreign/malformed input, so neither resolve nor
  revoke is an oracle. Collision retries with fresh entropy and never
  overwrites.
- **Cross-partition read.** The store key is origin *and* opaque
  partition *and* per-global nonce (opaque origins share the
  `blob:null/` prefix but never a key). Same origin alone never
  authorizes: `resolve` checks the full key before handing out the
  `Arc<BlobData>`. ServiceWorker contexts cannot create URLs at all.
- **URL content leakage.** URLs carry only `<serialized-origin>/<uuid>`;
  partition keys, capabilities, handles and paths never serialize.
  `ResolvedBlob` exposes only the shared bytes, media type and checked
  length. Logs/tests carry no full URLs, UUIDs or decoded bodies.
- **Clone exfiltration.** Payloads carry materialized bytes + public
  metadata only; host paths, capabilities, OS handles and snapshot
  identities never encode (checked by construction and by the
  filesystem-safety test, which also asserts no location in errors).
  Decode enforces version + checked bounds before allocation; future
  versions are rejected, never misread.
- **Shutdown race (M6).** `store.clear()` runs as a tracked closer at
  shutdown (strong refs released immediately, idempotent); URL creation
  and clone entry points reject after shutdown; already-handed-out
  `Arc` reads still complete safely without touching a destroyed
  context; no late jobs or callbacks.
