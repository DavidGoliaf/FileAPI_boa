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
use boa_fapi_core::blob::{BlobData, BlobReader};
use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::dom::{self, ListEntry};
use crate::error::type_error;
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
/// The job owns its data by value (jobs are `FnOnce`): the incremental core
/// reader and the text decoder travel from job to job without cloning, so
/// chunk and decoder state is never lost or re-read.
struct FileReadingJob {
    /// The reader object this job belongs to (traced GC root in the capture).
    reader: JsObject,
    /// The generation this job belongs to; stale jobs are strict no-ops.
    generation: u64,
    /// The step this job performs.
    step: JobStep,
}

/// The step a FileReading job performs.
enum JobStep {
    /// Pump one chunk of the read (or finish an empty blob).
    Pump(PumpState),
    /// Dispatch one event for the current generation.
    Dispatch(DispatchState),
}

/// Owned pump state travelling from job to job.
struct PumpState {
    /// Total bytes of the operation (for progress events).
    total: u64,
    /// Incremental core reader; one `read_next()` per job, never ahead.
    reader_core: BlobReader,
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

/// Supported text encodings for `readAsText`: the Encoding Standard label
/// resolved through the fixed `encoding_rs` dependency.
#[derive(Clone, Copy, Debug)]
struct TextEncoding {
    /// The resolved Encoding Standard encoding.
    encoding: &'static encoding_rs::Encoding,
    /// Whether the input starts with that encoding's BOM (UTF-8 only in
    /// this shim: `encoding_rs` strips the BOM when sniffing is enabled).
    strip_utf8_bom: bool,
}

/// Incremental decoder state for `readAsText`.
///
/// Wraps an `encoding_rs::Decoder`; `push` feeds one chunk and returns the
/// decoded prefix, `finish` flushes with `last = true`. Malformed sequences
/// decode with replacement, never as an exception. Split multibyte
/// sequences stay buffered inside the decoder, never emitted as U+FFFD
/// early. A leading UTF-8 BOM is stripped once (Encoding Standard BOM
/// handling) when the operation uses UTF-8 decoding.
struct IncrementalDecoder {
    /// The underlying `encoding_rs` decoder.
    decoder: Option<encoding_rs::Decoder>,
    /// Whether the decoder already finished.
    finished: bool,
    /// Whether the leading UTF-8 BOM was already consumed.
    bom_consumed: bool,
}

/// Per-`Context` FIFO FileReading task state: plain numbers only, no GC
/// pointers, so no tracing is required.
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

/// Resolves an encoding label.
///
/// Returns `None` for an unknown or unsupported label: the caller fails the
/// operation through the `error` path with `EncodingError` and no partial
/// result. `None` (absent label) defaults to UTF-8.
fn resolve_label(label: Option<&str>) -> Option<TextEncoding> {
    let Some(label) = label else {
        return Some(TextEncoding {
            encoding: encoding_rs::UTF_8,
            strip_utf8_bom: true,
        });
    };
    if label.trim().is_empty() {
        return Some(TextEncoding {
            encoding: encoding_rs::UTF_8,
            strip_utf8_bom: true,
        });
    }
    // `for_label_no_replacement` maps unknown labels and the `replacement`
    // encoding itself to `None`: both terminate with `EncodingError`.
    encoding_rs::Encoding::for_label_no_replacement(label.trim().as_bytes()).map(|encoding| {
        TextEncoding {
            encoding,
            strip_utf8_bom: encoding == encoding_rs::UTF_8,
        }
    })
}

/// Starts a read operation: the shared synchronous preamble.
///
/// Validates the brand and the Blob argument first (failures leave the
/// previous operation untouched). A `LOADING` reader throws a same-realm
/// `InvalidStateError` synchronously. Otherwise sets `(LOADING, null,
/// null)`, allocates a generation, reserves one quota slot (the 65th active
/// reader with the default limit fails as `SecurityError` through the
/// normal error path), snapshots the chunk ceiling, and enqueues the first
/// FileReading job.
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

    // Quota: the 65th active reader with the default limit fails through
    // the normal error path, consuming no slot.
    if active_count(context) >= limits.max_concurrent_reads_per_global.max(1) {
        return fail_fast(
            &object,
            data.size(),
            "SecurityError",
            "too many concurrent reads",
            context,
        );
    }

    let generation = queue_mut(context).map(|queue| {
        queue.active = queue.active.saturating_add(1);
        next_generation(queue)
    })?;
    let total = data.size();
    let reader_core = data
        .reader(&limits)
        .map_err(|_| type_error("the read chunk size is out of range"))?;
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
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: object,
            generation,
            step: JobStep::Pump(PumpState {
                total,
                reader_core,
                kind,
                encoding,
                media_type,
                data_url_limit: limits.max_data_url_output,
                loaded: 0,
                last_progress_at: i64::MIN,
                loadstart_sent: false,
                buffered: Vec::new(),
                text: String::new(),
                decoder: IncrementalDecoder {
                    decoder: None,
                    finished: false,
                    bom_consumed: false,
                },
                final_progress_sent: false,
            }),
        },
    );
    Ok(JsValue::undefined())
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
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: object.clone(),
            generation,
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
            strip_utf8_bom: true,
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
            strip_utf8_bom: true,
        },
        String::new(),
        context,
    )
}

/// `readAsText(blob, encoding?)`: `length = 1` (encoding optional).
fn read_as_text(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    // The encoding label converts before any state change, but after the
    // brand/argument checks so failures leave the reader untouched.
    let object = require_reader(this)?;
    let data = blob_arg(args)?;
    let label = if args.len() >= 2 && !args[1].is_undefined() {
        Some(dom_string(&args[1], context)?)
    } else {
        None
    };
    let Some(encoding) = resolve_label(label.as_deref()) else {
        // Unknown label: terminate through the `error` path with
        // `EncodingError` and no partial result.
        return fail_fast(
            &object,
            data.size(),
            "EncodingError",
            "unknown text encoding",
            context,
        );
    };
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
    // Checked Data-URL length: `data:<type>;base64,<payload>` must fit
    // `max_data_url_output` before any allocation. Base64 expands 3 bytes
    // to 4 characters: `((size + 2) / 3) * 4`.
    let size = data.size();
    let payload_len = size
        .checked_add(2)
        .and_then(|v| v.checked_div(3))
        .and_then(|v| v.checked_mul(4));
    let prefix_len = u64::try_from(media_type.len().saturating_add("data:;base64,".len())).ok();
    let total_len =
        payload_len.and_then(|payload| prefix_len.and_then(|prefix| payload.checked_add(prefix)));
    if total_len.is_none_or(|total| total > limits.max_data_url_output) {
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
            strip_utf8_bom: false,
        },
        media_type,
        context,
    )
}

/// `abort()`: `length = 0`.
///
/// In `EMPTY`/`DONE` sets `result = null`, returns `undefined`, alters no
/// `error` and queues no event. In `LOADING` invalidates the generation,
/// releases the quota slot once, sets `(DONE, null, null)`, then queues
/// `abort` followed conditionally by `loadend`.
fn abort(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
    let (generation, total, loaded) = {
        let (total, loaded) = object
            .downcast_ref::<FileReaderNative>()
            .map(|native| (native.total, native.loaded))
            .unwrap_or((0, 0));
        let queue = queue_mut(context)?;
        let generation = next_generation(queue);
        queue.active = queue.active.saturating_sub(1);
        (generation, total, loaded)
    };
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
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: object,
            generation,
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
/// slot, or emit an event.
fn run_reading_job(job: FileReadingJob, context: &mut Context) -> JsResult<JsValue> {
    let FileReadingJob {
        reader,
        generation,
        step,
    } = job;
    match step {
        JobStep::Pump(state) => run_pump(&reader, generation, state, context),
        JobStep::Dispatch(state) => run_dispatch(&reader, generation, state, context),
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

/// Pumps exactly one chunk of a read operation.
///
/// The first successful pump (including immediate EOF of an empty blob)
/// queues `loadstart`. Each chunk queues a throttled `progress`. At EOF the
/// job packages the result and dispatches `load` (+ conditional `loadend`)
/// through the terminal path. A source failure dispatches `error` (+
/// conditional `loadend`) with the mapped `DOMException` and no partial
/// result. Every terminal path releases exactly one quota slot; stale jobs
/// release none.
fn run_pump(
    reader: &JsObject,
    generation: u64,
    mut state: PumpState,
    context: &mut Context,
) -> JsResult<JsValue> {
    // Stale pump: strict no-op (no read, no mutation, no slot, no event).
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    let specs = crate::extension::snapshot(context)?;
    let clock = specs.config.clock.clone();
    let now = clock.now_unix_millis();

    // Read one configured chunk: at most one `read_next()` per job, never
    // ahead of the demand, never past an earlier queued reader. `loadstart`
    // dispatches synchronously inside this job (still within the task
    // source, never on the calling JS stack); the event carries this pump's
    // clock tick as its time stamp.
    if !state.loadstart_sent {
        state.loadstart_sent = true;
        dispatch_event_now(
            reader,
            generation,
            "loadstart",
            0,
            state.total,
            now as f64,
            context,
        )?;
    }
    match state.reader_core.read_next() {
        Err(error) => fail_operation(reader, generation, state.total, &error, context),
        Ok(None) => finish_at_eof(reader, generation, state, context),
        Ok(Some(chunk)) => {
            state.loaded = state
                .loaded
                .saturating_add(chunk.len() as u64)
                .min(state.total);
            match state.kind {
                ReadKind::ArrayBuffer | ReadKind::BinaryString | ReadKind::DataUrl => {
                    state.buffered.extend_from_slice(&chunk);
                }
                ReadKind::Text => {
                    let piece = state.decoder.push(&state.encoding, &chunk);
                    state.text.push_str(&piece);
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
            }
            if state.loaded >= state.total {
                return finish_at_eof(reader, generation, state, context);
            }
            // Enqueue the next pump for the same operation.
            enqueue_reading_job(
                context,
                FileReadingJob {
                    reader: reader.clone(),
                    generation,
                    step: JobStep::Pump(state),
                },
            );
            Ok(JsValue::undefined())
        }
    }
}

/// Finishes an operation at EOF: final progress, then terminal dispatch.
///
/// Queues the final `progress(loaded = total)` (unless already sent), sets
/// `DONE` with the packaged result, releases the quota slot once, and
/// dispatches `load` (the conditional `loadend` follows from the dispatch
/// step). Memory stays O(chunk + final result): no whole-blob copy exists
/// outside the packaged output.
fn finish_at_eof(
    reader: &JsObject,
    generation: u64,
    state: PumpState,
    context: &mut Context,
) -> JsResult<JsValue> {
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
    let PumpState {
        total,
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
    // in `run_pump`); the time stamp reuses the pump's clock tick.
    if !final_progress_sent {
        let time_stamp = crate::extension::snapshot(context)?
            .config
            .clock
            .now_unix_millis() as f64;
        dispatch_event_now(
            reader, generation, "progress", total, total, time_stamp, context,
        )?;
    }
    // Package the result (the Data-URL length was preflighted at read
    // start; re-check before allocation anyway).
    let result = match kind {
        ReadKind::ArrayBuffer => FileReaderResult::Bytes(buffered),
        ReadKind::BinaryString => {
            FileReaderResult::BinaryString(buffered.iter().map(|byte| char::from(*byte)).collect())
        }
        ReadKind::Text => {
            let tail = decoder.finish(&encoding);
            let mut text = text;
            text.push_str(&tail);
            FileReaderResult::Text(text)
        }
        ReadKind::DataUrl => {
            let payload =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &buffered);
            let prefix = if media_type.is_empty() {
                String::from("data:;base64,")
            } else {
                format!("data:{media_type};base64,")
            };
            let total_len = prefix.len().saturating_add(payload.len());
            if total_len as u64 > data_url_limit {
                return fail_operation(
                    reader,
                    generation,
                    total,
                    &FileApiError::ResourceLimit(ResourceLimitKind::DataUrlOutput),
                    context,
                );
            }
            let mut out = prefix;
            out.push_str(&payload);
            FileReaderResult::Text(out)
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
    release_slot(context)?;
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
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
    generation: u64,
    total: u64,
    error: &FileApiError,
    context: &mut Context,
) -> JsResult<JsValue> {
    if !generation_current(reader, generation) {
        return Ok(JsValue::undefined());
    }
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
    release_slot(context)?;
    enqueue_reading_job(
        context,
        FileReadingJob {
            reader: reader.clone(),
            generation,
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
    state: DispatchState,
    context: &mut Context,
) -> JsResult<JsValue> {
    // Stale dispatch: strict no-op.
    if !generation_current(reader, generation) {
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

impl IncrementalDecoder {
    /// Feeds one chunk and returns the decoded prefix.
    fn push(&mut self, encoding: &TextEncoding, chunk: &[u8]) -> String {
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder_without_bom_handling());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return String::new();
        };
        // `decode_to_string` with `last = false`: split multibyte sequences
        // stay buffered inside the decoder, never emitted as U+FFFD early.
        // The output `String` must have spare capacity: `decode_to_string`
        // treats capacity as the output limit and never reallocates.
        let mut out = String::with_capacity(chunk.len().saturating_add(8));
        let (_, _, _) = decoder.decode_to_string(chunk, &mut out, false);
        strip_leading_bom_once(encoding, &mut self.bom_consumed, &mut out);
        out
    }

    /// Flushes the decoder at EOF (`last = true`).
    fn finish(&mut self, encoding: &TextEncoding) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        if self.decoder.is_none() {
            self.decoder = Some(encoding.encoding.new_decoder_without_bom_handling());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return String::new();
        };
        let mut out = String::with_capacity(8);
        let (_, _, _) = decoder.decode_to_string(b"", &mut out, true);
        strip_leading_bom_once(encoding, &mut self.bom_consumed, &mut out);
        out
    }
}

/// Strips one leading U+FEFF once per UTF-8 operation (Encoding Standard
/// BOM handling for `readAsText`). Non-UTF-8 encodings keep the character.
fn strip_leading_bom_once(encoding: &TextEncoding, consumed: &mut bool, out: &mut String) {
    if *consumed || !encoding.strip_utf8_bom {
        return;
    }
    *consumed = true;
    if out.starts_with('\u{FEFF}') {
        out.drain(..'\u{FEFF}'.len_utf8());
    }
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
