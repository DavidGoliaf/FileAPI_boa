//! Host-side policy boundary implementations.
//!
//! - [`DenyRawPathPolicy`] is the default: every import is denied, so a
//!   raw path can never become a capability.
//! - [`RegistryPolicy`] approves only resources already registered in the
//!   [`FsRegistry`]: the request must reference a live slot whose live
//!   snapshot still matches the import snapshot. Approval runs before any
//!   JS `File` exists; per-chunk reads re-validate through
//!   [`authorize_read`].
//! - [`RootConfinedPolicy`] wraps an inner policy and additionally
//!   verifies the live snapshot on the open handle (canonical identity
//!   comparison, never a string-prefix check). `..`, symlink escape,
//!   Windows junction/reparse-point escape, and path aliasing cannot
//!   bypass it: there is no path input at all, only the open-handle
//!   identity captured at registration time.
//!
//! Denials carry no path, identity, or policy internals.

use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::policy::{FileAccessPolicy, FileGrant, FileOpenRequest};
use boa_fapi_core::snapshot::SnapshotState;

/// Default policy: denies every filesystem import.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyRawPathPolicy;

impl FileAccessPolicy for DenyRawPathPolicy {
    fn authorize_open(&self, _request: &FileOpenRequest) -> Result<FileGrant, FileApiError> {
        Err(FileApiError::PermissionDenied)
    }

    fn authorize_read(
        &self,
        _grant: &FileGrant,
        _snapshot: &SnapshotState,
    ) -> Result<(), FileApiError> {
        Err(FileApiError::PermissionDenied)
    }
}

/// Policy approving only resources already registered in the registry.
///
/// `authorize_open` accepts a request only when its opaque resource id
/// resolves to a live, snapshot-stable slot. `authorize_read` re-checks
/// that the live snapshot still matches the grant snapshot before every
/// new range/chunk operation.
#[derive(Clone, Debug)]
pub struct RegistryPolicy {
    registry: crate::capability::FsRegistry,
}

impl RegistryPolicy {
    /// Creates a policy bound to `registry`.
    pub fn new(registry: crate::capability::FsRegistry) -> Self {
        Self { registry }
    }
}

impl FileAccessPolicy for RegistryPolicy {
    fn authorize_open(&self, request: &FileOpenRequest) -> Result<FileGrant, FileApiError> {
        let import_snapshot = self.registry.import_snapshot(request.resource)?;
        let live = self.registry.live_snapshot(request.resource)?;
        if import_snapshot != live {
            return Err(FileApiError::SnapshotChanged);
        }
        if let Some(max) = request.max_bytes {
            let size = match &import_snapshot {
                SnapshotState::Memory => 0,
                SnapshotState::Filesystem(state) => state.size(),
                _ => 0,
            };
            if size > max {
                return Err(FileApiError::ResourceLimit(
                    boa_fapi_core::error::ResourceLimitKind::BlobSize,
                ));
            }
        }
        Ok(FileGrant::new(request.resource, import_snapshot))
    }

    fn authorize_read(
        &self,
        grant: &FileGrant,
        snapshot: &SnapshotState,
    ) -> Result<(), FileApiError> {
        if grant.snapshot != *snapshot {
            return Err(FileApiError::SnapshotChanged);
        }
        let live = self.registry.live_snapshot(grant.resource)?;
        if grant.snapshot != live {
            return Err(FileApiError::SnapshotChanged);
        }
        Ok(())
    }
}

/// Root-confined policy: verifies open-handle identity, never path strings.
///
/// Wraps an inner policy and additionally requires the live snapshot to
/// match the grant snapshot on every read. Because verification happens
/// on the open handle's opaque identity (captured at registration), path
/// aliasing, `..` traversal, symlink/junction/reparse-point escapes, and
/// string-prefix bypasses are structurally impossible: no path string is
/// ever compared.
#[derive(Clone, Debug)]
pub struct RootConfinedPolicy<P> {
    inner: P,
}

impl<P> RootConfinedPolicy<P> {
    /// Wraps `inner` with open-handle identity verification.
    pub fn new(inner: P) -> Self {
        Self { inner }
    }

    /// Returns the wrapped policy.
    pub fn inner(&self) -> &P {
        &self.inner
    }
}

impl<P: FileAccessPolicy> FileAccessPolicy for RootConfinedPolicy<P> {
    fn authorize_open(&self, request: &FileOpenRequest) -> Result<FileGrant, FileApiError> {
        self.inner.authorize_open(request)
    }

    fn authorize_read(
        &self,
        grant: &FileGrant,
        snapshot: &SnapshotState,
    ) -> Result<(), FileApiError> {
        // Identity comparison on opaque snapshots only — never a
        // `starts_with(root)` string check.
        if grant.snapshot != *snapshot {
            return Err(FileApiError::SnapshotChanged);
        }
        self.inner.authorize_read(grant, snapshot)
    }
}
