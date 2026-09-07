# Security model (M5 filesystem)

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

## Non-goals (M6+)

Blob URL store lifetime, structured-clone lifetime, full Workers
runtime, and the WPT harness are separate extension points and are not
implemented here.
