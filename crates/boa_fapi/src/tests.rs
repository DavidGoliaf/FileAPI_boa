//! Unit tests that inspect native data behind JS-created objects.
//!
//! These tests execute real JavaScript in a Boa `Context` and then verify
//! internal byte content and `Arc` sharing through child-module access to
//! the private native data. No production test hooks exist for this.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::object::builtins::JsArrayBuffer;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsObject, JsValue, Source, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::cancellation::CancellationToken;

use crate::blob::BlobNative;
use crate::clock::Clock;
use crate::extension::FileApiExtension;
use crate::file::FileNative;

/// Deterministic clock for tests.
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
    let extension = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build();
    extension
        .register(&mut context)
        .expect("registration failed");
    context
}

fn eval(context: &mut Context, source: &str) -> JsValue {
    context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"))
}

fn eval_object(context: &mut Context, source: &str) -> JsObject {
    eval(context, source)
        .as_object()
        .unwrap_or_else(|| panic!("expected an object from {source}"))
}

/// Reads the full byte content of a blob through the core model.
fn read_all(data: &BlobData) -> Vec<u8> {
    let cancel = CancellationToken::new();
    data.read_all(&cancel)
        .unwrap_or_else(|error| panic!("read failed: {error}"))
        .to_vec()
}

/// Extracts the blob payload of a JS object (Blob or File).
fn blob_data_of(object: &JsObject) -> Arc<BlobData> {
    if let Some(native) = object.downcast_ref::<BlobNative>() {
        return native.blob_data().clone();
    }
    if let Some(native) = object.downcast_ref::<FileNative>() {
        return native.blob_data().clone();
    }
    panic!("object carries no Blob brand");
}

/// Asserts that the first segment of two blobs shares its source allocation.
///
/// Uses the core identity probe instead of raw segments, so the public API
/// stays free of segment accessors.
fn assert_first_segment_shared(outer: &BlobData, inner: &BlobData) {
    assert!(outer.size() > 0, "expected a non-empty blob");
    assert!(
        outer.first_segment_shares_source_with(inner),
        "expected shared first-segment source"
    );
}

#[test]
fn string_parts_are_utf8_encoded() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob(['a\u{e9}\u{1f600}'])");
    let data = blob_data_of(&blob);
    assert_eq!(data.size(), 1 + 2 + 4);
    assert_eq!(read_all(&data), "a\u{e9}\u{1f600}".as_bytes());
}

#[test]
fn native_endings_convert_bytes() {
    let context = &mut setup();
    let blob = eval_object(
        context,
        "new Blob(['a\\nb\\rc\\r\\nd'], {endings: 'native'})",
    );
    let data = blob_data_of(&blob);
    let expected = if cfg!(windows) {
        "a\r\nb\r\nc\r\nd"
    } else {
        "a\nb\nc\nd"
    };
    assert_eq!(read_all(&data), expected.as_bytes());
}

#[test]
fn transparent_endings_preserve_bytes() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob(['a\\nb\\rc\\r\\nd'])");
    let data = blob_data_of(&blob);
    assert_eq!(read_all(&data), b"a\nb\rc\r\nd");
}

#[test]
fn buffer_source_copies_visible_range() {
    let context = &mut setup();
    let blob = eval_object(
        context,
        r"
        (() => {
            const buffer = new ArrayBuffer(8);
            const view = new Uint8Array(buffer, 2, 4);
            view.set([9, 10, 11, 12]);
            return new Blob([view]);
        })()
        ",
    );
    let data = blob_data_of(&blob);
    assert_eq!(read_all(&data), &[9, 10, 11, 12]);
}

#[test]
fn data_view_copies_visible_range() {
    let context = &mut setup();
    let blob = eval_object(
        context,
        r"
        (() => {
            const buffer = new ArrayBuffer(8);
            new Uint8Array(buffer).set([1, 2, 3, 4, 5, 6], 0);
            const view = new DataView(buffer, 1, 3);
            return new Blob([view]);
        })()
        ",
    );
    let data = blob_data_of(&blob);
    assert_eq!(read_all(&data), &[2, 3, 4]);
}

#[test]
fn post_construction_mutation_cannot_change_blob() {
    let context = &mut setup();
    let blob = eval_object(
        context,
        r"
        (() => {
            const view = new Uint8Array([1, 2, 3]);
            const blob = new Blob([view]);
            view[0] = 99;
            return blob;
        })()
        ",
    );
    let data = blob_data_of(&blob);
    assert_eq!(read_all(&data), &[1, 2, 3]);
}

#[test]
fn detached_buffer_copies_empty_sequence() {
    let context = &mut setup();
    let buffer_object = eval_object(context, "new ArrayBuffer(4)");
    let buffer = JsArrayBuffer::from_object(buffer_object).expect("array buffer");
    buffer.detach(&JsValue::undefined()).expect("detach");
    context
        .register_global_property(js_string!("detachedBuffer"), buffer, Attribute::all())
        .expect("register");
    let blob = eval_object(context, "new Blob([detachedBuffer])");
    let data = blob_data_of(&blob);
    assert_eq!(data.size(), 0);
    assert!(read_all(&data).is_empty());
}

#[test]
fn nested_blob_composition_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.inner = new Blob(['INNER']);");
    let outer = eval_object(context, "new Blob([globalThis.inner])");
    let inner = eval_object(context, "globalThis.inner");

    let outer_data = blob_data_of(&outer);
    let inner_data = blob_data_of(&inner);
    assert_first_segment_shared(&outer_data, &inner_data);
    assert_eq!(read_all(&outer_data), b"INNER");
}

#[test]
fn nested_file_composition_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.inner = new File(['FILE'], 'f.txt');");
    let outer = eval_object(context, "new Blob([globalThis.inner])");
    let inner = eval_object(context, "globalThis.inner");

    let outer_data = blob_data_of(&outer);
    let inner_data = blob_data_of(&inner);
    assert_first_segment_shared(&outer_data, &inner_data);
    // The inner type is ignored.
    assert_eq!(outer_data.media_type(), "");
    assert_eq!(read_all(&outer_data), b"FILE");
}

#[test]
fn slice_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.original = new Blob(['hello world']);");
    let original = eval_object(context, "globalThis.original");
    let sliced = eval_object(context, "globalThis.original.slice(3, 8)");

    let original_data = blob_data_of(&original);
    let sliced_data = blob_data_of(&sliced);
    assert_first_segment_shared(&sliced_data, &original_data);
    assert_eq!(sliced_data.size(), 5);
    assert_eq!(read_all(&sliced_data), b"lo wo");
}

#[test]
fn file_slice_is_a_plain_blob() {
    let context = &mut setup();
    eval(context, "globalThis.file = new File(['hello'], 'f.txt');");
    let file = eval_object(context, "globalThis.file");
    let sliced = eval_object(context, "globalThis.file.slice(1)");

    assert!(sliced.downcast_ref::<FileNative>().is_none());
    assert!(sliced.downcast_ref::<BlobNative>().is_some());

    let file_data = blob_data_of(&file);
    let sliced_data = blob_data_of(&sliced);
    assert_first_segment_shared(&sliced_data, &file_data);
    assert_eq!(read_all(&sliced_data), b"ello");
}

#[test]
fn empty_blob_allocates_no_segments() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob()");
    let data = blob_data_of(&blob);
    assert_eq!(data.size(), 0);
    assert_eq!(data.segment_count(), 0);
}
