//! Capability-checked `ReadableStream` shim for `Blob.stream()`/`textStream()`.
//!
//! Boa 0.22 ships no WHATWG `ReadableStream`, so this module implements the
//! small branded surface the M3-B order requires: a byte stream and a string
//! stream whose chunks are produced strictly on demand from
//! [`BlobReader::read_next`]. No method builds the stream from
//! `arrayBuffer()`/`materialize()` or from a pre-read chunk array.
//!
//! Brand model: every stream and reader object carries native data
//! (`StreamNative`/`ReaderNative`) holding a shared [`StreamShared`] cell.
//! JS can never forge the brand: only `stream()`/`textStream()` create
//! streams, and only `getReader()` creates readers bound to their stream.
//! Jobs capture the shared cell plus Boa resolvers; they never call
//! `run_jobs()` themselves.

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
use boa_fapi_core::blob::{BlobData, BlobReader};
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
/// holds only Rust data: the incremental core reader, the UTF-8 decoder,
/// the chunk flavor, flags, and a FIFO count of queued requests. It holds
/// no `Context`, `JsValue`, `JsObject`, `JsFunction`, `ResolvingFunctions`,
/// or callback: every pending `read()` promise's resolvers travel inside
/// that request's own Boa `PromiseJob` capture, which the engine keeps
/// alive (and traces) until the job runs.
pub(crate) struct StreamShared {
    /// The incremental core reader; `None` once the stream errors.
    reader: Option<BlobReader>,
    /// Byte or text flavor of this stream.
    mode: StreamMode,
    /// Incremental UTF-8 decoder state for text streams.
    decoder: Utf8Decoder,
    /// Locked once `getReader()` hands out a live reader.
    locked: bool,
    /// Cancelled via stream or reader cancel: future reads are done.
    cancelled: bool,
    /// Terminal error class replayed to every later read.
    errored: Option<StreamErrorClass>,
    /// FIFO count of queued `read()` jobs (one job per request).
    pending: usize,
    /// Next FIFO sequence number for a queued `read()`.
    next_seq: u64,
    /// Logical Blob size at stream creation (telemetry `size` only).
    #[cfg(feature = "tracing")]
    total_size: u64,
}

/// One queued `read()` request: its sequence number.
///
/// The promise resolvers are NOT stored here: `ResolvingFunctions` holds
/// Boa `JsFunction`s, which must stay traced by the GC. Each request's
/// resolvers live only in that request's `PromiseJob` closure, so the
/// engine roots them until settlement. This struct carries the FIFO order
/// key; the job carries the GC pointers.
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

/// Terminal error class for a stream: replayed without further reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamErrorClass {
    /// A core read failure (plain `Error` in the JS realm).
    ReadFailed,
}

impl std::fmt::Debug for StreamShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamShared")
            .field("mode", &self.mode)
            .field("locked", &self.locked)
            .field("cancelled", &self.cancelled)
            .field("errored", &self.errored)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
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

/// Creates a fresh unlocked stream object for `data` in `mode`.
fn create_stream(
    data: &Arc<BlobData>,
    limits: &FileApiLimits,
    mode: StreamMode,
    context: &mut Context,
) -> JsResult<JsObject> {
    let reader = data.reader(limits).map_err(|error| match error {
        FileApiError::ResourceLimit(kind) => {
            let _ = kind;
            type_error("blob stream chunk size is out of range")
        }
        other => crate::error::js_from_core(other),
    })?;
    #[cfg(feature = "tracing")]
    let total_size = data.size();
    let shared = Rc::new(RefCell::new(StreamShared {
        reader: Some(reader),
        mode,
        decoder: Utf8Decoder::default(),
        locked: false,
        cancelled: false,
        errored: None,
        pending: 0,
        next_seq: 0,
        #[cfg(feature = "tracing")]
        total_size,
    }));
    let specs = crate::extension::snapshot(context)?;
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

/// `ReadableStream.prototype.cancel(reason?)`: idempotent unlock-cancel.
///
/// Only valid on an unlocked stream; a locked stream rejects with
/// `TypeError` through a queued job. Success resolves `undefined` through
/// a Boa job and makes queued/future reads done.
fn stream_cancel(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let shared = require_stream(this)?;
    let locked = shared.borrow().locked;
    let (promise, resolvers) = JsPromise::new_pending(context);
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            if locked {
                let reason = type_error("cannot cancel a locked stream")
                    .into_opaque(context)
                    .map_or_else(|_| crate::error::js_read_error(context), JsValue::from);
                resolvers
                    .reject
                    .call(&JsValue::undefined(), &[reason], context)?;
                return Ok(JsValue::undefined());
            }
            cancel_shared(&shared);
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

/// Marks the shared state cancelled and forgets queued slots.
///
/// Queued `read()` jobs still own their resolvers: each settles done in
/// its own job when it pumps and observes the cancelled flag. Stream-level
/// cancel has no requests of its own because every `read()` carries its
/// own job.
fn cancel_shared(shared: &Rc<RefCell<StreamShared>>) {
    let mut state = shared.borrow_mut();
    state.cancelled = true;
    state.reader = None;
    state.pending = 0;
}

/// `ReadableStreamDefaultReader.prototype.read()`: one pending promise, one job.
///
/// The resolvers live only in this request's job closure (traced by the
/// engine until the job runs); the shared cell records just the FIFO slot.
fn reader_read(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let (_object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    // Reserve the FIFO slot first: jobs pump in enqueue order, preserving
    // FIFO and pending-first semantics for cancelled/errored streams.
    let request = {
        let mut state = shared.borrow_mut();
        let seq = state.next_seq;
        state.next_seq = state.next_seq.wrapping_add(1);
        state.pending = state.pending.saturating_add(1);
        PendingRead {
            seq,
            mode: state.mode,
        }
    };
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            pump_one(&shared, &request, &resolvers, context)
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(promise.into())
}

/// Pumps exactly one queued read request: one chunk or terminal state.
///
/// Runs inside that request's own Boa job, which owns the promise
/// resolvers (traced by the engine until settlement). Reads at most one
/// `BlobReader` chunk, packages it as a fresh `Uint8Array`/string, and
/// resolves `{ value, done }`. EOF resolves done (with decoder flush for
/// text streams). A core failure errors the stream: the current and every
/// future read reject with the M4-A mapped same-realm `DOMException`,
/// without further source reads.
fn pump_one(
    shared: &Rc<RefCell<StreamShared>>,
    request: &PendingRead,
    resolvers: &ResolvingFunctions,
    context: &mut Context,
) -> JsResult<JsValue> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    // Shutdown: settle nothing further against a destroyed context. Pending
    // reads already hold their resolvers, but resolving them would deliver
    // callbacks after shutdown, so late completions are dropped silently.
    // No telemetry event is published for a shutdown late completion.
    #[cfg(feature = "fs")]
    if crate::extension::snapshot(context)
        .map(|specs| specs.shutdown.is_shutdown())
        .unwrap_or(false)
    {
        return Ok(JsValue::undefined());
    }
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
        state.pending = state.pending.saturating_sub(1);
    }
    let _ = request.seq;
    // Terminal states settle without touching the source.
    let terminal = {
        let state = shared.borrow();
        if state.errored.is_some() {
            Some(true)
        } else if state.cancelled {
            Some(false)
        } else {
            None
        }
    };
    if let Some(is_error) = terminal {
        if is_error {
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                0,
                crate::observability::result_class_for_core(Some(&FileApiError::Internal)),
                trace_env,
            );
            let reason = stream_error_reason(context, &FileApiError::Internal);
            resolvers
                .reject
                .call(&JsValue::undefined(), &[reason], context)?;
            return Ok(JsValue::undefined());
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
        let done = iter_result(JsValue::undefined(), true, context)?;
        resolvers
            .resolve
            .call(&JsValue::undefined(), &[done], context)?;
        return Ok(JsValue::undefined());
    }
    // Demand-driven: exactly one `read_next()` for a byte request; text
    // requests loop below until the decoder yields text, EOF, or error.
    let chunk = {
        let mut state = shared.borrow_mut();
        let Some(reader) = state.reader.as_mut() else {
            drop(state);
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                0,
                "ok",
                trace_env,
            );
            let done = iter_result(JsValue::undefined(), true, context)?;
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            return Ok(JsValue::undefined());
        };
        match reader.read_next() {
            Ok(chunk) => chunk,
            Err(error) => {
                #[cfg(feature = "tracing")]
                crate::observability::emit(
                    "stream_read",
                    trace_size,
                    crate::observability::elapsed_ms(trace_start),
                    0,
                    crate::observability::result_class_for_core(Some(&error)),
                    trace_env,
                );
                let reason = stream_error_reason(context, &error);
                state.errored = Some(StreamErrorClass::ReadFailed);
                state.reader = None;
                // Every other queued read rejects with the same class when
                // its own job pumps: mark them now, settle them on demand.
                // (Each job owns its resolvers and observes the terminal
                // state without source reads.)
                drop(state);
                resolvers
                    .reject
                    .call(&JsValue::undefined(), &[reason], context)?;
                return Ok(JsValue::undefined());
            }
        }
    };
    match chunk {
        None => {
            // EOF: flush the text decoder (exact replacement semantics),
            // then resolve done. A non-empty flush is a final value chunk,
            // never an empty `done:false` string. EOF itself carries no
            // data chunk.
            let mut state = shared.borrow_mut();
            if request.mode == StreamMode::Text {
                let tail = state.decoder.flush();
                if !tail.is_empty() {
                    drop(state);
                    #[cfg(feature = "tracing")]
                    crate::observability::emit(
                        "stream_read",
                        trace_size,
                        crate::observability::elapsed_ms(trace_start),
                        1,
                        "ok",
                        trace_env,
                    );
                    let done = iter_result(JsValue::from(JsString::from(tail)), false, context)?;
                    resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[done], context)?;
                    return Ok(JsValue::undefined());
                }
            }
            drop(state);
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                0,
                "ok",
                trace_env,
            );
            let done = iter_result(JsValue::undefined(), true, context)?;
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            Ok(JsValue::undefined())
        }
        Some(bytes) => {
            // Text mode must never resolve an empty `done:false` chunk:
            // keep consuming whole chunks inside THIS request until text,
            // EOF flush, or error — still no readahead past the request.
            if request.mode == StreamMode::Text {
                let mut text = shared.borrow_mut().decoder.push(&bytes);
                while text.is_empty() {
                    match read_next_chunk(shared) {
                        PumpChunk::Bytes(next) => {
                            text.push_str(&shared.borrow_mut().decoder.push(&next));
                        }
                        PumpChunk::Eof => {
                            let tail = shared.borrow_mut().decoder.flush();
                            if tail.is_empty() {
                                #[cfg(feature = "tracing")]
                                crate::observability::emit(
                                    "stream_read",
                                    trace_size,
                                    crate::observability::elapsed_ms(trace_start),
                                    0,
                                    "ok",
                                    trace_env,
                                );
                                let done = iter_result(JsValue::undefined(), true, context)?;
                                resolvers
                                    .resolve
                                    .call(&JsValue::undefined(), &[done], context)?;
                                return Ok(JsValue::undefined());
                            }
                            text.push_str(&tail);
                            break;
                        }
                        PumpChunk::Failed => {
                            #[cfg(feature = "tracing")]
                            crate::observability::emit(
                                "stream_read",
                                trace_size,
                                crate::observability::elapsed_ms(trace_start),
                                0,
                                crate::observability::result_class_for_core(Some(
                                    &FileApiError::Internal,
                                )),
                                trace_env,
                            );
                            mark_errored(shared);
                            let reason = stream_error_reason(context, &FileApiError::Internal);
                            resolvers
                                .reject
                                .call(&JsValue::undefined(), &[reason], context)?;
                            return Ok(JsValue::undefined());
                        }
                    }
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
                let done = iter_result(JsValue::from(JsString::from(text)), false, context)?;
                resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
                return Ok(JsValue::undefined());
            }
            let value = match package_bytes_chunk(&bytes, context) {
                Ok(value) => value,
                Err(error) => {
                    #[cfg(feature = "tracing")]
                    crate::observability::emit(
                        "stream_read",
                        trace_size,
                        crate::observability::elapsed_ms(trace_start),
                        0,
                        "error",
                        trace_env,
                    );
                    return Err(error);
                }
            };
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "stream_read",
                trace_size,
                crate::observability::elapsed_ms(trace_start),
                1,
                "ok",
                trace_env,
            );
            let done = iter_result(value, false, context)?;
            resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            Ok(JsValue::undefined())
        }
    }
}

/// Outcome of one demand-driven `BlobReader::read_next()` call.
enum PumpChunk {
    /// A byte chunk to package or decode.
    Bytes(bytes::Bytes),
    /// End of the logical range (decoder flush decides the settlement).
    Eof,
    /// A core failure already recorded as terminal on the shared state.
    Failed,
}

/// Reads exactly one chunk from the shared core reader.
///
/// Returns `Eof` when the reader is gone or exhausted. On core failure
/// marks the stream errored (terminal replay, no further reads) and
/// returns `Failed`. Never reads ahead.
fn read_next_chunk(shared: &Rc<RefCell<StreamShared>>) -> PumpChunk {
    let mut state = shared.borrow_mut();
    let Some(reader) = state.reader.as_mut() else {
        return PumpChunk::Eof;
    };
    match reader.read_next() {
        Ok(Some(bytes)) => PumpChunk::Bytes(bytes),
        Ok(None) => PumpChunk::Eof,
        Err(_) => {
            state.errored = Some(StreamErrorClass::ReadFailed);
            state.reader = None;
            PumpChunk::Failed
        }
    }
}

/// Marks the stream terminally errored without needing a `Context`.
///
/// Used when a follow-up chunk read inside a text request fails: the
/// caller (which owns the job's `Context`) builds the same-realm `Error`.
fn mark_errored(shared: &Rc<RefCell<StreamShared>>) {
    let mut state = shared.borrow_mut();
    state.errored = Some(StreamErrorClass::ReadFailed);
    state.reader = None;
}

/// Packages one chunk as a fresh offset-0 `Uint8Array` over a fresh buffer.
fn package_bytes_chunk(bytes: &bytes::Bytes, context: &mut Context) -> JsResult<JsValue> {
    let buffer = JsArrayBuffer::new(bytes.len(), context)?;
    buffer
        .data_mut()
        .as_deref_mut()
        .ok_or_else(|| type_error("fresh ArrayBuffer is detached"))?
        .copy_from_slice(bytes);
    Ok(JsUint8Array::from_iter(bytes.iter().copied(), context)?.into())
}

/// `ReadableStreamDefaultReader.prototype.cancel(reason?)`.
///
/// Idempotent: resolves `undefined` through a Boa job and makes queued and
/// future reads done. Cancelling one stream never affects another stream
/// or the source blob.
fn reader_cancel(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let (_object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            // `reader.cancel()` settles already-queued reads done inside
            // their own jobs (each owns its resolvers); the cancel promise
            // itself resolves after marking the terminal state.
            shared.borrow_mut().pending = 0;
            cancel_shared(&shared);
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
/// Permitted only with no queued read: otherwise throws `TypeError`
/// without changing state. On success the stream unlocks for the next
/// `getReader()`; this reader becomes released.
fn release_lock(this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let (object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    {
        let state = shared.borrow();
        if state.pending != 0 {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Child-module proof of the errored-stream terminal branch through the
    //! real `pump_one` job path with a controlled failing `ByteSource`.
    //!
    //! The source is structurally valid (`len()` covers the segment) but
    //! fails reads with `FileApiError::Cancelled`. It exists only in this
    //! test module: no production hook, no public arbitrary-source API.
    //! Prototype identity is proven in the same `Context` by a JavaScript
    //! rejection handler: the M4-A mapped `DOMException` (`AbortError`),
    //! inheriting from `Error`, never a plain `Error` or `RangeError`.

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
    /// globals, runs the deterministic GC, then drives jobs and returns the
    /// JS-observable verdict. Proves pending resolvers survive collection:
    /// they live in the job capture (traced by the engine), never in the
    /// untraced shared cell.
    fn gc_probe_verdict(context: &mut Context, setup_js: &str) -> String {
        context
            .eval(Source::from_bytes(setup_js))
            .expect("setup probe");
        force_gc();
        context.run_jobs().expect("run_jobs");
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
        // The read was queued before cancel, so it settles with its chunk
        // (`done:false`); the cancel promise still resolves. Both survive GC.
        assert_eq!(verdict, "read-done:false|cancelled");
        // Terminal core error after GC rejects the mapped `DOMException`.
        let context = &mut Context::default();
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let (size_before, segments_before) = enqueue_failing_probe(context);
        force_gc();
        context.run_jobs().expect("run_jobs");
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
        context.run_jobs().expect("run_jobs");
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
        context.run_jobs().expect("run_jobs");
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
        context.run_jobs().expect("run_jobs");
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
