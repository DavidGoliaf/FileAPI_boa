//! Host-owned capability registry for pre-authorized read-only resources.
//!
//! The host opens a file read-only **before** any JS `File` exists (any
//! path handling happens in host code, outside this crate) and hands the
//! open [`std::fs::File`] to [`FsRegistry::register`]. Registration
//! captures the opaque [`FileSnapshot`] on the open handle and returns a
//! [`RegisteredResource`] holding only an opaque [`HostResourceId`]: no
//! path, no handle value, no secret name ever leaves the registry.
//!
//! Reads go through the registry by opaque id only. Closing releases the
//! handle; later reads fail with typed errors.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::policy::HostResourceId;
use boa_fapi_core::snapshot::SnapshotState;

/// Host handle to a registered read-only resource.
///
/// Carries only the opaque [`HostResourceId`]; it cannot be forged from
/// JS, serialized into JS, or turned back into a path. Cloning shares the
/// registration; closing any clone closes the resource for all readers.
#[derive(Clone, Debug)]
pub struct RegisteredResource {
    id: HostResourceId,
    registry: FsRegistry,
}

impl RegisteredResource {
    /// Returns the opaque resource identifier.
    pub fn id(&self) -> HostResourceId {
        self.id
    }

    /// Closes the underlying resource. Idempotent.
    pub fn close(&self) {
        self.registry.close(self.id);
    }
}

struct Slot {
    file: SlotFile,
    import_snapshot: SnapshotState,
    closed: bool,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("import_snapshot", &self.import_snapshot)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// Debug-view of a slot without leaking handle values.
struct SlotFile {
    file: std::fs::File,
}

impl std::fmt::Debug for SlotFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotFile").finish_non_exhaustive()
    }
}

/// Registry owning already-open read-only host resources.
///
/// `Send + Sync + 'static` so it can back
/// [`FileResource`](boa_fapi_core::policy::FileResource) sources across
/// read jobs. All interior locking is short-lived: slow I/O never holds
/// the registry lock (positional reads run on a per-slot handle).
#[derive(Clone, Debug, Default)]
pub struct FsRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    slots: HashMap<u64, Slot>,
    next_id: u64,
}

impl FsRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an already-open read-only file.
    ///
    /// Captures the opaque import snapshot on the open handle with safe
    /// Rust APIs. Returns the opaque [`RegisteredResource`] capability.
    pub fn register(&self, file: std::fs::File) -> Result<RegisteredResource, FileApiError> {
        let snapshot = crate::identity::capture(&file).map_err(|error| map_io(&error))?;
        let state = SnapshotState::Filesystem(snapshot);
        let mut inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
        let id_value = inner.next_id;
        inner.next_id = inner
            .next_id
            .wrapping_add(1)
            .max(1)
            .max(id_value.wrapping_add(1));
        // First registration uses id 1 (0 stays reserved as "no resource").
        let id_value = if id_value == 0 { 1 } else { id_value };
        if inner.next_id == 0 {
            inner.next_id = 1;
        }
        inner.slots.insert(
            id_value,
            Slot {
                file: SlotFile { file },
                import_snapshot: state,
                closed: false,
            },
        );
        Ok(RegisteredResource {
            id: HostResourceId::new(id_value),
            registry: self.clone(),
        })
    }

    /// Returns the import-time snapshot for `id`.
    pub(crate) fn import_snapshot(
        &self,
        id: HostResourceId,
    ) -> Result<SnapshotState, FileApiError> {
        let inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
        inner
            .slots
            .get(&id.get())
            .filter(|slot| !slot.closed)
            .map(|slot| slot.import_snapshot.clone())
            .ok_or(FileApiError::NotFound)
    }

    /// Captures the live snapshot for `id` from the open handle.
    pub(crate) fn live_snapshot(&self, id: HostResourceId) -> Result<SnapshotState, FileApiError> {
        let inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
        let slot = inner
            .slots
            .get(&id.get())
            .filter(|s| !s.closed)
            .ok_or(FileApiError::NotFound)?;
        let snapshot = crate::identity::capture(&slot.file.file).map_err(|error| map_io(&error))?;
        Ok(SnapshotState::Filesystem(snapshot))
    }

    /// Reads exactly `len` bytes at `offset` from the open handle.
    ///
    /// Uses positional reads so concurrent chunk reads never share a
    /// cursor. The registry lock is released before I/O.
    pub(crate) fn read_at(
        &self,
        id: HostResourceId,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, FileApiError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
            let slot = inner
                .slots
                .get(&id.get())
                .filter(|s| !s.closed)
                .ok_or(FileApiError::NotFound)?;
            let mut out = vec![0u8; len];
            let mut filled = 0usize;
            while filled < len {
                match slot
                    .file
                    .file
                    .read_at(&mut out[filled..], offset + filled as u64)
                {
                    Ok(0) => return Err(FileApiError::InvalidRange),
                    Ok(n) => filled += n,
                    Err(error) => return Err(map_io(&error)),
                }
            }
            Ok(out)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
            let slot = inner
                .slots
                .get_mut(&id.get())
                .filter(|s| !s.closed)
                .ok_or(FileApiError::NotFound)?;
            if slot.file.file.seek(SeekFrom::Start(offset)).is_err() {
                return Err(FileApiError::InvalidRange);
            }
            let mut out = vec![0u8; len];
            if slot.file.file.read_exact(&mut out).is_err() {
                return Err(FileApiError::InvalidRange);
            }
            Ok(out)
        }
    }

    /// Closes the resource. Idempotent; later reads fail as `NotFound`.
    pub fn close(&self, id: HostResourceId) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(slot) = inner.slots.get_mut(&id.get())
        {
            slot.closed = true;
        }
    }

    /// Returns the number of registered (including closed) slots.
    #[allow(dead_code)]
    pub(crate) fn slot_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.slots.len())
            .unwrap_or(0)
    }
}

/// Maps an I/O error to a typed [`FileApiError`] without path detail.
///
/// The message surface of `FileApiError` is already path-free; this
/// mapping only selects the variant.
pub(crate) fn map_io(error: &std::io::Error) -> FileApiError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => FileApiError::NotFound,
        ErrorKind::PermissionDenied => FileApiError::PermissionDenied,
        ErrorKind::WouldBlock => FileApiError::FileLocked,
        _ => {
            // `ErrorKind::Uncategorized` covers OS lock/contention failures
            // (e.g. Windows sharing violations surfaced as opaque errors);
            // treat those as locks, everything else as a generic read
            // failure that JS observes as `NotReadableError`.
            let raw = error.raw_os_error().unwrap_or(0);
            if is_lock_like(raw) {
                FileApiError::FileLocked
            } else {
                FileApiError::SnapshotChanged
            }
        }
    }
}

/// Returns `true` for OS error codes that indicate locking/sharing
/// contention rather than missing or changed content.
#[cfg(windows)]
fn is_lock_like(raw: i32) -> bool {
    // ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33),
    // ERROR_LOCK_FAILED / ERROR_BUSY variants.
    matches!(raw, 32 | 33 | 107 | 170)
}

#[cfg(not(windows))]
fn is_lock_like(_raw: i32) -> bool {
    false
}
