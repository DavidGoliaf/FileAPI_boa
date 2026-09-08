//! M2 integration tests: Blob, File and FileList in a real Boa Context.
//!
//! Every test registers the extension into a clean `Context` and executes
//! JavaScript, so the exercised surface is the real JS API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{Clock, FileApiExtension, FileApiHandle, HostFileOptions, RegisterError};

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

/// Creates a clean context with the extension registered.
fn setup() -> (Context, FileApiHandle) {
    let mut context = Context::default();
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    let handle = extension
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn setup_with_limits(max_blob_size: u64) -> Context {
    let mut context = Context::default();
    // Narrow per-operation fixture: only the blob ceiling is tightened.
    // The remaining ceilings keep defaults that satisfy the whole-config
    // `validate()` (sync <= materialize, chunk <= materialize,
    // materialize <= blob), so registration succeeds and the blob-size
    // limit is enforced per operation by `from_segments`/`slice`.
    // (Requires max_blob_size >= 64 KiB so the default chunk fits.)
    assert!(
        max_blob_size >= 64 * 1024,
        "fixture blob ceiling must fit the default chunk size"
    );
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_blob_size,
        max_materialize_bytes: max_blob_size,
        max_sync_read_bytes: max_blob_size.min(32 * 1024 * 1024),
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .build();
    extension
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

/// Evaluates `source` and asserts that it throws a `TypeError`.
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
// 1. Registration
// ──────────────────────────────────────────────

#[test]
fn registration_installs_globals() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "typeof Blob === 'function' && typeof File === 'function'",
    );
    // FileList has no public constructor or global name.
    assert_eval(&mut context, "typeof FileList === 'undefined'");
}

#[test]
fn constructor_descriptors_and_metadata() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blobDesc = Object.getOwnPropertyDescriptor(globalThis, 'Blob');
        blobDesc.writable === true && blobDesc.enumerable === false && blobDesc.configurable === true
        && Object.getOwnPropertyDescriptor(globalThis, 'File').configurable === true
        && Blob.name === 'Blob' && File.name === 'File'
        && Blob.length === 0 && File.length === 2
        && Blob.prototype.slice.length === 0 && Blob.prototype.slice.name === 'slice'
        && Blob.prototype.text.length === 0 && Blob.prototype.arrayBuffer.length === 0
        && Blob.prototype.bytes.length === 0
        ",
    );
}

#[test]
fn prototype_links() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        Blob.prototype.constructor === Blob
        && File.prototype.constructor === File
        && Object.getPrototypeOf(File.prototype) === Blob.prototype
        && Object.getPrototypeOf(Blob.prototype) === Object.prototype
        && Object.getPrototypeOf(File) === Function.prototype
        ",
    );
}

#[test]
fn prototype_member_descriptors() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var sizeDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'size');
        var sliceDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'slice');
        var textDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'text');
        var arrayBufferDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'arrayBuffer');
        var bytesDesc = Object.getOwnPropertyDescriptor(Blob.prototype, 'bytes');
        sizeDesc.enumerable === true && sizeDesc.configurable === true
        && typeof sizeDesc.get === 'function' && sizeDesc.set === undefined
        && sliceDesc.writable === true && sliceDesc.enumerable === false && sliceDesc.configurable === true
        && sliceDesc.value.length === 0 && sliceDesc.value.name === 'slice'
        && textDesc.writable === true && textDesc.enumerable === false && textDesc.configurable === true
        && textDesc.value.length === 0 && textDesc.value.name === 'text'
        && arrayBufferDesc.writable === true && arrayBufferDesc.enumerable === false && arrayBufferDesc.configurable === true
        && arrayBufferDesc.value.length === 0 && arrayBufferDesc.value.name === 'arrayBuffer'
        && bytesDesc.writable === true && bytesDesc.enumerable === false && bytesDesc.configurable === true
        && bytesDesc.value.length === 0 && bytesDesc.value.name === 'bytes'
        ",
    );
}

#[test]
fn tostring_tags() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        Object.prototype.toString.call(new Blob()) === '[object Blob]'
        && Object.prototype.toString.call(new File([], 'f')) === '[object File]'
        && Object.prototype.toString.call(Blob.prototype) === '[object Blob]'
        ",
    );
}

#[test]
fn registration_is_rejected_when_already_registered() {
    let (mut context, _handle) = setup();
    let extension = FileApiExtension::builder().build();
    assert!(matches!(
        extension.register(&mut context),
        Err(RegisterError::AlreadyRegistered)
    ));
}

#[test]
fn registration_name_conflict_rolls_back_atomically() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("globalThis.Blob = 42;"))
        .expect("setup eval");

    let extension = FileApiExtension::builder().build();
    match extension.register(&mut context) {
        Err(RegisterError::NameConflict(name)) => assert_eq!(name, "Blob"),
        Err(other) => panic!("expected NameConflict, got {other:?}"),
        Ok(_) => panic!("expected NameConflict, got Ok"),
    }

    // None of the three globals may be installed after a failed registration.
    assert_eval(
        &mut context,
        "globalThis.Blob === 42 && typeof File === 'undefined'",
    );
}

#[test]
fn registration_file_name_conflict_detected() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("globalThis.FileList = 7;"))
        .expect("setup eval");
    let extension = FileApiExtension::builder().build();
    match extension.register(&mut context) {
        Err(RegisterError::NameConflict(name)) => assert_eq!(name, "FileList"),
        Err(other) => panic!("expected NameConflict, got {other:?}"),
        Ok(_) => panic!("expected NameConflict, got Ok"),
    }
}

#[test]
fn registration_fails_on_non_extensible_global() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes("Object.preventExtensions(globalThis);"))
        .expect("setup eval");
    let extension = FileApiExtension::builder().build();
    assert!(matches!(
        extension.register(&mut context),
        Err(RegisterError::GlobalNotExtensible)
    ));
}

// ──────────────────────────────────────────────
// 2. Brands
// ──────────────────────────────────────────────

#[test]
fn borrowed_getters_require_brand() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(&mut context, r"Object.create(Blob.prototype).size");
    assert_eval_type_error(
        &mut context,
        r"const get = Object.getOwnPropertyDescriptor(Blob.prototype, 'size').get; get.call({})",
    );
    assert_eval_type_error(&mut context, r"Blob.prototype.slice.call(123)");
    assert_eval_type_error(
        &mut context,
        r"const get = Object.getOwnPropertyDescriptor(Blob.prototype, 'type').get;
          get.call({ constructor: Blob, [Symbol.toStringTag]: 'Blob' })",
    );
}

#[test]
fn copied_public_properties_do_not_grant_brand() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(
        &mut context,
        r"
        var desc = Object.getOwnPropertyDescriptor(Blob.prototype, 'size');
        var copy = {};
        Object.defineProperty(copy, 'size', desc);
        copy.size
        ",
    );
}

#[test]
fn real_file_passes_blob_brand() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var file = new File(['abc'], 'f.txt');
        file.size === 3
        && file instanceof Blob
        && Blob.prototype.slice.call(file, 1).size === 2
        && Object.prototype.toString.call(Blob.prototype.slice.call(file)) === '[object Blob]'
        ",
    );
}

#[test]
fn subclassing_keeps_brand() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        class MyBlob extends Blob {}
        var blob = new MyBlob(['x']);
        blob instanceof MyBlob && blob instanceof Blob && blob.size === 1
        && Object.prototype.toString.call(blob) === '[object Blob]'
        ",
    );
}

#[test]
fn file_brand_getters_reject_foreign_this() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(
        &mut context,
        r"Object.getOwnPropertyDescriptor(File.prototype, 'name').get.call({})",
    );
    assert_eval_type_error(
        &mut context,
        r"Object.getOwnPropertyDescriptor(File.prototype, 'lastModified').get.call([])",
    );
}

// ──────────────────────────────────────────────
// 3. Blob
// ──────────────────────────────────────────────

#[test]
fn empty_blob_defaults() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var empty = new Blob();
        var fromUndefined = new Blob(undefined);
        var fromEmptyArray = new Blob([]);
        empty.size === 0 && empty.type === ''
        && fromUndefined.size === 0 && fromEmptyArray.size === 0
        ",
    );
}

#[test]
fn blob_type_normalization_and_readonly() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "new Blob([], {type: 'TEXT/Plain'}).type === 'text/plain'",
    );
    assert_eval(
        &mut context,
        "new Blob([], {type: 'text/\\u0000plain'}).type === ''",
    );
    assert_eval_type_error(
        &mut context,
        "const blob = new Blob(); blob.type = 'text/plain'",
    );
}

#[test]
fn invalid_endings_throw() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(&mut context, "new Blob([], {endings: 'bogus'})");
    assert_eval_type_error(&mut context, "new Blob([], {endings: 'Native'})");
}

#[test]
fn throwing_options_getter_propagates() {
    let (mut context, _handle) = setup();
    let result = context
        .eval(Source::from_bytes(
            r#"
            (() => {
                try {
                    new Blob([], { get type() { throw new RangeError('boom'); } });
                    return 'no-throw';
                } catch (error) {
                    return error.message;
                }
            })()
            "#,
        ))
        .expect("eval failed");
    assert_eq!(
        result.as_string().map(|s| s.to_std_string_escaped()),
        Some("boom".to_owned())
    );
}

#[test]
fn string_parts_and_usv_replacement() {
    let (mut context, _handle) = setup();
    assert_eval(&mut context, "new Blob(['abc']).size === 3");
    // A lone surrogate becomes U+FFFD (3 UTF-8 bytes).
    assert_eval(&mut context, "new Blob(['\\uD800']).size === 3");
    assert_eval(&mut context, "new Blob(['a', 'b']).size === 2");
    // Web IDL union fallback: primitives/objects stringify via USVString.
    assert_eval(&mut context, "new Blob([123]).size === 3");
    assert_eval(&mut context, "new Blob([null]).size === 4");
    assert_eval(&mut context, "new Blob([undefined]).size === 9");
    assert_eval(&mut context, "new Blob([true]).size === 4");
    assert_eval(&mut context, "new Blob([{}]).size === 15");
    // Symbol/BigInt cannot convert via ToString for this union position.
    assert_eval_type_error(&mut context, "new Blob([Symbol('x')])");
    // Non-iterable parts fail.
    assert_eval_type_error(&mut context, "new Blob('abc')");
    assert_eval_type_error(&mut context, "new Blob(null)");
}

#[test]
fn native_endings_sizes() {
    let (mut context, _handle) = setup();
    // "a\nb\rc\r\nd": transparent keeps all 7 bytes; native converts every
    // line ending to the platform target (CRLF on Windows, LF elsewhere).
    assert_eval(&mut context, "new Blob(['a\\nb\\rc\\r\\nd']).size === 8");
    let expected_native = if cfg!(windows) { 10 } else { 7 };
    assert_eval(
        &mut context,
        &format!(
            "new Blob(['a\\nb\\rc\\r\\nd'], {{endings: 'native'}}).size === {expected_native}"
        ),
    );
}

#[test]
fn buffer_source_visible_ranges() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var buffer = new Uint8Array([1, 2, 3]).buffer;
        new Blob([buffer]).size === 3
        ",
    );
    // Only the visible view range is copied.
    assert_eval(
        &mut context,
        r"
        var buffer = new ArrayBuffer(8);
        var view = new Uint8Array(buffer, 2, 4);
        view.set([1, 2, 3, 4]);
        new Blob([view]).size === 4
        ",
    );
    // DataView as a part.
    assert_eval(
        &mut context,
        r"
        var buffer = new ArrayBuffer(8);
        var view = new DataView(buffer, 1, 3);
        new Blob([view]).size === 3
        ",
    );
    // Every typed array kind is a BufferSource.
    assert_eval(
        &mut context,
        r"
        var kinds = [
            new Int8Array(4), new Uint8Array(4), new Uint8ClampedArray(4),
            new Int16Array(2), new Uint16Array(2), new Int32Array(1),
            new Uint32Array(1), new Float32Array(2), new Float64Array(1),
            new BigInt64Array(1), new BigUint64Array(1),
        ];
        kinds.every(kind => new Blob([kind]).size === kind.byteLength)
        ",
    );
}

#[test]
fn detached_buffer_is_handled_without_panic() {
    // Full detach coverage lives in the `#[cfg(test)]` unit module, which
    // can call the engine's public detach API; here we only re-verify that a
    // zero-length view is a valid part.
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "new Blob([new Int8Array(2).subarray(0, 0)]).size === 0",
    );
}

#[test]
fn post_construction_mutation_is_invisible() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var view = new Uint8Array([1, 2, 3]);
        var blob = new Blob([view]);
        view[0] = 99;
        view.buffer[1] = 99;
        blob.size === 3
        ",
    );
}

#[test]
fn nested_blob_and_file_composition() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var inner = new Blob(['abc'], {type: 'text/plain'});
        var outer = new Blob([inner]);
        outer.size === 3 && outer.type === ''
        ",
    );
    assert_eval(
        &mut context,
        r"
        var file = new File(['abcd'], 'f.txt');
        var outer = new Blob([file, 'x']);
        outer.size === 5
        ",
    );
}

// ──────────────────────────────────────────────
// 4. Slice
// ──────────────────────────────────────────────

#[test]
fn slice_boundaries() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blob = new Blob(['hello world']);
        blob.slice().size === 11
        && blob.slice(undefined, undefined).size === 11
        && blob.slice(6).size === 5
        && blob.slice(6, 11).size === 5
        && blob.slice(0, 5).size === 5
        && blob.slice(3, 3).size === 0
        && blob.slice(5, 2).size === 0
        && blob.slice(100).size === 0
        && blob.slice(-5).size === 5
        && blob.slice(-100).size === 11
        && blob.slice(0, -6).size === 5
        && blob.slice(-1, -2).size === 0
        ",
    );
}

#[test]
fn slice_clamp_conversions() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blob = new Blob(['hello world']);
        blob.slice(2.5, 5.5).size === 4
        && blob.slice(2.4, 5.6).size === 4
        && blob.slice(NaN).size === 11
        && blob.slice(Infinity).size === 0
        && blob.slice(-Infinity).size === 11
        && blob.slice(1e300).size === 0
        && blob.slice(-1e300).size === 11
        && blob.slice(null).size === 11
        ",
    );
}

#[test]
fn slice_content_type() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blob = new Blob(['hello'], {type: 'text/plain'});
        blob.slice(0, 2).type === ''
        && blob.slice(0, 2, 'A/B').type === 'a/b'
        && blob.slice(0, 2, 'TEXT/\u0000BAD').type === ''
        ",
    );
}

#[test]
fn slice_extreme_values() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blob = new Blob(['hello']);
        blob.slice(Number.MAX_SAFE_INTEGER).size === 0
        && blob.slice(-Number.MAX_SAFE_INTEGER, 3).size === 3
        ",
    );
}

#[test]
fn slice_result_is_new_blob_not_file() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var file = new File(['hello'], 'f.txt');
        var sliced = file.slice(1);
        sliced instanceof Blob && !(sliced instanceof File)
        && Object.getPrototypeOf(sliced) === Blob.prototype
        && sliced.size === 4
        ",
    );
}

#[test]
fn sliced_source_stays_immutable() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var blob = new Blob(['hello']);
        var sliced = blob.slice(1, 3);
        blob.size === 5 && sliced.size === 2
        ",
    );
}

// ──────────────────────────────────────────────
// 5. File
// ──────────────────────────────────────────────

#[test]
fn file_name_conversions() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "new File([], 'report.txt').name === 'report.txt'",
    );
    assert_eval(&mut context, "new File([], 'a/b').name === 'a:b'");
    assert_eval(&mut context, "new File([], '/leading').name === ':leading'");
    // Lone surrogates become U+FFFD.
    assert_eval(&mut context, r"new File([], '\uD800x').name === '\uFFFDx'");
    // A regular USVString argument converts through ToString.
    assert_eval(&mut context, "new File([], 5).name === '5'");
}

#[test]
fn file_last_modified_default_uses_clock() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "new File([], 'f').lastModified === 1700000000000",
    );
    // Two files from the same registration read the same clock.
    assert_eval(
        &mut context,
        "new File([], 'a').lastModified === new File([], 'b').lastModified",
    );
}

#[test]
fn file_last_modified_supplied_conversion() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        new File([], 'a', {lastModified: 123}).lastModified === 123
        && new File([], 'b', {lastModified: 2.9}).lastModified === 2
        && new File([], 'c', {lastModified: -5.7}).lastModified === -5
        && new File([], 'd', {}).lastModified === 1700000000000
        ",
    );
}

#[test]
fn file_last_modified_ordinary_long_long_wrap() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        new File([], 'a', {lastModified: 18446744073709551616}).lastModified === 0
        && new File([], 'b', {lastModified: -1}).lastModified === -1
        ",
    );
}

#[test]
fn file_metadata_readonly() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(
        &mut context,
        "const file = new File([], 'a'); file.name = 'b'",
    );
    assert_eval_type_error(
        &mut context,
        "const file = new File([], 'a'); file.lastModified = 5",
    );
}

#[test]
fn file_type_and_endings() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var file = new File(['ab'], 'f', {type: 'TEXT/PLAIN', endings: 'native'});
        file.type === 'text/plain' && file.size === 2
        ",
    );
    assert_eval_type_error(&mut context, "new File([], 'f', {endings: 'nope'})");
}

#[test]
fn file_requires_two_arguments() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(&mut context, "new File()");
    assert_eval_type_error(&mut context, "new File([])");
    assert_eval_type_error(&mut context, "File(['x'], 'name')");
}

// ──────────────────────────────────────────────
// 6. FileList
// ──────────────────────────────────────────────

#[test]
fn file_list_order_identity_and_access() {
    let (mut context, handle) = setup();
    let f1 = eval_object(&mut context, "new File(['1'], 'a.txt')");
    let f2 = eval_object(&mut context, "new File(['22'], 'b.txt')");
    let list = handle
        .file_list([f1.clone(), f2.clone()], &mut context)
        .expect("file_list");

    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("list")),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("register list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("f0")),
            f1,
            boa_engine::property::Attribute::all(),
        )
        .expect("register f0");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("f1")),
            f2,
            boa_engine::property::Attribute::all(),
        )
        .expect("register f1");

    assert_eval(
        &mut context,
        r"
        list.length === 2
        && list[0] === f0 && list[1] === f1
        && list.item(0) === f0 && list.item(1) === f1
        && list.item(2) === null && list.item(999) === null
        && list[2] === undefined
        && Object.prototype.toString.call(list) === '[object FileList]'
        ",
    );
}

#[test]
fn file_list_indexed_descriptors() {
    let (mut context, handle) = setup();
    let f = eval_object(&mut context, "new File(['x'], 'a.txt')");
    let list = handle.file_list([f], &mut context).expect("file_list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("list")),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("register list");

    assert_eval(
        &mut context,
        r"
        var desc = Object.getOwnPropertyDescriptor(list, '0');
        desc.writable === false && desc.enumerable === true && desc.configurable === false
        && desc.value === list.item(0)
        ",
    );
}

#[test]
fn file_list_indexed_properties_are_readonly() {
    let (mut context, handle) = setup();
    let f = eval_object(&mut context, "new File(['x'], 'a.txt')");
    let other = eval_object(&mut context, "new File(['y'], 'b.txt')");
    let list = handle.file_list([f], &mut context).expect("file_list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("list")),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("register list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("other")),
            other,
            boa_engine::property::Attribute::all(),
        )
        .expect("register other");

    // Assignment is rejected in strict mode.
    assert_eval_type_error(&mut context, "list[0] = other");
    // Deletion fails for non-configurable properties.
    assert_eval(
        &mut context,
        "delete list[0] === false && list[0] !== other && list.length === 1",
    );
    assert_eval_type_error(
        &mut context,
        "Object.defineProperty(list, '0', {value: other})",
    );
    assert_eval_type_error(
        &mut context,
        "Object.defineProperty(list, '0', {configurable: true})",
    );
}

#[test]
fn file_list_brand_checks() {
    let (mut context, handle) = setup();
    let f = eval_object(&mut context, "new File(['x'], 'a.txt')");
    let list = handle.file_list([f], &mut context).expect("file_list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("list")),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("register list");

    // Borrowed methods and getters reject foreign `this`.
    assert_eval_type_error(&mut context, "list.item.call({}, 0)");
    assert_eval_type_error(
        &mut context,
        r"
        var lengthGet = Object.getOwnPropertyDescriptor(
            Object.getPrototypeOf(list), 'length'
        ).get;
        lengthGet.call({})
        ",
    );
}

fn eval_object(context: &mut Context, source: &str) -> boa_engine::JsObject {
    context
        .eval(Source::from_bytes(source))
        .expect("eval failed")
        .as_object()
        .expect("expected an object")
}

// The two brand tests above reference a helper; define it via globalThis.
// It is set by the test that needs it.

#[test]
fn file_list_length_conversion() {
    let (mut context, handle) = setup();
    let files: Vec<_> = (0..3)
        .map(|i| eval_object(&mut context, &format!("new File(['{i}'], 'f{i}.txt')")))
        .collect();
    let list = handle.file_list(files, &mut context).expect("file_list");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("list")),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("register list");
    assert_eval(
        &mut context,
        "list.length === 3 && list.item(4294967296) === list.item(0) && list.item(-1) === list.item(4294967295)",
    );
}

#[test]
fn host_file_list_rejects_non_file_elements() {
    let (mut context, handle) = setup();
    let blob = eval_object(&mut context, "new Blob(['x'])");
    let plain = eval_object(&mut context, "({})");
    assert!(
        handle.file_list([blob], &mut context).is_err(),
        "Blob elements must be rejected"
    );
    assert!(
        handle.file_list([plain], &mut context).is_err(),
        "plain objects must be rejected"
    );
}

#[test]
fn host_file_from_bytes() {
    let (mut context, handle) = setup();
    let file = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"data"),
            "host/path.txt",
            HostFileOptions {
                media_type: "TEXT/PLAIN".to_owned(),
                last_modified: None,
            },
            &mut context,
        )
        .expect("file_from_bytes");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("hostFile")),
            file,
            boa_engine::property::Attribute::all(),
        )
        .expect("register hostFile");

    assert_eval(
        &mut context,
        r"
        hostFile.name === 'host:path.txt'
        && hostFile.type === 'text/plain'
        && hostFile.lastModified === 1700000000000
        && hostFile.size === 4
        && hostFile instanceof File && hostFile instanceof Blob
        ",
    );
}

#[test]
fn host_blob_from_bytes() {
    let (mut context, handle) = setup();
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from_static(b"abc"),
            "TEXT/PLAIN",
            &mut context,
        )
        .expect("blob_from_bytes");
    context
        .register_global_property(
            boa_engine::property::PropertyKey::from(boa_engine::js_string!("hostBlob")),
            blob,
            boa_engine::property::Attribute::all(),
        )
        .expect("register hostBlob");
    assert_eval(
        &mut context,
        "hostBlob.size === 3 && hostBlob.type === 'text/plain' && hostBlob instanceof Blob",
    );
}

// ──────────────────────────────────────────────
// 7. Limits and hostile values
// ──────────────────────────────────────────────

#[test]
fn blob_size_limit_fails_synchronously() {
    let mut context = setup_with_limits(64 * 1024);
    let result = context.eval(Source::from_bytes(
        r"
        (() => {
            try {
                new Blob([new Uint8Array(64 * 1024 + 1).buffer]);
                return 'no-throw';
            } catch (error) {
                return error instanceof RangeError ? 'range' : error.name;
            }
        })()
        ",
    ));
    assert_eq!(
        result
            .expect("eval")
            .as_string()
            .map(|s| s.to_std_string_escaped()),
        Some("range".to_owned())
    );
    // The globals are still intact after the failure.
    assert_eval(&mut context, "typeof Blob === 'function'");
}

#[test]
fn hostile_values_never_panic() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        r"
        var hostile = [
            () => new Blob([Symbol('x')]),
            () => new File(['x'], {toString: null}),
        ];
        hostile.every(fn_must_throw => {
            try { fn_must_throw(); return false; } catch { return true; }
        })
        ",
    );
}

#[test]
fn slice_of_blob_limit_enforced() {
    let mut context = setup_with_limits(64 * 1024);
    let result = context.eval(Source::from_bytes(
        r"
        (() => {
            var blob = new Blob(['abcd']);
            try {
                // Slicing never grows a blob, so this must succeed.
                return blob.slice(0, 4).size === 4 ? 'ok' : 'size';
            } catch (error) {
                return 'threw: ' + error.name;
            }
        })()
        ",
    ));
    assert_eq!(
        result
            .expect("eval")
            .as_string()
            .map(|s| s.to_std_string_escaped()),
        Some("ok".to_owned())
    );
}

// ──────────────────────────────────────────────
// BigInt conversion errors
// ──────────────────────────────────────────────

#[test]
fn bigint_last_modified_throws() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(&mut context, "new File([], 'a', {lastModified: 5n})");
}

#[test]
fn symbol_slice_args_throw() {
    let (mut context, _handle) = setup();
    assert_eval_type_error(&mut context, "new Blob(['abc']).slice(Symbol('x'))");
}

// FileList has no public global name; its prototype is reachable only from
// instances created through the host handle.
#[test]
fn file_list_prototype_is_not_global() {
    let (mut context, _handle) = setup();
    assert_eval(&mut context, "typeof FileList === 'undefined'");
}
