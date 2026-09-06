//! Dependency and quality guards for boa_fapi_core.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// ──────────────────────────────────────────────
// Dependency guard: verify Cargo.toml content
// ──────────────────────────────────────────────

#[test]
fn core_cargo_toml_no_forbidden_dependencies() {
    let cargo_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let cargo_toml = match std::fs::read_to_string(cargo_path) {
        Ok(s) => s,
        Err(e) => panic!("could not read Cargo.toml: {e}"),
    };

    // Check that the [dependencies] section doesn't contain forbidden crates.
    // We look for dependency lines (not the package name itself).
    let forbidden = [
        "boa_engine",
        "boa_gc",
        "boa_runtime",
        "tokio",
        "futures",
        "url =",
    ];
    for dep in &forbidden {
        assert!(
            !cargo_toml.contains(dep),
            "boa_fapi_core Cargo.toml must not contain {} dependency",
            dep
        );
    }
}

// ──────────────────────────────────────────────
// Public API guard: no Path, PathBuf, file descriptors, JsValue
// ──────────────────────────────────────────────

#[test]
fn public_api_no_path_types() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = walk_src_files(&src_dir);
    let forbidden = [
        "PathBuf",
        "std::path::Path",
        "std::fs::",
        "FileDesc",
        "RawFd",
        "RawHandle",
        "JsValue",
        "JsString",
        "JsObject",
    ];
    for (path, content) in &entries {
        for pat in &forbidden {
            assert!(
                !content.contains(pat),
                "source {} must not use {}",
                path,
                pat
            );
        }
    }
}

// ──────────────────────────────────────────────
// Public BlobData API guard: exactly the fixed M1 contract + M2
// no-copy composition primitives, no test probes or content reads
// ──────────────────────────────────────────────

#[test]
fn blob_data_public_api_is_fixed() {
    let blob_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/blob.rs");
    let content = match std::fs::read_to_string(&blob_rs) {
        Ok(s) => s,
        Err(e) => panic!("could not read blob.rs: {e}"),
    };
    // Preamble lists the allowed surface; the impl block must match it.
    let allowed = [
        "pub fn empty(",
        "pub fn from_segments(",
        "pub fn size(",
        "pub fn media_type(",
        "pub fn snapshot(",
        "pub fn segment_count(",
        "pub fn concat_shared(",
        "pub fn push_shared(",
        "pub fn slice(",
    ];
    let mut actual = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("pub fn ") {
            actual.push(trimmed.to_owned());
        }
    }
    assert_eq!(
        actual.len(),
        allowed.len(),
        "BlobData public API changed: {actual:?}"
    );
    for (line, expected) in actual.iter().zip(allowed.iter()) {
        assert!(
            line.starts_with(expected),
            "unexpected BlobData public method: {line} (expected {expected})"
        );
    }
    // No public test probe or content-read accessor may exist.
    for forbidden in [
        "pub fn segments(",
        "pub fn read_all(",
        "pub fn shares_sources_with(",
        "pub fn first_segment_shares_source_with(",
        "pub fn materialize(",
        "pub fn segment_source_ptr(",
    ] {
        assert!(
            !content.contains(forbidden),
            "blob.rs must not expose {forbidden}"
        );
    }
}

fn walk_src_files(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                result.extend(walk_src_files(&path));
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                result.push((path.display().to_string(), content));
            }
        }
    }
    result
}

#[test]
fn production_source_no_unwrap_expect_panic() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = walk_src_files(&src_dir);

    for (path, content) in &entries {
        // Strip test modules from consideration
        let non_test = strip_test_modules(content);
        let lines: Vec<&str> = non_test.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip comments
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            // Skip #[cfg(test)] modules and their contents
            assert!(
                !trimmed.contains("unwrap("),
                "production code in {}:{} must not use unwrap()",
                path,
                i + 1
            );
            assert!(
                !trimmed.contains("expect("),
                "production code in {}:{} must not use expect()",
                path,
                i + 1
            );
            assert!(
                !trimmed.contains("panic!("),
                "production code in {}:{} must not use panic!()",
                path,
                i + 1
            );
            assert!(
                !trimmed.contains("todo!("),
                "production code in {}:{} must not use todo!()",
                path,
                i + 1
            );
            assert!(
                !trimmed.contains("unimplemented!("),
                "production code in {}:{} must not use unimplemented!()",
                path,
                i + 1
            );
        }
    }
}

/// Removes `#[cfg(test)]` module bodies from source text.
///
/// Once a `#[cfg(test)]` attribute is seen, every following line is stripped
/// until the braces opened inside the test module are balanced again. Inner
/// `#[test]` attributes do not reset the depth, so helper functions between
/// test functions (which may use `unwrap`/`expect`) stay excluded.
fn strip_test_modules(source: &str) -> String {
    let mut result = String::new();
    let mut in_test_module = false;
    let mut brace_depth = 0i32;

    for line in source.lines() {
        if in_test_module {
            brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
            brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
            if brace_depth <= 0 && line.contains('}') {
                in_test_module = false;
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#![cfg(test)]") {
            in_test_module = true;
            brace_depth = 0;
        }
        result.push_str(line);
        result.push('\n');
    }

    result
}

// ──────────────────────────────────────────────
// Guard self-tests: the scanner must be simple and correct
// ──────────────────────────────────────────────

#[test]
fn strip_test_modules_keeps_non_test_code() {
    let source = "fn a() {\n    let _ = 1;\n}\n\nfn b() {\n    let _ = 2;\n}\n";
    assert_eq!(strip_test_modules(source), source);
}

#[test]
fn strip_test_modules_removes_cfg_test_module_with_helpers() {
    let source = "fn a() {\n    let _ = 1;\n}\n\
                  #[cfg(test)]\n\
                  #[allow(clippy::unwrap_used)]\n\
                  mod tests {\n\
                  \x20   fn helper() {\n\
                  \x20       something.unwrap();\n\
                  \x20   }\n\
                  \x20   #[test]\n\
                  \x20   fn t() {\n\
                  \x20       helper();\n\
                  \x20   }\n\
                  }\n\
                  fn b() {\n    let _ = 2;\n}\n";
    let stripped = strip_test_modules(source);
    assert!(
        stripped.contains("fn a()"),
        "non-test code must survive: {stripped}"
    );
    assert!(
        stripped.contains("fn b()"),
        "trailing non-test code must survive: {stripped}"
    );
    assert!(
        !stripped.contains("unwrap("),
        "test module must be stripped: {stripped}"
    );
    assert!(
        !stripped.contains("mod tests"),
        "test module must be stripped: {stripped}"
    );
}

#[test]
fn strip_test_modules_ignores_inner_test_attribute_without_reset() {
    // An inner `#[test]` must not reset the brace depth of the enclosing
    // cfg(test) module, otherwise code after a nested test would be treated
    // as production code.
    let source = "#[cfg(test)]\n\
                  mod tests {\n\
                  \x20   #[test]\n\
                  \x20   fn t() {\n\
                  \x20       let _ = x.unwrap();\n\
                  \x20   }\n\
                  \x20   fn helper() {\n\
                  \x20       let _ = y.expect(\"no\");\n\
                  \x20   }\n\
                  }\n\
                  fn keep() {\n    let _ = 1;\n}\n";
    let stripped = strip_test_modules(source);
    assert!(
        stripped.contains("fn keep()"),
        "trailing code must survive: {stripped}"
    );
    assert!(
        !stripped.contains("unwrap("),
        "test code must be stripped: {stripped}"
    );
    assert!(
        !stripped.contains("expect("),
        "test code must be stripped: {stripped}"
    );
}
