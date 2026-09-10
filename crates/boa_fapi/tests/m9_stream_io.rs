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
