//! Resource limit kinds used in error reporting.

/// Identifies which resource limit was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceLimitKind {
    /// Total blob size exceeded.
    BlobSize,
    /// Number of blob parts exceeded.
    BlobParts,
    /// Number of blob segments exceeded.
    BlobSegments,
    /// Bytes to materialize exceeded.
    MaterializeBytes,
    /// Concurrent reads exceeded.
    ConcurrentReads,
    /// Blob URLs exceeded.
    BlobUrls,
    /// Data URL output exceeded.
    DataUrlOutput,
    /// Synchronous read bytes exceeded.
    SyncReadBytes,
}
