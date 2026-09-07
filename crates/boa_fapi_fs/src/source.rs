//! Filesystem-backed [`ByteSource`] over a pre-authorized read-only resource.
//!
//! Direct handle reads ([`FileSource`]) are available only on platforms
//! with a strong open-handle identity (see
//! [`crate::platform_has_strong_identity`]): constructors refuse on weak
//! platforms, where hosts must use [`open_copy_on_import`] instead.
//! `read_range` validates, in order:
//!
//! 1. cancellation (before any I/O);
//! 2. checked range arithmetic (before any system call);
//! 3. the live snapshot against the import snapshot (identity, size,
//!    modification marker) — before reading;
//! 4. the byte count after reading: short reads, unexpected EOF, and
//!    over-long responses fail with no partial bytes.
//!
//! A single `read_range` call is one range operation: it validates the
//! snapshot once before the read and verifies size/short-read afterwards.
//! Chunked consumers (core `materialize`, `BlobReader`, async FileReader
//! pumps, streams) issue one `read_range` per chunk, so validation
//! naturally runs before the first chunk and on every new chunk boundary.
//! The pre-read snapshot check plus the post-read size verification make a
//! finer boundary inside one system read unnecessary: content that changes
//! mid-read surfaces as a size/identity mismatch on this or the next
//! chunk, never as partial old bytes.
//!
//! The optional [`FileAccessPolicy`] hook runs after the snapshot check
//! and before the bytes are returned; denials fail the read with no
//! partial result.

use std::sync::Arc;

use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::policy::{FileAccessPolicy, FileGrant, FileResource, HostResourceId};
use boa_fapi_core::snapshot::{FileSnapshot, SnapshotState};
use boa_fapi_core::source::ByteSource;
use bytes::Bytes;

use crate::capability::FsRegistry;

/// Filesystem-backed byte source over a pre-authorized resource.
///
/// Holds only the opaque capability plus snapshots — never a path.
#[derive(Clone)]
pub struct FileSource {
    registry: FsRegistry,
    id: HostResourceId,
    import_snapshot: SnapshotState,
    len: u64,
    policy: Option<Arc<dyn FileAccessPolicy>>,
}

impl std::fmt::Debug for FileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSource")
            .field("id", &self.id)
            .field("import_snapshot", &self.import_snapshot)
            .field("len", &self.len)
            .field("has_policy", &self.policy.is_some())
            .finish()
    }
}

impl FileSource {
    /// Wraps `resource` with its import-time snapshot.
    ///
    /// Fails when the resource was closed, when the platform cannot
    /// guarantee a strong identity check on the open handle (non-Unix:
    /// use [`open_copy_on_import`] instead), or when the import snapshot
    /// cannot be captured; no partial source escapes.
    pub fn new(
        registry: &FsRegistry,
        resource: &crate::capability::RegisteredResource,
        policy: Option<Arc<dyn FileAccessPolicy>>,
    ) -> Result<Self, FileApiError> {
        if !crate::platform_has_strong_identity() {
            return Err(FileApiError::PermissionDenied);
        }
        Self::new_for_copy(registry, resource, policy)
    }

    /// Wraps `resource` without the strong-identity platform gate.
    ///
    /// `pub(crate)` backing constructor for [`open_copy_on_import`]: the
    /// copy path is safe on every platform because its result is immutable
    /// memory (no live handle survives), so it must stay available where
    /// direct [`FileSource::new`] is refused.
    pub(crate) fn new_for_copy(
        registry: &FsRegistry,
        resource: &crate::capability::RegisteredResource,
        policy: Option<Arc<dyn FileAccessPolicy>>,
    ) -> Result<Self, FileApiError> {
        let id = resource.id();
        let import_snapshot = registry.import_snapshot(id)?;
        let len = match &import_snapshot {
            SnapshotState::Memory => 0,
            SnapshotState::Filesystem(state) => state.size(),
            _ => 0,
        };
        Ok(Self {
            registry: registry.clone(),
            id,
            import_snapshot,
            len,
            policy,
        })
    }

    /// Returns the import-time snapshot this source validates against.
    pub fn import_snapshot(&self) -> &SnapshotState {
        &self.import_snapshot
    }

    /// Returns the opaque resource identifier (never a path or handle).
    pub fn resource_id(&self) -> HostResourceId {
        self.id
    }

    /// Releases the underlying host resource. Idempotent.
    pub fn close(&self) {
        self.registry.close(self.id);
    }

    /// Validates the live snapshot against the import snapshot.
    fn check_snapshot(&self, live: &SnapshotState) -> Result<(), FileApiError> {
        match (&self.import_snapshot, live) {
            (SnapshotState::Memory, SnapshotState::Memory) => Ok(()),
            (SnapshotState::Filesystem(expected), SnapshotState::Filesystem(actual)) => {
                check_file_snapshot(expected, actual)
            }
            // A memory/filesystem kind switch is always a replacement.
            _ => Err(FileApiError::SnapshotChanged),
        }
    }

    /// Runs the policy hook, if one is configured.
    fn check_policy(&self, live: &SnapshotState) -> Result<(), FileApiError> {
        if let Some(policy) = &self.policy {
            let grant = FileGrant::new(self.id, self.import_snapshot.clone());
            policy.authorize_read(&grant, live)?;
        }
        Ok(())
    }
}

/// Compares two filesystem snapshots.
///
/// Any difference in identity, size, or modification marker is a
/// replacement/change: the read fails with `SnapshotChanged` and no old
/// bytes leak. `mtime` alone is never trusted as sufficient: the identity
/// hash (platform file identity mixed with size/mtime) must match too.
fn check_file_snapshot(expected: &FileSnapshot, actual: &FileSnapshot) -> Result<(), FileApiError> {
    if expected == actual {
        Ok(())
    } else {
        Err(FileApiError::SnapshotChanged)
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn snapshot(&self) -> SnapshotState {
        self.import_snapshot.clone()
    }

    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<Bytes, FileApiError> {
        // 1. Cancellation before any I/O.
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        // 2. Checked arithmetic before any system call.
        if range.start > range.end {
            return Err(FileApiError::InvalidRange);
        }
        let len_u64 = range
            .end
            .checked_sub(range.start)
            .ok_or(FileApiError::InvalidRange)?;
        if range.end > self.len {
            return Err(FileApiError::InvalidRange);
        }
        let len = usize::try_from(len_u64).map_err(|_| {
            FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
        })?;
        if len == 0 {
            return Ok(Bytes::new());
        }
        // 3. Snapshot/policy validation before reading.
        let live = self.registry.live_snapshot(self.id)?;
        self.check_snapshot(&live)?;
        self.check_policy(&live)?;
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        // 4. Positional read, then exact-length verification: short reads,
        // unexpected EOF, and over-long responses fail with no partial
        // bytes.
        let bytes = self.registry.read_at(self.id, range.start, len)?;
        if bytes.len() != len {
            return Err(FileApiError::InvalidRange);
        }
        // Post-read identity confirmation: a replacement racing the read
        // surfaces here (or on the next chunk) instead of leaking old
        // bytes as a successful old snapshot.
        let after = self.registry.live_snapshot(self.id)?;
        self.check_snapshot(&after)?;
        Ok(Bytes::from(bytes))
    }
}

/// A [`FileResource`] adapter over a registered resource.
///
/// Lets host code treat a registry slot as the Boa-free resource
/// abstraction from `boa_fapi_core::policy`.
#[derive(Clone)]
pub struct HostFileSource {
    source: FileSource,
}

impl std::fmt::Debug for HostFileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFileSource")
            .field("source", &self.source)
            .finish()
    }
}

impl HostFileSource {
    /// Wraps `resource` with its import-time snapshot.
    ///
    /// Uses the copy-path constructor internally so the adapter stays
    /// available on every platform (weak targets never serve live-handle
    /// reads, but the adapter type still carries the import snapshot for
    /// `file_from_resource` validation and for `copy_on_import` flows).
    pub fn new(
        registry: &FsRegistry,
        resource: &crate::capability::RegisteredResource,
        policy: Option<Arc<dyn FileAccessPolicy>>,
    ) -> Result<Self, FileApiError> {
        Ok(Self {
            source: FileSource::new_for_copy(registry, resource, policy)?,
        })
    }

    /// Returns the underlying [`FileSource`].
    pub fn source(&self) -> &FileSource {
        &self.source
    }
}

impl FileResource for HostFileSource {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, FileApiError> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(FileApiError::InvalidRange)?;
        let bytes = self
            .source
            .read_range(offset..end, &CancellationToken::new())?;
        Ok(bytes.to_vec())
    }

    fn current_snapshot(&self) -> Result<SnapshotState, FileApiError> {
        self.source.registry.live_snapshot(self.source.id)
    }

    fn import_snapshot(&self) -> SnapshotState {
        self.source.import_snapshot.clone()
    }

    fn resource_id(&self) -> HostResourceId {
        self.source.id
    }

    fn close(&self) {
        self.source.close();
    }
}

/// Copies a registered resource into an immutable memory source.
///
/// The enforced fallback for platforms without a strong open-handle
/// identity (Windows and other non-Unix targets, where direct [`FileSource`]
/// construction is refused) and the recommended mode for untrusted JS:
/// the copy is a point-in-time snapshot, so later replacement races cannot
/// leak old bytes as new reads. Available on every platform. The returned
/// bytes are the complete resource content bounded by `max_bytes` (`==`
/// ok, `+1` rejected before allocation completes).
pub fn open_copy_on_import(
    registry: &FsRegistry,
    resource: &crate::capability::RegisteredResource,
    max_bytes: u64,
) -> Result<Bytes, FileApiError> {
    // Gated constructor bypass: safe on every platform because only
    // immutable memory escapes (the live handle is closed below).
    let source = FileSource::new_for_copy(registry, resource, None)?;
    let len = source.len();
    if len > max_bytes {
        return Err(FileApiError::ResourceLimit(
            boa_fapi_core::error::ResourceLimitKind::MaterializeBytes,
        ));
    }
    let capacity = usize::try_from(len).map_err(|_| {
        FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
    })?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(capacity).map_err(|_| {
        FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
    })?;
    let bytes = source.read_range(0..len, &CancellationToken::new())?;
    if bytes.len() as u64 != len {
        return Err(FileApiError::InvalidRange);
    }
    out.extend_from_slice(&bytes);
    // The live handle is no longer needed: the immutable copy is the only
    // thing that escapes. Close eagerly so a weak-platform copy never
    // retains an OS handle.
    source.close();
    Ok(Bytes::from(out))
}
