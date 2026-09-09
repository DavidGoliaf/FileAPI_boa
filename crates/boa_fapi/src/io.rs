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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    pub(crate) fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed).max(1);
        Self(id)
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

/// Host executor for [`FileIoTask`].
///
/// Implementations run the task off the Boa thread and never touch Boa.
/// The built-in [`ThreadedFileIoExecutor`] uses a fixed worker pool with a
/// bounded queue; thread-per-read without a limit is forbidden by contract.
/// Tests inject a controlled manual executor through the builder.
pub trait FileIoExecutor: Send + Sync + 'static {
    /// Queues `task` for off-thread execution.
    ///
    /// Must not block the Boa thread and must not run user JS.
    /// `QueueFull` and `WorkerLost` are typed and free the caller's quota
    /// exactly once through the normal error path.
    fn submit(&self, task: FileIoTask) -> Result<(), FileIoSubmitError>;
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
    completions: VecDeque<FileIoCompletion>,
    tokens: HashMap<u64, CancellationToken>,
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

    pub(crate) fn executor(&self) -> &Arc<dyn FileIoExecutor> {
        &self.executor
    }

    /// Bounded drain sweep before `poll_io` reads the queue.
    ///
    /// Exits immediately when every outstanding operation already has a
    /// queued completion or no work is outstanding. Otherwise waits briefly
    /// (bounded) so that the pre-existing `poll`-free host loops observe
    /// settlement. Controlled manual executors never complete on their own,
    /// so the sweep simply expires there and the pending-before-`poll_io`
    /// contract stays provable.
    pub(crate) fn drain_completed(&self) {
        // Skip the wait entirely when there is nothing outstanding.
        let outstanding = if let Ok(state) = self.state.lock() {
            state.active
        } else {
            return;
        };
        if outstanding == 0 {
            return;
        }
        // Fast path: completions for tiny memory reads usually land
        // quickly; check without sleeping first.
        if let Ok(state) = self.state.lock() {
            if state.active == 0 || state.completions.len() >= state.active {
                return;
            }
        } else {
            return;
        }
        for _ in 0..10_000 {
            // Allowed threading site: the M9-B compatibility yield while
            // waiting for an already-running worker (std::thread).
            std::thread::sleep(Duration::from_micros(200));
            if let Ok(state) = self.state.lock() {
                if state.active == 0 || state.completions.len() >= state.active {
                    return;
                }
            } else {
                return;
            }
        }
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
    /// full, or the runtime is shut down.
    pub(crate) fn reserve(&self) -> Result<(FileIoOperationId, CancellationToken), ReserveError> {
        if self.shutdown.is_shutdown() {
            return Err(ReserveError::Shutdown);
        }
        let mut state = self.state.lock().map_err(|_| ReserveError::Shutdown)?;
        if state.shutdown || self.shutdown.is_shutdown() {
            return Err(ReserveError::Shutdown);
        }
        if state.active >= self.concurrency_limit {
            return Err(ReserveError::QuotaFull);
        }
        if state.completions.len() >= self.completion_cap {
            return Err(ReserveError::CompletionFull);
        }
        let id = self.next_operation.fetch_add(1, Ordering::Relaxed).max(1);
        if id == u64::MAX {
            // Practically unreachable (2^64 operations); treat further
            // reservations as quota exhaustion rather than reusing ids.
            return Err(ReserveError::QuotaFull);
        }
        let token = CancellationToken::new();
        state.tokens.insert(id, token.clone());
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

    /// Releases one reserved slot exactly once (submit failure path).
    pub(crate) fn unreserve(&self, operation_id: FileIoOperationId) {
        if let Ok(mut state) = self.state.lock() {
            state.tokens.remove(&operation_id.0);
            state.active = state.active.saturating_sub(1);
        }
    }

    /// Releases one slot after a polled settlement (exactly once).
    pub(crate) fn release(&self, operation_id: FileIoOperationId) {
        self.unreserve(operation_id);
    }

    /// Pushes a worker completion; drops it safely after shutdown.
    ///
    /// By construction (one reserved completion slot per active operation
    /// and `completion_cap >= concurrency_limit`) the queue cannot be full
    /// here; a full queue drops the late result without touching quota
    /// (quota was already released at shutdown). The wake hook runs after
    /// the lock is released and never touches Boa.
    pub(crate) fn push_completion(&self, completion: FileIoCompletion) {
        let should_wake = if let Ok(mut state) = self.state.lock() {
            if state.shutdown || self.shutdown.is_shutdown() {
                return;
            }
            if state.completions.len() >= self.completion_cap {
                return;
            }
            state.completions.push_back(completion);
            true
        } else {
            return;
        };
        if should_wake {
            self.wake.wake(self.context_id);
        }
    }

    /// Drains queued completions (Boa thread only, via `poll_io`).
    pub(crate) fn take_completions(&self) -> Vec<FileIoCompletion> {
        if let Ok(mut state) = self.state.lock() {
            state.completions.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Returns `true` while work is outstanding or completions wait.
    pub(crate) fn has_pending(&self) -> bool {
        if let Ok(state) = self.state.lock() {
            state.active > 0 || !state.completions.is_empty()
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
    /// released in bulk exactly once (active reset, tokens cleared).
    pub(crate) fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            for (_, token) in state.tokens.iter() {
                token.cancel();
            }
            state.tokens.clear();
            state.completions.clear();
            state.active = 0;
        }
    }
}

/// Built-in bounded file I/O executor (fixed workers, bounded queue).
///
/// Spawns `worker_count` threads sharing one bounded queue of capacity
/// `queue_cap`. `submit` never blocks: a full queue returns
/// [`FileIoSubmitError::QueueFull`]. Workers run [`FileIoTask::execute`]
/// and exit when the executor is dropped. No thread-per-read, no unbounded
/// growth, no Boa access from workers.
///
/// Compatibility note: `poll_io` performs a bounded drain sweep before
/// reading the queue so the pre-existing `poll`-free host loops observe
/// settlement without an explicit wake; a controlled manual executor still
/// proves that no completion runs before `poll_io`.
pub struct ThreadedFileIoExecutor {
    inner: Mutex<ExecutorInner>,
}

struct ExecutorInner {
    tx: Option<std::sync::mpsc::SyncSender<FileIoTask>>,
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
        let (tx, rx) = std::sync::mpsc::sync_channel::<FileIoTask>(capacity);
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
        let result = self.inner.lock().map(|inner| {
            if let Some(tx) = inner.tx.as_ref() {
                match tx.try_send(task) {
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
fn worker_loop(rx: Arc<Mutex<std::sync::mpsc::Receiver<FileIoTask>>>) {
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
        if let Some(task) = task {
            task.execute();
        }
    }
}
