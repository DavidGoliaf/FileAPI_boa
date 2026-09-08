# `boa_fapi_core`

Boa-free pipeline role: platform-independent data model and algorithms for
the File API (M1). No Boa dependency by construction.

Contents: immutable `MemorySource`, segmented `BlobData` composition and
`slicing` without payload copies, `FileApiLimits` validation, typed
`FileApiError`, MIME normalization, line-ending conversion, cancellation,
opaque snapshot types, versioned Blob URL store and structured-clone
encoding.

Run:

```sh
cargo test --package boa_fapi_core --all-features
```

Wasm memory-only gate (no OS handles, no threads):

```sh
rustup target add wasm32-unknown-unknown
cargo check --target wasm32-unknown-unknown --package boa_fapi_core
```

Limits: engine-independent only; no JS bindings, no DOM, no streams, no
filesystem I/O, no network. Full WHATWG URL/Fetch/DOM/Streams/Workers and
File System Access API are out of scope.

See top-level `README.md` and `TZ_boa_fapi_FileAPI.md`.
