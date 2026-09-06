//! Resource limits for File API operations.

use crate::error::ResourceLimitKind;
use crate::file_api_error::FileApiError;

/// Configurable resource limits for File API operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileApiLimits {
    /// Maximum total blob size in bytes (default: 2 GiB).
    pub max_blob_size: u64,
    /// Maximum bytes to materialize in memory (default: 256 MiB).
    pub max_materialize_bytes: u64,
    /// Maximum bytes for synchronous reads (default: 32 MiB).
    pub max_sync_read_bytes: u64,
    /// Maximum number of blob parts (default: 1,000,000).
    pub max_parts: usize,
    /// Maximum segments after normalization (default: 65,536).
    pub max_segments_after_normalize: usize,
    /// Maximum concurrent reads per global (default: 64).
    pub max_concurrent_reads_per_global: usize,
    /// Maximum blob URLs per global (default: 10,000).
    pub max_blob_urls_per_global: usize,
    /// Default chunk size (default: 64 KiB).
    pub default_chunk_size: usize,
    /// Maximum data URL output size (default: 256 MiB).
    pub max_data_url_output: u64,
}

impl Default for FileApiLimits {
    fn default() -> Self {
        Self {
            max_blob_size: 2 * 1024 * 1024 * 1024,    // 2 GiB
            max_materialize_bytes: 256 * 1024 * 1024, // 256 MiB
            max_sync_read_bytes: 32 * 1024 * 1024,    // 32 MiB
            max_parts: 1_000_000,
            max_segments_after_normalize: 65_536,
            max_concurrent_reads_per_global: 64,
            max_blob_urls_per_global: 10_000,
            default_chunk_size: 64 * 1024,          // 64 KiB
            max_data_url_output: 256 * 1024 * 1024, // 256 MiB
        }
    }
}

impl FileApiLimits {
    /// Validates that the limits form a consistent configuration.
    ///
    /// Returns `Err(FileApiError::ResourceLimit(..))` if any limit is zero,
    /// if the stream chunk-size range (`16 KiB..=1 MiB`) is violated, or if
    /// the ordering constraints are violated.
    ///
    /// Ordering means: `sync <= materialize <= blob` and
    /// `chunk <= materialize`. This function reports the whole
    /// configuration; it does not clamp or repair it.
    pub fn validate(&self) -> Result<(), FileApiError> {
        if self.max_blob_size == 0 {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize));
        }
        if self.max_materialize_bytes == 0 {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        if self.max_sync_read_bytes == 0 {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::SyncReadBytes,
            ));
        }
        if self.max_parts == 0 {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobParts));
        }
        if self.max_segments_after_normalize == 0 {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments));
        }
        if self.max_concurrent_reads_per_global == 0 {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::ConcurrentReads,
            ));
        }
        if self.max_blob_urls_per_global == 0 {
            return Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobUrls));
        }
        if self.default_chunk_size == 0 {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        if !(16 * 1024..=1024 * 1024).contains(&self.default_chunk_size) {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        if self.max_data_url_output == 0 {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::DataUrlOutput,
            ));
        }
        if self.max_sync_read_bytes > self.max_materialize_bytes {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::SyncReadBytes,
            ));
        }
        if self.max_materialize_bytes > self.max_blob_size {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        if (self.default_chunk_size as u64) > self.max_materialize_bytes {
            return Err(FileApiError::ResourceLimit(
                ResourceLimitKind::MaterializeBytes,
            ));
        }
        Ok(())
    }
}
