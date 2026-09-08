//! M8 feature-off guard: `boa_fapi` without `tracing` keeps M1–M7 behavior.
//!
//! Compiles and runs only when `tracing` is disabled. Complements the
//! collector tests in `m8_observability` (which require the feature).

#![cfg(not(feature = "tracing"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl boa_fapi::Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

#[test]
fn tracing_feature_off_has_no_trace_surface() {
    assert!(!cfg!(feature = "tracing"));
    // Manifest proves the dependency is optional and default-off.
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read manifest");
    assert!(
        manifest.contains("tracing = { workspace = true, optional = true }"),
        "tracing dependency must stay optional"
    );
    // M1–M7 behavior preserved without the feature.
    let mut context = Context::default();
    let handle = boa_fapi::FileApiExtension::builder()
        .clock(Arc::new(FixedClock {
            millis: 1_700_000_000_000,
        }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    let value: String = context
        .eval(Source::from_bytes(
            "globalThis.result = 'pending'; \
             new Blob(['abc']).text().then(v => { globalThis.result = v; }); \
             'registered';",
        ))
        .expect("eval")
        .to_string(&mut context)
        .expect("string")
        .to_std_string_escaped();
    assert_eq!(value, "registered");
    context.run_jobs().expect("run_jobs");
    let settled: String = context
        .eval(Source::from_bytes("globalThis.result"))
        .expect("read")
        .to_string(&mut context)
        .expect("string")
        .to_std_string_escaped();
    assert_eq!(settled, "abc");
    let _ = handle;
}
