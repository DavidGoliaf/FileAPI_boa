//! Tests for FileApiLimits validation.

use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;

#[test]
fn default_limits_match_spec() {
    let limits = FileApiLimits::default();
    assert_eq!(limits.max_blob_size, 2 * 1024 * 1024 * 1024);
    assert_eq!(limits.max_materialize_bytes, 256 * 1024 * 1024);
    assert_eq!(limits.max_sync_read_bytes, 32 * 1024 * 1024);
    assert_eq!(limits.max_parts, 1_000_000);
    assert_eq!(limits.max_segments_after_normalize, 65_536);
    assert_eq!(limits.max_concurrent_reads_per_global, 64);
    assert_eq!(limits.max_blob_urls_per_global, 10_000);
    assert_eq!(limits.default_chunk_size, 64 * 1024);
    assert_eq!(limits.max_data_url_output, 256 * 1024 * 1024);
}

#[test]
fn default_limits_validate_ok() {
    assert!(FileApiLimits::default().validate().is_ok());
}

#[test]
fn zero_blob_size() {
    let mut limits = FileApiLimits::default();
    limits.max_blob_size = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSize))
    );
}

#[test]
fn zero_materialize_bytes() {
    let mut limits = FileApiLimits::default();
    limits.max_materialize_bytes = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes
        ))
    );
}

#[test]
fn zero_sync_read_bytes() {
    let mut limits = FileApiLimits::default();
    limits.max_sync_read_bytes = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::SyncReadBytes
        ))
    );
}

#[test]
fn zero_parts() {
    let mut limits = FileApiLimits::default();
    limits.max_parts = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobParts))
    );
}

#[test]
fn zero_segments() {
    let mut limits = FileApiLimits::default();
    limits.max_segments_after_normalize = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobSegments))
    );
}

#[test]
fn zero_concurrent_reads() {
    let mut limits = FileApiLimits::default();
    limits.max_concurrent_reads_per_global = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::ConcurrentReads
        ))
    );
}

#[test]
fn zero_blob_urls() {
    let mut limits = FileApiLimits::default();
    limits.max_blob_urls_per_global = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(ResourceLimitKind::BlobUrls))
    );
}

#[test]
fn zero_chunk_size() {
    let mut limits = FileApiLimits::default();
    limits.default_chunk_size = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes
        ))
    );
}

#[test]
fn zero_data_url_output() {
    let mut limits = FileApiLimits::default();
    limits.max_data_url_output = 0;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::DataUrlOutput
        ))
    );
}

#[test]
fn sync_exceeds_materialize() {
    let mut limits = FileApiLimits::default();
    limits.max_sync_read_bytes = limits.max_materialize_bytes + 1;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::SyncReadBytes
        ))
    );
}

#[test]
fn materialize_exceeds_blob() {
    let mut limits = FileApiLimits::default();
    limits.max_materialize_bytes = limits.max_blob_size + 1;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes
        ))
    );
}

#[test]
fn chunk_exceeds_materialize() {
    let mut limits = FileApiLimits::default();
    limits.default_chunk_size = (limits.max_materialize_bytes as usize) + 1;
    assert_eq!(
        limits.validate(),
        Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes
        ))
    );
}

#[test]
fn sync_equals_materialize_ok() {
    let mut limits = FileApiLimits::default();
    limits.max_sync_read_bytes = limits.max_materialize_bytes;
    assert!(limits.validate().is_ok());
}

#[test]
fn materialize_equals_blob_ok() {
    let mut limits = FileApiLimits::default();
    limits.max_materialize_bytes = limits.max_blob_size;
    assert!(limits.validate().is_ok());
}

#[test]
fn chunk_equals_materialize_ok() {
    let mut limits = FileApiLimits::default();
    limits.default_chunk_size = limits.max_materialize_bytes as usize;
    assert!(limits.validate().is_ok());
}
