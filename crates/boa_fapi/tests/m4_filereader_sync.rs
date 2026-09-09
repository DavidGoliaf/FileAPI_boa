//! M4-B integration tests: worker-only synchronous `FileReaderSync`.
//!
//! Every test uses a fresh real `boa_engine::Context` with an explicit
//! environment descriptor, executes real JavaScript, and asserts
//! synchronous JS-visible results. Sync methods return on the calling
//! stack: no test needs `context.run_jobs()` for a sync result, and the
//! no-jobs test proves delivery never depends on the job queue.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiEnvironment, FileApiExtension};

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

/// Creates a clean context with the extension registered for `env`.
fn setup_with_env(env: FileApiEnvironment) -> Context {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(env)
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Creates a worker context with `max_sync_read_bytes` overridden.
///
/// The sync ceiling shrinks while the materialize/blob ceilings keep their
/// defaults, so the whole-config `validate()` (sync <= materialize <=
/// blob) still passes.
fn setup_worker_with_sync_limit(max_sync_read_bytes: u64) -> Context {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_sync_read_bytes,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Creates a worker context with `max_data_url_output` overridden.
fn setup_worker_with_data_url_limit(max_data_url_output: u64) -> Context {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_data_url_output,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .environment(FileApiEnvironment::DedicatedWorker)
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

/// Evaluates `source` expecting a thrown same-realm `DOMException` with
/// the given name (also proving `Error` inheritance).
fn assert_throws_dom(context: &mut Context, source: &str, name: &str) {
    let probe = format!(
        "(function () {{ try {{ {source}; }} catch (e) {{ \
           return (e instanceof DOMException) + ':' + e.name + ':' + (e instanceof Error); }} \
           return 'no-throw'; }})()"
    );
    let observed: String = context
        .eval(Source::from_bytes(&probe))
        .unwrap_or_else(|error| panic!("probe eval failed for {source}: {error}"))
        .as_string()
        .expect("verdict string")
        .to_std_string_escaped();
    assert_eq!(observed, format!("true:{name}:true"));
}

// ──────────────────────────────────────────────
// 1. Capability matrix
// ──────────────────────────────────────────────

#[test]
fn window_has_no_sync_global() {
    let mut context = setup_with_env(FileApiEnvironment::Window);
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'undefined' \
         && typeof FileReader === 'function' \
         && typeof DOMException === 'function'",
    );
}

#[test]
fn service_worker_has_no_sync_global() {
    let mut context = setup_with_env(FileApiEnvironment::ServiceWorker);
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'undefined' \
         && typeof FileReader === 'function' \
         && typeof DOMException === 'function'",
    );
}

#[test]
fn dedicated_worker_installs_sync_global() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'function' \
         && (new FileReaderSync() instanceof FileReaderSync)",
    );
}

#[test]
fn shared_worker_installs_sync_global() {
    let mut context = setup_with_env(FileApiEnvironment::SharedWorker);
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'function' \
         && (new FileReaderSync() instanceof FileReaderSync)",
    );
}

#[test]
fn default_environment_is_window_without_sync() {
    // No explicit descriptor: the default stays `Window`, so no new global
    // appears for existing M4-A users.
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'undefined' \
         && typeof FileReader === 'function'",
    );
}

#[test]
fn sync_name_conflict_fails_atomically() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("globalThis.FileReaderSync = 42;"))
        .expect("setup eval");
    let extension = FileApiExtension::builder()
        .environment(FileApiEnvironment::DedicatedWorker)
        .build();
    match extension.register(&mut context) {
        Err(boa_fapi::RegisterError::NameConflict(name)) => {
            assert_eq!(name, "FileReaderSync");
        }
        Err(other) => panic!("expected NameConflict(FileReaderSync), got {other:?}"),
        Ok(_) => panic!("expected NameConflict(FileReaderSync), got Ok"),
    }
    assert_eval(
        &mut context,
        "globalThis.FileReaderSync === 42 && typeof Blob === 'undefined' \
         && typeof FileReader === 'undefined'",
    );
}

// ──────────────────────────────────────────────
// 2. Surface, descriptors, brand checks
// ──────────────────────────────────────────────

#[test]
fn window_leaves_foreign_sync_name_untouched() {
    // Without the worker capability the name is not preflighted: a
    // host-owned `FileReaderSync` global survives registration, which
    // succeeds normally.
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("globalThis.FileReaderSync = 42;"))
        .expect("setup eval");
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::Window)
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        "globalThis.FileReaderSync === 42 && typeof Blob === 'function' \
         && typeof FileReader === 'function'",
    );
}

#[test]
fn sync_surface_descriptors_and_tags() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        FileReaderSync.name === 'FileReaderSync' && FileReaderSync.length === 0
        && ['readAsArrayBuffer', 'readAsBinaryString', 'readAsText', 'readAsDataURL'].every(name => {
            var desc = Object.getOwnPropertyDescriptor(FileReaderSync.prototype, name);
            return desc && typeof desc.value === 'function'
                && desc.value.name === name && desc.value.length === 1
                && desc.writable === true && desc.enumerable === false
                && desc.configurable === true;
        })
        && Object.prototype.toString.call(new FileReaderSync()) === '[object FileReaderSync]'
        ",
    );
}

#[test]
fn sync_prototype_has_only_four_methods() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        (() => {
            var names = Object.getOwnPropertyNames(FileReaderSync.prototype);
            var methods = names.filter(n => n !== 'constructor');
            if (methods.length !== 4) return false;
            for (var name of ['readAsArrayBuffer', 'readAsBinaryString', 'readAsText', 'readAsDataURL']) {
                if (methods.indexOf(name) === -1) return false;
            }
            // No async-only or event surface: no state, no abort, no
            // handlers, no Promise API, and not an EventTarget.
            var sync = new FileReaderSync();
            return sync.readyState === undefined && sync.result === undefined
                && sync.error === undefined && sync.abort === undefined
                && sync.onload === undefined && sync.then === undefined
                && !(sync instanceof EventTarget);
        })()
        ",
    );
}

#[test]
fn sync_construction_and_receiver_brand_checks() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    // Direct call without `new` throws `TypeError`.
    let result = context.eval(Source::from_bytes("FileReaderSync()"));
    assert!(
        format!("{}", result.expect_err("expected TypeError")).contains("TypeError"),
        "FileReaderSync() without new must throw TypeError"
    );
    // Borrowed methods and forged receivers throw synchronously.
    for source in [
        "FileReaderSync.prototype.readAsArrayBuffer.call({}, new Blob(['x']))",
        "FileReaderSync.prototype.readAsText.call(new FileReaderSync(), null)",
        "Object.create(FileReaderSync.prototype).readAsDataURL(new Blob(['x']))",
        "FileReaderSync.prototype.readAsBinaryString.call(new FileReader(), new Blob(['x']))",
    ] {
        let result = context.eval(Source::from_bytes(&format!(
            "(() => {{ 'use strict'; return (() => {{ {source} }})(); }})()"
        )));
        let error = result.expect_err("expected a TypeError");
        assert!(
            format!("{error}").contains("TypeError"),
            "expected TypeError for {source}, got: {error}"
        );
    }
    // Missing/non-Blob arguments throw `TypeError`.
    for source in [
        "new FileReaderSync().readAsArrayBuffer()",
        "new FileReaderSync().readAsText(123)",
        "new FileReaderSync().readAsDataURL({})",
    ] {
        let result = context.eval(Source::from_bytes(&format!(
            "(() => {{ 'use strict'; return (() => {{ {source} }})(); }})()"
        )));
        let error = result.expect_err("expected a TypeError");
        assert!(
            format!("{error}").contains("TypeError"),
            "expected TypeError for {source}, got: {error}"
        );
    }
}

// ──────────────────────────────────────────────
// 3. Four methods: empty, NUL/high-byte, composed/sliced, File, freshness
// ──────────────────────────────────────────────

#[test]
fn sync_array_buffer_is_exact_and_fresh() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        (() => {
            var empty = new FileReaderSync().readAsArrayBuffer(new Blob());
            if (!(empty instanceof ArrayBuffer) || empty.byteLength !== 0) return false;
            var ab = new FileReaderSync().readAsArrayBuffer(new Blob(['abc']));
            if (!(ab instanceof ArrayBuffer) || ab.byteLength !== 3) return false;
            var view = new Uint8Array(ab);
            if (view[0] !== 97 || view[1] !== 98 || view[2] !== 99) return false;
            // Embedded NULs and high bytes survive exactly.
            var raw = new FileReaderSync().readAsArrayBuffer(new Blob([new Uint8Array([0, 255, 65, 0])]));
            var rv = new Uint8Array(raw);
            if (rv.length !== 4 || rv[0] !== 0 || rv[1] !== 255 || rv[2] !== 65 || rv[3] !== 0) return false;
            // Composed, sliced, and File inputs read in order.
            var composed = new FileReaderSync().readAsArrayBuffer(
                new Blob([new Blob(['he']), 'llo', new Uint8Array([33])]));
            if (String.fromCharCode.apply(null, new Uint8Array(composed)) !== 'hello!') return false;
            var sliced = new FileReaderSync().readAsArrayBuffer(new Blob(['hello world']).slice(6, 11));
            if (String.fromCharCode.apply(null, new Uint8Array(sliced)) !== 'world') return false;
            var fromFile = new FileReaderSync().readAsArrayBuffer(new File(['FB'], 'f.txt'));
            if (String.fromCharCode.apply(null, new Uint8Array(fromFile)) !== 'FB') return false;
            return true;
        })()
        ",
    );
    // Fresh backing: mutating one result never affects another read.
    assert_eval(
        &mut context,
        r"
        (() => {
            var blob = new Blob(['abc']);
            var first = new FileReaderSync().readAsArrayBuffer(blob);
            new Uint8Array(first)[0] = 0;
            var second = new FileReaderSync().readAsArrayBuffer(blob);
            return new Uint8Array(second)[0] === 97
                && new FileReaderSync().readAsText(blob) === 'abc';
        })()
        ",
    );
}

#[test]
fn sync_binary_string_preserves_nuls_and_high_bytes() {
    let mut context = setup_with_env(FileApiEnvironment::SharedWorker);
    assert_eval(
        &mut context,
        r"
        (() => {
            var value = new FileReaderSync().readAsBinaryString(new Blob([new Uint8Array([0, 65, 255, 128, 0])]));
            if (typeof value !== 'string' || value.length !== 5) return false;
            if (value.charCodeAt(0) !== 0 || value.charCodeAt(1) !== 65) return false;
            if (value.charCodeAt(2) !== 255 || value.charCodeAt(3) !== 128) return false;
            if (value.charCodeAt(4) !== 0) return false;
            return new FileReaderSync().readAsBinaryString(new Blob()) === '';
        })()
        ",
    );
}

// ──────────────────────────────────────────────
// 4. Text: BOM, malformed, multibyte, labels, unknown label fallback
// ──────────────────────────────────────────────

#[test]
fn sync_text_matches_async_representations() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        (() => {
            var sync = new FileReaderSync();
            // UTF-8 default, multibyte, BOM removal, replacement.
            if (sync.readAsText(new Blob(['héllo😀'])) !== 'héllo😀') return false;
            if (sync.readAsText(new Blob([new Uint8Array([0xEF, 0xBB, 0xBF, 0x41])]), 'utf-8') !== 'A') return false;
            if (sync.readAsText(new Blob([new Uint8Array([0x41, 0xFF, 0x42])])) !== 'A\uFFFD B'.replace(' ', '')) return false;
            if (sync.readAsText(new Blob([new Uint8Array([0xC3])])) !== '\uFFFD') return false;
            if (sync.readAsText(new Blob()) !== '') return false;
            // Supported non-UTF-8 label through the Encoding Standard.
            if (sync.readAsText(new Blob([new Uint8Array([0xE9])]), 'windows-1252') !== 'é') return false;
            // Empty label defaults to UTF-8.
            if (sync.readAsText(new Blob(['ab']), '') !== 'ab') return false;
            // File inherits the Blob path.
            if (sync.readAsText(new File(['f'], 'f.txt')) !== 'f') return false;
            // Unknown explicit label falls through to MIME/UTF-8, never
            // `EncodingError`: MIME charset wins when present, UTF-8
            // decoding otherwise.
            if (sync.readAsText(
                    new Blob([new Uint8Array([0xE9])], { type: 'text/plain;charset=windows-1252' }),
                    'not-an-encoding') !== 'é') return false;
            if (sync.readAsText(new Blob(['abc']), 'not-an-encoding') !== 'abc') return false;
            if (sync.readAsText(
                    new Blob([new Uint8Array([0xC3, 0xA9])], { type: 'text/plain;charset=bogus-charset' }),
                    'not-an-encoding') !== 'é') return false;
            return true;
        })()
        ",
    );
}

#[test]
fn throwing_label_is_converted_after_brand_and_argument_checks() {
    // A throwing encoding object must never be observed when the receiver
    // or the Blob argument itself is illegal: brand/argument `TypeError`s
    // come first. With a valid receiver/blob the conversion throw itself
    // propagates (not a brand failure; no `EncodingError` exists anymore).
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        "(() => { \
             globalThis.evil = {}; \
             Object.defineProperty(globalThis.evil, 'toString', \
                 { get() { throw new Error('label-boom'); } }); \
             return true; \
         })()",
    );
    assert_eval(
        &mut context,
        "(() => { \
             try { new FileReaderSync().readAsText(new Blob(['x']), globalThis.evil); \
                 return false; } \
             catch (e) { \
                 return (e instanceof Error) && !(e instanceof DOMException) \
                     && e.message === 'label-boom'; } \
         })()",
    );
    assert_eval(
        &mut context,
        "(() => { \
             try { FileReaderSync.prototype.readAsText.call({}, new Blob(['x']), globalThis.evil); \
                 return false; } \
             catch (e) { return e instanceof TypeError; } \
         })()",
    );
    assert_eval(
        &mut context,
        "(() => { \
             try { new FileReaderSync().readAsText(null, globalThis.evil); \
                 return false; } \
             catch (e) { return e instanceof TypeError; } \
         })()",
    );
}

// ──────────────────────────────────────────────
// 5. Data URL: exact packaging and boundary
// ──────────────────────────────────────────────

#[test]
fn sync_data_url_exact_packaging() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        (() => {
            var sync = new FileReaderSync();
            if (sync.readAsDataURL(new Blob(['hello'], { type: 'text/plain' })) !== 'data:text/plain;base64,aGVsbG8=') return false;
            if (sync.readAsDataURL(new Blob(['hello'])) !== 'data:;base64,aGVsbG8=') return false;
            if (sync.readAsDataURL(new Blob([new Uint8Array([])], { type: 'text/plain' })) !== 'data:text/plain;base64,') return false;
            if (sync.readAsDataURL(new Blob([new Uint8Array([0, 255, 16])])) !== 'data:;base64,AP8Q') return false;
            var url = sync.readAsDataURL(new Blob(['x']));
            if (url.indexOf(' ') !== -1 || url.indexOf('\n') !== -1) return false;
            return true;
        })()
        ",
    );
}

#[test]
fn sync_data_url_quota_boundary() {
    // `== max_data_url_output` succeeds, `+1` fails with
    // `QuotaExceededError` before any source read or allocation.
    let prefix = "data:;base64,";
    let limit = (prefix.len() + 4) as u64;
    let mut context = setup_worker_with_data_url_limit(limit);
    assert_eval(
        &mut context,
        "new FileReaderSync().readAsDataURL(new Blob([new Uint8Array([1, 2, 3])])) \
         === 'data:;base64,AQID'",
    );
    assert_throws_dom(
        &mut context,
        "new FileReaderSync().readAsDataURL(new Blob([new Uint8Array([1, 2, 3, 4])]))",
        "QuotaExceededError",
    );
}

// ──────────────────────────────────────────────
// 6. Sync-size boundary
// ──────────────────────────────────────────────

#[test]
fn sync_size_limit_boundary() {
    // `size == max_sync_read_bytes` succeeds for every method;
    // `size == max_sync_read_bytes + 1` throws `QuotaExceededError`.
    let mut context = setup_worker_with_sync_limit(64);
    assert_eval(
        &mut context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var exact = new Blob([new Uint8Array(64)]);
            if (sync.readAsArrayBuffer(exact).byteLength !== 64) return false;
            if (sync.readAsBinaryString(exact).length !== 64) return false;
            if (sync.readAsText(exact).length !== 64) return false;
            if (sync.readAsDataURL(exact).indexOf('data:;base64,') !== 0) return false;
            return true;
        })()
        ",
    );
    for method in [
        "readAsArrayBuffer",
        "readAsBinaryString",
        "readAsText",
        "readAsDataURL",
    ] {
        assert_throws_dom(
            &mut context,
            &format!("new FileReaderSync().{method}(new Blob([new Uint8Array(65)]))"),
            "QuotaExceededError",
        );
    }
}

// ──────────────────────────────────────────────
// 8. No Promise / events / jobs
// ──────────────────────────────────────────────

#[test]
fn sync_methods_return_without_jobs_or_events() {
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    // The value is returned on the calling stack: correct before any
    // `run_jobs()` call, and no job can be pending afterwards.
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.seen = [];
            var sync = new FileReaderSync();
            // FileReaderSync is not an event target: handler properties
            // do not exist and listener registration throws.
            if (sync.onload !== undefined) return false;
            try {
                EventTarget.prototype.addEventListener.call(sync, 'load', () => {});
                return false;
            } catch (e) {
                if (!(e instanceof TypeError)) return false;
            }
            var text = sync.readAsText(new Blob(['sync-now']));
            if (text !== 'sync-now') return false;
            var ab = sync.readAsArrayBuffer(new Blob(['ab']));
            if (!(ab instanceof ArrayBuffer) || ab.byteLength !== 2) return false;
            return true;
        })()
        ",
    );
    // Draining the job queue changes nothing and reports no error: no
    // Promise, no FileReader event, and no File Reading task was queued.
    context.run_jobs().expect("run_jobs failed");
    assert_eval(
        &mut context,
        "typeof FileReaderSync === 'function' \
         && new FileReaderSync().readAsText(new Blob(['again'])) === 'again'",
    );
}

// ──────────────────────────────────────────────
// 9. Async quota untouched by sync reads
// ──────────────────────────────────────────────

#[test]
fn sync_reads_do_not_consume_async_quota() {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.readers = [];
            for (var i = 0; i < 64; i++) {
                var reader = new FileReader();
                reader.readAsText(new Blob(['x']));
                globalThis.readers.push(reader);
            }
            // All 64 async slots are LOADING; sync reads still succeed
            // without consuming a slot.
            var sync = new FileReaderSync();
            if (sync.readAsText(new Blob(['sync'])) !== 'sync') return false;
            if (sync.readAsArrayBuffer(new Blob(['ab'])).byteLength !== 2) return false;
            // The 65th async read still fails: the quota is intact.
            globalThis.extra = new FileReader();
            globalThis.extra.onerror = function () {};
            globalThis.extra.readAsText(new Blob(['x']));
            return globalThis.readers.length === 64;
        })()
        ",
    );
    // M9-C host loop: `poll_io` drains worker chunks into pump jobs.
    for _ in 0..200 {
        let settled = handle.poll_io(&mut context).unwrap_or(0);
        context.run_jobs().expect("run_jobs failed");
        if settled == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs failed");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            for _ in 0..50 {
                let _ = handle.poll_io(&mut context);
                context.run_jobs().expect("run_jobs failed");
                if !handle.has_pending_io() {
                    break;
                }
            }
        }
    }
    assert_eval(
        &mut context,
        r"
        globalThis.extra.readyState === 2
        && (globalThis.extra.error instanceof DOMException)
        && globalThis.extra.error.name === 'SecurityError'
        && globalThis.readers.every(r => r.readyState === 2 && r.result === 'x')
        ",
    );
}

// ──────────────────────────────────────────────
// 10. Powerset and negative guards
// ──────────────────────────────────────────────

#[test]
fn window_and_service_worker_never_expose_sync() {
    for env in [
        FileApiEnvironment::Window,
        FileApiEnvironment::ServiceWorker,
    ] {
        let mut context = setup_with_env(env);
        assert_eval(
            &mut context,
            "typeof FileReaderSync === 'undefined' \
             && typeof FileReader === 'function' \
             && typeof Worker === 'undefined'",
        );
    }
}

#[test]
fn no_filesystem_url_clone_or_full_dom_surface() {
    // M4-B froze before the M6 URL milestone: this asserts the absence of
    // the filesystem/full-DOM names. The M6 `URL` namespace (exactly the
    // two static methods) is the expected addition, pinned by M6 suites.
    let mut context = setup_with_env(FileApiEnvironment::DedicatedWorker);
    assert_eval(
        &mut context,
        r"
        typeof FileReaderSync === 'function'
        && typeof FileReaderSyncSync === 'undefined'
        && typeof CustomEvent === 'undefined'
        && typeof AbortSignal === 'undefined'
        && typeof FileReaderSync.prototype.readAsTextSync === 'undefined'
        && typeof FileReaderSync.prototype.abort === 'undefined'
        ",
    );
}
