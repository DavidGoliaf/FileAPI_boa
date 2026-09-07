# Security model (M5 filesystem)

## Threat model

- **Arbitrary path access from JS.** Impossible by construction: no JS
  API takes a location; `file_from_resource` takes only an opaque
  `Arc<dyn FileResource>`. The default policy denies every import.
- **Path traversal (`..`) / symlink / Windows junction / reparse-point
  escape / aliasing.** Verification happens on the open handle's opaque
  identity captured at registration (`dev`+`ino` on Unix,
  attributes+creation-time+size+mtime on Windows, documented fallback
  elsewhere). No `starts_with(root)` string check exists anywhere — there
  is no path string to compare.
- **Replacement race (TOCTOU).** The import snapshot is captured on the
  open handle; every `read_range` checks live == import before reading
  and confirms identity after reading. Chunked consumers issue one range
  per chunk, so validation runs before the first chunk and on every new
  chunk boundary. `copy_on_import` is the recommended mode for untrusted
  JS or platforms without stable identity.
- **Metadata disclosure.** Error messages are fixed generics
  (`map_core_error`); snapshots never contain locations or handle
  values; `display_name` is host-chosen verbatim; tests assert no
  separators, drive letters, content, or temp markers in JS-visible
  errors.
- **Lock / permission failures.** `map_io` maps OS contention to
  `FileLocked`, denial to `PermissionDenied`, disappearance to
  `NotFound`; JS observes only `NotReadableError`/`SecurityError`/
  `NotFoundError` with no OS string.
- **Shutdown race.** `ShutdownFlag` (closed bit + shared cancellation)
  is observed by host-handle entry points, promise settlement, FileReader
  pump/dispatch jobs, stream pumps, and every fs `read_range`. Late
  completions settle nothing; no callback runs after context
  destruction; handles are released without leaving locations in queues
  or errors.

## Non-goals (M6+)

Blob URL store lifetime, structured-clone lifetime, full Workers
runtime, and the WPT harness are separate extension points and are not
implemented here.
