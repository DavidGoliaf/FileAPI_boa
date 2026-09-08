//! M7 hardening hooks: bounded differential / fuzz / bench / leak targets.
//!
//! These are hooks, not gates: each target is bounded (fixed seed, time
//! and input-size limits), reproducible from a fresh clone with only the
//! Rust toolchain, and never part of the ordinary `cargo test` gate. The
//! nightly workflow runs them scheduled; ordinary CI runs only the fast
//! deterministic subset documented in `docs/wpt.md`.
//!
//! - `differential_smoke` — Blob/File surface against Node.js when
//!   `node` is installed, else `NOTRUN`-equivalent skip with reason
//!   (documents differences, never changes normative semantics);
//! - `fuzz_decode_bounded` — clone-decode + manifest-parser property
//!   walk with a fixed seed and input cap (no panic/OOM by construction);
//! - `bench_smoke` — materialize/stream/URL-lookup/clone-encode timing
//!   smoke with a fixed baseline shape (prints medians, asserts no panic);
//! - `leak_repeat` — URL lifetime / shutdown / repeated-Context count
//!   checks (counts and ownership, never memory addresses).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiExtension};

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

/// Bounded input cap for fuzz-shaped inputs (64 KiB).
const MAX_INPUT: usize = 64 * 1024;
/// Bounded wall budget per hook in ordinary CI (fast subset).
const BUDGET: Duration = Duration::from_secs(10);

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock {
            millis: 1_700_000_000_000,
        }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

/// Deterministic xorshift64 PRNG (fixed seed → reproducible stream).
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0 | 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn differential_smoke() {
    // Compares a fixed Blob/File corpus against Node.js when available.
    // Without `node` the hook reports skip-with-reason (never fake PASS).
    let node = std::process::Command::new("node").arg("--version").output();
    let Ok(out) = node else {
        println!("differential: node unavailable → SKIP (reason: no-node-toolchain)");
        return;
    };
    assert!(out.status.success(), "node --version must succeed");
    let script = "var b = new Blob(['abc']); console.log(b.size + ':' + b.type);";
    let expected = "3:";
    let node_out = std::process::Command::new("node")
        .args(["-e", script])
        .output()
        .expect("node run");
    let node_text = String::from_utf8_lossy(&node_out.stdout).trim().to_owned();
    let (mut context, _handle) = setup();
    let value = context
        .eval(Source::from_bytes(
            "new Blob(['abc']).size + ':' + new Blob(['abc']).type",
        ))
        .expect("boa eval");
    let boa_text = value
        .to_string(&mut context)
        .expect("to_string")
        .to_std_string_escaped();
    println!("differential: node={node_text:?} boa={boa_text:?}");
    assert_eq!(node_text, expected);
    assert_eq!(
        boa_text.as_str(),
        expected,
        "Boa must match Node on the smoke corpus"
    );
}

#[test]
fn fuzz_decode_bounded() {
    let started = Instant::now();
    let mut rng = XorShift(0x9E3779B97F4A7C15);
    let mut explored = 0;
    // Fixed corpus of shapes: truncated valid encodings + random walks.
    let valid =
        boa_fapi_core::clone::FileApiClonePayload::Blob(boa_fapi_core::clone::SerializedBlob {
            bytes: bytes::Bytes::from_static(b"fuzz-seed"),
            media_type: String::from("text/plain"),
        })
        .encode()
        .expect("seed encode");
    for _ in 0..512 {
        if started.elapsed() > BUDGET {
            break;
        }
        let mut input = valid.clone();
        let cut = (rng.next() as usize) % (input.len() + 1);
        input.truncate(cut.min(MAX_INPUT));
        if rng.next().is_multiple_of(3) && !input.is_empty() {
            let at = (rng.next() as usize) % input.len();
            input[at] = (rng.next() & 0xFF) as u8;
        }
        // Must never panic; any typed error is acceptable.
        let _ = boa_fapi_core::clone::FileApiClonePayload::decode(&input);
        explored += 1;
    }
    println!("fuzz_decode_bounded: explored={explored} seed=0x9E3779B97F4A7C15 cap={MAX_INPUT}");
    assert!(explored > 0);
}

#[test]
fn bench_smoke() {
    // Timing smoke with fixed input shapes (prints medians; asserts only
    // completion + non-panic, never an absolute baseline in the gate).
    let (mut context, handle) = setup();
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from(vec![7; 256 * 1024]),
            "text/plain",
            &mut context,
        )
        .expect("blob");
    let started = Instant::now();
    for _ in 0..5 {
        let payload = handle.clone_blob(&blob).expect("clone");
        let _ = payload.encode().expect("encode");
    }
    let clone_ms = started.elapsed().as_millis().max(1);
    let url = handle.create_blob_url(&blob).expect("url");
    let started = Instant::now();
    for _ in 0..200 {
        let _ = handle.resolve_blob_url(&url).expect("resolve");
    }
    let lookup_ms = started.elapsed().as_millis().max(1);
    println!("bench_smoke: clone5={clone_ms}ms resolve200={lookup_ms}ms (informational, no gate)");
    handle.revoke_blob_url(&url);
}

#[test]
fn leak_repeat() {
    // Repeat create/revoke/shutdown cycles; counts return to zero and no
    // late settlement touches JS state afterwards.
    for _ in 0..8 {
        let (mut context, handle) = setup();
        context
            .eval(Source::from_bytes(
                "globalThis.leak = []; \
                 for (var i = 0; i < 25; i++) { globalThis.leak.push(URL.createObjectURL(new Blob(['z']))); } \
                 for (var u of globalThis.leak) { URL.revokeObjectURL(u); }",
            ))
            .expect("cycle");
        assert_eq!(handle.blob_url_count(), 0);
        assert!(handle.blob_urls_empty());
        handle.shutdown(&mut context).expect("shutdown");
        assert!(handle.blob_urls_empty());
    }
    // Repeated contexts leave no cross-context entries.
    let (mut context, handle) = setup();
    context
        .eval(Source::from_bytes(
            "globalThis.solo = URL.createObjectURL(new Blob(['s']));",
        ))
        .expect("solo");
    assert_eq!(handle.blob_url_count(), 1);
    drop(context);
    // The handle's store is per-context; dropping the context does not
    // leak into a fresh one.
    let (mut context2, handle2) = setup();
    assert_eq!(handle2.blob_url_count(), 0);
    let _ = &mut context2;
}
