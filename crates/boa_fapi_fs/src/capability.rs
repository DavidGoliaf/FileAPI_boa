//! Host-owned capability registry for pre-authorized read-only resources.
//!
//! The host opens a file read-only **before** any JS `File` exists (any
//! location handling happens in host code, outside this crate) and hands the
//! open [`std::fs::File`] to [`FsRegistry::register`]. Registration
//! captures the opaque [`FileSnapshot`] on the open handle and returns a
//! [`RegisteredResource`] holding only an opaque [`HostResourceId`]: no
//! location, no handle value, no secret name ever leaves the registry.
//!
//! Reads go through the registry by opaque id only. Closing removes the
//! slot and drops the OS handle immediately; later reads fail with typed
//! errors. [`FsRegistry::close_all`] drops every slot at once and is the
//! primitive the runtime shutdown path uses (via per-import closers).
//!
//! Locking discipline: the registry [`Mutex`] guards only the slot map and
//! is never held across I/O. Every operation clones (or `try_clone`s) the
//! needed state under a short lock, drops the lock, and only then touches
//! the OS (metadata reads, positional reads).

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
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("import_snapshot", &self.import_snapshot)
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
/// read jobs. The [`Mutex`] guards only the slot map and is never held
/// across I/O: `live_snapshot` clones the handle out of the map under a
/// short lock and reads metadata after the lock is dropped; `read_at`
/// clones the handle the same way (Unix additionally uses lock-free
/// positional reads on the clone; non-Unix clones an independent cursor).
#[derive(Clone, Debug, Default)]
pub struct FsRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    slots: HashMap<u64, Slot>,
    next_id: u64,
    closers: CloserList,
}

/// Shutdown closers pending exactly-once execution.
///
/// `FnOnce` is not `Debug`; this wrapper reports only the pending count.
#[derive(Default)]
struct CloserList {
    pending: Vec<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for CloserList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloserList")
            .field("pending", &self.pending.len())
            .finish()
    }
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
        let snapshot = crate::identity::capture(&file).map_err(map_io)?;
        let state = SnapshotState::Filesystem(snapshot);
        let mut inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
        let id_value = inner.next_id;
        inner.next_id = inner
            .next_id
            .wrapping_add(1)
            .max(1)
            .max(id_value.wrapping_add(1));
        // First registration uses id 1 (0 stays reserved as "no resource").
        // `next_id` may exceed the just-issued id when ids were consumed
        // without insertion (none today); keep them monotonic regardless.
        let id_value = if id_value == 0 { 1 } else { id_value };
        if inner.next_id <= id_value {
            inner.next_id = id_value.wrapping_add(1).max(1);
        }
        if inner.next_id == 0 {
            inner.next_id = 1;
        }
        inner.slots.insert(
            id_value,
            Slot {
                file: SlotFile { file },
                import_snapshot: state,
            },
        );
        Ok(RegisteredResource {
            id: HostResourceId::new(id_value),
            registry: self.clone(),
        })
    }

    /// Returns the import-time snapshot for `id`.
    ///
    /// Public so host policy code and integration tests can compare
    /// import-vs-live snapshots without touching handles. Fails with
    /// `NotFound` when the slot is missing or was closed.
    pub fn import_snapshot(&self, id: HostResourceId) -> Result<SnapshotState, FileApiError> {
        let inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
        inner
            .slots
            .get(&id.get())
            .map(|slot| slot.import_snapshot.clone())
            .ok_or(FileApiError::NotFound)
    }

    /// Captures the live snapshot for `id` from the open handle.
    ///
    /// Clones the handle out of the map under a short lock, then reads
    /// metadata after the lock is dropped — no I/O under the mutex.
    /// Public so per-chunk validators outside this crate (e.g. the
    /// `boa_fapi` fs adapter) can revalidate without touching handles.
    pub fn live_snapshot(&self, id: HostResourceId) -> Result<SnapshotState, FileApiError> {
        let handle = self.cloned_handle(id)?;
        let snapshot = crate::identity::capture(&handle).map_err(map_io)?;
        Ok(SnapshotState::Filesystem(snapshot))
    }

    /// Clones the OS handle for `id` under a short map lock.
    ///
    /// The lock is released before the caller performs any I/O on the
    /// clone. Fails with `NotFound` when the slot is missing or closed.
    fn cloned_handle(&self, id: HostResourceId) -> Result<std::fs::File, FileApiError> {
        let handle = {
            let inner = self.inner.lock().map_err(|_| FileApiError::Internal)?;
            let slot = inner.slots.get(&id.get()).ok_or(FileApiError::NotFound)?;
            slot.file.file.try_clone()
        };
        handle.map_err(map_io)
    }

    /// Reads exactly `len` bytes at `offset` from the open handle.
    ///
    /// Clones the handle under a short map lock, then performs all I/O on
    /// the clone after the lock is dropped — the global mutex is never held
    /// during slow reads. Unix uses lock-free positional reads on the clone
    /// so concurrent chunk reads never share a cursor; other platforms use
    /// an independent seek+read cursor on the per-call clone, so concurrent
    /// reads of any slot never disturb each other either.
    ///
    /// Public so external [`FileResource`](boa_fapi_core::policy::FileResource)
    /// adapters can reuse the same lock-free positional path.
    pub fn read_at(
        &self,
        id: HostResourceId,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, FileApiError> {
        let handle = self.cloned_handle(id)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let mut out = vec![0u8; len];
            let mut filled = 0usize;
            while filled < len {
                match handle.read_at(&mut out[filled..], offset + filled as u64) {
                    Ok(0) => return Err(FileApiError::InvalidRange),
                    Ok(n) => filled += n,
                    Err(error) => return Err(map_io(error)),
                }
            }
            Ok(out)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut handle = handle;
            if handle.seek(SeekFrom::Start(offset)).is_err() {
                return Err(FileApiError::InvalidRange);
            }
            let mut out = vec![0u8; len];
            if handle.read_exact(&mut out).is_err() {
                return Err(FileApiError::InvalidRange);
            }
            Ok(out)
        }
    }

    /// Closes the resource, dropping the OS handle immediately. Idempotent;
    /// later reads fail as `NotFound`.
    ///
    /// The slot is removed from the map, so the [`std::fs::File`] is
    /// dropped (and the OS handle released) before this returns — not
    /// deferred to registry destruction.
    pub fn close(&self, id: HostResourceId) {
        if let Ok(mut inner) = self.inner.lock()
            && inner.slots.remove(&id.get()).is_some()
        {}
    }

    /// Closes every registered resource, dropping all OS handles.
    ///
    /// Slots are removed from the map (each [`std::fs::File`] is dropped
    /// inline), so this releases handles immediately. Idempotent.
    pub fn close_all(&self) {
        let slots = if let Ok(mut inner) = self.inner.lock() {
            std::mem::take(&mut inner.slots)
        } else {
            return;
        };
        drop(slots);
    }

    /// Registers a one-shot closer invoked on runtime shutdown.
    ///
    /// The closer is stored (not run) and fires exactly once the next time
    /// [`FsRegistry::run_closers`] executes. Closers are plain
    /// `Box<dyn FnOnce()>` callbacks, so `boa_fapi` can hook the runtime
    /// shutdown path without `boa_fapi_fs` depending on the engine.
    /// Registration itself never runs user code and never fails.
    pub fn on_shutdown(&self, closer: impl FnOnce() + Send + 'static) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closers.pending.push(Box::new(closer));
        }
    }

    /// Runs every registered shutdown closer exactly once.
    ///
    /// Idempotent: a second call finds no closers left. Each closer runs
    /// outside the map lock (closures are taken out first), so a closer
    /// that calls [`FsRegistry::close`] / [`FsRegistry::close_all`] cannot
    /// deadlock.
    pub fn run_closers(&self) {
        let pending = if let Ok(mut inner) = self.inner.lock() {
            std::mem::take(&mut inner.closers.pending)
        } else {
            return;
        };
        for closer in pending {
            closer();
        }
    }

    /// Returns the number of live (open) slots.
    ///
    /// Closed slots are removed immediately, so this counts exactly the
    /// OS handles currently held. Used by shutdown tests to prove handles
    /// are released.
    pub fn live_slot_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.slots.len())
            .unwrap_or(0)
    }

    /// Returns the number of registered (including closed) slots.
    #[allow(dead_code)]
    pub(crate) fn slot_count(&self) -> usize {
        self.live_slot_count()
    }
}

/// Maps an I/O error to a typed [`FileApiError`] without location detail.
///
/// The message surface of `FileApiError` is already location-free; this
/// mapping only selects the variant.
pub(crate) fn map_io(error: std::io::Error) -> FileApiError {
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
