//! Snapshot state for byte sources.

/// Opaque, platform-neutral identity of a filesystem resource.
///
/// Contains no path, handle value, or secret name: only a platform-derived
/// identity hash, the size, and the modification time, as provided by the
/// platform. Comparison detects replacement of the resource by a different
/// object, not only `mtime`/size drift. See `boa_fapi_fs` for how the
/// identity hash is captured with safe Rust APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub struct FileSnapshot {
    /// Platform-derived identity hash (opaque, never a path or handle).
    identity: u64,
    /// Size in bytes at capture time.
    size: u64,
    /// Modification time seconds since the Unix epoch, if provided.
    mtime_secs: Option<u64>,
    /// Modification time sub-second nanos, if provided.
    mtime_nanos: Option<u32>,
}

impl FileSnapshot {
    /// Captures an opaque filesystem snapshot from its parts.
    ///
    /// `identity` must be a platform-derived file identity hash (or a
    /// documented fallback); it must never encode a path, handle value,
    /// or secret name.
    pub fn new(
        identity: u64,
        size: u64,
        mtime_secs: Option<u64>,
        mtime_nanos: Option<u32>,
    ) -> Self {
        Self {
            identity,
            size,
            mtime_secs,
            mtime_nanos,
        }
    }

    /// Returns the opaque platform identity hash.
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// Returns the size in bytes at capture time.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the modification time seconds since the Unix epoch, if known.
    pub fn mtime_secs(&self) -> Option<u64> {
        self.mtime_secs
    }

    /// Returns the modification time sub-second nanos, if known.
    pub fn mtime_nanos(&self) -> Option<u32> {
        self.mtime_nanos
    }
}

/// Describes the snapshot state of a byte source.
///
/// `Memory` sources are always valid. `Filesystem` sources carry the
/// import-time snapshot; readers revalidate it before the first chunk and
/// on every new chunk boundary, and any mismatch fails the read with no
/// partial bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SnapshotState {
    /// The source is an in-memory snapshot that never changes.
    Memory,
    /// The source is a host-authorized filesystem resource snapshot.
    Filesystem(FileSnapshot),
}
