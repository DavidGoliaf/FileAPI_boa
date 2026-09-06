# boa-fapi

A Rust implementation of the [File API](https://www.w3.org/TR/FileAPI/) for the [Boa](https://github.com/boa-dev/boa) JavaScript engine.

## Workspace structure

| Crate | Purpose |
|---|---|
| `boa_fapi_core` | Platform-independent data model and algorithms (no Boa dependency) |
| `boa_fapi` | Boa bindings: `Blob`, `File`, `FileList` (M2); Web IDL conversions, brands, GC-safe native data |
| `boa_fapi_fs` | Future filesystem-backed sources (M5+) |
| `boa_fapi_wpt` | Future WPT test harness (M7+) |

## Current scope (M1–M2)

- **M1 (`boa_fapi_core`)**: immutable byte sources, segmented blobs, File API slice semantics, MIME type normalization, and line ending conversion.
- **M2 (`boa_fapi`)**: registration of `Blob`, `File` and host-created `FileList` into a real `boa_engine::Context` — constructors with correct `name`/`length`/descriptors, `Symbol.toStringTag`, non-forgeable internal brands, `File.prototype → Blob.prototype` inheritance, Web IDL conversions (`DOMString`, `USVString`, `[Clamp] long long`, `long long`, `unsigned long`), Blob parts (USVString, BufferSource snapshot copy, Blob/File zero-copy composition), injectable `Clock` for `File.lastModified`, and atomic registration with rollback.

Not yet implemented (M3–M7): `Blob.text`/`arrayBuffer`/`bytes`/`stream`, FileReader, DOMException/EventTarget, blob URLs, structured clone, filesystem-backed sources, WPT harness.

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
