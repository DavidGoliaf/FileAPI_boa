//! File API error types.

use crate::error::ResourceLimitKind;

/// Errors that can occur during File API operations.
#[derive(Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FileApiError {
    /// The resource was not found.
    #[error("the resource was not found")]
    NotFound,
    /// The resource is unsafe.
    #[error("the resource is unsafe")]
    UnsafeFile,
    /// Too many concurrent reads.
    #[error("too many concurrent reads")]
    TooManyReads,
    /// The source snapshot changed.
    #[error("the source snapshot changed")]
    SnapshotChanged,
    /// The source cannot be read.
    #[error("the source cannot be read")]
    FileLocked,
    /// Access to the source was denied.
    #[error("access to the source was denied")]
    PermissionDenied,
    /// A resource limit was exceeded.
    #[error("resource limit exceeded: {0:?}")]
    ResourceLimit(ResourceLimitKind),
    /// The read was cancelled.
    #[error("read cancelled")]
    Cancelled,
    /// The source range is invalid.
    #[error("source range is invalid")]
    InvalidRange,
    /// An internal File API error occurred.
    #[error("internal File API error")]
    Internal,
}
