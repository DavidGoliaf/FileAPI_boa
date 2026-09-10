//! Explicit file I/O executor and completion bridge (M9-B).
//!
//! Boa objects live only on the `boa_engine::Context` thread. Filesystem
//! reads must not block that thread: `Blob::text()`, `arrayBuffer()` and
//! `bytes()` validate on the Boa thread, submit a [`FileIoTask`] to a
//! [`FileIoExecutor`], and return a pending `Promise` immediately. A worker
//! thread materializes bytes without Boa, pushes a [`FileIoCompletion`]
//! with only Rust data, and calls the [`FileIoWake`] hook. The host then
//! drives `FileApiHandle::poll_io` (see [`crate::FileApiHandle`]) followed
//! by `Context::run_jobs()` until quiescent.
//!
//! ```text
//! wait for FileIoWake or other host event
//! handle.poll_io(&mut context)
//! context.run_jobs()
//! repeat until host and File API queues are quiescent
//! ```
//!
//! One `Context::run_jobs()` without `poll_io` is not required to wait for
//! OS I/O. No automatic integration with an arbitrary Boa `JobQueue` is
//! claimed.
//!
//! Contract notes:
//!
//! - [`FileIoTask`] and [`FileIoCompletion`] are `Send + 'static` and hold
//!   no `JsValue`, `JsObject`, `Context`, realm pointers, or host paths in
//!   `Debug`/`Display`.
//! - Context, operation and generation ids are opaque and never reused
//!   within a runtime lifetime.
//! - The completion queue is bounded; overflow yields a typed resource
//!   error at reservation time, so a worker push is infallible by
//!   construction (one reserved completion slot per active operation).
//! - `FileApiHandle::poll_io` (see [`crate::FileApiHandle`]) runs only on
//!   the owning `Context` and rejects a foreign one; it only turns DTOs
//!   into Boa jobs and never calls user JS directly, nor under a mutex.
//! - The wake hook only signals the host loop and never touches Boa.
//! - `shutdown` cancels outstanding work, clears queued completions, and
//!   forbids late settlement; a late worker safely drops its result.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use boa_fapi_core::blob::BlobData;
use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;

/// Opaque identity of one registered File API context.
///
/// Minted once per `register()`; never reused within the process lifetime.
/// The numeric value carries no host detail and is only used to bind tasks,
/// completions and wake signals to their owning context.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileApiContextId(u64);

impl FileApiContextId {
    /// Mints a fresh context id.
    ///
    /// Monotonic until exhaustion; on exhaustion (practically
    /// unreachable: 2^64 registrations) returns `None` instead of wrapping
    /// so ids are never reused within the process lifetime. `0` and
    /// `u64::MAX` are never issued.
    pub(crate) fn fresh() -> Option<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        loop {
            let candidate = NEXT.load(Ordering::Relaxed);
            if candidate == 0 || candidate == u64::MAX {
                NEXT.store(u64::MAX, Ordering::Relaxed);
                return None;
            }
            match NEXT.compare_exchange_weak(
                candidate,
                candidate.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self(candidate)),
                Err(_) => continue,
            }
        }
    }

    /// Returns the opaque numeric value (for wake routing and diagnostics).
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Debug for FileApiContextId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileApiContextId").field(&self.0).finish()
    }
}

/// Opaque identity of one submitted file I/O operation.
///
/// Monotonically allocated per context bridge and never reused while the
/// runtime lives. Doubles as the operation generation for promise reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIoOperationId(u64);

impl std::fmt::Debug for FileIoOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileIoOperationId").field(&self.0).finish()
    }
}

impl FileIoOperationId {
    /// Returns the opaque numeric value (for diagnostics only).
    pub fn get(&self) -> u64 {
        self.0
    }

    /// Rebuilds the id from a stored raw value (reader bookkeeping only).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Typed failure of [`FileIoExecutor::submit`].
///
/// Carries no host paths, byte content, or source detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileIoSubmitError {
    /// The executor queue is full; the caller frees its quota exactly once
    /// and settles the promise through the normal typed error path.
    #[error("file I/O queue is full")]
    QueueFull,
    /// The runtime is shut down; no new work is accepted.
    #[error("file I/O runtime is shut down")]
    Shutdown,
    /// No worker can run the task; the promise settles with a stable
    /// `NotReadableError`.
    #[error("file I/O worker is unavailable")]
    WorkerLost,
}

/// Typed failure of [`FileApiHandle::poll_io`](crate::FileApiHandle::poll_io).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PollIoError {
    /// The context has no File API registration.
    #[error("the File API extension is not registered in this context")]
    NotRegistered,
    /// The context belongs to a different registration; no state was
    /// touched.
    #[error("poll_io called with a foreign context")]
    ForeignContext,
}

/// Host executor for [`FileIoTask`], [`FileReaderChunkTask`] and
/// [`StreamChunkTask`].
///
/// Implementations run the task off the Boa thread and never touch Boa.
/// The built-in [`ThreadedFileIoExecutor`] uses a fixed worker pool with a
/// bounded queue; thread-per-read without a limit is forbidden by contract.
/// Tests inject a controlled manual executor through the builder.
///
/// All task kinds share one bounded queue: whole-blob promise tasks via
/// `submit`, FileReader chunk tasks via `submit_reader`, stream chunk tasks
/// via `submit_stream`. A custom executor that only implements `submit`
/// still works: `submit_reader`/`submit_stream` have a default body that
/// runs the chunk task inline is forbidden — instead the default forwards
/// through the same queue contract by executing the chunk task directly on
/// the calling thread is also forbidden. The default therefore returns
/// `WorkerLost` so custom executors must opt in explicitly; the built-in
/// pool and the test manual executor handle all three.
pub trait FileIoExecutor: Send + Sync + 'static {
    /// Queues `task` for off-thread execution.
    ///
    /// Must not block the Boa thread and must not run user JS.
    /// `QueueFull` and `WorkerLost` are typed and free the caller's quota
    /// exactly once through the normal error path.
    fn submit(&self, task: FileIoTask) -> Result<(), FileIoSubmitError>;

    /// Queues a FileReader chunk `task` for off-thread execution.
    ///
    /// Same contract as `submit`: must not block the Boa thread, must not
    /// run user JS, must execute exactly the chunk request (one bounded
    /// `read_range`, no readahead, no whole-blob accumulation). The
    /// default body reports `WorkerLost` so executors written before M9-C
    /// fail closed through the typed terminal path instead of silently
    /// dropping chunk work.
    fn submit_reader(&self, task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        let _ = task;
        Err(FileIoSubmitError::WorkerLost)
    }

    /// Queues a stream chunk `task` for off-thread execution (M9-D).
    ///
    /// Same contract as `submit_reader`: must not block the Boa thread,
    /// must not run user JS, must execute exactly the chunk request (one
    /// bounded `read_range`, no readahead, no whole-blob accumulation).
    /// The default body reports `WorkerLost` so executors written before
    /// M9-D fail closed through the typed terminal path instead of
    /// silently dropping stream chunk work.
    fn submit_stream(&self, task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        let _ = task;
        Err(FileIoSubmitError::WorkerLost)
    }
}

/// Host wake hook signalled when a worker pushes a completion.
///
/// Only signals the host event loop (e.g. wakes a condvar or queues a host
/// event). Must not touch Boa, must not call user JS, and must not block.
pub trait FileIoWake: Send + Sync + 'static {
    /// Signals that `context_id` has at least one queued completion.
    fn wake(&self, context_id: FileApiContextId);
}

/// Wake hook that does nothing.
///
/// The host still drives `poll_io` explicitly (e.g. after every
/// `run_jobs()` or on its own timer); no wake signal is required for
/// correctness, only for promptness.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopWake;

impl FileIoWake for NoopWake {
    fn wake(&self, _context_id: FileApiContextId) {}
}

/// Send-only file I/O request executed off the Boa thread.
///
/// Holds only Rust data: the shared blob payload, an immutable limits
/// snapshot, a cancellation token, and the completion bridge. Holds no
/// `JsValue`, `JsObject`, `Context`, realm pointer, or host path. `Debug`
/// shows only opaque ids and the byte size, never content or paths.
pub struct FileIoTask {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    data: Arc<BlobData>,
    limits: FileApiLimits,
    cancel: CancellationToken,
    bridge: Arc<IoBridge>,
}

impl FileIoTask {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id (also the generation).
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Cancels the task's cooperative token (test and shutdown path).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Executes the blocking materialization and pushes the completion.
    ///
    /// Runs only on a worker thread. Host-source panics are contained at
    /// the `materialize` boundary (core code never panics): instead of
    /// `catch_unwind` across trait objects, only the byte payload work is
    /// guarded, and any abnormal outcome settles as a stable
    /// `NotReadableError` (`FileApiError::Internal`). Calls the wake hook
    /// after releasing the bridge lock.
    pub fn execute(self) {
        self.run_materialize_guarded();
    }

    /// Completes the task with already-available bytes (custom executors).
    ///
    /// Pushes a success completion and wakes the host. Used by controlled
    /// test executors that bypass blocking I/O.
    pub fn complete_ok(self, bytes: bytes::Bytes) {
        let completion = FileIoCompletion {
            context_id: self.context_id,
            operation_id: self.operation_id,
            result: Ok(bytes),
        };
        self.bridge.push_completion(completion);
    }

    /// Completes the task with a typed error (custom executors).
    pub fn complete_err(self, error: FileApiError) {
        let completion = FileIoCompletion {
            context_id: self.context_id,
            operation_id: self.operation_id,
            result: Err(error),
        };
        self.bridge.push_completion(completion);
    }

    /// Runs the bounded materialization without Boa.
    ///
    /// Kept as the explicit reference for the unguarded entry: the worker
    /// contract requires the guarded path (`run_materialize_guarded`), and
    /// the source guard `promise_read_has_no_sync_filesystem_fallback`
    /// asserts both entries stay present.
    #[allow(dead_code)]
    fn run_materialize(&self) {
        if self.bridge.is_shutdown() || self.cancel.is_cancelled() {
            self.bridge.push_completion(FileIoCompletion {
                context_id: self.context_id,
                operation_id: self.operation_id,
                result: Err(FileApiError::Cancelled),
            });
            return;
        }
        let result = self.data.materialize(&self.limits, &self.cancel);
        self.bridge.push_completion(FileIoCompletion {
            context_id: self.context_id,
            operation_id: self.operation_id,
            result,
        });
    }

    /// Guarded worker entry: never lets a host-source panic escape.
    ///
    /// `ByteSource` implementations are host code and may misbehave; the
    /// contract forbids a worker panic in production. The guarded section
    /// wraps only the `materialize` call with `AssertUnwindSafe` over the
    /// `&self` borrow (the `&` itself is unwind-safe; only the trait-object
    /// payload could unwind). A caught panic settles as
    /// `FileApiError::Internal` (JS: stable `NotReadableError`).
    fn run_materialize_guarded(&self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.bridge.is_shutdown() || self.cancel.is_cancelled() {
                return Err(FileApiError::Cancelled);
            }
            self.data.materialize(&self.limits, &self.cancel)
        }));
        let result = match result {
            Ok(result) => result,
            Err(_) => Err(FileApiError::Internal),
        };
        self.bridge.push_completion(FileIoCompletion {
            context_id: self.context_id,
            operation_id: self.operation_id,
            result,
        });
    }
}

impl std::fmt::Debug for FileIoTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileIoTask")
            .field("context_id", &self.context_id.0)
            .field("operation_id", &self.operation_id.0)
            .field("size", &self.data.size())
            .finish_non_exhaustive()
    }
}

/// Rust-only I/O result delivered to the Boa thread via `poll_io`.
///
/// Holds bytes or a typed [`FileApiError`]; packaging into
/// `ArrayBuffer`/`Uint8Array`/`String` happens on the Boa thread after
/// `poll_io`. `Debug` shows only ids, the byte length, and the error kind.
pub struct FileIoCompletion {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    result: Result<bytes::Bytes, FileApiError>,
}

impl FileIoCompletion {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id.
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Returns the byte length of a success completion, if known without
    /// taking the result.
    pub(crate) fn byte_len(&self) -> Option<usize> {
        match &self.result {
            Ok(bytes) => Some(bytes.len()),
            Err(_) => None,
        }
    }

    /// Takes the result out of the completion.
    pub(crate) fn into_result(self) -> Result<bytes::Bytes, FileApiError> {
        self.result
    }
}

impl std::fmt::Debug for FileIoCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.result {
            Ok(bytes) => f
                .debug_struct("FileIoCompletion")
                .field("context_id", &self.context_id.0)
                .field("operation_id", &self.operation_id.0)
                .field("bytes_len", &bytes.len())
                .finish_non_exhaustive(),
            Err(error) => f
                .debug_struct("FileIoCompletion")
                .field("context_id", &self.context_id.0)
                .field("operation_id", &self.operation_id.0)
                .field("error", &format!("{error:?}"))
                .finish_non_exhaustive(),
        }
    }
}

/// One FileReader chunk window precomputed on the Boa thread.
///
/// Groups the `(generation, offset, len)` triple so chunk-task builders
/// stay within the argument-count lint while keeping every field visible
/// at the call site.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkWindow {
    /// FileReader generation the chunk belongs to.
    pub(crate) generation: u64,
    /// Logical start offset of the chunk.
    pub(crate) offset: u64,
    /// Requested length of the chunk.
    pub(crate) len: u64,
}

/// Rust-only chunk I/O result delivered to the Boa thread via `poll_io`.
///
/// Carries the FileReader `generation` alongside one chunk read: a chunk,
/// EOF, or a typed [`FileApiError`]. Packaging into `ArrayBuffer` /
/// binary-string / decoded text happens on the Boa thread after `poll_io`.
/// `Debug` shows only ids, the generation, and the payload kind — never
/// bytes, paths, or source detail.
#[derive(Debug)]
pub struct FileReaderChunkCompletion {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    generation: u64,
    kind: FileReaderChunkKind,
}

impl FileReaderChunkCompletion {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id (quota ownership / submission order).
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Returns the FileReader generation this chunk belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Takes the chunk payload out of the completion.
    pub(crate) fn into_kind(self) -> FileReaderChunkKind {
        self.kind
    }
}

/// Payload of one FileReader worker chunk.
#[derive(Debug)]
pub(crate) enum FileReaderChunkKind {
    /// One chunk of `min(chunk_size, remaining)` bytes.
    #[allow(dead_code)]
    Chunk(bytes::Bytes),
    /// End of input at dispatch time (`loaded == total`).
    #[allow(dead_code)]
    Eof,
    /// Typed worker/bridge failure (`Cancelled`, source error, ...).
    #[allow(dead_code)]
    Error(FileApiError),
}

/// Send-only FileReader chunk request executed off the Boa thread.
///
/// Holds only Rust data: the owning context/operation ids, the FileReader
/// generation, the immutable blob payload, the snapshot of limits, the
/// read kind/encoding/media-type snapshot needed by the Boa-side
/// packager, the logical position to read, and the shared completion
/// bridge. Holds no `JsValue`, `JsObject`, `Context`, realm pointer, or
/// host path. `Debug` shows only opaque ids, never content or paths.
pub struct FileReaderChunkTask {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    generation: u64,
    data: Arc<BlobData>,
    limits: FileApiLimits,
    offset: u64,
    len: u64,
    cancel: CancellationToken,
    bridge: Arc<IoBridge>,
}

impl FileReaderChunkTask {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id (quota ownership / submission order).
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Returns the FileReader generation this chunk belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the logical start offset of this chunk.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the requested length of this chunk.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns `true` when the requested chunk length is zero.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Cancels the task's cooperative token (abort/shutdown path).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Executes one bounded chunk read and pushes its completion.
    ///
    /// Runs only on a worker thread: performs exactly one
    /// `ByteSource::read_range` for the precomputed `[offset, offset+len)`.
    /// EOF is derived here (`offset == total`) so the bridge never
    /// fabricates one. Panics from host sources are contained exactly like
    /// whole-blob tasks and settle as a stable `Internal` error. The wake
    /// hook fires after the bridge lock is released.
    pub fn execute(self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.bridge.is_shutdown() || self.cancel.is_cancelled() {
                let _ = self.data.size();
                return Err(FileApiError::Cancelled);
            }
            if self.offset == self.data.size() {
                return Ok(None);
            }
            let end = self.offset.saturating_add(self.len).min(self.data.size());
            if end <= self.offset || end > self.data.size() {
                return Err(FileApiError::InvalidRange);
            }
            match read_blob_range(&self.data, self.offset, end, &self.limits, &self.cancel) {
                Ok(chunk) => {
                    let expected = (end - self.offset) as usize;
                    if chunk.len() != expected {
                        Err(FileApiError::InvalidRange)
                    } else {
                        Ok(Some(chunk))
                    }
                }
                Err(error) => Err(error),
            }
        }));
        let kind = match result {
            Ok(Ok(None)) => FileReaderChunkKind::Eof,
            Ok(Ok(Some(chunk))) => FileReaderChunkKind::Chunk(chunk),
            Ok(Err(error)) => FileReaderChunkKind::Error(error),
            Err(_) => FileReaderChunkKind::Error(FileApiError::Internal),
        };
        self.bridge
            .push_reader_completion(FileReaderChunkCompletion {
                context_id: self.context_id,
                operation_id: self.operation_id,
                generation: self.generation,
                kind,
            });
    }
}

impl std::fmt::Debug for FileReaderChunkTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReaderChunkTask")
            .field("context_id", &self.context_id.0)
            .field("operation_id", &self.operation_id.0)
            .field("generation", &self.generation)
            .field("size", &self.data.size())
            .finish_non_exhaustive()
    }
}

/// Rust-only stream chunk I/O result delivered to the Boa thread via
/// `poll_io` (M9-D).
///
/// Carries the stream `generation` alongside one chunk read: a chunk, EOF,
/// or a typed [`FileApiError`]. Packaging into a fresh `Uint8Array` (or
/// decoding into a string) happens on the Boa thread after `poll_io`.
/// `Debug` shows only ids, the generation, and the payload kind — never
/// bytes, paths, or source detail.
#[derive(Debug)]
pub struct StreamChunkCompletion {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    generation: u64,
    kind: StreamChunkKind,
}

impl StreamChunkCompletion {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id (quota ownership / submission order).
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Returns the stream generation this chunk belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Takes the chunk payload out of the completion.
    pub(crate) fn into_kind(self) -> StreamChunkKind {
        self.kind
    }
}

/// Payload of one stream worker chunk (M9-D).
#[derive(Debug)]
pub(crate) enum StreamChunkKind {
    /// One chunk of `min(chunk_size, remaining)` bytes.
    Chunk(bytes::Bytes),
    /// End of input at dispatch time (`loaded == total`).
    Eof,
    /// Typed worker/bridge failure (`Cancelled`, source error, ...).
    Error(FileApiError),
}

/// Send-only stream chunk request executed off the Boa thread (M9-D).
///
/// Holds only Rust data: the owning context/operation ids, the stream
/// generation, the immutable blob payload, the snapshot of limits, the
/// logical position to read, and the shared completion bridge. Holds no
/// `JsValue`, `JsObject`, `Context`, realm pointer, or host path. `Debug`
/// shows only opaque ids, never content or paths.
pub struct StreamChunkTask {
    context_id: FileApiContextId,
    operation_id: FileIoOperationId,
    generation: u64,
    data: Arc<BlobData>,
    limits: FileApiLimits,
    offset: u64,
    len: u64,
    cancel: CancellationToken,
    bridge: Arc<IoBridge>,
}

impl StreamChunkTask {
    /// Returns the owning context id.
    pub fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    /// Returns the operation id (quota ownership / stream order).
    pub fn operation_id(&self) -> FileIoOperationId {
        self.operation_id
    }

    /// Returns the stream generation this chunk belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the logical start offset of this chunk.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the requested length of this chunk.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns `true` when the requested chunk length is zero.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Cancels the task's cooperative token (cancel/shutdown path).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Executes one bounded chunk read and pushes its completion.
    ///
    /// Runs only on a worker thread: performs exactly one bounded
    /// `read_blob_range` for the precomputed `[offset, offset+len)`.
    /// EOF is derived here (`offset == total`) so the bridge never
    /// fabricates one. Panics from host sources are contained exactly like
    /// whole-blob tasks and settle as a stable `Internal` error. The wake
    /// hook fires after the bridge lock is released.
    pub fn execute(self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.bridge.is_shutdown() || self.cancel.is_cancelled() {
                let _ = self.data.size();
                return Err(FileApiError::Cancelled);
            }
            if self.offset == self.data.size() {
                return Ok(None);
            }
            let end = self.offset.saturating_add(self.len).min(self.data.size());
            if end <= self.offset || end > self.data.size() {
                return Err(FileApiError::InvalidRange);
            }
            match read_blob_range(&self.data, self.offset, end, &self.limits, &self.cancel) {
                Ok(chunk) => {
                    let expected = (end - self.offset) as usize;
                    if chunk.len() != expected {
                        Err(FileApiError::InvalidRange)
                    } else {
                        Ok(Some(chunk))
                    }
                }
                Err(error) => Err(error),
            }
        }));
        let kind = match result {
            Ok(Ok(None)) => StreamChunkKind::Eof,
            Ok(Ok(Some(chunk))) => StreamChunkKind::Chunk(chunk),
            Ok(Err(error)) => StreamChunkKind::Error(error),
            Err(_) => StreamChunkKind::Error(FileApiError::Internal),
        };
        self.bridge.push_stream_completion(StreamChunkCompletion {
            context_id: self.context_id,
            operation_id: self.operation_id,
            generation: self.generation,
            kind,
        });
    }
}

impl std::fmt::Debug for StreamChunkTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamChunkTask")
            .field("context_id", &self.context_id.0)
            .field("operation_id", &self.operation_id.0)
            .field("generation", &self.generation)
            .field("size", &self.data.size())
            .finish_non_exhaustive()
    }
}

/// Reads `[start, end)` of a blob without touching Boa or whole-blob
/// accumulation.
///
/// Iterates the segment list like `BlobData::materialize` but returns only
/// the requested logical sub-range. Every segment response must match its
/// requested length exactly; short/long responses fail as `InvalidRange`
/// with no partial bytes. `cancel` is observed before the first and before
/// every segment read.
fn read_blob_range(
    data: &BlobData,
    start: u64,
    end: u64,
    limits: &FileApiLimits,
    cancel: &CancellationToken,
) -> Result<bytes::Bytes, FileApiError> {
    use boa_fapi_core::blob::BlobSegment;

    let segments: &[BlobSegment] = data.segments_slice();
    let requested = end.checked_sub(start).ok_or(FileApiError::InvalidRange)?;
    if start > data.size() || end > data.size() || end < start {
        return Err(FileApiError::InvalidRange);
    }
    let capacity = usize::try_from(requested).map_err(|_| {
        FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
    })?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(capacity).map_err(|_| {
        FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
    })?;
    let mut cursor = 0_u64;
    for seg in segments {
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        let seg_start = cursor;
        let seg_end = cursor.saturating_add(seg.len);
        cursor = seg_end;
        if seg_end <= start || seg_start >= end {
            continue;
        }
        let take_start = start.max(seg_start);
        let take_end = end.min(seg_end);
        let source_offset = seg
            .offset
            .saturating_add(take_start.saturating_sub(seg_start));
        let source_end = seg
            .offset
            .saturating_add(take_end.saturating_sub(seg_start));
        let chunk = seg.source.read_range(source_offset..source_end, cancel)?;
        let expected = (take_end - take_start) as usize;
        if chunk.len() != expected {
            return Err(FileApiError::InvalidRange);
        }
        out.extend_from_slice(&chunk);
        let _ = limits;
    }
    if out.len() != capacity {
        return Err(FileApiError::InvalidRange);
    }
    Ok(bytes::Bytes::from(out))
}

/// Reservation failure before any executor contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReserveError {
    QuotaFull,
    CompletionFull,
    Shutdown,
}

/// Per-context completion bridge shared between the Boa thread and workers.
///
/// Holds the bounded completion queue, the active-operation quota, the
/// per-operation cancellation tokens, and the wake/executor handles. The
/// mutex guards only queue/quota bookkeeping and is never held across
/// blocking I/O, Boa calls, or user JS.
pub(crate) struct IoBridge {
    context_id: FileApiContextId,
    state: Mutex<BridgeState>,
    concurrency_limit: usize,
    completion_cap: usize,
    next_operation: AtomicU64,
    executor: Arc<dyn FileIoExecutor>,
    wake: Arc<dyn FileIoWake>,
    shutdown: crate::lifecycle::ShutdownFlag,
}

#[derive(Debug, Default)]
struct BridgeState {
    active: usize,
    /// Completions grouped by operation id in submission order.
    ///
    /// Workers may finish out of order; `take_completions` releases only
    /// the longest in-order prefix (FIFO settlement by submission order),
    /// so an early-finishing later read can never overtake an earlier one.
    /// Entries hold at most one completion each (one task per operation).
    completions: BTreeMap<u64, FileIoCompletion>,
    /// Operation ids that already have a queued completion. Kept in sync
    /// with `completions`; used to compute the in-order prefix and to keep
    /// `has_pending`/reservation accounting exact.
    completed: BTreeSet<u64>,
    /// Submission order of live operations (monotonic by construction).
    /// Late/out-of-window completions whose id is absent here are dropped
    /// as stale by `take_completions`.
    order: VecDeque<u64>,
    tokens: HashMap<u64, CancellationToken>,
    /// FileReader chunk completions keyed by operation id. One reader
    /// operation holds exactly one reserved slot (`tokens`/`order` shared
    /// with promise reads); each chunk pushes one entry here, and the Boa
    /// thread drains them in push order through `take_reader_completions`
    /// (FIFO within one reader). Entries never contain JS values.
    reader_completions: HashMap<u64, VecDeque<FileReaderChunkCompletion>>,
    /// Stream chunk completions keyed by operation id (M9-D). One stream
    /// holds exactly one reserved slot (`tokens`/`order` shared with
    /// promise reads and FileReader); at most one chunk is in flight per
    /// stream, so each queue holds at most one entry by construction.
    /// Entries never contain JS values.
    stream_completions: HashMap<u64, VecDeque<StreamChunkCompletion>>,
    /// Operation ids owned by chunk (FileReader/stream) reservations.
    ///
    /// Marked at the first chunk push for the id and cleared at
    /// `unreserve`/`shutdown`. Lets `take_completions` skip live chunk
    /// reservations when scanning the whole-blob prefix: chunk ids never
    /// carry a whole-blob completion, so they must neither block nor join
    /// the promise-read FIFO prefix — including after their chunk queue
    /// drained while the reservation stays live (streams hold their slot
    /// past EOF; FileReader holds its slot across chunks).
    chunk_reservations: BTreeSet<u64>,
    /// When `true` no further ids can be minted (u64 space exhausted or a
    /// context id saturated): reservations fail as `QuotaFull` instead of
    /// reusing an id.
    ids_exhausted: bool,
    shutdown: bool,
}

impl IoBridge {
    pub(crate) fn new(
        context_id: FileApiContextId,
        concurrency_limit: usize,
        executor: Arc<dyn FileIoExecutor>,
        wake: Arc<dyn FileIoWake>,
        shutdown: crate::lifecycle::ShutdownFlag,
    ) -> Arc<Self> {
        let limit = concurrency_limit.max(1);
        let cap = limit.saturating_mul(2).max(64);
        Arc::new(Self {
            context_id,
            state: Mutex::new(BridgeState::default()),
            concurrency_limit: limit,
            completion_cap: cap,
            next_operation: AtomicU64::new(1),
            executor,
            wake,
            shutdown,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn context_id(&self) -> FileApiContextId {
        self.context_id
    }

    #[allow(dead_code)]
    pub(crate) fn executor(&self) -> &Arc<dyn FileIoExecutor> {
        &self.executor
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.is_shutdown()
            || self
                .state
                .lock()
                .map(|state| state.shutdown)
                .unwrap_or(true)
    }

    /// Reserves one active slot and one completion slot.
    ///
    /// Returns the operation id plus its cancellation token. Fails without
    /// consuming a slot when the quota is full, the completion queue is
    /// full, ids are exhausted, or the runtime is shut down. Ids are never
    /// reused: once the u64 space is exhausted every further reservation
    /// fails as `QuotaFull` (the runtime must be recreated).
    ///
    /// M9-D quota model: stream operations release their reservation at
    /// terminal EOF (see `settle_stream_eof`), so a second stream over the
    /// same blob reserves a fresh id — ids are consumed per stream, not
    /// per context lifetime.
    pub(crate) fn reserve(&self) -> Result<(FileIoOperationId, CancellationToken), ReserveError> {
        if self.shutdown.is_shutdown() {
            return Err(ReserveError::Shutdown);
        }
        let mut state = self.state.lock().map_err(|_| ReserveError::Shutdown)?;
        if state.shutdown || self.shutdown.is_shutdown() {
            return Err(ReserveError::Shutdown);
        }
        if state.ids_exhausted {
            return Err(ReserveError::QuotaFull);
        }
        if state.active >= self.concurrency_limit {
            return Err(ReserveError::QuotaFull);
        }
        if state.completed.len() >= self.completion_cap {
            return Err(ReserveError::CompletionFull);
        }
        let raw = self.next_operation.fetch_add(1, Ordering::Relaxed);
        // `0` and `u64::MAX` are never issued (sentinels): exhaustion
        // saturates the counter and refuses further ids instead of
        // wrapping around to a reused value. In particular a `0` (only
        // reachable via counter corruption/wrap) exhausts rather than
        // skipping to `1`, which could alias the very first operation.
        if raw == 0 || raw == u64::MAX {
            // Exhausted: saturate the counter and refuse further ids
            // instead of wrapping around to a reused value.
            self.next_operation.store(u64::MAX, Ordering::Relaxed);
            state.ids_exhausted = true;
            return Err(ReserveError::QuotaFull);
        }
        let id = raw;
        // Defensive: the counter is monotonic by construction, so a live
        // duplicate is unreachable; refuse rather than alias two operations.
        if state.tokens.contains_key(&id) || state.completed.contains(&id) {
            state.ids_exhausted = true;
            return Err(ReserveError::QuotaFull);
        }
        let token = CancellationToken::new();
        state.tokens.insert(id, token.clone());
        state.order.push_back(id);
        state.active = state.active.saturating_add(1);
        Ok((FileIoOperationId(id), token))
    }

    /// Builds the worker task for a reserved operation.
    pub(crate) fn task_for(
        self: &Arc<Self>,
        operation_id: FileIoOperationId,
        token: CancellationToken,
        data: Arc<BlobData>,
        limits: FileApiLimits,
    ) -> FileIoTask {
        FileIoTask {
            context_id: self.context_id,
            operation_id,
            data,
            limits,
            cancel: token,
            bridge: Arc::clone(self),
        }
    }

    /// Releases one reserved slot exactly once.
    ///
    /// Used by the submit-failure path and by `take_completions` cleanup.
    /// Removes the operation from the submission order as well, so a stale
    /// late completion can never match a future operation. Any queued
    /// reader/stream chunks for the operation are dropped with it.
    pub(crate) fn unreserve(&self, operation_id: FileIoOperationId) {
        if let Ok(mut state) = self.state.lock() {
            state.tokens.remove(&operation_id.0);
            state.completed.remove(&operation_id.0);
            state.completions.remove(&operation_id.0);
            state.reader_completions.remove(&operation_id.0);
            state.stream_completions.remove(&operation_id.0);
            state.chunk_reservations.remove(&operation_id.0);
            if let Some(position) = state.order.iter().position(|id| *id == operation_id.0) {
                state.order.remove(position);
            }
            state.active = state.active.saturating_sub(1);
        }
    }

    /// Releases one slot after a polled settlement (exactly once).
    pub(crate) fn release(&self, operation_id: FileIoOperationId) {
        self.unreserve(operation_id);
    }

    /// Builds the worker task for one FileReader chunk.
    ///
    /// The caller must hold a live reservation for `operation_id` (the
    /// reader's single quota slot): the chunk borrows the reservation's
    /// cancellation token without consuming quota itself, so one completion
    /// can never create more than one next request. `offset`/`len` are
    /// precomputed on the Boa thread from the reader's logical position.
    /// `params` carries `(generation, offset, len)` as one chunk window.
    pub(crate) fn chunk_task_for(
        self: &Arc<Self>,
        operation_id: FileIoOperationId,
        token: CancellationToken,
        data: Arc<BlobData>,
        limits: FileApiLimits,
        params: ChunkWindow,
    ) -> FileReaderChunkTask {
        FileReaderChunkTask {
            context_id: self.context_id,
            operation_id,
            generation: params.generation,
            data,
            limits,
            offset: params.offset,
            len: params.len,
            cancel: token,
            bridge: Arc::clone(self),
        }
    }

    /// Returns the cancellation token of a live reservation, if present.
    pub(crate) fn token_for(&self, operation_id: FileIoOperationId) -> Option<CancellationToken> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.tokens.get(&operation_id.0).cloned())
    }

    /// Submits a task through the configured executor with panic containment.
    ///
    /// A panicking third-party [`FileIoExecutor::submit`] is contained and
    /// reported as [`FileIoSubmitError::WorkerLost`] (stable
    /// `NotReadableError` downstream), exactly like a disconnected worker.
    /// The `task` is consumed in every outcome: on containment failure there
    /// is no task left to leak (it is dropped with its reservation released
    /// by the caller).
    pub(crate) fn submit_guarded(&self, task: FileIoTask) -> Result<(), FileIoSubmitError> {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.executor.submit(task)));
        match outcome {
            Ok(result) => result,
            Err(_) => Err(FileIoSubmitError::WorkerLost),
        }
    }

    /// Submits a FileReader chunk task with the same panic containment.
    ///
    /// `FileReaderChunkTask` travels through the same executor queue as
    /// whole-blob tasks: submission order decides FIFO settlement order,
    /// no readahead is created here, and a panicking executor reports
    /// `WorkerLost` for the terminal error path.
    pub(crate) fn submit_reader_guarded(
        &self,
        task: FileReaderChunkTask,
    ) -> Result<(), FileIoSubmitError> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.executor.submit_reader(task)
        }));
        match outcome {
            Ok(result) => result,
            Err(_) => Err(FileIoSubmitError::WorkerLost),
        }
    }

    /// Submits a stream chunk task with the same panic containment (M9-D).
    ///
    /// `StreamChunkTask` travels through the same executor queue as
    /// whole-blob and FileReader tasks: at most one chunk is in flight per
    /// stream, no readahead is created here, and a panicking executor
    /// reports `WorkerLost` for the terminal error path.
    pub(crate) fn submit_stream_guarded(
        &self,
        task: StreamChunkTask,
    ) -> Result<(), FileIoSubmitError> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.executor.submit_stream(task)
        }));
        match outcome {
            Ok(result) => result,
            Err(_) => Err(FileIoSubmitError::WorkerLost),
        }
    }

    /// Builds the worker task for one stream chunk (M9-D).
    ///
    /// The caller must hold a live reservation for `operation_id` (the
    /// stream's single quota slot): the chunk borrows the reservation's
    /// cancellation token without consuming quota itself, so one demand can
    /// never create more than one in-flight request. `offset`/`len` are
    /// precomputed on the Boa thread from the stream's logical position.
    /// `params` carries `(generation, offset, len)` as one chunk window.
    pub(crate) fn stream_task_for(
        self: &Arc<Self>,
        operation_id: FileIoOperationId,
        token: CancellationToken,
        data: Arc<BlobData>,
        limits: FileApiLimits,
        params: ChunkWindow,
    ) -> StreamChunkTask {
        StreamChunkTask {
            context_id: self.context_id,
            operation_id,
            generation: params.generation,
            data,
            limits,
            offset: params.offset,
            len: params.len,
            cancel: token,
            bridge: Arc::clone(self),
        }
    }

    /// Pushes a worker completion; drops it safely after shutdown.
    ///
    /// By construction (one reserved completion slot per active operation
    /// and `completion_cap >= concurrency_limit`) the queue cannot be full
    /// here; a full queue drops the late result without touching quota
    /// (quota was already released at shutdown). Late completions for
    /// unknown or already-settled operations are dropped as stale. The wake
    /// hook runs after the lock is released, never touches Boa, and is
    /// itself panic-contained: a panicking host wake can neither kill the
    /// worker nor poison the queue (the completion stays queued).
    pub(crate) fn push_completion(&self, completion: FileIoCompletion) {
        let id = completion.operation_id().0;
        let should_wake = if let Ok(mut state) = self.state.lock() {
            if state.shutdown || self.shutdown.is_shutdown() {
                return;
            }
            if !state.tokens.contains_key(&id) {
                // Unknown or already settled/shut down: stale, drop it.
                return;
            }
            if state.completed.contains(&id) {
                return;
            }
            if state.completed.len() >= self.completion_cap {
                return;
            }
            state.completions.insert(id, completion);
            state.completed.insert(id);
            true
        } else {
            return;
        };
        if should_wake {
            let wake_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.wake.wake(self.context_id);
            }));
            let _ = wake_result;
        }
    }

    /// Drains the longest in-order prefix of queued whole-blob completions.
    ///
    /// Boa thread only, via `poll_io`. Whole-blob completions are keyed by
    /// operation id; only the prefix starting at the oldest outstanding
    /// whole-blob operation is released, in whole-blob submission order, so
    /// a later-finishing worker can never overtake an earlier submission
    /// (FIFO settlement). Chunk operations (FileReader/stream) hold
    /// reservations in the shared `order` queue but complete through their
    /// own per-operation queues: they neither block nor join this prefix —
    /// only whole-blob ids participate. A completion for an unknown id
    /// (stale or post-shutdown) is dropped without settling.
    pub(crate) fn take_completions(&self) -> Vec<FileIoCompletion> {
        if let Ok(mut state) = self.state.lock() {
            let mut out = Vec::new();
            // Whole-blob submission order: ids in `order` that are not
            // chunk operations (chunk ops drain through their own queues).
            // A chunk id is one that currently owns a chunk queue entry OR
            // holds no whole-blob completion while a chunk queue exists for
            // context bookkeeping: `stream_completions`/`reader_completions`
            // entries are created only by chunk pushes, but an entry may
            // have been drained already while the reservation stays live
            // (streams hold their slot past EOF). Such ids must not block
            // the whole-blob prefix either: they never carry a whole-blob
            // completion, so the prefix scan skips them.
            //
            // Distinguishing rule: an id participates in the whole-blob
            // prefix only while it has no chunk-queue entry AND (it has a
            // whole-blob completion queued OR no chunk completion was ever
            // pushed for it in this reservation). The bridge does not track
            // "ever pushed" per id — instead chunk settlement removes the
            // id from `order` at terminal chunk settlement is wrong (slots
            // stay live past EOF by design).
            //
            // Practical invariant that keeps both M9-B FIFO and M9-D
            // liveness: an id with a queued whole-blob completion always
            // participates; an id without one participates only when it is
            // not a live chunk reservation. A live chunk reservation is an
            // id present in `tokens` whose most recent drain came from a
            // chunk queue. Track that explicitly below via
            // `chunk_reservations`.
            let whole_ids: Vec<u64> = state
                .order
                .iter()
                .copied()
                .filter(|id| !state.chunk_reservations.contains(id))
                .collect();
            for id in whole_ids {
                if !state.completed.contains(&id) {
                    break;
                }
                if let Some(position) = state.order.iter().position(|slot| *slot == id) {
                    state.order.remove(position);
                }
                if let Some(completion) = state.completions.remove(&id) {
                    state.completed.remove(&id);
                    out.push(completion);
                } else {
                    state.completed.remove(&id);
                }
            }
            // Opportunistically drop completions that lost their order slot
            // (e.g. after `unreserve` on a submit failure racing a worker):
            // they can never become in-order again.
            let stale: Vec<u64> = state
                .completions
                .keys()
                .copied()
                .filter(|id| !state.order.contains(id))
                .collect();
            for id in stale {
                state.completions.remove(&id);
                state.completed.remove(&id);
            }
            out
        } else {
            Vec::new()
        }
    }

    /// Pushes a FileReader chunk completion; drops it safely when stale.
    ///
    /// The operation must hold a live reservation (`tokens`): completions
    /// for unknown, already-released, or shut-down operations are dropped
    /// as stale without touching quota. One reader operation queues at
    /// most one chunk at a time (the Boa thread submits the next only
    /// after draining the previous), so the per-operation queue stays
    /// bounded by construction; the wake hook fires after the lock is
    /// released and is panic-contained like whole-blob completions.
    pub(crate) fn push_reader_completion(&self, completion: FileReaderChunkCompletion) {
        let id = completion.operation_id().0;
        let should_wake = if let Ok(mut state) = self.state.lock() {
            if state.shutdown || self.shutdown.is_shutdown() {
                return;
            }
            if !state.tokens.contains_key(&id) {
                return;
            }
            state.chunk_reservations.insert(id);
            state
                .reader_completions
                .entry(id)
                .or_default()
                .push_back(completion);
            true
        } else {
            return;
        };
        if should_wake {
            let wake_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.wake.wake(self.context_id);
            }));
            let _ = wake_result;
        }
    }

    /// Drains every queued FileReader chunk completion.
    ///
    /// Boa thread only, via `poll_io`. Whole-blob FIFO order is preserved
    /// by the separate `take_completions` prefix drain; reader chunks of
    /// one operation arrive in execution order through their own FIFO
    /// queue, so a late out-of-order worker completion can never overtake
    /// an earlier chunk of the same reader. Completions whose operation
    /// lost its reservation (abort/restart/shutdown/submit failure) are
    /// dropped as stale here.
    pub(crate) fn take_reader_completions(&self) -> Vec<FileReaderChunkCompletion> {
        if let Ok(mut state) = self.state.lock() {
            let mut out = Vec::new();
            let ids: Vec<u64> = state.order.iter().copied().collect();
            for id in ids {
                if let Some(queue) = state.reader_completions.get_mut(&id) {
                    while let Some(completion) = queue.pop_front() {
                        out.push(completion);
                    }
                }
            }
            let live: std::collections::HashSet<u64> = state.order.iter().copied().collect();
            state
                .reader_completions
                .retain(|id, queue| !queue.is_empty() && live.contains(id));
            out
        } else {
            Vec::new()
        }
    }

    /// Re-queues one FileReader chunk completion at the front of its queue.
    ///
    /// Used by the `poll_io` fairness budget: leftovers keep their FIFO
    /// position for the next host-loop turn.
    pub(crate) fn requeue_reader_completion(&self, completion: FileReaderChunkCompletion) {
        if let Ok(mut state) = self.state.lock() {
            if state.shutdown || self.shutdown.is_shutdown() {
                return;
            }
            let id = completion.operation_id().0;
            if !state.tokens.contains_key(&id) {
                return;
            }
            state
                .reader_completions
                .entry(id)
                .or_default()
                .push_front(completion);
        }
    }

    /// Pushes a stream chunk completion; drops it safely when stale (M9-D).
    ///
    /// The operation must hold a live reservation (`tokens`): completions
    /// for unknown, already-released, or shut-down operations are dropped
    /// as stale without touching quota. At most one chunk is in flight per
    /// stream, so the per-operation queue stays bounded by construction;
    /// the wake hook fires after the lock is released and is
    /// panic-contained like whole-blob completions.
    pub(crate) fn push_stream_completion(&self, completion: StreamChunkCompletion) {
        let id = completion.operation_id().0;
        let should_wake = if let Ok(mut state) = self.state.lock() {
            if state.shutdown || self.shutdown.is_shutdown() {
                return;
            }
            if !state.tokens.contains_key(&id) {
                return;
            }
            state.chunk_reservations.insert(id);
            state
                .stream_completions
                .entry(id)
                .or_default()
                .push_back(completion);
            true
        } else {
            return;
        };
        if should_wake {
            let wake_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.wake.wake(self.context_id);
            }));
            let _ = wake_result;
        }
    }

    /// Drains every queued stream chunk completion (M9-D).
    ///
    /// Boa thread only, via `poll_io`. Stream chunks of one operation
    /// arrive in execution order through their own FIFO queue; with at most
    /// one in-flight chunk per stream, reordering is impossible by
    /// construction. Completions whose operation lost its reservation
    /// (cancel/error/shutdown/submit failure) are dropped as stale here.
    pub(crate) fn take_stream_completions(&self) -> Vec<StreamChunkCompletion> {
        if let Ok(mut state) = self.state.lock() {
            let mut out = Vec::new();
            let ids: Vec<u64> = state.order.iter().copied().collect();
            for id in ids {
                if let Some(queue) = state.stream_completions.get_mut(&id) {
                    while let Some(completion) = queue.pop_front() {
                        out.push(completion);
                    }
                }
            }
            let live: std::collections::HashSet<u64> = state.order.iter().copied().collect();
            state
                .stream_completions
                .retain(|id, queue| !queue.is_empty() && live.contains(id));
            out
        } else {
            Vec::new()
        }
    }

    /// Signals the host wake hook without touching Boa or the queue.
    ///
    /// Used after a budget-truncated `poll_io` so the host loop schedules
    /// the next drain. Panic-contained like completion wakes.
    pub(crate) fn wake_host(&self) {
        let wake_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.wake.wake(self.context_id);
        }));
        let _ = wake_result;
    }

    /// Returns `true` while work is outstanding or completions wait.
    pub(crate) fn has_pending(&self) -> bool {
        if let Ok(state) = self.state.lock() {
            state.active > 0
                || !state.completed.is_empty()
                || state
                    .reader_completions
                    .values()
                    .any(|queue| !queue.is_empty())
                || state
                    .stream_completions
                    .values()
                    .any(|queue| !queue.is_empty())
        } else {
            false
        }
    }

    /// Returns the number of reserved active operations (diagnostics).
    #[allow(dead_code)]
    pub(crate) fn active_count(&self) -> usize {
        self.state.lock().map(|state| state.active).unwrap_or(0)
    }

    /// Cancels outstanding work, clears the queue, and forbids late
    /// settlement. Idempotent; late workers drop their results. Quota is
    /// released in bulk exactly once (active reset, tokens/order cleared).
    pub(crate) fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            for (_, token) in state.tokens.iter() {
                token.cancel();
            }
            state.tokens.clear();
            state.completions.clear();
            state.completed.clear();
            state.reader_completions.clear();
            state.stream_completions.clear();
            state.chunk_reservations.clear();
            state.order.clear();
            state.active = 0;
        }
    }
}

/// Built-in bounded file I/O executor (fixed workers, bounded queue).
///
/// Spawns `worker_count` threads sharing one bounded queue of capacity
/// `queue_cap`. `submit`/`submit_reader` never block: a full queue returns
/// [`FileIoSubmitError::QueueFull`]. Workers run [`FileIoTask::execute`]
/// or [`FileReaderChunkTask::execute`] and exit when the executor is
/// dropped. No thread-per-read, no unbounded growth, no Boa access from
/// workers.
///
/// `poll_io` never waits: it only drains already-queued completions. Hosts
/// that want prompt settlement wait on the [`FileIoWake`] signal (or their
/// own event) and then call `poll_io`; a controlled manual executor still
/// proves that no completion runs before `poll_io`.
pub struct ThreadedFileIoExecutor {
    inner: Mutex<ExecutorInner>,
}

/// One queued request of the bounded pool.
enum PoolRequest {
    Whole(FileIoTask),
    Chunk(FileReaderChunkTask),
    Stream(StreamChunkTask),
}

struct ExecutorInner {
    tx: Option<std::sync::mpsc::SyncSender<PoolRequest>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ThreadedFileIoExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedFileIoExecutor")
            .finish_non_exhaustive()
    }
}

impl ThreadedFileIoExecutor {
    /// Creates a bounded pool.
    ///
    /// `worker_count` is clamped to `1..=32`, `queue_cap` to `1..=4096`.
    /// A spawn failure degrades to fail-fast submits (`WorkerLost`)
    /// instead of hanging or panicking.
    pub fn new(worker_count: usize, queue_cap: usize) -> Self {
        let workers = worker_count.clamp(1, 32);
        let capacity = queue_cap.clamp(1, 4096);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PoolRequest>(capacity);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::new();
        let mut spawned_ok = true;
        for index in 0..workers {
            let rx = Arc::clone(&rx);
            let name = format!("boa-fapi-io-{index}");
            // NOTE: `std::thread::Builder` is the documented M9-B worker
            // host: the single allowed threading site (see the
            // `no_out_of_scope_surface` guard exception for `io.rs`).
            let builder = std::thread::Builder::new().name(name);
            match builder.spawn(move || {
                worker_loop(rx);
            }) {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    spawned_ok = false;
                    break;
                }
            }
        }
        let tx = if spawned_ok { Some(tx) } else { None };
        Self {
            inner: Mutex::new(ExecutorInner { tx, handles }),
        }
    }

    /// Returns the configured worker count (for diagnostics).
    pub fn worker_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.handles.len())
            .unwrap_or(0)
    }
}

impl Default for ThreadedFileIoExecutor {
    fn default() -> Self {
        Self::new(4, 128)
    }
}

impl FileIoExecutor for ThreadedFileIoExecutor {
    fn submit(&self, task: FileIoTask) -> Result<(), FileIoSubmitError> {
        Self::send(&self.inner, PoolRequest::Whole(task))
    }

    fn submit_reader(&self, task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        Self::send(&self.inner, PoolRequest::Chunk(task))
    }

    fn submit_stream(&self, task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        Self::send(&self.inner, PoolRequest::Stream(task))
    }
}

impl ThreadedFileIoExecutor {
    fn send(inner: &Mutex<ExecutorInner>, request: PoolRequest) -> Result<(), FileIoSubmitError> {
        let result = inner.lock().map(|inner| {
            if let Some(tx) = inner.tx.as_ref() {
                match tx.try_send(request) {
                    Ok(()) => Ok(()),
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        Err(FileIoSubmitError::QueueFull)
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        Err(FileIoSubmitError::WorkerLost)
                    }
                }
            } else {
                Err(FileIoSubmitError::WorkerLost)
            }
        });
        match result {
            Ok(outcome) => outcome,
            Err(_) => Err(FileIoSubmitError::WorkerLost),
        }
    }
}

impl Drop for ThreadedFileIoExecutor {
    fn drop(&mut self) {
        let handles = if let Ok(mut inner) = self.inner.lock() {
            inner.tx.take();
            std::mem::take(&mut inner.handles)
        } else {
            Vec::new()
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

/// Worker body: receives tasks without holding the lock across I/O.
fn worker_loop(rx: Arc<Mutex<std::sync::mpsc::Receiver<PoolRequest>>>) {
    loop {
        let task = {
            let guard = rx.lock();
            match guard {
                // `recv` (not `recv_timeout`) wakes immediately when a task
                // lands: no 100 ms poll latency for tiny memory reads.
                Ok(rx) => match rx.recv() {
                    Ok(task) => Some(task),
                    Err(_) => return,
                },
                Err(_) => return,
            }
        };
        match task {
            Some(PoolRequest::Whole(task)) => task.execute(),
            Some(PoolRequest::Chunk(task)) => task.execute(),
            Some(PoolRequest::Stream(task)) => task.execute(),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct RejectingExecutor;

    impl FileIoExecutor for RejectingExecutor {
        fn submit(&self, _task: FileIoTask) -> Result<(), FileIoSubmitError> {
            Err(FileIoSubmitError::WorkerLost)
        }
    }

    #[test]
    fn operation_id_exhaustion_fails_closed_without_reuse() {
        let bridge = IoBridge::new(
            FileApiContextId(7),
            2,
            Arc::new(RejectingExecutor),
            Arc::new(NoopWake),
            crate::lifecycle::ShutdownFlag::new(),
        );
        // Exercise the actual u64 boundary without performing 2^64 reads.
        bridge.next_operation.store(u64::MAX - 1, Ordering::Relaxed);
        let reserved = bridge.reserve();
        assert!(reserved.is_ok(), "last id before sentinel must reserve");
        let Some((last_id, _)) = reserved.ok() else {
            return;
        };
        assert_eq!(last_id.get(), u64::MAX - 1);
        bridge.unreserve(last_id);

        assert!(matches!(bridge.reserve(), Err(ReserveError::QuotaFull)));
        // The counter is saturated: another reservation cannot wrap to 1.
        assert!(matches!(bridge.reserve(), Err(ReserveError::QuotaFull)));
    }

    #[test]
    fn late_stream_completion_after_eof_release_is_a_strict_noop() {
        let bridge = IoBridge::new(
            FileApiContextId(8),
            1,
            Arc::new(RejectingExecutor),
            Arc::new(NoopWake),
            crate::lifecycle::ShutdownFlag::new(),
        );
        let reserved = bridge.reserve();
        assert!(reserved.is_ok(), "first reservation must succeed");
        let Ok((eof_operation, _)) = reserved else {
            return;
        };
        // This models the EOF transition: the operation is terminal and its
        // slot is free before any late worker completion can be accepted.
        bridge.unreserve(eof_operation);
        let replacement = bridge.reserve();
        assert!(replacement.is_ok(), "replacement reservation must succeed");
        let Ok((live_operation, _)) = replacement else {
            return;
        };

        bridge.push_stream_completion(StreamChunkCompletion {
            context_id: FileApiContextId(8),
            operation_id: eof_operation,
            generation: 1,
            kind: StreamChunkKind::Eof,
        });

        assert!(
            bridge.take_stream_completions().is_empty(),
            "late completion must not be delivered to poll_io"
        );
        assert!(
            matches!(bridge.state.lock().map(|state| state.active), Ok(1)),
            "late completion must not release the replacement stream slot"
        );
        bridge.unreserve(live_operation);
    }
}
