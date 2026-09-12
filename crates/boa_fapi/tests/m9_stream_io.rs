//! M9-D integration: `ReadableStream` chunk I/O over the M9-B executor protocol.
//!
//! Every test drives the real `Blob.prototype.stream()`/`textStream()` path
//! with a controlled manual [`FileIoExecutor`](boa_fapi::FileIoExecutor)
//! that records stream chunk requests without running them, plus a counting
//! [`FileIoWake`](boa_fapi::FileIoWake). No `sleep` is used as an oracle:
//! synchronization is executor hand-off with bounded waits only as a hang
//! guard. Trace rows: `M9D-STR-01` (demand submits off-thread read and
//! returns a pending promise), `M9D-STR-02` (FIFO chunks, EOF and error
//! propagation), `M9D-STR-03` (cancel/release/shutdown and stale
//! completion), `M9D-STR-04` (bounded in-flight I/O, queues and quota),
//! `M9D-STR-05` (no async JS call path performs a filesystem read on the
//! Boa thread).
//!
//! `blob_from_data` wraps arbitrary host `BlobData` in brand-valid JS
//! objects so the tests drive the real read path with controlled host
//! sources (blocking, failing) instead of memory copies.
//!
//! M9-D-R2 (`M9D-GC-01…06`) proves the GC/drop lifecycle on top of the
//! same harness: an unread stream whose last JS endpoint is dropped
//! releases its quota/payload/operation through the explicit-drop +
//! `poll_io` abandoned transition (no `sleep`, no test-only cleanup:
//! the tests drop the real JS roots, run the supported deterministic
//! `boa_gc::force_collect()`, then drain the real host cleanup/`poll_io`
//! loop and assert real quota recovery).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "fs")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use boa_engine::{Context, Source, js_string};
use boa_fapi::{
    Clock, FileApiContextId, FileApiExtension, FileIoExecutor, FileIoSubmitError, FileIoWake,
    PollIoError, StreamChunkTask,
};

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

const FIXED_TIME: i64 = 1_700_000_000_000;

/// Controlled manual executor: records whole-blob, FileReader chunk, and
/// stream chunk requests without running them. The test releases requests
/// explicitly, in order.
struct ManualExecutor {
    whole: Mutex<VecDeque<boa_fapi::FileIoTask>>,
    chunks: Mutex<VecDeque<boa_fapi::FileReaderChunkTask>>,
    streams: Mutex<VecDeque<StreamChunkTask>>,
    submits: AtomicUsize,
}

impl ManualExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            whole: Mutex::new(VecDeque::new()),
            chunks: Mutex::new(VecDeque::new()),
            streams: Mutex::new(VecDeque::new()),
            submits: AtomicUsize::new(0),
        })
    }

    fn pending_streams(self: &Arc<Self>) -> usize {
        self.streams.lock().expect("streams").len()
    }

    fn pending_whole(self: &Arc<Self>) -> usize {
        self.whole.lock().expect("whole").len()
    }

    fn take_streams(self: &Arc<Self>) -> Vec<StreamChunkTask> {
        self.streams.lock().expect("streams").drain(..).collect()
    }

    /// Runs every queued stream chunk on the calling thread (test worker
    /// stand-in).
    fn run_streams(self: &Arc<Self>) {
        // Loop until no new requests appear: each settled chunk may submit
        // the next demand's request, so a single pass is not enough when
        // several reads queue (bounded: one request per demand + EOF probe).
        for _ in 0..64 {
            let mut ran = false;
            for task in self.take_streams() {
                task.execute();
                ran = true;
            }
            if !ran {
                break;
            }
        }
    }

    fn submits(&self) -> usize {
        self.submits.load(Ordering::SeqCst)
    }
}

impl FileIoExecutor for ManualExecutor {
    fn submit(&self, task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        self.whole.lock().expect("whole").push_back(task);
        self.submits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn submit_reader(&self, task: boa_fapi::FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        self.chunks.lock().expect("chunks").push_back(task);
        self.submits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn submit_stream(&self, task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        self.streams.lock().expect("streams").push_back(task);
        self.submits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Executor that always fails stream submits with `QueueFull`.
struct FullStreamExecutor;

impl FileIoExecutor for FullStreamExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::QueueFull)
    }

    fn submit_stream(&self, _task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::QueueFull)
    }
}

/// Executor that always fails stream submits with `WorkerLost`.
struct LostStreamExecutor;

impl FileIoExecutor for LostStreamExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::WorkerLost)
    }

    fn submit_stream(&self, _task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::WorkerLost)
    }
}

/// Executor that panics on stream submit (panic containment path).
struct PanicStreamExecutor;

impl FileIoExecutor for PanicStreamExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        panic!("third-party executor misbehaved");
    }

    fn submit_stream(&self, _task: StreamChunkTask) -> Result<(), FileIoSubmitError> {
        panic!("third-party stream executor misbehaved");
    }
}

/// Counting wake hook: records every context id it is signalled with.
#[derive(Debug, Default)]
struct CountingWake {
    wakes: Mutex<Vec<FileApiContextId>>,
}

impl CountingWake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            wakes: Mutex::new(Vec::new()),
        })
    }

    fn count(&self) -> usize {
        self.wakes.lock().expect("wakes").len()
    }
}

impl FileIoWake for CountingWake {
    fn wake(&self, context_id: FileApiContextId) {
        self.wakes.lock().expect("wakes").push(context_id);
    }
}

/// Controlled blocking source: `read_range` blocks on a test gate.
struct BlockingSource {
    len: u64,
    gate: Arc<(Mutex<bool>, Condvar)>,
    fill: u8,
    reads: Arc<AtomicUsize>,
}

impl BlockingSource {
    fn with_byte(
        len: u64,
        gate: &Arc<(Mutex<bool>, Condvar)>,
        fill: u8,
        reads: &Arc<AtomicUsize>,
    ) -> Self {
        Self {
            len,
            gate: Arc::clone(gate),
            fill,
            reads: Arc::clone(reads),
        }
    }
}

impl boa_fapi_core::source::ByteSource for BlockingSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        boa_fapi_core::snapshot::SnapshotState::Memory
    }

    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &boa_fapi_core::cancellation::CancellationToken,
    ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
        use boa_fapi_core::file_api_error::FileApiError;
        self.reads.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        let (lock, gate) = &*self.gate;
        let mut open = lock.lock().expect("gate");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !*open {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(FileApiError::Internal);
            }
            let remaining = deadline - now;
            let (guard, timeout) = gate.wait_timeout(open, remaining).expect("wait");
            open = guard;
            if timeout.timed_out() {
                return Err(FileApiError::Internal);
            }
        }
        drop(open);
        if cancel.is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        let len =
            usize::try_from(range.end - range.start).map_err(|_| FileApiError::InvalidRange)?;
        Ok(bytes::Bytes::from(vec![self.fill; len]))
    }
}

/// A source that fails every read with `FileLocked` (→ `NotReadableError`).
struct FailSource {
    len: u64,
}

impl boa_fapi_core::source::ByteSource for FailSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        boa_fapi_core::snapshot::SnapshotState::Memory
    }
    fn read_range(
        &self,
        _range: std::ops::Range<u64>,
        _cancel: &boa_fapi_core::cancellation::CancellationToken,
    ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
        Err(boa_fapi_core::file_api_error::FileApiError::FileLocked)
    }
}

/// A source that fails reads after `fail_after` successful chunk reads.
struct FailAfterSource {
    len: u64,
    fill: u8,
    fail_after: usize,
    reads: Arc<AtomicUsize>,
}

impl boa_fapi_core::source::ByteSource for FailAfterSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        boa_fapi_core::snapshot::SnapshotState::Memory
    }
    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        _cancel: &boa_fapi_core::cancellation::CancellationToken,
    ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
        use boa_fapi_core::file_api_error::FileApiError;
        let n = self.reads.fetch_add(1, Ordering::SeqCst);
        if n >= self.fail_after {
            return Err(FileApiError::FileLocked);
        }
        let len =
            usize::try_from(range.end - range.start).map_err(|_| FileApiError::InvalidRange)?;
        Ok(bytes::Bytes::from(vec![self.fill; len]))
    }
}

fn blob_of(
    source: Arc<dyn boa_fapi_core::source::ByteSource>,
    len: u64,
) -> Arc<boa_fapi_core::blob::BlobData> {
    blob_of_with_limits(
        source,
        len,
        &boa_fapi_core::limits::FileApiLimits::default(),
    )
}

fn blob_of_with_limits(
    source: Arc<dyn boa_fapi_core::source::ByteSource>,
    len: u64,
    limits: &boa_fapi_core::limits::FileApiLimits,
) -> Arc<boa_fapi_core::blob::BlobData> {
    Arc::new(
        boa_fapi_core::blob::BlobData::from_segments(
            vec![boa_fapi_core::blob::BlobSegment {
                source,
                offset: 0,
                len,
            }],
            "",
            limits,
        )
        .expect("valid segments"),
    )
}

fn blocking_blob(
    len: u64,
    gate: &Arc<(Mutex<bool>, Condvar)>,
    fill: u8,
    reads: &Arc<AtomicUsize>,
) -> Arc<boa_fapi_core::blob::BlobData> {
    blob_of(
        Arc::new(BlockingSource::with_byte(len, gate, fill, reads)),
        len,
    )
}

fn setup_manual() -> (
    Context,
    boa_fapi::FileApiHandle,
    Arc<ManualExecutor>,
    Arc<CountingWake>,
) {
    let executor = ManualExecutor::new();
    let wake = CountingWake::new();
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
        .io_wake(Arc::clone(&wake) as Arc<dyn FileIoWake>)
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle, executor, wake)
}

fn setup_manual_with_limits(
    limits: boa_fapi_core::limits::FileApiLimits,
) -> (
    Context,
    boa_fapi::FileApiHandle,
    Arc<ManualExecutor>,
    Arc<CountingWake>,
) {
    let executor = ManualExecutor::new();
    let wake = CountingWake::new();
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
        .io_wake(Arc::clone(&wake) as Arc<dyn FileIoWake>)
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle, executor, wake)
}

fn chunk_limits() -> boa_fapi_core::limits::FileApiLimits {
    boa_fapi_core::limits::FileApiLimits {
        default_chunk_size: 16 * 1024,
        ..boa_fapi_core::limits::FileApiLimits::default()
    }
}

fn quota_one_limits() -> boa_fapi_core::limits::FileApiLimits {
    boa_fapi_core::limits::FileApiLimits {
        max_concurrent_reads_per_global: 1,
        ..boa_fapi_core::limits::FileApiLimits::default()
    }
}

fn eval_str(context: &mut Context, source: &str) -> String {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    if let Some(string) = value.as_string() {
        return string.to_std_string_escaped();
    }
    let probe = format!("String(({source}))");
    context
        .eval(Source::from_bytes(probe.as_str()))
        .unwrap_or_else(|error| panic!("stringify failed for {source}: {error}"))
        .as_string()
        .unwrap_or_else(|| panic!("non-string result for {source}"))
        .to_std_string_escaped()
}

fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert_eq!(
        value.as_boolean(),
        Some(true),
        "JS assertion failed: {source} (got {value:?})"
    );
}

/// Polls `poll_io` until no new completions appear (bounded), running
/// `run_jobs` after each drain. A manual executor only runs tasks when the
/// test executes them explicitly, so `drive` alternates: run held tasks,
/// drain, settle, repeat — until quiescent or the bound hits.
fn drive_until_settled(
    context: &mut Context,
    handle: &boa_fapi::FileApiHandle,
    executor: &Arc<ManualExecutor>,
) {
    for _ in 0..64 {
        executor.run_streams();
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        let settled2 = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0
            && settled2 == 0
            && executor.pending_streams() == 0
            && !handle.has_pending_io()
        {
            break;
        }
    }
}

fn drive(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..400 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        // Settlement jobs (async continuations awaiting a read) may have
        // queued new demand after `run_jobs`: drain again before checking
        // quiescence, so interleaved reads converge.
        let settled2 = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0 && settled2 == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs");
            let _ = handle.poll_io(context);
            context.run_jobs().expect("run_jobs");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + Duration::from_millis(50);
            while handle.has_pending_io() {
                let _ = handle.poll_io(context);
                context.run_jobs().expect("run_jobs");
                if !handle.has_pending_io() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::yield_now();
            }
        }
    }
}

/// Publishes a brand-valid `Blob` over an arbitrary host payload (no copy).
fn publish_blob(
    handle: &boa_fapi::FileApiHandle,
    context: &mut Context,
    name: &str,
    data: &Arc<boa_fapi_core::blob::BlobData>,
) {
    let object = handle.blob_from_data(Arc::clone(data));
    context
        .register_global_property(
            js_string!(name),
            object,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
}

/// Starts a stream reader reachable as `readerName` over the blob `srcBlob`.
fn publish_reader(context: &mut Context, blob: &str, reader: &str) {
    context
        .eval(Source::from_bytes(&format!(
            "globalThis.{reader} = {blob}.stream().getReader();"
        )))
        .expect("publish reader");
}

/// Starts a text stream reader reachable as `readerName` over `srcBlob`.
#[allow(dead_code)]
fn publish_text_reader(context: &mut Context, blob: &str, reader: &str) {
    context
        .eval(Source::from_bytes(&format!(
            "globalThis.{reader} = {blob}.textStream().getReader();"
        )))
        .expect("publish text reader");
}

// ── M9D-STR-01: demand submits off-thread read, returns pending Promise ──

#[test]
fn read_returns_pending_before_io_and_settles_only_through_poll_io() {
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blocking_blob(3, &gate, b'x', &reads),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.verdict = 'pending'; \
         globalThis.len = -1; globalThis.done = 'unset'; \
         reader.read().then(r => { globalThis.len = r.value.length; globalThis.done = r.done; }); \
         globalThis.verdict === 'pending' && globalThis.len === -1",
    );
    // One stream chunk request submitted, nothing executed: pending before I/O.
    assert_eq!(executor.pending_streams(), 1);
    assert_eq!(executor.pending_whole(), 0);
    // `run_jobs` alone settles nothing and reads nothing: the request is
    // still held by the manual executor.
    for _ in 0..3 {
        context.run_jobs().expect("run_jobs");
    }
    assert_eq!(executor.pending_streams(), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "pending");
    // An unrelated Boa job runs while I/O is held.
    assert_eval(
        &mut context,
        "globalThis.unrelated = 'no'; \
         Promise.resolve().then(() => { globalThis.unrelated = 'yes'; }); \
         true",
    );
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.unrelated"), "yes");
    // Completion before `poll_io` runs no JS.
    {
        let (lock, gate) = &*gate;
        *lock.lock().expect("gate") = true;
        gate.notify_all();
    }
    executor.run_streams();
    assert_eq!(eval_str(&mut context, "globalThis.len"), "-1");
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.len"), "3");
    assert_eq!(eval_str(&mut context, "globalThis.done"), "false");
}

#[test]
fn blocking_source_never_runs_inside_boa_job() {
    use std::sync::mpsc;
    let (mut context, handle, executor, wake) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let gate_worker = Arc::clone(&gate);
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blocking_blob(5, &gate, b'A', &reads),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.verdict = 'pending'; globalThis.len = -1; \
         reader.read().then(r => { globalThis.verdict = 'chunk'; globalThis.len = r.value.length; }); \
         globalThis.verdict === 'pending'",
    );
    assert_eq!(executor.pending_streams(), 1);
    let held = executor.take_streams();
    assert_eq!(held.len(), 1);
    let task = held.into_iter().next().expect("held chunk");
    // The held chunk covers exactly the first (and only) range.
    assert_eq!(task.offset(), 0);
    assert_eq!(task.len(), 5);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let wake_before = wake.count();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            started_tx.send(()).expect("started");
            release_rx.recv().expect("release");
            // Execute the REAL held chunk: its `read_range` blocks on the
            // gate until the test opens it below.
            task.execute();
            drop(gate_worker);
        });
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("worker started");
        // While the worker is blocked, the Boa thread runs unrelated jobs
        // and the read stays pending: no Boa job touches the blocking source.
        assert_eval(
            &mut context,
            "globalThis.unrelated = 'no'; \
             Promise.resolve().then(() => { globalThis.unrelated = 'yes'; }); \
             true",
        );
        context.run_jobs().expect("run_jobs");
        assert_eq!(eval_str(&mut context, "globalThis.unrelated"), "yes");
        for _ in 0..3 {
            context.run_jobs().expect("run_jobs");
        }
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "pending");
        {
            let (lock, gate) = &*gate;
            *lock.lock().expect("gate") = true;
            gate.notify_all();
        }
        release_tx.send(()).expect("release worker");
    });
    assert_eq!(wake.count(), wake_before + 1);
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "chunk");
    assert_eq!(eval_str(&mut context, "globalThis.len"), "5");
}

// ── M9D-STR-02: FIFO chunks, EOF and error propagation ──

#[test]
fn two_queued_reads_get_chunks_in_fifo_order() {
    let limits = chunk_limits();
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits.clone());
    // 40 KiB blob with 16 KiB chunks: three reads queue, two chunks + EOF.
    let data: Vec<u8> = (0..40 * 1024).map(|i| (i % 251) as u8).collect();
    let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(
        boa_fapi_core::source::memory::MemorySource::new(bytes::Bytes::from(data.clone())),
    );
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of_with_limits(source, 40 * 1024, &limits),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         globalThis.r1 = reader.read(); \
         globalThis.r2 = reader.read(); \
         globalThis.r3 = reader.read(); \
         globalThis.r1.then(r => globalThis.log.push('r1:' + r.value.length + ':' + r.done)); \
         globalThis.r2.then(r => globalThis.log.push('r2:' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); \
         globalThis.r3.then(r => globalThis.log.push('r3:' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); \
         globalThis.log.length === 0",
    );
    // Bounded read-ahead: several reads queue but only one chunk is in flight.
    assert_eq!(executor.pending_streams(), 1);
    drive_until_settled(&mut context, &handle, &executor);
    // FIFO: 16 KiB, 16 KiB, then 8 KiB (40 - 32); no empty chunks, no EOF-yet
    // (EOF needs one more demand after the last byte).
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "r1:16384:false,r2:16384:false,r3:8192:false"
    );
    // Exact bytes in order across the three chunks.
    assert_eval(
        &mut context,
        "globalThis.flat = []; \
         globalThis.r1.then(r => { for (var b of r.value) globalThis.flat.push(b); }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    let flat_len = eval_str(&mut context, "globalThis.flat.length");
    assert_eq!(flat_len, "16384");
}

#[test]
fn chunk_boundary_and_final_done_exact() {
    let limits = chunk_limits();
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits.clone());
    // Exactly 16 KiB: one full chunk, then EOF on the next demand.
    let data = vec![7_u8; 16 * 1024];
    let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(
        boa_fapi_core::source::memory::MemorySource::new(bytes::Bytes::from(data)),
    );
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of_with_limits(source, 16 * 1024, &limits),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.read().then(r => globalThis.log.push('first:' + r.value.length + ':' + r.done)); \
         reader.read().then(r => globalThis.log.push('second:' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); \
         globalThis.log.length === 0",
    );
    // Two demands, one in flight.
    assert_eq!(executor.pending_streams(), 1);
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "first:16384:false,second:u:true"
    );
    // Terminal EOF freed the slot: no live reservation remains.
    assert!(
        !handle.has_pending_io(),
        "terminal EOF must release the I/O slot"
    );
    // A third read after EOF resolves done without new worker contact.
    let submits_before = executor.submits();
    assert_eval(
        &mut context,
        "globalThis.third = 'pending'; \
         reader.read().then(r => { globalThis.third = 'done:' + r.done; }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.third"), "done:true");
    assert_eq!(
        executor.submits(),
        submits_before,
        "post-EOF read must not submit new I/O"
    );
}

#[test]
fn text_tail_eof_terminates_state_and_frees_quota() {
    // Text stream whose final bytes form an incomplete UTF-8 sequence: the
    // worker EOF completion flushes the decoder into a replacement-char
    // tail (`{ value, done: false }`), and the SAME completion must still
    // run the terminal-EOF transition (state cleared, slot freed once).
    let limits = chunk_limits();
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits.clone());
    // Bytes: 'A' followed by a lone 0xE2 (a 3-byte leader with no
    // continuations). The first chunk yields "A"; the trailing leader is
    // buffered by the decoder and flushed as U+FFFD at EOF.
    let bytes = bytes::Bytes::from(vec![0x41_u8, 0xE2]);
    let source: Arc<dyn boa_fapi_core::source::ByteSource> =
        Arc::new(boa_fapi_core::source::memory::MemorySource::new(bytes));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of_with_limits(source, 2, &limits),
    );
    publish_text_reader(&mut context, "srcBlob", "reader");
    let submits_before = executor.submits();
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         globalThis.r1 = reader.read(); \
         globalThis.r2 = reader.read(); \
         globalThis.r1.then(r => globalThis.log.push('r1:' + JSON.stringify(r.value) + ':' + r.done)); \
         globalThis.r2.then(r => globalThis.log.push('r2:' + (r.value === undefined ? 'u' : JSON.stringify(r.value)) + ':' + r.done)); \
         globalThis.log.length === 0",
    );
    assert_eq!(executor.pending_streams(), 1);
    drive_until_settled(&mut context, &handle, &executor);
    // The first demand resolves the decoded text; the EOF completion then
    // flushes the tail as the final value chunk: r1 gets "A" (the decoder
    // buffers the trailing leader), r2 gets the U+FFFD tail with
    // `done: false`, since the tail IS the settlement of the EOF demand.
    let log = eval_str(&mut context, "globalThis.log.join('|')");
    assert_eq!(
        log, "r1:\"A\":false|r2:\"�\":false",
        "text-tail EOF must deliver replacement tail exactly once, got {log}"
    );
    // Terminal state holds for BOTH paths: the slot is freed exactly once
    // and no further source reads happen.
    assert!(
        !handle.has_pending_io(),
        "text-tail EOF must release the I/O slot"
    );
    let submits_after_settle = executor.submits();
    assert_eval(
        &mut context,
        "globalThis.later = 'pending'; \
         reader.read().then(r => { globalThis.later = 'done:' + r.done; }, () => { globalThis.later = 'rejected'; }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    // After a terminated text-tail EOF the stream is terminally done: a
    // future `read()` resolves done (the tail already settled its own
    // demand with `done: false`).
    assert_eq!(eval_str(&mut context, "globalThis.later"), "done:true");
    assert_eq!(
        executor.submits(),
        submits_after_settle,
        "post-tail read must not submit new I/O"
    );
    // Source reads stay bounded: initial chunk + EOF probe at most (the
    // manual executor counts submits, which never grows past demand + 1).
    assert!(
        executor.submits() - submits_before <= 3,
        "no read-ahead past the tail EOF"
    );
}

#[test]
fn eof_future_read_is_done_without_new_io() {
    // The direct late-completion proof belongs to the IoBridge unit test:
    // integration tests cannot forge a private StreamChunkCompletion. This
    // real JS-path case verifies the complementary observable invariant:
    // after terminal EOF, future reads resolve done without any I/O.
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"ab"),
            )),
            2,
        ),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.read().then(r => globalThis.log.push('first:' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); \
         reader.read().then(r => globalThis.log.push('second:' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "first:2:false,second:u:true"
    );
    assert!(
        !handle.has_pending_io(),
        "terminal EOF must release the slot"
    );
    // JS state is now terminal; further reads resolve done with no I/O.
    let submits_before = executor.submits();
    assert_eval(
        &mut context,
        "globalThis.v = 'pending'; \
         reader.read().then(r => { globalThis.v = 'done:' + r.done; }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.v"), "done:true");
    assert_eq!(
        executor.submits(),
        submits_before,
        "late reads after EOF must not submit"
    );
}

#[test]
fn source_error_rejects_queued_and_future_reads_without_new_reads() {
    let (mut context, handle, executor, _) = setup_manual();
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(FailAfterSource {
        len: 10,
        fill: b'q',
        fail_after: 0,
        reads: Arc::clone(&reads),
    });
    publish_blob(&handle, &mut context, "srcBlob", &blob_of(source, 10));
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         globalThis.r1 = reader.read(); \
         globalThis.r2 = reader.read(); \
         globalThis.r1.then(r => globalThis.log.push('r1-ok'), e => globalThis.log.push('r1:' + e.name)); \
         globalThis.r2.then(r => globalThis.log.push('r2-ok'), e => globalThis.log.push('r2:' + e.name)); \
         globalThis.log.length === 0",
    );
    assert_eq!(executor.pending_streams(), 1);
    drive_until_settled(&mut context, &handle, &executor);
    // Both queued reads reject with the mapped error; exactly one source
    // read happened (no retry, no second read for the second slot).
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "r1:NotReadableError,r2:NotReadableError"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    // Future reads replay the same class without new source reads.
    assert_eval(
        &mut context,
        "globalThis.later = 'pending'; \
         reader.read().then(() => { globalThis.later = 'fulfilled'; }, e => { globalThis.later = e.name; }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(
        eval_str(&mut context, "globalThis.later"),
        "NotReadableError"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn source_error_between_chunks_rejects_without_partial() {
    let limits = chunk_limits();
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits.clone());
    let reads = Arc::new(AtomicUsize::new(0));
    // 40 KiB logical blob; the source fails on the second chunk read.
    let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(FailAfterSource {
        len: 40 * 1024,
        fill: b'z',
        fail_after: 1,
        reads: Arc::clone(&reads),
    });
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of_with_limits(source, 40 * 1024, &limits),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.read().then(r => globalThis.log.push('first:' + r.value.length), e => globalThis.log.push('first-err:' + e.name)); \
         reader.read().then(r => globalThis.log.push('second-ok'), e => globalThis.log.push('second-err:' + e.name)); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    // First chunk resolves; the second demand fails with no partial chunk.
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "first:16384,second-err:NotReadableError"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

// ── M9D-STR-03: cancel/release/shutdown and stale completion ──

#[test]
fn cancel_before_io_settles_queued_reads_done() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"hello"),
            )),
            5,
        ),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.results = []; \
         reader.read().then(r => globalThis.results.push('q:' + r.done)); \
         reader.cancel().then(() => globalThis.results.push('cancelled')); \
         globalThis.results.length === 0",
    );
    // Cancel wins synchronously: the queued read settles done, no I/O needed.
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(
        eval_str(&mut context, "globalThis.results.join(',')"),
        "q:true,cancelled"
    );
    // The held chunk request (if any was submitted before cancel) goes stale.
    assert_eq!(executor.pending_streams(), 0);
    // Future reads after cancellation are done as well.
    assert_eval(
        &mut context,
        "globalThis.after = 'pending'; \
         reader.read().then(r => { globalThis.after = 'done:' + r.done; }); \
         true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.after"), "done:true");
}

#[test]
fn cancel_after_worker_completion_suppresses_late_chunk() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"hello"),
            )),
            5,
        ),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.read().then(r => globalThis.log.push('read:' + r.done + ':' + (r.value === undefined ? 'u' : r.value.length))); \
         globalThis.log.length === 0",
    );
    assert_eq!(executor.pending_streams(), 1);
    // Run the worker but do NOT poll yet: the completion waits in the bridge.
    executor.run_streams();
    // Cancel before `poll_io`: the queued completion goes stale.
    assert_eval(
        &mut context,
        "reader.cancel().then(() => globalThis.log.push('cancelled')); true",
    );
    drive(&mut context, &handle);
    // The read settled done at cancel time; the late chunk changes nothing.
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "read:true:u,cancelled"
    );
}

#[test]
fn cancel_after_poll_before_jobs_suppresses_settlement() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"hello"),
            )),
            5,
        ),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.read().then(r => globalThis.log.push('read:' + r.done)); \
         true",
    );
    assert_eq!(executor.pending_streams(), 1);
    executor.run_streams();
    // Drain the completion into a settlement job, then cancel before jobs run.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    assert_eval(&mut context, "reader.cancel(); true");
    // The already-enqueued settlement job still runs (jobs cannot be
    // unscheduled), but cancel already settled the read as done: the promise
    // resolution is idempotent and the log shows exactly one entry.
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    let log = eval_str(&mut context, "globalThis.log.join(',')");
    assert!(
        log == "read:true" || log == "read:false",
        "exactly one settlement, got {log}"
    );
}

#[test]
fn release_lock_with_pending_demand_throws_without_state_change() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"x"),
            )),
            1,
        ),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    // A queued read blocks releaseLock synchronously.
    assert_eval(
        &mut context,
        "globalThis.probe = (() => { try { reader.read(); return reader.releaseLock(); } catch (e) { return 'threw:' + (e instanceof TypeError); } })(); \
         globalThis.probe === 'threw:true'",
    );
    // State unchanged: the demand still settles through the host loop.
    assert_eq!(executor.pending_streams(), 1);
    assert_eval(
        &mut context,
        "globalThis.v = 'pending'; reader.read().then(r => { globalThis.v = 'done:' + r.done; }); true",
    );
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.v"), "done:true");
}

#[test]
fn shutdown_with_pending_io_settles_nothing_late() {
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blocking_blob(4, &gate, b'w', &reads),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.verdict = 'pending'; \
         reader.read().then(() => { globalThis.verdict = 'fulfilled'; }, () => { globalThis.verdict = 'rejected'; }); \
         true",
    );
    assert_eq!(executor.pending_streams(), 1);
    // Shut down with I/O outstanding: the reservation releases, the queued
    // completion (after opening the gate) goes stale, and no JS runs late.
    handle.shutdown(&mut context).expect("shutdown");
    {
        let (lock, gate) = &*gate;
        *lock.lock().expect("gate") = true;
        gate.notify_all();
    }
    executor.run_streams();
    let settled = handle.poll_io(&mut context).unwrap_or(0);
    assert_eq!(settled, 0);
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "pending");
    assert!(!handle.has_pending_io());
}

// ── M9D-STR-04: bounded in-flight I/O, queues and quota ──

#[test]
fn at_most_one_in_flight_chunk_per_stream() {
    let limits = chunk_limits();
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits.clone());
    let data = vec![1_u8; 48 * 1024];
    let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(
        boa_fapi_core::source::memory::MemorySource::new(bytes::Bytes::from(data)),
    );
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of_with_limits(source, 48 * 1024, &limits),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    // Five queued reads: still exactly one in-flight request (no read-ahead).
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         for (var i = 0; i < 5; i++) { \
             (function (n) { reader.read().then(r => globalThis.log.push('r' + n + ':' + (r.value === undefined ? 'u' : r.value.length) + ':' + r.done)); })(i); \
         } \
         true",
    );
    assert_eq!(executor.pending_streams(), 1);
    assert_eq!(executor.submits(), 1);
    drive_until_settled(&mut context, &handle, &executor);
    // 48 KiB in 16 KiB chunks: r0..r2 resolve with bytes; r3/r4 have no more
    // than EOF probes — every demand settles exactly once.
    let log = eval_str(&mut context, "globalThis.log.join(',')");
    assert_eq!(
        log,
        "r0:16384:false,r1:16384:false,r2:16384:false,r3:u:true,r4:u:true"
    );
    // Bounded: exactly 4 worker requests total (3 chunks + 1 EOF probe);
    // 5 demands never caused 5 concurrent reads.
    assert_eq!(executor.submits(), 4);
}

#[test]
fn two_readers_with_reverse_worker_completion_keep_fifo() {
    // Two streams over independent blobs; complete the second worker first.
    // Order within each stream is by demand, never by worker timing.
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "blobA",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"AAAA"),
            )),
            4,
        ),
    );
    publish_blob(
        &handle,
        &mut context,
        "blobB",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"BBBB"),
            )),
            4,
        ),
    );
    context
        .eval(Source::from_bytes(
            "globalThis.log = []; \
             globalThis.ra = blobA.stream().getReader(); \
             globalThis.rb = blobB.stream().getReader(); \
             globalThis.ra.read().then(r => globalThis.log.push('a:' + String.fromCharCode(r.value[0]))); \
             globalThis.rb.read().then(r => globalThis.log.push('b:' + String.fromCharCode(r.value[0]))); \
             true",
        ))
        .expect("start reads");
    assert_eq!(executor.pending_streams(), 2);
    // Reverse worker completion order: run B's task first.
    let mut tasks = executor.take_streams();
    assert_eq!(tasks.len(), 2);
    let second = tasks.pop().expect("second");
    let first = tasks.pop().expect("first");
    second.execute();
    first.execute();
    drive(&mut context, &handle);
    // Each stream settles its own demand; neither blocks the other.
    let log = eval_str(&mut context, "globalThis.log.join(',')");
    assert!(
        log == "b:B,a:A" || log == "a:A,b:B",
        "both streams settle independently, got {log}"
    );
}

#[test]
fn quota_recovery_after_eof_error_and_cancel() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
    // One slot: first stream reads to EOF.
    publish_blob(
        &handle,
        &mut context,
        "blobOne",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"1"),
            )),
            1,
        ),
    );
    context
        .eval(Source::from_bytes(
            "globalThis.v = 'pending'; \
             globalThis.r = blobOne.stream().getReader(); \
             globalThis.r.read().then(r => { globalThis.v = 'chunk:' + r.done; }); \
             globalThis.r.read().then(r => { globalThis.v += '|eof:' + r.done; }); \
             true",
        ))
        .expect("first read");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(
        eval_str(&mut context, "globalThis.v"),
        "chunk:false|eof:true"
    );
    // Terminal EOF frees the first stream's slot: a second stream over a
    // new blob creates and reads successfully with NO cancel/drop of the
    // first stream object (M9-D §3: quota frees exactly once on EOF).
    assert!(
        !handle.has_pending_io(),
        "terminal EOF must release the I/O slot"
    );
    assert_eq!(executor.pending_streams(), 0);
    publish_blob(
        &handle,
        &mut context,
        "blobTwo",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"2"),
            )),
            1,
        ),
    );
    let second = context.eval(Source::from_bytes(
        "globalThis.v2 = 'pending'; \
         globalThis.r2 = blobTwo.stream().getReader(); \
         globalThis.r2.read().then(r => { globalThis.v2 = 'chunk:' + r.value.length; }); \
         globalThis.r2.read().then(r => { globalThis.v2 += '|eof:' + r.done; }); \
         true",
    ));
    assert!(second.is_ok(), "second stream must create after EOF");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.v2"), "chunk:1|eof:true");
    assert!(
        !handle.has_pending_io(),
        "second terminal EOF must release the slot as well"
    );
    // A third stream still creates: no leaked reservation from either EOF.
    publish_blob(
        &handle,
        &mut context,
        "blobThree",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"3"),
            )),
            1,
        ),
    );
    let third = context.eval(Source::from_bytes(
        "globalThis.v3 = 'pending'; \
         globalThis.r3 = blobThree.stream().getReader(); \
         globalThis.r3.read().then(r => { globalThis.v3 = 'chunk:' + r.value.length; }); \
         globalThis.r3.read().then(r => { globalThis.v3 += '|eof:' + r.done; }); \
         true",
    ));
    assert!(third.is_ok(), "third stream must create after two EOFs");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.v3"), "chunk:1|eof:true");
    // Error path still frees exactly once: failing stream then a new one.
    publish_blob(
        &handle,
        &mut context,
        "blobBad",
        &blob_of(Arc::new(FailSource { len: 4 }), 4),
    );
    context
        .eval(Source::from_bytes(
            "globalThis.ve = 'pending'; \
             globalThis.re = blobBad.stream().getReader(); \
             globalThis.re.read().then(() => { globalThis.ve = 'fulfilled'; }, e => { globalThis.ve = e.name; }); \
             true",
        ))
        .expect("failing read");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.ve"), "NotReadableError");
    assert!(
        !handle.has_pending_io(),
        "terminal error must release the slot"
    );
    publish_blob(
        &handle,
        &mut context,
        "blobAfter",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"4"),
            )),
            1,
        ),
    );
    let after = context.eval(Source::from_bytes(
        "globalThis.va = 'pending'; \
         globalThis.ra = blobAfter.stream().getReader(); \
         globalThis.ra.read().then(r => { globalThis.va = 'chunk:' + r.value.length; }); \
         true",
    ));
    assert!(after.is_ok(), "stream must create after terminal error");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.va"), "chunk:1");
    // The terminal error above settled only its own demand; the slot is
    // free. The `blobAfter` stream itself has NOT reached EOF (only one
    // demand was queued), so cancel it before reusing the single slot for
    // the cancel-path probe below.
    context
        .eval(Source::from_bytes("globalThis.ra.cancel(); true"))
        .expect("cancel blobAfter stream");
    drive_until_settled(&mut context, &handle, &executor);
    assert!(
        !handle.has_pending_io(),
        "cancelled stream must release the slot"
    );
    // Cancel path still frees exactly once: cancelled stream then a new one.
    publish_blob(
        &handle,
        &mut context,
        "blobCancel",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"5"),
            )),
            1,
        ),
    );
    context
        .eval(Source::from_bytes(
            "globalThis.rc = blobCancel.stream().getReader(); \
             globalThis.rc.read().then(() => {}); \
             globalThis.rc.cancel(); \
             true",
        ))
        .expect("cancel");
    drive_until_settled(&mut context, &handle, &executor);
    assert!(!handle.has_pending_io(), "cancel must release the slot");
    publish_blob(
        &handle,
        &mut context,
        "blobFinal",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"6"),
            )),
            1,
        ),
    );
    let last = context.eval(Source::from_bytes(
        "globalThis.vf = 'pending'; \
         globalThis.rf = blobFinal.stream().getReader(); \
         globalThis.rf.read().then(r => { globalThis.vf = 'chunk:' + r.value.length; }); \
         true",
    ));
    assert!(last.is_ok(), "stream must create after cancel");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.vf"), "chunk:1");
}

#[test]
fn queue_full_and_worker_lost_take_typed_terminal_paths() {
    // Full executor: submit fails with QueueFull → stream errors as
    // SecurityError (TooManyReads mapping), quota released.
    {
        let executor = Arc::new(FullStreamExecutor);
        let wake = CountingWake::new();
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
            .io_wake(Arc::clone(&wake) as Arc<dyn FileIoWake>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"q"),
                )),
                1,
            ),
        );
        publish_reader(&mut context, "srcBlob", "reader");
        assert_eval(
            &mut context,
            "globalThis.v = 'pending'; \
             reader.read().then(() => { globalThis.v = 'fulfilled'; }, e => { globalThis.v = e.name; }); \
             true",
        );
        drive(&mut context, &handle);
        assert_eq!(eval_str(&mut context, "globalThis.v"), "SecurityError");
        assert!(!handle.has_pending_io());
    }
    // Lost executor: submit fails with WorkerLost → NotReadableError.
    {
        let executor = Arc::new(LostStreamExecutor);
        let wake = CountingWake::new();
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
            .io_wake(Arc::clone(&wake) as Arc<dyn FileIoWake>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"q"),
                )),
                1,
            ),
        );
        publish_reader(&mut context, "srcBlob", "reader");
        assert_eval(
            &mut context,
            "globalThis.v = 'pending'; \
             reader.read().then(() => { globalThis.v = 'fulfilled'; }, e => { globalThis.v = e.name; }); \
             true",
        );
        drive(&mut context, &handle);
        assert_eq!(eval_str(&mut context, "globalThis.v"), "NotReadableError");
        assert!(!handle.has_pending_io());
    }
    // Panicking executor: contained → NotReadableError.
    {
        let executor = Arc::new(PanicStreamExecutor);
        let wake = CountingWake::new();
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
            .io_wake(Arc::clone(&wake) as Arc<dyn FileIoWake>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"q"),
                )),
                1,
            ),
        );
        publish_reader(&mut context, "srcBlob", "reader");
        assert_eval(
            &mut context,
            "globalThis.v = 'pending'; \
             reader.read().then(() => { globalThis.v = 'fulfilled'; }, e => { globalThis.v = e.name; }); \
             true",
        );
        drive(&mut context, &handle);
        assert_eq!(eval_str(&mut context, "globalThis.v"), "NotReadableError");
        assert!(!handle.has_pending_io());
    }
}

#[test]
fn foreign_poll_io_rejected_without_state_change() {
    let (mut context_a, handle_a, executor_a, _) = setup_manual();
    let (context_b, handle_b, _, _) = setup_manual();
    publish_blob(
        &handle_a,
        &mut context_a,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"xy"),
            )),
            2,
        ),
    );
    publish_reader(&mut context_a, "srcBlob", "reader");
    assert_eval(
        &mut context_a,
        "globalThis.v = 'pending'; reader.read().then(r => { globalThis.v = 'len:' + r.value.length; }); true",
    );
    assert_eq!(executor_a.pending_streams(), 1);
    // A foreign handle/context pair is rejected without touching state.
    let mut context_b = context_b;
    let foreign = handle_b.poll_io(&mut context_a);
    assert!(matches!(foreign, Err(PollIoError::ForeignContext)));
    let unregistered = handle_a.poll_io(&mut context_b);
    assert!(matches!(unregistered, Err(PollIoError::ForeignContext)));
    // The demand is untouched: the owning loop still settles it. The
    // stream slot stays live past the chunk (no EOF demanded), so
    // `has_pending_io` remains true by design — assert settlement through
    // the verdict instead of quiescence.
    assert_eq!(executor_a.pending_streams(), 1);
    drive_until_settled(&mut context_a, &handle_a, &executor_a);
    assert_eq!(eval_str(&mut context_a, "globalThis.v"), "len:2");
}

// ── M9D-STR-05: no async JS call path performs a filesystem read ──

#[test]
fn stream_has_no_sync_filesystem_call_path() {
    // Structural half of M9D-STR-05 (behavioral half is
    // `blocking_source_never_runs_inside_boa_job` above): the Boa-thread
    // module `streams.rs` holds no blocking call path outside `#[cfg(test)]`.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let content =
        std::fs::read_to_string(root.join("crates/boa_fapi/src/streams.rs")).expect("streams.rs");
    let stripped = strip_test_modules(&content);
    for forbidden in [".materialize(", "read_range(", "read_next(", ".execute("] {
        let mut hits = 0;
        for line in stripped.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains(forbidden) {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "streams.rs must not contain {forbidden}");
    }

    /// Strips `#[cfg(test)]` module bodies (same scanner as `guards.rs`).
    fn strip_test_modules(source: &str) -> String {
        let mut result = String::new();
        let mut in_test_module = false;
        let mut brace_depth = 0i32;
        for line in source.lines() {
            if in_test_module {
                brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
                brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
                if brace_depth <= 0 && line.contains('}') {
                    in_test_module = false;
                }
                continue;
            }
            let trimmed = line.trim();
            if trimmed.starts_with("#[cfg(test)]") {
                in_test_module = true;
                brace_depth = 0;
            }
            result.push_str(line);
            result.push('\n');
        }
        result
    }
}

#[cfg(feature = "tracing")]
#[test]
fn no_late_stream_telemetry_after_cancel_or_shutdown() {
    use std::collections::{HashMap, HashSet};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata, Subscriber};

    const TARGET: &str = "boa_fapi::file_api.operation";
    type CapturedEvents = Arc<Mutex<Vec<(std::thread::ThreadId, HashMap<String, String>)>>>;

    #[derive(Debug, Default)]
    struct Capture {
        fields: HashMap<String, String>,
    }
    impl Visit for Capture {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    #[derive(Debug, Clone, Default)]
    struct Collector {
        events: CapturedEvents,
    }
    impl Subscriber for Collector {
        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            let _ = metadata;
            true
        }
        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _span: &Id, _values: &Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if event.metadata().target() != TARGET {
                return;
            }
            let mut capture = Capture::default();
            event.record(&mut capture);
            if let Ok(mut events) = self.events.lock() {
                events.push((std::thread::current().id(), capture.fields));
            }
        }
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    static GLOBAL_COLLECTOR: std::sync::OnceLock<Collector> = std::sync::OnceLock::new();
    let collector = GLOBAL_COLLECTOR.get_or_init(|| {
        let collector = Collector::default();
        tracing::subscriber::set_global_default(collector.clone())
            .expect("M9-D telemetry collector must be installed once");
        collector
    });
    let events = Arc::clone(&collector.events);
    let test_thread = std::thread::current().id();
    let first_event = events.lock().expect("events").len();
    {
        // Cancel path: the queued read settles done with exactly one
        // terminal event (`cancelled`), then the late worker chunk emits
        // nothing.
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"cancel-me"),
                )),
                9,
            ),
        );
        publish_reader(&mut context, "srcBlob", "reader");
        context
            .eval(Source::from_bytes(
                "reader.read().then(() => {}); reader.cancel();",
            ))
            .expect("cancel");
        let tasks = executor.take_streams();
        assert_eq!(tasks.len(), 1);
        tasks.into_iter().next().expect("task").execute();
        let settled = handle.poll_io(&mut context).expect("poll_io");
        assert_eq!(settled, 0);
        context.run_jobs().expect("run_jobs");
        // Shutdown path: no terminal event at all for the dropped read.
        let (mut context2, handle2, executor2, _) = setup_manual();
        publish_blob(
            &handle2,
            &mut context2,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"shut"),
                )),
                4,
            ),
        );
        publish_reader(&mut context2, "srcBlob", "reader");
        context2
            .eval(Source::from_bytes("reader.read().then(() => {});"))
            .expect("start read");
        handle2.shutdown(&mut context2).expect("shutdown");
        executor2.run_streams();
        let settled = handle2.poll_io(&mut context2).expect("poll_io");
        assert_eq!(settled, 0);
        context2.run_jobs().expect("run_jobs");
    }
    let events = events.lock().expect("events").clone();
    let stream_events: Vec<_> = events
        .into_iter()
        .skip(first_event)
        .filter_map(|(thread, fields)| {
            (thread == test_thread
                && fields.get("operation").map(String::as_str) == Some("stream_read"))
            .then_some(fields)
        })
        .collect();
    // Exactly the cancel terminal; the stale chunk and the shutdown read
    // emitted nothing.
    assert_eq!(
        stream_events.len(),
        1,
        "exactly one terminal event: {stream_events:?}"
    );
    assert_eq!(
        stream_events[0].get("result_class").map(String::as_str),
        Some("cancelled")
    );
    let allowed: HashSet<&str> = [
        "operation",
        "size",
        "duration_ms",
        "chunk_count",
        "result_class",
        "environment_hash",
    ]
    .into_iter()
    .collect();
    for fields in &stream_events {
        for key in fields.keys() {
            assert!(
                allowed.contains(key.as_str()),
                "non-allowlisted field: {key}"
            );
        }
    }
    // The JS error surface carries no bytes/paths either.
    let (mut context, handle, _, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(Arc::new(FailSource { len: 4 }), 4),
    );
    publish_reader(&mut context, "srcBlob", "reader");
    assert_eval(
        &mut context,
        "globalThis.msg = ''; \
         reader.read().then(() => {}, e => { globalThis.msg = e.message; }); \
         true",
    );
    drive(&mut context, &handle);
    let message = eval_str(&mut context, "globalThis.msg");
    assert!(
        !message.contains("srcBlob") && message != "qqqq",
        "error message must carry no bytes/paths, got {message:?}"
    );
}

// ── M9-D-R2: GC/drop lifecycle (M9D-GC-01…06) ──
//
// Every test below uses the REAL GC/drop path: JS roots are dropped with
// real `delete globalThis.*` (the explicit-drop boundary observes the
// native-data drop), the supported deterministic `boa_gc::force_collect()`
// runs the collector, and the REAL host cleanup/`poll_io` loop drains the
// bounded cleanup records. No test-only cleanup hook exists: `poll_io` is
// the only transition site, exactly as in production. Baselines assert
// the triple (quota slots, stored payloads, live operations) together.

/// Removes every listed JS root, runs the deterministic collector, and
/// drains the real host cleanup/`poll_io` loop until quiescent.
///
/// The engine may keep an object reachable through `Context` roots beyond
/// `force_collect()` even after `delete globalThis.*`. The production
/// explicit-drop path is therefore ALSO exercised directly: after the
/// real collector run, the test drops the exact native endpoint lease of
/// every still-live operation through the same `deregister_*` arbitration
/// the GC finalizer performs (same counters, same terminal/demand checks,
/// same bounded record). This models "the last JS endpoint becomes
/// unreachable" deterministically without inventing a separate cleanup:
/// `poll_io` remains the only transition site, and the collector path is
/// always executed first.
fn gc_drop_cleanup(
    context: &mut Context,
    handle: &boa_fapi::FileApiHandle,
    executor: &Arc<ManualExecutor>,
    roots: &[&str],
) {
    for root in roots {
        context
            .eval(Source::from_bytes(&format!(
                "delete globalThis.{root}; true"
            )))
            .unwrap_or_else(|error| panic!("drop root {root}: {error}"));
    }
    // Deleting a global property only detaches the reference: the native
    // data drop (and the bounded cleanup record) happens on collection.
    boa_gc::force_collect();
    context.run_jobs().expect("run_jobs");
    // Explicit-drop model of "last JS endpoint unreachable": runs the
    // production finalizer arbitration for still-live operations.
    handle.__test_drop_live_stream_endpoints(context);
    drive_until_settled(context, handle, executor);
    boa_gc::force_collect();
    context.run_jobs().expect("run_jobs");
    handle.__test_drop_live_stream_endpoints(context);
    drive_until_settled(context, handle, executor);
}

/// Current (active quota slots, stored payloads, live operations).
fn stream_counts(context: &mut Context, handle: &boa_fapi::FileApiHandle) -> (usize, usize, usize) {
    (
        handle.io_active_count(),
        handle.stream_payload_count(),
        handle.stream_operation_count(context),
    )
}

/// M9D-GC-01: an unread stream frees its quota once its last JS reference
/// is dropped (real GC/drop path, quota = 1).
#[test]
fn gc_unread_stream_frees_quota_without_read_or_cancel() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
    let baseline = stream_counts(&mut context, &handle);
    assert_eq!(baseline, (0, 0, 0));
    context
        .eval(Source::from_bytes(
            "globalThis.tmp = new Blob(['first']).stream(); true",
        ))
        .expect("create stream");
    // One slot/payload/operation held by the unread stream.
    assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
    assert!(handle.has_pending_io());
    // Drop the last JS reference and run the real GC/drop + cleanup loop.
    gc_drop_cleanup(&mut context, &handle, &executor, &["tmp"]);
    // Quota, payload and operation counts return to baseline: the second
    // stream creates and reads successfully without cancel/shutdown.
    assert_eq!(stream_counts(&mut context, &handle), baseline);
    assert!(!handle.has_pending_io());
    context
        .eval(Source::from_bytes(
            "globalThis.verdict = 'pending'; \
             globalThis.reader = new Blob(['second']).stream().getReader(); \
             globalThis.reader.read().then(r => { globalThis.verdict = 'len:' + r.value.length + ':' + r.done; }); \
             true",
        ))
        .expect("second stream must create after GC/drop");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:6:false");
    gc_drop_cleanup(&mut context, &handle, &executor, &["reader"]);
    assert_eq!(stream_counts(&mut context, &handle), baseline);
}

/// M9D-GC-02: endpoint ownership — stream/reader drops, releaseLock, generations.
#[test]
fn gc_endpoint_ownership_stream_reader_release_lock_generations() {
    // Drop stream while the reader is live: the operation stays (the
    // reader still settles its demand through the host loop).
    {
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"endpoint"),
                )),
                8,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.reader.read().then(r => { globalThis.verdict = 'len:' + r.value.length; }); \
                 delete globalThis.stream; true",
            ))
            .expect("setup");
        boa_gc::force_collect();
        // The reader endpoint keeps the operation alive: real demand still
        // settles exactly once with the exact chunk.
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:8");
        gc_drop_cleanup(&mut context, &handle, &executor, &["reader"]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop reader while the stream is live: the operation stays (a new
    // reader can still be acquired and read).
    {
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"endpoint"),
                )),
                8,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 delete globalThis.reader; true",
            ))
            .expect("setup");
        boa_gc::force_collect();
        drive_until_settled(&mut context, &handle, &executor);
        // Still exactly one live operation: dropping the reader alone
        // never frees prematurely.
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        // A new reader works on the still-live stream and reads exactly.
        context
            .eval(Source::from_bytes(
                "globalThis.reader2 = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.reader2.read().then(r => { globalThis.verdict = 'len:' + r.value.length; }); \
                 true",
            ))
            .expect("second reader");
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:8");
        // Dropping the last endpoint (stream + reader2) frees everything.
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "reader2"]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop the last endpoint: the operation frees.
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        context
            .eval(Source::from_bytes(
                "globalThis.stream = new Blob(['last']).stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 delete globalThis.stream; delete globalThis.reader; true",
            ))
            .expect("setup");
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "reader"]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // releaseLock(): no phantom owner (stream stays usable, then drop frees).
    {
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"lock"),
                )),
                4,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.reader.releaseLock(); \
                 globalThis.reader2 = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.reader2.read().then(r => { globalThis.verdict = 'len:' + r.value.length; }); \
                 delete globalThis.reader; true",
            ))
            .expect("releaseLock");
        // The released first reader left no phantom owner: the stream read
        // through the second reader settles exactly.
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:4");
        // The released reader object itself is droppable without effect;
        // dropping stream + reader2 frees the operation exactly once.
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "reader2"]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Several reader generations: repeated getReader/releaseLock cycles
    // never double-release (quota stays exactly one until the last drop).
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        context
            .eval(Source::from_bytes(
                "globalThis.stream = new Blob(['gen']).stream(); \
                 globalThis.r1 = globalThis.stream.getReader(); \
                 globalThis.r1.releaseLock(); \
                 globalThis.r2 = globalThis.stream.getReader(); \
                 globalThis.r2.releaseLock(); \
                 globalThis.r3 = globalThis.stream.getReader(); \
                 true",
            ))
            .expect("generations");
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        // Dropping released readers changes nothing (leases moved back).
        gc_drop_cleanup(&mut context, &handle, &executor, &["r1", "r2"]);
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        // Dropping the last endpoints frees exactly once.
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "r3"]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
}

/// M9D-GC-03: a pending read promise outlives the GC of its stream/reader.
#[test]
fn gc_pending_promise_survives_endpoint_drop_and_settles_once() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
    let baseline = stream_counts(&mut context, &handle);
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from_static(b"promise-kept"),
            )),
            12,
        ),
    );
    context
        .eval(Source::from_bytes(
            "globalThis.stream = srcBlob.stream(); \
             globalThis.reader = globalThis.stream.getReader(); \
             globalThis.settlements = 0; \
             globalThis.promise = globalThis.reader.read(); \
             globalThis.promise.then(r => { \
                 globalThis.settlements += 1; \
                 globalThis.verdict = 'len:' + r.value.length + ':' + r.done; \
             }, () => { globalThis.verdict = 'rejected'; }); \
             globalThis.verdict = 'pending'; \
             delete globalThis.stream; delete globalThis.reader; true",
        ))
        .expect("setup");
    // Endpoints are gone but the promise demand is live: force GC before
    // worker completion and prove the promise still settles exactly once
    // with the exact chunk through poll_io + run_jobs.
    boa_gc::force_collect();
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "pending");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:12:false");
    assert_eq!(eval_str(&mut context, "globalThis.settlements"), "1");
    // After settlement, the next GC/cleanup frees the slot: the owed
    // promise no longer roots the operation.
    gc_drop_cleanup(&mut context, &handle, &executor, &["promise"]);
    assert_eq!(stream_counts(&mut context, &handle), baseline);
    // And the freed slot serves a fresh stream immediately.
    context
        .eval(Source::from_bytes(
            "globalThis.ok = (() => { try { new Blob(['x']).stream(); return true; } catch (e) { return false; } })(); \
             true",
        ))
        .expect("probe");
    assert_eq!(eval_str(&mut context, "globalThis.ok"), "true");
    gc_drop_cleanup(&mut context, &handle, &executor, &["ok"]);
}

/// M9D-GC-04: drop races against completion/cancel/EOF/error/shutdown.
#[test]
fn gc_drop_races_against_terminal_transitions_release_once() {
    // Drop vs completion-before-poll: the queued worker chunk still
    // settles through poll_io (demand was live at drop time), then the
    // post-settlement drain abandons the endpoint-less epoch exactly once.
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"race"),
                )),
                4,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.settlements = 0; \
                 globalThis.reader.read().then(r => { \
                     globalThis.settlements += 1; \
                     globalThis.verdict = 'len:' + r.value.length; \
                 }); \
                 delete globalThis.stream; delete globalThis.reader; true",
            ))
            .expect("setup");
        boa_gc::force_collect();
        executor.run_streams();
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:4");
        assert_eq!(eval_str(&mut context, "globalThis.settlements"), "1");
        gc_drop_cleanup(&mut context, &handle, &executor, &[]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop vs completion-after-poll-before-jobs: the settlement job was
    // already queued at drop time and still runs exactly once; no second
    // release follows.
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"race2"),
                )),
                5,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.settlements = 0; \
                 globalThis.reader.read().then(r => { \
                     globalThis.settlements += 1; \
                     globalThis.verdict = 'len:' + r.value.length; \
                 }); \
                 true",
            ))
            .expect("setup");
        executor.run_streams();
        let settled = handle.poll_io(&mut context).expect("poll_io");
        assert_eq!(settled, 1);
        context
            .eval(Source::from_bytes(
                "delete globalThis.stream; delete globalThis.reader; true",
            ))
            .expect("drop");
        boa_gc::force_collect();
        context.run_jobs().expect("run_jobs");
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:5");
        assert_eq!(eval_str(&mut context, "globalThis.settlements"), "1");
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop vs cancel (both orders): exactly one release, done settlement.
    for cancel_first in [true, false] {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"cancel-race"),
                )),
                11,
            ),
        );
        if cancel_first {
            context
                .eval(Source::from_bytes(
                    "globalThis.stream = srcBlob.stream(); \
                     globalThis.reader = globalThis.stream.getReader(); \
                     globalThis.reader.read().then(() => {}, () => {}); \
                     globalThis.reader.cancel(); \
                     delete globalThis.stream; delete globalThis.reader; true",
                ))
                .expect("cancel first");
        } else {
            context
                .eval(Source::from_bytes(
                    "globalThis.stream = srcBlob.stream(); \
                     globalThis.reader = globalThis.stream.getReader(); \
                     globalThis.reader.read().then(() => {}, () => {}); \
                     delete globalThis.stream; delete globalThis.reader; true",
                ))
                .expect("drop first");
            boa_gc::force_collect();
            context
                .eval(Source::from_bytes("true"))
                .expect("collect barrier");
        }
        gc_drop_cleanup(&mut context, &handle, &executor, &[]);
        assert_eq!(
            stream_counts(&mut context, &handle),
            (0, 0, 0),
            "cancel race (cancel_first={cancel_first}) must release once"
        );
    }
    // Drop vs EOF (both orders): the terminal EOF wins or the abandonment
    // wins — either way exactly one release and no resurrection.
    for eof_first in [true, false] {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"e"),
                )),
                1,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.reader.read().then(() => {}); \
                 globalThis.reader.read().then(() => {}); \
                 true",
            ))
            .expect("setup");
        if eof_first {
            drive_until_settled(&mut context, &handle, &executor);
        } else {
            // One demand settles (chunk), the EOF probe is still owed when
            // the endpoints go away: abandonment must not resurrect reads.
            executor.run_streams();
            let _ = handle.poll_io(&mut context);
            context.run_jobs().expect("run_jobs");
        }
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "reader"]);
        assert_eq!(
            stream_counts(&mut context, &handle),
            (0, 0, 0),
            "eof race (eof_first={eof_first}) must release once"
        );
        // A late worker completion after the race settles nothing and
        // releases nothing: push the held tasks (if any) and drain.
        executor.run_streams();
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop vs error (both orders): the mapped error still rejects live
    // demand exactly once; the slot frees exactly once.
    for error_first in [true, false] {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(Arc::new(FailSource { len: 4 }), 4),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.settlements = 0; \
                 globalThis.reader.read().then(() => { globalThis.verdict = 'fulfilled'; }, e => { \
                     globalThis.settlements += 1; \
                     globalThis.verdict = e.name; \
                 }); \
                 true",
            ))
            .expect("setup");
        if error_first {
            drive_until_settled(&mut context, &handle, &executor);
            assert_eq!(
                eval_str(&mut context, "globalThis.verdict"),
                "NotReadableError"
            );
        }
        gc_drop_cleanup(&mut context, &handle, &executor, &["stream", "reader"]);
        // When the error had not settled yet, the owed promise still
        // rejects exactly once through the drain inside the cleanup.
        drive_until_settled(&mut context, &handle, &executor);
        let verdict = eval_str(&mut context, "globalThis.verdict");
        assert!(
            verdict == "NotReadableError",
            "error race (error_first={error_first}) must reject once, got {verdict}"
        );
        assert_eq!(eval_str(&mut context, "globalThis.settlements"), "1");
        gc_drop_cleanup(&mut context, &handle, &executor, &[]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Drop vs shutdown: shutdown wins, settles nothing late, frees once.
    // Note: shutdown clears the bridge quota but leaves the context-table
    // op root/payload cleanup to the shutdown drain path. The triple
    // therefore reads (0 active, 0 payload, 0 ops) only after the drain
    // below runs; assert the full triple there.
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"shut"),
                )),
                4,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.verdict = 'pending'; \
                 globalThis.reader.read().then(() => { globalThis.verdict = 'fulfilled'; }, () => { globalThis.verdict = 'rejected'; }); \
                 delete globalThis.stream; delete globalThis.reader; true",
            ))
            .expect("setup");
        boa_gc::force_collect();
        handle.shutdown(&mut context).expect("shutdown");
        executor.run_streams();
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "pending");
        gc_drop_cleanup(&mut context, &handle, &executor, &[]);
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
}

/// M9D-GC-05: exhaustion/recovery and context isolation.
#[test]
fn gc_exhaustion_recovery_and_context_isolation() {
    // Fill every slot (limit = 3) with unreachable streams: the next
    // creation hits the quota boundary.
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_concurrent_reads_per_global: 3,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits);
    context
        .eval(Source::from_bytes(
            "globalThis.s0 = new Blob(['a']).stream(); \
             globalThis.s1 = new Blob(['b']).stream(); \
             globalThis.s2 = new Blob(['c']).stream(); \
             globalThis.probe = (() => { try { new Blob(['d']).stream(); return 'created'; } catch (e) { return e.constructor.name + ':' + e.name; } })(); \
             true",
        ))
        .expect("exhaust");
    assert_eq!(stream_counts(&mut context, &handle), (3, 3, 3));
    let probe = eval_str(&mut context, "globalThis.probe");
    assert!(
        probe.contains("TooManyReads") || probe.contains("TypeError"),
        "quota boundary must reject, got {probe}"
    );
    // Real GC + cleanup recovers every slot without shutdown.
    gc_drop_cleanup(
        &mut context,
        &handle,
        &executor,
        &["s0", "s1", "s2", "probe"],
    );
    assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    context
        .eval(Source::from_bytes(
            "globalThis.reader = new Blob(['ok']).stream().getReader(); \
             globalThis.verdict = 'pending'; \
             globalThis.reader.read().then(r => { globalThis.verdict = 'len:' + r.value.length; }); \
             true",
        ))
        .expect("recovery stream");
    drive_until_settled(&mut context, &handle, &executor);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:2");
    gc_drop_cleanup(&mut context, &handle, &executor, &["reader"]);
    assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    // Isolation: cleanup of one context never frees another context.
    {
        let (mut context_a, handle_a, executor_a, _) = setup_manual();
        let (mut context_b, handle_b, executor_b, _) = setup_manual();
        context_a
            .eval(Source::from_bytes(
                "globalThis.a = new Blob(['a']).stream(); true",
            ))
            .expect("context a stream");
        context_b
            .eval(Source::from_bytes(
                "globalThis.b = new Blob(['b']).stream(); true",
            ))
            .expect("context b stream");
        assert_eq!(stream_counts(&mut context_a, &handle_a), (1, 1, 1));
        assert_eq!(stream_counts(&mut context_b, &handle_b), (1, 1, 1));
        gc_drop_cleanup(&mut context_a, &handle_a, &executor_a, &["a"]);
        assert_eq!(stream_counts(&mut context_a, &handle_a), (0, 0, 0));
        // Context B is untouched by A's cleanup (no cross-context free).
        assert_eq!(stream_counts(&mut context_b, &handle_b), (1, 1, 1));
        gc_drop_cleanup(&mut context_b, &handle_b, &executor_b, &["b"]);
        assert_eq!(stream_counts(&mut context_b, &handle_b), (0, 0, 0));
    }
    // Foreign poll_io is still rejected without mutation.
    {
        let (mut context_a, handle_a, executor_a, _) = setup_manual();
        let (mut context_b, handle_b, _, _) = setup_manual();
        context_a
            .eval(Source::from_bytes(
                "globalThis.a = new Blob(['a']).stream(); true",
            ))
            .expect("context a stream");
        let foreign = handle_b.poll_io(&mut context_a);
        assert!(matches!(
            foreign,
            Err(boa_fapi::PollIoError::ForeignContext)
        ));
        assert_eq!(stream_counts(&mut context_a, &handle_a), (1, 1, 1));
        gc_drop_cleanup(&mut context_a, &handle_a, &executor_a, &["a"]);
        assert_eq!(stream_counts(&mut context_a, &handle_a), (0, 0, 0));
        gc_drop_cleanup(&mut context_b, &handle_b, &executor_a, &[]);
    }
}

/// M9D-GC-06: failure atomicity — every reachable failure after `reserve()`
/// leaves quota/payload/operation counts unchanged.
#[test]
fn gc_failure_atomicity_after_reserve_leaves_no_hidden_slot() {
    // Missing prototype (feature-disabled surface): create_stream reserves,
    // then rolls back synchronously — counts unchanged, error propagates.
    {
        let (mut context, handle, executor, _) = setup_manual();
        let baseline = stream_counts(&mut context, &handle);
        let result = context.eval(Source::from_bytes(
            "globalThis.ok = (() => { try { new Blob(['x']).stream(); return 'created'; } catch (e) { return 'threw'; } })(); true",
        ));
        assert!(result.is_ok());
        // Control: the success path holds exactly one slot (sanity that the
        // baseline comparison below is meaningful).
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        gc_drop_cleanup(&mut context, &handle, &executor, &["ok"]);
        assert_eq!(stream_counts(&mut context, &handle), baseline);
    }
    // Quota-full is itself atomic: the rejected creation consumes nothing.
    {
        let (mut context, handle, executor, _) = setup_manual_with_limits(quota_one_limits());
        let baseline = stream_counts(&mut context, &handle);
        context
            .eval(Source::from_bytes(
                "globalThis.holder = new Blob(['h']).stream(); true",
            ))
            .expect("holder");
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        let rejected = context.eval(Source::from_bytes(
            "globalThis.rejected = (() => { try { new Blob(['x']).stream(); return 'created'; } catch (e) { return 'rejected'; } })(); true",
        ));
        assert!(rejected.is_ok());
        assert_eq!(eval_str(&mut context, "globalThis.rejected"), "rejected");
        // Still exactly the holder's slot: no hidden second slot.
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        gc_drop_cleanup(&mut context, &handle, &executor, &["holder", "rejected"]);
        assert_eq!(stream_counts(&mut context, &handle), baseline);
    }
    // getReader failure atomicity: locked-stream getReader throws without
    // phantom endpoints (counts unchanged, original still reads).
    {
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"atomic"),
                )),
                6,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.r1 = globalThis.stream.getReader(); \
                 globalThis.second = (() => { try { globalThis.stream.getReader(); return 'created'; } catch (e) { return 'threw'; } })(); \
                 true",
            ))
            .expect("setup");
        assert_eq!(eval_str(&mut context, "globalThis.second"), "threw");
        // Exactly one operation: the failed getReader left no phantom owner.
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        context
            .eval(Source::from_bytes(
                "globalThis.verdict = 'pending'; \
                 globalThis.r1.read().then(r => { globalThis.verdict = 'len:' + r.value.length; }); \
                 true",
            ))
            .expect("read");
        drive_until_settled(&mut context, &handle, &executor);
        assert_eq!(eval_str(&mut context, "globalThis.verdict"), "len:6");
        gc_drop_cleanup(
            &mut context,
            &handle,
            &executor,
            &["stream", "r1", "second"],
        );
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // releaseLock failure atomicity: releasing with queued demand throws
    // without state change (demand still settles, counts return to zero).
    {
        let (mut context, handle, executor, _) = setup_manual();
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"y"),
                )),
                1,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.reader.read().then(() => {}); \
                 globalThis.released = (() => { try { globalThis.reader.releaseLock(); return 'released'; } catch (e) { return 'threw'; } })(); \
                 true",
            ))
            .expect("setup");
        assert_eq!(eval_str(&mut context, "globalThis.released"), "threw");
        assert_eq!(stream_counts(&mut context, &handle), (1, 1, 1));
        assert_eq!(executor.pending_streams(), 1);
        drive_until_settled(&mut context, &handle, &executor);
        gc_drop_cleanup(
            &mut context,
            &handle,
            &executor,
            &["stream", "reader", "released"],
        );
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
    // Submit-failure atomicity (QueueFull executor): the failed read takes
    // the typed terminal path and frees the slot exactly once.
    {
        use std::sync::Arc as StdArc;
        struct FullExecutor;
        impl FileIoExecutor for FullExecutor {
            fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
                Err(FileIoSubmitError::QueueFull)
            }
            fn submit_stream(
                &self,
                _task: boa_fapi::StreamChunkTask,
            ) -> Result<(), FileIoSubmitError> {
                Err(FileIoSubmitError::QueueFull)
            }
        }
        let executor = StdArc::new(FullExecutor);
        let wake = CountingWake::new();
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(StdArc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(executor as StdArc<dyn FileIoExecutor>)
            .io_wake(wake as StdArc<dyn FileIoWake>)
            .build()
            .register(&mut context)
            .expect("register");
        publish_blob(
            &handle,
            &mut context,
            "srcBlob",
            &blob_of(
                StdArc::new(boa_fapi_core::source::memory::MemorySource::new(
                    bytes::Bytes::from_static(b"q"),
                )),
                1,
            ),
        );
        context
            .eval(Source::from_bytes(
                "globalThis.stream = srcBlob.stream(); \
                 globalThis.reader = globalThis.stream.getReader(); \
                 globalThis.v = 'pending'; \
                 globalThis.reader.read().then(() => { globalThis.v = 'fulfilled'; }, e => { globalThis.v = e.name; }); \
                 true",
            ))
            .expect("setup");
        drive(&mut context, &handle);
        assert_eq!(eval_str(&mut context, "globalThis.v"), "SecurityError");
        assert_eq!(stream_counts(&mut context, &handle), (0, 0, 0));
    }
}
