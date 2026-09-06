//! In-memory byte source implementation.

use bytes::Bytes;

use crate::cancellation::CancellationToken;
use crate::file_api_error::FileApiError;
use crate::snapshot::SnapshotState;
use crate::source::ByteSource;
use std::ops::Range;

/// An immutable in-memory byte source.
///
/// Wraps a `Bytes` value without copying the underlying data.
/// The source is snapshot-stable: repeated reads return the same data.
#[derive(Clone, Debug)]
pub struct MemorySource {
    data: Bytes,
}

impl MemorySource {
    /// Creates a new memory source from the given bytes.
    pub fn new(bytes: Bytes) -> Self {
        Self { data: bytes }
    }
}

impl ByteSource for MemorySource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn snapshot(&self) -> SnapshotState {
        SnapshotState::Memory
    }

    fn read_range(
        &self,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<Bytes, FileApiError> {
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }

        let start = range.start;
        let end = range.end;

        if start > end || end > self.len() {
            return Err(FileApiError::InvalidRange);
        }

        let start_usize = start as usize;
        let end_usize = end as usize;

        Ok(self.data.slice(start_usize..end_usize))
    }
}
