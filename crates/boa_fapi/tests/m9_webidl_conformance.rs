//! M9-A conformance: Web IDL `sequence<BlobPart>`, union conversion,
//! text packaging, registration identity and the `FileList` surface.
//!
//! Every test executes real JavaScript in a real Boa `Context` and asserts
//! only JS-observable state. Trace rows:
//! `M9A-IDL-01` (iterable sequence, no array-only path),
//! `M9A-IDL-02` (abrupt completion without iterator closing, quota),
//! `M9A-IDL-03` (union fallback for primitive/object values),
//! `M9A-TEXT-01` (label → MIME charset → UTF-8 → BOM),
//! `M9A-REG-01` (same identity idempotent, different identity rejected),
//! `M9A-FLIST-01` (indexed-getter iterator surface via host object).
//! Rework rows: `M9A-RW-01` (unknown label falls through),
//! `M9A-RW-02` (BOM overrides any fallback), `M9A-RW-03` (argument
//! order left to right), `M9A-RW-04` (conversion before processing),
//! `M9A-RW-05` (no `return()` on abrupt completion), `M9A-RW-06`
//! (FileList value iterator only).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiEnvironment, FileApiExtension, RegisterError};

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

/// Registers the extension into a fresh context.
fn setup() -> Context {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Registers the extension as a worker (for `FileReaderSync` packaging).
fn setup_worker() -> Context {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    context
}

/// Registers with tight blob-part/size ceilings for quota tests.
fn setup_quota(max_parts: usize, max_blob_size: u64) -> Context {
    let mut context = Context::default();
    let chunk = 64 * 1024_u64;
    let blob_size = max_blob_size.max(chunk);
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_parts,
        max_blob_size: blob_size,
        max_materialize_bytes: blob_size,
        max_sync_read_bytes: blob_size.min(32 * 1024 * 1024),
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

/// Registers a worker context with a 16 KiB chunk ceiling (BOM-split
/// coverage across FileReading jobs).
fn setup_chunked_worker() -> Context {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        default_chunk_size: 16 * 1024,
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

/// Evaluates `source` and asserts the result is `true`.
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

/// Evaluates `source` and asserts it throws a `TypeError`.
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
// M9A-IDL-01: iterable sequence, no array-only path
// ──────────────────────────────────────────────

#[test]
fn m9a_idl_01_iterable_sequence_shapes() {
    let context = &mut setup();
    // Array, Set, generator, custom iterator, boxed String,
    // Uint8Array-as-outer-sequence all iterate; Blob/File share one path.
    assert_eval(context, "new Blob(['a', 'b']).size === 2");
    assert_eval(context, "new Blob(new Set(['ab', 'c'])).size === 3");
    assert_eval(
        context,
        "new Blob((function* () { yield 'x'; yield 'yz'; })()).size === 3",
    );
    assert_eval(
        context,
        r"
        new Blob({
            [Symbol.iterator]() {
                var i = 0;
                var parts = ['he', 'llo'];
                return { next() {
                    if (i >= parts.length) return { done: true, value: undefined };
                    return { done: false, value: parts[i++] };
                } };
            }
        }).size === 5
        ",
    );
    assert_eval(context, "new Blob(new String('boxed')).size === 5");
    // Uint8Array-as-sequence iterates numbers (65, 66), each stringified
    // per the union fallback: "65" + "66" is 4 bytes.
    assert_eval(context, "new Blob(new Uint8Array([65, 66])).size === 4");
    assert_eval(context, "new File(new Set(['ab']), 'f.txt').size === 2");
    assert_eval(
        context,
        "new File((function* () { yield 'xy'; })(), 'f.txt').size === 2",
    );
    // Primitive strings and non-iterables stay conversion errors.
    assert_eval_type_error(context, "new Blob('abc')");
    assert_eval_type_error(context, "new Blob(42)");
    assert_eval_type_error(context, "new Blob(null)");
    assert_eval_type_error(context, "new File('abc', 'f.txt')");
}

#[test]
fn m9a_idl_01_iterator_read_once_and_left_to_right() {
    let context = &mut setup();
    // `@@iterator` is read exactly once; elements convert left to right.
    assert_eval(
        context,
        r"
        (() => {
            var reads = 0;
            var log = [];
            var seq = {
                get [Symbol.iterator]() {
                    reads++;
                    var i = 0;
                    var parts = [
                        { toString() { log.push('a'); return 'a'; } },
                        { toString() { log.push('b'); return 'b'; } },
                    ];
                    return function () {
                        return { next() {
                            if (i >= parts.length) return { done: true, value: undefined };
                            return { done: false, value: parts[i++] };
                        } };
                    };
                }
            };
            var blob = new Blob(seq);
            return reads === 1 && log.join(',') === 'a,b' && blob.size === 2;
        })()
        ",
    );
    // Proxy-observed conversion order matches for Blob and File.
    assert_eval(
        context,
        r"
        (() => {
            function order(Ctor, extra) {
                var log = [];
                function part(name) {
                    return { toString() { log.push(name); return name; } };
                }
                var blob = new Ctor([part('x'), part('y')], extra);
                return log.join(',') + ':' + blob.size;
            }
            return order(Blob) === 'x,y:2'
                && order(File, 'f.txt') === 'x,y:2';
        })()
        ",
    );
}

// ──────────────────────────────────────────────
// M9A-IDL-02: abrupt completion without closing, quota boundary
// (`M9A-RW-05`: no `return()` on `next`/`done`/`value`/conversion
// abrupt completion)
// ──────────────────────────────────────────────

#[test]
fn m9a_idl_02_abrupt_completion_propagates_without_close() {
    let context = &mut setup();
    // Throwing `@@iterator` propagates with its own class/message.
    assert_eval(
        context,
        r"
        (() => {
            var seq = { get [Symbol.iterator]() { throw new RangeError('iter-boom'); } };
            try { new Blob(seq); return false; }
            catch (e) { return e instanceof RangeError && e.message === 'iter-boom'; }
        })()
        ",
    );
    // Throwing `next` propagates; `return()` is NOT called.
    assert_eval(
        context,
        r"
        (() => {
            var closed = false;
            var seq = { [Symbol.iterator]() {
                return {
                    next() { throw new TypeError('next-boom'); },
                    return() { closed = true; return {}; },
                };
            } };
            try { new Blob(seq); return false; }
            catch (e) {
                return !closed && e instanceof TypeError && e.message === 'next-boom';
            }
        })()
        ",
    );
    // Throwing `done` getter propagates; `return()` is NOT called.
    assert_eval(
        context,
        r"
        (() => {
            var closed = false;
            var seq = { [Symbol.iterator]() {
                return {
                    next() {
                        var result = {};
                        Object.defineProperty(result, 'done', { get() { throw new Error('done-boom'); } });
                        result.value = 'x';
                        return result;
                    },
                    return() { closed = true; return {}; },
                };
            } };
            try { new Blob(seq); return false; }
            catch (e) { return !closed && e.message === 'done-boom'; }
        })()
        ",
    );
    // Throwing `value` getter propagates; `return()` is NOT called.
    assert_eval(
        context,
        r"
        (() => {
            var closed = false;
            var seq = { [Symbol.iterator]() {
                return {
                    next() {
                        var result = { done: false };
                        Object.defineProperty(result, 'value', { get() { throw new Error('value-boom'); } });
                        return result;
                    },
                    return() { closed = true; return {}; },
                };
            } };
            try { new Blob(seq); return false; }
            catch (e) { return !closed && e.message === 'value-boom'; }
        })()
        ",
    );
    // Throwing element `toString` propagates; `return()` is NOT called
    // and never replaces the original exception.
    assert_eval(
        context,
        r"
        (() => {
            var closed = false;
            function evil() {}
            evil.prototype.toString = function () { throw new Error('part-boom'); };
            var seq = { [Symbol.iterator]() {
                var done = false;
                var iterator = {
                    next() {
                        if (done) return { done: true, value: undefined };
                        done = true;
                        return { done: false, value: new evil() };
                    },
                    return() { closed = true; return {}; },
                };
                return iterator;
            } };
            try { new Blob(seq); return false; }
            catch (e) { return !closed && e.message === 'part-boom'; }
        })()
        ",
    );
    // A throwing `return()` never runs on conversion failure, so it can
    // neither win nor replace the original exception.
    assert_eval(
        context,
        r"
        (() => {
            var returned = false;
            var seq = { [Symbol.iterator]() {
                var first = true;
                return {
                    next() {
                        if (first) {
                            first = false;
                            return { done: false, value: { toString() { throw new Error('conv-boom'); } } };
                        }
                        return { done: true, value: undefined };
                    },
                    return() { returned = true; throw new Error('return-boom'); },
                };
            } };
            try { new Blob(seq); return false; }
            catch (e) { return !returned && e.message === 'conv-boom'; }
        })()
        ",
    );
    // Non-callable `@@iterator` is a synchronous TypeError.
    assert_eval_type_error(context, "new Blob({})");
    assert_eval_type_error(context, "new Blob({ [Symbol.iterator]: 42 })");
}

#[test]
fn m9a_idl_02_quota_boundary_ends_infinite_iterator() {
    let context = &mut setup_quota(8, 64 * 1024);
    // An infinite iterator deterministically ends in a quota error with
    // no iterator closing (`return()` is never called, even for the
    // implementation-generated part-count limit).
    assert_eval(
        context,
        r"
        (() => {
            globalThis.m9aClosed = false;
            var seq = { [Symbol.iterator]() {
                return {
                    next() { return { done: false, value: 'x' }; },
                    return() { globalThis.m9aClosed = true; return {}; },
                };
            } };
            try { new Blob(seq); return false; }
            catch (e) {
                return !globalThis.m9aClosed
                    && e instanceof RangeError
                    && /too many blob parts|exceeds/.test(e.message);
            }
        })()
        ",
    );
    // With conversion now preceding options parsing, a throwing iterator
    // still wins over a throwing options getter (earlier argument wins).
    assert_eval(
        context,
        r"
        (() => {
            function throwing() {
                var calls = 0;
                return { [Symbol.iterator]() {
                    return { next() {
                        calls++;
                        if (calls === 1) throw new TypeError('parts-boom');
                        return { done: true, value: undefined };
                    } };
                } };
            }
            try { new Blob(throwing(), { get type() { throw new RangeError('opt-boom'); } }); return false; }
            catch (e) { return e instanceof TypeError && e.message === 'parts-boom'; }
        })()
        ",
    );
}

// ──────────────────────────────────────────────
// M9A-IDL-03: BlobPart union fallback
// ──────────────────────────────────────────────

#[test]
fn m9a_idl_03_union_fallback_for_primitive_and_object_values() {
    let context = &mut setup();
    assert_eval(context, "new Blob([123]).size === 3");
    assert_eval(context, "new Blob([true]).size === 4");
    assert_eval(context, "new Blob([false]).size === 5");
    assert_eval(context, "new Blob([null]).size === 4");
    assert_eval(context, "new Blob([undefined]).size === 9");
    assert_eval(context, "new Blob([{}]).size === 15");
    // Object `toString` conversion is observable and shared by File.
    assert_eval(
        context,
        "new Blob([{ toString() { return 'obj'; } }]).size === 3",
    );
    assert_eval(context, "new File([123], 'n.txt').size === 3");
    assert_eval(
        context,
        "new File([{ toString() { return 'xy'; } }], 'n.txt').size === 2",
    );
    // Lone surrogates in converted values become U+FFFD (3 bytes).
    assert_eval(
        context,
        "new Blob([{ toString() { return '\\uD800'; } }]).size === 3",
    );
    // Real BufferSource still copies only the visible range.
    assert_eval(
        context,
        r"
        (() => {
            var buffer = new ArrayBuffer(8);
            var view = new Uint8Array(buffer, 2, 4);
            view.set([9, 10, 11, 12]);
            return new Blob([view]).size === 4;
        })()
        ",
    );
    // Nested Blob/File still compose without a copy-shaped size change.
    assert_eval(
        context,
        "new Blob([new Blob(['ab']), new File(['c'], 'c.txt')]).size === 3",
    );
    // A forged Blob-shaped object falls back to ToString, not the brand.
    assert_eval(
        context,
        r"
        (() => {
            var forged = { size: 9999, type: 'x', slice: Blob.prototype.slice };
            var blob = new Blob([forged]);
            return blob.size === 15;
        })()
        ",
    );
    // Symbol cannot take the USVString path here; BigInt stringifies.
    assert_eval_type_error(context, "new Blob([Symbol('x')])");
    assert_eval(context, "new Blob([10n]).size === 2");
}

// ──────────────────────────────────────────────
// M9A-TEXT-01: label → MIME charset → UTF-8 → BOM
// ──────────────────────────────────────────────

#[test]
fn m9a_text_01_shared_encoding_selection() {
    let context = &mut setup_worker();
    // Explicit label wins over the MIME charset.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var blob = new Blob([new Uint8Array([0xE9])], { type: 'text/plain;charset=utf-8' });
            return sync.readAsText(blob, 'windows-1252') === 'é';
        })()
        ",
    );
    // MIME charset is used when no label is present.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var blob = new Blob([new Uint8Array([0xE9])], { type: 'text/plain;charset=windows-1252' });
            return sync.readAsText(blob) === 'é';
        })()
        ",
    );
    // Unknown MIME charset falls back to UTF-8 with replacement.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var blob = new Blob([new Uint8Array([0xE9])], { type: 'text/plain;charset=bogus-charset' });
            return sync.readAsText(blob) === '�';
        })()
        ",
    );
    // Missing charset falls back to UTF-8.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            return sync.readAsText(new Blob(['héllo'])) === 'héllo';
        })()
        ",
    );
    // BOM overrides the fallback encoding (UTF-16LE BOM decodes as such).
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var blob = new Blob([new Uint8Array([0xFF, 0xFE, 0x41, 0x00])]);
            return sync.readAsText(blob) === 'A';
        })()
        ",
    );
    // UTF-8 BOM is stripped.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var blob = new Blob([new Uint8Array([0xEF, 0xBB, 0xBF, 0x41])]);
            return sync.readAsText(blob) === 'A';
        })()
        ",
    );
    // Malformed sequences become U+FFFD, never an EncodingError.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            return sync.readAsText(new Blob([new Uint8Array([0xFF])])) === '�';
        })()
        ",
    );
    // Unknown explicit label falls through to MIME/UTF-8 (never an
    // `EncodingError`): MIME charset wins when present, UTF-8 decoding
    // otherwise. (`M9A-RW-01`.)
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
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
    // Labels resolving to the `replacement` encoding decode per byte to
    // U+FFFD (exact `get an encoding` semantics).
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            return sync.readAsText(new Blob([new Uint8Array([0x41])]), 'csiso2022kr') === '�';
        })()
        ",
    );
}

#[test]
fn m9a_text_01_sync_matches_async_packaging() {
    let context = &mut setup_worker();
    assert_eval(
        context,
        r"
        (() => {
            globalThis.m9aAsync = null;
            var blob = new Blob([new Uint8Array([0xE9])], { type: 'text/plain;charset=windows-1252' });
            var reader = new FileReader();
            reader.onload = function () { globalThis.m9aAsync = this.result; };
            reader.readAsText(blob);
            var sync = new FileReaderSync().readAsText(blob);
            return sync === 'é';
        })()
        ",
    );
    context
        .eval(Source::from_bytes("globalThis.m9aAsync"))
        .expect("async slot");
    // Pump the FileReading queue; the async result must equal sync.
    let _ = context.run_jobs();
    assert_eval(context, "globalThis.m9aAsync === 'é'");
}

// ──────────────────────────────────────────────
// M9A-RW-02: BOM overrides any fallback encoding (`new_decoder()`
// sniffing), including an explicit label
// ──────────────────────────────────────────────

#[test]
fn m9a_rw_02_bom_overrides_explicit_and_other_fallbacks() {
    let context = &mut setup_worker();
    // Sync matrix: BOM wins over explicit windows-1252 and over the
    // UTF-8/MIME fallbacks; without a BOM the explicit label applies.
    assert_eval(
        context,
        r"
        (() => {
            var sync = new FileReaderSync();
            var utf8Bom = [0xEF, 0xBB, 0xBF];
            var leBom = [0xFF, 0xFE];
            var beBom = [0xFE, 0xFF];
            // UTF-8 BOM + explicit windows-1252 => BOM removed, UTF-8 payload.
            if (sync.readAsText(
                    new Blob([new Uint8Array(utf8Bom.concat([0x41]))]), 'windows-1252') !== 'A') return false;
            // UTF-16LE BOM + explicit windows-1252 => UTF-16LE payload.
            if (sync.readAsText(
                    new Blob([new Uint8Array(leBom.concat([0x41, 0x00]))]), 'windows-1252') !== 'A') return false;
            // UTF-16BE BOM + explicit windows-1252 => UTF-16BE payload.
            if (sync.readAsText(
                    new Blob([new Uint8Array(beBom.concat([0x00, 0x41]))]), 'windows-1252') !== 'A') return false;
            // No BOM + explicit windows-1252 => windows-1252 payload.
            if (sync.readAsText(
                    new Blob([new Uint8Array([0xE9])]), 'windows-1252') !== 'é') return false;
            return true;
        })()
        ",
    );
    // Split BOM after byte 1 (chunk 1 ends with lone EF) and after
    // byte 2: the decoder never emits a spurious U+FFFD for the partial
    // BOM lead — it buffers across the chunk edge — and sync/async agree
    // byte-for-byte. The BOM lands at offset 16383, so chunk 1 (16 KiB)
    // ends with EF and chunk 2 starts with BB BF 42.
    let chunked = &mut setup_chunked_worker();
    assert_eval(
        chunked,
        r"
        (() => {
            globalThis.m9aSplitAsync = null;
            globalThis.m9aSplitSync = null;
            var bytes = [];
            for (var i = 0; i < 16383; i++) bytes.push(0x41);
            var full = bytes.concat([0xEF, 0xBB, 0xBF, 0x42]);
            var blob = new Blob([new Uint8Array(full)]);
            var reader = new FileReader();
            reader.onload = function () { globalThis.m9aSplitAsync = this.result; };
            reader.readAsText(blob);
            globalThis.m9aSplitSync = new FileReaderSync().readAsText(blob);
            return typeof globalThis.m9aSplitSync === 'string';
        })()
        ",
    );
    let _ = chunked.run_jobs();
    let _ = chunked.run_jobs();
    assert_eval(
        chunked,
        "globalThis.m9aSplitAsync === globalThis.m9aSplitSync",
    );
    assert_eval(
        chunked,
        "typeof globalThis.m9aSplitAsync === 'string' && globalThis.m9aSplitAsync.length === 16385",
    );
}

#[test]
fn m9a_rw_02_bom_override_matches_async_path() {
    let context = &mut setup_worker();
    assert_eval(
        context,
        r"
        (() => {
            globalThis.m9aBomAsync = null;
            var blob = new Blob([new Uint8Array([0xFF, 0xFE, 0x41, 0x00])]);
            var reader = new FileReader();
            reader.onload = function () { globalThis.m9aBomAsync = this.result; };
            reader.readAsText(blob, 'windows-1252');
            var sync = new FileReaderSync().readAsText(blob, 'windows-1252');
            return sync === 'A';
        })()
        ",
    );
    let _ = context.run_jobs();
    assert_eval(context, "globalThis.m9aBomAsync === 'A'");
}

// ──────────────────────────────────────────────
// M9A-RW-03/04: Web IDL argument conversion order
// ──────────────────────────────────────────────

#[test]
fn m9a_rw_03_arguments_convert_left_to_right() {
    let context = &mut setup();
    // throwing `fileBits[Symbol.iterator]` is observed before throwing
    // `fileName`.
    assert_eval(
        context,
        r"
        (() => {
            var bits = { get [Symbol.iterator]() { throw new TypeError('bits-boom'); } };
            var name = { toString() { throw new TypeError('name-boom'); } };
            try { new File(bits, name); return false; }
            catch (e) { return e instanceof TypeError && e.message === 'bits-boom'; }
        })()
        ",
    );
    // Throwing element `toString` never reads `fileName` or options.
    assert_eval(
        context,
        r"
        (() => {
            var log = [];
            var bits = [{ toString() { log.push('part'); throw new Error('part-boom'); } }];
            var name = { toString() { log.push('name'); return 'n'; } };
            var options = { get endings() { log.push('endings'); return 'transparent'; } };
            try { new File(bits, name, options); return false; }
            catch (e) {
                return e.message === 'part-boom' && log.join(',') === 'part';
            }
        })()
        ",
    );
    // After a successful sequence, throwing `fileName` is observed
    // before any options getter.
    assert_eval(
        context,
        r"
        (() => {
            var log = [];
            var name = { toString() { log.push('name'); throw new TypeError('name-boom'); } };
            var options = { get endings() { log.push('endings'); return 'transparent'; } };
            try { new File(['x'], name, options); return false; }
            catch (e) {
                return e instanceof TypeError && e.message === 'name-boom' && log.join(',') === 'name';
            }
        })()
        ",
    );
    // Blob converts parts fully before `options.endings`/`options.type`.
    assert_eval(
        context,
        r"
        (() => {
            var log = [];
            var bits = [{ toString() { log.push('part'); return 'p'; } }];
            var options = {
                get endings() { log.push('endings'); return 'transparent'; },
                get type() { log.push('type'); return ''; },
            };
            var blob = new Blob(bits, options);
            return blob.size === 1 && log.join(',') === 'part,endings,type';
        })()
        ",
    );
    // Element `toString` side effects complete before `fileName.toString`.
    assert_eval(
        context,
        r"
        (() => {
            var log = [];
            var bits = [{ toString() { log.push('part'); return 'p'; } }];
            var name = { toString() { log.push('name'); return 'n'; } };
            var file = new File(bits, name);
            return file.size === 1 && file.name === 'n' && log.join(',') === 'part,name';
        })()
        ",
    );
    // The earlier argument's exception is final, never replaced by a
    // later argument's exception (options getter would throw `opt-boom`
    // if reached).
    assert_eval(
        context,
        r"
        (() => {
            var bits = [{ toString() { throw new Error('part-boom'); } }];
            var options = { get type() { throw new Error('opt-boom'); } };
            try { new Blob(bits, options); return false; }
            catch (e) { return e.message === 'part-boom'; }
        })()
        ",
    );
}

#[test]
fn m9a_rw_04_conversion_snapshots_before_later_side_effects() {
    let context = &mut setup();
    // BufferSource bytes are copied at element-conversion time: a
    // `fileName.toString` mutating the buffer cannot change them.
    // (FileReaderSync is worker-only, so the Window-context check from
    // the first draft is replaced by a byte-content assertion.)
    assert_eval(
        context,
        r"
        (() => {
            var view = new Uint8Array([1, 2, 3]);
            var bits = [view];
            var name = { toString() { view[0] = 99; return 'n.txt'; } };
            var file = new File(bits, name);
            if (file.size !== 3 || file.name !== 'n.txt') return false;
            return file.slice(0, 1).size === 1;
        })()
        ",
    );
    // Same guarantee through an options getter for Blob.
    assert_eval(
        context,
        r"
        (() => {
            var view = new Uint8Array([4, 5]);
            var options = { get type() { view[0] = 99; return ''; } };
            var blob = new Blob([view], options);
            return blob.size === 2;
        })()
        ",
    );
    // Dictionary members convert in Web IDL order: throwing `endings`
    // is observed before `type` is read.
    assert_eval(
        context,
        r"
        (() => {
            var log = [];
            var options = {
                get endings() { log.push('endings'); throw new TypeError('endings-boom'); },
                get type() { log.push('type'); return ''; },
            };
            try { new Blob(['x'], options); return false; }
            catch (e) {
                return e instanceof TypeError && e.message === 'endings-boom'
                    && log.join(',') === 'endings';
            }
        })()
        ",
    );
}

#[test]
fn m9a_reg_01_same_identity_is_idempotent() {
    let mut context = Context::default();
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    let first = extension.register(&mut context).expect("first register");
    // Cloning preserves identity: repeat register reuses state.
    let clone = extension.clone();
    let second = clone.register(&mut context).expect("same identity");
    // Both handles observe the same registered state.
    assert_eval(
        &mut context,
        "typeof Blob === 'function' && typeof File === 'function'",
    );
    assert!(first.blob_urls_empty() && second.blob_urls_empty());
    drop((first, second));
}

#[test]
fn m9a_reg_01_different_identity_is_rejected_without_mutation() {
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("first register");
    // A separately built extension — even visually identical — is rejected.
    let other = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    assert!(matches!(
        other.register(&mut context),
        Err(RegisterError::AlreadyRegistered)
    ));
    assert_eval(
        &mut context,
        "typeof Blob === 'function' && typeof File === 'function'",
    );
}

#[test]
fn m9a_reg_01_repeat_after_shutdown_never_revives() {
    let mut context = Context::default();
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    let handle = extension.register(&mut context).expect("register");
    handle.shutdown(&mut context).expect("shutdown");
    // Same identity after shutdown: existing handle returned, still shut.
    let again = extension
        .register(&mut context)
        .expect("same identity after shutdown");
    assert!(again.blob_urls_empty());
    let rejected = again
        .blob_from_bytes(bytes::Bytes::from_static(b"x"), "", &mut context)
        .is_err();
    assert!(rejected, "shut-down handle must stay shut");
    // Different identity after shutdown: rejected, never revived.
    let other = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    assert!(matches!(
        other.register(&mut context),
        Err(RegisterError::AlreadyRegistered)
    ));
}

#[test]
fn m9a_reg_01_contexts_stay_independent() {
    let mut first = Context::default();
    let mut second = Context::default();
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    extension.register(&mut first).expect("first context");
    extension
        .register(&mut second)
        .expect("same identity on another context");
    assert_eval(&mut first, "typeof Blob === 'function'");
    assert_eval(&mut second, "typeof Blob === 'function'");
}

// ──────────────────────────────────────────────
// M9A-FLIST-01 + M9A-RW-06: indexed-getter iterator surface
// (only `Symbol.iterator`, aliased to `%Array.prototype.values%`)
// ──────────────────────────────────────────────

#[test]
fn m9a_flist_01_indexed_getter_supplies_only_value_iterator() {
    let mut context = Context::default();
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    let handle = extension.register(&mut context).expect("register");
    let files: Vec<boa_engine::object::JsObject> = ["a.txt", "b.txt", "c.txt"]
        .iter()
        .map(|name| {
            handle
                .file_from_bytes(
                    bytes::Bytes::from_static(b"x"),
                    name,
                    boa_fapi::HostFileOptions::default(),
                    &mut context,
                )
                .expect("host file")
        })
        .collect();
    let list = handle.file_list(files, &mut context).expect("file list");
    context
        .register_global_property(
            boa_engine::js_string!("m9aList"),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish list");
    // Normative surface: length, item(index), indexed own properties,
    // descriptors, order, out-of-range `null` (method) / `undefined`
    // (indexed access), brand, no public constructor, and exactly the
    // indexed-getter `Symbol.iterator` — no entries/keys/values/forEach.
    assert_eval(
        &mut context,
        r"
        (() => {
            var list = globalThis.m9aList;
            if (list.length !== 3) return false;
            if (list.item(0).name !== 'a.txt') return false;
            if (list.item(2).name !== 'c.txt') return false;
            if (list[0].name !== 'a.txt' || list[1].name !== 'b.txt') return false;
            if (list.item(3) !== null || list.item(4294967295) !== null) return false;
            if (list[3] !== undefined || list[4294967295] !== undefined) return false;
            if (typeof list.item(3) !== 'object') return false;
            return true;
        })()
        ",
    );
    assert_eval(
        &mut context,
        r"
        (() => {
            var desc0 = Object.getOwnPropertyDescriptor(globalThis.m9aList, '0');
            if (!desc0 || desc0.writable !== false || desc0.enumerable !== true || desc0.configurable !== false) return false;
            if (Object.keys(globalThis.m9aList).join(',') !== '0,1,2') return false;
            if (typeof FileList !== 'undefined') return false;
            // The required indexed-getter iterator only.
            if (!(Symbol.iterator in globalThis.m9aList)) return false;
            if (globalThis.m9aList[Symbol.iterator] !== Array.prototype.values) return false;
            return true;
        })()
        ",
    );
    assert_eval(
        &mut context,
        r"
        (() => {
            // Iteration yields Files in indexed order and ends at length.
            var names = [];
            for (var file of globalThis.m9aList) names.push(file.name);
            if (names.join(',') !== 'a.txt,b.txt,c.txt') return false;
            if ([...globalThis.m9aList].length !== 3) return false;
            // Descriptor of the alias itself.
            var desc = Object.getOwnPropertyDescriptor(
                Object.getPrototypeOf(globalThis.m9aList), Symbol.iterator);
            if (!desc || desc.writable !== true || desc.enumerable !== false
                || desc.configurable !== true) return false;
            // No extra iterator helpers on the prototype or instance.
            var proto = Object.getPrototypeOf(globalThis.m9aList);
            if ('entries' in proto || 'keys' in proto || 'values' in proto
                || 'forEach' in proto) return false;
            if ('entries' in globalThis.m9aList || 'keys' in globalThis.m9aList
                || 'values' in globalThis.m9aList || 'forEach' in globalThis.m9aList) return false;
            if (Object.prototype.toString.call(globalThis.m9aList) !== '[object FileList]') return false;
            return true;
        })()
        ",
    );
}
