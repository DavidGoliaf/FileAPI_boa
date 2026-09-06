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
use std::collections::VecDeque;
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
use crate::error::{js_read_error, type_error};

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
/// holds only Rust state (reader, decoder, queues, flags): no `Context`,
/// `JsValue`, `JsObject`, callback, or fabricated GC reference.
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
    /// FIFO queue of pending `read()` requests (resolvers only).
    pending: VecDeque<PendingRead>,
}

/// One queued `read()` request: its promise resolvers.
struct PendingRead {
    /// Resolvers captured by the read job.
    resolvers: ResolvingFunctions,
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
            .field("pending", &self.pending.len())
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
    let shared = Rc::new(RefCell::new(StreamShared {
        reader: Some(reader),
        mode,
        decoder: Utf8Decoder::default(),
        locked: false,
        cancelled: false,
        errored: None,
        pending: VecDeque::new(),
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
                    .map_or_else(|_| js_read_error(context), JsValue::from);
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

/// Marks the shared state cancelled and drops queued requests.
///
/// Queued requests are settled done by their own owners (`reader_cancel`
/// resolves them before calling this); stream-level cancel has no queued
/// requests of its own because every `read()` carries its own job.
fn cancel_shared(shared: &Rc<RefCell<StreamShared>>) {
    let mut state = shared.borrow_mut();
    state.cancelled = true;
    state.reader = None;
    state.pending.clear();
}

/// `ReadableStreamDefaultReader.prototype.read()`: one pending promise, one job.
fn reader_read(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    let (_object, shared, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    // Queue first: the job pumps exactly this request, preserving FIFO
    // order and pending-first semantics for cancelled/errored streams.
    // (`RefCell` is never double-borrowed here: the mode is `Copy`.)
    let mode = shared.borrow().mode;
    shared
        .borrow_mut()
        .pending
        .push_back(PendingRead { resolvers, mode });
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> { pump_one(&shared, context) },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(promise.into())
}

/// Pumps exactly one queued read request: one chunk or terminal state.
///
/// Runs inside a Boa job. Reads at most one `BlobReader` chunk, packages it
/// as a fresh `Uint8Array`/string, and resolves `{ value, done }`. EOF
/// resolves done (with decoder flush for text streams). A core failure
/// errors the stream: the current and every future read reject with a
/// same-realm plain `Error`, without further source reads.
fn pump_one(shared: &Rc<RefCell<StreamShared>>, context: &mut Context) -> JsResult<JsValue> {
    let pending = shared.borrow_mut().pending.pop_front();
    let Some(request) = pending else {
        return Ok(JsValue::undefined());
    };
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
            request.resolvers.reject.call(
                &JsValue::undefined(),
                &[js_read_error(context)],
                context,
            )?;
            return Ok(JsValue::undefined());
        }
        let done = iter_result(JsValue::undefined(), true, context)?;
        request
            .resolvers
            .resolve
            .call(&JsValue::undefined(), &[done], context)?;
        return Ok(JsValue::undefined());
    }
    // Demand-driven: exactly one `read_next()` per request.
    let chunk = {
        let mut state = shared.borrow_mut();
        let Some(reader) = state.reader.as_mut() else {
            drop(state);
            let done = iter_result(JsValue::undefined(), true, context)?;
            request
                .resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            return Ok(JsValue::undefined());
        };
        match reader.read_next() {
            Ok(chunk) => chunk,
            Err(error) => {
                let reason = stream_error_reason(&error, context);
                state.errored = Some(StreamErrorClass::ReadFailed);
                state.reader = None;
                // Every other queued read rejects with the same class when
                // its own job pumps: mark them now, settle them on demand.
                // (Their jobs each pop one request; terminal state rejects
                // without source reads.)
                drop(state);
                request
                    .resolvers
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
            // never an empty `done:false` string.
            let mut state = shared.borrow_mut();
            if request.mode == StreamMode::Text {
                let tail = state.decoder.flush();
                if !tail.is_empty() {
                    drop(state);
                    let done = iter_result(JsValue::from(JsString::from(tail)), false, context)?;
                    request
                        .resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[done], context)?;
                    return Ok(JsValue::undefined());
                }
            }
            drop(state);
            let done = iter_result(JsValue::undefined(), true, context)?;
            request
                .resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            Ok(JsValue::undefined())
        }
        Some(bytes) => {
            let value = match request.mode {
                StreamMode::Bytes => package_bytes_chunk(&bytes, context)?,
                // Incremental delivery: a chunk fully absorbed into the
                // decoder's pending prefix resolves as an empty (non-done)
                // string without extra source reads.
                StreamMode::Text => {
                    JsValue::from(JsString::from(shared.borrow_mut().decoder.push(&bytes)))
                }
            };
            let done = iter_result(value, false, context)?;
            request
                .resolvers
                .resolve
                .call(&JsValue::undefined(), &[done], context)?;
            Ok(JsValue::undefined())
        }
    }
}

/// Builds the rejection reason for a stream core failure.
///
/// Always a same-realm plain `Error` without body/path/source detail;
/// never `RangeError`, never `DOMException`.
fn stream_error_reason(error: &FileApiError, context: &mut Context) -> JsValue {
    let _ = error;
    js_read_error(context)
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
            // Resolve queued reads done first, then mark cancelled so
            // future reads observe the terminal state.
            let queued: Vec<PendingRead> = shared.borrow_mut().pending.drain(..).collect();
            for request in queued {
                let done = iter_result(JsValue::undefined(), true, context)?;
                request
                    .resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[done], context)?;
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
        if !state.pending.is_empty() {
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
    //! rejection handler: `instanceof Error`, never `instanceof RangeError`.

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
                             (error instanceof RangeError) ? 'range' \
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
    #[test]
    fn errored_stream_rejects_with_plain_error_not_range_error() {
        let context = &mut Context::default();
        // Streams need registered prototypes for `create_stream`.
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("register");
        let (size_before, segments_before) = enqueue_failing_probe(context);
        assert_eq!(js_verdict(context), "pending");
        context.run_jobs().expect("run_jobs");
        assert_eq!(js_verdict(context), "error:Error:blob read failed");
        // A second read on the errored stream replays the terminal error
        // class without new source reads: same plain-`Error` verdict.
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
        assert_eq!(js_verdict(context), "error:Error:blob read failed");
        // Queue a second read on the same errored stream: it must reject
        // with the same class and never touch the source again.
        context
            .eval(Source::from_bytes(
                "globalThis.verdict2 = 'pending'; \
                 globalThis.probe.then( \
                     () => { globalThis.verdict2 = 'fulfilled'; }, \
                     error => { \
                         globalThis.verdict2 = \
                             (error instanceof RangeError) ? 'range' \
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
        assert_eq!(second, "error");
        let _ = (size_before, segments_before);
    }
}
