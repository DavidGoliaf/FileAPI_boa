# Host integration (M5 filesystem)

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
// `request.resource` is the opaque id from `resource.id()`; no path input exists.
```

## Passing an explicit display name

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
    .file_from_resource(adapter, "report.txt", HostFileOptions::default(), &mut context)
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

Idempotent; rejects new host operations; cancels pending filesystem
work through the shared cancellation token; late Boa jobs settle
nothing after context destruction. The examples above never print a
location: display names in outputs are placeholders only.
