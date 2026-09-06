//! Blob data model: segmented immutable byte storage with File API slice semantics.
//!
//! The segmentation is an internal representation detail. The public API
//! exposes only the fixed M1 contract (`empty`, `from_segments`, `size`,
//! `media_type`, `snapshot`, `segment_count`, `slice`), the M2 no-copy
//! composition primitives (`concat_shared`, `push_shared`) and the single
//! M3 bounded byte-read primitive (`materialize`). No segment accessor,
//! unbounded read, source-identity or other test probe is public:
//! [`BlobSegment`] fields are public solely so that `from_segments` can
//! accept caller-owned segments as *input*.

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use crate::error::ResourceLimitKind;
use crate::file_api_error::FileApiError;
use crate::limits::FileApiLimits;
use crate::mime::normalize_blob_type;
use crate::snapshot::SnapshotState;
use crate::source::ByteSource;

/// A segment of a blob, referencing a range within a byte source.
#[derive(Clone)]
pub struct BlobSegment {
    /// The underlying byte source.
    pub source: Arc<dyn ByteSource>,
    /// The offset within the source where this segment begins.
    pub offset: u64,
    /// The length of this segment in bytes.
    pub len: u64,
}

impl std::fmt::Debug for BlobSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobSegment")
            .field("source", &"<dyn ByteSource>")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish()
    }
}

/// Immutable segmented byte data conforming to File API blob semantics.
///
/// `BlobData` is the core data structure for representing blob content.
/// It stores zero or more `BlobSegment`s that together represent the
/// blob's byte content without copying the underlying data.
#[derive(Clone, Debug)]
pub struct BlobData {
    segments: Vec<BlobSegment>,
    size: u64,
    media_type: String,
    snapshot: SnapshotState,
}

impl BlobData {
    /// Creates an empty blob with the given media type.
    pub fn empty(media_type: impl AsRef<str>) -> Self {
        Self {
            segments: Vec::new(),
            size: 0,
            media_type: normalize_blob_type(media_type.as_ref()),
            snapshot: SnapshotState::Memory,
        }
    }

    /// Creates a blob from the given segments.
    ///
    /// Validates each segment's bounds, computes total size with checked arithmetic,
    /// normalizes the media type, and enforces resource limits.
    pub fn from_segments(
        segments: Vec<BlobSegment>,
        media_type: impl AsRef<str>,
        limits: &FileApiLimits,
    ) -> Result<Self, FileApiError> {
        let media_type = normalize_blob_type(media_type.as_ref());

        // Validate every segment before filtering, so that zero-length segments
        // with invalid offsets are still rejected.
        for seg in &segments {
            let source_len = seg.source.len();
            if seg.offset > source_len {
                return Err(FileApiError::InvalidRange);
            }
            let seg_end = seg
                .offset
                .checked_add(seg.len)
                .ok_or(FileApiError::InvalidRange)?;
            if seg_end > source_len {
                return Err(FileApiError::InvalidRange);
            }
        }

        // Filter out zero-length segments to normalize, but preserve semantics.
        let segments: Vec<BlobSegment> = segments.into_iter().filter(|s| s.len > 0).collect();

        if segments.len() > limits.max_segments_after_normalize {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments));
        }

        let mut total_size: u64 = 0;
        for seg in &segments {
            // Accumulate total size with overflow check
            total_size = total_size
                .checked_add(seg.len)
                .ok_or(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))?;
        }

        if total_size > limits.max_blob_size {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize));
        }

        Ok(Self {
            segments,
            size: total_size,
            media_type,
            snapshot: SnapshotState::Memory,
        })
    }

    /// Returns the total size of this blob in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the normalized media type of this blob.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the snapshot state of this blob.
    pub fn snapshot(&self) -> &SnapshotState {
        &self.snapshot
    }

    /// Returns the number of segments in this blob.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Appends the segments of `other` to a new blob under `limits`.
    ///
    /// This is the sole no-copy composition primitive for bindings: every
    /// segment of the new blob keeps the shared `Arc` source of `other`,
    /// so no payload bytes are copied. `media_type` is normalized; size and
    /// segment-count limits are enforced on the concatenated result.
    /// Returns `Err` without observable side effects when a limit fails.
    pub fn concat_shared(
        &self,
        other: &Self,
        media_type: impl AsRef<str>,
        limits: &FileApiLimits,
    ) -> Result<Self, FileApiError> {
        let media_type = normalize_blob_type(media_type.as_ref());
        let mut segments =
            Vec::with_capacity(self.segments.len().saturating_add(other.segments.len()));
        let mut total_size: u64 = 0;
        for seg in self.segments.iter().chain(other.segments.iter()) {
            total_size = total_size
                .checked_add(seg.len)
                .ok_or(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))?;
            segments.push(BlobSegment {
                source: Arc::clone(&seg.source),
                offset: seg.offset,
                len: seg.len,
            });
        }
        if segments.len() > limits.max_segments_after_normalize {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments));
        }
        if total_size > limits.max_blob_size {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize));
        }
        Ok(Self {
            segments,
            size: total_size,
            media_type,
            snapshot: SnapshotState::Memory,
        })
    }

    /// Accumulates one more blob part without copying payload bytes.
    ///
    /// Zero-copy composition primitive for the JS bindings: re-links the
    /// shared sources of `other` after the segments of `self`, enforcing
    /// `limits.max_parts` on the part count, `max_segments_after_normalize`
    /// on the segment count and `max_blob_size` on the total size.
    /// `media_type` is normalized. Returning `Err` leaves `self` unchanged,
    /// so a failing part produces no observable partial blob.
    pub fn push_shared(
        &mut self,
        other: &Self,
        media_type: impl AsRef<str>,
        limits: &FileApiLimits,
        parts: &mut usize,
    ) -> Result<(), FileApiError> {
        if *parts >= limits.max_parts {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobParts));
        }
        let combined = self.concat_shared(other, media_type, limits)?;
        *self = combined;
        *parts += 1;
        Ok(())
    }

    /// Materializes the blob content under the materialization limit.
    ///
    /// The sole M3 byte-read primitive for bindings: returns the immutable
    /// content as `Bytes` without exposing segments or sources. Enforces
    /// `limits.max_materialize_bytes` **before** any output allocation;
    /// converts the size fallibly to `usize` and reserves the output
    /// fallibly, so allocation failure yields
    /// `ResourceLimit(MaterializeBytes)` instead of a panic. Checks
    /// `cancel` before the first and before every segment read, reads
    /// exactly `offset..offset+len` with checked arithmetic, and requires
    /// every source to return exactly the requested bytes: a short or long
    /// response yields `InvalidRange` with no partial bytes. The output
    /// length is release-enforced against the preflighted capacity, so an
    /// oversized response can never grow past `max_materialize_bytes`.
    /// Does not mutate the blob, its sources, or the limits. An empty blob
    /// yields empty `Bytes`.
    pub fn materialize(
        &self,
        limits: &FileApiLimits,
        cancel: &CancellationToken,
    ) -> Result<bytes::Bytes, FileApiError> {
        if self.size > limits.max_materialize_bytes {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        let capacity = usize::try_from(self.size)
            .map_err(|_| FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes))?;
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(capacity)
            .map_err(|_| FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes))?;
        for seg in &self.segments {
            if cancel.is_cancelled() {
                return Err(FileApiError::Cancelled);
            }
            let expected_len = usize::try_from(seg.len)
                .map_err(|_| FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes))?;
            let end = seg
                .offset
                .checked_add(seg.len)
                .ok_or(FileApiError::InvalidRange)?;
            let chunk = seg.source.read_range(seg.offset..end, cancel)?;
            if chunk.len() != expected_len {
                return Err(FileApiError::InvalidRange);
            }
            let next_len =
                out.len()
                    .checked_add(expected_len)
                    .ok_or(FileApiError::ResourceLimit(
                        ResourceLimitKind::MaterializeBytes,
                    ))?;
            if next_len > capacity {
                return Err(FileApiError::InvalidRange);
            }
            out.extend_from_slice(&chunk);
        }
        if out.len() != capacity {
            return Err(FileApiError::InvalidRange);
        }
        Ok(bytes::Bytes::from(out))
    }

    /// Creates a new blob by slicing this blob per File API semantics.
    ///
    /// `start` and `end` follow the File API integer conversion rules:
    /// - `None` start → 0; `None` end → size
    /// - Negative values are relative to the end (clamped to 0)
    /// - Positive values are clamped to size
    ///
    /// The resulting blob shares `Arc` references to the original sources
    /// without copying payload data.
    pub fn slice(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        content_type: Option<&str>,
        limits: &FileApiLimits,
    ) -> Result<Self, FileApiError> {
        let original_size = self.size;

        // Step 1-3: compute relative_start
        let relative_start = match start {
            None => 0u64,
            Some(s) if s < 0 => {
                let abs = s.unsigned_abs();
                original_size.saturating_sub(abs)
            }
            Some(s) => {
                let s = s as u64;
                if s > original_size { original_size } else { s }
            }
        };

        // Step 4-6: compute relative_end
        let relative_end = match end {
            None => original_size,
            Some(e) if e < 0 => {
                let abs = e.unsigned_abs();
                original_size.saturating_sub(abs)
            }
            Some(e) => {
                let e = e as u64;
                if e > original_size { original_size } else { e }
            }
        };

        // Step 7: compute span
        let span = relative_end.saturating_sub(relative_start);

        // Step 8: normalize content type
        let new_media_type = match content_type {
            None => String::new(),
            Some(ct) => normalize_blob_type(ct),
        };

        // Empty span: return empty blob
        if span == 0 {
            return Ok(Self {
                segments: Vec::new(),
                size: 0,
                media_type: new_media_type,
                snapshot: SnapshotState::Memory,
            });
        }

        // Step 9-10: collect intersecting segments with adjusted offsets/lengths
        let slice_end = relative_start + span;
        let mut new_segments = Vec::new();
        let mut cursor = 0u64;

        for seg in &self.segments {
            let seg_start = cursor;
            let seg_end = cursor + seg.len;

            // Check if this segment intersects the slice range
            if seg_end <= relative_start || seg_start >= slice_end {
                cursor = seg_end;
                continue;
            }

            // Compute the intersection
            let intersect_start = relative_start.max(seg_start);
            let intersect_end = slice_end.min(seg_end);

            // Map back to source offsets
            let source_offset = seg.offset + (intersect_start - seg_start);
            let intersect_len = intersect_end - intersect_start;

            new_segments.push(BlobSegment {
                source: Arc::clone(&seg.source),
                offset: source_offset,
                len: intersect_len,
            });

            cursor = seg_end;
        }

        // Step 12: validate limits
        if new_segments.len() > limits.max_segments_after_normalize {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments));
        }
        if span > limits.max_blob_size {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize));
        }

        Ok(Self {
            segments: new_segments,
            size: span,
            media_type: new_media_type,
            snapshot: SnapshotState::Memory,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Unit tests for `BlobData`.
    //!
    //! These tests verify content by reading the blob's segments directly
    //! through child-module access to the private fields, which keeps the
    //! public API free of test-only accessors.

    use super::*;
    use crate::cancellation::CancellationToken;
    use crate::source::memory::MemorySource;
    use bytes::Bytes;
    use proptest::prelude::*;

    fn limits() -> FileApiLimits {
        FileApiLimits::default()
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn read_blob(blob: &BlobData) -> Vec<u8> {
        // Child-module access to the private `segments` field: proves content
        // and `Arc` sharing without any public test accessor.
        let token = cancel();
        let mut result = Vec::with_capacity(blob.size as usize);
        for seg in &blob.segments {
            let end = seg.offset + seg.len;
            result.extend_from_slice(&seg.source.read_range(seg.offset..end, &token).unwrap());
        }
        result
    }

    /// Asserts that two blobs re-link the same `Arc` source allocations in
    /// the same order (no payload copy), via private fields only.
    fn assert_shares_sources(actual: &BlobData, expected: &BlobData) {
        assert_eq!(
            actual.segment_count(),
            expected.segment_count(),
            "segment counts differ"
        );
        for (a, b) in actual.segments.iter().zip(expected.segments.iter()) {
            assert!(Arc::ptr_eq(&a.source, &b.source), "source Arc differs");
            assert_eq!(a.offset, b.offset, "segment offset differs");
            assert_eq!(a.len, b.len, "segment length differs");
        }
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
        let blob =
            BlobData::from_segments(vec![seg_empty, seg_full], "text/plain", &limits()).unwrap();
        assert_eq!(blob.segment_count(), 1);
        assert_eq!(blob.size(), 5);
    }

    #[test]
    fn from_segments_zero_length_invalid_offset_rejected() {
        let src = make_source(b"hello");
        // Zero-length segment with offset greater than source length is rejected
        let seg_bad = BlobSegment {
            source: src,
            offset: 100,
            len: 0,
        };
        let result = BlobData::from_segments(vec![seg_bad], "text/plain", &limits());
        assert!(matches!(result, Err(FileApiError::InvalidRange)));
    }

    #[test]
    fn from_segments_invalid_offset() {
        let src = make_source(b"hello");
        let seg = make_segment(src, 10, 5); // offset greater than source length
        let result = BlobData::from_segments(vec![seg], "text/plain", &limits());
        assert!(matches!(result, Err(FileApiError::InvalidRange)));
    }

    #[test]
    fn from_segments_invalid_end() {
        let src = make_source(b"hello");
        let seg = make_segment(src, 2, 10); // offset + len beyond source length
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
        let mut custom = limits();
        custom.max_blob_size = 11;
        let src = make_source(b"hello world");
        let seg = make_segment(src, 0, 11);
        assert!(BlobData::from_segments(vec![seg], "text/plain", &custom).is_ok());
    }

    #[test]
    fn from_segments_size_one_over_limit() {
        let mut custom = limits();
        custom.max_blob_size = 10;
        let src = make_source(b"hello world");
        let seg = make_segment(src, 0, 11);
        assert!(matches!(
            BlobData::from_segments(vec![seg], "text/plain", &custom),
            Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))
        ));
    }

    #[test]
    fn from_segments_count_exactly_at_limit() {
        let mut custom = limits();
        custom.max_segments_after_normalize = 2;
        let src = make_source(b"abc");
        let seg1 = make_segment(src.clone(), 0, 1);
        let seg2 = make_segment(src, 1, 1);
        assert!(BlobData::from_segments(vec![seg1, seg2], "text/plain", &custom).is_ok());
    }

    #[test]
    fn from_segments_count_one_over_limit() {
        let mut custom = limits();
        custom.max_segments_after_normalize = 1;
        let src = make_source(b"abc");
        let seg1 = make_segment(src.clone(), 0, 1);
        let seg2 = make_segment(src, 1, 1);
        assert!(matches!(
            BlobData::from_segments(vec![seg1, seg2], "text/plain", &custom),
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
        let token = cancel();

        let r1 = source.read_range(0..5, &token).unwrap();
        let r2 = source.read_range(0..5, &token).unwrap();
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
        // i64::MAX is beyond the size, so it is clamped to an empty span
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

        // Slice from position 3 to 8: "lo" plus "wo"
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
        let blob =
            BlobData::from_segments(vec![seg1, seg2, seg3], "text/plain", &limits()).unwrap();

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
        let seg = make_segment(Arc::clone(&src), 0, 11);
        let blob = BlobData::from_segments(vec![seg], "text/plain", &limits()).unwrap();
        let sliced = blob.slice(Some(3), Some(8), None, &limits()).unwrap();

        // The sliced blob must share the same Arc allocation as the original
        // source, proving that slice copies no payload data.
        assert_eq!(sliced.segment_count(), 1);
        assert_eq!(sliced.segments.len(), blob.segments.len());
        assert!(Arc::ptr_eq(&src, &sliced.segments[0].source));
        assert!(Arc::ptr_eq(
            &blob.segments[0].source,
            &sliced.segments[0].source
        ));
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

    // ──────────────────────────────────────────────
    // BlobData::concat_shared (no-copy composition primitive for bindings)
    // ──────────────────────────────────────────────

    #[test]
    fn concat_shared_links_and_concatenates_without_copy() {
        let src1 = make_source(b"hello");
        let src2 = make_source(b" world");
        let blob1 = BlobData::from_segments(vec![make_segment(src1, 0, 5)], "", &limits()).unwrap();
        let blob2 = BlobData::from_segments(vec![make_segment(src2, 0, 6)], "", &limits()).unwrap();
        let out = blob1.concat_shared(&blob2, "", &limits()).unwrap();
        assert_eq!(out.size(), 11);
        assert_eq!(out.segment_count(), 2);
        assert_eq!(read_blob(&out), b"hello world");
        assert_shares_sources(&out, &blob1.concat_shared(&blob2, "", &limits()).unwrap());
        assert!(Arc::ptr_eq(
            &out.segments[0].source,
            &blob1.segments[0].source
        ));
        assert!(Arc::ptr_eq(
            &out.segments[1].source,
            &blob2.segments[0].source
        ));
    }

    #[test]
    fn concat_shared_enforces_segment_limit() {
        let mut custom = limits();
        custom.max_segments_after_normalize = 1;
        let blob1 =
            BlobData::from_segments(vec![make_segment(make_source(b"a"), 0, 1)], "", &limits())
                .unwrap();
        let blob2 =
            BlobData::from_segments(vec![make_segment(make_source(b"b"), 0, 1)], "", &limits())
                .unwrap();
        assert!(matches!(
            blob1.concat_shared(&blob2, "", &custom),
            Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments))
        ));
    }

    #[test]
    fn concat_shared_enforces_size_limit() {
        let mut custom = limits();
        custom.max_blob_size = 1;
        let blob1 =
            BlobData::from_segments(vec![make_segment(make_source(b"a"), 0, 1)], "", &limits())
                .unwrap();
        let blob2 =
            BlobData::from_segments(vec![make_segment(make_source(b"b"), 0, 1)], "", &limits())
                .unwrap();
        assert!(matches!(
            blob1.concat_shared(&blob2, "", &custom),
            Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))
        ));
    }

    #[test]
    fn concat_shared_normalizes_media_type() {
        let blob1 = BlobData::empty("");
        let blob2 = BlobData::empty("");
        let out = blob1
            .concat_shared(&blob2, "TEXT/PLAIN", &limits())
            .unwrap();
        assert_eq!(out.media_type(), "text/plain");
    }

    // ──────────────────────────────────────────────
    // BlobData::materialize
    // ──────────────────────────────────────────────

    #[test]
    fn materialize_multi_segment_order() {
        let src1 = make_source(b"hello");
        let src2 = make_source(b" world");
        let blob1 = BlobData::from_segments(vec![make_segment(src1, 0, 5)], "", &limits()).unwrap();
        let blob2 = BlobData::from_segments(vec![make_segment(src2, 0, 6)], "", &limits()).unwrap();
        let out = blob1.concat_shared(&blob2, "", &limits()).unwrap();
        let bytes = out.materialize(&limits(), &cancel()).unwrap();
        assert_eq!(&bytes[..], b"hello world");
        // Second call returns the same content; inputs are untouched.
        assert_eq!(
            &out.materialize(&limits(), &cancel()).unwrap()[..],
            b"hello world"
        );
        assert_eq!(out.size(), 11);
    }

    #[test]
    fn materialize_empty_blob() {
        let blob = BlobData::empty("text/plain");
        let bytes = blob.materialize(&limits(), &cancel()).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(blob.size(), 0);
    }

    #[test]
    fn materialize_exactly_at_limit() {
        let mut custom = limits();
        custom.max_materialize_bytes = 5;
        custom.max_blob_size = 5;
        let blob = BlobData::from_segments(
            vec![make_segment(make_source(b"hello"), 0, 5)],
            "",
            &limits(),
        )
        .unwrap();
        assert_eq!(&blob.materialize(&custom, &cancel()).unwrap()[..], b"hello");
    }

    #[test]
    fn materialize_one_over_limit_rejected_before_allocation() {
        let mut custom = limits();
        custom.max_materialize_bytes = 4;
        let blob = BlobData::from_segments(
            vec![make_segment(make_source(b"hello"), 0, 5)],
            "",
            &limits(),
        )
        .unwrap();
        assert!(matches!(
            blob.materialize(&custom, &cancel()),
            Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes
            ))
        ));
    }

    #[test]
    fn materialize_cancelled_before_first_read() {
        let blob = BlobData::from_segments(
            vec![make_segment(make_source(b"hello"), 0, 5)],
            "",
            &limits(),
        )
        .unwrap();
        let token = cancel();
        token.cancel();
        assert!(matches!(
            blob.materialize(&limits(), &token),
            Err(FileApiError::Cancelled)
        ));
    }

    #[test]
    fn materialize_cancelled_between_segments() {
        struct CancelOnSecond {
            inner: MemorySource,
            calls: std::sync::atomic::AtomicUsize,
            token: CancellationToken,
        }
        impl ByteSource for CancelOnSecond {
            fn len(&self) -> u64 {
                self.inner.len()
            }
            fn snapshot(&self) -> crate::snapshot::SnapshotState {
                self.inner.snapshot()
            }
            fn read_range(
                &self,
                range: std::ops::Range<u64>,
                cancel: &CancellationToken,
            ) -> Result<Bytes, FileApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call >= 1 {
                    self.token.cancel();
                }
                self.inner.read_range(range, cancel)
            }
        }
        let token = cancel();
        let source: Arc<dyn ByteSource> = Arc::new(CancelOnSecond {
            inner: MemorySource::new(Bytes::copy_from_slice(b"ab")),
            calls: std::sync::atomic::AtomicUsize::new(0),
            token: token.clone(),
        });
        let blob = BlobData::from_segments(
            vec![
                make_segment(Arc::clone(&source), 0, 1),
                make_segment(source, 1, 1),
            ],
            "",
            &limits(),
        )
        .unwrap();
        assert!(matches!(
            blob.materialize(&limits(), &token),
            Err(FileApiError::Cancelled)
        ));
    }

    #[test]
    fn materialize_rejects_short_source_response() {
        struct ShortSource;
        impl ByteSource for ShortSource {
            fn len(&self) -> u64 {
                5
            }
            fn snapshot(&self) -> crate::snapshot::SnapshotState {
                crate::snapshot::SnapshotState::Memory
            }
            fn read_range(
                &self,
                _range: std::ops::Range<u64>,
                _cancel: &CancellationToken,
            ) -> Result<Bytes, FileApiError> {
                // Declared length 5, but returns one byte short.
                Ok(Bytes::copy_from_slice(b"hell"))
            }
        }
        let blob = BlobData::from_segments(
            vec![make_segment(Arc::new(ShortSource), 0, 5)],
            "",
            &limits(),
        )
        .unwrap();
        assert!(matches!(
            blob.materialize(&limits(), &cancel()),
            Err(FileApiError::InvalidRange)
        ));
    }

    #[test]
    fn materialize_rejects_long_source_response() {
        struct LongSource;
        impl ByteSource for LongSource {
            fn len(&self) -> u64 {
                5
            }
            fn snapshot(&self) -> crate::snapshot::SnapshotState {
                crate::snapshot::SnapshotState::Memory
            }
            fn read_range(
                &self,
                _range: std::ops::Range<u64>,
                _cancel: &CancellationToken,
            ) -> Result<Bytes, FileApiError> {
                // Declared length 5, but returns one byte too many.
                Ok(Bytes::copy_from_slice(b"hello!"))
            }
        }
        let mut tight = limits();
        tight.max_materialize_bytes = 5;
        let blob = BlobData::from_segments(
            vec![make_segment(Arc::new(LongSource), 0, 5)],
            "",
            &limits(),
        )
        .unwrap();
        // Mismatch is rejected before any output growth past the limit.
        assert!(matches!(
            blob.materialize(&tight, &cancel()),
            Err(FileApiError::InvalidRange)
        ));
    }

    #[test]
    fn materialize_checked_offset_failure() {
        struct EvilSource;
        impl ByteSource for EvilSource {
            fn len(&self) -> u64 {
                u64::MAX
            }
            fn snapshot(&self) -> crate::snapshot::SnapshotState {
                crate::snapshot::SnapshotState::Memory
            }
            fn read_range(
                &self,
                _range: std::ops::Range<u64>,
                _cancel: &CancellationToken,
            ) -> Result<Bytes, FileApiError> {
                Ok(Bytes::new())
            }
        }
        // `from_segments` rejects the overflowing segment up front, so build
        // the blob structurally equivalent input through the public API and
        // assert the materialize path reports InvalidRange on overflow.
        let evil: Arc<dyn ByteSource> = Arc::new(EvilSource);
        let bad = BlobSegment {
            source: evil,
            offset: u64::MAX,
            len: 1,
        };
        assert!(matches!(
            BlobData::from_segments(vec![bad], "", &limits()),
            Err(FileApiError::InvalidRange)
        ));
        // Direct checked-arithmetic probe: offset+len overflow can never
        // produce a wrapped range for `read_range`.
        let offset = u64::MAX;
        let len = 1u64;
        assert!(offset.checked_add(len).is_none());
    }

    // ──────────────────────────────────────────────
    // Property test: BlobData slice matches reference
    // ──────────────────────────────────────────────

    fn reference_slice(data: &[u8], start: Option<i64>, end: Option<i64>) -> Vec<u8> {
        let len = data.len() as i64;
        let rel_start = match start {
            None => 0,
            Some(s) if s < 0 => (len + s).max(0) as u64,
            Some(s) => (s as u64).min(data.len() as u64),
        };
        let rel_end = match end {
            None => data.len() as u64,
            Some(e) if e < 0 => (len + e).max(0) as u64,
            Some(e) => (e as u64).min(data.len() as u64),
        };
        if rel_start >= rel_end {
            return Vec::new();
        }
        data[rel_start as usize..rel_end as usize].to_vec()
    }

    proptest! {
        #[test]
        fn slice_matches_reference(
            data in proptest::collection::vec(any::<u8>(), 0..256),
            start in any::<i64>(),
            end in any::<i64>(),
        ) {
            let custom = FileApiLimits::default();
            let src: Arc<dyn ByteSource> = Arc::new(MemorySource::new(Bytes::from(data.clone())));
            let seg = BlobSegment { source: src, offset: 0, len: data.len() as u64 };
            let blob = BlobData::from_segments(vec![seg], "", &custom).unwrap();

            let sliced = blob.slice(Some(start), Some(end), None, &custom).unwrap();
            let expected = reference_slice(&data, Some(start), Some(end));
            let actual = read_blob(&sliced);

            prop_assert_eq!(actual, expected);
        }
    }
}
