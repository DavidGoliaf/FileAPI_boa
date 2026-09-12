//! Dependency and quality guards for the M2 bindings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

// ──────────────────────────────────────────────
// Core isolation: boa_fapi_core stays Boa-free
// ──────────────────────────────────────────────

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("readable file")
}

fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                result.extend(walk_rs(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                result.push(path);
            }
        }
    }
    result
}

#[test]
fn core_cargo_toml_has_no_boa_dependencies() {
    let manifest = read(&workspace_root().join("crates/boa_fapi_core/Cargo.toml"));
    for forbidden in ["boa_engine", "boa_gc", "boa_runtime", "boa_macros"] {
        assert!(
            !manifest.contains(forbidden),
            "boa_fapi_core must not depend on {forbidden}"
        );
    }
}

#[test]
fn core_source_has_no_boa_types() {
    let src = workspace_root().join("crates/boa_fapi_core/src");
    for path in walk_rs(&src) {
        let content = read(&path);
        for forbidden in [
            "boa_engine",
            "boa_gc",
            "JsValue",
            "JsObject",
            "JsString",
            "JsError",
            "Context",
        ] {
            assert!(
                !content.contains(forbidden),
                "{} must not reference {forbidden}",
                path.display()
            );
        }
    }
}

#[test]
fn only_boa_fapi_depends_on_boa() {
    let core = read(&workspace_root().join("crates/boa_fapi_core/Cargo.toml"));
    assert!(
        !core.contains("boa_engine"),
        "boa_fapi_core must not depend on boa_engine"
    );
    let facade = read(&workspace_root().join("crates/boa_fapi/Cargo.toml"));
    assert!(
        facade.contains("boa_engine"),
        "boa_fapi must depend on boa_engine"
    );
}

// ──────────────────────────────────────────────
// Production scan: no unwrap/expect/panic macros
// ──────────────────────────────────────────────

/// Strips `#[cfg(test)]` module bodies from source text so that test-only
/// `unwrap`/`expect` usage is not flagged. Inner `#[test]` attributes do not
/// reset the brace depth, so helper functions between tests stay excluded. A
/// file beginning with `#![cfg(test)]` is test code in its entirety.
fn strip_test_modules(source: &str) -> String {
    let mut result = String::new();
    let mut in_test_module = false;
    let mut whole_file = false;
    let mut brace_depth = 0i32;

    for line in source.lines() {
        if whole_file {
            continue;
        }
        if in_test_module {
            brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
            brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
            if brace_depth <= 0 && line.contains('}') {
                in_test_module = false;
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("#![cfg(test)]") {
            whole_file = true;
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            in_test_module = true;
            brace_depth = 0;
        }
        result.push_str(line);
        result.push('\n');
    }

    result
}

#[test]
fn production_source_no_unwrap_expect_panic() {
    let src = workspace_root().join("crates/boa_fapi/src");
    for path in walk_rs(&src) {
        let content = read(&path);
        let production = strip_test_modules(&content);
        for (index, line) in production.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            for forbidden in ["unwrap(", "expect(", "panic!(", "todo!(", "unimplemented!("] {
                assert!(
                    !trimmed.contains(forbidden),
                    "production code in {}:{} must not use {forbidden}",
                    path.display(),
                    index + 1
                );
            }
        }
    }
}

#[test]
fn strip_test_modules_removes_test_code_with_helpers() {
    let source = "fn a() {\n    let _ = 1;\n}\n\
                  #[cfg(test)]\n\
                  mod tests {\n\
                  \x20   fn helper() {\n\
                  \x20       x.unwrap();\n\
                  \x20   }\n\
                  \x20   #[test]\n\
                  \x20   fn t() {\n\
                  \x20       helper();\n\
                  \x20   }\n\
                  }\n\
                  fn b() {\n    let _ = 2;\n}\n";
    let stripped = strip_test_modules(source);
    assert!(stripped.contains("fn a()"));
    assert!(stripped.contains("fn b()"));
    assert!(!stripped.contains("unwrap("));
    assert!(!stripped.contains("mod tests"));
}

#[test]
fn strip_test_modules_handles_whole_file_test_modules() {
    let source = "#![cfg(test)]\nfn helper() {\n    x.unwrap();\n}\n";
    let stripped = strip_test_modules(source);
    assert!(!stripped.contains("unwrap("));
}

// ──────────────────────────────────────────────
// Public API surface guards
// ──────────────────────────────────────────────

#[test]
fn internal_binding_modules_expose_no_public_items() {
    let src = workspace_root().join("crates/boa_fapi/src");
    for module in [
        "brand.rs",
        "blob.rs",
        "file.rs",
        "file_list.rs",
        "promise_read.rs",
        "streams.rs",
        "dom.rs",
        "filereader.rs",
        "filereader_sync.rs",
        "lifecycle.rs",
        "package.rs",
        "url_shim.rs",
        "clone_bridge.rs",
        "webidl.rs",
    ] {
        let content = read(&src.join(module));
        for line in content.lines() {
            let trimmed = line.trim();
            let starts_public = trimmed == "pub" || trimmed.starts_with("pub ");
            let starts_crate_public =
                trimmed.starts_with("pub(crate) ") || trimmed.starts_with("pub(super) ");
            // The reference fake bridge is the one deliberate exception:
            // it is re-exportable host API, documented in its module docs.
            if module == "clone_bridge.rs" && trimmed.starts_with("pub struct FakeCloneBridge") {
                continue;
            }
            if module == "clone_bridge.rs" && trimmed.starts_with("pub fn ") {
                continue;
            }
            // M9-D-R2 documents its GC/drop test-only arbitration through
            // `#[doc(hidden)] __test_*` helpers: they are explicitly not
            // part of the host surface (never re-exported from `lib.rs`),
            // run the production finalizer arbitration (not a cleanup
            // bypass), and are asserted absent from `lib.rs` below.
            if module == "streams.rs" && trimmed.starts_with("pub fn __test_") {
                continue;
            }
            assert!(
                !starts_public || starts_crate_public,
                "{module} must not expose public items: {trimmed}"
            );
        }
    }
}

#[test]
fn lib_rs_denies_unsafe_and_limits_re_exports() {
    let lib = read(&workspace_root().join("crates/boa_fapi/src/lib.rs"));
    assert!(lib.contains("#![deny(unsafe_code)]"));
    for exposed in [
        "pub use clock::{Clock, SystemClock};",
        "pub use error::RegisterError;",
        "pub use extension::{",
        "CloneAdapter",
        "CloneBridgeDescriptor",
        "FileApiEnvironment",
        "FileApiExtension",
        "FileApiExtensionBuilder",
        "FileApiHandle",
        "HostFileOptions",
        "OsEntropy",
        "UrlEntropySource",
        "pub use io::{",
        "FileApiContextId",
        "FileIoExecutor",
        "FileIoWake",
        "PollIoError",
        "ThreadedFileIoExecutor",
    ] {
        assert!(lib.contains(exposed), "lib.rs must contain `{exposed}`");
    }
}

#[test]
fn streams_shim_surface_is_bounded() {
    // M3-B registers exactly the ordered surface: two globals, five
    // stream/reader methods, one accessor, two tags. No pipe/tee/iterator,
    // BYOB, controller, strategy, transform/writable, decoder, reader,
    // event, or DOM API may appear in the shim module. (FileReader and the
    // DOM shim live in `dom.rs`/`filereader.rs`, not here.)
    let streams = read(&workspace_root().join("crates/boa_fapi/src/streams.rs"));
    for required in [
        "\"ReadableStream\"",
        "\"ReadableStreamDefaultReader\"",
        "\"getReader\"",
        "\"cancel\"",
        "\"locked\"",
        "\"read\"",
        "\"releaseLock\"",
    ] {
        assert!(
            streams.contains(required),
            "streams.rs must contain {required}"
        );
    }
    for forbidden in [
        "pipeTo",
        "pipeThrough",
        "tee",
        "AsyncIterator",
        "BYOB",
        "Controller",
        "Strategy",
        "TransformStream",
        "WritableStream",
        "TextDecoder",
        "ReadableStreamBYOBReader",
    ] {
        let mut hits = 0;
        for line in streams.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "streams.rs must not contain {forbidden}");
    }
    // The M4-A DOM mapping is referenced by streams errors through the
    // qualified `crate::dom::` path only; no literal FileReader/Event/DOM
    // surface may appear here.
    for forbidden in ["\"FileReader\"", "\"EventTarget\"", "\"DOMException\""] {
        let mut hits = 0;
        for line in streams.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "streams.rs must not contain {forbidden}");
    }
}

#[test]
fn filereader_and_dom_surface_is_bounded() {
    // M4-A registers exactly: `EventTarget`, `Event`, `ProgressEvent`,
    // `DOMException`, `FileReader` globals; EventTarget's 3 methods;
    // Event's 7 attributes + 2 methods; ProgressEvent's 3 attributes;
    // DOMException's `name`/`message`; FileReader's 5 methods, 3 readonly
    // attributes, 6 handlers, 3 constants. No FileReaderSync, workers,
    // URL/clone/WPT, full DOM (tree dispatch, capture/bubble, CustomEvent,
    // AbortSignal) or full Streams surface may appear in `dom.rs` or
    // `filereader.rs`. M5 lifecycle shutdown is the only exception: the
    // `shutdown`/`ShutdownFlag` checks in `filereader.rs` are
    // allowed and asserted by the dedicated M5 shutdown tests. M6 shutdown
    // is configuration-wide (no longer `fs`-gated), so the same exception
    // covers the ungated checks.
    for module in ["dom.rs", "filereader.rs"] {
        let content = read(&workspace_root().join(format!("crates/boa_fapi/src/{module}")));
        // Every production mention of an excluded API must be absent; only
        // negative test/docs comments in the sibling integration suite may
        // name them.
        for forbidden in [
            "FileReaderSync",
            "DedicatedWorker",
            "SharedWorker",
            "createObjectURL",
            "revokeObjectURL",
            "structuredClone",
            "CustomEvent",
            "AbortSignal",
            "capturePhase",
            "std::fs",
            "std::path",
            "BlobData::materialize",
        ] {
            let mut hits = 0;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if module == "filereader.rs"
                    && (trimmed.contains("shutdown") || trimmed.contains("ShutdownFlag"))
                {
                    continue;
                }
                if trimmed.contains(forbidden) {
                    hits += 1;
                }
            }
            assert_eq!(hits, 0, "{module} must not contain {forbidden}");
        }
    }
    let dom = read(&workspace_root().join("crates/boa_fapi/src/dom.rs"));
    for required in [
        "\"EventTarget\"",
        "\"Event\"",
        "\"ProgressEvent\"",
        "\"DOMException\"",
        "\"addEventListener\"",
        "\"removeEventListener\"",
        "\"dispatchEvent\"",
    ] {
        assert!(dom.contains(required), "dom.rs must contain {required}");
    }
    let filereader = read(&workspace_root().join("crates/boa_fapi/src/filereader.rs"));
    for required in [
        "\"FileReader\"",
        "\"readAsArrayBuffer\"",
        "\"readAsBinaryString\"",
        "\"readAsText\"",
        "\"readAsDataURL\"",
        "\"abort\"",
        "\"readyState\"",
        "\"result\"",
        "\"error\"",
        "\"onloadstart\"",
        "\"onloadend\"",
    ] {
        assert!(
            filereader.contains(required),
            "filereader.rs must contain {required}"
        );
    }
}

#[test]
fn promise_read_has_no_sync_filesystem_fallback() {
    // M9-B: `promise_read.rs` (Boa thread) must never call the blocking
    // primitives itself. `BlobData::materialize`, `ByteSource::read_range`
    // and `BlobReader::read_next` may appear only in comments/docs and in
    // the `#[cfg(test)]` module (controlled unit sources); the worker entry
    // lives in `io.rs` (`FileIoTask::execute`). Behaviourally this is
    // proven by `m9_promise_io::blocking_source_never_runs_inside_boa_job`
    // with a gated blocking source; this guard keeps the call path absent
    // by construction.
    let content = read(&workspace_root().join("crates/boa_fapi/src/promise_read.rs"));
    let stripped = strip_test_modules(&content);
    for forbidden in [".materialize(", "read_range(", "read_next(", ".execute("] {
        let mut hits = 0;
        for line in stripped.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "promise_read.rs must not contain {forbidden}");
    }
    // The worker entry point itself is asserted present in `io.rs`, so the
    // guard above cannot go vacuous (a rename would fail loudly here).
    let io = read(&workspace_root().join("crates/boa_fapi/src/io.rs"));
    for required in [
        "fn run_materialize_guarded",
        ".materialize(&self.limits",
        "fn execute(",
    ] {
        assert!(io.contains(required), "io.rs must contain `{required}`");
    }
    // Docs still describe the intended threading contract.
    let promise_docs = read(&workspace_root().join("crates/boa_fapi/src/promise_read.rs"));
    assert!(
        promise_docs.contains("worker"),
        "promise_read.rs docs must describe the worker path"
    );
}

#[test]
fn stream_has_no_sync_read_fallback() {
    // M9-D: `streams.rs` (Boa thread) must never call the blocking
    // primitives itself. `BlobData::materialize`, `ByteSource::read_range`
    // and `BlobReader::read_next` may appear only in comments/docs and in
    // the `#[cfg(test)]` module (controlled unit sources); the worker entry
    // lives in `io.rs` (`StreamChunkTask::execute` → `read_blob_range`).
    // Behaviourally this is proven by
    // `m9_stream_io::blocking_source_never_runs_inside_boa_job` with a
    // gated blocking source; this guard keeps the call path absent by
    // construction.
    let content = read(&workspace_root().join("crates/boa_fapi/src/streams.rs"));
    let stripped = strip_test_modules(&content);
    for forbidden in [".materialize(", "read_range(", "read_next(", ".execute("] {
        let mut hits = 0;
        for line in stripped.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "streams.rs must not contain {forbidden}");
    }
    // The worker entry point itself is asserted present in `io.rs`, so the
    // guard above cannot go vacuous (a rename would fail loudly here).
    let io = read(&workspace_root().join("crates/boa_fapi/src/io.rs"));
    for required in [
        "struct StreamChunkTask",
        "struct StreamChunkCompletion",
        "fn submit_stream",
        "fn push_stream_completion",
        "fn take_stream_completions",
        "fn stream_task_for",
        "fn submit_stream_guarded",
        "read_blob_range(&self.data",
        "StreamChunkKind::Chunk",
    ] {
        assert!(io.contains(required), "io.rs must contain `{required}`");
    }
    // `poll_io` must drain stream completions through the Boa-thread
    // settlement entry; docs still describe the worker contract.
    let extension = read(&workspace_root().join("crates/boa_fapi/src/extension.rs"));
    assert!(
        extension.contains("settle_stream_completion"),
        "extension.rs poll_io must settle stream completions"
    );
    let stream_docs = read(&workspace_root().join("crates/boa_fapi/src/streams.rs"));
    assert!(
        stream_docs.contains("worker"),
        "streams.rs docs must describe the worker path"
    );
}

#[test]
fn sync_surface_is_bounded() {
    // M4-B registers exactly: the `FileReaderSync` global (worker
    // environments only), four prototype methods with `length = 1`, and
    // the `FileReaderSync` tag. No async state (`readyState`, `result`,
    // `error`), no `abort`, no `on*` handlers, no Promise/EventTarget/job
    // machinery, and no filesystem/URL/clone/full-DOM surface may appear
    // in `filereader_sync.rs`. Shared packaging lives in `package.rs`,
    // which carries no registration or scheduling code.
    let sync = read(&workspace_root().join("crates/boa_fapi/src/filereader_sync.rs"));
    for required in [
        "\"FileReaderSync\"",
        "\"readAsArrayBuffer\"",
        "\"readAsBinaryString\"",
        "\"readAsText\"",
        "\"readAsDataURL\"",
    ] {
        assert!(
            sync.contains(required),
            "filereader_sync.rs must contain {required}"
        );
    }
    for forbidden in [
        "\"readyState\"",
        "\"result\"",
        "\"error\"",
        "\"abort\"",
        "\"onload\"",
        "Promise",
        "EventTarget",
        "addEventListener",
        "enqueue_job",
        "run_jobs",
        "GenericJob",
        "PromiseJob",
        "max_concurrent_reads",
        "FileReaderSyncSync",
        "DedicatedWorker(",
        "std::fs",
        "std::path",
        "createObjectURL",
        "structuredClone",
        "CustomEvent",
        "AbortSignal",
        "tokio",
        "std::thread",
    ] {
        let mut hits = 0;
        for line in sync.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "filereader_sync.rs must not contain {forbidden}");
    }
    let package = read(&workspace_root().join("crates/boa_fapi/src/package.rs"));
    for required in [
        "resolve_text_encoding",
        "mime_charset",
        "decode_text",
        "package_binary_string",
        "package_data_url",
        "data_url_len",
    ] {
        assert!(
            package.contains(required),
            "package.rs must contain {required}"
        );
    }
    for forbidden in [
        "FileReaderSync",
        "FileReader",
        "enqueue_job",
        "run_jobs",
        "Context",
        "JsObject",
        "JsValue",
    ] {
        let mut hits = 0;
        for line in package.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "package.rs must not contain {forbidden}");
    }
}

#[test]
fn no_out_of_scope_surface() {
    // M5 filesystem surface plus the M6 URL/clone surface live only in
    // their own modules and the capability wiring. `FileReaderSync` and
    // the worker descriptors live only in `filereader_sync.rs` (surface)
    // and `extension.rs` (capability wiring); URL/clone names live only
    // in `url_shim.rs`, `clone_bridge.rs`, `extension.rs` and `lib.rs`.
    // Every other module must not mention them.
    let src = workspace_root().join("crates/boa_fapi/src");
    for entry in walk_rs(&src) {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        // The sync/URL/clone surfaces, the capability wiring, and the
        // public re-exports legitimately name the descriptors.
        if name == "filereader_sync.rs"
            || name == "url_shim.rs"
            || name == "clone_bridge.rs"
            || name == "extension.rs"
            || name == "lib.rs"
        {
            continue;
        }
        let content = read(&entry);
        for forbidden in [
            "\"FileReaderSync\"",
            "FileReaderSync",
            "FileReaderSyncNative",
            "DedicatedWorker",
            "SharedWorker",
            "ServiceWorker",
            "FileApiEnvironment",
        ] {
            let mut hits = 0;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains(forbidden) {
                    hits += 1;
                }
            }
            assert_eq!(
                hits,
                0,
                "{} must not introduce {forbidden}",
                entry.display()
            );
        }
    }
    // M6 owns exactly the bounded URL/clone surface: `createObjectURL`,
    // `revokeObjectURL` (shim only) and the host bridge names. Everything
    // else out-of-scope stays absent everywhere, including the new
    // modules. (`boa_fapi_fs` owns filesystem I/O behind the opaque
    // capability; `boa_fapi` production code holds no `std::fs`/`std::path`
    // itself — the `fs` feature only wires the opaque `FileResource`
    // trait. `structuredClone` as a JS global never exists: the bridge is
    // host-side only. M9-B/M9-C own the single allowed threading site: the
    // `io.rs` executor/worker plus its documented compatibility yield may
    // use `std::thread`; every other module must not. `filereader.rs`
    // production code holds no threading primitive at all: its
    // `#[cfg(test)]` drain helper spins the non-blocking `poll_io` loop
    // without sleep/yield (the scanner strips test modules, so the
    // production assertion below stays exact).)
    let mut all = String::new();
    for entry in walk_rs(&src) {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name == "io.rs" {
            continue;
        }
        all.push_str(&read(&entry));
        all.push('\n');
    }
    for forbidden in [
        "structuredClone",
        "CustomEvent",
        "AbortSignal",
        "std::fs",
        "std::path",
        "tokio",
        "std::thread",
        "MediaSource",
        "boa-idb",
        "boa_idb",
    ] {
        let mut hits = 0;
        for line in all.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "must not introduce {forbidden}");
    }
    // The bounded URL methods appear only where they belong.
    for module in ["url_shim.rs", "extension.rs"] {
        let content = read(&src.join(module));
        assert!(
            content.contains("createObjectURL") && content.contains("revokeObjectURL"),
            "{module} must own the URL methods"
        );
    }
    for entry in walk_rs(&src) {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name == "url_shim.rs" || name == "extension.rs" {
            continue;
        }
        let content = read(&entry);
        for forbidden in ["createObjectURL", "revokeObjectURL"] {
            let mut hits = 0;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains(forbidden) {
                    hits += 1;
                }
            }
            assert_eq!(
                hits,
                0,
                "{} must not introduce {forbidden}",
                entry.display()
            );
        }
    }
    // The `io.rs` threading site itself stays bounded: the I/O protocol
    // is the single allowed public surface outside `extension.rs`/`lib.rs`
    // (M9-B), plus only the worker pool, the join-handle list, and the
    // documented yield may name threading.
    let io = read(&src.join("io.rs"));
    for required in [
        "ThreadedFileIoExecutor",
        "FileIoExecutor",
        "FileIoWake",
        "poll_io",
        "pub struct FileApiContextId",
        "pub struct FileIoTask",
        "pub struct FileIoCompletion",
        "pub enum FileIoSubmitError",
        "pub enum PollIoError",
    ] {
        assert!(io.contains(required), "io.rs must contain {required}");
    }
    let mut io_hits = 0;
    for line in io.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains("tokio") {
            io_hits += 1;
        }
    }
    assert_eq!(io_hits, 0, "io.rs must not introduce tokio");
    let extension = read(&src.join("extension.rs"));
    assert!(
        extension.contains("environment"),
        "extension must register the environment capability"
    );
}

#[test]
fn public_api_exposes_no_paths_or_mutable_bytes() {
    // The public Rust API surface is exactly the extension/handle/clock/error
    // types; native data, segments and brand keys stay private. The `fs`
    // host import takes an opaque `Arc<dyn FileResource>` (never a
    // filesystem location): `file_from_resource` is the only allowed
    // production mention of the resource trait in `extension.rs`. M6 adds
    // the opaque URL/clone entry points (`create_blob_url`,
    // `resolve_blob_url`, `revoke_blob_url`, `clone_*`,
    // `blob_url_count`/`blob_urls_empty`) plus the
    // `UrlEntropySource`/`CloneAdapter` host traits — still no paths,
    // handles, partition keys or `BlobData` internals in the public types.
    // The live store and the environment key are never returned: no public
    // method may hand out the store or the key (`SharedUrlStore` itself is
    // `pub(crate)`-only; checked as a declaration, not a substring, so the
    // private alias definition does not trip the guard).
    let extension = read(&workspace_root().join("crates/boa_fapi/src/extension.rs"));
    assert!(!extension.contains("pub(crate) struct BlobNative"));
    assert!(!extension.contains("pub struct BlobNative"));
    for forbidden in [
        "PathBuf",
        "std::fs",
        "std::path",
        "boa-idb",
        "boa_idb",
        "pub fn url_store",
        "pub fn environment_key",
        "-> EnvironmentKey",
        "pub type SharedUrlStore",
    ] {
        let mut hits = 0;
        for line in extension.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "extension.rs must not contain {forbidden}");
    }
    // The store/key types themselves must never appear in a public
    // signature. `extension.rs` keeps them `pub(crate)`-internal: assert
    // no `pub fn` returns them and no `pub use` re-exports them.
    for line in extension.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("pub fn ") {
            assert!(
                !trimmed.contains("SharedUrlStore")
                    && !trimmed.contains("BlobUrlStore")
                    && !trimmed.contains("EnvironmentKey"),
                "no public method may return the store or key: {trimmed}"
            );
        }
    }
    let lib = read(&workspace_root().join("crates/boa_fapi/src/lib.rs"));
    for forbidden in [
        "BlobNative",
        "FileNative",
        "FileListNative",
        "PathBuf",
        "std::fs",
        "SharedUrlStore",
        "BlobUrlStore",
        "EnvironmentKey",
    ] {
        assert!(
            !lib.contains(forbidden),
            "lib.rs public surface must not expose {forbidden}"
        );
    }
    // The `pub(crate)` store alias itself must never become public.
    assert!(
        !extension.contains("pub type SharedUrlStore"),
        "SharedUrlStore alias must stay pub(crate)"
    );
    // `boa-idb` must never become a dependency: docs may name it as the
    // explicitly-absent integration, but no manifest and no `use` may.
    for source in ["extension.rs", "lib.rs", "clone_bridge.rs", "url_shim.rs"] {
        let content = read(&workspace_root().join(format!("crates/boa_fapi/src/{source}")));
        for forbidden in ["extern crate boa_idb", "use boa_idb", "boa_idb::"] {
            let mut hits = 0;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains(forbidden) {
                    hits += 1;
                }
            }
            assert_eq!(hits, 0, "{source} must not use {forbidden}");
        }
    }
    // No workspace manifest may depend on `boa-idb` in any feature
    // combination.
    for manifest in [
        "Cargo.toml",
        "crates/boa_fapi/Cargo.toml",
        "crates/boa_fapi_core/Cargo.toml",
        "crates/boa_fapi_fs/Cargo.toml",
    ] {
        let content = read(&workspace_root().join(manifest));
        assert!(
            !content.contains("boa-idb") && !content.contains("boa_idb"),
            "{manifest} must not depend on boa-idb"
        );
    }
    // The public URL/clone surface exists and stays opaque.
    for required in [
        "create_blob_url",
        "resolve_blob_url",
        "revoke_blob_url",
        "clone_blob",
        "blob_url_count",
        "blob_urls_empty",
        "UrlEntropySource",
        "CloneAdapter",
    ] {
        assert!(
            extension.contains(required),
            "extension.rs must contain `{required}`"
        );
    }
    // The M9-B I/O surface exists and stays opaque (no paths, handles or
    // JS values in the public types).
    for required in [
        "poll_io",
        "has_pending_io",
        "io_active_count",
        "stream_payload_count",
        "stream_operation_count",
        "io_executor",
        "io_wake",
        "FileIoExecutor",
        "FileIoWake",
        "PollIoError",
    ] {
        assert!(
            extension.contains(required),
            "extension.rs must contain `{required}`"
        );
    }
    // Advanced host constructors accept only immutable `BlobData`; they
    // expose no path, handle or mutable byte surface to JavaScript.
    for required in ["blob_from_data", "file_from_data"] {
        assert!(
            extension.contains(required),
            "extension.rs must contain `{required}`"
        );
    }
    let lib_rs = read(&workspace_root().join("crates/boa_fapi/src/lib.rs"));
    assert!(
        !lib_rs.contains("pub use blob::BlobNative"),
        "lib.rs must not expose BlobNative"
    );
    // M9-D-R2 `__test_*` helpers stay module-internal: they are reachable
    // from integration tests through the `#[doc(hidden)]` path on the
    // handle/module, never as re-exported host API.
    for forbidden in [
        "__test_live_stream",
        "__test_drop_live_stream",
        "__test_drop_",
    ] {
        assert!(
            !lib_rs.contains(forbidden),
            "lib.rs public surface must not expose {forbidden}"
        );
    }
}
