//! Blob data model: segmented immutable byte storage with File API slice semantics.

use std::sync::Arc;

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

    #[allow(dead_code)]
    pub(crate) fn segments(&self) -> &[BlobSegment] {
        &self.segments
    }

    /// Returns the `Arc` pointer for the source of the segment at `index`.
    ///
    /// This is a testing accessor — it does not expose mutable internals.
    pub fn segment_source_ptr(&self, index: usize) -> *const () {
        Arc::as_ptr(&self.segments[index].source) as *const ()
    }

    /// Reads the entire blob content, up to `max_bytes`.
    ///
    /// Returns `Err(FileApiError::ResourceLimit(MaterializeBytes))` if the blob
    /// size exceeds `max_bytes`. This prevents unbounded allocation.
    pub fn materialize(
        &self,
        max_bytes: u64,
        cancel: &crate::cancellation::CancellationToken,
    ) -> Result<Vec<u8>, FileApiError> {
        if self.size > max_bytes {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        let mut result = Vec::with_capacity(self.size as usize);
        for seg in &self.segments {
            let bytes = seg
                .source
                .read_range(seg.offset..seg.offset + seg.len, cancel)?;
            result.extend_from_slice(&bytes);
        }
        Ok(result)
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
