//! Tests for BlobData: from_segments, slice, and immutability.

use std::sync::Arc;

use boa_fapi_core::blob::{BlobData, BlobSegment};
use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;
use boa_fapi_core::snapshot::SnapshotState;
use boa_fapi_core::source::ByteSource;
use boa_fapi_core::source::memory::MemorySource;
use bytes::Bytes;

fn limits() -> FileApiLimits {
    FileApiLimits::default()
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn read_blob(blob: &BlobData) -> Vec<u8> {
    blob.materialize(u64::MAX, &cancel())
        .expect("materialize failed")
}

fn make_source(data: &[u8]) -> Arc<dyn ByteSource> {
    Arc::new(MemorySource::new(Bytes::copy_from_slice(data)))
}

fn make_segment(source: Arc<dyn ByteSource>, offset: u64, len: u64) -> BlobSegment {
    BlobSegment {
        source,
        offset,
        len,
    }
}

// ──────────────────────────────────────────────
// BlobData::empty
// ──────────────────────────────────────────────

#[test]
fn empty_blob() {
    let blob = BlobData::empty("text/plain");
    assert_eq!(blob.size(), 0);
    assert_eq!(blob.media_type(), "text/plain");
    assert_eq!(blob.segment_count(), 0);
    assert_eq!(blob.snapshot(), &SnapshotState::Memory);
}

#[test]
fn empty_blob_normalizes_type() {
    let blob = BlobData::empty("TEXT/PLAIN");
    assert_eq!(blob.media_type(), "text/plain");
}

// ──────────────────────────────────────────────
// BlobData::from_segments
// ──────────────────────────────────────────────

#[test]
fn from_single_source() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    assert_eq!(blob.size(), 11);
    assert_eq!(blob.segment_count(), 1);
}

#[test]
fn from_multiple_sources() {
    let src1 = make_source(b"hello");
    let src2 = make_source(b" world");
    let seg1 = make_segment(src1, 0, 5);
    let seg2 = make_segment(src2, 0, 6);
    let blob = BlobData::from_segments(vec![seg1, seg2], "text/plain", &limits()).unwrap();
    assert_eq!(blob.size(), 11);
    assert_eq!(blob.segment_count(), 2);
}

#[test]
fn from_segments_with_offset() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 6, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    assert_eq!(blob.size(), 5);
    let content = read_blob(&blob);
    assert_eq!(&content, b"world");
}

#[test]
fn from_segments_zero_length_filtered() {
    let src = make_source(b"hello");
    let seg_empty = make_segment(src.clone(), 0, 0);
    let seg_full = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg_empty, seg_full], "text/plain", &limits()).unwrap();
    assert_eq!(blob.segment_count(), 1);
    assert_eq!(blob.size(), 5);
}

#[test]
fn from_segments_zero_length_invalid_offset_rejected() {
    let src = make_source(b"hello");
    // Zero-length segment with offset > source.len() must be rejected
    let seg_bad = BlobSegment {
        source: src,
        offset: 100,
        len: 0,
    };
    let result = BlobData::from_segments(vec![seg_bad], "text/plain", &limits());
    assert!(matches!(result, Err(FileApiError::InvalidRange)));
}

#[test]
fn materialize_respects_limit() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let cancel = CancellationToken::new();
    // 11 bytes > 10 byte limit
    let result = blob.materialize(10, &cancel);
    assert!(matches!(
        result,
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes
        ))
    ));
}

#[test]
fn materialize_within_limit() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let cancel = CancellationToken::new();
    let result = blob.materialize(5, &cancel).unwrap();
    assert_eq!(&result, b"hello");
}

#[test]
fn from_segments_invalid_offset() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 10, 5); // offset > source.len()
    let result = BlobData::from_segments(vec![seg], "text/plain", &limits());
    assert!(matches!(result, Err(FileApiError::InvalidRange)));
}

#[test]
fn from_segments_invalid_end() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 2, 10); // offset + len > source.len()
    let result = BlobData::from_segments(vec![seg], "text/plain", &limits());
    assert!(matches!(result, Err(FileApiError::InvalidRange)));
}

#[test]
fn from_segments_offset_overflow() {
    let src = make_source(b"hello");
    let seg = BlobSegment {
        source: src,
        offset: u64::MAX,
        len: 1,
    };
    let result = BlobData::from_segments(vec![seg], "text/plain", &limits());
    assert!(matches!(result, Err(FileApiError::InvalidRange)));
}

#[test]
fn from_segments_size_exactly_at_limit() {
    let mut limits = limits();
    limits.max_blob_size = 11;
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    assert!(BlobData::from_segments(vec![seg], "text/plain", &limits).is_ok());
}

#[test]
fn from_segments_size_one_over_limit() {
    let mut limits = limits();
    limits.max_blob_size = 10;
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    assert!(matches!(
        BlobData::from_segments(vec![seg], "text/plain", &limits),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))
    ));
}

#[test]
fn from_segments_count_exactly_at_limit() {
    let mut limits = limits();
    limits.max_segments_after_normalize = 2;
    let src = make_source(b"abc");
    let seg1 = make_segment(src.clone(), 0, 1);
    let seg2 = make_segment(src, 1, 1);
    assert!(BlobData::from_segments(vec![seg1, seg2], "text/plain", &limits).is_ok());
}

#[test]
fn from_segments_count_one_over_limit() {
    let mut limits = limits();
    limits.max_segments_after_normalize = 1;
    let src = make_source(b"abc");
    let seg1 = make_segment(src.clone(), 0, 1);
    let seg2 = make_segment(src, 1, 1);
    assert!(matches!(
        BlobData::from_segments(vec![seg1, seg2], "text/plain", &limits),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments))
    ));
}

#[test]
fn from_segments_normalizes_mime_type() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "TEXT/PLAIN", &limits()).unwrap();
    assert_eq!(blob.media_type(), "text/plain");
}

// ──────────────────────────────────────────────
// Immutability tests
// ──────────────────────────────────────────────

#[test]
fn blob_data_immutable_repeated_reads() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();

    let first = read_blob(&blob);
    let second = read_blob(&blob);
    assert_eq!(first, second);
    assert_eq!(first, b"hello world");
}

#[test]
fn memory_source_immutable_after_new() {
    let data = Bytes::from(vec![1, 2, 3, 4, 5]);
    let source = MemorySource::new(data);
    let cancel = boa_fapi_core::cancellation::CancellationToken::new();

    let r1 = source.read_range(0..5, &cancel).unwrap();
    let r2 = source.read_range(0..5, &cancel).unwrap();
    assert_eq!(&r1[..], &[1, 2, 3, 4, 5]);
    assert_eq!(&r2[..], &[1, 2, 3, 4, 5]);
}

// ──────────────────────────────────────────────
// BlobData::slice tests
// ──────────────────────────────────────────────

#[test]
fn slice_none_none() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(None, None, None, &limits()).unwrap();
    assert_eq!(sliced.size(), 11);
    assert_eq!(read_blob(&sliced), b"hello world");
}

#[test]
fn slice_start_0_end_size() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(0), Some(11), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 11);
    assert_eq!(read_blob(&sliced), b"hello world");
}

#[test]
fn slice_positive_start_end() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(6), Some(11), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 5);
    assert_eq!(read_blob(&sliced), b"world");
}

#[test]
fn slice_positive_beyond_size() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(3), Some(100), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 2);
    assert_eq!(read_blob(&sliced), b"lo");
}

#[test]
fn slice_negative_end_minus_1() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(0), Some(-1), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 10);
    assert_eq!(read_blob(&sliced), b"hello worl");
}

#[test]
fn slice_negative_start_minus_size() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(-11), None, None, &limits()).unwrap();
    assert_eq!(sliced.size(), 11);
    assert_eq!(read_blob(&sliced), b"hello world");
}

#[test]
fn slice_negative_less_than_minus_size() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(-100), None, None, &limits()).unwrap();
    assert_eq!(sliced.size(), 5);
    assert_eq!(read_blob(&sliced), b"hello");
}

#[test]
fn slice_i64_min() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(i64::MIN), None, None, &limits()).unwrap();
    assert_eq!(sliced.size(), 5);
    assert_eq!(read_blob(&sliced), b"hello");
}

#[test]
fn slice_i64_max() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(i64::MAX), None, None, &limits()).unwrap();
    // i64::MAX > 5, so clamped to 5
    assert_eq!(sliced.size(), 0);
}

#[test]
fn slice_end_less_than_start() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(5), Some(2), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 0);
}

#[test]
fn slice_empty_blob() {
    let blob = BlobData::empty("text/plain");
    let sliced = blob.slice(Some(0), Some(0), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 0);
}

#[test]
fn slice_crosses_two_segments() {
    let src1 = make_source(b"hello");
    let src2 = make_source(b"world");
    let seg1 = make_segment(src1, 0, 5);
    let seg2 = make_segment(src2, 0, 5);
    let blob = BlobData::from_segments(vec![seg1, seg2], "text/plain", &limits()).unwrap();

    // Slice from position 3 to 8: "lo" + "wo"
    let sliced = blob.slice(Some(3), Some(8), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 5);
    assert_eq!(read_blob(&sliced), b"lowor");
}

#[test]
fn slice_crosses_all_segments() {
    let src1 = make_source(b"aaa");
    let src2 = make_source(b"bbb");
    let src3 = make_source(b"ccc");
    let seg1 = make_segment(src1, 0, 3);
    let seg2 = make_segment(src2, 0, 3);
    let seg3 = make_segment(src3, 0, 3);
    let blob = BlobData::from_segments(vec![seg1, seg2, seg3], "text/plain", &limits()).unwrap();

    let sliced = blob.slice(Some(0), Some(9), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 9);
    assert_eq!(read_blob(&sliced), b"aaabbbccc");
}

#[test]
fn slice_override_type_normal() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob
        .slice(None, None, Some("application/octet-stream"), &limits())
        .unwrap();
    assert_eq!(sliced.media_type(), "application/octet-stream");
}

#[test]
fn slice_override_type_invalid_unicode() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob
        .slice(None, None, Some("text/\nplain"), &limits())
        .unwrap();
    assert_eq!(sliced.media_type(), "");
}

#[test]
fn slice_override_type_none_is_empty() {
    let src = make_source(b"hello");
    let seg = make_segment(src, 0, 5);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(None, None, None, &limits()).unwrap();
    assert_eq!(sliced.media_type(), "");
}

#[test]
fn slice_does_not_modify_original() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let _sliced = blob.slice(Some(0), Some(5), None, &limits()).unwrap();
    assert_eq!(read_blob(&blob), b"hello world");
}

#[test]
fn slice_preserves_arc_pointers() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(3), Some(8), None, &limits()).unwrap();

    // Verify that the sliced blob shares the same Arc pointer
    let original_ptr = blob.segment_source_ptr(0);
    let sliced_ptr = sliced.segment_source_ptr(0);
    assert!(std::ptr::eq(original_ptr, sliced_ptr));
}

#[test]
fn slice_single_segment_fast_path() {
    let src = make_source(b"hello world");
    let seg = make_segment(src, 0, 11);
    let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
    let sliced = blob.slice(Some(1), Some(4), None, &limits()).unwrap();
    assert_eq!(sliced.size(), 3);
    assert_eq!(read_blob(&sliced), b"ell");
    assert_eq!(sliced.segment_count(), 1);
}
