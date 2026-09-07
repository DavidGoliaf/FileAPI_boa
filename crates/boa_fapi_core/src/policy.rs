//! Host-side filesystem policy boundary: opaque capability types.
//!
//! The policy types live in `boa_fapi_core` (Boa-free) so that capability
//! reasoning never depends on the JS engine. No type here carries a
//! filesystem location, a handle value, or a secret name:
//!
//! - [`HostResourceId`] is an opaque host reference to an already-open
//!   read-only resource, created through [`FileResourceOpener`];
//! - [`FileGrant`] is the opaque capability handed to the reader;
//! - [`FileOpenRequest`] describes only a display name plus byte budget;
//! - [`FileResource`] is the Boa-free host abstraction for the already
//!   open read-only resource: byte reads plus opaque metadata for
//!   snapshot capture.
//!
//! Path handling (opening, canonicalization, root checks) lives behind the
//! [`FileResourceOpener`] host callback and must never surface a path into
//! JS, `DOMException` messages, tracing, blob URLs, or test artifacts.

use crate::file_api_error::FileApiError;
use crate::snapshot::SnapshotState;

/// Opaque handle to an already-open host resource.
///
/// The inner value is host-private (for example a registry slot); it is
/// never a location and is never exposed to JavaScript.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct HostResourceId(u64);

impl HostResourceId {
    /// Creates an opaque resource identifier.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the opaque identifier value.
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// Host request to authorize a filesystem import.
///
/// Carries only the opaque reference to the already-open resource plus the
/// host-chosen display name and an optional byte budget. A raw filesystem
/// location is never a valid input: the resource is already open, and JS
/// can never supply one.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileOpenRequest {
    /// Opaque reference to the already-open host resource.
    pub resource: HostResourceId,
    /// The display name the host wants JS to observe.
    pub display_name: String,
    /// Maximum bytes the host expects to expose, if known.
    pub max_bytes: Option<u64>,
}

impl FileOpenRequest {
    /// Creates a request for the already-open `resource` with `display_name`.
    pub fn new(
        resource: HostResourceId,
        display_name: impl Into<String>,
        max_bytes: Option<u64>,
    ) -> Self {
        Self {
            resource,
            display_name: display_name.into(),
            max_bytes,
        }
    }
}

/// Opaque capability granting reads of one pre-authorized resource.
///
/// Holds only an opaque resource identifier plus the import-time snapshot;
/// never a location, handle value, or secret name.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileGrant {
    /// The opaque host resource reference.
    pub resource: HostResourceId,
    /// The import-time snapshot the reader must match.
    pub snapshot: SnapshotState,
}

impl FileGrant {
    /// Creates a grant for `resource` with its import-time `snapshot`.
    pub fn new(resource: HostResourceId, snapshot: SnapshotState) -> Self {
        Self { resource, snapshot }
    }
}

/// Boa-free abstraction for an already-open read-only resource.
///
/// Implemented by the host (or by the `boa_fapi_fs` capability holder).
/// `read_at` performs positional reads without JS; `current_snapshot`
/// captures the live opaque metadata for per-chunk validation. Neither
/// method reveals a location: failures map to typed [`FileApiError`]s
/// whose messages carry no location, handle, or identity detail.
pub trait FileResource: Send + Sync {
    /// Reads exactly `len` bytes at `offset` (checked before I/O).
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, FileApiError>;

    /// Captures the live opaque snapshot for validation.
    fn current_snapshot(&self) -> Result<SnapshotState, FileApiError>;

    /// Returns the import-time snapshot captured at open.
    fn import_snapshot(&self) -> SnapshotState;

    /// Returns the opaque host resource identifier.
    fn resource_id(&self) -> HostResourceId;

    /// Releases the host resource. Idempotent; reads fail afterwards.
    fn close(&self);
}

/// Host callback that opens a pre-authorized resource for a request.
///
/// The callback performs any location handling internally
/// (canonicalization, root confinement, symlink/junction checks on the
/// already-open handle) and returns only an opaque [`HostResourceId`].
/// String-prefix checks such as `starts_with(root)` are forbidden;
/// identity must be verified on the open handle.
pub trait FileResourceOpener: Send + Sync {
    /// Opens the pre-authorized resource for `request`, or denies it.
    fn open(&self, request: &FileOpenRequest) -> Result<HostResourceId, FileApiError>;
}

/// Host-side policy boundary for filesystem imports and reads.
///
/// `authorize_open` runs before any JS `File` exists; `authorize_read`
/// runs before the first read and before every new range/chunk operation.
/// Denials carry no location, identity, or policy internals.
pub trait FileAccessPolicy: Send + Sync {
    /// Authorizes the import described by `request`.
    ///
    /// The default implementation denies every raw-path import.
    fn authorize_open(&self, request: &FileOpenRequest) -> Result<FileGrant, FileApiError>;

    /// Authorizes one more read of `grant` against the live `snapshot`.
    fn authorize_read(
        &self,
        grant: &FileGrant,
        snapshot: &SnapshotState,
    ) -> Result<(), FileApiError>;
}

/// Default policy: denies every import.
///
/// The host replaces this with an explicit approval (for example the
/// `boa_fapi_fs` registry policy) that only accepts already-open
/// resources; raw locations are never valid inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllPolicy;

impl FileAccessPolicy for DenyAllPolicy {
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
