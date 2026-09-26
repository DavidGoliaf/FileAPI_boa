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
    // M9-B promise reads complete off the Boa thread. Drive the documented
    // host loop rather than assuming a single Boa job turn waits for I/O.
    for _ in 0..200 {
        let settled_io = handle.poll_io(&mut context).expect("poll_io");
        context.run_jobs().expect("run_jobs");
        if settled_io == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("final run_jobs");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while handle.has_pending_io() && std::time::Instant::now() < deadline {
                let _ = handle.poll_io(&mut context).expect("poll_io");
                std::thread::yield_now();
            }
        }
    }
    let settled: String = context
        .eval(Source::from_bytes("globalThis.result"))
        .expect("read")
        .to_string(&mut context)
        .expect("string")
        .to_std_string_escaped();
    assert_eq!(settled, "abc");
}
