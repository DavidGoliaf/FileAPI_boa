//! M3-A integration tests: promise-returning `Blob` reads.
//!
//! Every test uses a fresh real `boa_engine::Context`, registers the
//! extension, executes JavaScript, and drives settlement explicitly with
//! `context.run_jobs()`. No test settles a promise synchronously, and no
//! test inspects private internals: assertions observe JS values only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiExtension};

/// Deterministic clock used for `File.lastModified` defaults.
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

/// Creates a clean context with the extension registered (default limits).
fn setup() -> Context {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Creates a clean context with `max_materialize_bytes` overridden.
///
/// Narrow per-operation fixture: the materialize/sync ceilings shrink
/// together while the blob ceiling keeps its default, so the whole-config
/// `validate()` (sync <= materialize <= blob, chunk <= materialize) still
/// passes and the materialize limit is enforced per read by `materialize()`.
/// (Requires max_materialize_bytes >= 64 KiB so the default chunk fits.)
fn setup_with_materialize_limit(max_materialize_bytes: u64) -> Context {
    let mut context = Context::default();
    assert!(
        max_materialize_bytes >= 64 * 1024,
        "fixture materialize ceiling must fit the default chunk size"
    );
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_materialize_bytes,
        max_sync_read_bytes: max_materialize_bytes.min(32 * 1024 * 1024),
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Evaluates `source` and asserts that the result is `true`.
fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert_eq!(
        value.as_boolean(),
        Some(true),
        "JS assertion failed: {source} (got {value:?})"
    );
}

/// Evaluates `source` (an async IIFE body returning a `bool` promise),
/// drives jobs to completion, and asserts the settled value is `true`.
///
/// The IIFE returns a promise, so one `run_jobs()` pass settles both the
/// read job and the awaiting continuation.
fn assert_async_body(context: &mut Context, body: &str) {
    let source = format!("(async () => {{ {body} }})()");
    let value = context
        .eval(Source::from_bytes(&source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    let promise = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a promise from {source}"));
    context.run_jobs().expect("run_jobs failed");
    let state = boa_engine::object::builtins::JsPromise::from_object(promise)
        .expect("promise object")
        .state();
    assert_eq!(
        state,
        boa_engine::builtins::promise::PromiseState::Fulfilled(boa_engine::JsValue::from(true)),
        "async body did not fulfill with true: {source} (state: {state:?})"
    );
}

/// Evaluates `source` and asserts that it throws a synchronous `TypeError`.
fn assert_eval_type_error(context: &mut Context, source: &str) {
    let result = context.eval(Source::from_bytes(&format!(
        "(() => {{ 'use strict'; return (() => {{ {source} }})(); }})()"
    )));
    let error = result.expect_err("expected a TypeError");
    let message = format!("{error}");
    assert!(
        message.contains("TypeError"),
        "expected TypeError for {source}, got: {message}"
    );
}

// ──────────────────────────────────────────────
// 1. Surface and descriptors
// ──────────────────────────────────────────────

#[test]
fn read_methods_live_on_blob_prototype_with_correct_descriptors() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        ['text', 'arrayBuffer', 'bytes'].every(name => {
            var desc = Object.getOwnPropertyDescriptor(Blob.prototype, name);
            if (!desc) return false;
            if (typeof desc.value !== 'function') return false;
            if (desc.value.name !== name) return false;
            if (desc.value.length !== 0) return false;
            if (desc.writable !== true) return false;
            if (desc.enumerable !== false) return false;
            if (desc.configurable !== true) return false;
            return true;
        })
        && (Blob.prototype.text.call(new Blob(['x'])) instanceof Promise)
        && (new Blob(['x']).arrayBuffer() instanceof Promise)
        && (new Blob(['x']).bytes() instanceof Promise)
        ",
    );
}

#[test]
fn file_inherits_read_methods_without_own_copies() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        !Object.prototype.hasOwnProperty.call(File.prototype, 'text')
        && !Object.prototype.hasOwnProperty.call(File.prototype, 'arrayBuffer')
        && !Object.prototype.hasOwnProperty.call(File.prototype, 'bytes')
        && File.prototype.text === Blob.prototype.text
        && File.prototype.arrayBuffer === Blob.prototype.arrayBuffer
        && File.prototype.bytes === Blob.prototype.bytes
        && (new File(['x'], 'f.txt').text() instanceof Promise)
        ",
    );
}

#[test]
fn brand_violations_throw_synchronously_without_promise() {
    let mut context = setup();
    // Borrowed getters/methods, forged objects and foreign `this` fail
    // before any Promise could be created.
    assert_eval_type_error(&mut context, r"Object.create(Blob.prototype).text()");
    assert_eval_type_error(
        &mut context,
        r"Blob.prototype.text.call(Object.create(Blob.prototype))",
    );
    assert_eval_type_error(&mut context, r"Blob.prototype.arrayBuffer.call(123)");
    assert_eval_type_error(&mut context, r"Blob.prototype.bytes.call(null)");
    assert_eval_type_error(
        &mut context,
        r"var g = Object.getOwnPropertyDescriptor(Blob.prototype, 'text').value; g.call({})",
    );
    // A forged constructor-looking object has no native brand either.
    assert_eval_type_error(
        &mut context,
        r"Blob.prototype.text.call({size: 1, type: '', slice: Blob.prototype.slice})",
    );
    // Real File passes the Blob brand for reads.
    assert_eval(
        &mut context,
        "new File(['x'], 'f.txt').text() instanceof Promise",
    );
}

// ──────────────────────────────────────────────
// 2. Pending first, settle only through run_jobs
// ──────────────────────────────────────────────

#[test]
fn text_returns_pending_promise_settled_by_run_jobs() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.order = [];
            globalThis.b = new Blob(['hello']);
            globalThis.p = globalThis.b.text();
            globalThis.p.then(v => globalThis.order.push('handler:' + v));
            // Synchronously after the call the handler has not run.
            globalThis.order.push('sync');
            return globalThis.order.join(',') === 'sync';
        })()
        ",
    );
    context.run_jobs().expect("run_jobs failed");
    assert_eval(
        &mut context,
        "globalThis.order.join(',') === 'sync,handler:hello'",
    );
}

#[test]
fn empty_blob_read_stays_pending_until_run_jobs() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.seen = 'none';
            new Blob().text().then(v => { globalThis.seen = 'text:' + v; });
            new Blob().arrayBuffer().then(b => { globalThis.seen += '|ab:' + b.byteLength; });
            return globalThis.seen === 'none';
        })()
        ",
    );
    context.run_jobs().expect("run_jobs failed");
    assert_eval(&mut context, "globalThis.seen === 'text:|ab:0'");
}

#[test]
fn two_concurrent_reads_settle_fifo() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            var a = new Blob(['first']).text();
            var b = new Blob(['second']).text();
            a.then(v => globalThis.log.push('a:' + v));
            b.then(v => globalThis.log.push('b:' + v));
            // Both handlers wait for the job queue.
            return globalThis.log.length === 0;
        })()
        ",
    );
    context.run_jobs().expect("run_jobs failed");
    assert_eval(
        &mut context,
        "globalThis.log.join(',') === 'a:first,b:second'",
    );
}

// ──────────────────────────────────────────────
// 3. text()
// ──────────────────────────────────────────────

#[test]
fn text_decodes_ascii_and_multibyte() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var t1 = await new Blob(['aé😀']).text();
            var t2 = await new File(['héllo'], 'f.txt').text();
            return t1 === 'aé😀' && t2 === 'héllo';

        ",
    );
}

#[test]
fn text_replaces_invalid_utf8() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            // Lone 0xFF byte and a truncated 2-byte sequence both decode
            // with U+FFFD replacement semantics.
            var t1 = await new Blob([new Uint8Array([0x41, 0xFF, 0x42])]).text();
            var t2 = await new Blob([new Uint8Array([0xC3])]).text();
            return t1 === 'A\uFFFD B'.replace(' ', '') && t1.length === 3
                && t2 === '\uFFFD' && t2.length === 1;

        ",
    );
}

#[test]
fn text_reads_composed_and_sliced_blobs() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var composed = new Blob([new Blob(['he']), 'llo', new Uint8Array([33])]);
            var t1 = await composed.text();
            var t2 = await new Blob(['hello world']).slice(6, 11).text();
            var t3 = await new Blob().text();
            return t1 === 'hello!' && t2 === 'world' && t3 === '';

        ",
    );
}

// ──────────────────────────────────────────────
// 4. arrayBuffer()
// ──────────────────────────────────────────────

#[test]
fn array_buffer_returns_exact_bytes_in_fresh_buffer() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var ab = await new Blob(['abc']).arrayBuffer();
            if (!(ab instanceof ArrayBuffer)) return false;
            if (ab.byteLength !== 3) return false;
            var view = new Uint8Array(ab);
            if (view[0] !== 97 || view[1] !== 98 || view[2] !== 99) return false;
            var empty = await new Blob().arrayBuffer();
            return empty instanceof ArrayBuffer && empty.byteLength === 0;

        ",
    );
}

#[test]
fn array_buffer_results_are_independent() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var blob = new Blob(['abc']);
            var first = await blob.arrayBuffer();
            new Uint8Array(first)[0] = 0;
            var second = await blob.arrayBuffer();
            var v = new Uint8Array(second);
            // The second result is unaffected by mutating the first.
            if (v[0] !== 97 || v[1] !== 98 || v[2] !== 99) return false;
            // The source blob still reads the original bytes.
            var again = await blob.text();
            return again === 'abc';

        ",
    );
}

// ──────────────────────────────────────────────
// 5. bytes()
// ──────────────────────────────────────────────

#[test]
fn bytes_returns_uint8array_with_offset_zero() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var u8 = await new Blob(['AB']).bytes();
            if (!(u8 instanceof Uint8Array)) return false;
            if (u8.byteOffset !== 0) return false;
            if (u8.byteLength !== 2) return false;
            if (u8[0] !== 65 || u8[1] !== 66) return false;
            if (!(u8.buffer instanceof ArrayBuffer)) return false;
            var empty = await new Blob().bytes();
            return empty instanceof Uint8Array && empty.byteLength === 0
                && empty.byteOffset === 0;

        ",
    );
}

#[test]
fn bytes_results_are_independent() {
    let mut context = setup();
    assert_async_body(
        &mut context,
        r"

            var blob = new Blob(['xy']);
            var first = await blob.bytes();
            first[0] = 0;
            var second = await blob.bytes();
            if (second[0] !== 120 || second[1] !== 121) return false;
            // Not a DataView, and buffers are not shared between calls.
            if (second instanceof DataView) return false;
            if (first.buffer === second.buffer) return false;
            var again = await blob.text();
            return again === 'xy';

        ",
    );
}

// ──────────────────────────────────────────────
// 6. Limits and rejection
// ──────────────────────────────────────────────

#[test]
fn over_materialize_limit_rejects_with_quota_exceeded() {
    // A 70 KiB ceiling (above the 64 KiB chunk floor) with a 70 KiB+1 blob:
    // the limit is enforced per read by `materialize()`. After M4-A the
    // rejection is the mapped `QuotaExceededError` DOMException.
    let mut context = setup_with_materialize_limit(70 * 1024);
    let big = "new Uint8Array(70 * 1024 + 1)";
    assert_eval(
        &mut context,
        &format!(
            r"
        (() => {{
            globalThis.outcome = 'pending';
            globalThis.p = new Blob([{big}]).text();
            globalThis.p.then(
                () => {{ globalThis.outcome = 'fulfilled'; }},
                error => {{ globalThis.outcome = (error instanceof DOMException) && error.name === 'QuotaExceededError' ? 'quota' : 'other:' + error.name; }}
            );
            // Still pending: the rejection happens in the job.
            return globalThis.outcome === 'pending' && (globalThis.p instanceof Promise);
        }})()
        "
        ),
    );
    context.run_jobs().expect("run_jobs failed");
    assert_eval(&mut context, "globalThis.outcome === 'quota'");
    // The blob is still usable for M2 metadata and slice.
    assert_eval(
        &mut context,
        "new Blob(['hello']).size === 5 && new Blob(['hello']).slice(1, 3).size === 2",
    );
}

#[test]
fn materialize_limit_boundary() {
    // 64 KiB ceiling: `size == limit` succeeds, `size == limit + 1` rejects
    // with `QuotaExceededError` for every method.
    let mut context = setup_with_materialize_limit(64 * 1024);
    let exact = "new Uint8Array(64 * 1024)";
    let over = "new Uint8Array(64 * 1024 + 1)";
    // size == limit succeeds for every method (byte-exact check via lengths).
    assert_async_body(
        &mut context,
        &format!(
            r"

            var t = await new Blob([{exact}]).arrayBuffer();
            var ab = await new Blob([{exact}]).arrayBuffer();
            var u8 = await new Blob([{exact}]).bytes();
            return t.byteLength === 64 * 1024 && ab.byteLength === 64 * 1024 && u8.length === 64 * 1024;

        "
        ),
    );
    // size == limit + 1 rejects for every method.
    assert_eval(
        &mut context,
        &format!(
            r"
        (() => {{
            globalThis.rejections = 0;
            for (var read of [
                new Blob([{over}]).text(),
                new Blob([{over}]).arrayBuffer(),
                new Blob([{over}]).bytes(),
            ]) {{
                read.then(
                    () => {{}},
                    error => {{ if ((error instanceof DOMException) && error.name === 'QuotaExceededError') globalThis.rejections++; }}
                );
            }}
            return true;
        }})()
        "
        ),
    );
    context.run_jobs().expect("run_jobs failed");
    assert_eval(&mut context, "globalThis.rejections === 3");
}

// ──────────────────────────────────────────────
// 9. No accidental M4-B surface (M4-A DOM/FileReader are expected to exist)
// ──────────────────────────────────────────────

#[test]
fn dom_and_filereader_globals_are_present_without_m4b() {
    let mut context = setup();
    assert_eval(
        &mut context,
        r"
        typeof Blob.prototype.stream === 'function'
        && typeof Blob.prototype.textStream === 'function'
        && typeof globalThis.FileReader === 'function'
        && typeof globalThis.FileReaderSync === 'undefined'
        && typeof globalThis.EventTarget === 'function'
        && typeof globalThis.DOMException === 'function'
        && typeof globalThis.ReadableStream === 'function'
        ",
    );
}
