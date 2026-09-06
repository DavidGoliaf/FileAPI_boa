# boa-fapi

A Rust implementation of the [File API](https://www.w3.org/TR/FileAPI/) for the [Boa](https://github.com/nickel-org/boa) JavaScript engine.

## Workspace structure

| Crate | Purpose |
|---|---|
| `boa_fapi_core` | Platform-independent data model and algorithms (no Boa dependency) |
| `boa_fapi` | Future Web IDL bindings and GC integration (M2+) |
| `boa_fapi_fs` | Future filesystem-backed sources (M5+) |
| `boa_fapi_wpt` | Future WPT test harness (M7+) |

## M1 scope

M1 delivers `boa_fapi_core`: an independent crate implementing immutable byte data sources, segmented blobs, slice semantics, MIME type normalization, and line ending conversion. No JavaScript bindings, filesystem access, or async runtime is involved at this stage.

## Building

```sh
cargo build --workspace --all-features
```

## Testing

```sh
cargo test --workspace --all-features
```
