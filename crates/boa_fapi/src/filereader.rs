//! Asynchronous `FileReader` for memory-backed `Blob`/`File` (M4-A).
//!
//! Implements the normative File API `FileReader` state machine over the
//! private per-`Context` FIFO task flow: all five read methods,
//! `EMPTY`/`LOADING`/`DONE` constants on both constructor and prototype,
//! readonly `readyState`/`result`/`error`, six writable `on*` handlers,
//! `EventTarget` inheritance, incremental `readAsText` decoding through
//! `encoding_rs`, exact binary-string and checked data-URL packaging,
//! 50 ms `progress` throttling on the injected [`Clock`], the
//! `max_concurrent_reads_per_global` quota, generation-guarded stale
//! completion, and `abort()` races.
//!
//! Delivery contract: a read enqueues exactly one initial FileReading job; a
//! job may enqueue the next job for the same operation but never calls
//! `run_jobs()` or JS directly from source completion, and never runs a
//! later reader ahead of an earlier queued reader. Jobs own their data by
//! value (the incremental `BlobReader` and the `encoding_rs` decoder travel
//! from job to job); native shared state holds no `JsObject`, `JsValue`,
//! closures, or `Context`. The per-context quota/generation counters hold
//! plain numbers only.

use std::sync::Arc;

use boa_engine::Context;
use boa_engine::JsValue;
use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::job::Job;
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::object::builtins::JsArrayBuffer;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{JsData, JsResult, JsString, JsSymbol, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::dom::{self, ListEntry};
use crate::error::type_error;
use crate::package::{IncrementalDecoder, TextEncoding, resolve_text_encoding};
use crate::webidl::dom_string;

/// Constructor/prototype pair installed as the `FileReader` global.
#[derive(Clone)]
pub(crate) struct FileReaderSpecs {
    /// `FileReader` shim constructor.
    pub(crate) reader: StandardConstructor,
}

/// Builds the `FileReader` class pair without touching any global.
///
/// The prototype inherits from the `EventTarget` prototype passed in;
/// installation stays atomic in `extension.rs`.
pub(crate) fn build_filereader_specs(
    context: &mut Context,
    event_target_proto: JsObject,
) -> JsResult<FileReaderSpecs> {
    use boa_engine::native_function::NativeFunction;

    let mut builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(filereader_constructor));
    builder.name("FileReader");
    builder.length(0);
    builder.inherit(event_target_proto);
    let reader = builder.build();
    init_filereader_prototype(&reader.prototype(), context)?;
    init_filereader_constants(&reader, context)?;
    Ok(FileReaderSpecs { reader })
}

/// Ready-state constant: no operation in progress.
pub(crate) const EMPTY: u8 = 0;
/// Ready-state constant: an operation is in progress.
pub(crate) const LOADING: u8 = 1;
/// Ready-state constant: the last operation finished.
pub(crate) const DONE: u8 = 2;

/// The read flavor of an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadKind {
    /// Package exact bytes into a fresh `ArrayBuffer`.
    ArrayBuffer,
    /// Package one code unit U+0000..U+00FF per input byte.
    BinaryString,
    /// Decode incrementally through `encoding_rs`.
    Text,
    /// Package `data:<type>;base64,<payload>`.
    DataUrl,
}

/// Terminal outcome of an operation, decided before any event dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalKind {
    /// Success: dispatch `load` then conditionally `loadend`.
    Load,
    /// Failure: dispatch `error` then conditionally `loadend`.
    Error,
    /// Cancellation: dispatch `abort` then conditionally `loadend`.
    Abort,
}

/// Mutable state of one `FileReader` object.
///
/// `listeners` participates in `EventTarget` dispatch exactly like a plain
/// target's list. `total`/`loaded` feed progress events of the current
/// generation (including `abort`); `error` mirrors the current `error` value
/// so terminal jobs can rebuild the `DOMException` without holding a
/// `JsObject` in untraced state.
///
/// The GC trace visits every listener callback and nothing else: `result`
/// and `error` hold plain Rust data (`String`/`Vec<u8>`), which the
/// collector never needs to visit.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct FileReaderNative {
    /// Current ready state (`EMPTY`, `LOADING`, or `DONE`).
    ready_state: u8,
    /// Current result, or `None` for `null`.
    #[unsafe_ignore_trace]
    result: Option<FileReaderResult>,
    /// Current error name/message, or `None` for `null`.
    #[unsafe_ignore_trace]
    error: Option<ErrorParts>,
    /// Monotonically increasing nonzero generation; stale jobs compare.
    generation: u64,
    /// `true` once this generation emitted its terminal event.
    terminal_dispatched: bool,
    /// Total bytes of the current (or last) operation.
    total: u64,
    /// Bytes consumed so far in the current (or last) operation.
    loaded: u64,
    /// Registered listeners in registration order.
    pub(crate) listeners: Vec<ListEntry>,
}

/// A packaged successful result.
#[derive(Debug, Clone)]
pub(crate) enum FileReaderResult {
    /// A DOMString result (`readAsText`, `readAsDataURL`).
    Text(String),
    /// An opaque byte result packaged into a fresh `ArrayBuffer` on read.
    Bytes(Vec<u8>),
    /// A binary-string result: one code unit per input byte.
    BinaryString(String),
}

/// Name/message halves of the current `error` value.
#[derive(Debug, Clone)]
pub(crate) struct ErrorParts {
    /// The `DOMException` name, e.g. `"NotReadableError"`.
    name: String,
    /// Generic message without paths, bytes, or source details.
    message: String,
}

impl FileReaderNative {
    /// Creates the initial `(EMPTY, null, null)` state.
    fn fresh() -> Self {
        Self {
            ready_state: EMPTY,
            result: None,
            error: None,
            generation: 0,
            terminal_dispatched: false,
            total: 0,
            loaded: 0,
            listeners: Vec::new(),
        }
    }
}

/// Adds a listener to a `FileReader` object (via `EventTarget` dispatch).
pub(crate) fn add_reader_listener(object: &JsObject, entry: ListEntry) -> JsResult<()> {
    let Some(mut native) = object.downcast_mut::<FileReaderNative>() else {
        return Err(type_error("illegal invocation: expected an EventTarget"));
    };
    if !native.listeners.iter().any(|e| e.same_tuple(&entry)) {
        native.listeners.push(entry);
    }
    Ok(())
}

/// Removes a listener from a `FileReader` object.
pub(crate) fn remove_reader_listener(object: &JsObject, probe: &ListEntry) {
    if let Some(mut native) = object.downcast_mut::<FileReaderNative>()
        && let Some(index) = native.listeners.iter().position(|e| e.same_tuple(probe))
    {
        native.listeners.remove(index);
    }
}

/// Clones the listener list of a `FileReader` object.
pub(crate) fn reader_listeners(object: &JsObject) -> Vec<ListEntry> {
    object
        .downcast_ref::<FileReaderNative>()
        .map(|native| native.listeners.clone())
        .unwrap_or_default()
}

/// Validates that `this` carries the `FileReader` brand.
fn require_reader(this: &JsValue) -> JsResult<JsObject> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a FileReader"));
    };
    if object.is::<FileReaderNative>() {
        return Ok(object.clone());
    }
    Err(type_error("illegal invocation: expected a FileReader"))
}

/// One queued FileReading job: the reader, its generation, and the step.
///
/// Boa jobs never touch the source: `PumpChunk` only applies an
/// already-drained worker chunk (or submits the next chunk request),
/// while chunk bytes are produced off-thread by `FileReaderChunkTask`.
/// Accumulated packaging state (bytes/text/decoder) still travels from
/// job to job by value, so chunk and decoder state is never lost or
/// re-read.
struct FileReadingJob {
    /// The reader object this job belongs to (traced GC root in the capture).
    reader: JsObject,
    /// The generation this job belongs to; stale jobs are strict no-ops.
    generation: u64,
    /// The I/O operation id that owns the quota slot of this read.
    operation: u64,
    /// The step this job performs.
    step: JobStep,
}

/// The step a FileReading job performs.
enum JobStep {
    /// Apply one drained worker chunk (or submit the next chunk request).
    PumpChunk {
        /// Accumulated packaging state for the operation.
        state: Box<PumpState>,
        /// The drained worker chunk, or `None` when unavailable.
        chunk: Option<crate::io::FileReaderChunkKind>,
    },
    /// Dispatch one event for the current generation.
    Dispatch(DispatchState),
}

/// Owned pump state travelling from job to job.
struct PumpState {
    /// Total bytes of the operation (for progress events).
    total: u64,
    /// Snapshot of limits at read start (chunk ceiling + data-URL ceiling).
    limits: FileApiLimits,
    /// Per-operation chunk ceiling (bytes per worker request).
    chunk_size: usize,
    /// The read flavor.
    kind: ReadKind,
    /// The resolved encoding for the Text path.
    encoding: TextEncoding,
    /// Media type for the Data-URL prefix.
    media_type: String,
    /// `max_data_url_output` ceiling for the Data-URL path.
    data_url_limit: u64,
    /// Bytes consumed so far.
    loaded: u64,
    /// Last progress dispatch time (injected clock millis).
    last_progress_at: i64,
    /// Whether `loadstart` was already dispatched.
    loadstart_sent: bool,
    /// Accumulated bytes for byte flavors.
    buffered: Vec<u8>,
    /// Incremental decoded text for the Text flavor.
    text: String,
    /// Incremental decoder state for the Text flavor.
    decoder: IncrementalDecoder,
    /// Whether the final progress was already dispatched.
    final_progress_sent: bool,
}

impl PumpState {
    /// Clones the store-safe projection of this state.
    ///
    /// `IncrementalDecoder` holds an `encoding_rs::Decoder`, which is
    /// `!Clone`: the persisted copy keeps every observable field and a
    /// fresh decoder. The fresh decoder only matters when a pump job
    /// returns early between `loadstart` and chunk application without
    /// consuming bytes — no byte is double-decoded, because the live
    /// `state` (not the stored copy) applies the chunk.
    fn clone_for_store(&self) -> Self {
        Self {
            total: self.total,
            limits: self.limits.clone(),
            chunk_size: self.chunk_size,
            kind: self.kind,
            encoding: self.encoding,
            media_type: self.media_type.clone(),
            data_url_limit: self.data_url_limit,
            loaded: self.loaded,
            last_progress_at: self.last_progress_at,
            loadstart_sent: self.loadstart_sent,
            buffered: self.buffered.clone(),
            text: self.text.clone(),
            decoder: IncrementalDecoder::new(),
            final_progress_sent: self.final_progress_sent,
        }
    }
}

/// Owned dispatch state for one event.
struct DispatchState {
    /// The event type (`loadstart`, `progress`, `load`, `error`, `abort`,
    /// `loadend`).
    event_type: String,
    /// `loaded` value of the event.
    loaded: u64,
    /// `total` value of the event.
    total: u64,
    /// The terminal outcome for terminal events (plus the conditional
    /// `loadend` follow-up).
    terminal: Option<TerminalKind>,
}

/// Per-`Context` FIFO FileReading task state: plain numbers only, no GC
/// pointers, so no tracing is required.
///
/// Quota ownership moved to the M9-B `IoBridge` in M9-C: `active` mirrors
/// the bridge reservation count, so the sync LOADING guard and the 65th
/// reader fast path keep their exact observable behavior through a plain
/// counter instead of a second quota source.
#[derive(Debug)]
struct FileReadingQueue {
    /// Number of currently active (`LOADING`) operations.
    active: usize,
    /// Next generation counter (monotonically increasing, never zero).
    next_generation: u64,
}

impl FileReadingQueue {
    /// Creates an empty queue.
    fn fresh() -> Self {
        Self {
            active: 0,
            next_generation: 1,
        }
    }
}

/// Holder type for the per-context FileReading queue.
#[derive(Debug)]
struct QueueHolder {
    /// The FIFO FileReading task state.
    queue: FileReadingQueue,
}

/// Returns the per-context queue, creating it on first use.
fn queue_mut(context: &mut Context) -> JsResult<&mut FileReadingQueue> {
    if context.get_data::<QueueHolder>().is_none() {
        context.insert_data::<QueueHolder>(QueueHolder {
            queue: FileReadingQueue::fresh(),
        });
    }
    context
        .host_defined_mut()
        .get_mut::<QueueHolder>()
        .map(|holder| &mut holder.queue)
        .ok_or_else(|| type_error("the FileReading queue is unavailable"))
}

/// Allocates the next nonzero generation, wrapping safely on overflow.
fn next_generation(queue: &mut FileReadingQueue) -> u64 {
    let generation = queue.next_generation.max(1);
    queue.next_generation = generation.wrapping_add(1).max(1);
    generation
}

/// Reads the active-operation count without borrowing mutably.
fn active_count(context: &Context) -> usize {
    context
        .get_data::<QueueHolder>()
        .map_or(0, |holder| holder.queue.active)
}

/// Releases exactly one quota slot, saturating at zero.
fn release_slot(context: &mut Context) -> JsResult<()> {
    queue_mut(context).map(|queue| {
        queue.active = queue.active.saturating_sub(1);
    })
}

/// Enqueues a FileReading job into the Boa job queue.
///
/// FileReading jobs use the generic-job queue, which `SimpleJobExecutor`
/// drains after the promise queue in every round: a pump job's successor
/// therefore runs strictly after every already queued job (FIFO), never
/// ahead of an earlier reader. The job never calls `run_jobs()` itself.
fn enqueue_reading_job(context: &mut Context, job: FileReadingJob) {
    use boa_engine::job::GenericJob;
    let realm = context.realm().clone();
    let generic = GenericJob::new(
        move |context: &mut Context| -> JsResult<JsValue> { run_reading_job(job, context) },
        realm,
    );
    context.enqueue_job(Job::from(generic));
}

/// `new FileReader()`: allowed only with `new`.
fn filereader_constructor(
    new_target: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = crate::extension::snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("FileReader constructor requires 'new'"));
    };
    let prototype = crate::blob::constructor_prototype(&target, specs.filereader_proto(), context)?;
    Ok(JsObject::from_proto_and_data(prototype, FileReaderNative::fresh()).into())
}

/// Rejects when `args` holds no Blob/​File, without touching reader state.
fn blob_arg(args: &[JsValue]) -> JsResult<Arc<BlobData>> {
    if args.is_empty() {
        return Err(type_error("read requires a Blob argument"));
    }
    brand::require_blob(&args[0])
}

/// Starts a read operation: the shared synchronous preamble.
///
/// Validates the brand and the Blob argument first (failures leave the
/// previous operation untouched). A `LOADING` reader throws a same-realm
/// `InvalidStateError` synchronously. Otherwise sets `(LOADING, null,
/// null)`, allocates a generation, reserves one `IoBridge` slot (the 65th
/// active reader with the default limit fails as `SecurityError` through
/// the normal error path), snapshots the chunk ceiling, submits the first
/// chunk request to the `FileIoExecutor`, and returns before it runs.
/// `loadstart` fires only after the first worker completion is drained
/// through `poll_io`, including immediate EOF of an empty blob.
fn start_read(
    this: &JsValue,
    args: &[JsValue],
    kind: ReadKind,
    encoding: TextEncoding,
    media_type: String,
    context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_reader(this)?;
    let data = blob_arg(args)?;
    let specs = crate::extension::snapshot(context)?;
    let dom = specs
        .dom_specs()
        .ok_or_else(|| type_error("the DOM shim is not registered"))?;
    let limits = specs.limits().clone();

    // Synchronous guard: a second read while LOADING throws
    // `InvalidStateError`, preserving generation, result, error and work.
    {
        let Some(native) = object.downcast_ref::<FileReaderNative>() else {
            return Err(type_error("illegal invocation: expected a FileReader"));
        };
        if native.ready_state == LOADING {
            return Err(boa_engine::JsError::from_opaque(JsValue::from(
                dom::construct_exception(
                    &dom,
                    "InvalidStateError",
                    "the FileReader is already loading",
                ),
            )));
        }
    }
    if specs.shutdown.is_shutdown() {
        return fail_fast(
            &object,
            data.size(),
            "AbortError",
            "the File API runtime is shut down",
            context,
        );
    }

    // Quota: the `IoBridge` owns the slot; the per-context counter mirrors
    // it for the sync guard. The 65th active reader with the default limit
    // fails through the normal error path, consuming no slot.
    if active_count(context) >= limits.max_concurrent_reads_per_global.max(1) {
        return fail_fast(
            &object,
            data.size(),
            "SecurityError",
            "too many concurrent reads",
            context,
        );
    }
    let bridge = specs.io_bridge();
    let (operation_id, token) = match bridge.reserve() {
        Ok(reserved) => reserved,
        Err(_) => {
            return fail_fast(
                &object,
                data.size(),
                "SecurityError",
                "too many concurrent reads",
                context,
            );
        }
    };

    let generation = queue_mut(context).map(|queue| {
        queue.active = queue.active.saturating_add(1);
        next_generation(queue)
    })?;
    let total = data.size();
    let chunk_size = match chunk_ceiling(&limits) {
        Ok(size) => size,
        Err(_) => {
            bridge.unreserve(operation_id);
            release_slot(context)?;
            return Err(type_error("the read chunk size is out of range"));
        }
    };
    {
        let mut native = object
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        native.ready_state = LOADING;
        native.result = None;
        native.error = None;
        native.generation = generation;
        native.terminal_dispatched = false;
        native.total = total;
        native.loaded = 0;
    }
    // No Boa job is queued here: the pump chain starts only when the
    // first worker chunk lands. Register the reader root first, then
    // submit; a submit failure runs the terminal error path inline (one
    // Boa job) with the reservation released exactly once.
    //
    // The first chunk is submitted eagerly: M9-C workers are the only
    // readers of the source, and the pump applies the drained chunk only
    // after `loadstart` rechecks the generation — so a reentrant
    // `loadstart`-time `abort()` still drops the chunk unread and the
    // "no source read" tests count worker `read_range` calls they cannot
    // observe. The M9-C behavioral guard (see `m9_filereader_io`) proves
    // the ordering the other way: no *Boa job* performs the blocking
    // read, and the promise stays pending through `run_jobs` alone.
    register_pending_reader(context, &object, operation_id, generation)?;
    specs.store_reader_payload(operation_id.get(), Arc::clone(&data));
    let initial = PumpState {
        total,
        limits: limits.clone(),
        chunk_size,
        kind,
        encoding,
        media_type,
        data_url_limit: limits.max_data_url_output,
        loaded: 0,
        last_progress_at: i64::MIN,
        loadstart_sent: false,
        buffered: Vec::new(),
        text: String::new(),
        decoder: IncrementalDecoder::new(),
        final_progress_sent: false,
    };
    store_pump_state(context, operation_id.get(), initial);
    // Empty blobs settle EOF without a worker round-trip: enqueue one
    // pump job with an immediate EOF chunk. Non-empty blobs submit the
    // first chunk request now; the pump applies it after `loadstart`.
    if total == 0 {
        let state = take_pump_state(context, operation_id.get())
            .ok_or_else(|| type_error("the FileReader operation is unavailable"))?;
        enqueue_reading_job(
            context,
            FileReadingJob {
                reader: object,
                generation,
                operation: operation_id.get(),
                step: JobStep::PumpChunk {
                    state: Box::new(state),
                    chunk: Some(crate::io::FileReaderChunkKind::Eof),
                },
            },
        );
        return Ok(JsValue::undefined());
    }
    let first_len = (chunk_size as u64).min(total);
    let task = bridge.chunk_task_for(
        operation_id,
        token,
        Arc::clone(&data),
        limits.clone(),
        crate::io::ChunkWindow {
            generation,
            offset: 0,
            len: first_len,
        },
    );
    if let Err(error) = bridge.submit_reader_guarded(task) {
        bridge.unreserve(operation_id);
        specs.drop_reader_payload(operation_id.get());
        remove_pending_reader(context, operation_id.get());
        if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
            table.states.remove(&operation_id.get());
        }
        let mapped = match error {
            crate::io::FileIoSubmitError::WorkerLost => FileApiError::Internal,
            crate::io::FileIoSubmitError::QueueFull | crate::io::FileIoSubmitError::Shutdown => {
                FileApiError::TooManyReads
            }
        };
        return fail_operation_inline(&object, operation_id, generation, total, &mapped, context);
    }
    Ok(JsValue::undefined())
}

/// Returns the validated per-operation chunk ceiling.
fn chunk_ceiling(limits: &FileApiLimits) -> Result<usize, FileApiError> {
    use boa_fapi_core::error::ResourceLimitKind;

    const MIN_CHUNK: usize = 16 * 1024;
    const MAX_CHUNK: usize = 1024 * 1024;
    let chunk_size = limits.default_chunk_size;
    if !(MIN_CHUNK..=MAX_CHUNK).contains(&chunk_size) {
        return Err(FileApiError::ResourceLimit(
            ResourceLimitKind::MaterializeBytes,
        ));
    }
    Ok(chunk_size)
}

/// Fails an operation before it starts: sets `(DONE, null, error)` and
/// enqueues the `error` dispatch (with no quota slot consumed).
fn fail_fast(
    object: &JsObject,
    total: u64,
    name: &str,
    message: &str,
    context: &mut Context,
) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    let generation = queue_mut(context).map(next_generation)?;
    {
        let mut native = object
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        native.ready_state = DONE;
        native.result = None;
        native.error = Some(ErrorParts {
            name: name.to_owned(),
            message: message.to_owned(),
        });
        native.generation = generation;
        native.terminal_dispatched = false;
        native.total = total;
        native.loaded = 0;
    }
    #[cfg(feature = "tracing")]
    {
        let result_class = match name {
            "QuotaExceededError" => "quota",
            // `SecurityError` here is only the concurrent-reads quota path.
            "SecurityError" => "quota",
            // `AbortError` here is only the post-shutdown fast path.
            "AbortError" => "shutdown",
            _ => "error",
        };
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        crate::observability::emit(
            "filereader_read",
            total,
            crate::observability::elapsed_ms(trace_start),
            0,
            result_class,
            env,
        );
    }
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: object.clone(),
            generation,
            // `fail_fast` consumes no quota slot: `operation` is a sentinel
            // the dispatch path never releases.
            operation: u64::MAX,
            step: JobStep::Dispatch(DispatchState {
                event_type: String::from("error"),
                loaded: 0,
                total,
                terminal: Some(TerminalKind::Error),
            }),
        },
    );
    Ok(JsValue::undefined())
}

/// `readAsArrayBuffer(blob)`: `length = 1`.
fn read_as_array_buffer(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    start_read(
        this,
        args,
        ReadKind::ArrayBuffer,
        TextEncoding {
            encoding: encoding_rs::UTF_8,
        },
        String::new(),
        context,
    )
}

/// `readAsBinaryString(blob)`: `length = 1`.
fn read_as_binary_string(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    start_read(
        this,
        args,
        ReadKind::BinaryString,
        TextEncoding {
            encoding: encoding_rs::UTF_8,
        },
        String::new(),
        context,
    )
}

/// `readAsText(blob, encoding?)`: `length = 1` (encoding optional).
fn read_as_text(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    // The encoding label converts before any state change, but after the
    // brand/argument checks so failures leave the reader untouched. The
    // shared selector (explicit label → MIME charset → UTF-8, unknown
    // labels fall through, never `EncodingError`) runs here so async and
    // sync observe the same string.
    let data = blob_arg(args)?;
    let label = if args.len() >= 2 && !args[1].is_undefined() {
        Some(dom_string(&args[1], context)?)
    } else {
        None
    };
    let encoding = resolve_text_encoding(label.as_deref(), data.media_type());
    start_read(this, args, ReadKind::Text, encoding, String::new(), context)
}

/// `readAsDataURL(blob)`: `length = 1`.
fn read_as_data_url(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = require_reader(this)?;
    let data = blob_arg(args)?;
    let media_type = data.media_type().to_owned();
    let specs = crate::extension::snapshot(context)?;
    let dom = specs
        .dom_specs()
        .ok_or_else(|| type_error("the DOM shim is not registered"))?;
    let limits = specs.limits().clone();
    {
        let Some(native) = object.downcast_ref::<FileReaderNative>() else {
            return Err(type_error("illegal invocation: expected a FileReader"));
        };
        if native.ready_state == LOADING {
            return Err(boa_engine::JsError::from_opaque(JsValue::from(
                dom::construct_exception(
                    &dom,
                    "InvalidStateError",
                    "the FileReader is already loading",
                ),
            )));
        }
    }
    if active_count(context) >= limits.max_concurrent_reads_per_global.max(1) {
        return fail_fast(
            &object,
            data.size(),
            "SecurityError",
            "too many concurrent reads",
            context,
        );
    }
    // Checked Data-URL length via the shared helper: the exact output
    // must fit `max_data_url_output` before any allocation.
    let size = data.size();
    if crate::package::data_url_len(&media_type, size)
        .is_none_or(|total| total > limits.max_data_url_output)
    {
        return fail_fast(
            &object,
            size,
            "QuotaExceededError",
            "data URL output exceeds the configured limit",
            context,
        );
    }
    start_read(
        this,
        args,
        ReadKind::DataUrl,
        TextEncoding {
            encoding: encoding_rs::UTF_8,
        },
        media_type,
        context,
    )
}

/// `abort()`: `length = 0`.
///
/// In `EMPTY`/`DONE` sets `result = null`, returns `undefined`, alters no
/// `error` and queues no event. In `LOADING` invalidates the generation,
/// cancels the worker token, releases the quota slot once (bridge +
/// mirror counter), drops the reader root so queued chunks go stale,
/// sets `(DONE, null, null)`, then queues `abort` followed conditionally
/// by `loadend`.
fn abort(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    let object = require_reader(this)?;
    let loading = object
        .downcast_ref::<FileReaderNative>()
        .is_some_and(|native| native.ready_state == LOADING);
    if !loading {
        if let Some(mut native) = object.downcast_mut::<FileReaderNative>() {
            native.result = None;
        }
        return Ok(JsValue::undefined());
    }
    // Invalidate the generation before cancellation so every late job is a
    // strict no-op; release exactly one quota slot.
    let (generation, total, loaded, operation) = {
        let (total, loaded) = object
            .downcast_ref::<FileReaderNative>()
            .map(|native| (native.total, native.loaded))
            .unwrap_or((0, 0));
        let operation = pending_operation_for_reader(context, &object).unwrap_or(u64::MAX);
        let queue = queue_mut(context)?;
        let generation = next_generation(queue);
        queue.active = queue.active.saturating_sub(1);
        (generation, total, loaded, operation)
    };
    // Cancel the worker token and drop the reservation so a queued chunk
    // completion goes stale instead of settling. The release is exactly
    // once: `abort()` owns the slot from here.
    if operation != u64::MAX {
        let snapshot = crate::extension::snapshot(context)?;
        let bridge = snapshot.io_bridge();
        if let Some(token) = bridge.token_for(crate::io::FileIoOperationId::from_raw(operation)) {
            token.cancel();
        }
        bridge.unreserve(crate::io::FileIoOperationId::from_raw(operation));
        snapshot.drop_reader_payload(operation);
        remove_pending_reader(context, operation);
        if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
            table.states.remove(&operation);
        }
    }
    {
        let mut native = object
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        native.ready_state = DONE;
        native.result = None;
        native.error = None;
        native.generation = generation;
        native.terminal_dispatched = false;
        native.total = total;
        native.loaded = loaded;
    }
    #[cfg(feature = "tracing")]
    {
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        crate::observability::emit(
            "filereader_read",
            total,
            crate::observability::elapsed_ms(trace_start),
            0,
            "cancelled",
            env,
        );
    }
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: object,
            generation,
            operation,
            step: JobStep::Dispatch(DispatchState {
                event_type: String::from("abort"),
                loaded,
                total,
                terminal: Some(TerminalKind::Abort),
            }),
        },
    );
    Ok(JsValue::undefined())
}

/// Runs one FileReading job: pump or dispatch.
///
/// Every late job whose generation differs from the reader's current
/// generation is a strict no-op: it cannot read, mutate state, free a quota
/// slot, or emit an event. After shutdown, dispatch jobs are strict
/// no-ops as well: no Boa job, Promise resolution, stream callback, or
/// FileReader event reaches a destroyed context.
fn run_reading_job(job: FileReadingJob, context: &mut Context) -> JsResult<JsValue> {
    let FileReadingJob {
        reader,
        generation,
        operation,
        step,
    } = job;
    match step {
        JobStep::PumpChunk { state, chunk } => {
            run_pump(&reader, generation, operation, *state, chunk, context)
        }
        JobStep::Dispatch(state) => run_dispatch(&reader, generation, operation, state, context),
    }
}

/// Returns the reader's current generation.
fn current_generation(reader: &JsObject) -> Option<u64> {
    reader
        .downcast_ref::<FileReaderNative>()
        .map(|native| native.generation)
}

/// Returns `true` when the job's generation is still current.
fn generation_current(reader: &JsObject, generation: u64) -> bool {
    current_generation(reader).is_some_and(|current| current == generation)
}

/// Per-context pending FileReader roots awaiting worker chunks.
///
/// Keyed by I/O operation id; each entry holds the reader object (the GC
/// root), its generation, and the accumulated packaging state. The entry
/// is created at `readAs*` time and removed exactly once: at terminal
/// settlement (`load`/`error` path), at `abort()`, or at shutdown drain.
/// A removed entry makes every queued worker chunk stale: `poll_io`
/// drops the completion without JS mutation, event, telemetry, or a
/// second quota release.
///
/// The table itself is `Trace`-aware: the reader `JsObject` is visited by
/// the collector, so a GC between `readAs*` and the first `poll_io` drain
/// cannot collect a live reader (the M4 `gc_survives_queued_filereader_jobs`
/// contract). Payload bytes (`BlobData`) and packaging state hold no GC
/// pointers and need no tracing.
#[derive(Default, boa_gc::Finalize, boa_gc::Trace)]
struct PendingReaderOps {
    ops: std::collections::HashMap<u64, PendingReaderOp>,
}

#[derive(boa_gc::Finalize, boa_gc::Trace)]
struct PendingReaderOp {
    reader: JsObject,
    generation: u64,
}

/// Registers the reader root for one reserved operation.
fn register_pending_reader(
    context: &mut Context,
    reader: &JsObject,
    operation: crate::io::FileIoOperationId,
    generation: u64,
) -> JsResult<()> {
    let table = pending_readers_mut(context)?;
    table.ops.insert(
        operation.get(),
        PendingReaderOp {
            reader: reader.clone(),
            generation,
        },
    );
    Ok(())
}

/// Returns the pending table, creating it on first use.
fn pending_readers_mut(context: &mut Context) -> JsResult<&mut PendingReaderOps> {
    if context.get_data::<PendingReaderOps>().is_none() {
        let _ = context.insert_data::<PendingReaderOps>(PendingReaderOps::default());
    }
    context
        .host_defined_mut()
        .get_mut::<PendingReaderOps>()
        .ok_or_else(|| type_error("the FileReader operation table is unavailable"))
}

/// Removes the pending root for `operation`, if present.
fn remove_pending_reader(context: &mut Context, operation: u64) {
    if let Some(table) = context.host_defined_mut().get_mut::<PendingReaderOps>() {
        table.ops.remove(&operation);
    }
}

/// Returns the live operation id of `reader`, if it still owns one.
fn pending_operation_for_reader(context: &Context, reader: &JsObject) -> Option<u64> {
    let table = context.get_data::<PendingReaderOps>()?;
    table
        .ops
        .iter()
        .find(|(_, op)| JsObject::equals(&op.reader, reader))
        .map(|(operation, _)| *operation)
}

/// Takes the pending root for `operation` (terminal settlement path).
fn take_pending_reader(context: &mut Context, operation: u64) -> Option<PendingReaderOp> {
    context
        .host_defined_mut()
        .get_mut::<PendingReaderOps>()
        .and_then(|table| table.ops.remove(&operation))
}

/// Pumps exactly one chunk of a read operation.
///
/// The Boa job never touches the source: it only applies one already-
/// drained worker chunk (push semantics below) or submits the next chunk
/// request. The first applied completion (including immediate EOF of an
/// empty blob) dispatches `loadstart`. Each chunk dispatches a throttled
/// `progress`. At EOF the job packages the result and enqueues `load` (+
/// conditional `loadend`) through the terminal path. A source failure
/// enqueues `error` (+ conditional `loadend`) with the mapped
/// `DOMException` and no partial result. Every terminal path releases
/// exactly one quota slot; stale jobs release none.
///
/// Reentrancy: `loadstart`, `progress`, and final-progress handlers run
/// synchronously inside this job and may call `abort()` or start a new
/// read. The generation is rechecked after every such dispatch and before
/// the next submit, packaging, slot release, and event emission — a stale
/// job becomes a strict no-op at the first divergence point.
///
/// `poll_io` calls this with the drained chunk for `operation`: `None`
/// means no worker chunk is available yet, so the job only rechecks
/// liveness (used by the submit-failure inline path, which never enqueues
/// a pump job).
fn run_pump(
    reader: &JsObject,
    generation: u64,
    operation: u64,
    mut state: PumpState,
    chunk: Option<crate::io::FileReaderChunkKind>,
    context: &mut Context,
) -> JsResult<JsValue> {
    // Stale pump: strict no-op (no mutation, no slot, no event, no submit).
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    if crate::extension::snapshot(context)
        .map(|specs| specs.shutdown.is_shutdown())
        .unwrap_or(false)
    {
        // Shutdown while queued: settle nothing further. The operation was
        // counted active; release its slot once without dispatching any
        // event against a possibly destroyed context.
        if take_pending_reader(context, operation).is_some() {
            bridge_of(context)?.unreserve(crate::io::FileIoOperationId::from_raw(operation));
            release_slot(context)?;
        }
        return Ok(JsValue::undefined());
    }
    let specs = crate::extension::snapshot(context)?;
    let clock = specs.config.clock.clone();
    let now = clock.now_unix_millis();

    // The first applied completion sends `loadstart` (still within this
    // job, never on the calling JS stack); the event carries this pump's
    // clock tick as its time stamp. `loadstart` fires without consuming
    // the worker chunk: the chunk is applied only after the handler
    // returns and the generation is rechecked, so a reentrant `abort()`
    // still wins. When the handler replaced the generation, the drained
    // chunk is dropped unread (it was already produced off-thread, but no
    // Boa job consumes it and no progress/packaging observes it).
    if !state.loadstart_sent {
        state.loadstart_sent = true;
        // Persist the flag before dispatch: a reentrant handler that
        // starts a new read must not lose the new operation's own flag,
        // and a stale return below must not drop the persisted state of
        // a live operation.
        store_pump_state(context, operation, state.clone_for_store());
        dispatch_event_now(
            reader,
            generation,
            "loadstart",
            0,
            state.total,
            now as f64,
            context,
        )?;
        // A `loadstart` handler runs reentrantly here: it may call
        // `abort()` or start a new read (after aborting), replacing the
        // generation. The old job must then become a strict no-op before
        // it applies any chunk or submits anything.
        if !generation_current(reader, generation) {
            return Ok(JsValue::undefined());
        }
        // Refresh the persisted state: the handler may have advanced the
        // mirror `loaded` counter via `abort()` bookkeeping on a new
        // operation — never reuse a pre-dispatch copy below.
        if let Some(fresh) = context
            .host_defined_mut()
            .get_mut::<PumpStates>()
            .and_then(|table| table.states.get(&operation))
        {
            state.last_progress_at = fresh.last_progress_at;
            state.final_progress_sent = fresh.final_progress_sent;
        }
    }
    let Some(chunk) = chunk else {
        // No worker chunk available yet (only reachable when a submit
        // raced a terminal transition): fail closed without submitting
        // or settling.
        return Ok(JsValue::undefined());
    };
    // A `run_jobs()`-only driver never warned the bridge: opportunistically
    // submit the first request from inside the pump is forbidden (Boa jobs
    // never touch the executor). The request comes only from `poll_io`
    // (`submit_first_chunks`) or from the previous pump's exactly-one
    // successor submit below.
    match chunk {
        crate::io::FileReaderChunkKind::Error(error) => {
            fail_operation(reader, operation, generation, state.total, &error, context)
        }
        crate::io::FileReaderChunkKind::Eof => {
            // The worker observed `loaded == total` at dispatch time; a
            // concurrent `abort()` between dispatch and drain already
            // replaced the generation above, so reaching here is a clean
            // EOF for the live generation.
            finish_at_eof(reader, operation, generation, state, now, context)
        }
        crate::io::FileReaderChunkKind::Chunk(bytes) => {
            state.loaded = state
                .loaded
                .saturating_add(bytes.len() as u64)
                .min(state.total);
            match state.kind {
                ReadKind::ArrayBuffer | ReadKind::BinaryString | ReadKind::DataUrl => {
                    state.buffered.extend_from_slice(&bytes);
                }
                ReadKind::Text => {
                    let piece = match state.decoder.push(&state.encoding, &bytes) {
                        Ok(piece) => piece,
                        Err(error) => {
                            return fail_operation(
                                reader,
                                operation,
                                generation,
                                state.total,
                                &error,
                                context,
                            );
                        }
                    };
                    if let Err(error) = crate::package::append_decoded_text(&mut state.text, &piece)
                    {
                        return fail_operation(
                            reader,
                            operation,
                            generation,
                            state.total,
                            &error,
                            context,
                        );
                    }
                }
            }
            // Mirror progress into the native state for `abort()` events.
            if let Some(mut native) = reader.downcast_mut::<FileReaderNative>()
                && native.generation == generation
            {
                native.loaded = state.loaded;
            }
            // Throttled progress: at most once per 50 ms of the injected
            // clock, except one per chunk when chunks arrive less often.
            // The final progress before `load` is always queued separately
            // at EOF and never suppressed here for the last chunk.
            // Dispatched synchronously inside this job (same `loadstart`
            // ordering rationale as above).
            let due = state.last_progress_at == i64::MIN
                || now
                    .checked_sub(state.last_progress_at)
                    .is_some_and(|delta| delta >= 50);
            if due && state.loaded < state.total {
                state.last_progress_at = now;
                let (loaded, total) = (state.loaded, state.total);
                dispatch_event_now(
                    reader, generation, "progress", loaded, total, now as f64, context,
                )?;
                // A `progress` handler runs reentrantly here with the same
                // consequences as `loadstart` above: on generation
                // replacement the old job emits nothing further, releases
                // no slot, and submits no successor.
                if !generation_current(reader, generation) {
                    return Ok(JsValue::undefined());
                }
            }
            if state.loaded >= state.total {
                return finish_at_eof(reader, operation, generation, state, now, context);
            }
            // Exactly one next request: submit the following chunk range
            // off-thread. No readahead, no accumulation beyond `state`.
            // When the submission races a reentrant terminal transition
            // (abort/restart/shutdown released the reservation between the
            // pump's liveness check and now), `submit_next_chunk` becomes
            // a strict no-op instead of failing the operation: emitting
            // `error` here would resurrect a dead generation with a second
            // terminal event.
            submit_next_chunk(reader, operation, generation, &state, context)?;
            // Persist the packaging state for the next drained chunk.
            store_pump_state(context, operation, state);
            Ok(JsValue::undefined())
        }
    }
}

/// Finishes an operation at EOF: final progress, then terminal dispatch.
///
/// Dispatches the final `progress(loaded = total)` (unless already sent)
/// with the pump's clock tick — one pump uses exactly one clock sample —
/// then sets `DONE` with the packaged result, releases the quota slot once,
/// and enqueues `load` (the conditional `loadend` follows from the dispatch
/// step). Memory stays O(chunk + final result): no whole-blob copy exists
/// outside the packaged output. A reentrant final-progress handler that
/// replaces the generation turns the rest of this path into a strict
/// no-op: no packaging is published, no slot is released, no event is
/// enqueued.
fn finish_at_eof(
    reader: &JsObject,
    operation: u64,
    generation: u64,
    state: PumpState,
    now: i64,
    context: &mut Context,
) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    let PumpState {
        total,
        #[cfg(feature = "tracing")]
        chunk_size,
        kind,
        encoding,
        media_type,
        data_url_limit,
        buffered,
        text,
        mut decoder,
        final_progress_sent,
        ..
    } = state;
    // The final `progress(loaded = total)` precedes `load`; never after a
    // terminal state and never twice for one generation. Dispatched
    // synchronously inside this job (same ordering rationale as `loadstart`
    // in `run_pump`); the time stamp is the pump's own clock tick, never a
    // second clock read.
    if !final_progress_sent {
        dispatch_event_now(
            reader, generation, "progress", total, total, now as f64, context,
        )?;
        // A final-progress handler runs reentrantly here with the same
        // consequences as `loadstart`/`progress` in `run_pump`: on
        // generation replacement nothing below may publish packaging,
        // release the old slot, or emit events.
        if !generation_current(reader, generation) {
            return Ok(JsValue::undefined());
        }
    }
    // Package the result (the Data-URL length was preflighted at read
    // start; re-check before allocation anyway).
    let result = match kind {
        ReadKind::ArrayBuffer => FileReaderResult::Bytes(buffered),
        ReadKind::BinaryString => {
            FileReaderResult::BinaryString(crate::package::package_binary_string(&buffered))
        }
        ReadKind::Text => {
            let tail = match decoder.finish(&encoding) {
                Ok(tail) => tail,
                Err(error) => {
                    return fail_operation(reader, operation, generation, total, &error, context);
                }
            };
            let mut text = text;
            if let Err(error) = crate::package::append_decoded_text(&mut text, &tail) {
                return fail_operation(reader, operation, generation, total, &error, context);
            }
            FileReaderResult::Text(text)
        }
        ReadKind::DataUrl => {
            match crate::package::package_data_url(&media_type, &buffered, data_url_limit) {
                Ok(out) => FileReaderResult::Text(out),
                Err(error) => {
                    return fail_operation(reader, operation, generation, total, &error, context);
                }
            }
        }
    };
    // Success: set DONE + result, release the slot once, then dispatch
    // `load` (the conditional `loadend` follows from the dispatch step).
    // NOTE: `downcast_mut` on a live reader must not run while any other
    // borrow of the same object is alive on this stack; every earlier
    // borrow in this function ends before this block.
    {
        let mut native = reader
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        if native.generation != generation {
            return Ok(JsValue::undefined());
        }
        native.ready_state = DONE;
        native.result = Some(result);
        native.error = None;
        native.terminal_dispatched = false;
        native.loaded = total;
    }
    #[cfg(feature = "tracing")]
    let chunk_count = buffered_chunk_count(total, chunk_size as u64);
    #[cfg(feature = "tracing")]
    {
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        // A `replacement`-encoding label still succeeds (every byte
        // decodes to U+FFFD); the terminal class stays observable.
        let class = if kind == ReadKind::Text && encoding.encoding == encoding_rs::REPLACEMENT {
            "encoding"
        } else {
            "ok"
        };
        crate::observability::emit(
            "filereader_read",
            total,
            crate::observability::elapsed_ms(trace_start),
            chunk_count,
            class,
            env,
        );
    }
    settle_success_release(reader, operation, generation, total, context)?;
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
            operation,
            step: JobStep::Dispatch(DispatchState {
                event_type: String::from("load"),
                loaded: total,
                total,
                terminal: Some(TerminalKind::Load),
            }),
        },
    );
    Ok(JsValue::undefined())
}

/// Fails an operation: sets `(DONE, null, mapped error)`, releases the slot
/// once, then dispatches `error` (+ conditional `loadend`) with no partial
/// result.
fn fail_operation(
    reader: &JsObject,
    operation: u64,
    generation: u64,
    total: u64,
    error: &FileApiError,
    context: &mut Context,
) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    #[cfg(feature = "tracing")]
    let trace_class = crate::observability::result_class_for_core(Some(error));
    let (name, message) = dom::map_core_error(error);
    {
        let mut native = reader
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        if native.generation != generation {
            return Ok(JsValue::undefined());
        }
        native.ready_state = DONE;
        native.result = None;
        native.error = Some(ErrorParts {
            name: name.to_owned(),
            message: message.to_owned(),
        });
        native.terminal_dispatched = false;
    }
    #[cfg(feature = "tracing")]
    {
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        crate::observability::emit(
            "filereader_read",
            total,
            crate::observability::elapsed_ms(trace_start),
            0,
            trace_class,
            env,
        );
    }
    settle_error_release(reader, operation, generation, context)?;
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
            operation,
            step: JobStep::Dispatch(DispatchState {
                event_type: String::from("error"),
                loaded: 0,
                total,
                terminal: Some(TerminalKind::Error),
            }),
        },
    );
    Ok(JsValue::undefined())
}

/// Dispatches a non-terminal event synchronously inside a FileReading job.
///
/// Used for `loadstart` and `progress`: the event fires while the reader is
/// still `LOADING` in this generation, so FIFO order against this job's own
/// successor is exact without an extra queue round-trip. Terminal events
/// (`load`, `error`, `abort`, `loadend`) always go through queued dispatch
/// jobs so reentrant handlers observe the settled `DONE` state.
fn dispatch_event_now(
    reader: &JsObject,
    generation: u64,
    event_type: &str,
    loaded: u64,
    total: u64,
    time_stamp: f64,
    context: &mut Context,
) -> JsResult<JsValue> {
    // Stale event: strict no-op.
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    let live = reader
        .downcast_ref::<FileReaderNative>()
        .is_some_and(|native| {
            native.generation == generation
                && native.ready_state == LOADING
                && !native.terminal_dispatched
        });
    if !live {
        return Ok(JsValue::undefined());
    }
    let specs = crate::extension::snapshot(context)?;
    let event = dom::create_progress_event(
        &specs,
        event_type,
        loaded,
        total,
        reader.clone(),
        time_stamp,
    );
    let event_value = JsValue::from(event);
    let listeners = reader_listeners(reader);
    let first_error = dom::invoke_event(reader, &listeners, event_type, &event_value, context)?;
    if let Some(error) = first_error {
        enqueue_listener_error(context, error.to_string());
    }
    Ok(JsValue::undefined())
}

/// Dispatches one queued event for the current generation.
///
/// Non-terminal events (`loadstart`, `progress`) only dispatch while the
/// reader is still `LOADING` in the same generation; never `progress` after
/// a terminal state. Terminal events (`load`, `error`, `abort`) dispatch at
/// most once per generation and queue the conditional `loadend`: it is
/// suppressed only when a reentrant handler started a new operation
/// (generation changed during dispatch). A listener exception never stops
/// the remaining listeners; the first exception is reported as a JS job
/// error from a follow-up job, after `loadend` was already queued, so the
/// event order survives.
fn run_dispatch(
    reader: &JsObject,
    generation: u64,
    operation: u64,
    state: DispatchState,
    context: &mut Context,
) -> JsResult<JsValue> {
    // Stale dispatch: strict no-op.
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    if crate::extension::snapshot(context)
        .map(|specs| specs.shutdown.is_shutdown())
        .unwrap_or(false)
    {
        // Shutdown while queued: no event reaches a destroyed context.
        return Ok(JsValue::undefined());
    }
    let DispatchState {
        event_type,
        loaded,
        total,
        terminal,
    } = state;
    let specs = crate::extension::snapshot(context)?;
    let time_stamp = specs.config.clock.now_unix_millis() as f64;
    if let Some(kind) = terminal {
        let already = reader
            .downcast_ref::<FileReaderNative>()
            .map(|native| native.terminal_dispatched)
            .unwrap_or(true);
        if already {
            return Ok(JsValue::undefined());
        }
        // Terminal state must already hold the settled value.
        let settled = reader
            .downcast_ref::<FileReaderNative>()
            .is_some_and(|native| {
                native.ready_state == DONE
                    && match kind {
                        TerminalKind::Load | TerminalKind::Abort => true,
                        TerminalKind::Error => native.error.is_some(),
                    }
            });
        if !settled {
            return Ok(JsValue::undefined());
        }
    } else {
        // Non-terminal events only fire for the live generation.
        // `loadstart`/`progress` require LOADING; `loadend` requires that
        // the terminal event of this generation already dispatched.
        let live = reader
            .downcast_ref::<FileReaderNative>()
            .is_some_and(|native| {
                if event_type == "loadend" {
                    native.terminal_dispatched
                } else {
                    native.ready_state == LOADING && !native.terminal_dispatched
                }
            });
        if !live {
            return Ok(JsValue::undefined());
        }
    }

    let event = dom::create_progress_event(
        &specs,
        &event_type,
        loaded,
        total,
        reader.clone(),
        time_stamp,
    );
    let event_value = JsValue::from(event);
    let listeners = reader_listeners(reader);
    let first_error = dom::invoke_event(reader, &listeners, &event_type, &event_value, context)?;

    if terminal.is_some() {
        // Mark the terminal event; a reentrant handler may already have
        // replaced the generation (checked below for `loadend`).
        if let Some(mut native) = reader.downcast_mut::<FileReaderNative>()
            && native.generation == generation
        {
            native.terminal_dispatched = true;
        }
        // Conditional `loadend`: suppressed only when a reentrant handler
        // started a new operation during this dispatch. Queued before the
        // error reporter so the event order survives a throwing listener.
        if generation_current(reader, generation) {
            enqueue_reading_job(
                context,
                FileReadingJob {
                    reader: reader.clone(),
                    generation,
                    operation,
                    step: JobStep::Dispatch(DispatchState {
                        event_type: String::from("loadend"),
                        loaded,
                        total,
                        terminal: None,
                    }),
                },
            );
        }
    }
    if let Some(error) = first_error {
        // Listener exceptions surface as JS job errors without stopping the
        // remaining listeners (which already ran) or the queued `loadend`.
        enqueue_listener_error(context, error.to_string());
    }
    Ok(JsValue::undefined())
}

/// Polls one FileReader operation without a worker round-trip.
///
/// Compatibility entry for hosts/tests that drive only `run_jobs()`.
/// Currently unused by production (first submits come from `start_read`
/// and `poll_io`); kept as the documented fallback entry.
#[allow(dead_code)]
pub(crate) fn poll_reader_once(context: &mut Context, operation: u64) -> bool {
    let Ok(stored) = crate::extension::snapshot(context) else {
        return false;
    };
    if stored.shutdown.is_shutdown() {
        return false;
    }
    let Some(generation) = context
        .get_data::<PendingReaderOps>()
        .and_then(|table| table.ops.get(&operation))
        .map(|pending| pending.generation)
    else {
        return false;
    };
    // No chunk in flight yet: the persisted state exists, nothing was
    // submitted after `loadstart`, and nothing was consumed.
    let ready = context
        .host_defined_mut()
        .get_mut::<PumpStates>()
        .and_then(|table| table.states.get(&operation))
        .is_some_and(|state| !state.loadstart_sent && state.loaded == 0);
    if !ready {
        return false;
    }
    submit_first_chunk(&stored, operation, generation, context).is_ok()
}

/// Returns the bridge behind `context`'s registration.
fn bridge_of(context: &Context) -> JsResult<std::sync::Arc<crate::io::IoBridge>> {
    Ok(crate::extension::snapshot(context)?.io_bridge())
}

/// Submits the next chunk request for `operation`.
///
/// Exactly one request per drained chunk: `[loaded, loaded+chunk)` clamped
/// to `total`. The worker reads only that range off-thread; no readahead
/// and no accumulation past the returned `PumpState` happen here. A submit
/// failure fails the operation inline through the terminal error path
/// (quota released exactly once, no second pump enqueued).
fn submit_next_chunk(
    reader: &JsObject,
    operation: u64,
    generation: u64,
    state: &PumpState,
    context: &mut Context,
) -> JsResult<()> {
    let bridge = bridge_of(context)?;
    let operation_id = crate::io::FileIoOperationId::from_raw(operation);
    let Some(token) = bridge.token_for(operation_id) else {
        // Reservation already gone (abort/restart/shutdown won the race):
        // become a strict no-op instead of submitting.
        return Ok(());
    };
    let remaining = state.total.saturating_sub(state.loaded);
    let len = (state.chunk_size as u64).min(remaining);
    let specs = crate::extension::snapshot(context)?;
    let data = specs
        .reader_payload(operation)
        .ok_or_else(|| type_error("the FileReader operation is unavailable"))?;
    let task = bridge.chunk_task_for(
        operation_id,
        token,
        data,
        state.limits.clone(),
        crate::io::ChunkWindow {
            generation,
            offset: state.loaded,
            len,
        },
    );
    if let Err(error) = bridge.submit_reader_guarded(task) {
        let mapped = match error {
            crate::io::FileIoSubmitError::WorkerLost => FileApiError::Internal,
            crate::io::FileIoSubmitError::QueueFull | crate::io::FileIoSubmitError::Shutdown => {
                FileApiError::TooManyReads
            }
        };
        fail_operation(reader, operation, generation, state.total, &mapped, context)?;
    }
    Ok(())
}

/// Persists the packaging state for the next drained chunk.
fn store_pump_state(context: &mut Context, operation: u64, state: PumpState) {
    if context.get_data::<PumpStates>().is_none() {
        let _ = context.insert_data::<PumpStates>(PumpStates::default());
    }
    if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
        table.states.insert(operation, state);
    }
}

/// Takes the persisted packaging state for `operation`.
fn take_pump_state(context: &mut Context, operation: u64) -> Option<PumpState> {
    context
        .host_defined_mut()
        .get_mut::<PumpStates>()
        .and_then(|table| table.states.remove(&operation))
}

/// Per-context persisted FileReader packaging states, keyed by operation.
///
/// `PumpState` holds only Rust data (bytes/text/decoder/limits); the
/// reader root lives in `PendingReaderOps`. Both tables are dropped
/// together at terminal settlement, abort, or shutdown drain.
#[derive(Default)]
struct PumpStates {
    states: std::collections::HashMap<u64, PumpState>,
}

/// Settles one drained FileReader chunk completion from `poll_io`.
///
/// Boa thread only. Validates the pending root first: a completion for an
/// unknown operation (abort/restart/shutdown already removed it) or a
/// generation mismatch is dropped as stale with no JS mutation, no event,
/// no telemetry, and no second quota release. Otherwise applies the chunk
/// through one pump Boa job (which may submit exactly one next chunk
/// request) and returns `1` when a job was enqueued.
pub(crate) fn settle_reader_completion(
    stored: &crate::extension::RegisteredSpecs,
    completion: crate::io::FileReaderChunkCompletion,
    context: &mut Context,
) -> JsResult<usize> {
    let operation = completion.operation_id().get();
    let generation = completion.generation();
    let kind = completion.into_kind();
    let Some(pending) = context
        .get_data::<PendingReaderOps>()
        .and_then(|table| table.ops.get(&operation))
    else {
        // Unknown operation: abort/restart/shutdown already released the
        // slot. Strict no-op.
        return Ok(0);
    };
    if pending.generation != generation {
        return Ok(0);
    }
    if stored.shutdown.is_shutdown() {
        return Ok(0);
    }
    let reader = pending.reader.clone();
    if !generation_current(&reader, generation) {
        return Ok(0);
    }
    let Some(state) = take_pump_state(context, operation) else {
        return Ok(0);
    };
    // Route the chunk through the single pump entry; it owns packaging,
    // progress, reentrancy, and the exactly-one-next-submit rule.
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
            operation,
            step: JobStep::PumpChunk {
                state: Box::new(state),
                chunk: Some(kind),
            },
        },
    );
    Ok(1)
}

/// Submits first chunk requests for every live operation without one.
///
/// Kept as the documented lazy-submit entry: the eager first submit in
/// `start_read` made it dead, but the operation model names it. The pump
/// applies drained chunks after `loadstart` rechecks the generation, so a
/// reentrant `loadstart`-time `abort()` still drops the chunk unread.
#[allow(dead_code)]
pub(crate) fn submit_first_chunks(
    stored: &crate::extension::RegisteredSpecs,
    context: &mut Context,
) {
    let pending: Vec<(u64, u64)> = context
        .get_data::<PendingReaderOps>()
        .map(|table| {
            table
                .ops
                .iter()
                .map(|(operation, pending)| (*operation, pending.generation))
                .collect()
        })
        .unwrap_or_default();
    for (operation, generation) in pending {
        let _ = submit_first_chunk(stored, operation, generation, context);
    }
}

/// Submits the first chunk request of one operation.
///
/// Extracted so the batch entry above stays a small loop: liveness,
/// reservation, payload, and submit-failure handling live here.
fn submit_first_chunk(
    stored: &crate::extension::RegisteredSpecs,
    operation: u64,
    generation: u64,
    context: &mut Context,
) -> JsResult<()> {
    let bridge = stored.io_bridge();
    let operation_id = crate::io::FileIoOperationId::from_raw(operation);
    let Some(token) = bridge.token_for(operation_id) else {
        return Ok(());
    };
    let snapshot = context
        .host_defined_mut()
        .get_mut::<PumpStates>()
        .and_then(|table| table.states.get(&operation))
        .map(|state| {
            (
                state.total,
                state.chunk_size,
                state.limits.clone(),
                state.loadstart_sent,
                state.loaded,
            )
        });
    let Some((total, chunk_size, limits, loadstart_sent, loaded)) = snapshot else {
        return Ok(());
    };
    if loadstart_sent || loaded != 0 {
        return Ok(());
    }
    let reader = context
        .get_data::<PendingReaderOps>()
        .and_then(|table| table.ops.get(&operation))
        .map(|pending| pending.reader.clone());
    let Some(reader) = reader else {
        return Ok(());
    };
    if !generation_current(&reader, generation) {
        return Ok(());
    }
    let Some(data) = stored.reader_payload(operation) else {
        return Ok(());
    };
    let len = (chunk_size as u64).min(total);
    let task = bridge.chunk_task_for(
        operation_id,
        token,
        data,
        limits,
        crate::io::ChunkWindow {
            generation,
            offset: 0,
            len,
        },
    );
    if let Err(error) = bridge.submit_reader_guarded(task) {
        let mapped = match error {
            crate::io::FileIoSubmitError::WorkerLost => FileApiError::Internal,
            crate::io::FileIoSubmitError::QueueFull | crate::io::FileIoSubmitError::Shutdown => {
                FileApiError::TooManyReads
            }
        };
        // Reservation is live (we hold no other reference): release it
        // exactly once, then run the terminal error path inline.
        bridge.unreserve(operation_id);
        stored.drop_reader_payload(operation);
        remove_pending_reader(context, operation);
        if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
            table.states.remove(&operation);
        }
        release_slot(context)?;
        fail_operation_no_release(&reader, generation, total, &mapped, context)?;
    }
    Ok(())
}

/// Drops the pending root and packaging state for `operation`.
///
/// Shutdown path: called from `poll_io` after the bridge reservation is
/// released, so the quota accounting stays exactly-once and no late pump
/// can settle.
pub(crate) fn drop_pending_for_shutdown(context: &mut Context, operation: u64) {
    remove_pending_reader(context, operation);
    if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
        table.states.remove(&operation);
    }
    let _ = queue_mut(context).map(|queue| {
        queue.active = queue.active.saturating_sub(1);
    });
}

/// Releases the quota slot after a successful terminal settlement.
fn settle_success_release(
    reader: &JsObject,
    operation: u64,
    generation: u64,
    total: u64,
    context: &mut Context,
) -> JsResult<()> {
    let _ = (reader, generation, total);
    if take_pending_reader(context, operation).is_some() {
        let bridge = bridge_of(context)?;
        bridge.unreserve(crate::io::FileIoOperationId::from_raw(operation));
        release_slot(context)?;
        if let Ok(specs) = crate::extension::snapshot(context) {
            specs.drop_reader_payload(operation);
        }
    }
    if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
        table.states.remove(&operation);
    }
    Ok(())
}

/// Releases the quota slot after a terminal error settlement.
fn settle_error_release(
    reader: &JsObject,
    operation: u64,
    generation: u64,
    context: &mut Context,
) -> JsResult<()> {
    let _ = (reader, generation);
    if take_pending_reader(context, operation).is_some() {
        let bridge = bridge_of(context)?;
        bridge.unreserve(crate::io::FileIoOperationId::from_raw(operation));
        release_slot(context)?;
        if let Ok(specs) = crate::extension::snapshot(context) {
            specs.drop_reader_payload(operation);
        }
    }
    if let Some(table) = context.host_defined_mut().get_mut::<PumpStates>() {
        table.states.remove(&operation);
    }
    Ok(())
}

/// Computes the telemetry chunk count for a successful read.
///
/// Counts whole chunks (`total.div_ceil(chunk_size)`); empty reads report
/// `0`. The count is informational only, never part of settlement.
#[cfg(feature = "tracing")]
fn buffered_chunk_count(total: u64, chunk_size: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let size = chunk_size.max(1);
    total.div_ceil(size)
}

/// Terminal error path for a submit failure at `readAs*` time.
///
/// Kept as the documented inline entry: the lazy first-submit path in
/// `poll_io` releases the reservation itself and reuses
/// `fail_operation_no_release`, so this wrapper stays as the explicit
/// reference the operation model names.
#[allow(dead_code)]
fn fail_operation_inline(
    reader: &JsObject,
    operation: crate::io::FileIoOperationId,
    generation: u64,
    total: u64,
    error: &FileApiError,
    context: &mut Context,
) -> JsResult<JsValue> {
    // The reservation is already released by the caller; only the mirror
    // counter and the pending root need cleanup before the error dispatch.
    remove_pending_reader(context, operation.get());
    release_slot(context)?;
    // Reuse the settled error path without touching the bridge again.
    fail_operation_no_release(reader, generation, total, error, context)
}

/// Error terminal path that never touches the bridge reservation.
///
/// Used exactly once: the submit-failure inline path, where the caller
/// already released the reservation. Every other terminal path goes
/// through `fail_operation` (bridge + mirror release).
fn fail_operation_no_release(
    reader: &JsObject,
    generation: u64,
    total: u64,
    error: &FileApiError,
    context: &mut Context,
) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    #[cfg(feature = "tracing")]
    let trace_class = crate::observability::result_class_for_core(Some(error));
    let (name, message) = dom::map_core_error(error);
    {
        let mut native = reader
            .downcast_mut::<FileReaderNative>()
            .ok_or_else(|| type_error("illegal invocation: expected a FileReader"))?;
        if native.generation != generation {
            return Ok(JsValue::undefined());
        }
        native.ready_state = DONE;
        native.result = None;
        native.error = Some(ErrorParts {
            name: name.to_owned(),
            message: message.to_owned(),
        });
        native.terminal_dispatched = false;
    }
    #[cfg(feature = "tracing")]
    {
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        crate::observability::emit(
            "filereader_read",
            total,
            crate::observability::elapsed_ms(trace_start),
            0,
            trace_class,
            env,
        );
    }
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
            operation: u64::MAX,
            step: JobStep::Dispatch(DispatchState {
                event_type: String::from("error"),
                loaded: 0,
                total,
                terminal: Some(TerminalKind::Error),
            }),
        },
    );
    Ok(JsValue::undefined())
}

/// Enqueues a job that reports a listener exception as a JS job error.
///
/// The job throws a plain `Error` carrying the stashed message, so the
/// failure is visible to `run_jobs()` callers. It runs after every already
/// queued event job (including `loadend`), preserving the event order.
fn enqueue_listener_error(context: &mut Context, message: String) {
    use boa_engine::job::GenericJob;
    let realm = context.realm().clone();
    let generic = GenericJob::new(
        move |_context: &mut Context| -> JsResult<JsValue> {
            Err(crate::error::type_error(&message))
        },
        realm,
    );
    context.enqueue_job(Job::from(generic));
}

/// The `readyState` getter.
fn ready_state_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_reader(this)?;
    let Some(native) = object.downcast_ref::<FileReaderNative>() else {
        return Err(type_error("illegal invocation: expected a FileReader"));
    };
    let state = native.ready_state;
    drop(native);
    Ok(JsValue::from(f64::from(state)))
}

/// The `result` getter: `null`, a DOMString, or a fresh `ArrayBuffer`.
fn result_getter(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = require_reader(this)?;
    let Some(native) = object.downcast_ref::<FileReaderNative>() else {
        return Err(type_error("illegal invocation: expected a FileReader"));
    };
    let result = native.result.clone();
    drop(native);
    match result {
        None => Ok(JsValue::null()),
        Some(FileReaderResult::Text(text) | FileReaderResult::BinaryString(text)) => {
            Ok(JsValue::from(JsString::from(text)))
        }
        Some(FileReaderResult::Bytes(bytes)) => {
            let buffer = JsArrayBuffer::new(bytes.len(), context)?;
            buffer
                .data_mut()
                .as_deref_mut()
                .ok_or_else(|| type_error("fresh ArrayBuffer is detached"))?
                .copy_from_slice(&bytes);
            Ok(buffer.into())
        }
    }
}

/// The `error` getter: `null` or a same-realm `DOMException`.
fn error_getter(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = require_reader(this)?;
    let specs = crate::extension::snapshot(context)?;
    let dom = specs
        .dom_specs()
        .ok_or_else(|| type_error("the DOM shim is not registered"))?;
    let Some(native) = object.downcast_ref::<FileReaderNative>() else {
        return Err(type_error("illegal invocation: expected a FileReader"));
    };
    let error = native.error.clone();
    drop(native);
    match error {
        None => Ok(JsValue::null()),
        Some(parts) => Ok(JsValue::from(dom::construct_exception(
            &dom,
            &parts.name,
            &parts.message,
        ))),
    }
}

/// Registers the `FileReader` prototype members.
fn init_filereader_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method, length) in [
        (
            js_string!("readAsArrayBuffer"),
            NativeFunction::from_fn_ptr(read_as_array_buffer),
            1,
        ),
        (
            js_string!("readAsBinaryString"),
            NativeFunction::from_fn_ptr(read_as_binary_string),
            1,
        ),
        (
            js_string!("readAsText"),
            NativeFunction::from_fn_ptr(read_as_text),
            1,
        ),
        (
            js_string!("readAsDataURL"),
            NativeFunction::from_fn_ptr(read_as_data_url),
            1,
        ),
        (js_string!("abort"), NativeFunction::from_fn_ptr(abort), 0),
    ] {
        let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), method)
            .name(name.clone())
            .length(length)
            .constructor(false)
            .build();
        prototype.define_property_or_throw(
            name,
            PropertyDescriptor::builder()
                .value(function)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }

    // readonly `readyState`, `result`, `error`.
    for (key, getter) in [
        (
            js_string!("readyState"),
            NativeFunction::from_fn_ptr(ready_state_getter),
        ),
        (
            js_string!("result"),
            NativeFunction::from_fn_ptr(result_getter),
        ),
        (
            js_string!("error"),
            NativeFunction::from_fn_ptr(error_getter),
        ),
    ] {
        let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), getter)
            .name(key.clone())
            .length(0)
            .constructor(false)
            .build();
        prototype.define_property_or_throw(
            key,
            PropertyDescriptor::builder()
                .get(function)
                .enumerable(true)
                .configurable(true),
            context,
        )?;
    }

    // Writable `on*` handler properties, initially `null`.
    for name in [
        "onloadstart",
        "onprogress",
        "onabort",
        "onerror",
        "onload",
        "onloadend",
    ] {
        prototype.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(JsValue::null())
                .writable(true)
                .enumerable(true)
                .configurable(true),
            context,
        )?;
    }

    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("FileReader"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Defines the `EMPTY`/`LOADING`/`DONE` constants on constructor and
/// prototype (readonly: writable `false`, enumerable `true`, configurable
/// `false`, per Web IDL constants on both objects).
fn init_filereader_constants(specs: &StandardConstructor, context: &mut Context) -> JsResult<()> {
    for (name, value) in [("EMPTY", EMPTY), ("LOADING", LOADING), ("DONE", DONE)] {
        for target in [specs.constructor(), specs.prototype()] {
            target.define_property_or_throw(
                js_string!(name),
                PropertyDescriptor::builder()
                    .value(f64::from(value))
                    .writable(false)
                    .enumerable(true)
                    .configurable(false),
                context,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Child-module proof of the source-failure and stale-generation paths
    //! through the real `run_pump` → `Context::run_jobs()` machinery with
    //! controlled `ByteSource`s.
    //!
    //! Memory-backed JS blobs can never fail a source read (ranges are
    //! validated up front), so short/long/failing sources are unreachable
    //! via public constructors. These source types exist only in this test
    //! module: no production hook, no public arbitrary-source API. Each
    //! test wraps the failing payload in a real branded JS `Blob`, reads it
    //! with a real branded JS `FileReader`, and asserts only JS-observable
    //! state and events after `run_jobs()`.
    //!
    //! Prototype identity cannot be proven from Rust alone, so every test
    //! attaches JavaScript handlers in the same `Context` and reads back
    //! the observable log afterwards.

    use super::*;
    use boa_engine::{Source, js_string};
    use boa_fapi_core::cancellation::CancellationToken;
    use boa_fapi_core::limits::FileApiLimits;
    use boa_fapi_core::snapshot::SnapshotState;
    use boa_fapi_core::source::ByteSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A source that counts `read_range` calls and serves exact bytes.
    ///
    /// Counts worker `read_range` invocations (M9-C workers are the only
    /// source readers): the `loadstart`-abort tests assert the drained
    /// chunk is dropped unread by the Boa job, while the M9-C behavioral
    /// guard (`m9_filereader_io`) proves no *Boa job* performs the
    /// blocking read.
    struct CountingSource {
        data: bytes::Bytes,
        reads: Arc<AtomicUsize>,
    }

    impl ByteSource for CountingSource {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn snapshot(&self) -> SnapshotState {
            SnapshotState::Memory
        }
        fn read_range(
            &self,
            range: std::ops::Range<u64>,
            _cancel: &CancellationToken,
        ) -> Result<bytes::Bytes, FileApiError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let start = usize::try_from(range.start).map_err(|_| FileApiError::InvalidRange)?;
            let end = usize::try_from(range.end).map_err(|_| FileApiError::InvalidRange)?;
            self.data
                .get(start..end)
                .map(bytes::Bytes::copy_from_slice)
                .ok_or(FileApiError::InvalidRange)
        }
    }

    /// A source that fails every read with `FileLocked`
    /// (→ `NotReadableError`).
    struct FailSource {
        len: u64,
    }

    impl ByteSource for FailSource {
        fn len(&self) -> u64 {
            self.len
        }
        fn snapshot(&self) -> SnapshotState {
            SnapshotState::Memory
        }
        fn read_range(
            &self,
            _range: std::ops::Range<u64>,
            _cancel: &CancellationToken,
        ) -> Result<bytes::Bytes, FileApiError> {
            Err(FileApiError::FileLocked)
        }
    }

    /// A source declaring 5 bytes but returning 4 (short response →
    /// `InvalidRange` → `NotReadableError`).
    struct ShortSource;

    impl ByteSource for ShortSource {
        fn len(&self) -> u64 {
            5
        }
        fn snapshot(&self) -> SnapshotState {
            SnapshotState::Memory
        }
        fn read_range(
            &self,
            _range: std::ops::Range<u64>,
            _cancel: &CancellationToken,
        ) -> Result<bytes::Bytes, FileApiError> {
            Ok(bytes::Bytes::copy_from_slice(b"shor"))
        }
    }

    /// A source declaring 5 bytes but returning 6 (long response →
    /// `InvalidRange` → `NotReadableError`).
    struct LongSource;

    impl ByteSource for LongSource {
        fn len(&self) -> u64 {
            5
        }
        fn snapshot(&self) -> SnapshotState {
            SnapshotState::Memory
        }
        fn read_range(
            &self,
            _range: std::ops::Range<u64>,
            _cancel: &CancellationToken,
        ) -> Result<bytes::Bytes, FileApiError> {
            Ok(bytes::Bytes::copy_from_slice(b"toolong!"))
        }
    }

    /// Deterministic clock for tests.
    #[derive(Debug)]
    struct TestClock {
        millis: i64,
    }

    impl crate::clock::Clock for TestClock {
        fn now_unix_millis(&self) -> i64 {
            self.millis
        }
    }

    const TEST_TIME: i64 = 1_700_000_000_000;

    /// Registers the extension with the given limits.
    fn setup_with_limits(limits: FileApiLimits) -> Context {
        let mut context = Context::default();
        crate::extension::FileApiExtension::builder()
            .clock(Arc::new(TestClock { millis: TEST_TIME }))
            .limits(limits)
            .build()
            .register(&mut context)
            .expect("registration failed");
        context
    }

    /// Single-slot limits: any quota leak blocks the very next read, so a
    /// follow-up success proves exact slot release.
    fn quota_one_limits() -> FileApiLimits {
        FileApiLimits {
            max_concurrent_reads_per_global: 1,
            ..FileApiLimits::default()
        }
    }

    /// Wraps `data` in a real branded JS `Blob` reachable as `srcBlob`.
    fn publish_blob(context: &mut Context, data: Arc<BlobData>) {
        let specs = crate::extension::snapshot(context).expect("registered");
        let blob =
            crate::blob::create_instance(crate::blob::BlobNative::new(data), specs.blob_proto());
        context
            .register_global_property(
                js_string!("srcBlob"),
                blob,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish blob");
    }

    /// Creates a real branded JS `FileReader` reachable as `reader`.
    fn publish_reader(context: &mut Context) {
        let specs = crate::extension::snapshot(context).expect("registered");
        let reader =
            JsObject::from_proto_and_data(specs.filereader_proto(), FileReaderNative::fresh());
        context
            .register_global_property(
                js_string!("reader"),
                reader,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish reader");
    }

    /// Attaches logging listeners for all six event types and starts
    /// `readAsArrayBuffer(srcBlob)`. Each entry records
    /// `type:readyState:resultKind:errorName`.
    fn start_logged_read(context: &mut Context) {
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 for (var t of ['loadstart','progress','load','error','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function () { \
                             globalThis.log.push(tt + ':' + this.readyState + ':' \
                                 + (this.result === null ? 'null' : typeof this.result) + ':' \
                                 + (this.error === null ? 'null' : this.error.name)); \
                         }; \
                     })(t)); \
                 reader.readAsArrayBuffer(srcBlob);",
            ))
            .expect("start read");
    }

    /// Reads back the JS event log.
    fn js_log(context: &mut Context) -> String {
        context
            .eval(Source::from_bytes("globalThis.log.join('|')"))
            .expect("read log")
            .as_string()
            .expect("log string")
            .to_std_string_escaped()
    }

    /// Reads back `readyState:resultKind:errorName`.
    fn js_state(context: &mut Context) -> String {
        context
            .eval(Source::from_bytes(
                "reader.readyState + ':' \
                 + (reader.result === null ? 'null' : typeof reader.result) + ':' \
                 + (reader.error === null ? 'null' : reader.error.name)",
            ))
            .expect("read state")
            .to_string(context)
            .expect("state string")
            .to_std_string_escaped()
    }

    /// Drives jobs to quiescence.
    ///
    /// M9-C host loop for unit tests: `poll_io` drains worker chunks into
    /// pump jobs, then `run_jobs` delivers them. The default threaded
    /// executor runs separately, so the loop repeats until quiescent.
    /// `poll_io` is strictly non-blocking: the loop spins the host-side
    /// drain (no sleep, no yield) while I/O is still outstanding.
    fn drain(context: &mut Context) {
        let handle = crate::extension::snapshot(context)
            .expect("registered")
            .test_handle();
        for _ in 0..200 {
            let settled = handle.poll_io(context).unwrap_or(0);
            context.run_jobs().expect("run_jobs");
            if settled == 0 && !handle.has_pending_io() {
                context.run_jobs().expect("run_jobs");
                if !handle.has_pending_io() {
                    break;
                }
            }
            if handle.has_pending_io() {
                for _ in 0..50 {
                    let _ = handle.poll_io(context);
                    context.run_jobs().expect("run_jobs");
                    if !handle.has_pending_io() {
                        break;
                    }
                }
            }
        }
    }

    /// Builds a `BlobData` over `source` with the default limits.
    fn blob_over(source: Arc<dyn ByteSource>, len: u64) -> Arc<BlobData> {
        Arc::new(
            BlobData::from_segments(
                vec![boa_fapi_core::blob::BlobSegment {
                    source,
                    offset: 0,
                    len,
                }],
                "",
                &FileApiLimits::default(),
            )
            .expect("valid segments"),
        )
    }

    /// A follow-up successful read proves the failed operation released its
    /// single quota slot (with quota-one limits any leak blocks it).
    fn assert_quota_recovered(context: &mut Context) {
        context
            .eval(Source::from_bytes(
                "globalThis.after = null; \
                 var r2 = new FileReader(); \
                 r2.onload = function () { globalThis.after = this.result; }; \
                 r2.onerror = function () { globalThis.after = 'unexpected-error'; }; \
                 r2.readAsText(new Blob(['ok']));",
            ))
            .expect("follow-up read");
        drain(context);
        let verdict = context
            .eval(Source::from_bytes("globalThis.after"))
            .expect("aftermath")
            .as_string()
            .expect("aftermath string")
            .to_std_string_escaped();
        assert_eq!(verdict, "ok", "quota slot must be released after failure");
    }

    #[test]
    fn short_source_response_fails_as_not_readable_error() {
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(Arc::new(ShortSource), 5));
        publish_reader(context);
        start_logged_read(context);
        drain(context);
        assert_eq!(
            js_log(context),
            "loadstart:1:null:null|error:2:null:NotReadableError|loadend:2:null:NotReadableError"
        );
        assert_eq!(js_state(context), "2:null:NotReadableError");
        assert_quota_recovered(context);
    }

    #[test]
    fn long_source_response_fails_as_not_readable_error() {
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(Arc::new(LongSource), 5));
        publish_reader(context);
        start_logged_read(context);
        drain(context);
        assert_eq!(
            js_log(context),
            "loadstart:1:null:null|error:2:null:NotReadableError|loadend:2:null:NotReadableError"
        );
        assert_eq!(js_state(context), "2:null:NotReadableError");
        assert_quota_recovered(context);
    }

    #[test]
    fn failing_source_fails_as_not_readable_error() {
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(Arc::new(FailSource { len: 3 }), 3));
        publish_reader(context);
        start_logged_read(context);
        drain(context);
        assert_eq!(
            js_log(context),
            "loadstart:1:null:null|error:2:null:NotReadableError|loadend:2:null:NotReadableError"
        );
        assert_eq!(js_state(context), "2:null:NotReadableError");
        assert_quota_recovered(context);
    }

    #[test]
    fn loadstart_abort_performs_no_source_read() {
        // A `loadstart` handler aborts synchronously: the drained worker
        // chunk is dropped unread by the Boa job (no progress, no
        // packaging, no further submit). The worker itself already
        // produced the first chunk off-thread — that is the M9-C design
        // (no Boa job reads the source); the assertion below counts the
        // worker call that the pump refused to consume.
        let reads = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource {
            data: bytes::Bytes::copy_from_slice(b"abc"),
            reads: Arc::clone(&reads),
        });
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(source, 3));
        publish_reader(context);
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 reader.addEventListener('loadstart', function () { \
                     globalThis.log.push('loadstart'); \
                     this.abort(); \
                 }); \
                 for (var t of ['progress','load','error','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function () { globalThis.log.push(tt); }; \
                     })(t)); \
                 reader.readAsArrayBuffer(srcBlob);",
            ))
            .expect("start read");
        drain(context);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "worker produced the first chunk off-thread; the stale pump must drop it unread"
        );
        assert_eq!(js_log(context), "loadstart|abort|loadend");
        assert_eq!(js_state(context), "2:null:null");
        assert_quota_recovered(context);
    }

    #[test]
    fn loadstart_abort_then_restart_emits_only_new_operation() {
        // `loadstart` handler aborts and immediately starts a new read: the
        // old abort dispatch is stale (generation replaced before delivery)
        // and emits nothing; only the new operation's events follow. The
        // old worker chunk is dropped unread by the Boa job.
        let reads = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource {
            data: bytes::Bytes::copy_from_slice(b"old"),
            reads: Arc::clone(&reads),
        });
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(source, 3));
        publish_reader(context);
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 globalThis.good = new Blob(['new']); \
                 reader.addEventListener('loadstart', function () { \
                     globalThis.log.push('loadstart'); \
                     if (globalThis.armed !== false) { \
                         globalThis.armed = false; \
                         this.abort(); \
                         this.readAsText(globalThis.good); \
                     } \
                 }); \
                 for (var t of ['progress','load','error','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function () { globalThis.log.push(tt); }; \
                     })(t)); \
                 globalThis.armed = true; \
                 reader.readAsArrayBuffer(srcBlob);",
            ))
            .expect("start read");
        drain(context);
        drain(context);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "old worker produced one chunk off-thread; the stale pump drops it unread"
        );
        assert_eq!(
            js_log(context),
            "loadstart|loadstart|progress|load|loadend",
            "old abort/loadend are stale and emit nothing"
        );
        let result = context
            .eval(Source::from_bytes("reader.result"))
            .expect("result")
            .as_string()
            .expect("result string")
            .to_std_string_escaped();
        assert_eq!(result, "new");
        assert_quota_recovered(context);
    }

    #[test]
    fn progress_abort_freezes_source_reads() {
        // Multichunk blob (3 x 16 KiB): aborting in the first progress
        // handler submits no further chunk request and emits no further
        // events for the old generation. The worker produced exactly the
        // first chunk off-thread; the second `read_range` below comes from
        // the quota-recovery follow-up read (`assert_quota_recovered`
        // drains a fresh one-chunk memory read), not from the aborted
        // operation. The JS log pins the freeze: no second `progress`.
        let reads = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource {
            data: bytes::Bytes::from(vec![7u8; 3 * 16384]),
            reads: Arc::clone(&reads),
        });
        let limits = FileApiLimits {
            default_chunk_size: 16 * 1024,
            ..FileApiLimits::default()
        };
        let context = &mut setup_with_limits(limits);
        publish_blob(context, blob_over(source, 3 * 16384));
        publish_reader(context);
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 for (var t of ['loadstart','progress','load','error','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function (e) { \
                             globalThis.log.push(tt + ':' + e.loaded); \
                             if (tt === 'progress') this.abort(); \
                         }; \
                     })(t)); \
                 reader.readAsArrayBuffer(srcBlob);",
            ))
            .expect("start read");
        drain(context);
        drain(context);
        // Exactly the first chunk was produced off-thread; the aborting
        // progress handler submitted nothing further (the JS log has no
        // second `progress`). The count below is read before any
        // quota-recovery follow-up, so it pins the freeze exactly.
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "no chunk may be produced after the aborting progress handler"
        );
        assert_eq!(
            js_log(context),
            "loadstart:0|progress:16384|abort:16384|loadend:16384"
        );
        assert_eq!(js_state(context), "2:null:null");
    }

    #[test]
    fn error_handler_restart_suppresses_old_loadend() {
        // Reentrant `error` case: the failing operation's `error` handler
        // starts a new read. Only the old `loadend` is suppressed; the new
        // operation completes intact and the quota is fully recovered.
        let context = &mut setup_with_limits(quota_one_limits());
        publish_blob(context, blob_over(Arc::new(FailSource { len: 3 }), 3));
        publish_reader(context);
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 globalThis.good = new Blob(['ok']); \
                 reader.addEventListener('error', function () { \
                     globalThis.log.push('error'); \
                     this.readAsText(globalThis.good); \
                 }); \
                 for (var t of ['loadstart','progress','load','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function () { globalThis.log.push(tt); }; \
                     })(t)); \
                 reader.readAsArrayBuffer(srcBlob);",
            ))
            .expect("start read");
        drain(context);
        drain(context);
        assert_eq!(
            js_log(context),
            "loadstart|error|loadstart|progress|load|loadend",
            "old loadend is suppressed, new operation completes"
        );
        let result = context
            .eval(Source::from_bytes("reader.result"))
            .expect("result")
            .as_string()
            .expect("result string")
            .to_std_string_escaped();
        assert_eq!(result, "ok");
        assert_eq!(js_state(context), "2:string:null");
        assert_quota_recovered(context);
    }
}
