//! M9-B integration: explicit file I/O executor + Promise Blob reads.
//!
//! Every test uses a controlled manual [`FileIoExecutor`](boa_fapi::FileIoExecutor)
//! that records requests without running them, plus a counting
//! [`FileIoWake`](boa_fapi::FileIoWake). No `sleep` is used as an oracle:
//! synchronization is channel/barrier/manual-executor hand-off with bounded
//! waits only as a hang guard. Trace rows: `M9B-IO-01` (Send-only task and
//! completion DTO), `M9B-IO-02` (context-bound bounded queue + wake),
//! `M9B-IO-03` (promise returns before filesystem I/O, settle only through
//! a Boa job), `M9B-IO-04` (cancel/shutdown/stale/quota exact-once),
//! `M9B-HOST-01` (host `poll_io`/`run_jobs` loop).
//!
//! `blob_from_data`/`file_from_data` wrap arbitrary host `BlobData` in
//! brand-valid JS objects so the tests
//! drive the real `Blob.prototype.*` path with controlled host sources
//! (blocking, panicking, filesystem-backed) instead of memory copies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "fs")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use boa_engine::{Context, Source, js_string};
use boa_fapi::{
    Clock, FileApiContextId, FileApiExtension, FileIoCompletion, FileIoExecutor, FileIoOperationId,
    FileIoSubmitError, FileIoTask, FileIoWake, PollIoError,
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

/// Controlled manual executor: `submit` records the request without running
/// it. The test releases requests explicitly (FIFO), optionally holding
/// them to prove the Boa thread stays usable.
struct ManualExecutor {
    queue: Mutex<VecDeque<FileIoTask>>,
    submits: AtomicUsize,
    max_observed: AtomicUsize,
}

impl ManualExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            submits: AtomicUsize::new(0),
            max_observed: AtomicUsize::new(0),
        })
    }

    fn pending(self: &Arc<Self>) -> usize {
        self.queue.lock().expect("queue").len()
    }

    fn take_all(self: &Arc<Self>) -> Vec<FileIoTask> {
        let mut queue = self.queue.lock().expect("queue");
        queue.drain(..).collect()
    }

    /// Runs every queued task on the calling thread (test worker stand-in).
    fn run_all(self: &Arc<Self>) {
        for task in self.take_all() {
            task.execute();
        }
    }
}

impl FileIoExecutor for ManualExecutor {
    fn submit(&self, task: FileIoTask) -> Result<(), FileIoSubmitError> {
        let mut queue = self.queue.lock().expect("queue");
        queue.push_back(task);
        let len = queue.len();
        drop(queue);
        self.submits.fetch_add(1, Ordering::SeqCst);
        let mut observed = self.max_observed.load(Ordering::SeqCst);
        while len > observed {
            match self.max_observed.compare_exchange(
                observed,
                len,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
        Ok(())
    }
}

/// Executor that always fails with `QueueFull` (typed resource error path).
struct FullExecutor;

impl FileIoExecutor for FullExecutor {
    fn submit(&self, _task: FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::QueueFull)
    }
}

/// Executor that always fails with `WorkerLost` (stable `NotReadableError`).
struct LostExecutor;

impl FileIoExecutor for LostExecutor {
    fn submit(&self, _task: FileIoTask) -> Result<(), FileIoSubmitError> {
        Err(FileIoSubmitError::WorkerLost)
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

/// Controlled blocking source: a host `ByteSource` whose `read_range`
/// blocks on a test gate. Used below to prove by behaviour (not text
/// search) that no Boa job performs the blocking read: the task is
/// executed on the test thread only after `run_jobs`-alone is observed to
/// leave the promise pending.
struct BlockingSource {
    len: u64,
    gate: Arc<(Mutex<bool>, Condvar)>,
    fill: u8,
}

impl BlockingSource {
    fn with_byte(len: u64, gate: &Arc<(Mutex<bool>, Condvar)>, fill: u8) -> Self {
        Self {
            len,
            gate: Arc::clone(gate),
            fill,
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
        let len =
            usize::try_from(range.end - range.start).map_err(|_| FileApiError::InvalidRange)?;
        Ok(bytes::Bytes::from(vec![self.fill; len]))
    }
}

fn blocking_blob(
    len: u64,
    gate: &Arc<(Mutex<bool>, Condvar)>,
) -> Arc<boa_fapi_core::blob::BlobData> {
    blocking_blob_with_byte(len, gate, 7)
}

/// Blocking blob variant with a configurable fill byte, so File/Blob and
/// parity tests can distinguish payloads without a second source type.
fn blocking_blob_with_byte(
    len: u64,
    gate: &Arc<(Mutex<bool>, Condvar)>,
    fill: u8,
) -> Arc<boa_fapi_core::blob::BlobData> {
    let source: Arc<dyn boa_fapi_core::source::ByteSource> =
        Arc::new(BlockingSource::with_byte(len, gate, fill));
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

fn eval_str(context: &mut Context, source: &str) -> String {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    if let Some(string) = value.as_string() {
        return string.to_std_string_escaped();
    }
    // Numbers and booleans: stringify through JS for stable assertions.
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
/// `poll_io` is strictly non-blocking: with the default threaded executor
/// a completion may land just after a pass observed an empty queue, so the
/// loop also yields briefly (bounded, hang-guard only) while I/O is still
/// outstanding before giving up on a pass. Controlled manual executors
/// never complete on their own, so the yield never masks the
/// pending-before-`poll_io` contract there.
fn drive(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..200 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            break;
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while handle.has_pending_io() {
                let _ = handle.poll_io(context);
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

/// Publishes a brand-valid `Blob` object over an arbitrary host payload.
///
/// Uses the test-only `blob_from_data` constructor: no copy, no
/// materialization, so blocking/panicking/filesystem sources reach the
/// real `Blob.prototype.*` path intact.
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

/// Publishes a brand-valid `File` object over an arbitrary host payload.
fn publish_file(
    handle: &boa_fapi::FileApiHandle,
    context: &mut Context,
    name: &str,
    data: &Arc<boa_fapi_core::blob::BlobData>,
    file_name: &str,
) {
    let object = handle.file_from_data(Arc::clone(data), file_name, None);
    context
        .register_global_property(
            js_string!(name),
            object,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
}

/// Publishes a brand-valid `FileList` of the named globals.
fn publish_file_list(
    context: &mut Context,
    handle: &boa_fapi::FileApiHandle,
    name: &str,
    files: &[&str],
) {
    use boa_engine::property::PropertyKey;
    let mut objects = Vec::new();
    for file_name in files {
        let value = context
            .eval(Source::from_bytes(*file_name))
            .unwrap_or_else(|error| panic!("eval failed for {file_name}: {error}"));
        objects.push(
            value
                .as_object()
                .unwrap_or_else(|| panic!("{file_name} is not an object"))
                .clone(),
        );
    }
    let list = handle.file_list(objects, context).expect("file list");
    context
        .register_global_property(
            js_string!(name),
            list,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
    let _ = PropertyKey::from(0_u32);
}

// ── M9B-IO-01: Send-only task and completion DTO ──

#[test]
fn io_task_and_completion_are_send_static_without_js() {
    fn assert_send<T: Send + 'static>() {}
    assert_send::<FileIoTask>();
    assert_send::<FileIoCompletion>();
    assert_send::<FileIoOperationId>();
    assert_send::<FileApiContextId>();
}

// ── M9B-IO-03: promise returns before filesystem I/O ──

#[test]
fn pending_promise_before_blocking_io_boa_thread_stays_usable() {
    let (mut context, handle, executor, _wake) = setup_manual();
    // The manual executor holds every request until the test releases it:
    // this is the controlled blocking source. The promise must return
    // pending before any worker runs, and the Boa thread must stay usable
    // for unrelated work while the request is held.
    assert_eval(
        &mut context,
        "globalThis.order = []; \
         globalThis.p = new Blob(['held-bytes']).text(); \
         globalThis.p.then(v => globalThis.order.push('settled:' + v)); \
         globalThis.order.push('sync'); \
         (globalThis.order.join(',') === 'sync') && (globalThis.p instanceof Promise)",
    );
    // One request was submitted and not executed: the promise is pending
    // and the Boa thread is usable for unrelated work.
    assert_eq!(executor.pending(), 1);
    assert_eval(
        &mut context,
        "globalThis.unrelated = 1 + 1; globalThis.unrelated === 2",
    );
    // An unrelated Boa job runs while I/O is held by the manual executor.
    assert_eval(
        &mut context,
        "globalThis.jobRan = 'no'; \
         Promise.resolve().then(() => { globalThis.jobRan = 'yes'; }); \
         true",
    );
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.jobRan"), "yes");
    assert_eq!(eval_str(&mut context, "globalThis.order.join(',')"), "sync");
    // Completion before `poll_io` executes no JS.
    executor.run_all();
    // The worker pushed a completion and woke the host, but no JS ran yet.
    assert_eq!(eval_str(&mut context, "globalThis.order.join(',')"), "sync");
    // `poll_io` + `run_jobs` settles through a Boa job.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    context.run_jobs().expect("run_jobs");
    assert_eval(
        &mut context,
        "globalThis.order.join(',') === 'sync,settled:held-bytes'",
    );
    assert!(!handle.has_pending_io());
}

// ── M9B-IO-03: File and FileList reads follow the same off-thread path ──

#[test]
fn file_and_file_list_reads_use_off_thread_path() {
    // `File` inherits `Blob.prototype.*`: publish brand-valid File objects
    // over controllable blocking payloads and prove each read submits one
    // held task, stays pending through `run_jobs` alone, and settles only
    // through `poll_io` + `run_jobs`.
    let (mut context, handle, executor, _) = setup_manual();
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    publish_file(
        &handle,
        &mut context,
        "fileA",
        &blocking_blob_with_byte(5, &gate, b'A'),
        "a.txt",
    );
    publish_file(
        &handle,
        &mut context,
        "fileB",
        &blocking_blob_with_byte(6, &gate, b'B'),
        "b.txt",
    );
    publish_file_list(&mut context, &handle, "files", &["fileA", "fileB"]);
    assert_eval(
        &mut context,
        "globalThis.results = []; \
         fileA.text().then(v => globalThis.results.push('fileA:' + v)); \
         files.item(1).arrayBuffer().then(b => globalThis.results.push('fileB:' + b.byteLength)); \
         (globalThis.results.length === 0);",
    );
    assert_eq!(executor.pending(), 2);
    for _ in 0..2 {
        context.run_jobs().expect("run_jobs");
    }
    assert_eq!(eval_str(&mut context, "globalThis.results.join(',')"), "");
    // Open the gate and run both tasks on the test worker stand-in.
    {
        let (lock, gate) = &*gate;
        *lock.lock().expect("gate") = true;
        gate.notify_all();
    }
    executor.run_all();
    // Still nothing settled before `poll_io`.
    assert_eq!(eval_str(&mut context, "globalThis.results.join(',')"), "");
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.results.join(',')"),
        "fileA:AAAAA,fileB:6"
    );
    assert!(!handle.has_pending_io());
}

// ── Guard: no Boa-job filesystem materialize fallback ──
//
// The `BlockingSource` above is the behavioural oracle behind this guard:
// the JS object published below carries the blocking payload itself (via
// the test-only `blob_from_data` wrapper — no copy, no materialization),
// so the held worker task really blocks on the gate. If any Boa job
// performed the blocking read synchronously, the barrier would hang the
// Boa thread (or the promise would settle without `poll_io`).

#[test]
fn blocking_source_never_runs_inside_boa_job() {
    use std::sync::mpsc;
    let (mut context, handle, executor, wake) = setup_manual();
    // Publish a brand-valid Blob whose payload IS the blocking host
    // source. The `text()` call below submits a task that blocks on the
    // gate when executed; the manual executor holds it until released.
    let gate: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let gate_worker = Arc::clone(&gate);
    let data = blocking_blob(11, &gate);
    publish_blob(&handle, &mut context, "guardBlob", &data);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    // Drive the public read path; the manual executor holds the task.
    assert_eval(
        &mut context,
        "globalThis.guardVerdict = 'pending'; \
         guardBlob.arrayBuffer().then(
           b => { globalThis.guardVerdict = 'fulfilled:' + b.byteLength; },
           e => { globalThis.guardVerdict = 'rejected:' + e.name; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    // Move the held task to a worker thread that blocks on the gate: this
    // is the REAL task (blocking payload included), not a substitute.
    let held = executor.take_all();
    assert_eq!(held.len(), 1);
    let task = held.into_iter().next().expect("held task");
    let wake_count_before = wake.count();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            started_tx.send(()).expect("started");
            // Wait for the test's release before executing the blocking
            // read, so the Boa thread is provably usable while blocked.
            release_rx.recv().expect("release");
            // Execute the REAL held task: its `materialize` blocks on the
            // gate until the test opens it below.
            task.execute();
            drop(gate_worker);
        });
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("worker started");
        // While the worker is blocked on the gate, the Boa thread runs
        // unrelated jobs and the promise stays pending: no Boa job touches
        // the blocking source.
        assert_eval(
            &mut context,
            "globalThis.unrelated = 'no'; \
             Promise.resolve().then(() => { globalThis.unrelated = 'yes'; }); \
             true",
        );
        context.run_jobs().expect("run_jobs");
        assert_eq!(eval_str(&mut context, "globalThis.unrelated"), "yes");
        for _ in 0..2 {
            context.run_jobs().expect("run_jobs");
        }
        assert_eq!(eval_str(&mut context, "globalThis.guardVerdict"), "pending");
        // Open the gate and let the worker finish, then settle through the
        // documented loop. The 11-byte blocking payload packages exactly.
        // `wake` must have fired exactly once for this completion (the
        // worker pushed one DTO, then signalled the host loop).
        {
            let (lock, gate) = &*gate;
            *lock.lock().expect("gate") = true;
            gate.notify_all();
        }
        release_tx.send(()).expect("release worker");
    });
    assert_eq!(
        wake.count(),
        wake_count_before + 1,
        "exactly one wake per completion"
    );
    drive(&mut context, &handle);
    assert_eval(&mut context, "globalThis.guardVerdict === 'fulfilled:11'");
}

// ── M9B-IO-02: context-bound queue and wake contract ──

#[test]
fn poll_io_rejects_foreign_context_without_state_change() {
    let (mut context_a, handle_a, executor_a, _) = setup_manual();
    let (mut context_b, handle_b, _, _) = setup_manual();
    assert_eval(
        &mut context_a,
        "globalThis.a = new Blob(['a']).text(); true",
    );
    assert_eq!(executor_a.pending(), 1);
    // `poll_io` of another registration on this context is rejected.
    let error = handle_b
        .poll_io(&mut context_a)
        .expect_err("foreign poll_io must fail");
    assert_eq!(error, PollIoError::ForeignContext);
    // Nothing was consumed: the owning handle still settles it.
    assert_eq!(executor_a.pending(), 1);
    executor_a.run_all();
    drive(&mut context_a, &handle_a);
    assert_eval(&mut context_a, "globalThis.a instanceof Promise");
    let _ = (&mut context_b, handle_b);
}

#[test]
fn completion_before_poll_io_runs_no_js_and_wake_fires() {
    let (mut context, handle, executor, wake) = setup_manual();
    assert_eval(
        &mut context,
        "globalThis.seen = 'none'; \
         new Blob(['xy']).bytes().then(b => { globalThis.seen = 'got:' + b.length; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    assert!(wake.count() >= 1, "worker must signal the wake hook");
    // Completion is queued but no JS ran before `poll_io`.
    assert_eq!(eval_str(&mut context, "globalThis.seen"), "none");
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 1);
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.seen"), "got:2");
}

// ── M9B-IO-04: quota / error / cancel / shutdown exact-once ──

#[test]
fn success_error_cancel_shutdown_and_queue_full_free_quota_once() {
    // Queue-full (typed resource error) consumes no slot: the next read
    // still works after the rejection settles.
    {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::new(FullExecutor) as Arc<dyn FileIoExecutor>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        assert_eval(
            &mut context,
            "globalThis.q = 'pending'; \
             new Blob(['q']).text().then(
               () => { globalThis.q = 'fulfilled'; },
               e => { globalThis.q = e.name; }); \
             true",
        );
        context.run_jobs().expect("run_jobs");
        // TooManyReads maps to SecurityError (existing central mapping).
        assert_eq!(eval_str(&mut context, "globalThis.q"), "SecurityError");
        assert!(!handle.has_pending_io());
    }
    // Worker loss settles with a stable NotReadableError.
    {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .io_executor(Arc::new(LostExecutor) as Arc<dyn FileIoExecutor>)
            .build()
            .register(&mut context)
            .expect("registration failed");
        let _ = &handle;
        assert_eval(
            &mut context,
            "globalThis.w = 'pending'; \
             new Blob(['w']).text().then(
               () => { globalThis.w = 'fulfilled'; },
               e => { globalThis.w = (e instanceof DOMException) + ':' + e.name; }); \
             true",
        );
        context.run_jobs().expect("run_jobs");
        assert_eq!(
            eval_str(&mut context, "globalThis.w"),
            "true:NotReadableError"
        );
    }
    // Shutdown cancels outstanding work, clears the queue, and forbids
    // late settlement (exactly-once quota release).
    {
        let (mut context, handle, executor, _) = setup_manual();
        assert_eval(
            &mut context,
            "globalThis.s = 'pending'; \
             new Blob(['shut']).text().then(
               v => { globalThis.s = 'fulfilled'; },
               e => { globalThis.s = e.name; }); \
             true",
        );
        assert_eq!(executor.pending(), 1);
        assert!(handle.has_pending_io());
        handle.shutdown(&mut context).expect("shutdown");
        assert!(!handle.has_pending_io());
        // Late worker result is dropped safely: no job, no JS, no quota.
        executor.run_all();
        let settled = handle.poll_io(&mut context).expect("poll_io");
        assert_eq!(settled, 0);
        context.run_jobs().expect("run_jobs");
        assert_eq!(eval_str(&mut context, "globalThis.s"), "pending");
        // Post-shutdown reads reject without creating work.
        assert_eval(
            &mut context,
            "globalThis.s2 = 'pending'; \
             new Blob(['x']).text().then(
               () => { globalThis.s2 = 'fulfilled'; },
               e => { globalThis.s2 = e.name; }); \
             true",
        );
        context.run_jobs().expect("run_jobs");
        assert_eq!(eval_str(&mut context, "globalThis.s2"), "AbortError");
        assert!(!handle.has_pending_io());
    }
}

#[test]
fn sixty_five_concurrent_reads_obey_limit_and_recover() {
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_concurrent_reads_per_global: 64,
        ..Default::default()
    };
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits);
    assert_eval(
        &mut context,
        "globalThis.ok = 0; globalThis.err = 0; \
         globalThis.promises = []; \
         for (var i = 0; i < 64; i++) { \
           globalThis.promises.push(new Blob(['x']).text().then(
             () => { globalThis.ok++; }, () => { globalThis.err++; })); \
         } \
         globalThis.extra = new Blob(['x']).text().then(
           () => { globalThis.ok++; }, e => { if (e.name === 'SecurityError') globalThis.err++; }); \
         true",
    );
    assert_eq!(executor.pending(), 64);
    // The 65th read fails fast with a typed quota error (no slot taken).
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.err"), "1");
    // Run the 64 held tasks through the documented loop: all succeed and
    // capacity is fully recovered.
    executor.run_all();
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.ok"), "64");
    assert!(!handle.has_pending_io());
    // A new read works again after recovery.
    assert_eval(
        &mut context,
        "globalThis.again = 'pending'; \
         new Blob(['recovered']).text().then(v => { globalThis.again = v; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.again"), "recovered");
}

#[test]
fn mutation_snapshot_error_carries_no_partial_bytes() {
    // Gated live-filesystem oracle (unix only): import a live temp file,
    // mutate it after import, then read through the real promise path.
    // The worker's snapshot check fails with no partial bytes.
    //
    // On weak platforms (no strong open-handle identity) live imports are
    // refused by enforced policy, so this oracle cannot run there: the
    // copy-fallback semantics are covered by `m5_file_fs` instead.
    #[cfg(unix)]
    {
        use boa_fapi_fs::{FsRegistry, HostFileSource};
        let (mut context, handle, executor, _) = setup_manual();
        let dir = std::env::temp_dir().join(format!(
            "boa-fapi-m9b-mutation-{}-{}",
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
        // Mutate after import so the worker observes a changed snapshot.
        std::fs::write(&path, b"CHANGED-payload!!").expect("mutate temp file");
        assert_eval(
            &mut context,
            "globalThis.m = 'pending'; \
             mutFile.text().then(
               v => { globalThis.m = 'fulfilled:' + v; },
               e => { globalThis.m = (e instanceof DOMException) + ':' + e.name; }); \
             true",
        );
        assert_eq!(executor.pending(), 1);
        executor.run_all();
        // Still pending before `poll_io`: no JS ran on the worker path.
        assert_eq!(eval_str(&mut context, "globalThis.m"), "pending");
        drive(&mut context, &handle);
        assert_eq!(
            eval_str(&mut context, "globalThis.m"),
            "true:NotReadableError"
        );
        assert!(!handle.has_pending_io());
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}

#[test]
fn worker_panic_settles_stable_not_readable_error() {
    struct PanicSource;
    impl boa_fapi_core::source::ByteSource for PanicSource {
        fn len(&self) -> u64 {
            3
        }
        fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
            boa_fapi_core::snapshot::SnapshotState::Memory
        }
        fn read_range(
            &self,
            _range: std::ops::Range<u64>,
            _cancel: &boa_fapi_core::cancellation::CancellationToken,
        ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
            panic!("host source misbehaved");
        }
    }
    fn panic_blob() -> Arc<boa_fapi_core::blob::BlobData> {
        let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(PanicSource);
        Arc::new(
            boa_fapi_core::blob::BlobData::from_segments(
                vec![boa_fapi_core::blob::BlobSegment {
                    source,
                    offset: 0,
                    len: 3,
                }],
                "",
                &boa_fapi_core::limits::FileApiLimits::default(),
            )
            .expect("valid segments"),
        )
    }
    // The guarded worker entry contains the source panic: the JS object
    // published below carries the panicking payload itself (test-only
    // `blob_from_data`, no copy), and the held task is executed for real.
    // The completion is the stable `NotReadableError`, and no panic
    // escapes the worker.
    let (mut context, handle, executor, _) = setup_manual();
    publish_blob(&handle, &mut context, "panicBlob", &panic_blob());
    assert_eval(
        &mut context,
        "globalThis.p = 'pending'; \
         panicBlob.text().then(
           () => { globalThis.p = 'fulfilled'; },
           e => { globalThis.p = (e instanceof DOMException) + ':' + e.name; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    // Still pending before `poll_io`: containment happened on the worker,
    // settlement happens only through a Boa job.
    assert_eq!(eval_str(&mut context, "globalThis.p"), "pending");
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.p"),
        "true:NotReadableError"
    );
    assert!(!handle.has_pending_io());
}

#[test]
fn panicking_submit_settles_stable_not_readable_error() {
    struct PanickingExecutor;
    impl FileIoExecutor for PanickingExecutor {
        fn submit(&self, _task: FileIoTask) -> Result<(), FileIoSubmitError> {
            panic!("third-party executor misbehaved");
        }
    }
    // A panicking third-party `submit` is contained by `submit_guarded`
    // and surfaces as the stable `NotReadableError` (WorkerLost mapping),
    // with the reservation released exactly once.
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .io_executor(Arc::new(PanickingExecutor) as Arc<dyn FileIoExecutor>)
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        "globalThis.q = 'pending'; \
         new Blob(['q']).text().then(
           () => { globalThis.q = 'fulfilled'; },
           e => { globalThis.q = (e instanceof DOMException) + ':' + e.name; }); \
         true",
    );
    context.run_jobs().expect("run_jobs");
    assert_eq!(
        eval_str(&mut context, "globalThis.q"),
        "true:NotReadableError"
    );
    assert!(!handle.has_pending_io());
    // Capacity is intact: the failed submit released its reservation, so a
    // second read through a healthy executor path still works.
    drop(handle);
    let (mut context, handle, executor, _) = setup_manual();
    assert_eval(
        &mut context,
        "globalThis.r = 'pending'; \
         new Blob(['recovered']).text().then(v => { globalThis.r = v; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.r"), "recovered");
}

#[test]
fn panicking_wake_keeps_completion_queued() {
    struct PanickingWake;
    impl FileIoWake for PanickingWake {
        fn wake(&self, _context_id: FileApiContextId) {
            panic!("third-party wake misbehaved");
        }
    }
    // A panicking host wake is contained inside `push_completion`: the
    // worker survives and the completion stays queued for `poll_io`.
    let executor = ManualExecutor::new();
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .io_executor(Arc::clone(&executor) as Arc<dyn FileIoExecutor>)
        .io_wake(Arc::new(PanickingWake) as Arc<dyn FileIoWake>)
        .build()
        .register(&mut context)
        .expect("registration failed");
    assert_eval(
        &mut context,
        "globalThis.v = 'pending'; \
         new Blob(['wake-ok']).text().then(v => { globalThis.v = v; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    // Would unwind the worker without containment; with it, the completion
    // lands and the wake panic is swallowed.
    executor.run_all();
    assert!(handle.has_pending_io());
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.v"), "wake-ok");
    assert!(!handle.has_pending_io());
}

#[test]
fn memory_and_filesystem_results_match_byte_for_byte() {
    let (mut context, handle, executor, _) = setup_manual();
    let expected = b"byte-exact-payload-0123456789";
    // Memory path through the real worker task.
    assert_eval(
        &mut context,
        "globalThis.mem = null; \
         new Blob([new Uint8Array([98, 121, 116, 101, 45, 101, 120, 97, 99, 116, 45, 112, 97, 121, 108, 111, 97, 100, 45, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57])]).arrayBuffer().then(b => { globalThis.mem = Array.from(new Uint8Array(b)).join(','); }); \
         true",
    );
    executor.run_all();
    drive(&mut context, &handle);
    let mem = eval_str(&mut context, "globalThis.mem");
    // Real filesystem-backed path: live temp file imported as a File,
    // read through the real worker task (no `complete_ok` substitute).
    let dir = std::env::temp_dir().join(format!(
        "boa-fapi-m9b-parity-{}-{}",
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
    assert_eval(
        &mut context,
        "globalThis.fs = null; \
         fsFile.arrayBuffer().then(b => { globalThis.fs = Array.from(new Uint8Array(b)).join(','); }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    drive(&mut context, &handle);
    let fs = eval_str(&mut context, "globalThis.fs");
    // Both paths package identically: byte-exact.
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

// ── M9B-IO-04: cancellation exact-once via task.cancel() ──

#[test]
fn cancelled_task_settles_abort_and_frees_quota_once() {
    // `FileIoTask::cancel()` flips the cooperative token the worker
    // observes: the completion is `Cancelled` → `AbortError`, and the slot
    // is released exactly once (a follow-up read reserves cleanly).
    let (mut context, handle, executor, _) = setup_manual();
    assert_eval(
        &mut context,
        "globalThis.c = 'pending'; \
         new Blob(['cancel-me']).text().then(
           () => { globalThis.c = 'fulfilled'; },
           e => { globalThis.c = (e instanceof DOMException) + ':' + e.name; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    let tasks = executor.take_all();
    assert_eq!(tasks.len(), 1);
    let task = tasks.into_iter().next().expect("task");
    task.cancel();
    task.execute();
    // Still pending before `poll_io`: cancellation settled on the worker,
    // JS runs only after the host loop.
    assert_eq!(eval_str(&mut context, "globalThis.c"), "pending");
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.c"), "true:AbortError");
    assert!(!handle.has_pending_io());
    // Quota released exactly once: a new read reserves and settles.
    assert_eval(
        &mut context,
        "globalThis.c2 = 'pending'; \
         new Blob(['after-cancel']).text().then(v => { globalThis.c2 = v; }); \
         true",
    );
    assert_eq!(executor.pending(), 1);
    executor.run_all();
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.c2"), "after-cancel");
    assert!(!handle.has_pending_io());
}

// ── M9B-IO-02 (FIFO): out-of-order worker completion settles in order ──

#[test]
fn out_of_order_completions_settle_fifo() {
    // Two reads submitted a-then-b; the worker for b finishes first. The
    // bridge releases only the in-order prefix, so JS observes
    // `a:first,b:second` (the accepted M3 FIFO contract), never
    // `b:second,a:first`.
    let (mut context, handle, executor, _) = setup_manual();
    assert_eval(
        &mut context,
        "globalThis.log = []; \
         var a = new Blob(['first']).text(); \
         var b = new Blob(['second']).text(); \
         a.then(v => globalThis.log.push('a:' + v)); \
         b.then(v => globalThis.log.push('b:' + v)); \
         (globalThis.log.length === 0);",
    );
    assert_eq!(executor.pending(), 2);
    let mut tasks = executor.take_all();
    assert_eq!(tasks.len(), 2);
    let second = tasks.pop().expect("second task");
    let first = tasks.pop().expect("first task");
    // Complete b first (out of order), then a.
    second.execute();
    // b's completion is queued but NOT released: a is still outstanding.
    let settled = handle.poll_io(&mut context).expect("poll_io");
    assert_eq!(settled, 0);
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.log.join(',')"), "");
    // Now complete a: both release in submission order.
    first.execute();
    drive(&mut context, &handle);
    assert_eq!(
        eval_str(&mut context, "globalThis.log.join(',')"),
        "a:first,b:second"
    );
    assert!(!handle.has_pending_io());
}

// ── M9B-IO-01 (ids): exhaustion never reuses an id ──

#[test]
fn operation_id_exhaustion_fails_without_reuse() {
    // Integration-level invariant: settled public reads receive strictly
    // increasing, never-reused operation ids. The exact `u64::MAX`
    // boundary is exercised in `io::tests::operation_id_exhaustion_fails_closed_without_reuse`.
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_concurrent_reads_per_global: 1,
        ..Default::default()
    };
    let (mut context, handle, executor, _) = setup_manual_with_limits(limits);
    let mut seen: Vec<u64> = Vec::new();
    for index in 0..5 {
        assert_eval(
            &mut context,
            &format!(
                "globalThis.v{index} = 'pending'; \
                 new Blob(['x']).text().then(v => {{ globalThis.v{index} = v; }}); \
                 true"
            ),
        );
        assert_eq!(executor.pending(), 1);
        let tasks = executor.take_all();
        assert_eq!(tasks.len(), 1);
        let task = tasks.into_iter().next().expect("task");
        seen.push(task.operation_id().get());
        task.execute();
        drive(&mut context, &handle);
        assert_eq!(eval_str(&mut context, &format!("globalThis.v{index}")), "x");
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "operation ids must be unique");
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "operation ids must be monotonic: {seen:?}"
    );
    assert!(!handle.has_pending_io());
}

// ── M9B-HOST-01: reproducible host loop ──

#[test]
fn host_poll_run_jobs_loop_until_quiescent() {
    let (mut context, handle, executor, _) = setup_manual();
    assert_eval(
        &mut context,
        "globalThis.a = 'pending'; globalThis.b = 'pending'; \
         new Blob(['one']).text().then(v => { globalThis.a = v; }); \
         new Blob(['two']).text().then(v => { globalThis.b = v; }); \
         true",
    );
    assert_eq!(executor.pending(), 2);
    // The documented loop, verbatim: wake → poll_io → run_jobs, repeated
    // until both queues are quiescent.
    executor.run_all();
    loop {
        let settled = handle.poll_io(&mut context).expect("poll_io");
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            break;
        }
    }
    assert_eq!(eval_str(&mut context, "globalThis.a"), "one");
    assert_eq!(eval_str(&mut context, "globalThis.b"), "two");
    // One `run_jobs()` without `poll_io` is not required to wait for I/O:
    // a fresh read stays pending until the loop runs again.
    assert_eval(
        &mut context,
        "globalThis.c = 'pending'; \
         new Blob(['three']).text().then(v => { globalThis.c = v; }); \
         true",
    );
    context.run_jobs().expect("run_jobs");
    assert_eq!(eval_str(&mut context, "globalThis.c"), "pending");
    executor.run_all();
    drive(&mut context, &handle);
    assert_eq!(eval_str(&mut context, "globalThis.c"), "three");
}
