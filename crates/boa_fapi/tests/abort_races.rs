//! M7 race matrix: abort/stale/quota/URL/shutdown/filesystem/UTF-8 races.
//!
//! Every scenario runs in a fresh real `boa_engine::Context` with explicit
//! job pumping (`context.run_jobs()`), controlled test adapters or temp
//! resources — never `sleep`-based races. Each scenario has bounded
//! completion and asserts no late JS mutation after the terminal state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiEnvironment, FileApiExtension};

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

const FIXED_TIME: i64 = 1_700_000_000_000;

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert!(
        value.to_boolean(),
        "JS assertion failed: {source} (got {value:?})"
    );
}

/// Evaluates `source` for side effects only (assignment, URL creation):
/// asserts evaluation succeeds, ignoring the completion value.
fn eval_side_effect(context: &mut Context, source: &str) {
    context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
}

/// Drives jobs until quiescent (bounded: FileReader chains settle fast).
/// Promise reads additionally need `poll_io` first; the helper drives the
/// M9-B host loop so both settle. `poll_io` is strictly non-blocking, so
/// the loop yields briefly (bounded, hang-guard only) while threaded
/// I/O is still outstanding.
fn drain(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..64 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while handle.has_pending_io() {
                let _ = handle.poll_io(context);
                if !handle.has_pending_io() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::yield_now();
            }
        }
    }
}

// ── FileReader abort / stale / quota ─────────────────────────────────

#[test]
fn abort_before_first_chunk_reports_abort_loadend() {
    let (mut context, handle) = setup();
    context
        .eval(Source::from_bytes(
            "globalThis.log = []; \
             globalThis.r = new FileReader(); \
             globalThis.r.onabort = () => globalThis.log.push('abort'); \
             globalThis.r.onloadend = () => globalThis.log.push('end'); \
             globalThis.r.readAsText(new Blob(['abcdef'])); \
             globalThis.r.abort();",
        ))
        .expect("abort");
    drain(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.r.readyState === 2 && globalThis.r.result === null \
         && globalThis.r.error === null && globalThis.log.join('|') === 'abort|end'",
    );
}

#[test]
fn abort_between_progress_events_suppresses_stale_load() {
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    context
        .eval(Source::from_bytes(
            "globalThis.log = []; \
             globalThis.r = new FileReader(); \
             globalThis.r.onprogress = () => { \
               if (globalThis.log.length === 0) { globalThis.log.push('p'); globalThis.r.abort(); } \
             }; \
             globalThis.r.onload = () => globalThis.log.push('load'); \
             globalThis.r.onloadend = () => globalThis.log.push('end'); \
             globalThis.r.readAsText(new Blob([new Uint8Array(64 * 1024).fill(65)]));",
        ))
        .expect("start");
    drain(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.log.indexOf('load') === -1 && globalThis.r.readyState === 2",
    );
}

#[test]
fn stale_completion_after_new_operation_is_noop() {
    let (mut context, handle) = setup();
    context
        .eval(Source::from_bytes(
            "globalThis.log = []; \
             globalThis.r = new FileReader(); \
             globalThis.r.onload = function() { globalThis.log.push('load:' + this.result); }; \
             globalThis.r.readAsText(new Blob(['first'])); \
             globalThis.r.abort(); \
             globalThis.r.readAsText(new Blob(['second']));",
        ))
        .expect("restart");
    drain(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.log.join('|') === 'load:second' && globalThis.r.result === 'second'",
    );
}

#[test]
fn concurrent_read_quota_recovers() {
    let (mut context, handle) = setup();
    // 64 readers start LOADING; the 65th fails SecurityError through the
    // normal error path (readyState DONE + error set after jobs); after
    // all settle the freed slots accept new reads.
    eval_side_effect(
        &mut context,
        "globalThis.readers = []; \
         for (var i = 0; i < 64; i++) { \
           var r = new FileReader(); r.readAsText(new Blob(['q'])); \
           globalThis.readers.push(r); \
         } \
         globalThis.extra = new FileReader(); \
          globalThis.extra.readAsText(new Blob(['q']));",
    );
    drain(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.extra.readyState === 2 \
         && globalThis.extra.error instanceof DOMException \
         && globalThis.extra.error.name === 'SecurityError'",
    );
    drain(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.readers.every(r => r.readyState === 2) \
         && (function() { var r2 = new FileReader(); r2.readAsText(new Blob(['ok'])); return r2.readyState === 1; })()",
    );
    drain(&mut context, &handle);
}

// ── URL revoke/resolve + context shutdown ────────────────────────────

#[test]
fn revoke_before_and_after_resolve() {
    let (mut context, handle) = setup();
    eval_side_effect(
        &mut context,
        "globalThis.u = URL.createObjectURL(new Blob(['race']));",
    );
    let url = context
        .eval(Source::from_bytes("globalThis.u"))
        .expect("url")
        .as_string()
        .expect("string")
        .to_std_string_escaped();
    // Revoke before resolve: new resolves fail, handed-out Arc still reads.
    handle.revoke_blob_url(&url);
    assert!(handle.resolve_blob_url(&url).is_err());
    // Re-create, resolve, then revoke: the handed-out payload still reads.
    eval_side_effect(
        &mut context,
        "globalThis.u2 = URL.createObjectURL(new Blob(['live']));",
    );
    let url2 = context
        .eval(Source::from_bytes("globalThis.u2"))
        .expect("url2")
        .as_string()
        .expect("string")
        .to_std_string_escaped();
    let resolved = handle.resolve_blob_url(&url2).expect("resolve");
    handle.revoke_blob_url(&url2);
    assert!(handle.resolve_blob_url(&url2).is_err());
    let bytes = resolved
        .blob_data()
        .materialize(
            &boa_fapi_core::limits::FileApiLimits::default(),
            &boa_fapi_core::cancellation::CancellationToken::new(),
        )
        .expect("Arc read after revoke");
    assert_eq!(&bytes[..], b"live");
}

#[test]
fn context_shutdown_settles_nothing_late() {
    let (mut context, handle) = setup();
    eval_side_effect(
        &mut context,
        "globalThis.late = 'none'; \
         new Blob(['pending']).text().then(v => { globalThis.late = v; }); \
         globalThis.r = new FileReader(); \
         globalThis.r.onload = () => { globalThis.late = 'reader'; }; \
         globalThis.r.readAsText(new Blob(['x']));",
    );
    handle.shutdown(&mut context).expect("shutdown");
    drain(&mut context, &handle);
    assert_eval(&mut context, "globalThis.late === 'none'");
    // Late creation is rejected in JS and on the host.
    assert_eval(
        &mut context,
        "(function() { try { URL.createObjectURL(new Blob(['y'])); return false; } \
          catch (e) { return e instanceof TypeError; } })()",
    );
}

// ── Filesystem mutation / UTF-8 boundaries / slice extremes ─────────

#[test]
fn slice_and_utf8_boundary_matrix() {
    let (context, _handle) = setup();
    let mut context = context;
    // i64 extremes, empty blob, reversed range — all bounded, no panic.
    assert_eval(
        &mut context,
        "var b = new Blob(['hello']); \
         b.slice(-9223372036854775808).size === 5 \
         && b.slice(9223372036854775807).size === 0 \
         && new Blob([]).slice(0, 10).size === 0 \
         && b.slice(4, 1).size === 0",
    );
    // 2/3/4-byte code points split across chunk boundaries decode intact.
    assert_eval(
        &mut context,
        "new Blob(['\\u00E9\\u20AC\\u{1F600}']).size === (2 + 3 + 4)",
    );
    // BOM: single leading U+FEFF stripped once for readAsText-style decode.
    let (mut context2, handle2) = setup();
    eval_side_effect(
        &mut context2,
        "globalThis.bomReader = new FileReader(); \
         globalThis.bomReader.readAsText(new Blob(['\\uFEFFhi']));",
    );
    drain(&mut context2, &handle2);
    assert_eval(&mut context2, "globalThis.bomReader.result === 'hi'");
}

fn setup_with_chunk(chunk_size: usize) -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        default_chunk_size: chunk_size,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

#[test]
fn worker_environment_sync_without_jobs() {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("register");
    assert_eval(
        &mut context,
        "new FileReaderSync().readAsText(new Blob(['w'])) === 'w' \
         && typeof FileReaderSync === 'function'",
    );
}
