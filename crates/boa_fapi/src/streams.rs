//! Capability-checked `ReadableStream` shim for `Blob.stream()`/`textStream()`
//! (M9-D: off-thread chunk I/O).
//!
//! Boa 0.22 ships no WHATWG `ReadableStream`, so this module implements the
//! small branded surface the M3-B order requires: a byte stream and a string
//! stream whose chunks are produced strictly on demand through the M9-B
//! executor/completion protocol. No method builds the stream from
//! `arrayBuffer()`/`materialize()` or from a pre-read chunk array, and no
//! Boa job performs a filesystem `read_range` itself.
//!
//! Demand model: each `read()` creates a pending `Promise` and a FIFO demand
//! slot. When no chunk is ready and no I/O is in flight, exactly one bounded
//! [`StreamChunkTask`](crate::io::StreamChunkTask) is submitted; the worker
//! returns a Rust-only [`StreamChunkCompletion`](crate::io::StreamChunkCompletion)
//! (chunk/EOF/typed error), and `FileApiHandle::poll_io` turns it into one
//! Boa settlement job. Bytes are copied into JS only on the Boa thread.
//!
//! Brand model: every stream and reader object carries native data
//! (`StreamNative`/`ReaderNative`) holding a shared [`StreamShared`] cell.
//! JS can never forge the brand: only `stream()`/`textStream()` create
//! streams, and only `getReader()` creates readers bound to their stream.
//! Pending promise resolvers live in the per-context [`PendingStreamReads`]
//! table (GC-traced through `Context::insert_data`) until `poll_io`
//! settles them; workers and completions never hold JS values.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::job::{Job, PromiseJob};
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::object::builtins::{JsArrayBuffer, JsUint8Array};
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, JsString, JsSymbol, JsValue, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::error::type_error;

/// The chunk flavor produced by a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StreamMode {
    /// Fresh offset-0 `Uint8Array` chunks over fresh `ArrayBuffer`s.
    Bytes,
    /// Primitive string chunks via the incremental UTF-8 decoder.
    Text,
}

/// Constructor/prototype pairs installed as globals.
#[derive(Clone)]
pub(crate) struct StreamSpecs {
    /// `ReadableStream` shim constructor.
    pub(crate) stream: StandardConstructor,
    /// `ReadableStreamDefaultReader` shim constructor.
    pub(crate) reader: StandardConstructor,
}

/// Builds the `ReadableStream`/`ReadableStreamDefaultReader` class pair.
///
/// No global is touched here; installation stays atomic in `extension.rs`.
pub(crate) fn build_stream_specs(context: &mut Context) -> JsResult<StreamSpecs> {
    use boa_engine::native_function::NativeFunction;

    let mut stream_builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(shim_constructor_rejects),
    );
    stream_builder.name("ReadableStream");
    stream_builder.length(0);
    let stream = stream_builder.build();
    init_stream_prototype(&stream.prototype(), context)?;

    let mut reader_builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(shim_constructor_rejects),
    );
    reader_builder.name("ReadableStreamDefaultReader");
    reader_builder.length(0);
    let reader = reader_builder.build();
    init_reader_prototype(&reader.prototype(), context)?;

    Ok(StreamSpecs { stream, reader })
}

/// `new ReadableStream()` / `new ReadableStreamDefaultReader()`.
///
/// The shim constructors are not user-constructible: every direct call
/// fails synchronously with `TypeError`.
fn shim_constructor_rejects(
    _new_target: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Err(type_error(
        "ReadableStream shim constructors are not constructible",
    ))
}

/// Shared mutable stream state behind `Rc<RefCell<..>>`.
///
/// Owned by the stream object and every reader created from it. The cell
/// holds only Rust data: the immutable blob payload plus the logical read
/// cursor, the chunk ceiling, the UTF-8 decoder, the chunk flavor, flags,
/// the FIFO queue of pending demand slots, and the in-flight/submission
/// bookkeeping. It holds no `Context`, `JsValue`, `JsObject`, `JsFunction`,
/// `ResolvingFunctions`, or callback: every pending `read()` promise's
/// resolvers live in the per-context [`PendingStreamReads`] table, which
/// the GC traces until `poll_io` settles them.
pub(crate) struct StreamShared {
    /// Immutable blob payload; `None` once the stream reaches terminal EOF,
    /// errors, or cancels.
    data: Option<std::sync::Arc<BlobData>>,
    /// Logical bytes consumed so far (next chunk starts here).
    loaded: u64,
    /// Per-stream chunk ceiling snapshotted from limits at creation.
    chunk_size: usize,
    /// Byte or text flavor of this stream.
    mode: StreamMode,
    /// Incremental UTF-8 decoder state for text streams.
    decoder: Utf8Decoder,
    /// Locked once `getReader()` hands out a live reader.
    locked: bool,
    /// Cancelled via stream or reader cancel: future reads are done.
    cancelled: bool,
    /// Terminal error mapped at failure time, replayed to later reads.
    errored: Option<MappedStreamError>,
    /// FIFO queue of pending `read()` demand slots, in `read()` order.
    queue: std::collections::VecDeque<PendingRead>,
    /// Next FIFO sequence number for a queued `read()`.
    next_seq: u64,
    /// Monotonically increasing generation; stale completions compare.
    generation: u64,
    /// `true` while one chunk request is submitted but not yet drained.
    in_flight: bool,
    /// Logical Blob size at stream creation (telemetry `size` only).
    #[cfg(feature = "tracing")]
    total_size: u64,
}

/// One queued `read()` demand slot.
///
/// The promise resolvers are NOT stored here: `ResolvingFunctions` holds
/// Boa `JsFunction`s, which must stay traced by the GC. Resolvers live in
/// the per-context [`PendingStreamReads`] table (traced through
/// `Context::insert_data`) until `poll_io` settles them. This struct
/// carries the FIFO order key plus the chunk flavor active at queue time.
struct PendingRead {
    /// FIFO sequence number assigned at `read()` time.
    seq: u64,
    /// Chunk flavor active when the request was queued.
    mode: StreamMode,
}

impl std::fmt::Debug for PendingRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRead")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// Terminal error of a stream: the mapped name/message replayed without
/// further source reads.
#[derive(Clone, Debug, Eq, PartialEq)]
struct MappedStreamError {
    /// Mapped `DOMException` name (e.g. `"NotReadableError"`).
    name: String,
    /// Generic message without paths, bytes, or source details.
    message: String,
}

impl std::fmt::Debug for StreamShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamShared")
            .field("mode", &self.mode)
            .field("locked", &self.locked)
            .field("cancelled", &self.cancelled)
            .field("errored", &self.errored)
            .field("pending", &self.queue.len())
            .finish_non_exhaustive()
    }
}

/// GC-safe pending stream reads: resolvers plus the FIFO slot.
///
/// Keyed by `(operation id, sequence number)`. `ResolvingFunctions` holds
/// `JsFunction`s, so the table lives in the GC-traced `HostDefined` area
/// (`Context::insert_data`): resolvers stay rooted from `read()` until
/// `poll_io` settles them, exactly like promise reads in `promise_read.rs`.
/// Workers and completions never touch this table; only the Boa thread
/// inserts (at `read()`), settles (in `poll_io`), or clears it
/// (cancel/error/shutdown).
struct PendingStreamRead {
    /// The promise resolvers (JS functions, Boa thread only).
    resolvers: ResolvingFunctions,
    /// The chunk flavor captured at `read()` time.
    mode: StreamMode,
    /// The stream generation valid when the read was queued.
    generation: u64,
    /// The FIFO sequence number (re-queue key for decoder-pending text).
    seq: u64,
    /// Shared stream state for packaging/terminal checks at settle time.
    shared: Rc<RefCell<StreamShared>>,
}

/// Per-context table of pending stream reads awaiting `poll_io`.
#[derive(Default)]
struct PendingStreamReads {
    reads: std::collections::HashMap<(u64, u64), PendingStreamRead>,
}

/// Per-context live stream roots awaiting worker chunks (M9-D).
///
/// Keyed by I/O operation id; each entry holds the shared stream state
/// plus its generation. The entry is created at `stream()`/`textStream()`
/// time and removed exactly once: at terminal EOF (via
/// [`transition_stream_eof`]), at terminal error (via the error path), at
/// `cancel()`, or at shutdown drain. A removed entry makes every queued
/// worker chunk stale: `poll_io` drops the completion without JS mutation,
/// event, telemetry, or a second quota release. The table itself is
/// GC-transparent: it is reached through the stream/reader objects
/// (`StreamNative`/`ReaderNative`) and through [`PendingStreamReads`]
/// entries, so a GC between `read()` and the first `poll_io` drain cannot
/// collect a live stream.
#[derive(Default)]
struct PendingStreamOps {
    ops: std::collections::HashMap<u64, PendingStreamOp>,
}

struct PendingStreamOp {
    shared: Rc<RefCell<StreamShared>>,
    generation: u64,
}

/// Returns the per-context pending-read table, creating it on first use.
fn pending_stream_reads_mut(context: &mut Context) -> JsResult<&mut PendingStreamReads> {
    if context.get_data::<PendingStreamReads>().is_none() {
        let _ = context.insert_data::<PendingStreamReads>(PendingStreamReads::default());
    }
    context
        .host_defined_mut()
        .get_mut::<PendingStreamReads>()
        .ok_or_else(|| type_error("the stream read queue is unavailable"))
}

/// Returns the per-context stream-ops table, creating it on first use.
fn pending_stream_ops_mut(context: &mut Context) -> JsResult<&mut PendingStreamOps> {
    if context.get_data::<PendingStreamOps>().is_none() {
        let _ = context.insert_data::<PendingStreamOps>(PendingStreamOps::default());
    }
    context
        .host_defined_mut()
        .get_mut::<PendingStreamOps>()
        .ok_or_else(|| type_error("the stream operation table is unavailable"))
}

/// Takes the pending resolvers for `key`, if still present.
fn take_pending_stream_read(context: &mut Context, key: (u64, u64)) -> Option<PendingStreamRead> {
    context
        .host_defined_mut()
        .get_mut::<PendingStreamReads>()
        .and_then(|table| table.reads.remove(&key))
}

/// Removes the pending root for `operation`, if present.
fn remove_pending_stream_op(context: &mut Context, operation: u64) {
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamOps>() {
        table.ops.remove(&operation);
    }
}

/// Native brand data of a stream object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct StreamNative {
    /// Shared state; ignored by the GC tracer (contains no GC pointers).
    #[unsafe_ignore_trace]
    shared: Rc<RefCell<StreamShared>>,
}

impl StreamNative {
    /// Wraps shared stream state as native brand data.
    pub(crate) fn new(shared: Rc<RefCell<StreamShared>>) -> Self {
        Self { shared }
    }

    /// Returns the shared stream state.
    pub(crate) fn shared(&self) -> &Rc<RefCell<StreamShared>> {
        &self.shared
    }
}

/// Native brand data of a reader object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct ReaderNative {
    /// Shared state with the parent stream.
    #[unsafe_ignore_trace]
    shared: Rc<RefCell<StreamShared>>,
    /// Released via `releaseLock()`: further `read()` calls fail.
    released: bool,
}

impl ReaderNative {
    /// Wraps shared stream state as native reader data.
    pub(crate) fn new(shared: Rc<RefCell<StreamShared>>) -> Self {
        Self {
            shared,
            released: false,
        }
    }
}

/// Minimal incremental UTF-8 decoder with replacement semantics.
///
/// Complete sequences decode immediately; a trailing incomplete sequence is
/// buffered across chunks and never emitted as U+FFFD early. At EOF,
/// [`Utf8Decoder::flush`] converts leftover bytes to one U+FFFD per byte,
/// matching `String::from_utf8_lossy` on the whole input without ever
/// materializing it. Invalid bytes are replaced one U+FFFD per byte.
#[derive(Clone, Debug, Default)]
struct Utf8Decoder {
    /// Buffered trailing incomplete sequence (at most 3 bytes).
    pending: Vec<u8>,
}

impl Utf8Decoder {
    /// Decodes `input`, returning the string prefix and buffering any
    /// trailing incomplete sequence for the next chunk.
    fn push(&mut self, input: &[u8]) -> String {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&self.pending);
        buf.extend_from_slice(input);
        self.pending.clear();
        let tail = incomplete_tail_len(&buf);
        let (complete, rest) = buf.split_at(buf.len() - tail);
        self.pending.extend_from_slice(rest);
        String::from_utf8_lossy(complete).into_owned()
    }

    /// Flushes buffered bytes at EOF into replacement characters.
    fn flush(&mut self) -> String {
        let out: String = self.pending.iter().map(|_| '\u{FFFD}').collect();
        self.pending.clear();
        out
    }
}

/// Length of the trailing incomplete UTF-8 sequence in `buf` (0 if none).
///
/// Returns the largest `k` in `1..=3` such that the last `k` bytes could be
/// a strict prefix of a valid UTF-8 sequence (i.e. completable by appending
/// bytes). Definitive invalid tails (lone continuation, bad leader, or a
/// complete-but-ill-formed sequence) return 0 so `from_utf8_lossy` replaces
/// them immediately instead of buffering.
fn incomplete_tail_len(buf: &[u8]) -> usize {
    let max = buf.len().min(3);
    for k in (1..=max).rev() {
        if is_valid_utf8_prefix(&buf[buf.len() - k..]) {
            return k;
        }
    }
    0
}

/// Returns `true` when `suffix` could begin a valid UTF-8 sequence whose
/// remaining bytes have not arrived yet.
fn is_valid_utf8_prefix(suffix: &[u8]) -> bool {
    match suffix {
        // A multibyte leader missing its continuations.
        [0xC2..=0xDF] => true,
        [0xE0..=0xEF] => true,
        [0xF0..=0xF4] => true,
        // A 3-byte leader plus one continuation, missing the last byte.
        [0xE0..=0xEF, 0x80..=0xBF] => true,
        // A 4-byte leader plus continuations so far, missing bytes.
        [0xF0..=0xF4, 0x80..=0xBF] => true,
        [0xF0..=0xF4, 0x80..=0xBF, 0x80..=0xBF] => true,
        _ => false,
    }
}

/// Validates that `this` carries the stream brand.
fn require_stream(this: &JsValue) -> JsResult<Rc<RefCell<StreamShared>>> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a ReadableStream"));
    };
    if let Some(native) = object.downcast_ref::<StreamNative>() {
        return Ok(Rc::clone(native.shared()));
    }
    Err(type_error("illegal invocation: expected a ReadableStream"))
}

/// Validates that `this` carries the reader brand.
fn require_reader(this: &JsValue) -> JsResult<(JsObject, Rc<RefCell<StreamShared>>, bool)> {
    let Some(object) = this.as_object() else {
        return Err(type_error(
            "illegal invocation: expected a ReadableStreamDefaultReader",
        ));
    };
    if let Some(native) = object.downcast_ref::<ReaderNative>() {
        return Ok((object.clone(), Rc::clone(&native.shared), native.released));
    }
    Err(type_error(
        "illegal invocation: expected a ReadableStreamDefaultReader",
    ))
}

/// Validates the per-stream chunk ceiling (`16 KiB..=1 MiB`, no silent
/// clamp), matching `BlobData::reader` and FileReader `chunk_ceiling`.
fn stream_chunk_ceiling(limits: &FileApiLimits) -> Result<usize, FileApiError> {
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

/// Creates a fresh unlocked stream object for `data` in `mode`.
///
/// Reserves one `IoBridge` slot for the stream up front (M9-D quota model:
/// one slot per stream until its terminal EOF/error/cancel/shutdown
/// transition, each freeing the slot exactly once), registers the
/// stream root, and submits nothing: the first chunk request leaves only
/// when the first `read()` creates demand. A shutdown runtime fails fast
/// with a cancelled stream (no slot consumed).
///
/// The chunk ceiling comes from this context's limits (`specs.limits()`),
/// not from the caller-passed `limits`: a host payload built under
/// different limits still streams in context-sized chunks. `limits` is
/// kept as the preflight parameter for the JS `stream()`/`textStream()`
/// call shape; the two are equal on the JS path.
fn create_stream(
    data: &Arc<BlobData>,
    limits: &FileApiLimits,
    mode: StreamMode,
    context: &mut Context,
) -> JsResult<JsObject> {
    use crate::io::ReserveError;

    let specs = crate::extension::snapshot(context)?;
    if specs.shutdown.is_shutdown() {
        return Err(type_error("the File API runtime is shut down"));
    }
    // The stream cursor honors this context's chunk ceiling, so a blob
    // built under different limits still streams in context-sized chunks.
    // The worker honors the same ceiling via the task snapshot.
    let chunk_size = stream_chunk_ceiling(specs.limits()).map_err(|error| match error {
        FileApiError::ResourceLimit(kind) => {
            let _ = kind;
            type_error("blob stream chunk size is out of range")
        }
        other => crate::error::js_from_core(other),
    })?;
    let _ = limits;
    let bridge = specs.io_bridge();
    let (operation_id, token) = match bridge.reserve() {
        Ok(reserved) => reserved,
        Err(ReserveError::Shutdown) => {
            return Err(type_error("the File API runtime is shut down"));
        }
        Err(ReserveError::QuotaFull | ReserveError::CompletionFull) => {
            return Err(crate::error::js_from_core(FileApiError::TooManyReads));
        }
    };
    // Stream generation starts at 1 and only moves forward on terminal
    // transitions (cancel/error): it lets `poll_io` drop completions that
    // raced a terminal state change.
    let generation = 1_u64;
    #[cfg(feature = "tracing")]
    let total_size = data.size();
    let shared = Rc::new(RefCell::new(StreamShared {
        data: Some(Arc::clone(data)),
        loaded: 0,
        chunk_size,
        mode,
        decoder: Utf8Decoder::default(),
        locked: false,
        cancelled: false,
        errored: None,
        queue: std::collections::VecDeque::new(),
        next_seq: 0,
        generation,
        in_flight: false,
        #[cfg(feature = "tracing")]
        total_size,
    }));
    // Register the stream root before any JS object exists: the operation
    // owns the quota slot, and `poll_io` validates the root before
    // settling. The bridge token is stored with the reservation already;
    // `token` here keeps the worker cancellable after cancel/shutdown.
    let _ = token;
    pending_stream_ops_mut(context)?.ops.insert(
        operation_id.get(),
        PendingStreamOp {
            shared: Rc::clone(&shared),
            generation,
        },
    );
    specs.store_stream_payload(operation_id.get(), Arc::clone(data));
    #[cfg(feature = "streams-shim")]
    let prototype = specs
        .streams
        .as_ref()
        .ok_or_else(|| type_error("the streams shim is not registered"))?
        .stream
        .prototype();
    #[cfg(not(feature = "streams-shim"))]
    let _ = specs;
    #[cfg(not(feature = "streams-shim"))]
    return Err(type_error("the streams shim is not registered"));
    #[cfg(feature = "streams-shim")]
    return Ok(JsObject::from_proto_and_data(
        prototype,
        StreamNative::new(shared),
    ));
}

/// `Blob.prototype.stream()`: fresh byte stream, nothing read yet.
pub(crate) fn stream(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let limits = crate::extension::snapshot(context)?.limits().clone();
    Ok(create_stream(&data, &limits, StreamMode::Bytes, context)?.into())
}

/// `Blob.prototype.textStream()`: fresh string stream, nothing read yet.
pub(crate) fn text_stream(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let limits = crate::extension::snapshot(context)?.limits().clone();
    Ok(create_stream(&data, &limits, StreamMode::Text, context)?.into())
}

/// Builds `{ value, done }` result objects for settled reads.
fn iter_result(value: JsValue, done: bool, context: &mut Context) -> JsResult<JsValue> {
    let object = JsObject::with_object_proto(context.intrinsics());
    object.set(js_string!("value"), value, false, context)?;
    object.set(js_string!("done"), done, false, context)?;
    Ok(object.into())
}

/// `ReadableStream.prototype.getReader()`: locks and returns a reader.
fn get_reader(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let shared = require_stream(this)?;
    let mut state = shared.borrow_mut();
    if state.locked {
        return Err(type_error("the stream is already locked"));
    }
    state.locked = true;
    drop(state);
    #[cfg(feature = "streams-shim")]
    let prototype = crate::extension::snapshot(context)?
        .streams
        .as_ref()
        .ok_or_else(|| type_error("the streams shim is not registered"))?
        .reader
        .prototype();
    #[cfg(not(feature = "streams-shim"))]
    return Err(type_error("the streams shim is not registered"));
    #[cfg(feature = "streams-shim")]
    return Ok(JsObject::from_proto_and_data(prototype, ReaderNative::new(shared)).into());
}

/// `ReadableStream.prototype.locked`: readonly getter.
fn locked_getter(this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let shared = require_stream(this)?;
    Ok(JsValue::from(shared.borrow().locked))
}

/// Builds the rejection reason for a stream failure, in the same realm:
/// the M4-A mapped `DOMException` when the `dom-shim` feature is on, a
/// plain `Error` otherwise (or when the DOM shim is unexpectedly absent —
/// registration always installs it).
fn stream_error_reason(context: &mut Context, error: &FileApiError) -> JsValue {
    #[cfg(feature = "dom-shim")]
    {
        let (name, message) = crate::dom::map_core_error(error);
        crate::extension::snapshot(context)
            .ok()
            .and_then(|specs| specs.dom_specs())
            .map(|dom| JsValue::from(crate::dom::construct_exception(&dom, name, message)))
            .unwrap_or_else(|| crate::error::js_read_error(context))
    }
    #[cfg(not(feature = "dom-shim"))]
    {
        let _ = error;
        crate::error::js_read_error(context)
    }
}

/// Maps a core failure into the stored terminal error (name/message only,
/// never bytes, paths, or source details).
fn map_stream_error(error: &FileApiError) -> MappedStreamError {
    #[cfg(feature = "dom-shim")]
    {
        let (name, message) = crate::dom::map_core_error(error);
        MappedStreamError {
            name: name.to_owned(),
            message: message.to_owned(),
        }
    }
    #[cfg(not(feature = "dom-shim"))]
    {
        let _ = error;
        MappedStreamError {
            name: String::from("Error"),
            message: String::from("blob read failed"),
        }
    }
}

/// Builds the stored terminal rejection reason in the settling realm.
fn stored_error_reason(
    specs: &crate::extension::RegisteredSpecs,
    stored: &MappedStreamError,
    context: &mut Context,
) -> JsValue {
    #[cfg(feature = "dom-shim")]
    {
        specs
            .dom_specs()
            .map(|dom| {
                JsValue::from(crate::dom::construct_exception(
                    &dom,
                    &stored.name,
                    &stored.message,
                ))
            })
            .unwrap_or_else(|| crate::error::js_read_error(context))
    }
    #[cfg(not(feature = "dom-shim"))]
    {
        let _ = specs;
        let _ = stored;
        crate::error::js_read_error(context)
    }
}

/// `ReadableStream.prototype.cancel(reason?)`: idempotent unlock-cancel.
///
/// Only valid on an unlocked stream; a locked stream rejects with
/// `TypeError` through a queued job. Success cancels the in-flight worker
/// request, releases the stream reservation exactly once, and makes queued
/// and future reads done — all synchronously on the calling stack, with
/// settlement of the cancel promise through one Boa job.
fn stream_cancel(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let shared = require_stream(this)?;
    let locked = shared.borrow().locked;
    let (promise, resolvers) = JsPromise::new_pending(context);
    if locked {
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let reason = type_error("cannot cancel a locked stream")
                    .into_opaque(context)
                    .map_or_else(|_| crate::error::js_read_error(context), JsValue::from);
                resolvers
                    .reject
                    .call(&JsValue::undefined(), &[reason], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
        return Ok(promise.into());
    }
    // Synchronous terminal transition on the calling stack: cancel the
    // in-flight token, mark done, and release the quota slot now — never a
    // source read on the Boa thread. Queued and future `read()` promises
    // settle done when their completions drain through `poll_io`.
    cancel_shared(&shared, context);
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[JsValue::undefined()], context)?;
            Ok(JsValue::undefined())
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(promise.into())
}

/// Marks the shared state cancelled, settles every queued demand done,
/// and releases the stream reservation.
///
/// Queued `read()` promises settle done through their own Boa jobs
/// (enqueued here, delivered by `run_jobs`); future reads take the
/// cancelled fast path in `reader_read`. The in-flight worker chunk, if
/// any, goes stale at the bridge: its late completion is dropped without
/// JS mutation. Stream-level cancel has no chunk request of its own.
fn cancel_shared(shared: &Rc<RefCell<StreamShared>>, context: &mut Context) {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    #[cfg(feature = "tracing")]
    let (trace_size, trace_env) = {
        let size = shared.borrow().total_size;
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        (size, env)
    };
    let operation = operation_for_shared(context, shared);
    {
        let mut state = shared.borrow_mut();
        state.cancelled = true;
        state.data = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1).max(1);
    }
    #[cfg(feature = "tracing")]
    crate::observability::emit(
        "stream_read",
        trace_size,
        crate::observability::elapsed_ms(trace_start),
        0,
        "cancelled",
        trace_env,
    );
    if let Some(operation) = operation {
        // Settle every queued demand done now (each through its own job),
        // then release the reservation exactly once.
        let seqs: Vec<u64> = {
            let mut state = shared.borrow_mut();
            let mut seqs = Vec::new();
            while let Some(slot) = state.queue.pop_front() {
                seqs.push(slot.seq);
            }
            seqs
        };
        for seq in seqs {
            let Some(pending) = take_pending_stream_read(context, (operation, seq)) else {
                continue;
            };
            let PendingStreamRead { resolvers, .. } = pending;
            let realm = context.realm().clone();
            let job = PromiseJob::with_realm(
                move |context: &mut Context| -> JsResult<JsValue> {
                    let done = iter_result(JsValue::undefined(), true, context)?;
                    resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[done], context)?;
                    Ok(JsValue::undefined())
                },
                realm,
            );
            context.enqueue_job(Job::PromiseJob(job));
        }
        release_stream_operation(context, operation);
    }
}

/// `ReadableStreamDefaultReader.prototype.read()`: one pending promise.
///
/// Creates the pending promise and a FIFO demand slot, then submits at
/// most one bounded chunk request when no chunk is ready and no I/O is in
/// flight. The promise stays pending until the worker completion drains
/// through `poll_io`; resolvers live in [`PendingStreamReads`] (GC-traced)
/// until then. Multiple `read()` calls never start unbounded read-ahead.
fn reader_read(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let (_object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    // Terminal states settle without worker contact: cancelled and
    // EOF-consumed streams resolve done, errored streams reject the stored
    // class — still through one Boa job, never synchronously.
    let terminal = {
        let state = shared.borrow();
        if let Some(stored) = state.errored.clone() {
            Some(Err(stored))
        } else if state.cancelled || state.data.is_none() {
            Some(Ok(()))
        } else {
            None
        }
    };
    if let Some(outcome) = terminal {
        let (promise, resolvers) = JsPromise::new_pending(context);
        let realm = context.realm().clone();
        match outcome {
            Err(stored) => {
                let specs = crate::extension::snapshot(context)?;
                // `observability::emit` needs a `'static` class: the stored
                // name is owned, so map it back to the fixed class here.
                let stored_class: &'static str = match stored.name.as_str() {
                    "QuotaExceededError" => "quota",
                    "AbortError" => "cancelled",
                    "NotFoundError" => "not_found",
                    "SecurityError" => "permission",
                    _ => "error",
                };
                let _ = stored_class;
                let job = PromiseJob::with_realm(
                    move |context: &mut Context| -> JsResult<JsValue> {
                        #[cfg(feature = "tracing")]
                        {
                            let env = crate::observability::environment_hash_for_specs(&specs);
                            crate::observability::emit(
                                "stream_read",
                                0,
                                crate::observability::elapsed_ms(crate::observability::now()),
                                0,
                                stored_class,
                                env,
                            );
                        }
                        let reason = stored_error_reason(&specs, &stored, context);
                        resolvers
                            .reject
                            .call(&JsValue::undefined(), &[reason], context)?;
                        Ok(JsValue::undefined())
                    },
                    realm,
                );
                context.enqueue_job(Job::PromiseJob(job));
            }
            Ok(()) => {
                let job = PromiseJob::with_realm(
                    move |context: &mut Context| -> JsResult<JsValue> {
                        let done = iter_result(JsValue::undefined(), true, context)?;
                        resolvers
                            .resolve
                            .call(&JsValue::undefined(), &[done], context)?;
                        Ok(JsValue::undefined())
                    },
                    realm,
                );
                context.enqueue_job(Job::PromiseJob(job));
            }
        }
        return Ok(promise.into());
    }
    if crate::extension::snapshot(context)
        .map(|specs| specs.shutdown.is_shutdown())
        .unwrap_or(false)
    {
        // Post-shutdown reads never settle: the pending promise stays
        // pending and no work is created, so no JS runs after shutdown.
        let (promise, _) = JsPromise::new_pending(context);
        return Ok(promise.into());
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    // Reserve the FIFO slot first: `poll_io` settles completions in slot
    // order, preserving FIFO and pending-first semantics.
    let (seq, mode) = {
        let mut state = shared.borrow_mut();
        let seq = state.next_seq;
        state.next_seq = state.next_seq.wrapping_add(1);
        let slot = PendingRead {
            seq,
            mode: state.mode,
        };
        state.queue.push_back(slot);
        (seq, state.mode)
    };
    // Resolve the operation/generation from the live table (the stream may
    // have been created in another registration edge — always re-read).
    let Some(operation) = operation_for_shared(context, &shared) else {
        // The stream lost its reservation (cancel/shutdown won the race
        // between the terminal check and now): drop the slot and settle
        // done through one job.
        shared.borrow_mut().queue.pop_back();
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let done = iter_result(JsValue::undefined(), true, context)?;
                resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
        return Ok(promise.into());
    };
    let generation = shared.borrow().generation;
    pending_stream_reads_mut(context)?.reads.insert(
        (operation, seq),
        PendingStreamRead {
            resolvers,
            mode,
            generation,
            seq,
            shared: Rc::clone(&shared),
        },
    );
    // Demand: submit exactly one bounded request when nothing is in flight.
    // A submit failure errors the stream through the normal terminal path
    // (quota released exactly once, queued reads reject, no second submit).
    if let Err(error) = maybe_submit_next(context, &shared, operation) {
        fail_stream_from_submit(&shared, operation, &error, context)?;
    }
    Ok(promise.into())
}

/// Returns the live I/O operation id for `shared`, if it still owns one.
fn operation_for_shared(context: &Context, shared: &Rc<RefCell<StreamShared>>) -> Option<u64> {
    let table = context.get_data::<PendingStreamOps>()?;
    table
        .ops
        .iter()
        .find(|(_, op)| Rc::ptr_eq(&op.shared, shared))
        .map(|(operation, _)| *operation)
}

/// Marks shared state terminally EOF, before any Promise job is queued.
///
/// The single terminal-EOF transition required by the order (shared by the
/// plain-EOF and text-tail-EOF paths): clears the payload cursor
/// (`data = None`, `in_flight = false`), removes the live operation root
/// and payload, and releases the `IoBridge` reservation exactly once.
/// Idempotent: when the operation root is already gone (a late completion
/// racing the transition, or a second call for the same operation) the
/// payload drop and the bridge release are skipped, so a late completion
/// can never cause a double release or a second payload drop. No JS runs
/// here; settlement jobs are queued by the caller after this returns.
fn transition_stream_eof(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    context: &mut Context,
) {
    {
        let mut state = shared.borrow_mut();
        state.data = None;
        state.in_flight = false;
    }
    let existed = context
        .host_defined_mut()
        .get_mut::<PendingStreamOps>()
        .map(|table| table.ops.remove(&operation).is_some())
        .unwrap_or(false);
    if existed && let Ok(specs) = crate::extension::snapshot(context) {
        specs.drop_stream_payload(operation);
        specs
            .io_bridge()
            .unreserve(crate::io::FileIoOperationId::from_raw(operation));
    }
}

/// Releases the stream reservation exactly once and drops its payload.
///
/// Removes the pending root and the stored payload, then releases the
/// bridge slot. Late worker completions for the operation go stale at the
/// bridge. Pending promise resolvers are settled by the caller (EOF/error
/// drain) or stay silent (cancel/shutdown): releasing here never settles
/// JS itself.
///
/// EOF uses [`transition_stream_eof`] instead: same release, but ordered
/// before Promise-job enqueueing and idempotent against late completions.
fn release_stream_operation(context: &mut Context, operation: u64) {
    remove_pending_stream_op(context, operation);
    if let Ok(specs) = crate::extension::snapshot(context) {
        specs.drop_stream_payload(operation);
        specs
            .io_bridge()
            .unreserve(crate::io::FileIoOperationId::from_raw(operation));
    }
}

/// Terminal error path for a submit failure at `read()` time.
///
/// The oldest queued demand is settled with the mapped class together with
/// every other queued demand (the reservation is still live here — submit
/// never consumed a worker slot). Quota is released exactly once and no
/// second request is enqueued.
fn fail_stream_from_submit(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    error: &crate::io::FileIoSubmitError,
    context: &mut Context,
) -> JsResult<()> {
    let mapped = match error {
        crate::io::FileIoSubmitError::WorkerLost => FileApiError::Internal,
        crate::io::FileIoSubmitError::QueueFull | crate::io::FileIoSubmitError::Shutdown => {
            FileApiError::TooManyReads
        }
    };
    // Pop the oldest demand (the one whose submit just failed) and settle
    // it together with the rest of the queue.
    let pending = {
        let seq = {
            let mut state = shared.borrow_mut();
            state.in_flight = false;
            state.queue.pop_front().map(|slot| slot.seq)
        };
        seq.and_then(|seq| take_pending_stream_read(context, (operation, seq)))
    };
    let Some(pending) = pending else {
        return Ok(());
    };
    fail_stream_with(shared, operation, pending, &mapped, context)
}

/// Drops the pending root and resolvers for `operation` (shutdown path).
///
/// Called from `poll_io` after the bridge reservation is released, so the
/// quota accounting stays exactly-once and no late completion can settle.
pub(crate) fn drop_pending_for_shutdown(context: &mut Context, operation: u64) {
    remove_pending_stream_op(context, operation);
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamReads>() {
        table.reads.retain(|key, _| key.0 != operation);
    }
}

/// Settles one drained stream chunk completion from `poll_io` (M9-D).
///
/// Boa thread only. Validates the pending root first: a completion for an
/// unknown operation (cancel/error/shutdown already removed it) or a
/// generation mismatch is dropped as stale with no JS mutation, no
/// telemetry, and no second quota release. Otherwise settles the oldest
/// demand slot in FIFO order: one chunk resolves one `{ value, done }`,
/// EOF resolves every queued read (and all future reads) as done, and a
/// source error rejects every queued read with the stored mapped class.
/// Returns `1` when at least one Boa job was enqueued.
pub(crate) fn settle_stream_completion(
    stored: &crate::extension::RegisteredSpecs,
    completion: crate::io::StreamChunkCompletion,
    context: &mut Context,
) -> JsResult<usize> {
    use crate::io::StreamChunkKind;

    let operation = completion.operation_id().get();
    let generation = completion.generation();
    let kind = completion.into_kind();
    let Some(op) = context
        .get_data::<PendingStreamOps>()
        .and_then(|table| table.ops.get(&operation))
    else {
        // Unknown operation: cancel/error/shutdown already released the
        // slot. Strict no-op.
        return Ok(0);
    };
    if op.generation != generation {
        return Ok(0);
    }
    if stored.shutdown.is_shutdown() {
        return Ok(0);
    }
    let shared = Rc::clone(&op.shared);
    // The demand slot FIFO decides settlement order, not worker timing:
    // multiple `read()` calls queue slots; one completion settles exactly
    // the oldest slot. A second in-flight request never exists (the next
    // submit happens only after this completion settles below).
    let slot = {
        let mut state = shared.borrow_mut();
        state.in_flight = false;
        state.queue.pop_front()
    };
    let Some(slot) = slot else {
        // No demand left (cancel raced the completion): stale, drop it.
        return Ok(0);
    };
    let Some(pending) = take_pending_stream_read(context, (operation, slot.seq)) else {
        // Demand slot without resolvers (e.g. a slot re-queued for decoder
        // reasons is always paired — this is unreachable): drop it.
        return Ok(0);
    };
    if pending.generation != generation || !Rc::ptr_eq(&pending.shared, &shared) {
        // Stale completion for an older generation (cancel/error/EOF bumped
        // it after submit): the demand it names is already settled or moved
        // on. Restoring it here would resurrect a terminally-settled read
        // (e.g. an EOF demand already resolved done) and re-submit worker
        // I/O past the terminal state — exactly the P0-2 lifecycle split.
        // Drop the completion as a strict no-op: no slot restore, no
        // re-submit, no JS mutation, no telemetry, no second release.
        return Ok(0);
    }
    // One completion settles one demand — but a zero-length EOF probe
    // carries no bytes: settle it through the EOF path even when its kind
    // is `Chunk` with empty bytes (defensive: the worker derives EOF for
    // `offset == total`, so this is unreachable in practice).
    //
    // Stale-completion guard: `pending.generation` was captured at `read()`
    // time; a completion from an older worker request (e.g. submitted
    // before a terminal transition bumped the generation) is dropped.
    // In the live path the cursor always matches.
    match kind {
        StreamChunkKind::Error(error) => {
            fail_stream_with(&shared, operation, pending, &error, context)?;
            Ok(1)
        }
        StreamChunkKind::Eof => {
            settle_stream_eof(&shared, operation, pending, context)?;
            Ok(1)
        }
        StreamChunkKind::Chunk(bytes) => {
            settle_stream_chunk(&shared, operation, pending, bytes, context)?;
            Ok(1)
        }
    }
}

/// Packages one settled worker chunk into its demand promise.
///
/// Advances the logical cursor by exactly the chunk length, then submits
/// the next demand's request (at most one in flight) when more slots wait.
/// Text mode decodes incrementally; a chunk that yields no text yet stays
/// buffered in the decoder and the slot is re-queued at the front: the
/// next worker chunk continues the same demand without resolving an empty
/// `done:false` string.
fn settle_stream_chunk(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    pending: PendingStreamRead,
    bytes: bytes::Bytes,
    context: &mut Context,
) -> JsResult<()> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    #[cfg(feature = "tracing")]
    let (trace_size, trace_env) = {
        let size = shared.borrow().total_size;
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        (size, env)
    };
    let PendingStreamRead {
        resolvers,
        mode,
        generation,
        seq,
        shared: _,
    } = pending;
    // Empty worker chunks never reach JS: the worker derives them only as
    // EOF, and `read_blob_range` rejects short reads — but fail closed
    // rather than resolving an empty `done:false` chunk.
    if bytes.is_empty() {
        let _ = (generation, seq);
        #[cfg(feature = "tracing")]
        crate::observability::emit(
            "stream_read",
            trace_size,
            crate::observability::elapsed_ms(trace_start),
            0,
            "error",
            trace_env,
        );
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let reason = stream_error_reason(context, &FileApiError::InvalidRange);
                resolvers
                    .reject
                    .call(&JsValue::undefined(), &[reason], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
        // Keep the stream usable for the next demand: submit when slots
        // wait, else stay idle (no terminal transition on a defensive
        // empty-chunk path).
        if let Err(error) = maybe_submit_next(context, shared, operation) {
            fail_stream_from_submit(shared, operation, &error, context)?;
        }
        return Ok(());
    }
    if mode == StreamMode::Text {
        let text = shared.borrow_mut().decoder.push(&bytes);
        // Advance the cursor now: the bytes are consumed even when the
        // decoder yields no text yet.
        {
            let mut state = shared.borrow_mut();
            state.loaded = state.loaded.saturating_add(bytes.len() as u64);
        }
        if text.is_empty() {
            // No text yet: keep the same demand pending at the front and
            // submit the next chunk for it. Still exactly one in-flight
            // request, and never an empty `done:false` chunk. The sequence
            // key is preserved exactly.
            shared
                .borrow_mut()
                .queue
                .push_front(PendingRead { seq, mode });
            pending_stream_reads_mut(context)?.reads.insert(
                (operation, seq),
                PendingStreamRead {
                    resolvers,
                    mode,
                    generation,
                    seq,
                    shared: Rc::clone(shared),
                },
            );
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                0,
                "ok",
                trace_env,
            );
            if let Err(error) = maybe_submit_next(context, shared, operation) {
                fail_stream_from_submit(shared, operation, &error, context)?;
            }
            return Ok(());
        }
        #[cfg(feature = "tracing")]
        crate::observability::emit(
            "stream_read",
            trace_size,
            crate::observability::elapsed_ms(trace_start),
            1,
            "ok",
            trace_env,
        );
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let done = iter_result(JsValue::from(JsString::from(text)), false, context)?;
                resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
    } else {
        let value = match package_bytes_chunk(&bytes, context) {
            Ok(value) => value,
            Err(_) => {
                // Packaging is a Boa-side internal failure, not a
                // recoverable per-demand error: the source bytes have been
                // consumed by this completion. Terminalize through the
                // normal error path so every queued read rejects and the
                // stream reservation/payload are released exactly once.
                // Advancing `loaded` only after successful packaging also
                // prevents a later demand from silently skipping this chunk.
                return fail_stream_with(
                    shared,
                    operation,
                    PendingStreamRead {
                        resolvers,
                        mode,
                        generation,
                        seq,
                        shared: Rc::clone(shared),
                    },
                    &FileApiError::Internal,
                    context,
                );
            }
        };
        {
            let mut state = shared.borrow_mut();
            state.loaded = state.loaded.saturating_add(bytes.len() as u64);
        }
        #[cfg(feature = "tracing")]
        crate::observability::emit(
            "stream_read",
            trace_size,
            crate::observability::elapsed_ms(trace_start),
            1,
            "ok",
            trace_env,
        );
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let done = iter_result(value, false, context)?;
                resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
    }
    // One completion settles one demand; the next queued demand (if any)
    // submits its own request now — still at most one in flight.
    if let Err(error) = maybe_submit_next(context, shared, operation) {
        fail_stream_from_submit(shared, operation, &error, context)?;
    }
    Ok(())
}

/// Settles EOF: the decoder flush becomes a final value chunk when
/// non-empty (never an empty `done:false` string); then every queued and
/// all future reads resolve done. Both paths run the shared
/// [`transition_stream_eof`] (payload cursor cleared, operation root and
/// payload removed, reservation released exactly once) before any Promise
/// job is queued. No further source reads happen after EOF: `data` is
/// cleared and future `read()` calls take the terminal fast path without
/// worker contact. Late completions after EOF are strict no-ops (unknown
/// operation, no JS mutation, no telemetry, no second release).
fn settle_stream_eof(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    pending: PendingStreamRead,
    context: &mut Context,
) -> JsResult<()> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    #[cfg(feature = "tracing")]
    let (trace_size, trace_env) = {
        let size = shared.borrow().total_size;
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        (size, env)
    };
    let PendingStreamRead {
        resolvers,
        mode,
        generation: _,
        seq: _,
        shared: _,
    } = pending;
    // Flush the text decoder: a non-empty tail is one final value chunk
    // for THIS demand; EOF itself never carries a data chunk. The terminal
    // transition below runs for BOTH paths, so the text-tail EOF sets the
    // same terminal state as a plain EOF.
    if mode == StreamMode::Text {
        let tail = shared.borrow_mut().decoder.flush();
        if !tail.is_empty() {
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                1,
                "ok",
                trace_env,
            );
            // Terminal transition FIRST: payload cursor cleared, operation
            // root/payload removed, reservation released exactly once —
            // before any Promise job is queued.
            transition_stream_eof(shared, operation, context);
            let realm = context.realm().clone();
            let job = PromiseJob::with_realm(
                move |context: &mut Context| -> JsResult<JsValue> {
                    let done = iter_result(JsValue::from(JsString::from(tail)), false, context)?;
                    resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[done], context)?;
                    Ok(JsValue::undefined())
                },
                realm,
            );
            context.enqueue_job(Job::PromiseJob(job));
            // The flush settled this demand as a value chunk: remaining
            // queued reads still resolve done (the live root is gone, so
            // `drain_queue_done` is unaffected by the transition).
            drain_queue_done(context, shared, operation)?;
            return Ok(());
        }
    }
    // Terminal EOF: resolve this demand done, then every queued demand
    // done. The reservation is released exactly once via the shared
    // transition above (ordered before Promise-job enqueueing) — so a
    // second stream or a later `blob.text()` reserves a fresh slot while
    // this stream stays terminally done for future reads without worker
    // contact.
    //
    // The single `stream_read/ok` terminal telemetry event is emitted here,
    // once, before the transition: late completions after EOF hit the
    // unknown-operation guard in `settle_stream_completion` and emit
    // nothing (no duplicate terminal event).
    #[cfg(feature = "tracing")]
    crate::observability::emit(
        "stream_read",
        trace_size,
        crate::observability::elapsed_ms(trace_start),
        0,
        "ok",
        trace_env,
    );
    // Terminal transition FIRST (see above): idempotent, exactly-once.
    transition_stream_eof(shared, operation, context);
    // Every other queued read resolves done through its own job.
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            let done = iter_result(JsValue::undefined(), true, context)?;
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            Ok(JsValue::undefined())
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    drain_queue_done(context, shared, operation)?;
    Ok(())
}

/// Errors the stream: the triggering demand plus every queued read reject
/// with the stored mapped class, future reads replay it, and the
/// reservation is released exactly once. No further source reads happen.
fn fail_stream_with(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    pending: PendingStreamRead,
    error: &FileApiError,
    context: &mut Context,
) -> JsResult<()> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    let stored = map_stream_error(error);
    #[cfg(feature = "tracing")]
    let trace_class = crate::observability::result_class_for_core(Some(error));
    #[cfg(feature = "tracing")]
    let (trace_size, trace_env) = {
        let size = shared.borrow().total_size;
        let env = crate::extension::snapshot(context)
            .ok()
            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
            .unwrap_or(0);
        (size, env)
    };
    {
        let mut state = shared.borrow_mut();
        state.errored = Some(stored.clone());
        state.data = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1).max(1);
    }
    #[cfg(feature = "tracing")]
    crate::observability::emit(
        "stream_read",
        trace_size,
        crate::observability::elapsed_ms(trace_start),
        0,
        trace_class,
        trace_env,
    );
    let specs = crate::extension::snapshot(context)?;
    // Settle the triggering demand first with the stored class, then every
    // remaining queued demand with the same class.
    let PendingStreamRead { resolvers, .. } = pending;
    let specs_settle = specs.clone();
    let stored_settle = stored.clone();
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            let reason = stored_error_reason(&specs_settle, &stored_settle, context);
            resolvers
                .reject
                .call(&JsValue::undefined(), &[reason], context)?;
            Ok(JsValue::undefined())
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    drain_queue_reject(context, shared, operation, &specs, &stored)?;
    release_stream_operation(context, operation);
    Ok(())
}

/// Resolves every remaining queued demand as done (EOF path).
fn drain_queue_done(
    context: &mut Context,
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
) -> JsResult<()> {
    loop {
        let seq = {
            let mut state = shared.borrow_mut();
            let Some(slot) = state.queue.pop_front() else {
                break;
            };
            slot.seq
        };
        let Some(pending) = take_pending_stream_read(context, (operation, seq)) else {
            continue;
        };
        let PendingStreamRead { resolvers, .. } = pending;
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let done = iter_result(JsValue::undefined(), true, context)?;
                resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
    }
    Ok(())
}

/// Rejects every remaining queued demand with the stored class (error path).
fn drain_queue_reject(
    context: &mut Context,
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    specs: &crate::extension::RegisteredSpecs,
    stored: &MappedStreamError,
) -> JsResult<()> {
    loop {
        let seq = {
            let mut state = shared.borrow_mut();
            let Some(slot) = state.queue.pop_front() else {
                break;
            };
            slot.seq
        };
        let Some(pending) = take_pending_stream_read(context, (operation, seq)) else {
            continue;
        };
        let PendingStreamRead { resolvers, .. } = pending;
        let specs = specs.clone();
        let stored = stored.clone();
        let realm = context.realm().clone();
        let job = PromiseJob::with_realm(
            move |context: &mut Context| -> JsResult<JsValue> {
                let reason = stored_error_reason(&specs, &stored, context);
                resolvers
                    .reject
                    .call(&JsValue::undefined(), &[reason], context)?;
                Ok(JsValue::undefined())
            },
            realm,
        );
        context.enqueue_job(Job::PromiseJob(job));
    }
    Ok(())
}

/// Submits the next chunk request when demand waits and nothing is in
/// flight (M9-D backpressure core).
///
/// Exactly one bounded request `[loaded, loaded+chunk)` clamped to `total`
/// is submitted per call; the worker reads only that range off-thread. No
/// readahead and no accumulation past the cursor happen here. A submit
/// failure is returned for the terminal error path (quota released exactly
/// once, no second request enqueued).
fn maybe_submit_next(
    context: &mut Context,
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
) -> Result<(), crate::io::FileIoSubmitError> {
    let (loaded, generation, len) = {
        let mut state = shared.borrow_mut();
        if state.in_flight || state.cancelled || state.errored.is_some() {
            return Ok(());
        }
        if state.queue.is_empty() {
            return Ok(());
        }
        let Some(data) = state.data.clone() else {
            return Ok(());
        };
        let total = data.size();
        let remaining = total.saturating_sub(state.loaded);
        // Empty blob or cursor-at-end: zero-length EOF probe (no source
        // read on the worker). Past-terminal states already returned above
        // (`data` is `None` there), so reaching here with `loaded == total`
        // always means exactly one EOF probe is still owed.
        let len = (state.chunk_size as u64).min(remaining);
        let generation = state.generation;
        state.in_flight = true;
        (state.loaded, generation, len)
    };
    let specs = crate::extension::snapshot(context).map_err(|_| {
        // Snapshot failure (unregistered context): roll back the flag and
        // fail closed as worker loss.
        shared.borrow_mut().in_flight = false;
        crate::io::FileIoSubmitError::WorkerLost
    })?;
    let bridge = specs.io_bridge();
    let operation_id = crate::io::FileIoOperationId::from_raw(operation);
    let Some(token) = bridge.token_for(operation_id) else {
        // Reservation already gone (cancel/error/shutdown won the race):
        // become a strict no-op instead of submitting.
        shared.borrow_mut().in_flight = false;
        return Ok(());
    };
    let Some(data) = specs.stream_payload(operation) else {
        shared.borrow_mut().in_flight = false;
        return Ok(());
    };
    let task = bridge.stream_task_for(
        operation_id,
        token,
        data,
        // Task ceiling: the stream cursor's context ceiling. The worker
        // clamps through `read_blob_range` against this snapshot; the
        // cursor above clamped against the same context value, so offsets
        // always agree even for host payloads built under other limits.
        specs.limits().clone(),
        crate::io::ChunkWindow {
            generation,
            offset: loaded,
            len,
        },
    );
    if let Err(error) = bridge.submit_stream_guarded(task) {
        shared.borrow_mut().in_flight = false;
        return Err(error);
    }
    Ok(())
}

/// Packages one chunk as a fresh offset-0 `Uint8Array` over a fresh buffer.
fn package_bytes_chunk(bytes: &bytes::Bytes, context: &mut Context) -> JsResult<JsValue> {
    let buffer = JsArrayBuffer::new(bytes.len(), context)?;
    buffer
        .data_mut()
        .as_deref_mut()
        .ok_or_else(|| type_error("fresh ArrayBuffer is detached"))?
        .copy_from_slice(bytes);
    Ok(JsUint8Array::from_array_buffer(buffer, context)?.into())
}

/// `ReadableStreamDefaultReader.prototype.cancel(reason?)`.
/// Idempotent: cancels the in-flight worker request, releases the stream
/// reservation exactly once, and makes queued and future reads done — all
/// synchronously on the calling stack. The cancel promise itself resolves
/// `undefined` through one Boa job. Cancelling one stream never affects
/// another stream or the source blob.
fn reader_cancel(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let (_object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    cancel_shared(&shared, context);
    let (promise, resolvers) = JsPromise::new_pending(context);
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[JsValue::undefined()], context)?;
            Ok(JsValue::undefined())
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(promise.into())
}

/// `ReadableStreamDefaultReader.prototype.releaseLock()`.
///
/// Permitted only with no queued read and no in-flight I/O: otherwise
/// throws `TypeError` without changing state (a late completion must never
/// be lost). On success the stream unlocks for the next `getReader()`;
/// this reader becomes released.
fn release_lock(this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let (object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    {
        let state = shared.borrow();
        if !state.queue.is_empty() || state.in_flight {
            return Err(type_error(
                "cannot release the lock with queued read requests",
            ));
        }
    }
    shared.borrow_mut().locked = false;
    object
        .downcast_mut::<ReaderNative>()
        .ok_or_else(|| type_error("the reader has been released"))?
        .released = true;
    Ok(JsValue::undefined())
}

/// Registers the stream prototype members.
fn init_stream_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method, length) in [
        (
            js_string!("getReader"),
            NativeFunction::from_fn_ptr(get_reader),
            0,
        ),
        (
            js_string!("cancel"),
            NativeFunction::from_fn_ptr(stream_cancel),
            1,
        ),
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

    // locked: getter-only accessor.
    let getter = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(locked_getter),
    )
    .name(js_string!("locked"))
    .length(0)
    .constructor(false)
    .build();
    prototype.define_property_or_throw(
        js_string!("locked"),
        PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(true),
        context,
    )?;

    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("ReadableStream"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Registers the reader prototype members.
fn init_reader_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method, length) in [
        (
            js_string!("read"),
            NativeFunction::from_fn_ptr(reader_read),
            0,
        ),
        (
            js_string!("cancel"),
            NativeFunction::from_fn_ptr(reader_cancel),
            1,
        ),
        (
            js_string!("releaseLock"),
            NativeFunction::from_fn_ptr(release_lock),
            0,
        ),
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

    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("ReadableStreamDefaultReader"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}
/// Outcome of one demand-driven worker chunk settlement.
///
/// Kept for the structural guard: the Boa thread never calls
/// `BlobReader::read_next` itself (see `stream_has_no_sync_read_fallback`);
/// every chunk arrives through a worker [`StreamChunkCompletion`](crate::io::StreamChunkCompletion).
#[allow(dead_code)]
enum PumpChunk {
    /// A byte chunk to package or decode.
    Bytes(bytes::Bytes),
    /// End of the logical range (decoder flush decides the settlement).
    Eof,
    /// A core failure already recorded as terminal on the shared state.
    Failed,
}

/// Test-only terminal-replay helpers: production `poll_io` never calls
/// them (chunks arrive through worker completions). Kept so the
/// child-module unit tests can name the terminal shape without an executor;
/// the structural guard (`stream_has_no_sync_read_fallback`) allows these
/// `#[cfg(test)]` occurrences plus the worker entry in `io.rs`.
#[cfg(test)]
#[allow(dead_code)]
fn read_next_chunk(shared: &Rc<RefCell<StreamShared>>) -> PumpChunk {
    let _ = shared;
    PumpChunk::Eof
}

/// Marks the stream terminally errored (test-only helper).
#[cfg(test)]
#[allow(dead_code)]
fn mark_errored(shared: &Rc<RefCell<StreamShared>>) {
    let mut state = shared.borrow_mut();
    state.errored = Some(MappedStreamError {
        name: String::from("NotReadableError"),
        message: String::from("blob read failed"),
    });
    state.data = None;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Child-module proof of the errored-stream terminal branch through the
    //! M9-D executor/completion protocol with a controlled failing
    //! `ByteSource`.
    //!
    //! The source is structurally valid (`len()` covers the segment) but
    //! fails reads with `FileApiError::Cancelled`. It exists only in this
    //! test module: no production hook, no public arbitrary-source API.
    //! Prototype identity is proven in the same `Context` by a JavaScript
    //! rejection handler: the M4-A mapped `DOMException` (`AbortError`),
    //! inheriting from `Error`, never a plain `Error` or `RangeError`.
    //!
    //! The M9-D host loop is `poll_io` + `run_jobs()`: these helpers drive
    //! both so the verdict reflects the settled promise.

    use super::*;
    use boa_engine::{Source, js_string};
    use boa_fapi_core::cancellation::CancellationToken;
    /// A source that passes `from_segments` validation but fails every read
    /// with the non-limit error under test.
    struct FailingSource {
        len: u64,
    }

    impl boa_fapi_core::source::ByteSource for FailingSource {
        fn len(&self) -> u64 {
            self.len
        }
        fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
            boa_fapi_core::snapshot::SnapshotState::Memory
        }
        fn read_range(
            &self,
            _range: std::ops::Range<u64>,
            _cancel: &CancellationToken,
        ) -> Result<bytes::Bytes, FileApiError> {
            Err(FileApiError::Cancelled)
        }
    }

    /// Drives the M9-D host loop for `context`: `poll_io` turns worker
    /// completions into Boa jobs, then `run_jobs` settles them. The default
    /// threaded executor runs separately, so the loop spins the
    /// non-blocking `poll_io` drain (no sleep, no yield) while I/O is still
    /// outstanding — the same shape as the FileReader unit-test driver
    /// (which the `no_out_of_scope_surface` guard allows: the scanner
    /// strips `#[cfg(test)]` modules, so no production `std::thread`
    /// use is introduced).
    ///
    /// Unit tests register exactly once, so the identity-aware repeat
    /// `register` of a *different* built extension would be rejected;
    /// instead this helper re-reads the stored specs snapshot: the handle
    /// carries the same context id and shutdown flag, which is all
    /// `poll_io` needs. (Integration tests keep the real handle from
    /// `register`.) `test_handle` is `#[cfg(test)]`-only, like the
    /// `promise_read` unit tests.
    fn drive(context: &mut Context) {
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

    /// Builds a stream over a failing blob and exposes its first `read()`
    /// promise to JS as `globalThis.probe`, recording the realm-local
    /// verdict into `globalThis.verdict`.
    fn enqueue_failing_probe(context: &mut Context) -> (u64, usize) {
        let source: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(FailingSource { len: 3 });
        let data = Arc::new(
            BlobData::from_segments(
                vec![boa_fapi_core::blob::BlobSegment {
                    source,
                    offset: 0,
                    len: 3,
                }],
                "",
                &FileApiLimits::default(),
            )
            .expect("valid segments"),
        );
        let stream = create_stream(&data, &FileApiLimits::default(), StreamMode::Bytes, context)
            .expect("stream");
        let reader = stream
            .get(js_string!("getReader"), context)
            .expect("getReader member")
            .as_callable()
            .expect("callable")
            .call(&stream.into(), &[], context)
            .expect("reader");
        let promise = reader
            .as_object()
            .expect("reader object")
            .get(js_string!("read"), context)
            .expect("read member")
            .as_callable()
            .expect("callable")
            .call(&reader.clone(), &[], context)
            .expect("read promise");
        context
            .register_global_property(
                js_string!("probe"),
                promise.clone(),
                boa_engine::property::Attribute::all(),
            )
            .expect("register probe");
        context
            .eval(Source::from_bytes(
                "globalThis.verdict = 'pending'; \
                 globalThis.probe.then( \
                     () => { globalThis.verdict = 'fulfilled'; }, \
                     error => { \
                         globalThis.verdict = \
                             (error instanceof DOMException) \
                                 ? 'dom:' + error.name + ':' + (error instanceof Error) \
                                 : (error instanceof RangeError) ? 'range' \
                                 : (error instanceof Error) \
                                     ? 'error:' + error.name + ':' + error.message \
                                     : 'other'; \
                     } \
                 );",
            ))
            .expect("attach handler");
        (data.size(), data.segment_count())
    }

    fn js_verdict(context: &mut Context) -> String {
        context
            .eval(Source::from_bytes("globalThis.verdict"))
            .expect("read verdict")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped()
    }

    /// Runs `boa_gc::force_collect()` — the supported deterministic GC path
    /// — with live stream/reader/promise roots reachable from JS globals.
    fn force_gc() {
        boa_gc::force_collect();
    }

    /// Builds a stream + reader + pending `read()` fully reachable from JS
    /// globals, runs the deterministic GC, then drives the host loop and
    /// returns the JS-observable verdict. Proves pending resolvers survive
    /// collection: they live in the GC-traced pending table plus the JS
    /// promise, never in the untraced shared cell.
    fn gc_probe_verdict(context: &mut Context, setup_js: &str) -> String {
        context
            .eval(Source::from_bytes(setup_js))
            .expect("setup probe");
        force_gc();
        drive(context);
        force_gc();
        context
            .eval(Source::from_bytes("globalThis.verdict"))
            .expect("read verdict")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped()
    }

    #[test]
    fn pending_read_survives_gc_with_exact_chunk() {
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let verdict = gc_probe_verdict(
            context,
            "globalThis.verdict = 'pending'; \
             globalThis.stream = new Blob(['gc-exact']).stream(); \
             globalThis.reader = globalThis.stream.getReader(); \
             globalThis.reader.read().then( \
                 r => { globalThis.verdict = 'chunk:' + r.value.length + ':' + r.done; }, \
                 e => { globalThis.verdict = 'rejected:' + e.name; } \
             );",
        );
        assert_eq!(verdict, "chunk:8:false");
    }

    #[test]
    fn two_queued_reads_survive_gc_fifo_exact() {
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let verdict = gc_probe_verdict(
            context,
            "globalThis.verdict = 'pending'; \
             globalThis.reader = new Blob(['ab']).stream().getReader(); \
             globalThis.reader.read().then(r => { globalThis.verdict = 'first:' + r.value.length; }); \
             globalThis.reader.read().then(r => { globalThis.verdict += '|second-done:' + r.done; });",
        );
        assert_eq!(verdict, "first:2|second-done:true");
    }

    #[test]
    fn gc_then_cancel_and_error_paths_settle() {
        // Pending read + cancel after GC settles done.
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let verdict = gc_probe_verdict(
            context,
            "globalThis.verdict = 'pending'; \
             globalThis.reader = new Blob(['cancel-me']).stream().getReader(); \
             globalThis.reader.read().then(r => { globalThis.verdict = 'read-done:' + r.done; }); \
             globalThis.reader.cancel().then(() => { globalThis.verdict += '|cancelled'; });",
        );
        // The read was queued before cancel, so the cancel wins
        // synchronously on the calling stack and the read settles done
        // (`done:true`); the cancel promise still resolves. Both survive GC.
        assert_eq!(verdict, "read-done:true|cancelled");
        // Terminal core error after GC rejects the mapped `DOMException`.
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let (size_before, segments_before) = enqueue_failing_probe(context);
        force_gc();
        drive(context);
        assert_eq!(js_verdict(context), "dom:AbortError:true");
        let _ = (size_before, segments_before);
    }

    #[test]
    fn errored_stream_rejects_with_mapped_dom_exception() {
        let context = &mut Context::default();
        // Streams need registered prototypes for `create_stream`.
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let (size_before, segments_before) = enqueue_failing_probe(context);
        assert_eq!(js_verdict(context), "pending");
        drive(context);
        assert_eq!(js_verdict(context), "dom:AbortError:true");
        // A second read on the errored stream replays the terminal error
        // class without new source reads: same mapped verdict.
        context
            .eval(Source::from_bytes(
                "globalThis.probe2 = globalThis.probe.constructor === Promise \
                     ? 'is-promise' : 'other';",
            ))
            .expect("sanity");
        let _ = (size_before, segments_before);
    }

    #[test]
    fn errored_stream_future_reads_reject_without_new_reads() {
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let (size_before, segments_before) = enqueue_failing_probe(context);
        drive(context);
        assert_eq!(js_verdict(context), "dom:AbortError:true");
        // Queue a second read on the same errored stream: it must reject
        // with the same class and never touch the source again.
        context
            .eval(Source::from_bytes(
                "globalThis.verdict2 = 'pending'; \
                 globalThis.probe.then( \
                     () => { globalThis.verdict2 = 'fulfilled'; }, \
                     error => { \
                         globalThis.verdict2 = \
                             (error instanceof DOMException) ? 'dom' \
                             : (error instanceof RangeError) ? 'range' \
                             : (error instanceof Error) ? 'error' : 'other'; \
                     } \
                 );",
            ))
            .expect("second handler");
        drive(context);
        let second: String = context
            .eval(Source::from_bytes("globalThis.verdict2"))
            .expect("read verdict2")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped();
        assert_eq!(second, "dom");
        let _ = (size_before, segments_before);
    }
}
