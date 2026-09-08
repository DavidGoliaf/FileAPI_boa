# `boa_fapi_fs`

Pipeline role: capability-based filesystem-backed `ByteSource` with
snapshot validation (M5). Owns already-open read-only handles by opaque
id; JS never supplies a path.

Snapshot checks: every `read_range` validates cancellation, checked
arithmetic, and the live opaque snapshot (identity + size + mtime) against
the import snapshot before reading, verifies exact bytes afterwards, and
fails with `SnapshotChanged`/`NotFound`/`FileLocked`/`PermissionDenied`/
`InvalidRange` without partial bytes or location detail.

Platform rule: Unix uses the live-handle path with strong identity
(`dev`+`ino`); Windows and other non-Unix targets refuse live-handle
imports outright and require `copy_on_import` (point-in-time memory bytes)
or deny. No path appears in JS, errors, tracing, or artifacts.

Shutdown: `close`/`close_all` drop OS handles immediately; tracked closers
run once outside the lock at `FileApiHandle::shutdown`.

Run:

```sh
cargo test --package boa_fapi_fs --all-features -- --nocapture
```

Limits: read-only sources only; no directory enumeration, no raw-path
import, no write API.

See top-level `README.md` and `TZ_boa_fapi_FileAPI.md`.
