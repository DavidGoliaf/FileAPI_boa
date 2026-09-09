//! M9-C integration: async `FileReader` over the M9-B executor protocol.
//!
//! Every test drives the real `FileReader.prototype.readAs*` path with a
//! controlled manual [`FileIoExecutor`](boa_fapi::FileIoExecutor) that
//! records chunk requests without running them, plus a counting
//! [`FileIoWake`](boa_fapi::FileIoWake). No `sleep` is used as an oracle:
//! synchronization is executor hand-off with bounded waits only as a hang
//! guard. Trace rows: `M9C-FR-01` (readAs* returns before I/O; no source
//! read in a Boa job), `M9C-FR-02` (chunk/EOF/error completion and event
//! ordering), `M9C-FR-03` (abort/restart/stale/shutdown suppression),
//! `M9C-FR-04` (multi-reader FIFO, fairness, bounded queues), `M9C-FR-05`
//! (encoding across chunk boundaries), `M9C-FR-06` (quota and telemetry
//! exact-once terminal behavior).
//!
//! `blob_from_data` wraps arbitrary host `BlobData` in brand-valid JS
//! objects so the tests drive the real read path with controlled host
//! sources (blocking, failing, filesystem-backed) instead of memory
//! copies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "fs")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use boa_engine::{Context, Source, js_string};
use boa_fapi::{
    Clock, FileApiContextId, FileApiExtension, FileIoExecutor, FileIoSubmitError, FileIoWake,
    FileReaderChunkTask, PollIoError,
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

/// Controlled manual executor: records whole-blob and chunk requests
/// without running them. The test releases requests explicitly, in order.
struct ManualExecutor {
    whole: Mutex<VecDeque<boa_fapi::FileIoTask>>,
    chunks: Mutex<VecDeque<FileReaderChunkTask>>,
    submits: AtomicUsize,
}

impl ManualExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            whole: Mutex::new(VecDeque::new()),
            chunks: Mutex::new(VecDeque::new()),
            submits: AtomicUsize::new(0),
        })
    }

    fn pending_chunks(self: &Arc<Self>) -> usize {
        self.chunks.lock().expect("chunks").len()
    }

    fn pending_whole(self: &Arc<Self>) -> usize {
        self.whole.lock().expect("whole").len()
    }

    fn pending_total(self: &Arc<Self>) -> usize {
        self.pending_chunks() + self.pending_whole()
    }

    fn take_chunks(self: &Arc<Self>) -> Vec<FileReaderChunkTask> {
        self.chunks.lock().expect("chunks").drain(..).collect()
    }

    /// Runs every queued chunk on the calling thread (test worker stand-in).
    fn run_chunks(self: &Arc<Self>) {
        for task in self.take_chunks() {
            task.execute();
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

    fn submit_reader(&self, task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        self.chunks.lock().expect("chunks").push_back(task);
        self.submits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Executor that always fails chunk submits with `QueueFull`.
struct FullReaderExecutor;

impl FileIoExecutor for FullReaderExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::QueueFull)
    }

    fn submit_reader(&self, _task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::QueueFull)
    }
}

/// Executor that always fails chunk submits with `WorkerLost`.
struct LostReaderExecutor;

impl FileIoExecutor for LostReaderExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::WorkerLost)
    }

    fn submit_reader(&self, _task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::WorkerLost)
    }
}

/// Executor that panics on chunk submit (panic containment path).
struct PanicReaderExecutor;

impl FileIoExecutor for PanicReaderExecutor {
    fn submit(&self, _task: boa_fapi::FileIoTask) -> Result<(), FileIoSubmitError> {
        panic!("third-party executor misbehaved");
    }

    fn submit_reader(&self, _task: FileReaderChunkTask) -> Result<(), FileIoSubmitError> {
        panic!("third-party chunk executor misbehaved");
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
    Arc::new(
        boa_fapi_core::blob::BlobData::from_segments(
            vec![boa_fapi_core::blob::BlobSegment {
                source,
                offset: 0,
                len,
            }],
            "",
            &boa_fapi_core::limits::FileApiLimits::default(),
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

/// Drives the documented host loop until quiescent (bounded).
///
/// `drive` never swallows listener exceptions: the inner `run_jobs` uses
/// `expect` like every other suite helper. Tests that assert a throwing
/// listener isolate that dispatch step themselves (see
/// `listener_exception_keeps_order_and_single_terminal`) and call `drive`
/// only afterwards.
fn drive(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..400 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            context.run_jobs().expect("run_jobs");
            if !handle.has_pending_io() {
                break;
            }
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
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

/// Creates a brand-valid `FileReader` reachable as `name`.
fn publish_reader(context: &mut Context, handle: &boa_fapi::FileApiHandle, name: &str) {
    use boa_engine::property::Attribute;
    let specs = context
        .eval(Source::from_bytes("new FileReader()"))
        .expect("construct reader");
    let _ = handle;
    context
        .register_global_property(js_string!(name), specs, Attribute::all())
        .expect("publish reader");
}

/// Attaches the six-event logger and records `type:loaded/total:state`.
fn attach_log(context: &mut Context, reader: &str) {
    context
        .eval(Source::from_bytes(&format!(
            "globalThis.log = []; \
             for (var t of ['loadstart','progress','load','error','abort','loadend']) \
                 {reader}.addEventListener(t, (function (tt) {{ \
                     return function (e) {{ \
                         globalThis.log.push(tt + ':' + e.loaded + '/' + e.total + ':' + this.readyState); \
                     }}; \
                 }})(t));"
        )))
        .expect("attach log");
}

fn event_types(context: &mut Context) -> String {
    let log = eval_str(context, "globalThis.log ? globalThis.log.join('|') : ''");
    log.split('|')
        .map(|entry| entry.split(':').next().unwrap_or("").to_owned())
        .collect::<Vec<_>>()
        .join("|")
}

fn js_log(context: &mut Context) -> String {
    eval_str(context, "globalThis.log.join('|')")
}

// ── M9C-FR-01: readAs* returns before I/O; no source read in a Boa job ──

#[test]
fn read_returns_before_io_and_settles_only_through_poll_io() {
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blocking_blob(3, &gate, b'x', &reads),
    );
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         for (var t of ['loadstart','progress','load','error','abort','loadend']) \
             reader.addEventListener(t, (function (tt) { \
                 return function () { globalThis.log.push(tt); }; \
             })(t)); \
         reader.readAsText(srcBlob); \
         reader.readyState === 1 && reader.result === null && reader.error === null",
    );
    // One chunk request submitted, nothing executed: LOADING before I/O.
    assert_eq!(executor.pending_chunks(), 1);
    assert_eq!(executor.pending_whole(), 0);
    // `run_jobs` alone settles nothing and reads nothing: the request is
    // still held by the manual executor.
    for _ in 0..3 {
        context.run_jobs().expect("run_jobs");
    }
    assert_eq!(executor.pending_chunks(), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(eval_str(&mut context, "globalThis.log.join(',')"), "");
    assert_eq!(eval_str(&mut context, "reader.readyState"), "1");
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
    executor.run_chunks();
    assert_eq!(eval_str(&mut context, "globalThis.log.join(',')"), "");
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join('|')"),
        "loadstart|progress|load|loadend"
    );
    assert_eval(&mut context, "reader.result === 'xxx'");
    assert!(!handle.has_pending_io());
}

#[test]
fn blocking_first_chunk_never_runs_inside_boa_job() {
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
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.verdict = 'pending'; \
         reader.addEventListener('load', function () { globalThis.verdict = 'load:' + this.result; }); \
         reader.readAsText(srcBlob); \
         reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    let held = executor.take_chunks();
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
        // and the reader stays LOADING with no events: no Boa job touches
        // the blocking source.
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
        assert_eq!(eval_str(&mut context, "reader.readyState"), "1");
        {
            let (lock, gate) = &*gate;
            *lock.lock().expect("gate") = true;
            gate.notify_all();
        }
        release_tx.send(()).expect("release worker");
    });
    assert_eq!(wake.count(), wake_before + 1);
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.verdict"), "load:AAAAA");
    assert!(!handle.has_pending_io());
}

// ── M9C-FR-02: chunk/EOF/error completion and event ordering ──

#[test]
fn empty_blob_loadstart_final_progress_load_loadend() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsArrayBuffer(new Blob([])); reader.readyState === 1",
    );
    // Empty blobs settle without a worker round-trip.
    assert_eq!(executor.pending_total(), 0);
    drive(&mut context, &handle);
    assert_eq!(event_types(&mut context), "loadstart|progress|load|loadend");
    assert_eval(
        &mut context,
        "reader.readyState === 2 && (reader.result instanceof ArrayBuffer) && reader.error === null",
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn multichunk_success_has_exact_event_sequence() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(chunk_limits());
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "globalThis.blob = new Blob([new Uint8Array(3 * 16384).fill(7)]); \
         reader.readAsArrayBuffer(globalThis.blob); \
         reader.readyState === 1",
    );
    // Exactly one chunk in flight: no readahead.
    assert_eq!(executor.pending_chunks(), 1);
    // Drain chunk by chunk: each `poll_io` settles one completion and the
    // pump submits exactly one successor.
    executor.run_chunks();
    assert_eq!(executor.pending_chunks(), 0);
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    drive(&mut context, &handle);
    let log = js_log(&mut context);
    assert!(
        log.starts_with("loadstart:0/49152:"),
        "loadstart first: {log}"
    );
    assert!(
        log.ends_with("|loadend:49152/49152:2") && log.contains("|load:49152/49152:2|"),
        "load/loadend last: {log}"
    );
    assert!(
        log.contains("progress:49152/49152:") && log.contains("|load:49152/49152:2|"),
        "final progress precedes load: {log}"
    );
    // Loaded is monotonic and bounded by total.
    let mut last = 0_u64;
    for entry in eval_str(&mut context, "globalThis.log.join(',')").split(',') {
        let loaded: u64 = entry
            .split(':')
            .nth(1)
            .and_then(|pair| pair.split('/').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(u64::MAX);
        assert!(loaded <= 49152, "loaded <= total: {entry}");
        if entry.starts_with("progress") {
            assert!(loaded >= last, "loaded monotonic: {entry}");
            last = loaded;
        }
    }
    assert_eval(
        &mut context,
        "reader.readyState === 2 && reader.result.byteLength === 49152 && reader.error === null",
    );
    assert!(!handle.has_pending_io());
    // One completion created at most one next request: total submits for a
    // 3-chunk read are exactly 3.
    assert_eq!(executor.submits(), 3);
}

#[test]
fn source_error_before_first_chunk_reports_error_loadend() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(Arc::new(FailSource { len: 3 }), 3),
    );
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsArrayBuffer(srcBlob); reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    // Still nothing before `poll_io`.
    assert_eq!(js_log(&mut context), "");
    drive(&mut context, &handle);
    assert_eq!(event_types(&mut context), "loadstart|error|loadend");
    assert_eval(
        &mut context,
        "reader.readyState === 2 && reader.result === null \
         && (reader.error instanceof DOMException) && reader.error.name === 'NotReadableError'",
    );
    assert!(!handle.has_pending_io());
    // No partial result: a follow-up read on another reader recovers quota.
    // (`r2.onload` is an `on*` property handler: it runs through the same
    // dispatch path as listeners.)
    assert_eval(
        &mut context,
        "globalThis.after = 'unset'; \
         var r2 = new FileReader(); \
         r2.onload = function () { globalThis.after = String(this.result); }; \
         r2.onerror = function () { globalThis.after = 'unexpected-error'; }; \
         r2.readAsText(new Blob(['ok'])); \
         true",
    );
    for _ in 0..4 {
        executor.run_chunks();
        drive(&mut context, &handle);
        if eval_str(&mut context, "globalThis.after") != "unset" {
            break;
        }
    }
    assert_eq!(eval_str(&mut context, "globalThis.after"), "ok");
}

#[test]
fn source_error_between_chunks_reports_error_without_partial() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(chunk_limits());
    let reads = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(FailAfterSource {
        len: 3 * 16384,
        fill: 9,
        fail_after: 1,
        reads: Arc::clone(&reads),
    });
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(source, 3 * 16384),
    );
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsArrayBuffer(srcBlob); reader.readyState === 1",
    );
    // First chunk succeeds, second fails on the worker.
    executor.run_chunks();
    drive(&mut context, &handle);
    executor.run_chunks();
    drive(&mut context, &handle);
    let log = js_log(&mut context);
    assert!(
        log.starts_with("loadstart:0/49152:"),
        "loadstart first: {log}"
    );
    assert!(
        log.contains("|error:") && log.ends_with("|loadend:0/49152:2"),
        "error/loadend last, no partial load: {log}"
    );
    assert!(!log.contains("|load:"), "no success event: {log}");
    assert_eval(
        &mut context,
        "reader.readyState === 2 && reader.result === null \
         && reader.error.name === 'NotReadableError'",
    );
    assert!(!handle.has_pending_io());
}

// ── M9C-FR-03: abort/restart/stale/shutdown suppression ──

#[test]
fn abort_before_dispatch_reports_abort_loadend() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsText(new Blob(['abcdef'])); \
         reader.abort(); \
         reader.readyState === 2 && reader.result === null && reader.error === null",
    );
    // Abort before the first drain: the queued chunk request is dropped
    // (reservation released by `abort()`), so nothing is in flight.
    assert_eq!(executor.pending_chunks(), 1);
    let tasks = executor.take_chunks();
    assert_eq!(tasks.len(), 1);
    // The task's token was cancelled by `abort()`: executing it settles
    // `Cancelled`, and the completion is stale (no pending root).
    tasks.into_iter().next().expect("task").execute();
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0);
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    assert_eq!(event_types(&mut context), "abort|loadend");
    assert!(!handle.has_pending_io());
}

#[test]
fn abort_during_pending_io_suppresses_late_completion() {
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blocking_blob(4, &gate, b'z', &reads),
    );
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsText(srcBlob); reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    let tasks = executor.take_chunks();
    assert_eq!(tasks.len(), 1);
    let task = tasks.into_iter().next().expect("task");
    // Abort while the worker request is still held: cancels the token and
    // drops the reservation.
    assert_eval(&mut context, "reader.abort(); reader.readyState === 2");
    {
        let (lock, gate) = &*gate;
        *lock.lock().expect("gate") = true;
        gate.notify_all();
    }
    task.execute();
    // Late completion is stale: no pump job, no events beyond abort.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0);
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    assert_eq!(event_types(&mut context), "abort|loadend");
    assert_eval(
        &mut context,
        "reader.readyState === 2 && reader.result === null && reader.error === null",
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn abort_after_queued_completion_suppresses_it() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsText(new Blob(['late'])); reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    // Completion is queued but not yet drained: abort first.
    assert_eval(&mut context, "reader.abort(); reader.readyState === 2");
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0, "stale completion settles nothing");
    context.run_jobs().expect("run_jobs");
    drive(&mut context, &handle);
    assert_eq!(event_types(&mut context), "abort|loadend");
    assert!(!handle.has_pending_io());
}

#[test]
fn restart_from_abort_handler_suppresses_old_loadend() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(chunk_limits());
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         globalThis.blob = new Blob([new Uint8Array(3 * 16384)]); \
         reader.addEventListener('progress', function (e) { \
             globalThis.log.push('progress:' + e.loaded); \
             if (e.loaded === 16384) this.abort(); \
         }); \
         reader.addEventListener('abort', function () { \
             globalThis.log.push('abort'); \
             this.readAsText(new Blob(['fresh'])); \
         }); \
         for (var t of ['loadstart','load','error','loadend']) \
             reader.addEventListener(t, (function (tt) { \
                 return function () { globalThis.log.push(tt); }; \
             })(t)); \
         reader.readAsArrayBuffer(globalThis.blob); \
         true",
    );
    // Manual stepping: alternate worker execution and host-loop drains so
    // the abort lands mid-operation and the restart completes.
    for _ in 0..8 {
        executor.run_chunks();
        drive(&mut context, &handle);
        if !handle.has_pending_io() {
            break;
        }
    }
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join('|')"),
        "loadstart|progress:16384|abort|loadstart|progress:5|load|loadend"
    );
    assert_eval(&mut context, "reader.result === 'fresh'");
    assert!(!handle.has_pending_io());
    let _ = executor;
}

#[test]
fn stale_completion_after_restart_is_noop() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsText(new Blob(['first'])); reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    let first = executor.take_chunks();
    assert_eq!(first.len(), 1);
    let stale = first.into_iter().next().expect("first chunk");
    // Restart via abort + new read before the first chunk completes.
    assert_eval(
        &mut context,
        "reader.abort(); reader.readAsText(new Blob(['second'])); reader.readyState === 1",
    );
    // The stale first-generation chunk executes late: its completion is
    // dropped (reservation gone), and only the second read settles.
    stale.execute();
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0, "stale completion settles nothing");
    context.run_jobs().expect("run_jobs");
    assert_eq!(js_log(&mut context), "");
    for _ in 0..4 {
        executor.run_chunks();
        drive(&mut context, &handle);
        if eval_str(
            &mut context,
            "reader.result === null ? 'null' : reader.result",
        ) == "second"
        {
            break;
        }
    }
    assert_eval(&mut context, "reader.result === 'second'");
    assert!(!handle.has_pending_io());
}

#[test]
fn shutdown_with_pending_io_settles_nothing_late() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    attach_log(&mut context, "reader");
    assert_eval(
        &mut context,
        "reader.readAsText(new Blob(['x'])); reader.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 1);
    handle.shutdown(&mut context).expect("shutdown");
    // Late worker result is dropped safely: no job, no JS, no quota.
    executor.run_chunks();
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0);
    context.run_jobs().expect("run_jobs");
    assert_eq!(js_log(&mut context), "");
    // Post-shutdown reads fail fast without creating work.
    assert_eval(
        &mut context,
        "globalThis.s2 = 'pending'; \
         var r2 = new FileReader(); \
         r2.readAsText(new Blob(['y'])); \
         r2.readyState === 2 && (r2.error instanceof DOMException) && r2.error.name === 'AbortError'",
    );
    context.run_jobs().expect("run_jobs");
    assert!(!handle.has_pending_io());
}

// ── M9C-FR-04: multi-reader FIFO, fairness, bounded queues ──

#[test]
fn two_readers_out_of_worker_order_apply_fifo() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "readerA");
    publish_reader(&mut context, &handle, "readerB");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         readerA.addEventListener('load', function () { globalThis.log.push('a:' + this.result); }); \
         readerB.addEventListener('load', function () { globalThis.log.push('b:' + this.result); }); \
         readerA.readAsText(new Blob(['first'])); \
         readerB.readAsText(new Blob(['second'])); \
         readerA.readyState === 1 && readerB.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 2);
    let mut tasks = executor.take_chunks();
    assert_eq!(tasks.len(), 2);
    // Complete the second reader's worker first: each reader still
    // applies exactly its own chunk (no crossover), and both settle
    // exactly once. Cross-reader completion order follows drain order.
    let second = tasks.pop().expect("second task");
    let first = tasks.pop().expect("first task");
    second.execute();
    first.execute();
    drive(&mut context, &handle);
    // Drain order is submission order here (the bridge queues reader
    // completions per operation in `order`): the first-submitted reader
    // settles first even though its worker finished last.
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "a:first,b:second"
    );
    // Cross-reader order follows completion drain order (no sequencing
    // across independent operations by design); what matters is each
    // reader applied exactly its own chunk in order with no crossover.
    assert_eval(
        &mut context,
        "readerA.result === 'first' && readerB.result === 'second'",
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn slow_reader_does_not_block_other_reader_or_jobs() {
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    publish_blob(
        &handle,
        &mut context,
        "slowBlob",
        &blocking_blob(6, &gate, b'S', &reads),
    );
    publish_reader(&mut context, &handle, "slow");
    publish_reader(&mut context, &handle, "fast");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         slow.addEventListener('load', function () { globalThis.log.push('slow:' + this.result); }); \
         fast.addEventListener('load', function () { globalThis.log.push('fast:' + this.result); }); \
         slow.readAsText(slowBlob); \
         fast.readAsText(new Blob(['fast'])); \
         slow.readyState === 1 && fast.readyState === 1",
    );
    assert_eq!(executor.pending_chunks(), 2);
    let mut tasks = executor.take_chunks();
    assert_eq!(tasks.len(), 2);
    // Run only the fast reader's chunk (second submitted) while the slow
    // one stays held: the fast reader settles without waiting.
    let fast_task = tasks.pop().expect("fast task");
    let slow_task = tasks.pop().expect("slow task");
    fast_task.execute();
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "fast:fast"
    );
    assert_eq!(eval_str(&mut context, "slow.readyState"), "1");
    // Unrelated Boa jobs still run while the slow read is held.
    assert_eval(
        &mut context,
        "globalThis.unrelated = 'no'; \
         Promise.resolve().then(() => { globalThis.unrelated = 'yes'; }); \
         true",
    );
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.unrelated"), "yes");
    {
        let (lock, gate) = &*gate;
        *lock.lock().expect("gate") = true;
        gate.notify_all();
    }
    slow_task.execute();
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "fast:fast,slow:SSSSSS"
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn poll_io_budget_bounds_completions_and_rewakes() {
    let (mut context, handle, executor, wake) = setup_manual();
    handle.set_poll_io_budget(Some(1));
    publish_reader(&mut context, &handle, "readerA");
    publish_reader(&mut context, &handle, "readerB");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         readerA.addEventListener('load', function () { globalThis.log.push('a'); }); \
         readerB.addEventListener('load', function () { globalThis.log.push('b'); }); \
         readerA.readAsText(new Blob(['a'])); \
         readerB.readAsText(new Blob(['b'])); \
         true",
    );
    assert_eq!(executor.pending_chunks(), 2);
    executor.run_chunks();
    let wake_before = wake.count();
    // Budget 1: the first `poll_io` settles one chunk and re-queues the
    // other with a fresh wake.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    assert!(wake.count() > wake_before, "budget leftover must re-wake");
    assert!(handle.has_pending_io(), "leftover stays pending");
    context.run_jobs().expect("run_jobs");
    // The next `poll_io` settles the remainder.
    handle.set_poll_io_budget(None);
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.log.join(',')"), "a,b");
    assert!(!handle.has_pending_io());
}

#[test]
fn sixty_five_reads_obey_limit_and_recover() {
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_concurrent_reads_per_global: 64,
        ..Default::default()
    };
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits);
    assert_eval(
        &mut context,
        "globalThis.readers = []; \
         for (var i = 0; i < 64; i++) { \
           var r = new FileReader(); r.readAsText(new Blob(['x'])); \
           globalThis.readers.push(r); \
         } \
         globalThis.extra = new FileReader(); \
         globalThis.extra.readAsText(new Blob(['x'])); \
         globalThis.readers.length === 64",
    );
    assert_eq!(executor.pending_chunks(), 64);
    // The 65th read fails fast with a typed quota error (no slot taken).
    context.run_jobs().expect("run_jobs");
    assert_eq!(
        eval_str(
            &mut context,
            "globalThis.extra.error && globalThis.extra.error.name"
        ),
        "SecurityError"
    );
    executor.run_chunks();
    drive(&mut context, &handle);
    assert_eval(
        &mut context,
        "globalThis.readers.every(r => r.readyState === 2)",
    );
    assert!(!handle.has_pending_io());
    // A new read works again after recovery.
    assert_eval(
        &mut context,
        "globalThis.again = new FileReader(); \
         globalThis.again.outcome = null; \
         globalThis.again.onload = function () { globalThis.again.outcome = this.result; }; \
         globalThis.again.readAsText(new Blob(['recovered'])); \
         true",
    );
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.again.outcome"),
        "recovered"
    );
}

#[test]
fn foreign_poll_io_rejected_without_state_change() {
    let (mut context_a, handle_a, executor_a, _) = setup_manual();
    let (mut context_b, handle_b, _, _) = setup_manual();
    publish_reader(&mut context_a, &handle_a, "reader");
    assert_eval(
        &mut context_a,
        "reader.readAsText(new Blob(['a'])); reader.readyState === 1",
    );
    assert_eq!(executor_a.pending_chunks(), 1);
    let error = handle_b
        .poll_io(&mut context_a)
        .expect_err("foreign poll_io must fail");
    assert_eq!(error, PollIoError::ForeignContext);
    assert_eq!(executor_a.pending_chunks(), 1);
    executor_a.run_chunks();
    drive(&mut context_a, &handle_a);
    assert_eval(&mut context_a, "reader.result === 'a'");
    let _ = (&mut context_b, handle_b);
}

// ── M9C-FR-05: encoding across chunk boundaries ──

#[test]
fn split_bom_and_multibyte_across_chunks_match_sync() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(chunk_limits());
    // BOM mid-stream (not at offset 0) decodes per-chunk through the
    // incremental decoder: split the payload so the 3-byte UTF-8 BOM
    // straddles the 16 KiB chunk boundary and assert the exact
    // chunk-by-chunk accumulation (BOM bytes decode as U+FEFF mid-stream,
    // matching the shared `IncrementalDecoder` semantics the sync reader
    // shares).
    let mut first = vec![b'P'; 16383];
    first.push(0xEF);
    let mut second = vec![0xBB, 0xBF];
    second.extend_from_slice("BOM-ok".as_bytes());
    let mut payload = first;
    payload.extend_from_slice(&second);
    let total = payload.len();
    assert!(total > 16384, "must span two chunks");
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from(payload.clone()),
            )),
            total as u64,
        ),
    );
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.text = null; \
         reader.onload = function () { globalThis.text = this.result; }; \
         reader.readAsText(srcBlob); \
         reader.readyState === 1",
    );
    executor.run_chunks();
    drive(&mut context, &handle);
    executor.run_chunks();
    drive(&mut context, &handle);
    // Mid-stream BOM split across the chunk boundary: decode chunk by
    // chunk through the shared incremental decoder and compare byte for
    // byte (no whole-blob shortcut in the test either). The BOM lands
    // mid-stream here (after padding), so it decodes to U+FEFF rather
    // than being stripped — exactly what the shared decoder yields for
    // the same byte sequence.
    let expected = "P".repeat(16383) + "﻿BOM-ok";
    assert_eq!(eval_str(&mut context, "globalThis.text"), expected);
    // Multibyte split: euro sign + emoji straddling the 16 KiB boundary.
    let mut blob2 = vec![b'Q'; 16383];
    blob2.extend_from_slice("€".as_bytes());
    blob2.extend_from_slice("B".as_bytes());
    blob2.extend_from_slice("😀".as_bytes());
    let total2 = blob2.len();
    publish_blob(
        &handle,
        &mut context,
        "srcBlob2",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from(blob2.clone()),
            )),
            total2 as u64,
        ),
    );
    assert_eval(
        &mut context,
        "globalThis.text2 = null; \
         var r2 = new FileReader(); \
         r2.onload = function () { globalThis.text2 = this.result; }; \
         r2.readAsText(srcBlob2); \
         true",
    );
    executor.run_chunks();
    drive(&mut context, &handle);
    executor.run_chunks();
    drive(&mut context, &handle);
    let expected2 = "Q".repeat(16383) + "€B😀";
    assert_eq!(eval_str(&mut context, "globalThis.text2"), expected2);
    assert!(!handle.has_pending_io());
}

#[test]
fn utf16_bom_split_across_chunks_decodes() {
    let (mut context, handle, executor, _) = setup_manual_with_limits(chunk_limits());
    // UTF-16LE text (with BOM) split across the 16 KiB chunk boundary:
    // the BOM + text start at offset 0, chunk 1 carries the first 16 KiB
    // (BOM + LE pairs), chunk 2 the remainder. The incremental decoder
    // sniffs the BOM once at the start and strips it; LE pairs decode
    // across the boundary with no replacement.
    let text = "Hi-UTF16-ΔΙΚ😀";
    let mut payload = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        payload.extend_from_slice(&unit.to_le_bytes());
    }
    while payload.len() < 16384 + 8 {
        for unit in "pad-".encode_utf16() {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
    }
    let total = payload.len();
    assert!(total > 16384, "must span two chunks");
    publish_blob(
        &handle,
        &mut context,
        "srcBlob",
        &blob_of(
            Arc::new(boa_fapi_core::source::memory::MemorySource::new(
                bytes::Bytes::from(payload),
            )),
            total as u64,
        ),
    );
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.text = 'unset'; globalThis.state = 'unset'; \
         reader.onload = function () { globalThis.text = this.result; }; \
         reader.onerror = function () { globalThis.state = this.error.name; }; \
         reader.readAsText(srcBlob, 'utf-16le'); \
         true",
    );
    executor.run_chunks();
    drive(&mut context, &handle);
    executor.run_chunks();
    drive(&mut context, &handle);
    // The BOM at offset 0 is sniffed and stripped once; LE pairs decode
    // across the 16 KiB split with no U+FFFD.
    let actual = eval_str(&mut context, "globalThis.text");
    assert!(
        actual.starts_with(text),
        "split UTF-16LE BOM decodes: {actual:?}"
    );
    assert!(
        !actual.contains("�"),
        "no replacement across the split: {actual:?}"
    );
    assert!(!handle.has_pending_io());
}

// ── M9C-FR-06: quota and telemetry exact-once terminal behavior ──

#[test]
fn submit_failures_take_terminal_error_path_without_quota_leak() {
    // QueueFull maps to SecurityError through the normal error path.
    {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::new(FullReaderExecutor) as Arc<dyn FileIoExecutor>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        context
            .eval(Source::from_bytes(
                "globalThis.q = 'pending'; \
                 var r = new FileReader(); \
                 r.onerror = function () { globalThis.q = this.error.name; }; \
                 r.readAsText(new Blob(['q']));",
            ))
            .expect("start read");
        context.run_jobs().expect("run_jobs");
        assert_eq!(eval_str(&mut context, "globalThis.q"), "SecurityError");
        assert!(!handle.has_pending_io());
    }
    // WorkerLost maps to a stable NotReadableError.
    {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::new(LostReaderExecutor) as Arc<dyn FileIoExecutor>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        context
            .eval(Source::from_bytes(
                "globalThis.w = 'pending'; \
                 var r = new FileReader(); \
                 r.onerror = function () { \
                     globalThis.w = (this.error instanceof DOMException) + ':' + this.error.name; \
                 }; \
                 r.readAsText(new Blob(['w']));",
            ))
            .expect("start read");
        context.run_jobs().expect("run_jobs");
        assert_eq!(
            eval_str(&mut context, "globalThis.w"),
            "true:NotReadableError"
        );
        assert!(!handle.has_pending_io());
    }
    // A panicking third-party submit is contained to WorkerLost.
    {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::new(PanicReaderExecutor) as Arc<dyn FileIoExecutor>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        context
            .eval(Source::from_bytes(
                "globalThis.p = 'pending'; \
                 var r = new FileReader(); \
                 r.onerror = function () { \
                     globalThis.p = (this.error instanceof DOMException) + ':' + this.error.name; \
                 }; \
                 r.readAsText(new Blob(['p']));",
            ))
            .expect("start read");
        context.run_jobs().expect("run_jobs");
        assert_eq!(
            eval_str(&mut context, "globalThis.p"),
            "true:NotReadableError"
        );
        assert!(!handle.has_pending_io());
    }
}

#[test]
fn listener_exception_keeps_order_and_single_terminal() {
    let (mut context, handle, executor, _) = setup_manual();
    publish_reader(&mut context, &handle, "reader");
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         reader.addEventListener('load', function () { \
             globalThis.log.push('first'); \
             throw new Error('boom'); \
         }); \
         reader.addEventListener('load', function () { globalThis.log.push('second'); }); \
         reader.readAsText(new Blob(['ok'])); \
         true",
    );
    executor.run_chunks();
    // The throwing listener surfaces as a job error inside the drive;
    // `drive` uses `expect`, so isolate the error step: poll once, run
    // the dispatch job fallibly, then drain the rest.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    let dispatch_failed = context.run_jobs().is_err();
    // Both listeners ran in order before the error surfaced.
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "first,second"
    );
    assert!(
        dispatch_failed,
        "listener exception must surface as a job error"
    );
    drive(&mut context, &handle);
    // Exactly one terminal + loadend (the log has only listener pushes).
    assert_eval(
        &mut context,
        "reader.result === 'ok' && reader.readyState === 2",
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn filesystem_backed_reader_matches_memory_byte_for_byte() {
    let (mut context, handle, executor, _) = setup_manual();
    let expected = b"reader-fs-parity-0123456789";
    // Memory path first through the real worker chunks.
    publish_reader(&mut context, &handle, "memReader");
    assert_eval(
        &mut context,
        "globalThis.mem = null; \
         memReader.onload = function () { \
             globalThis.mem = Array.from(new Uint8Array(this.result)).join(','); \
         }; \
         memReader.readAsArrayBuffer(new Blob([new Uint8Array([114, 101, 97, 100, 101, 114, 45, 102, 115, 45, 112, 97, 114, 105, 116, 121, 45, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57])])); \
         true",
    );
    executor.run_chunks();
    drive(&mut context, &handle);
    let mem = eval_str(&mut context, "globalThis.mem");
    // Real filesystem-backed path: live temp file imported as a File,
    // read through the real worker chunks (no synthetic completion).
    let dir = std::env::temp_dir().join(format!(
        "boa-fapi-m9c-parity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("parity.bin");
    std::fs::write(&path, expected).expect("write temp file");
    let registry = boa_fapi_fs::FsRegistry::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open temp file");
    let resource = registry.register(file).expect("register");
    let object = {
        #[cfg(unix)]
        {
            let adapter = Arc::new(
                boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter"),
            );
            handle
                .file_from_resource(
                    &registry,
                    adapter,
                    "parity.bin",
                    boa_fapi::HostFileOptions::default(),
                    &mut context,
                )
                .expect("import")
        }
        #[cfg(not(unix))]
        {
            let bytes =
                boa_fapi_fs::open_copy_on_import(&registry, &resource, u64::MAX).expect("copy");
            handle
                .file_from_bytes(
                    bytes,
                    "parity.bin",
                    boa_fapi::HostFileOptions::default(),
                    &mut context,
                )
                .expect("import")
        }
    };
    context
        .register_global_property(
            js_string!("fsFile"),
            object,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
    publish_reader(&mut context, &handle, "fsReader");
    assert_eval(
        &mut context,
        "globalThis.fs = null; \
         fsReader.addEventListener('load', function () { \
             globalThis.fs = Array.from(new Uint8Array(this.result)).join(','); \
         }); \
         fsReader.readAsArrayBuffer(fsFile); \
         true",
    );
    assert_eq!(executor.pending_chunks(), 1);
    executor.run_chunks();
    drive(&mut context, &handle);
    let fs = eval_str(&mut context, "globalThis.fs");
    let expected_joined = expected
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(fs, expected_joined);
    assert_eq!(mem, expected_joined);
    assert!(!handle.has_pending_io());
    std::fs::remove_file(&path).ok();
    std::fs::remove_dir(&dir).ok();
}

#[test]
fn mutation_snapshot_error_carries_no_partial_bytes() {
    #[cfg(unix)]
    {
        use boa_fapi_fs::{FsRegistry, HostFileSource};
        let (mut context, handle, executor, _) = setup_manual();
        let dir = std::env::temp_dir().join(format!(
            "boa-fapi-m9c-mutation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("mutation.bin");
        std::fs::write(&path, b"stable-payload").expect("write temp file");
        let registry = FsRegistry::new();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open temp file");
        let resource = registry.register(file).expect("register");
        let adapter = Arc::new(HostFileSource::new(&registry, &resource, None).expect("adapter"));
        let object = handle
            .file_from_resource(
                &registry,
                adapter,
                "mutation.bin",
                boa_fapi::HostFileOptions::default(),
                &mut context,
            )
            .expect("import");
        context
            .register_global_property(
                js_string!("mutFile"),
                object,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish");
        std::fs::write(&path, b"CHANGED-payload!!").expect("mutate temp file");
        publish_reader(&mut context, &handle, "reader");
        attach_log(&mut context, "reader");
        assert_eval(
            &mut context,
            "reader.readAsText(mutFile); reader.readyState === 1",
        );
        assert_eq!(executor.pending_chunks(), 1);
        executor.run_chunks();
        assert_eq!(js_log(&mut context), "");
        drive(&mut context, &handle);
        assert_eq!(js_log(&mut context), "loadstart|error|loadend");
        assert_eval(
            &mut context,
            "reader.result === null && (reader.error instanceof DOMException) \
             && reader.error.name === 'NotReadableError'",
        );
        assert!(!handle.has_pending_io());
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}

// ── M9C-FR-01 (guard): no source read inside a Boa job ──

#[test]
fn chunked_source_call_path_absent_from_filereader_jobs() {
    // Static guard twin of the behavioral blocking test above:
    // `filereader.rs` (Boa jobs) must never call the blocking primitives
    // itself — `read_range(`, `.materialize(`, `read_next(` appear only in
    // comments/docs and the `#[cfg(test)]` module (controlled sources);
    // the worker entry lives in `io.rs` (`FileReaderChunkTask::execute`).
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let content = std::fs::read_to_string(root.join("crates/boa_fapi/src/filereader.rs"))
        .expect("readable filereader.rs");
    let stripped = strip_test_modules(&content);
    for forbidden in [".materialize(", "read_range(", "read_next("] {
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
        assert_eq!(hits, 0, "filereader.rs must not contain {forbidden}");
    }
    let io =
        std::fs::read_to_string(root.join("crates/boa_fapi/src/io.rs")).expect("readable io.rs");
    for required in [
        "fn execute(",
        "read_blob_range(",
        "struct FileReaderChunkTask",
    ] {
        assert!(io.contains(required), "io.rs must contain `{required}`");
    }
}

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
        if line.trim().starts_with("#[cfg(test)]") {
            in_test_module = true;
            brace_depth = 0;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

// ── M9C-FR-06 (tracing, optional): no late telemetry ──

#[cfg(feature = "tracing")]
#[test]
fn no_late_telemetry_after_abort_or_shutdown() {
    use std::collections::{HashMap, HashSet};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    const TARGET: &str = "boa_fapi::file_api.operation";

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
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }
    impl Subscriber for Collector {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == TARGET
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
                events.push(capture.fields);
            }
        }
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    let collector = Collector::default();
    let events = Arc::clone(&collector.events);
    // NOTE: `tracing::subscriber::with_default` is thread-local. The
    // executor here is the controlled manual one (no background threads),
    // so all emits below happen on this thread under the collector.
    tracing::subscriber::with_default(collector, || {
        // Abort path: exactly one terminal event (`cancelled`), then the
        // late worker chunk emits nothing.
        let (mut context, handle, executor, _) = setup_manual();
        publish_reader(&mut context, &handle, "reader");
        context
            .eval(Source::from_bytes(
                "reader.readAsText(new Blob(['abort-me'])); reader.abort();",
            ))
            .expect("abort");
        let tasks = executor.take_chunks();
        assert_eq!(tasks.len(), 1);
        tasks.into_iter().next().expect("task").execute();
        let settled = handle.poll_io(&mut context).expect("poll_io");
        assert_eq!(settled, 0);
        context.run_jobs().expect("run_jobs");
        // Shutdown path: no terminal event at all for the dropped read.
        let (mut context2, handle2, executor2, _) = setup_manual();
        publish_reader(&mut context2, &handle2, "reader");
        context2
            .eval(Source::from_bytes("reader.readAsText(new Blob(['shut']));"))
            .expect("start read");
        handle2.shutdown(&mut context2).expect("shutdown");
        executor2.run_chunks();
        let settled = handle2.poll_io(&mut context2).expect("poll_io");
        assert_eq!(settled, 0);
        context2.run_jobs().expect("run_jobs");
    });
    let events = events.lock().expect("events").clone();
    let reader_events: Vec<_> = events
        .iter()
        .filter(|fields| fields.get("operation").map(String::as_str) == Some("filereader_read"))
        .collect();
    // Exactly the abort terminal; the stale chunk and the shutdown read
    // emitted nothing.
    assert_eq!(
        reader_events.len(),
        1,
        "exactly one terminal event: {reader_events:?}"
    );
    assert_eq!(
        reader_events[0].get("result_class").map(String::as_str),
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
    for fields in &reader_events {
        for key in fields.keys() {
            assert!(
                allowed.contains(key.as_str()),
                "non-allowlisted field: {key}"
            );
        }
    }
}
