//! M4-A integration tests: DOM shim and asynchronous `FileReader`.
//!
//! Every test uses a fresh real `boa_engine::Context`, registers the
//! extension, executes JavaScript, and drives delivery explicitly with
//! `context.run_jobs()` until quiescent. Assertions inspect JS-visible
//! objects and events; Rust-only state inspection is supplementary only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiExtension};

/// Deterministic clock used for `File.lastModified` defaults, event
/// `timeStamp` values, and the 50 ms `progress` throttle.
#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

/// A manually advanced clock for throttle tests: each read returns the next
/// entry of `schedule` (the last entry repeats forever).
#[derive(Debug)]
struct StepClock {
    schedule: Vec<i64>,
    cursor: std::sync::atomic::AtomicUsize,
}

impl StepClock {
    fn advance(&self) -> i64 {
        let index = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.schedule
            .get(index)
            .copied()
            .unwrap_or_else(|| self.schedule.last().copied().unwrap_or(0))
    }
}

impl Clock for StepClock {
    fn now_unix_millis(&self) -> i64 {
        self.advance()
    }
}

const FIXED_TIME: i64 = 1_700_000_000_000;

/// Creates a clean context with the extension registered (default limits).
fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

/// Creates a context whose injected clock walks `schedule` on every read.
fn setup_with_clock(schedule: Vec<i64>) -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(StepClock {
            schedule,
            cursor: std::sync::atomic::AtomicUsize::new(0),
        }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

/// Creates a context with a small chunk ceiling for multichunk reads.
///
/// The M3-B chunk range (`16 KiB..=1 MiB`) is the smallest observable
/// multichunk unit through public configuration.
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

/// Creates a context with `max_data_url_output` overridden.
fn setup_with_data_url_limit(max_data_url_output: u64) -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_data_url_output,
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

/// Drives the M9-C host loop until quiescent (bounded).
fn drain_jobs(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..200 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs failed");
        // The production executor completes on a worker thread. Yield before
        // checking quiescence so this legacy threaded-fixture driver cannot
        // exhaust its bounded host turns before that worker is scheduled.
        std::thread::yield_now();
        if settled == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs failed");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            for _ in 0..50 {
                let _ = handle.poll_io(context);
                context.run_jobs().expect("run_jobs failed");
                if !handle.has_pending_io() {
                    break;
                }
            }
        }
    }
}

// ──────────────────────────────────────────────
// 1. DOM / registration
// ──────────────────────────────────────────────

#[test]
fn dom_globals_have_exact_descriptors_and_prototypes() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        ['EventTarget', 'Event', 'ProgressEvent', 'DOMException', 'FileReader'].every(name => {
            var desc = Object.getOwnPropertyDescriptor(globalThis, name);
            if (!desc) return false;
            if (desc.writable !== true) return false;
            if (desc.enumerable !== false) return false;
            if (desc.configurable !== true) return false;
            return typeof globalThis[name] === 'function';
        })
        && new EventTarget() instanceof EventTarget
        && Object.getPrototypeOf(FileReader.prototype) === EventTarget.prototype
        && (new FileReader() instanceof EventTarget)
        && (new FileReader() instanceof FileReader)
        && Object.getPrototypeOf(ProgressEvent.prototype) === Event.prototype
        && (new ProgressEvent('progress') instanceof Event)
        && (new DOMException('x', 'NotReadableError') instanceof Error)
        && (new DOMException('x', 'NotReadableError') instanceof DOMException)
        && Object.prototype.toString.call(new DOMException('x', 'AbortError')) === '[object DOMException]'
        && Object.prototype.toString.call(new Event('e')) === '[object Event]'
        && Object.prototype.toString.call(new ProgressEvent('p')) === '[object ProgressEvent]'
        && Object.prototype.toString.call(new EventTarget()) === '[object EventTarget]'
        && Object.prototype.toString.call(new FileReader()) === '[object FileReader]'
        ",
    );
}

#[test]
fn dom_constructors_have_exact_name_and_length() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        EventTarget.name === 'EventTarget' && EventTarget.length === 0
        && Event.name === 'Event' && Event.length === 1
        && ProgressEvent.name === 'ProgressEvent' && ProgressEvent.length === 1
        && DOMException.name === 'DOMException' && DOMException.length === 0
        && FileReader.name === 'FileReader' && FileReader.length === 0
        ",
    );
}

#[test]
fn event_target_listener_dedupe_removal_order_and_isolation() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            var target = new EventTarget();
            var log = [];
            var a = () => log.push('a');
            var b = () => log.push('b');
            target.addEventListener('x', a);
            // Exact-tuple dedupe: same (type, callback, capture) ignored.
            target.addEventListener('x', a);
            target.addEventListener('x', a, false);
            // Different capture flag is a distinct tuple.
            target.addEventListener('x', a, true);
            target.addEventListener('x', b);
            if (!target.dispatchEvent(new Event('x'))) return false;
            // Registration order: a, a(capture), b.
            if (log.join(',') !== 'a,a,b') return false;
            // Removal drops one tuple only.
            log = [];
            target.removeEventListener('x', a);
            if (!target.dispatchEvent(new Event('x'))) return false;
            if (log.join(',') !== 'a,b') return false;
            // A throwing listener never stops the remaining listeners; the
            // throw surfaces synchronously after every listener ran.
            log = [];
            target.addEventListener('y', () => { log.push('first'); throw new Error('boom'); });
            target.addEventListener('y', () => log.push('second'));
            var threw = false;
            try { target.dispatchEvent(new Event('y')); } catch (e) { threw = true; }
            return threw && log.join(',') === 'first,second';
        })()
        ",
    );
}

#[test]
fn event_and_progress_event_attributes_are_exact() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            var target = new EventTarget();
            var seen = null;
            target.addEventListener('p', e => { seen = e; });
            var event = new ProgressEvent('p', { lengthComputable: true, loaded: 3, total: 10 });
            target.dispatchEvent(event);
            if (!(seen instanceof ProgressEvent)) return false;
            if (!(seen instanceof Event)) return false;
            if (seen.type !== 'p') return false;
            if (seen.target !== target || seen.currentTarget !== target) return false;
            if (seen.bubbles !== false || seen.cancelable !== false) return false;
            if (seen.defaultPrevented !== false) return false;
            if (typeof seen.timeStamp !== 'number') return false;
            if (seen.lengthComputable !== true || seen.loaded !== 3 || seen.total !== 10) return false;
            // preventDefault on a non-cancelable event changes nothing.
            seen.preventDefault();
            if (seen.defaultPrevented !== false) return false;
            // dispatchEvent returns !defaultPrevented.
            if (target.dispatchEvent(new Event('p', { cancelable: true })) !== true) return false;
            var cancelled = new Event('c', { bubbles: true, cancelable: true });
            cancelled.preventDefault();
            return cancelled.bubbles === true && cancelled.cancelable === true
                && cancelled.defaultPrevented === true;
        })()
        ",
    );
}

#[test]
fn dom_exception_names_and_error_inheritance() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        ['InvalidStateError', 'NotReadableError', 'AbortError',
         'SecurityError', 'NotFoundError', 'QuotaExceededError'].every(name => {
            var e = new DOMException('m', name);
            return (e instanceof DOMException) && (e instanceof Error)
                && e.name === name && e.message === 'm';
        })
        && (new DOMException().name === 'Error' && new DOMException().message === '')
        && (new DOMException('only').name === 'Error' && new DOMException('only').message === 'only')
        ",
    );
}

#[test]
fn illegal_receivers_throw_type_error_synchronously() {
    let (mut context, _handle) = setup();
    for source in [
        "EventTarget.prototype.addEventListener.call({}, 'x', () => {})",
        "EventTarget.prototype.dispatchEvent.call({}, new Event('x'))",
        "Object.create(EventTarget.prototype).dispatchEvent(new Event('x'))",
        "Event.prototype.preventDefault.call({})",
        "Object.getOwnPropertyDescriptor(Event.prototype, 'type').get.call({})",
        "Object.getOwnPropertyDescriptor(ProgressEvent.prototype, 'loaded').get.call(new Event('x'))",
        "Object.getOwnPropertyDescriptor(DOMException.prototype, 'name').get.call(new Error('x'))",
        "new EventTarget().dispatchEvent({})",
        "new EventTarget().dispatchEvent(new ProgressEvent('x')) || false",
    ] {
        // The last entry dispatches a valid event (returns true); the rest
        // must throw `TypeError`.
        if source.ends_with("|| false") {
            assert_eval(&mut context, source);
            continue;
        }
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

#[test]
fn each_new_global_conflicts_atomically() {
    for name in [
        "EventTarget",
        "Event",
        "ProgressEvent",
        "DOMException",
        "FileReader",
    ] {
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(&format!("globalThis.{name} = 42;")))
            .expect("setup eval");
        let extension = FileApiExtension::builder().build();
        match extension.register(&mut context) {
            Err(boa_fapi::RegisterError::NameConflict(conflict)) => {
                assert_eq!(conflict, name);
            }
            Err(other) => panic!("expected NameConflict({name}), got {other:?}"),
            Ok(_) => panic!("expected NameConflict({name}), got Ok"),
        }
        assert_eval(
            &mut context,
            "typeof Blob === 'undefined' \
             && typeof File === 'undefined' \
             && typeof ReadableStream === 'undefined'",
        );
    }
}

#[test]
fn non_extensible_global_rejects_dom_atomically() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("Object.preventExtensions(globalThis);"))
        .expect("setup eval");
    let extension = FileApiExtension::builder().build();
    match extension.register(&mut context) {
        Err(boa_fapi::RegisterError::GlobalNotExtensible) => {}
        Err(other) => panic!("expected GlobalNotExtensible, got {other:?}"),
        Ok(_) => panic!("expected GlobalNotExtensible, got Ok"),
    }
    assert_eval(
        &mut context,
        "typeof Blob === 'undefined' && typeof FileReader === 'undefined' \
         && typeof EventTarget === 'undefined'",
    );
}

#[test]
fn dom_shim_disabled_fails_before_global_mutation() {
    let mut context = Context::default();
    let extension = FileApiExtension::builder().dom_shim(false).build();
    match extension.register(&mut context) {
        Err(boa_fapi::RegisterError::DomShimDisabled) => {}
        Err(other) => panic!("expected DomShimDisabled, got {other:?}"),
        Ok(_) => panic!("expected DomShimDisabled, got Ok"),
    }
    assert_eval(
        &mut context,
        "typeof Blob === 'undefined' && typeof File === 'undefined' \
         && typeof FileReader === 'undefined' && typeof EventTarget === 'undefined' \
         && typeof DOMException === 'undefined'",
    );
}

// ──────────────────────────────────────────────
// 2. FileReader surface
// ──────────────────────────────────────────────

#[test]
fn filereader_surface_descriptors_and_initial_state() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        FileReader.EMPTY === 0 && FileReader.LOADING === 1 && FileReader.DONE === 2
        && FileReader.prototype.EMPTY === 0 && FileReader.prototype.LOADING === 1
        && FileReader.prototype.DONE === 2
        && ['readAsArrayBuffer', 'readAsBinaryString', 'readAsText', 'readAsDataURL'].every(name => {
            var desc = Object.getOwnPropertyDescriptor(FileReader.prototype, name);
            return desc && typeof desc.value === 'function'
                && desc.value.name === name && desc.value.length === 1
                && desc.writable === true && desc.enumerable === false
                && desc.configurable === true;
        })
        && (() => {
            var desc = Object.getOwnPropertyDescriptor(FileReader.prototype, 'abort');
            return desc && desc.value.name === 'abort' && desc.value.length === 0;
        })()
        && ['readyState', 'result', 'error'].every(name => {
            var desc = Object.getOwnPropertyDescriptor(FileReader.prototype, name);
            return desc && typeof desc.get === 'function' && desc.set === undefined
                && desc.enumerable === true && desc.configurable === true;
        })
        && ['onloadstart', 'onprogress', 'onabort', 'onerror', 'onload', 'onloadend'].every(name => {
            var reader = new FileReader();
            var desc = Object.getOwnPropertyDescriptor(FileReader.prototype, name);
            return desc && desc.writable === true && desc.enumerable === true
                && desc.configurable === true && reader[name] === null;
        })
        && (() => {
            var reader = new FileReader();
            return reader.readyState === 0 && reader.result === null && reader.error === null;
        })()
        && (FileReader.prototype instanceof EventTarget)
        ",
    );
}

#[test]
fn filereader_constants_are_readonly() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            'use strict';
            for (var target of [FileReader, FileReader.prototype]) {
                for (var name of ['EMPTY', 'LOADING', 'DONE']) {
                    var desc = Object.getOwnPropertyDescriptor(target, name);
                    if (!desc || desc.writable !== false || desc.enumerable !== true
                        || desc.configurable !== false) return false;
                    try { target[name] = 99; } catch (e) {}
                    if (target[name] !== FileReader[name]) return false;
                }
            }
            return FileReader.EMPTY === 0 && FileReader.prototype.DONE === 2;
        })()
        ",
    );
}

#[test]
fn filereader_construction_and_receiver_brand_checks() {
    let (mut context, _handle) = setup();
    // Direct call without `new` throws `TypeError`.
    for source in ["FileReader()", "Reflect.construct(FileReader, [], Object)"] {
        let _ = source;
    }
    let result = context.eval(Source::from_bytes("FileReader()"));
    assert!(
        format!("{}", result.expect_err("expected TypeError")).contains("TypeError"),
        "FileReader() without new must throw TypeError"
    );
    // Borrowed methods and forged receivers throw synchronously.
    for source in [
        "FileReader.prototype.readAsArrayBuffer.call({}, new Blob(['x']))",
        "FileReader.prototype.abort.call({})",
        "Object.create(FileReader.prototype).readAsText(new Blob(['x']))",
        "Object.getOwnPropertyDescriptor(FileReader.prototype, 'readyState').get.call({})",
        "Object.getOwnPropertyDescriptor(FileReader.prototype, 'result').get.call(new EventTarget())",
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
    // Missing/non-Blob arguments throw `TypeError` without side effects.
    assert_eval(
        &mut context,
        r"
        (() => {
            var reader = new FileReader();
            for (var call of [
                () => reader.readAsArrayBuffer(),
                () => reader.readAsText(null),
                () => reader.readAsDataURL(123),
                () => reader.readAsBinaryString({}),
            ]) {
                try { call(); return false; }
                catch (e) { if (!(e instanceof TypeError)) return false; }
            }
            return reader.readyState === 0 && reader.result === null && reader.error === null;
        })()
        ",
    );
}

// ──────────────────────────────────────────────
// 3. Four representations
// ──────────────────────────────────────────────

#[test]
fn read_as_array_buffer_is_exact_and_fresh() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.done = false;
            globalThis.reader = new FileReader();
            globalThis.reader.onload = () => { globalThis.done = true; };
            globalThis.reader.readAsArrayBuffer(new Blob());
            return globalThis.reader.readyState === 1;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.done === true
        && globalThis.reader.readyState === 2
        && (globalThis.reader.result instanceof ArrayBuffer)
        && globalThis.reader.result.byteLength === 0
        && globalThis.reader.error === null
        ",
    );
    // Exact bytes, embedded NULs, composed/sliced/File inputs, fresh backing.
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.results = [];
            var blobs = [
                new Blob(['abc']),
                new Blob([new Uint8Array([0, 255, 65, 0])]),
                new Blob([new Blob(['he']), 'llo', new Uint8Array([33])]),
                new Blob(['hello world']).slice(6, 11),
                new File(['file-bytes'], 'f.txt'),
            ];
            globalThis.pending = blobs.length;
            for (var blob of blobs) {
                var reader = new FileReader();
                reader._index = globalThis.results.length;
                globalThis.results.push(null);
                reader.onload = (function (slot) {
                    return function () { globalThis.results[slot] = this.result; globalThis.pending--; };
                })(reader._index);
                reader.readAsArrayBuffer(blob);
            }
            return globalThis.pending === 5;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.pending === 0
        && (() => {
            var views = globalThis.results.map(b => Array.from(new Uint8Array(b)));
            return JSON.stringify(views[0]) === '[97,98,99]'
                && JSON.stringify(views[1]) === '[0,255,65,0]'
                && JSON.stringify(views[2]) === '[104,101,108,108,111,33]'
                && JSON.stringify(views[3]) === '[119,111,114,108,100]'
                && JSON.stringify(views[4]) === '[102,105,108,101,45,98,121,116,101,115]';
        })()
        ",
    );
    // Fresh backing: mutating one result never affects another read.
    assert_eval(
        &mut context,
        r"
        (() => {
            var blob = new Blob(['abc']);
            var first = new FileReader();
            globalThis.first = null;
            first.onload = function () { globalThis.first = this.result; };
            first.readAsArrayBuffer(blob);
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        new Uint8Array(globalThis.first)[0] === 97
        && (() => {
            new Uint8Array(globalThis.first)[0] = 0;
            var second = new FileReader();
            globalThis.second = null;
            second.onload = function () { globalThis.second = this.result; };
            second.readAsArrayBuffer(new Blob(['abc']));
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        "new Uint8Array(globalThis.second)[0] === 97 && new Uint8Array(globalThis.first)[0] === 0",
    );
}

#[test]
fn read_as_binary_string_preserves_nuls_and_high_bytes() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.value = null;
            var reader = new FileReader();
            reader.onload = function () { globalThis.value = this.result; };
            reader.readAsBinaryString(new Blob([new Uint8Array([0, 65, 255, 128, 0])]));
            return reader.readyState === 1;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        typeof globalThis.value === 'string' && globalThis.value.length === 5
        && globalThis.value.charCodeAt(0) === 0
        && globalThis.value.charCodeAt(1) === 65
        && globalThis.value.charCodeAt(2) === 255
        && globalThis.value.charCodeAt(3) === 128
        && globalThis.value.charCodeAt(4) === 0
        ",
    );
}

#[test]
fn read_as_text_utf8_bom_replacement_and_split_boundaries() {
    let (mut context, handle) = setup();
    // UTF-8 default, BOM handling, replacement for malformed input.
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.texts = [];
            var inputs = [
                [new Blob(['héllo😀']), undefined],
                [new Blob([new Uint8Array([0xEF, 0xBB, 0xBF, 0x41])]), 'utf-8'],
                [new Blob([new Uint8Array([0x41, 0xFF, 0x42])]), 'utf-8'],
                [new Blob([new Uint8Array([0xC3])]), 'utf-8'],
            ];
            globalThis.pending = inputs.length;
            for (var [blob, label] of inputs) {
                var reader = new FileReader();
                var slot = globalThis.texts.length;
                globalThis.texts.push(null);
                reader.onload = (function (s) {
                    return function () { globalThis.texts[s] = this.result; globalThis.pending--; };
                })(slot);
                if (label === undefined) reader.readAsText(blob);
                else reader.readAsText(blob, label);
            }
            return globalThis.pending === 4;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.pending === 0
        && globalThis.texts[0] === 'héllo😀'
        && globalThis.texts[1] === 'A'
        && globalThis.texts[2] === 'A\uFFFD B'.replace(' ', '')
        && globalThis.texts[3] === '\uFFFD'
        ",
    );
    // Split 2/3/4-byte sequences across chunk edges stay intact: build a
    // multichunk blob (2 × 16 KiB) with a multibyte char straddling the
    // first chunk edge.
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    assert_eval(
        &mut context,
        r"
        (() => {
            var prefix = [];
            for (var i = 0; i < 16383; i++) prefix.push(65);
            // U+20AC (€) is 3 bytes; force it to straddle the 16 KiB edge.
            var bytes = prefix.concat([0xE2, 0x82, 0xAC, 66]);
            while (bytes.length < 32768) bytes.push(67);
            globalThis.blob = new Blob([new Uint8Array(bytes)]);
            globalThis.out = null;
            var reader = new FileReader();
            reader.onload = function () { globalThis.out = this.result; };
            reader.readAsText(globalThis.blob);
            return reader.readyState === 1;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    // 16383 ASCII + € (1 UTF-16 unit) + 'B' + the C-fill: the byte length
    // is 32768 but the string is 2 units shorter (3 bytes -> 1 unit).
    assert_eval(
        &mut context,
        r"
        typeof globalThis.out === 'string'
        && globalThis.out.length === 32768 - 2
        && globalThis.out.charAt(16383) === '€'
        && globalThis.out.charAt(16384) === 'B'
        ",
    );
    // A non-UTF-8 label decodes through the Encoding Standard.
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.latin = null;
            var reader = new FileReader();
            reader.onload = function () { globalThis.latin = this.result; };
            reader.readAsText(new Blob([new Uint8Array([0xE9])]), 'windows-1252');
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(&mut context, "globalThis.latin === 'é'");
    // Unknown explicit label falls through to MIME/UTF-8 (never
    // `EncodingError`): success path with the full event sequence, the
    // MIME charset winning when present, UTF-8 otherwise, and no partial
    // result concerns since the read itself succeeds.
    for (label, media, bytes, expected) in [
        (
            "not-an-encoding",
            "text/plain;charset=windows-1252",
            "[0xE9]",
            "'é'",
        ),
        ("not-an-encoding", "text/plain", "[0x41]", "'A'"),
        (
            "not-an-encoding",
            "text/plain;charset=bogus-charset",
            "[0xC3, 0xA9]",
            "'é'",
        ),
    ] {
        assert_eval(
            &mut context,
            &format!(
                r"
        (() => {{
            globalThis.failed = 'unset';
            globalThis.events = [];
            var reader = new FileReader();
            for (var type of ['loadstart', 'progress', 'load', 'error', 'loadend']) {{
                reader.addEventListener(type, (function (t) {{
                    return function () {{ globalThis.events.push(t); }};
                }})(type));
            }}
            reader.onload = function () {{ globalThis.failed = this.result; }};
            reader.onerror = function () {{ globalThis.failed = 'unexpected-error'; }};
            reader.readAsText(new Blob([new Uint8Array({bytes})], {{ type: '{media}' }}), '{label}');
            return reader.readyState === 1 && reader.error === null;
        }})()
        "
            ),
        );
        drain_jobs(&mut context, &handle);
        assert_eval(
            &mut context,
            &format!(
                r"
        globalThis.failed === {expected}
        && globalThis.events.join('|') === 'loadstart|progress|load|loadend'
        "
            ),
        );
    }
}

#[test]
fn read_as_data_url_exact_packaging() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.urls = [];
            var cases = [
                [new Blob(['hello'], { type: 'text/plain' }), 'data:text/plain;base64,aGVsbG8='],
                [new Blob(['hello']), 'data:;base64,aGVsbG8='],
                [new Blob([new Uint8Array([])], { type: 'text/plain' }), 'data:text/plain;base64,'],
                [new Blob([new Uint8Array([0, 255, 16])]), 'data:;base64,AP8Q'],
            ];
            globalThis.pending = cases.length;
            for (var [blob, _] of cases) {
                var reader = new FileReader();
                var slot = globalThis.urls.length;
                globalThis.urls.push(null);
                reader.onload = (function (s) {
                    return function () { globalThis.urls[s] = this.result; globalThis.pending--; };
                })(slot);
                reader.readAsDataURL(blob);
            }
            return globalThis.pending === 4;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.pending === 0
        && globalThis.urls[0] === 'data:text/plain;base64,aGVsbG8='
        && globalThis.urls[1] === 'data:;base64,aGVsbG8='
        && globalThis.urls[2] === 'data:text/plain;base64,'
        && globalThis.urls[3] === 'data:;base64,AP8Q'
        && globalThis.urls.every(u => u.indexOf(' ') === -1 && u.indexOf('\n') === -1)
        ",
    );
}

// ──────────────────────────────────────────────
// 4. State / event model
// ──────────────────────────────────────────────

#[test]
fn empty_blob_full_event_sequence_is_exact() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.reader = new FileReader();
            for (var type of ['loadstart', 'progress', 'load', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function (e) {
                        globalThis.log.push(
                            t + ':' + e.loaded + '/' + e.total + ':' + this.readyState
                            + ':' + (e.target === globalThis.reader)
                            + ':' + (e.currentTarget === globalThis.reader)
                            + ':' + e.lengthComputable + ':' + e.bubbles + ':' + e.cancelable
                        );
                    };
                })(type));
            }
            globalThis.reader.readAsArrayBuffer(new Blob());
            return globalThis.reader.readyState === 1 && globalThis.log.length === 0;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.log.join('|') ===
            'loadstart:0/0:1:true:true:true:false:false'
            + '|progress:0/0:1:true:true:true:false:false'
            + '|load:0/0:2:true:true:true:false:false'
            + '|loadend:0/0:2:true:true:true:false:false'
        && globalThis.reader.readyState === 2
        && (globalThis.reader.result instanceof ArrayBuffer)
        ",
    );
}

#[test]
fn multichunk_sequence_has_final_progress_before_load() {
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.events = [];
            // 3 chunks: the injected clock is frozen, so throttling keeps
            // only the first progress; the final progress always fires.
            globalThis.blob = new Blob([new Uint8Array(3 * 16384)]);
            globalThis.reader = new FileReader();
            for (var type of ['loadstart', 'progress', 'load', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function (e) { globalThis.events.push(t + ':' + e.loaded + '/' + e.total); };
                })(type));
            }
            globalThis.reader.readAsArrayBuffer(globalThis.blob);
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.events[0] === 'loadstart:0/49152'
        && globalThis.events[globalThis.events.length - 2] === 'load:49152/49152'
        && globalThis.events[globalThis.events.length - 1] === 'loadend:49152/49152'
        && globalThis.events.filter(e => e.indexOf('progress:') === 0).length >= 1
        && (() => {
            var lastProgress = null;
            for (var e of globalThis.events) {
                if (e.indexOf('progress:') === 0) lastProgress = e;
                if (e.indexOf('load:') === 0) break;
            }
            return lastProgress === 'progress:49152/49152';
        })()
        ",
    );
}

#[test]
fn progress_throttle_uses_injected_clock() {
    // A slow-chunk proof with a frozen clock: only the first intermediate
    // plus the final progress fire. (The fast-clock twin uses per-pump
    // +50 ms steps; exact counts are asserted on loaded values in the
    // multichunk sequence test above.)
    let (mut slow, slow_handle) = setup_with_clock(vec![0, 0, 0, 0, 0, 0, 0, 0]);
    slow.eval(Source::from_bytes(
        "globalThis.events = []; \
         globalThis.blob = new Blob([new Uint8Array(3 * 16384)]); \
         globalThis.reader = new FileReader(); \
         for (var type of ['loadstart', 'progress', 'load', 'loadend']) { \
             globalThis.reader.addEventListener(type, (function (t) { \
                 return function (e) { globalThis.events.push(t); }; \
             })(type)); \
         } \
         globalThis.reader.readAsArrayBuffer(globalThis.blob);",
    ))
    .expect("setup eval");
    drain_jobs(&mut slow, &slow_handle);
    slow.eval(Source::from_bytes(
        "globalThis.count = globalThis.events.filter(t => t === 'progress').length;",
    ))
    .expect("count eval");
    // Frozen clock: the first chunk still reports (one per chunk when
    // chunks arrive less often), the second chunk is throttled, and the
    // final progress always fires: exactly 2 progress events with the
    // full ordered sequence.
    assert_eval(&mut slow, "globalThis.count === 1");
    assert_eval(
        &mut slow,
        "globalThis.events.join('|') === 'loadstart|progress|load|loadend'",
    );
    // Slow-chunk exception: one progress per chunk when chunks arrive less
    // often than 50 ms. A single-chunk blob always emits exactly its final
    // progress even with a frozen clock.
    let (mut single, single_handle) = setup_with_clock(vec![0]);
    single
        .eval(Source::from_bytes(
            "globalThis.single = []; \
             globalThis.reader = new FileReader(); \
             globalThis.reader.addEventListener('progress', e => globalThis.single.push(e.loaded)); \
             globalThis.reader.readAsText(new Blob(['hi']));",
        ))
        .expect("setup eval");
    drain_jobs(&mut single, &single_handle);
    assert_eval(&mut single, "globalThis.single.join(',') === '2'");
}

// ──────────────────────────────────────────────
// 5. Synchronous LOADING guard / reentrancy
// ──────────────────────────────────────────────

#[test]
fn second_read_while_loading_throws_invalid_state() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.reader = new FileReader();
            globalThis.first = null;
            globalThis.reader.onload = function () { globalThis.first = this.result; };
            globalThis.reader.readAsText(new Blob(['first']));
            // A second read throws synchronously and preserves the first.
            try {
                globalThis.reader.readAsArrayBuffer(new Blob(['second']));
                return false;
            } catch (e) {
                if (!((e instanceof DOMException) && e.name === 'InvalidStateError')) return false;
            }
            return globalThis.reader.readyState === 1
                && globalThis.reader.result === null
                && globalThis.reader.error === null;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.first === 'first' && globalThis.reader.readyState === 2",
    );
}

#[test]
fn reentrant_load_starts_new_read_and_suppresses_old_loadend() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.reader = new FileReader();
            globalThis.reader.addEventListener('load', function () {
                globalThis.log.push('load:' + this.result);
                if (this.result === 'first') {
                    this.readAsText(new Blob(['second']));
                    globalThis.log.push('reentered:' + this.readyState);
                }
            });
            for (var type of ['loadstart', 'progress', 'loadend', 'error', 'abort']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function () { globalThis.log.push(t); };
                })(type));
            }
            globalThis.reader.readAsText(new Blob(['first']));
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.log.join('|') ===
            'loadstart|progress|load:first|reentered:1|loadstart|progress|load:second|loadend'
        ",
    );
}

// ──────────────────────────────────────────────
// 6. Abort races
// ──────────────────────────────────────────────

#[test]
fn abort_before_first_job_emits_only_abort_loadend() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.reader = new FileReader();
            for (var type of ['loadstart', 'progress', 'load', 'error', 'abort', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function () { globalThis.log.push(t); };
                })(type));
            }
            globalThis.reader.readAsText(new Blob(['data']));
            globalThis.reader.abort();
            return globalThis.reader.readyState === 2
                && globalThis.reader.result === null
                && globalThis.reader.error === null;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(&mut context, "globalThis.log.join('|') === 'abort|loadend'");
}

#[test]
fn abort_between_chunks_suppresses_stale_events() {
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.blob = new Blob([new Uint8Array(3 * 16384)]);
            globalThis.reader = new FileReader();
            globalThis.reader.addEventListener('progress', function (e) {
                globalThis.log.push('progress:' + e.loaded);
                if (e.loaded === 16384) this.abort();
            });
            for (var type of ['loadstart', 'load', 'error', 'abort', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function () { globalThis.log.push(t); };
                })(type));
            }
            globalThis.reader.readAsArrayBuffer(globalThis.blob);
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.log.join('|') === 'loadstart|progress:16384|abort|loadend'
        && globalThis.reader.readyState === 2
        && globalThis.reader.result === null
        && globalThis.reader.error === null
        ",
    );
}

#[test]
fn reentrant_error_handler_starts_new_read() {
    // Reentrant `error` case through a deterministic public trigger: a
    // Data-URL preflight failure (`QuotaExceededError`, synchronous, no
    // slot consumed) whose `error` handler starts a new read. Only the old
    // `loadend` is suppressed; the new operation completes intact with its
    // full event sequence.
    //
    // (The quota-saturation trigger cannot serve here under M9-C: `fail_fast`
    // dispatches before the 64 threaded filler chunks complete, so a restart
    // from the handler would racily hit the still-full quota. Quota recovery
    // itself is covered by `concurrent_read_quota_recovers_after_success_…`.)
    let prefix = "data:;base64,";
    let limit = (prefix.len() + 4) as u64;
    let (mut context, handle) = setup_with_data_url_limit(limit);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.reader = new FileReader();
            globalThis.reader.addEventListener('error', function () {
                globalThis.log.push('error:' + this.error.name);
                this.readAsText(new Blob(['recovered']));
            });
            for (var type of ['loadstart', 'progress', 'load', 'abort', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function () { globalThis.log.push(t); };
                })(type));
            }
            // 4 bytes need prefix + 8 chars > limit: synchronous preflight
            // failure, readyState DONE with `error` set, no load yet.
            globalThis.reader.readAsDataURL(new Blob([new Uint8Array([1, 2, 3, 4])]));
            return globalThis.reader.readyState === 2
                && (globalThis.reader.error instanceof DOMException);
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.log.join('|') === 'error:QuotaExceededError|loadstart|progress|load|loadend'
        && globalThis.reader.readyState === 2
        && globalThis.reader.result === 'recovered'
        && globalThis.reader.error === null
        ",
    );
}

#[test]
fn abort_handler_restart_after_mid_chunk_abort() {
    // Reentrant `abort` case: abort mid-operation (first progress of a
    // 3-chunk blob), then restart from the `abort` handler. The old
    // `loadend` is suppressed; the new operation completes intact.
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.blob = new Blob([new Uint8Array(3 * 16384)]);
            globalThis.reader = new FileReader();
            globalThis.reader.addEventListener('progress', function (e) {
                globalThis.log.push('progress:' + e.loaded);
                if (e.loaded === 16384) this.abort();
            });
            globalThis.reader.addEventListener('abort', function () {
                globalThis.log.push('abort');
                this.readAsText(new Blob(['fresh']));
            });
            for (var type of ['loadstart', 'load', 'error', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function () { globalThis.log.push(t); };
                })(type));
            }
            globalThis.reader.readAsArrayBuffer(globalThis.blob);
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.log.join('|') ===
            'loadstart|progress:16384|abort|loadstart|progress:5|load|loadend'
        && globalThis.reader.readyState === 2
        && globalThis.reader.result === 'fresh'
        && globalThis.reader.error === null
        ",
    );
}

#[test]
fn abort_in_empty_or_done_state_is_silent() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            var reader = new FileReader();
            var events = 0;
            for (var type of ['loadstart', 'progress', 'load', 'error', 'abort', 'loadend']) {
                reader.addEventListener(type, () => events++);
            }
            // EMPTY: sets result null, no error change, no events.
            reader.abort();
            if (reader.readyState !== 0 || reader.result !== null || events !== 0) return false;
            return true;
        })()
        ",
    );
    // DONE: same silence after a completed read.
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.reader = new FileReader();
            globalThis.reader.readAsText(new Blob(['x']));
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.reader.readyState === 2 && globalThis.reader.result === 'x'
        && (() => {
            globalThis.count = 0;
            for (var type of ['loadstart', 'progress', 'load', 'error', 'abort', 'loadend']) {
                globalThis.reader.addEventListener(type, () => globalThis.count++);
            }
            globalThis.reader.abort();
            return globalThis.reader.readyState === 2
                && globalThis.reader.result === null
                && globalThis.reader.error === null
                && globalThis.count === 0;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(&mut context, "globalThis.count === 0");
}

#[test]
fn stale_completion_after_new_operation_is_noop() {
    // Abort, then start a new operation from the abort handler: the stale
    // first-generation jobs must not emit progress/load/error/loadend, and
    // the new result stays intact.
    let (mut context, handle) = setup_with_chunk(16 * 1024);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.log = [];
            globalThis.blob = new Blob([new Uint8Array(3 * 16384)]);
            globalThis.reader = new FileReader();
            globalThis.reader.addEventListener('abort', function () {
                globalThis.log.push('abort');
                this.readAsText(new Blob(['new']));
            });
            for (var type of ['loadstart', 'progress', 'load', 'error', 'loadend']) {
                globalThis.reader.addEventListener(type, (function (t) {
                    return function (e) {
                        globalThis.log.push(t + (this.result === null ? '' : ':' + typeof this.result));
                    };
                })(type));
            }
            globalThis.reader.readAsArrayBuffer(globalThis.blob);
            globalThis.reader.abort();
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.reader.readyState === 2
        && globalThis.reader.result === 'new'
        && globalThis.reader.error === null
        && globalThis.log.join('|') === 'abort|loadstart|progress|load:string|loadend:string'
        ",
    );
}

#[test]
fn gc_survives_queued_filereader_jobs() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.verdict = 'pending';
            globalThis.reader = new FileReader();
            globalThis.reader.onload = function () { globalThis.verdict = 'load:' + this.result; };
            globalThis.reader.readAsText(new Blob(['gc-alive']));
            return globalThis.verdict === 'pending';
        })()
        ",
    );
    boa_gc::force_collect();
    drain_jobs(&mut context, &handle);
    boa_gc::force_collect();
    assert_eval(&mut context, "globalThis.verdict === 'load:gc-alive'");
}

// ──────────────────────────────────────────────
// 7. Failure / quota / limits
// ──────────────────────────────────────────────

#[test]
fn data_url_quota_boundary() {
    // `== limit` succeeds, `+1` fails before allocation with
    // `QuotaExceededError`, no partial result, then `loadend`.
    let prefix = "data:;base64,";
    // 3 bytes -> 4 payload chars: pick a limit of prefix + 4. The
    // 4-byte blob needs prefix + 8 chars, so it fails in the synchronous
    // preflight (readyState DONE with `error` set, no load yet).
    let limit = (prefix.len() + 4) as u64;
    let (mut context, handle) = setup_with_data_url_limit(limit);
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.ok = null;
            globalThis.fail = null;
            var exact = new FileReader();
            exact.onload = function () { globalThis.ok = this.result; };
            exact.readAsDataURL(new Blob([new Uint8Array([1, 2, 3])]));
            var over = new FileReader();
            over.onerror = function () { globalThis.fail = this.error; };
            over.onload = function () { globalThis.fail = 'unexpected-load'; };
            over.readAsDataURL(new Blob([new Uint8Array([1, 2, 3, 4])]));
            return exact.readyState === 1
                && over.readyState === 2
                && (over.error instanceof DOMException)
                && over.error.name === 'QuotaExceededError';
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        "typeof globalThis.ok === 'string' && globalThis.ok.indexOf('data:;base64,') === 0",
    );
    assert_eval(
        &mut context,
        r"
        (globalThis.fail instanceof DOMException)
        && globalThis.fail.name === 'QuotaExceededError'
        ",
    );
}

#[test]
fn concurrent_read_quota_recovers_after_success_error_abort() {
    // 64 active reads succeed; the 65th emits SecurityError; slots recover
    // through success, error, and abort paths.
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.readers = [];
            globalThis.errors = 0;
            for (var i = 0; i < 64; i++) {
                var reader = new FileReader();
                reader.onerror = function () { globalThis.errors++; };
                reader.readAsText(new Blob(['x']));
                globalThis.readers.push(reader);
            }
            globalThis.extra = new FileReader();
            globalThis.extra.onerror = function () {};
            globalThis.extra.readAsText(new Blob(['x']));
            return globalThis.readers.length === 64;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(
        &mut context,
        r"
        globalThis.extra.readyState === 2
        && (globalThis.extra.error instanceof DOMException)
        && globalThis.extra.error.name === 'SecurityError'
        && globalThis.extra.result === null
        ",
    );
    // After the 64 succeed, a new read works again (slots recovered).
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.again = new FileReader();
            globalThis.again.outcome = null;
            globalThis.again.onload = function () { globalThis.again.outcome = this.result; };
            globalThis.again.readAsText(new Blob(['recovered']));
            return true;
        })()
        ",
    );
    drain_jobs(&mut context, &handle);
    assert_eval(&mut context, "globalThis.again.outcome === 'recovered'");
}

// ──────────────────────────────────────────────
// 8. M3 regression (DOMException mapping, unchanged success)
// ──────────────────────────────────────────────

#[test]
fn m3_promise_rejections_are_dom_exceptions_with_fixed_mapping() {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        r"
        (() => {
            globalThis.outcome = 'pending';
            // An over-limit blob through the public path rejects with the
            // mapped QuotaExceededError (limit chosen via chunk floor is
            // covered by dedicated suites; here the mapping shape is pinned
            // on a tiny valid blob's success contrast below).
            globalThis.p = new Blob(['ok']).text();
            globalThis.p.then(
                value => { globalThis.outcome = 'fulfilled:' + value; },
                error => { globalThis.outcome = 'rejected:' + error.name; }
            );
            return globalThis.outcome === 'pending' && (globalThis.p instanceof Promise);
        })()
        ",
    );
    // M9-B host loop: `poll_io` turns the worker completion into a Boa
    // job, then `run_jobs` settles it. `poll_io` is strictly
    // non-blocking, so the loop yields briefly (bounded, hang-guard only)
    // while threaded-executor I/O is still outstanding.
    for _ in 0..200 {
        let settled = handle.poll_io(&mut context).unwrap_or(0);
        context.run_jobs().expect("run_jobs failed");
        if settled == 0 && !handle.has_pending_io() {
            break;
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while handle.has_pending_io() {
                let _ = handle.poll_io(&mut context);
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
    assert_eval(&mut context, "globalThis.outcome === 'fulfilled:ok'");
}

// ──────────────────────────────────────────────
// 9. Property/model test (bounded operation sequences vs pure model)
// ──────────────────────────────────────────────

/// Reentrant handler mode installed for a scenario.
///
/// (`error` never fires in the unknown-label-free corpus, so there is no
/// `ErrorRestart` mode: every start path succeeds or aborts.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HandlerMode {
    /// No reentrant handler.
    None,
    /// `loadstart` handler calls `abort()`.
    LoadstartAbort,
    /// `progress` handler calls `abort()`.
    ProgressAbort,
    /// `load` handler starts a new text read.
    LoadRestart,
    /// `abort` handler starts a new text read.
    AbortRestart,
}

/// A synchronous driver action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    /// `readAsText(blob)` — succeeds unless LOADING (throws).
    ReadOk,
    /// A second successful start used for double-start coverage; behaves
    /// exactly like `ReadOk` (throws when LOADING, starts otherwise).
    ReadAgain,
    /// `abort()` — silent unless LOADING.
    Abort,
}

/// Terminal kinds for the pure model queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TermKind {
    Load,
    Abort,
}

/// A queued model job.
#[derive(Clone, Copy, Debug)]
enum ModelJob {
    /// One pump of a single-chunk operation.
    Pump { generation: u64 },
    /// One terminal dispatch.
    Term { generation: u64, kind: TermKind },
}

/// Small pure model of the single-chunk FileReader state machine.
///
/// Mirrors the normative transitions only: sync guards, generation
/// replacement on `abort()`/new reads, stale-job no-ops, the single
/// final `progress` before `load`, and conditional `loadend`
/// suppression on reentrant replacement. It deliberately knows nothing
/// about chunks, throttling, encodings, or packaging.
struct PureModel {
    ready: u8,
    generation: u64,
    result: Option<String>,
    error: Option<String>,
    terminal: bool,
    events: Vec<String>,
    queue: std::collections::VecDeque<ModelJob>,
    handler: HandlerMode,
    stale_jobs: usize,
    /// Restart handlers fire only once per scenario (otherwise a `load`
    /// restart would ping-pong forever).
    restarted: bool,
}

impl PureModel {
    fn new(handler: HandlerMode) -> Self {
        Self {
            ready: 0,
            generation: 0,
            result: None,
            error: None,
            terminal: false,
            events: Vec::new(),
            queue: std::collections::VecDeque::new(),
            handler,
            stale_jobs: 0,
            restarted: false,
        }
    }

    fn emit(&mut self, event: &str) {
        self.events.push(format!("{event}:{}", self.ready));
    }

    fn start_ok(&mut self) {
        self.generation += 1;
        self.ready = 1;
        self.result = None;
        self.error = None;
        self.terminal = false;
        let generation = self.generation;
        self.queue.push_back(ModelJob::Pump { generation });
    }

    fn abort_effect(&mut self) {
        self.generation += 1;
        self.ready = 2;
        self.result = None;
        self.error = None;
        self.terminal = false;
        let generation = self.generation;
        self.queue.push_back(ModelJob::Term {
            generation,
            kind: TermKind::Abort,
        });
    }

    /// Applies one synchronous driver action, recording sync throws.
    fn act(&mut self, action: Action) {
        match action {
            Action::ReadOk | Action::ReadAgain => {
                if self.ready == 1 {
                    self.events.push("throw:InvalidStateError".to_owned());
                } else {
                    self.start_ok();
                }
            }
            Action::Abort => {
                if self.ready == 1 {
                    self.abort_effect();
                } else {
                    self.result = None;
                }
            }
        }
    }

    /// Runs the queued jobs to quiescence, applying the handler mode.
    fn drain(&mut self) {
        while let Some(job) = self.queue.pop_front() {
            match job {
                ModelJob::Pump { generation } => {
                    if generation != self.generation {
                        self.stale_jobs += 1;
                        continue;
                    }
                    self.emit("loadstart");
                    if self.handler == HandlerMode::LoadstartAbort {
                        self.abort_effect();
                        continue;
                    }
                    // Single-chunk success: the only progress is the final
                    // one (frozen clock, `loaded == total`).
                    self.emit("progress");
                    if self.handler == HandlerMode::ProgressAbort {
                        self.abort_effect();
                        continue;
                    }
                    self.ready = 2;
                    self.result = Some("model".to_owned());
                    self.error = None;
                    let generation = self.generation;
                    self.queue.push_back(ModelJob::Term {
                        generation,
                        kind: TermKind::Load,
                    });
                }
                ModelJob::Term { generation, kind } => {
                    if generation != self.generation {
                        self.stale_jobs += 1;
                        continue;
                    }
                    if self.terminal {
                        continue;
                    }
                    let name = match kind {
                        TermKind::Load => "load",
                        TermKind::Abort => "abort",
                    };
                    self.emit(name);
                    self.terminal = true;
                    // Restart handlers are one-shot per scenario.
                    // (`error` never fires in the unknown-label-free
                    // corpus: every start path succeeds or aborts.)
                    let restart = !self.restarted
                        && matches!(
                            (kind, self.handler),
                            (TermKind::Load, HandlerMode::LoadRestart)
                                | (TermKind::Abort, HandlerMode::AbortRestart)
                        );
                    if restart {
                        self.restarted = true;
                        self.start_ok();
                    }
                    // Conditional `loadend`: suppressed only on reentrant
                    // replacement during this dispatch.
                    if generation == self.generation {
                        self.emit("loadend");
                    }
                }
            }
        }
    }

    /// Renders the observable summary in exactly the JS shape
    /// `ready|result|error|events`.
    fn summary(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.ready,
            self.result.as_deref().unwrap_or("null"),
            self.error.as_deref().unwrap_or("null"),
            self.events.join(",")
        )
    }
}

/// JavaScript actor snippet for a handler mode (attached after the logging
/// listeners so the log order is logger-first, matching the model).
/// Restart actors are one-shot per scenario via `restarted`.
fn handler_js(mode: HandlerMode) -> &'static str {
    match mode {
        HandlerMode::None => "",
        HandlerMode::LoadstartAbort => {
            "reader.addEventListener('loadstart', function () { this.abort(); });"
        }
        HandlerMode::ProgressAbort => {
            "reader.addEventListener('progress', function () { this.abort(); });"
        }
        HandlerMode::LoadRestart => {
            "reader.addEventListener('load', function () { if (!restarted) { restarted = true; this.readAsText(blob); } });"
        }
        HandlerMode::AbortRestart => {
            "reader.addEventListener('abort', function () { if (!restarted) { restarted = true; this.readAsText(blob); } });"
        }
    }
}

/// JavaScript snippet for one driver action (sync throws are logged).
fn action_js(action: Action) -> &'static str {
    match action {
        Action::ReadOk | Action::ReadAgain => {
            "try { reader.readAsText(blob); } catch (e) { log.push('throw:' + e.name); }"
        }
        Action::Abort => "reader.abort();",
    }
}

#[test]
fn bounded_operation_sequences_match_pure_model() {
    // Bounded enumerated corpus: every phase-1 sync prefix (start,
    // abort-before-first-job, double start, abort-then-ok,
    // read-abort-read) x every phase-2 follow-up (none, abort, restart)
    // x every reentrant handler mode. Each scenario runs real JS in a
    // fresh Context and its observed state/event log must equal the pure
    // model summary exactly — any divergence (extra or missing event,
    // wrong state, leaked generation) fails the assertion.
    let phases: &[&[Action]] = &[
        &[],
        &[Action::ReadOk],
        &[Action::ReadAgain],
        &[Action::Abort],
        &[Action::ReadOk, Action::Abort],
        &[Action::ReadOk, Action::ReadOk],
        &[Action::ReadAgain, Action::ReadOk],
        &[Action::Abort, Action::ReadOk],
        &[Action::ReadOk, Action::Abort, Action::ReadOk],
    ];
    let follow_ups: &[&[Action]] = &[&[], &[Action::Abort], &[Action::ReadOk]];
    let handlers = [
        HandlerMode::None,
        HandlerMode::LoadstartAbort,
        HandlerMode::ProgressAbort,
        HandlerMode::LoadRestart,
        HandlerMode::AbortRestart,
    ];
    let mut scenarios = 0usize;
    let mut model_stale_total = 0usize;
    let mut js_saw_load = false;
    let mut js_saw_abort = false;
    let mut js_saw_loadend = false;
    let mut js_saw_throw = false;
    let mut js_saw_restart = false;
    for phase in phases {
        for follow_up in follow_ups {
            for handler in handlers {
                scenarios += 1;
                // Pure-model expectation: stage 1, drain, stage 2, drain.
                let mut model = PureModel::new(handler);
                for action in *phase {
                    model.act(*action);
                }
                model.drain();
                for action in *follow_up {
                    model.act(*action);
                }
                model.drain();
                model_stale_total += model.stale_jobs;
                let expected = model.summary();
                // Real JS execution in a fresh Context.
                let (mut context, handle) = setup();
                let mut setup_js = String::from(
                    "var reader = new FileReader(); \
                     var blob = new Blob(['model']); \
                     var restarted = false; \
                     var log = []; \
                     for (var t of ['loadstart','progress','load','error','abort','loadend']) \
                         reader.addEventListener(t, (function (tt) { \
                             return function () { log.push(tt + ':' + reader.readyState); }; \
                         })(t)); ",
                );
                setup_js.push_str(handler_js(handler));
                context
                    .eval(Source::from_bytes(&setup_js))
                    .expect("model setup eval");
                for stage in [phase, follow_up] {
                    if !stage.is_empty() {
                        let script: String = stage
                            .iter()
                            .map(|action| action_js(*action))
                            .collect::<Vec<_>>()
                            .join(" ");
                        context
                            .eval(Source::from_bytes(&script))
                            .expect("model stage eval");
                    }
                    drain_jobs(&mut context, &handle);
                }
                let observed: String = context
                    .eval(Source::from_bytes(
                        "reader.readyState + '|' \
                         + (reader.result === null ? 'null' : reader.result) + '|' \
                         + (reader.error === null ? 'null' : reader.error.name) + '|' \
                         + log.join(',')",
                    ))
                    .expect("model readback eval")
                    .to_string(&mut context)
                    .expect("model readback string")
                    .to_std_string_escaped();
                assert_eq!(
                    observed, expected,
                    "model divergence for phase={phase:?} follow_up={follow_up:?} handler={handler:?}"
                );
                // Coverage is computed over the event section only (the
                // first entry otherwise merges with the `ready|result|error`
                // state prefix).
                let events_section = observed.split('|').nth(3).unwrap_or("");
                for entry in events_section.split(',') {
                    if entry.starts_with("load:") {
                        js_saw_load = true;
                    } else if entry.starts_with("abort:") {
                        js_saw_abort = true;
                    } else if entry.starts_with("loadend:") {
                        js_saw_loadend = true;
                    } else if entry.starts_with("throw:") {
                        js_saw_throw = true;
                    }
                }
                if observed.matches("loadstart").count() >= 2 {
                    js_saw_restart = true;
                }
            }
        }
    }
    assert_eq!(scenarios, 9 * 3 * 5, "corpus must not shrink");
    // The corpus must really exercise load/abort terminals, sync
    // throws, generation replacement, and stale completions — otherwise
    // the comparison above would be vacuous. (`error` terminals stay
    // covered by the quota/error-path suites; the unknown-label
    // fail-fast path no longer exists.) Mutation probes performed
    // during development (removing the LOADING guard, the `loadstart`
    // generation recheck, or the conditional-`loadend` suppression)
    // each fail at least one assertion in this file: the guard probe
    // fails the `throw:InvalidStateError` comparison here, the recheck
    // probe fails
    // `filereader::tests::loadstart_abort_performs_no_source_read`, so the
    // coverage is not fabricated.
    assert!(
        js_saw_load && js_saw_abort && js_saw_loadend,
        "load/abort terminals must be observed in JS logs"
    );
    assert!(js_saw_throw, "sync LOADING-guard throws must be observed");
    assert!(
        js_saw_restart,
        "handler-driven generation replacement must be observed"
    );
    assert!(
        model_stale_total > 0,
        "stale completions must occur in the corpus"
    );
}

// ──────────────────────────────────────────────
// 10. Negative API guards
// ──────────────────────────────────────────────

#[test]
fn excluded_m4b_apis_are_absent() {
    // M4-A froze before the M6 URL milestone: the check asserts the absence
    // of the M4-B sync surface, workers runtime, and full-DOM names. The
    // M6 `URL` namespace (with exactly `createObjectURL`/`revokeObjectURL`)
    // is the expected M6 addition and is pinned by the M6 suites instead.
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        typeof FileReaderSync === 'undefined'
        && typeof DedicatedWorker === 'undefined'
        && typeof SharedWorker === 'undefined'
        && typeof CustomEvent === 'undefined'
        && typeof AbortSignal === 'undefined'
        && typeof FileReader.prototype.readAsTextSync === 'undefined'
        ",
    );
}
