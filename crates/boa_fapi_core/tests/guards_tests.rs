//! Dependency and quality guards for boa_fapi_core.

use proptest::prelude::*;

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
    for (path, content) in &entries {
        // Skip test modules
        if content.contains("#[cfg(test)]") && path.ends_with("lib.rs") {
            continue;
        }
        assert!(
            !content.contains("PathBuf"),
            "public API in {} must not use PathBuf",
            path
        );
        assert!(
            !content.contains("std::fs::"),
            "public API in {} must not use std::fs",
            path
        );
    }
}

// ──────────────────────────────────────────────
// Production source guard: no unwrap/expect/panic/todo/unimplemented
// ──────────────────────────────────────────────

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

fn strip_test_modules(source: &str) -> String {
    let mut result = String::new();
    let mut in_test_module = false;
    let mut brace_depth = 0i32;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[test]") {
            in_test_module = true;
            brace_depth = 0;
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
        result.push_str(line);
        result.push('\n');
    }

    result
}

// ──────────────────────────────────────────────
// Property test: BlobData slice matches reference
// ──────────────────────────────────────────────

use boa_fapi_core::blob::{BlobData, BlobSegment};
use boa_fapi_core::limits::FileApiLimits;
use boa_fapi_core::source::ByteSource;
use boa_fapi_core::source::memory::MemorySource;
use bytes::Bytes;
use std::sync::Arc;

fn reference_slice(data: &[u8], start: Option<i64>, end: Option<i64>) -> Vec<u8> {
    let len = data.len() as i64;
    let rel_start = match start {
        None => 0,
        Some(s) if s < 0 => (len + s).max(0) as u64,
        Some(s) => (s as u64).min(data.len() as u64),
    };
    let rel_end = match end {
        None => data.len() as u64,
        Some(e) if e < 0 => (len + e).max(0) as u64,
        Some(e) => (e as u64).min(data.len() as u64),
    };
    if rel_start >= rel_end {
        return Vec::new();
    }
    data[rel_start as usize..rel_end as usize].to_vec()
}

proptest! {
    #[test]
    fn slice_matches_reference(
        data in proptest::collection::vec(any::<u8>(), 0..256),
        start in any::<i64>(),
        end in any::<i64>(),
    ) {
        let limits = FileApiLimits::default();
        let src: Arc<dyn ByteSource> = Arc::new(MemorySource::new(Bytes::from(data.clone())));
        let seg = BlobSegment { source: src, offset: 0, len: data.len() as u64 };
        let blob = BlobData::from_segments(vec![seg], "", &limits).unwrap();

        let sliced = blob.slice(Some(start), Some(end), None, &limits).unwrap();
        let expected = reference_slice(&data, Some(start), Some(end));
        let actual = sliced.read_all().unwrap();

        prop_assert_eq!(actual, expected);
    }
}
