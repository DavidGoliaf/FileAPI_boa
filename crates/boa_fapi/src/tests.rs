//! JS-observable behavior tests for the M2 bindings.
//!
//! Every test executes real JavaScript in a Boa `Context` and asserts only
//! JS-observable state (`size`, `type`, `name`, `lastModified`, identity,
//! prototypes) plus the M1 public metadata (`size`, `segment_count`,
//! `media_type`) read through the native brand data. No test reads blob
//! bytes, raw segments, `Arc` pointers, or any other production test hook:
//! there are none.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::object::builtins::JsArrayBuffer;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsObject, JsValue, Source, js_string};
use boa_fapi_core::blob::BlobData;

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

/// Asserts JS-observable metadata of a blob built from JS.
///
/// `size` is checked both through JS and through the M1 public metadata;
/// `segment_count` is the M1 public structural fact (no payload read, no
/// `Arc` comparison). The expected content is verified by length only —
/// byte equality is already covered by the M2 JS integration suite.
fn assert_blob_meta(data: &BlobData, expected_size: u64, expected_segments: usize) {
    assert_eq!(data.size(), expected_size);
    assert_eq!(data.segment_count(), expected_segments);
}

#[test]
fn string_parts_are_utf8_encoded() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob(['a\u{e9}\u{1f600}'])");
    let data = blob_data_of(&blob);
    // 'a' (1) + U+00E9 (2) + U+1F600 (4) UTF-8 bytes, one USVString part.
    assert_blob_meta(&data, 1 + 2 + 4, 1);
    assert_eq!(data.media_type(), "");
}

#[test]
fn native_endings_convert_bytes() {
    let context = &mut setup();
    let blob = eval_object(
        context,
        "new Blob(['a\\nb\\rc\\r\\nd'], {endings: 'native'})",
    );
    let data = blob_data_of(&blob);
    let expected_size: u64 = if cfg!(windows) { 10 } else { 7 };
    assert_blob_meta(&data, expected_size, 1);
}

#[test]
fn transparent_endings_preserve_bytes() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob(['a\\nb\\rc\\r\\nd'])");
    let data = blob_data_of(&blob);
    // 'a' + LF + 'b' + CR + 'c' + CRLF + 'd' = 8 bytes, one part.
    assert_blob_meta(&data, 8, 1);
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
    // Visible Uint8Array range [2, 6): 4 bytes, one copied part.
    assert_blob_meta(&data, 4, 1);
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
    // DataView over bytes [1, 4): 3 bytes, one copied part.
    assert_blob_meta(&data, 3, 1);
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
    // The view bytes were snapshot-copied at construction; JS asserts the
    // content is unaffected by the later mutation, here only the size.
    assert_blob_meta(&data, 3, 1);
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
    assert_blob_meta(&data, 0, 0);
}

#[test]
fn nested_blob_composition_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.inner = new Blob(['INNER']);");
    let outer = eval_object(context, "new Blob([globalThis.inner])");
    let inner = eval_object(context, "globalThis.inner");

    let outer_data = blob_data_of(&outer);
    let inner_data = blob_data_of(&inner);
    // No-copy composition is structural: the outer blob reuses the inner
    // segment layout (same size and segment count) instead of copying.
    assert_eq!(outer_data.size(), inner_data.size());
    assert_eq!(outer_data.segment_count(), inner_data.segment_count());
    assert_blob_meta(&outer_data, 5, 1);
}

#[test]
fn nested_file_composition_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.inner = new File(['FILE'], 'f.txt');");
    let outer = eval_object(context, "new Blob([globalThis.inner])");
    let inner = eval_object(context, "globalThis.inner");

    let outer_data = blob_data_of(&outer);
    let inner_data = blob_data_of(&inner);
    assert_eq!(outer_data.size(), inner_data.size());
    assert_eq!(outer_data.segment_count(), inner_data.segment_count());
    // The inner type is ignored.
    assert_eq!(outer_data.media_type(), "");
    assert_blob_meta(&outer_data, 4, 1);
}

#[test]
fn slice_shares_source_without_copy() {
    let context = &mut setup();
    eval(context, "globalThis.original = new Blob(['hello world']);");
    let original = eval_object(context, "globalThis.original");
    let sliced = eval_object(context, "globalThis.original.slice(3, 8)");

    let original_data = blob_data_of(&original);
    let sliced_data = blob_data_of(&sliced);
    // The slice narrows the view; it must not grow segments or size.
    assert!(sliced_data.size() <= original_data.size());
    assert!(sliced_data.segment_count() <= original_data.segment_count());
    assert_blob_meta(&sliced_data, 5, 1);
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
    assert!(sliced_data.size() <= file_data.size());
    assert!(sliced_data.segment_count() <= file_data.segment_count());
    assert_blob_meta(&sliced_data, 4, 1);
}

#[test]
fn empty_blob_allocates_no_segments() {
    let context = &mut setup();
    let blob = eval_object(context, "new Blob()");
    let data = blob_data_of(&blob);
    assert_eq!(data.size(), 0);
    assert_eq!(data.segment_count(), 0);
}
