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
        "pub use extension::{FileApiExtension, FileApiExtensionBuilder, FileApiHandle, HostFileOptions};",
    ] {
        assert!(lib.contains(exposed), "lib.rs must contain `{exposed}`");
    }
}

#[test]
fn public_api_exposes_no_paths_or_mutable_bytes() {
    // The public Rust API surface is exactly the extension/handle/clock/error
    // types; native data, segments and brand keys stay private.
    let extension = read(&workspace_root().join("crates/boa_fapi/src/extension.rs"));
    assert!(!extension.contains("pub(crate) struct BlobNative"));
    assert!(!extension.contains("pub struct BlobNative"));
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
