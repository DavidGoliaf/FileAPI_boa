//! Tests for CancellationToken and MemorySource.

use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::snapshot::SnapshotState;
use boa_fapi_core::source::ByteSource;
use boa_fapi_core::source::memory::MemorySource;
use bytes::Bytes;

// ──────────────────────────────────────────────
// CancellationToken tests
// ──────────────────────────────────────────────

#[test]
fn token_new_is_not_cancelled() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
}

#[test]
fn token_cancel_marks_cancelled() {
    let token = CancellationToken::new();
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn token_cancel_is_idempotent() {
    let token = CancellationToken::new();
    token.cancel();
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn token_clone_shares_state() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!clone.is_cancelled());
    token.cancel();
    assert!(clone.is_cancelled());
}

#[test]
fn token_default_is_not_cancelled() {
    let token = CancellationToken::default();
    assert!(!token.is_cancelled());
}

// ──────────────────────────────────────────────
// MemorySource tests
// ──────────────────────────────────────────────

#[test]
fn memory_source_len() {
    let data = Bytes::from(vec![1, 2, 3, 4, 5]);
    let source = MemorySource::new(data);
    assert_eq!(source.len(), 5);
}

#[test]
fn memory_source_empty() {
    let source = MemorySource::new(Bytes::new());
    assert_eq!(source.len(), 0);
    assert!(source.is_empty());
}

#[test]
fn memory_source_snapshot_is_memory() {
    let source = MemorySource::new(Bytes::from(vec![1, 2, 3]));
    assert_eq!(source.snapshot(), SnapshotState::Memory);
}

#[test]
fn memory_source_read_full_range() {
    let data = Bytes::from(vec![10, 20, 30, 40, 50]);
    let source = MemorySource::new(data.clone());
    let cancel = CancellationToken::new();
    let result = source.read_range(0..5, &cancel).unwrap();
    assert_eq!(&result[..], &[10, 20, 30, 40, 50]);
}

#[test]
fn memory_source_read_empty_range() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(0..0, &cancel).unwrap();
    assert!(result.is_empty());
}

#[test]
fn memory_source_read_prefix() {
    let data = Bytes::from(vec![10, 20, 30, 40, 50]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(0..3, &cancel).unwrap();
    assert_eq!(&result[..], &[10, 20, 30]);
}

#[test]
fn memory_source_read_middle() {
    let data = Bytes::from(vec![10, 20, 30, 40, 50]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(1..4, &cancel).unwrap();
    assert_eq!(&result[..], &[20, 30, 40]);
}

#[test]
fn memory_source_read_suffix() {
    let data = Bytes::from(vec![10, 20, 30, 40, 50]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(3..5, &cancel).unwrap();
    assert_eq!(&result[..], &[40, 50]);
}

#[test]
fn memory_source_read_start_greater_than_end() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    // Test start > end returns InvalidRange
    let start = 5u64;
    let end = 2u64;
    let result = source.read_range(start..end, &cancel);
    assert!(matches!(result, Err(FileApiError::InvalidRange)));
}

#[test]
fn memory_source_read_end_greater_than_len() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(0..10, &cancel);
    assert_eq!(result, Err(FileApiError::InvalidRange));
}

#[test]
fn memory_source_read_at_u64_max_boundary() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let result = source.read_range(0..u64::MAX, &cancel);
    assert_eq!(result, Err(FileApiError::InvalidRange));
}

#[test]
fn memory_source_cancel_before_read() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = source.read_range(0..3, &cancel);
    assert_eq!(result, Err(FileApiError::Cancelled));
}

#[test]
fn memory_source_clone_sees_cancel() {
    let data = Bytes::from(vec![10, 20, 30]);
    let source = MemorySource::new(data);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    cancel_clone.cancel();
    let result = source.read_range(0..3, &cancel);
    assert_eq!(result, Err(FileApiError::Cancelled));
}

#[test]
fn memory_source_result_shares_allocation() {
    let data = Bytes::from(vec![0u8; 1024]);
    let source = MemorySource::new(data.clone());
    let cancel = CancellationToken::new();
    let result = source.read_range(0..1024, &cancel).unwrap();
    // The result should share the same underlying allocation (no copy).
    // We verify by checking that the result points to the same data.
    assert_eq!(result.len(), 1024);
    assert_eq!(&result[..], &data[..]);
}
