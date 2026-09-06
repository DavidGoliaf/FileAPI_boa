//! Byte source abstraction for File API data.

pub mod memory;

use crate::cancellation::CancellationToken;
use crate::file_api_error::FileApiError;
use crate::snapshot::SnapshotState;
use bytes::Bytes;
use std::ops::Range;

/// A read-only, immutable source of bytes.
///
/// Implementations must be `Send + Sync + 'static` and must not mutate
/// their underlying data after construction.
pub trait ByteSource: Send + Sync + 'static {
    /// Returns the total number of bytes available in this source.
    fn len(&self) -> u64;

    /// Returns `true` if the source contains zero bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the snapshot state of this source.
    fn snapshot(&self) -> SnapshotState;

    /// Reads the specified byte range from this source.
    ///
    /// The caller provides a cancellation token; if cancelled before the read
    /// completes, `Err(FileApiError::Cancelled)` is returned.
    ///
    /// Returns `Err(FileApiError::InvalidRange)` if the range is invalid
    /// (start > end or end > len).
    fn read_range(
        &self,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<Bytes, FileApiError>;
}
