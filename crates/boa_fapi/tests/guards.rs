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
        "webidl.rs",
    ] {
        let content = read(&src.join(module));
        for line in content.lines() {
            let trimmed = line.trim();
            let starts_public = trimmed == "pub" || trimmed.starts_with("pub ");
            let starts_crate_public =
                trimmed.starts_with("pub(crate) ") || trimmed.starts_with("pub(super) ");
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
        "FileApiEnvironment",
        "FileApiExtension",
        "FileApiExtensionBuilder",
        "FileApiHandle",
        "HostFileOptions",
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
    // `fs`-gated `shutdown`/`ShutdownFlag` checks in `filereader.rs` are
    // allowed and asserted by the dedicated M5 shutdown tests.
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
        "resolve_label",
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
    // M5+ APIs (filesystem, URL, clone, WPT, full DOM/workers runtime)
    // never appear in production sources. `FileReaderSync` and the worker
    // descriptors live only in `filereader_sync.rs` (surface) and
    // `extension.rs` (capability wiring); every other module must not
    // mention them.
    let src = workspace_root().join("crates/boa_fapi/src");
    for entry in walk_rs(&src) {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        // The sync surface, the capability wiring, and the public
        // re-exports legitimately name the descriptor.
        if name == "filereader_sync.rs" || name == "extension.rs" || name == "lib.rs" {
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
    // Out-of-scope APIs are absent everywhere, including the new modules.
    // (`boa_fapi_fs` owns filesystem I/O behind the opaque capability;
    // `boa_fapi` production code holds no `std::fs`/`std::path` itself —
    // the `fs` feature only wires the opaque `FileResource` trait.)
    let mut all = String::new();
    for entry in walk_rs(&src) {
        all.push_str(&read(&entry));
        all.push('\n');
    }
    for forbidden in [
        "createObjectURL",
        "revokeObjectURL",
        "structuredClone",
        "CustomEvent",
        "AbortSignal",
        "std::fs",
        "std::path",
        "tokio",
        "std::thread",
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
    // production mention of the resource trait in `extension.rs`.
    let extension = read(&workspace_root().join("crates/boa_fapi/src/extension.rs"));
    assert!(!extension.contains("pub(crate) struct BlobNative"));
    assert!(!extension.contains("pub struct BlobNative"));
    for forbidden in ["PathBuf", "std::fs", "std::path"] {
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
    let lib = read(&workspace_root().join("crates/boa_fapi/src/lib.rs"));
    for forbidden in [
        "BlobNative",
        "FileNative",
        "FileListNative",
        "PathBuf",
        "std::fs",
    ] {
        assert!(
            !lib.contains(forbidden),
            "lib.rs public surface must not expose {forbidden}"
        );
    }
}
