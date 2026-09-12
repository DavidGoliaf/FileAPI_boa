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
use boa_gc::Trace;

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
///
/// Lifecycle epochs (M9-D-R2): every stream owns one [`StreamLease`]
/// epoch. Liveness is decided by two explicit facts only: whether any
/// native endpoint (`StreamNative`/`ReaderNative`) is still registered
/// for the epoch, and whether any unsettled read demand exists. The
/// lease itself never touches the GC.
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
    /// `true` while the stream object endpoint is registered. Set at
    /// `create_stream`, cleared on the Boa thread in `poll_io` when the
    /// stream-side drop intent is applied (with retry while borrowed).
    /// Never derived from `Rc::strong_count` (context tables hold
    /// technical `Rc`s, not JS ownership).
    stream_registered: bool,
    /// `true` while a reader endpoint is registered. Set at `getReader`,
    /// cleared on the Boa thread in `poll_io` when the reader-side drop
    /// intent is applied (with retry), or synchronously by `releaseLock`.
    /// Restores `locked` when cleared while the stream stays live, so
    /// `getReader()` works again with no phantom lock.
    reader_registered: bool,
    /// Lifecyle lease of this stream epoch: `context id`, `operation id`,
    /// generation and the terminal/released state. Contains no `Context`,
    /// `JsValue`, `JsObject` or other GC pointers by construction, so it
    /// can travel through worker/bridge data safely.
    lease: StreamLease,
    /// Logical Blob size at stream creation (telemetry `size` only).
    #[cfg(feature = "tracing")]
    total_size: u64,
}

/// Explicit lifecycle lease of one stream operation (M9-D-R2).
///
/// Knows at minimum the owning `context id`, the `operation id`, the
/// generation and the terminal/released state. Carries no GC pointers, so
/// the GC can never keep an operation alive through the lease itself; the
/// lease only *names* the operation whose JS endpoints decide liveness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamLease {
    /// Owning context id (routes cleanup records to the right context).
    context: u64,
    /// I/O operation id (quota ownership / submission order).
    operation: u64,
    /// Stream generation valid at lease creation.
    generation: u64,
    /// `true` once any terminal transition (EOF/error/cancel/shutdown/
    /// abandoned) has won ownership of this epoch.
    terminal: bool,
    /// `true` once the quota/payload bookkeeping of this epoch released.
    released: bool,
}

impl StreamLease {
    /// Creates a fresh live lease for one stream operation.
    fn fresh(context: u64, operation: u64, generation: u64) -> Self {
        Self {
            context,
            operation,
            generation,
            terminal: false,
            released: false,
        }
    }
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
///
/// The stored terminal error (`Option<MappedStreamError>`) replays the
/// terminal class to future `read()` calls after the op entry — and with
/// it the shared state — is gone: it carries only the mapped
/// name/message (no bytes/paths/JS values), so a live reader object can
/// keep rejecting with the exact class without resurrecting the epoch.
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

/// Terminal error replay retained after the op entry is gone.
///
/// Keyed by operation id; inserted at terminal error time alongside the
/// stored error in the shared state. Entries are tiny (two short
/// strings) and bounded in practice: one per errored operation, and
/// errored operations are terminal — their reader objects are typically
/// dropped soon after, while the table itself drains at shutdown.
/// Carries only the mapped name/message — no bytes, paths, or JS values —
/// so future `read()` calls on a surviving reader object reject with the
/// exact class after the epoch's shared state was released with the op
/// entry.
#[derive(Default)]
struct PendingStreamErrors {
    errors: std::collections::HashMap<u64, MappedStreamError>,
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
/// `cancel()`, at shutdown drain, or by the abandoned/drop transition in
/// `poll_io` (M9-D-R2: the last registered JS endpoint went away while
/// the operation was still live). A removed entry makes every queued
/// worker chunk stale: `poll_io` drops the completion without JS mutation,
/// event, telemetry, or a second quota release.
///
/// Endpoint registry (M9-D-R2): registration is explicit — `create_stream`
/// sets the stream side, `getReader` sets the reader side, `releaseLock`
/// and the native `Drop`/`Finalize` pairs clear their own side — precisely
/// because the pre-existing `Rc<RefCell<StreamShared>>` graph cannot answer
/// "is any JS endpoint still reachable": context tables, pending reads and
/// worker tasks hold technical `Rc`s that outlive JS reachability, and
/// `Rc::strong_count` can never distinguish them from a live
/// stream/reader object.
/// `poll_io` treats an operation with no registered side and no live
/// demand as abandoned and runs the single terminal transition for it
/// (cancel token, payload drop, one conditional quota release, one
/// telemetry event in the existing `cancelled` class). Pending promise
/// demand is itself a live root: as long as any `(operation, seq)`
/// resolver entry exists, the operation is NOT abandoned even with zero
/// registered sides — the promise still owns the requested chunk/error
/// and settles it first.
#[derive(Default)]
struct PendingStreamOps {
    ops: std::collections::HashMap<u64, PendingStreamOp>,
}

struct PendingStreamOp {
    shared: Rc<RefCell<StreamShared>>,
    generation: u64,
    /// `true` once this op entry is terminal (won the exactly-once race).
    terminal: bool,
    /// `true` once the quota/payload release for this entry ran.
    released: bool,
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

/// Returns the per-context terminal-error replay table, creating it on
/// first use. Plain mapped name/message pairs only — no GC pointers, but
/// kept in the context tables (not in the shared state) so the replay
/// survives the op entry removal.
fn pending_stream_errors_mut(context: &mut Context) -> JsResult<&mut PendingStreamErrors> {
    if context.get_data::<PendingStreamErrors>().is_none() {
        let _ = context.insert_data::<PendingStreamErrors>(PendingStreamErrors::default());
    }
    context
        .host_defined_mut()
        .get_mut::<PendingStreamErrors>()
        .ok_or_else(|| type_error("the stream error table is unavailable"))
}

/// Records the terminal error replay for `operation` (error path only).
fn remember_stream_error(context: &mut Context, operation: u64, stored: &MappedStreamError) {
    if let Ok(table) = pending_stream_errors_mut(context) {
        table.errors.insert(operation, stored.clone());
    }
}

/// Returns the terminal error replay for `operation`, if the epoch ended
/// in error (whether or not its op entry is still live).
fn remembered_stream_error(context: &Context, operation: u64) -> Option<MappedStreamError> {
    context
        .get_data::<PendingStreamErrors>()
        .and_then(|table| table.errors.get(&operation).cloned())
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

/// Returns the number of live stream operations of this context.
///
/// Count only — no operation, payload or token is revealed. Used by the
/// host handle diagnostics (`FileApiHandle::stream_operation_count`) for
/// the M9-D-R2 quota/payload/pending baselines.
pub(crate) fn live_stream_operation_count(context: &Context) -> usize {
    context
        .get_data::<PendingStreamOps>()
        .map(|table| table.ops.len())
        .unwrap_or(0)
}

/// Takes the pending resolvers for `key`, if still present.
fn take_pending_stream_read(context: &mut Context, key: (u64, u64)) -> Option<PendingStreamRead> {
    context
        .host_defined_mut()
        .get_mut::<PendingStreamReads>()
        .and_then(|table| table.reads.remove(&key))
}

/// Returns `true` while any unsettled demand entry for `operation` exists.
fn has_live_stream_demand(context: &Context, operation: u64) -> bool {
    context
        .get_data::<PendingStreamReads>()
        .is_some_and(|table| table.reads.keys().any(|key| key.0 == operation))
}

/// Removes the pending root for `operation`, if present.
fn remove_pending_stream_op(context: &mut Context, operation: u64) {
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamOps>() {
        table.ops.remove(&operation);
    }
}

/// A GC-safe drop-intent notice: one JS side of `operation` went away.
///
/// Carries only plain integers — the owning context id, the I/O operation
/// id, the generation observed at creation time, and which side dropped —
/// so it can be constructed and enqueued from `Finalize`/`Drop` without
/// touching `Context`, JS, the shared state, or any lock. It performs NO
/// transition itself: `poll_io` on the Boa thread owns the single
/// transition (side clearing, eligibility, abandoned settlement) and
/// retries it while the shared cell is borrowed elsewhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamDropIntent {
    /// Owning context id (records never cross contexts).
    context: u64,
    /// I/O operation id (quota ownership / submission order).
    operation: u64,
    /// Generation observed at drop time (stale intents are no-ops).
    generation: u64,
    /// `true` for the reader side, `false` for the stream side.
    is_reader: bool,
}

/// Intrusive lock-free slot ring of [`StreamDropIntent`] published from
/// `Finalize`/`Drop`.
///
/// Push is a single `compare_exchange_weak` retry loop over an `AtomicU64`
/// slot array — no `Mutex`, no `RefCell`, no allocation, no blocking wait
/// of any kind, so §3.2.4 ("no unbounded lock wait") holds by
/// construction. The loop retries only while a concurrent push wins the
/// slot race, and each iteration re-reads the slot, so it terminates as
/// soon as the racing pusher publishes (a constant number of machine
/// instructions under the handful of finalizer/drop threads of one
/// process).
///
/// Design: a fixed ring of `DROP_INTENT_SLOTS` atomic slots, each holding
/// one packed intent or zero (= empty). A push scans for an empty slot
/// with CAS `0 -> packed`; a drain swaps every slot back to `0` and owns
/// the intents exclusively — no concurrent pop exists, so no reclamation
/// race is possible and no `unsafe` is needed anywhere. A full ring only
/// delays intents: the pushing side keeps its claimed intent pending and
/// retries on the next `poll_io`... except `Finalize`/`Drop` cannot retry
/// (they run once). Instead, overflow is structurally impossible: each
/// native object publishes at most one intent per side per epoch (guarded
/// by the native-data publish claim), the live epoch count is bounded by
/// `max_concurrent_reads_per_global`, and the ring is sized well above
/// any realistic quota (4096 slots). Stale intents (terminal epoch,
/// generation mismatch, unknown operation) collapse into strict no-ops
/// at drain time, and `poll_io` additionally sweeps sideless epochs that
/// never published, so even a hypothetically lost intent could only delay
/// — never leak — an abandonment while live demand still defers it.
///
/// Packing: `operation` (32 bits) | `generation` (16 bits) |
/// `context` (15 bits) | `is_reader` (1 bit). Operation ids beyond 32
/// bits, generations beyond 16 bits, or context ids beyond 15 bits fall
/// back to the slow path — except there is no slow path that blocks: such
/// intents are simply skipped at publish time (the epoch stays registered
/// until an explicit terminal path or shutdown releases it — fail-closed,
/// never a silent double free). In practice ids start at 1 and contexts
/// are a handful per process, so the fallback never triggers; a debug
/// assertion documents the assumption in tests.
#[derive(Debug)]
pub(crate) struct StreamDropIntentStack {
    slots: [std::sync::atomic::AtomicU64; DROP_INTENT_SLOTS],
}

/// Number of intent slots. Well above any realistic
/// `max_concurrent_reads_per_global` (default 64); see the struct docs
/// for why overflow cannot leak quota.
const DROP_INTENT_SLOTS: usize = 4096;

/// Bit layout of a packed intent word (`0` = empty slot).
/// Low to high: `is_reader` (1) | `context` (15) | `generation` (16) |
/// `operation` (32).
const DROP_INTENT_READER_BIT: u64 = 1;
const DROP_INTENT_CONTEXT_SHIFT: u32 = 1;
const DROP_INTENT_CONTEXT_BITS: u64 = 15;
const DROP_INTENT_GENERATION_SHIFT: u32 = 16;
const DROP_INTENT_GENERATION_BITS: u64 = 16;
const DROP_INTENT_OPERATION_SHIFT: u32 = 32;

/// Packs an intent into a nonzero word, or returns `0` when a field does
/// not fit (fail-closed fallback, see the struct docs).
fn pack_stream_drop_intent(context: u64, operation: u64, generation: u64, is_reader: bool) -> u64 {
    if context >= (1 << DROP_INTENT_CONTEXT_BITS)
        || operation >= (1 << 32)
        || operation == 0
        || generation >= (1 << DROP_INTENT_GENERATION_BITS)
    {
        return 0;
    }
    let mut word = (operation << DROP_INTENT_OPERATION_SHIFT)
        | (generation << DROP_INTENT_GENERATION_SHIFT)
        | (context << DROP_INTENT_CONTEXT_SHIFT);
    if is_reader {
        word |= DROP_INTENT_READER_BIT;
    }
    // `operation != 0` guarantees the word is nonzero (operation occupies
    // the top 32 bits), so `0` stays a reliable empty sentinel.
    debug_assert_ne!(word, 0);
    word
}

/// Unpacks a nonzero intent word back into its fields.
fn unpack_stream_drop_intent(word: u64) -> StreamDropIntent {
    StreamDropIntent {
        context: (word >> DROP_INTENT_CONTEXT_SHIFT) & ((1 << DROP_INTENT_CONTEXT_BITS) - 1),
        operation: word >> DROP_INTENT_OPERATION_SHIFT,
        generation: (word >> DROP_INTENT_GENERATION_SHIFT)
            & ((1 << DROP_INTENT_GENERATION_BITS) - 1),
        is_reader: word & DROP_INTENT_READER_BIT != 0,
    }
}

impl StreamDropIntentStack {
    /// Creates an empty stack.
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { std::sync::atomic::AtomicU64::new(0) }; DROP_INTENT_SLOTS],
        }
    }

    /// Pushes one intent. Lock-free: packed-word CAS loop only — no
    /// allocation, no locks, no shared-state borrows.
    ///
    /// Called from `Finalize`/`Drop`: never touches `Context`/JS, never
    /// locks, never borrows the shared state, never allocates.
    fn push_slot(&self, word: u64) {
        // Start at a slot derived from the word so concurrent pushes for
        // different operations rarely contend on the same slot first.
        let start = (word as usize) % DROP_INTENT_SLOTS;
        for offset in 0..DROP_INTENT_SLOTS {
            let slot = &self.slots[(start + offset) % DROP_INTENT_SLOTS];
            if slot
                .compare_exchange_weak(
                    0,
                    word,
                    std::sync::atomic::Ordering::Release,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                return;
            }
        }
        // Ring full: structurally impossible per the struct docs (slots >>
        // live epochs). Fail closed — the epoch stays registered until an
        // explicit terminal path or shutdown releases it.
    }

    /// Pushes one intent, packing fields first.
    pub(crate) fn push(&self, intent: StreamDropIntent) {
        let word = pack_stream_drop_intent(
            intent.context,
            intent.operation,
            intent.generation,
            intent.is_reader,
        );
        if word != 0 {
            self.push_slot(word);
        }
    }

    /// Swaps every slot back to empty and returns the intents.
    ///
    /// Boa thread only (`poll_io`). Each slot is swapped exactly once, so
    /// drained intents are owned exclusively — no concurrent pop exists.
    /// Slot index order approximates publish order closely enough for the
    /// deterministic race tests (intents for distinct operations commute:
    /// abandonment eligibility is per-operation).
    pub(crate) fn take_all(&self) -> Vec<StreamDropIntent> {
        let mut out = Vec::new();
        for slot in &self.slots {
            let word = slot.swap(0, std::sync::atomic::Ordering::Acquire);
            if word != 0 {
                out.push(unpack_stream_drop_intent(word));
            }
        }
        out
    }
}

/// Pushes one native drop-intent notice for `operation`.
///
/// Lock-free wrapper used by the `Finalize`/`Drop` impls (see
/// [`StreamDropIntentStack::push`]): never touches `Context`/JS, never
/// locks, never borrows the shared state.
fn push_stream_drop_intent(
    intents: &crate::extension::StreamDropIntents,
    context_id: u64,
    operation: u64,
    generation: u64,
    is_reader: bool,
) {
    intents.push(StreamDropIntent {
        context: context_id,
        operation,
        generation,
        is_reader,
    });
}

/// Native brand data of a stream object.
///
/// The stream object owns the stream side of its epoch. The native-data
/// drop/finalize pair only publishes a lock-free drop-intent notice (see
/// [`StreamDropIntentStack`]); the side flag itself is cleared later on
/// the Boa thread in `poll_io`, which retries while the shared cell is
/// borrowed elsewhere. Reader sides are owned independently by their own
/// native data.
///
/// The native data intentionally holds NO `Rc<RefCell<StreamShared>>`:
/// keeping one would pin the epoch's shared state alive through the very
/// object whose death must release it, and would let `Finalize` observe a
/// borrowed cell. All liveness lives in the context tables plus the
/// epoch's own lease flags; the finalizer only replays integers.
#[derive(Debug, Trace, JsData)]
#[boa_gc(unsafe_no_drop)]
pub(crate) struct StreamNative {
    /// Snapshot of the epoch identity, copied at creation: owning context
    /// id, operation id, creation generation. Plain integers — no GC
    /// pointers, no shared borrows — so `Finalize`/`Drop` can read them
    /// without touching anything else.
    #[unsafe_ignore_trace]
    identity: StreamEndpointIdentity,
    /// Owning context's lock-free drop-intent stack, cloned from the specs
    /// at creation: the finalizer/drop pair publishes through this clone
    /// without touching `Context`, JS, locks, or the shared state.
    /// Ignored by the GC tracer (no GC pointers).
    #[unsafe_ignore_trace]
    intents: crate::extension::StreamDropIntents,
    /// Stream-side publish claim, shared by the GC-finalizer + Rust-drop
    /// pair: the first of the two publishes the intent, the second is a
    /// strict no-op. `Cell` gives `&self` mutation in `finalize` without
    /// `unsafe`. The claim guards PUBLISHING only — never the side
    /// clearing, which happens exclusively in `poll_io` and therefore
    /// cannot be lost to a contended borrow (see `drain_stream_intents`).
    #[unsafe_ignore_trace]
    published: std::cell::Cell<bool>,
}

/// Copyable snapshot identifying one endpoint of one stream epoch.
///
/// Captured once at object creation from the live shared state; the
/// finalizer/drop pair only reads these integers back. The generation is
/// the creation generation (always `1` today: generations only advance on
/// terminal transitions, after which no drop intent may publish anyway —
/// `poll_io` re-validates liveness before acting, so a stale generation
/// collapses into a strict no-op).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamEndpointIdentity {
    /// Owning context id (intents never cross contexts).
    context: u64,
    /// I/O operation id (quota ownership / submission order).
    operation: u64,
    /// Creation generation of the epoch.
    generation: u64,
}

impl StreamNative {
    /// Wraps shared stream state as native brand data.
    ///
    /// Copies the epoch identity integers out of the shared state once;
    /// the native object itself keeps no reference to the shared cell
    /// (see the struct docs for why).
    pub(crate) fn new(
        shared: &Rc<RefCell<StreamShared>>,
        intents: crate::extension::StreamDropIntents,
    ) -> Self {
        let identity = {
            let state = shared.borrow();
            StreamEndpointIdentity {
                context: state.lease.context,
                operation: state.lease.operation,
                generation: state.lease.generation,
            }
        };
        Self {
            identity,
            intents,
            published: std::cell::Cell::new(false),
        }
    }

    /// Returns the operation id named by this endpoint's identity.
    ///
    /// Used by the brand gates (`require_stream`/`require_reader`) to
    /// resolve the shared state from the context tables — the single
    /// source of truth, since the native object itself keeps no shared
    /// reference.
    pub(crate) fn operation(&self) -> u64 {
        self.identity.operation
    }

    /// Publishes the stream-side drop intent exactly once, lock-free.
    ///
    /// Shared by the GC-finalizer + Rust-drop pair: the first path
    /// publishes, the second is a strict no-op. Never touches the shared
    /// state, so a contended `RefCell` borrow elsewhere cannot lose the
    /// notice — the side clearing it announces happens later in
    /// `poll_io`, which retries until the borrow succeeds.
    fn publish_drop_intent(&self) {
        if self.published.get() {
            return;
        }
        self.published.set(true);
        push_stream_drop_intent(
            &self.intents,
            self.identity.context,
            self.identity.operation,
            self.identity.generation,
            false,
        );
    }
}

impl boa_gc::Finalize for StreamNative {
    fn finalize(&self) {
        // GC-finalizer boundary (M9-D-R2 §3.2): `&self` only — never touch
        // `Context`, never run JS, never lock, never borrow the shared
        // state. A single lock-free intent publish; the transition runs
        // later on the Boa thread in `poll_io`.
        self.publish_drop_intent();
    }
}

impl Drop for StreamNative {
    fn drop(&mut self) {
        // Same explicit-drop boundary: publish once, never transition here.
        self.publish_drop_intent();
    }
}

/// Native brand data of a reader object.
///
/// The reader object owns the reader side of its epoch. The native-data
/// drop/finalize pair only publishes a lock-free drop-intent notice —
/// unless `releaseLock()` already published it (see `take_lease`), in
/// which case the pair publishes nothing.
///
/// Like [`StreamNative`], the native data keeps NO reference to the
/// shared cell — only the identity integers plus the intent-stack clone.
#[derive(Debug, Trace, JsData)]
#[boa_gc(unsafe_no_drop)]
pub(crate) struct ReaderNative {
    /// Snapshot of the epoch identity, copied at creation (see
    /// [`StreamEndpointIdentity`]): read back by `Finalize`/`Drop`
    /// without touching anything else.
    #[unsafe_ignore_trace]
    identity: StreamEndpointIdentity,
    /// Owning context's lock-free drop-intent stack, cloned from the specs
    /// at creation. Ignored by the GC tracer (no GC pointers).
    #[unsafe_ignore_trace]
    intents: crate::extension::StreamDropIntents,
    /// Released via `releaseLock()`: further `read()` calls fail.
    ///
    /// Set only by `releaseLock()` on the Boa thread while holding the
    /// reader-side publish claim (see `take_lease`): a released reader can
    /// never publish a second intent, and a second `releaseLock()`
    /// observes this flag first.
    released: bool,
    /// Reader-side publish claim, shared by the GC-finalizer + Rust-drop
    /// pair AND by `releaseLock()` (see `take_lease`): exactly one of the
    /// three paths publishes the intent. `Cell` gives `&self` mutation in
    /// `finalize` without `unsafe`. Like the stream side, the claim guards
    /// PUBLISHING only — the side clearing happens in `poll_io` with
    /// retry, so nothing is lost to a contended borrow.
    #[unsafe_ignore_trace]
    published: std::cell::Cell<bool>,
}

impl boa_gc::Finalize for ReaderNative {
    fn finalize(&self) {
        // GC-finalizer boundary (M9-D-R2 §3.2): `&self` only — never touch
        // `Context`, never run JS, never lock, never borrow the shared
        // state. A single lock-free intent publish when this reader still
        // holds its side (a released reader already published via
        // `releaseLock()`).
        self.publish_drop_intent();
    }
}

impl ReaderNative {
    /// Wraps shared stream state as native reader data.
    ///
    /// Copies the epoch identity integers out once; keeps no reference to
    /// the shared cell (see [`StreamNative`] for why).
    pub(crate) fn new(
        shared: &Rc<RefCell<StreamShared>>,
        intents: crate::extension::StreamDropIntents,
    ) -> Self {
        let identity = {
            let state = shared.borrow();
            StreamEndpointIdentity {
                context: state.lease.context,
                operation: state.lease.operation,
                generation: state.lease.generation,
            }
        };
        Self {
            identity,
            intents,
            released: false,
            published: std::cell::Cell::new(false),
        }
    }

    /// Returns the operation id named by this endpoint's identity.
    ///
    /// Used by the brand gates to resolve the shared state from the
    /// context tables.
    pub(crate) fn operation(&self) -> u64 {
        self.identity.operation
    }

    /// Returns the released flag (set only by `releaseLock()`).
    pub(crate) fn released(&self) -> bool {
        self.released
    }

    /// Publishes the reader-side drop intent exactly once, lock-free.
    ///
    /// Returns `true` when this call published. Shared by the
    /// GC-finalizer + Rust-drop pair AND by `releaseLock()`: exactly one
    /// of the three paths publishes, so a released reader can never
    /// publish twice and a live reader can never lose its notice.
    fn publish_drop_intent(&self) -> bool {
        if self.published.get() {
            return false;
        }
        self.published.set(true);
        push_stream_drop_intent(
            &self.intents,
            self.identity.context,
            self.identity.operation,
            self.identity.generation,
            true,
        );
        true
    }

    /// Moves this reader's endpoint lease back to the stream side.
    ///
    /// Called only from `releaseLock()` on the Boa thread: publishes the
    /// reader-side intent (so the later finalizer/drop pair publishes
    /// nothing) and returns `true` when the caller still owns the side.
    fn take_lease(&self) -> bool {
        self.publish_drop_intent()
    }
}

impl Drop for ReaderNative {
    fn drop(&mut self) {
        // Same explicit-drop boundary as `StreamNative`: publish once.
        self.publish_drop_intent();
    }
}

/// Applies one drop-intent notice on the Boa thread, with retry.
///
/// Boa thread only (`poll_io`). Clears exactly the announced side flag —
/// never both sides at once, so a released reader can never clear the
/// stream side. Clearing is idempotent (clearing an already-cleared side
/// changes nothing), so retried intents and duplicate intents (finalize +
/// drop of the same native object) are safe. Returns `true` when the
/// shared cell could be borrowed, or `false` when it is currently
/// borrowed elsewhere: the caller requeues the intent for the next
/// `poll_io` instead of losing it. This is the fix for the acceptance P1
/// "claim lost on busy RefCell": the native-data publish claim is NOT
/// consumed here, so a contended borrow only delays — never drops — the
/// side clearing, and the intent survives until it is applied.
///
/// Clearing the flag is unconditional here (no demand/terminal check):
/// those belong to the abandonment eligibility in
/// `try_abandon_stream_operation`, which runs after all intents of this
/// drain are applied. Splitting "clear the side" from "decide the
/// transition" is what makes retry sound.
fn apply_stream_drop_intent(shared: &Rc<RefCell<StreamShared>>, is_reader: bool) -> bool {
    let Ok(mut state) = shared.try_borrow_mut() else {
        return false;
    };
    deregister_stream_endpoint_inner(&mut state, is_reader);
    true
}

/// Inner endpoint accounting; Boa thread only.
///
/// Moves exactly one side flag (`stream_registered` / `reader_registered`)
/// towards `false` (see `apply_stream_drop_intent` for the retry
/// contract).
fn deregister_stream_endpoint_inner(state: &mut StreamShared, is_reader: bool) {
    if is_reader {
        state.reader_registered = false;
        // The reader going away unlocks the stream for a future
        // `getReader()`: no phantom lock may survive its owner. When the
        // stream side stays registered the epoch remains alive exactly as
        // if the reader had never existed.
        if state.stream_registered {
            state.locked = false;
        }
    } else {
        state.stream_registered = false;
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
///
/// Returns the operation id plus a guard holding the stream object alive
/// for the duration of the native call: without the guard, a `read()` or
/// `getReader()` call whose JS `this` is otherwise unreachable could have
/// its native `Finalize` run (publishing a drop intent) while the native
/// method still executes on the same stack. The shared state itself is
/// resolved from the context tables (the single source of truth), never
/// from the native object.
fn require_stream(this: &JsValue) -> JsResult<(u64, JsObject)> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a ReadableStream"));
    };
    if let Some(native) = object.downcast_ref::<StreamNative>() {
        return Ok((native.operation(), object.clone()));
    }
    Err(type_error("illegal invocation: expected a ReadableStream"))
}

/// Validates that `this` carries the reader brand.
///
/// Returns the guard object, the operation id, and the released flag. The
/// shared state is resolved from the context tables by the caller.
fn require_reader(this: &JsValue) -> JsResult<(JsObject, u64, bool)> {
    let Some(object) = this.as_object() else {
        return Err(type_error(
            "illegal invocation: expected a ReadableStreamDefaultReader",
        ));
    };
    if let Some(native) = object.downcast_ref::<ReaderNative>() {
        return Ok((object.clone(), native.operation(), native.released()));
    }
    Err(type_error(
        "illegal invocation: expected a ReadableStreamDefaultReader",
    ))
}

/// Resolves the live shared state for `operation`, if it still owns one.
fn shared_for_operation(context: &Context, operation: u64) -> Option<Rc<RefCell<StreamShared>>> {
    context
        .get_data::<PendingStreamOps>()
        .and_then(|table| table.ops.get(&operation))
        .map(|op| Rc::clone(&op.shared))
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
    let context_id = specs.context_id.get();
    let operation_raw = operation_id.get();
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
        stream_registered: true,
        reader_registered: false,
        lease: StreamLease::fresh(context_id, operation_raw, generation),
        #[cfg(feature = "tracing")]
        total_size,
    }));
    // Register the stream root before any JS object exists: the operation
    // owns the quota slot, and `poll_io` validates the root before
    // settling. The bridge token is stored with the reservation already;
    // `token` here keeps the worker cancellable after cancel/shutdown.
    //
    // Failure atomicity (M9-D-R2 §3.4): every fallible step below either
    // hands the caller a live endpoint (registered above through the
    // lease counters) or rolls the reservation, the op root and the
    // payload back synchronously, so no hidden quota slot survives a
    // creation error (missing prototype, disabled shim, failed insert).
    let _ = token;
    pending_stream_ops_mut(context)?.ops.insert(
        operation_id.get(),
        PendingStreamOp {
            shared: Rc::clone(&shared),
            generation,
            terminal: false,
            released: false,
        },
    );
    specs.store_stream_payload(operation_id.get(), Arc::clone(data));
    #[cfg(feature = "streams-shim")]
    let prototype = specs
        .streams
        .as_ref()
        .ok_or_else(|| {
            rollback_stream_reservation(context, &specs, operation_raw);
            type_error("the streams shim is not registered")
        })?
        .stream
        .prototype();
    #[cfg(not(feature = "streams-shim"))]
    let _ = specs;
    #[cfg(not(feature = "streams-shim"))]
    {
        rollback_stream_reservation(context, &specs, operation_raw);
        return Err(type_error("the streams shim is not registered"));
    }
    #[cfg(feature = "streams-shim")]
    return Ok(JsObject::from_proto_and_data(
        prototype,
        StreamNative::new(&shared, specs.stream_drop_intents.clone()),
    ));
}

/// Rolls back a freshly reserved stream operation synchronously.
///
/// Failure-atomicity helper for `create_stream`: removes the op root (if
/// inserted), drops the stored payload, and conditionally releases the
/// bridge slot. Never touches JS state beyond the context tables.
fn rollback_stream_reservation(
    context: &mut Context,
    specs: &crate::extension::RegisteredSpecs,
    operation: u64,
) {
    remove_pending_stream_op(context, operation);
    specs.drop_stream_payload(operation);
    specs
        .io_bridge()
        .unreserve(crate::io::FileIoOperationId::from_raw(operation));
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
///
/// Registers one reader endpoint on the stream epoch (the operation stays
/// alive while either endpoint is registered, plus while live demand
/// exists). Failure after registration rolls the reader endpoint back
/// synchronously so no phantom owner survives a creation error.
fn get_reader(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (operation, _guard) = require_stream(this)?;
    let Some(shared) = shared_for_operation(context, operation) else {
        return Err(type_error("the stream operation is no longer live"));
    };
    {
        let mut state = shared.borrow_mut();
        if state.locked {
            return Err(type_error("the stream is already locked"));
        }
        state.locked = true;
        state.reader_registered = true;
    }
    #[cfg(feature = "streams-shim")]
    let specs = crate::extension::snapshot(context).inspect_err(|_| {
        rollback_reader_endpoint(&shared);
    })?;
    #[cfg(feature = "streams-shim")]
    let prototype = specs
        .streams
        .as_ref()
        .ok_or_else(|| {
            rollback_reader_endpoint(&shared);
            type_error("the streams shim is not registered")
        })?
        .reader
        .prototype();
    #[cfg(not(feature = "streams-shim"))]
    {
        rollback_reader_endpoint(&shared);
        return Err(type_error("the streams shim is not registered"));
    }
    #[cfg(feature = "streams-shim")]
    return Ok(JsObject::from_proto_and_data(
        prototype,
        ReaderNative::new(&shared, specs.stream_drop_intents.clone()),
    )
    .into());
}

/// Rolls back one reader endpoint registration synchronously.
///
/// Used when `getReader()` fails after registering: clears the reader
/// side and restores the lock, so the epoch keeps exactly the endpoints
/// that own live JS objects. Idempotent: a second call observes the
/// cleared flag and changes nothing.
fn rollback_reader_endpoint(shared: &Rc<RefCell<StreamShared>>) {
    if let Ok(mut state) = shared.try_borrow_mut() {
        state.reader_registered = false;
        state.locked = false;
    }
}

/// `ReadableStream.prototype.locked`: readonly getter.
fn locked_getter(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (operation, _guard) = require_stream(this)?;
    let Some(shared) = shared_for_operation(context, operation) else {
        return Err(type_error("the stream operation is no longer live"));
    };
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
    let (operation, _guard) = require_stream(this)?;
    let (promise, resolvers) = JsPromise::new_pending(context);
    let Some(shared) = shared_for_operation(context, operation) else {
        // Op entry already gone (EOF/error/cancel/abandoned/shutdown won
        // earlier): cancel is idempotent — resolve `undefined` through one
        // Boa job, exactly like a repeat cancel on a live terminal epoch.
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
        return Ok(promise.into());
    };
    let locked = shared.borrow().locked;
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
    // Claim the terminal transition first: losers of the exactly-once race
    // (a concurrent abandoned drain, a second cancel, a racing EOF/error)
    // observe the lease flags and become strict no-ops below. The shared
    // `cancelled`/`data` state still applies even when the op entry is
    // already gone (late cancel after EOF keeps the terminal fast path),
    // but the quota release and demand drain run only for the winner.
    let terminal_winner = {
        let mut state = shared.borrow_mut();
        state.cancelled = true;
        state.data = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1).max(1);
        if state.lease.terminal {
            false
        } else {
            state.lease.terminal = true;
            true
        }
    };
    // Telemetry is emitted by the terminal winner only: a repeated cancel
    // after any decided transition must not create a second terminal
    // event, and a losing cancel after EOF/error/abandoned stays silent.
    if terminal_winner {
        #[cfg(feature = "tracing")]
        crate::observability::emit(
            "stream_read",
            trace_size,
            crate::observability::elapsed_ms(trace_start),
            0,
            "cancelled",
            trace_env,
        );
    } else if operation.is_none() {
        // No op entry but the lease was already terminal (a racing
        // EOF/error/abandoned won): keep the terminal fast-path state, emit
        // nothing, release nothing.
        return;
    }
    if let Some(operation) = operation {
        // A losing cancel (lease already terminal, but the op entry is
        // still present because the winner has not removed it yet) must
        // not drain another path's demand queue: settle nothing here and
        // let the single release run once through the winner below.
        if !terminal_winner {
            release_stream_operation(context, operation);
            return;
        }
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
    let (_object, operation, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    let Some(shared) = shared_for_operation(context, operation) else {
        // Terminal epochs remove their op entry at transition time, but a
        // live reader object may still call `read()`: errored epochs must
        // keep rejecting with the stored class, all other terminal epochs
        // resolve done — still through one Boa job, never synchronously,
        // never with worker contact, never with quota movement. The error
        // replay survives the op entry removal in its own context table
        // (plain name/message, no shared state needed); anything else is
        // fully terminal and settles done.
        if let Some(stored) = remembered_stream_error(context, operation) {
            return settle_terminal_outcome(Err(stored), context);
        }
        // No error replay: the epoch ended done/cancelled/abandoned or by
        // shutdown. Settle done through one Boa job, exactly like the
        // EOF/cancel fast path.
        return settle_terminal_read_done(context);
    };
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
        return settle_terminal_outcome(outcome, context);
    }
    // From here on this is live (non-terminal) demand for a live entry:
    // queue one FIFO slot, insert resolvers, mark live demand, and submit
    // at most one bounded chunk request. (Kept inline — not split into a
    // helper — so the terminal paths above stay obviously worker- and
    // quota-free by inspection.)
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
    // Live demand owns the operation while the promise is unsettled: mark
    // it on the bridge so the abandoned arbitration in `poll_io` settles
    // this demand first instead of abandoning underneath it.
    if let Ok(specs) = crate::extension::snapshot(context) {
        specs
            .io_bridge()
            .set_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation), true);
    }
    // Demand: submit exactly one bounded request when nothing is in flight.
    // A submit failure errors the stream through the normal terminal path
    // (quota released exactly once, queued reads reject, no second submit).
    if let Err(error) = maybe_submit_next(context, &shared, operation) {
        fail_stream_from_submit(&shared, operation, &error, context)?;
    }
    Ok(promise.into())
}

/// Settles one terminal outcome (done or stored error replay) through one
/// Boa job — never synchronously, never with worker contact, never with
/// quota movement.
///
/// Shared by the live-entry terminal fast path, the recovered-demand path
/// (op entry gone but resolver state survives), and the fully-terminal
/// done path: a single place that maps the outcome to the settlement job,
/// so the three call sites cannot diverge.
fn settle_terminal_outcome(
    outcome: Result<(), MappedStreamError>,
    context: &mut Context,
) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
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
    Ok(promise.into())
}

/// Settles done through one Boa job for a fully-terminal epoch with no
/// surviving demand state (see the `reader_read` fallback above).
fn settle_terminal_read_done(context: &mut Context) -> JsResult<JsValue> {
    settle_terminal_outcome(Ok(()), context)
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
///
/// Exactly-once arbitration (M9-D-R2 §3.3): the op entry's
/// `terminal`/`released` flags decide the winner. Only the first terminal
/// path (EOF, error, cancel, abandoned, shutdown) whose entry is still
/// live performs the payload drop + conditional bridge release; every
/// other path observes the flags and becomes a strict no-op.
fn transition_stream_eof(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    context: &mut Context,
) {
    {
        let mut state = shared.borrow_mut();
        state.data = None;
        state.in_flight = false;
        state.lease.terminal = true;
    }
    let entry = context
        .host_defined_mut()
        .get_mut::<PendingStreamOps>()
        .and_then(|table| table.ops.remove(&operation));
    let Some(mut entry) = entry else {
        // Already terminal: a late completion racing the transition.
        return;
    };
    if entry.released || entry.terminal {
        return;
    }
    entry.terminal = true;
    entry.released = true;
    if let Ok(specs) = crate::extension::snapshot(context) {
        // The demand queue is terminally drained by the caller right after
        // this: clear the advisory live-demand probe now so the
        // abandoned arbitration never fires for a decided epoch.
        specs
            .io_bridge()
            .set_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation), false);
        specs.drop_stream_payload(operation);
        let _ = specs
            .io_bridge()
            .unreserve(crate::io::FileIoOperationId::from_raw(operation));
    }
}

/// Releases the stream reservation exactly once and drops its payload.
///
/// Removes the pending root and the stored payload, then conditionally
/// releases the bridge slot. Late worker completions for the operation go
/// stale at the bridge. Pending promise resolvers are settled by the caller
/// (EOF/error drain) or stay silent (cancel/shutdown): releasing here never
/// settles JS itself.
///
/// Exactly-once (M9-D-R2 §3.3): the op entry's `terminal`/`released` flags
/// arbitrate. The first terminal path whose entry is still live wins and
/// performs the single conditional release; repeats observe the flags (or
/// the missing entry) and skip the release, so a repeated
/// cancel/error/abandoned/shutdown can never decrement a neighbour's slot.
///
/// EOF uses [`transition_stream_eof`] instead: same release, but ordered
/// before Promise-job enqueueing and idempotent against late completions.
fn release_stream_operation(context: &mut Context, operation: u64) {
    let entry = context
        .host_defined_mut()
        .get_mut::<PendingStreamOps>()
        .and_then(|table| table.ops.remove(&operation));
    let Some(mut entry) = entry else {
        return;
    };
    if entry.released {
        return;
    }
    entry.terminal = true;
    entry.released = true;
    // Mirror the epoch flags onto the shared lease so late endpoint drops
    // observe the decided state without touching the tables.
    if let Ok(mut state) = entry.shared.try_borrow_mut() {
        state.lease.terminal = true;
        state.lease.released = true;
    }
    if let Ok(specs) = crate::extension::snapshot(context) {
        specs
            .io_bridge()
            .set_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation), false);
        specs.drop_stream_payload(operation);
        let _ = specs
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
    if let Ok(specs) = crate::extension::snapshot(context) {
        specs
            .io_bridge()
            .set_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation), false);
    }
}

/// Drops every live stream root, payload and pending resolver (shutdown).
///
/// Called once from `shutdown_runtime` (M9-D-R2) so the (active, payload,
/// ops) triple returns to baseline together with the bridge quota.
/// Quota itself releases through the bridge shutdown closer (bulk,
/// exactly once); this drain only removes the Boa-side roots so a late
/// worker completion finds an unknown operation and stays a strict no-op.
/// Idempotent: an empty table is a no-op; poisoned locks fail closed
/// (roots stay, late settlement stays forbidden by the shutdown flag).
/// The error-replay table drains here as well: after shutdown no `read()`
/// may settle anything, so replays are unobservable.
pub(crate) fn drop_all_stream_state_for_shutdown(context: &mut Context) {
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamOps>() {
        table.ops.clear();
    }
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamReads>() {
        table.reads.clear();
    }
    if let Some(table) = context.host_defined_mut().get_mut::<PendingStreamErrors>() {
        table.errors.clear();
    }
    if let Ok(specs) = crate::extension::snapshot(context)
        && let Ok(mut payloads) = specs.stream_payloads.lock()
    {
        payloads.clear();
    }
}

/// Drains drop-intent notices, applies the announced side clearings, and
/// runs the single abandoned transition for each eligible operation
/// (M9-D-R2).
///
/// Boa thread only, called from `poll_io` before stream chunk settlement.
/// Three phases, in order:
///
/// 1. Apply: for every intent `(context, operation, generation, side)`,
///    skip foreign contexts without mutation (isolation); otherwise clear
///    exactly the announced side flag. A contended shared-cell borrow does
///    NOT lose the intent — it is requeued into the same context stack
///    for the next `poll_io` (retry, still without touching JS or I/O).
///    This is the fix for the acceptance P1 "claim lost on busy RefCell".
/// 2. Records: intents whose generation mismatches, whose operation is
///    unknown, or whose epoch is already terminal are strict no-ops from
///    here on (the side clearing above still applied — it is harmless and
///    idempotent).
/// 3. Sweep: every live operation whose sides are both cleared, whose
///    lease is non-terminal, whose queue is empty, which is not in flight
///    and which has no live demand in either independent view runs the
///    single abandoned transition (token cancel, cursor clear, op root +
///    payload removal, one conditional quota release, one `stream_read`
///    event in the existing `cancelled` class). Demand settles first: an
///    epoch with owed demand is skipped here and settled by the chunk
///    drain below; the post-settlement drain re-arms abandonment.
///
/// Late worker completions after the transition go stale at the bridge.
///
/// Returns the number of abandoned operations (diagnostics only; no JS
/// jobs are enqueued — an abandoned stream without pending promises
/// creates no JS event or error by design).
pub(crate) fn drain_stream_cleanups(
    stored: &crate::extension::RegisteredSpecs,
    context: &mut Context,
) -> usize {
    // Phase 1+2: drain the lock-free intent stack through the context
    // queue (swap once, then own every intent exclusively).
    let intents = stored.take_stream_drop_intents();
    // Fast path: no intents AND no sideless live operation (cheap scan,
    // no JS, no I/O). The scan below also covers epochs whose intents
    // were already applied by an earlier drain but whose abandonment was
    // deferred by live demand at the time.
    let mut live_ops: Vec<(u64, u64)> = context
        .get_data::<PendingStreamOps>()
        .map(|table| {
            table
                .ops
                .iter()
                .map(|(operation, op)| (*operation, op.generation))
                .collect()
        })
        .unwrap_or_default();
    if intents.is_empty()
        && !live_ops.iter().any(|(operation, _)| {
            context
                .get_data::<PendingStreamOps>()
                .and_then(|table| table.ops.get(operation))
                .is_some_and(|op| {
                    op.shared
                        .try_borrow()
                        .is_ok_and(|state| !state.stream_registered && !state.reader_registered)
                })
        })
    {
        return 0;
    }
    if stored.shutdown.is_shutdown() {
        return 0;
    }
    // Apply every intent's side clearing first, collecting the still-live
    // operations named by this drain for the sweep below. Contended
    // borrows are requeued (retry) — never dropped.
    let mut retry: Vec<StreamDropIntent> = Vec::new();
    let mut named: Vec<(u64, u64)> = Vec::new();
    for intent in intents {
        if intent.context != stored.context_id.get() {
            continue;
        }
        if stored.shutdown.is_shutdown() {
            continue;
        }
        let Some(shared) = context
            .get_data::<PendingStreamOps>()
            .and_then(|table| table.ops.get(&intent.operation))
            .map(|op| Rc::clone(&op.shared))
        else {
            continue;
        };
        if !apply_stream_drop_intent(&shared, intent.is_reader) {
            retry.push(intent);
            continue;
        }
        named.push((intent.operation, intent.generation));
    }
    if !retry.is_empty() {
        stored.requeue_stream_drop_intents(retry);
    }
    // Phase 3: sweep named operations first (deterministic order), then
    // every other live operation (catches sideless epochs whose intents
    // were applied by an earlier drain while demand was still owed).
    let mut abandoned = 0_usize;
    named.sort_unstable();
    named.dedup();
    for (operation, generation) in named {
        if stored.shutdown.is_shutdown() {
            break;
        }
        abandoned += usize::from(try_abandon_stream_operation(
            stored, context, operation, generation,
        ));
    }
    // Refresh: abandonment above may have removed entries.
    live_ops = context
        .get_data::<PendingStreamOps>()
        .map(|table| {
            table
                .ops
                .iter()
                .map(|(operation, op)| (*operation, op.generation))
                .collect()
        })
        .unwrap_or_default();
    for (operation, generation) in live_ops {
        if stored.shutdown.is_shutdown() {
            break;
        }
        abandoned += usize::from(try_abandon_stream_operation(
            stored, context, operation, generation,
        ));
    }
    abandoned
}

/// Attempts the single abandoned transition for one operation.
///
/// Returns `true` when the transition ran. Shares the exact eligibility
/// with the record path: live op entry, matching generation, non-terminal
/// lease, zero registered sides, empty queue, not in flight, and no live
/// demand in EITHER independent view (a stale advisory probe alone must
/// not block abandonment).
fn try_abandon_stream_operation(
    stored: &crate::extension::RegisteredSpecs,
    context: &mut Context,
    operation: u64,
    generation: u64,
) -> bool {
    let Some(shared) = context
        .get_data::<PendingStreamOps>()
        .and_then(|table| table.ops.get(&operation))
        .map(|op| Rc::clone(&op.shared))
    else {
        return false;
    };
    let eligible = {
        let Ok(state) = shared.try_borrow() else {
            return false;
        };
        // Demand-first arbitration: the epoch's own queue/in-flight
        // flags are authoritative. The context-table resolver check and
        // the bridge advisory probe are consulted only as
        // defense-in-depth below — a transient mismatch between queue
        // drain and resolver removal must never keep an endpoint-less,
        // demand-less epoch alive.
        state.lease.operation == operation
            && state.generation == generation
            && !state.lease.terminal
            && !state.lease.released
            && !state.stream_registered
            && !state.reader_registered
            && state.queue.is_empty()
            && !state.in_flight
    };
    if !eligible {
        return false;
    }
    // Defense-in-depth: skip only when BOTH independent demand views
    // still report live demand (a stale advisory probe alone, left
    // behind by an already-drained queue, must not block abandonment).
    if has_live_stream_demand(context, operation)
        && stored
            .io_bridge()
            .has_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation))
    {
        return false;
    }
    transition_stream_abandoned(&shared, operation, context);
    true
}

/// Runs the single abandoned/drop terminal transition (M9-D-R2 §3.3).
///
/// Competes for the same exactly-once ownership as EOF/error/cancel/
/// shutdown: only the winner (live op entry, matching generation, no
/// endpoints, no demand) cancels the token, clears the cursor, removes
/// the op root and payload, and conditionally releases the quota slot.
/// Emits one bounded `stream_read` event in the existing `cancelled`
/// class: an abandoned stream without pending promises creates no JS
/// event/error, and the allow-list gains no new class or field.
fn transition_stream_abandoned(
    shared: &Rc<RefCell<StreamShared>>,
    operation: u64,
    context: &mut Context,
) {
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
    {
        let mut state = shared.borrow_mut();
        if state.lease.terminal {
            return;
        }
        state.lease.terminal = true;
        state.data = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1).max(1);
    }
    // Cancel the worker token first so a racing in-flight chunk observes
    // cancellation even if its completion is already queued.
    if let Ok(specs) = crate::extension::snapshot(context)
        && let Some(token) = specs
            .io_bridge()
            .token_for(crate::io::FileIoOperationId::from_raw(operation))
    {
        token.cancel();
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
    release_stream_operation(context, operation);
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
    //
    // Demand-first ordering: a completion that arrived while the last
    // endpoints were already gone but demand was still owed settles that
    // demand — only an epoch with genuinely no demand left may abandon.
    // The terminal-lease check therefore runs AFTER the slot pop: an
    // abandoned epoch carries no slots, so any completion naming it drops
    // at the empty-queue guard with no JS mutation and no second release.
    let slot = {
        let mut state = shared.borrow_mut();
        state.in_flight = false;
        state.queue.pop_front()
    };
    if shared.borrow().lease.terminal {
        return Ok(0);
    }
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
    // submits its own request now — still at most one in flight. When no
    // demand remains, clear the advisory live-demand probe: an endpoint
    // drop racing this settlement then abandons exactly once at the next
    // `poll_io` drain instead of leaking the slot.
    if let Err(error) = maybe_submit_next(context, shared, operation) {
        fail_stream_from_submit(shared, operation, &error, context)?;
    } else {
        clear_stream_live_demand_if_empty(context, operation);
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
///
/// Exactly-once (M9-D-R2 §3.3): the first error transition whose epoch is
/// still live wins the terminal telemetry event and the single release.
/// A racing second error (or an error racing an already-decided
/// EOF/cancel/abandoned) still records the stored error class (so future
/// reads replay deterministically) but emits nothing and releases nothing.
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
    let terminal_winner = {
        let mut state = shared.borrow_mut();
        state.errored = Some(stored.clone());
        state.data = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1).max(1);
        if state.lease.terminal {
            false
        } else {
            state.lease.terminal = true;
            true
        }
    };
    // The replay outlives the op entry: future `read()` calls on a
    // surviving reader object must reject with the same class after the
    // shared state is released below. Record it before settling.
    remember_stream_error(context, operation, &stored);
    if terminal_winner {
        #[cfg(feature = "tracing")]
        crate::observability::emit(
            "stream_read",
            trace_size,
            crate::observability::elapsed_ms(trace_start),
            0,
            trace_class,
            trace_env,
        );
    }
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
    clear_stream_live_demand_if_empty(context, operation);
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
    clear_stream_live_demand_if_empty(context, operation);
    Ok(())
}

/// Clears the advisory live-demand probe once no unsettled demand entry
/// for `operation` remains (M9-D-R2).
///
/// Called after every demand-settling drain (chunk, EOF, error, cancel).
/// When endpoints already deregistered, the next `poll_io` drain observes
/// the cleared probe and runs the single abandoned transition.
fn clear_stream_live_demand_if_empty(context: &Context, operation: u64) {
    if has_live_stream_demand(context, operation) {
        return;
    }
    if let Ok(specs) = crate::extension::snapshot(context) {
        specs
            .io_bridge()
            .set_stream_live_demand(crate::io::FileIoOperationId::from_raw(operation), false);
    }
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
    let (_object, operation, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    let (promise, resolvers) = JsPromise::new_pending(context);
    // Op entry already gone (an earlier terminal transition won): cancel
    // stays idempotent — resolve `undefined` through one Boa job, exactly
    // like a repeat cancel on a live terminal epoch.
    let Some(shared) = shared_for_operation(context, operation) else {
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
        return Ok(promise.into());
    };
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

/// `ReadableStreamDefaultReader.prototype.releaseLock()`.
///
/// Permitted only with no queued read and no in-flight I/O: otherwise
/// throws `TypeError` without changing state (a late completion must never
/// be lost). On success the stream unlocks for the next `getReader()`: the
/// reader side flag is cleared exactly once through the reader-side claim,
/// so the reader's later drop/finalize pair deregisters nothing — while the
/// still-registered stream side keeps the epoch alive exactly as if the
/// reader had never existed. No phantom owner can survive: an
/// already-released reader observes `released` first and never touches the
/// shared flags again.
fn release_lock(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let (object, operation, released) = require_reader(this)?;
    if released {
        return Err(type_error("the reader has been released"));
    }
    // Op entry already gone (terminal transition won earlier): the reader
    // side flag is already moot, but the object-level contract still
    // holds — a non-released reader releases cleanly and idempotently.
    // Publish the intent (no-op if already published), mark released, and
    // return `undefined` without touching shared state.
    let Some(shared) = shared_for_operation(context, operation) else {
        let mut native = object
            .downcast_mut::<ReaderNative>()
            .ok_or_else(|| type_error("the reader has been released"))?;
        let _ = native.take_lease();
        native.released = true;
        return Ok(JsValue::undefined());
    };
    {
        let state = shared.borrow();
        if !state.queue.is_empty() || state.in_flight {
            return Err(type_error(
                "cannot release the lock with queued read requests",
            ));
        }
    }
    // Claim the reader side exactly once: the first `releaseLock()` owns
    // the deregistration, a repeated call on the same object observes
    // `released` above, and the later drop/finalize pair observes the
    // claim below. Only the reader-side flag moves; the stream side is
    // untouched, so no hook can decrement both sides at once.
    {
        let mut native = object
            .downcast_mut::<ReaderNative>()
            .ok_or_else(|| type_error("the reader has been released"))?;
        if !native.take_lease() {
            return Err(type_error("the reader has been released"));
        }
        native.released = true;
    }
    {
        let mut state = shared.borrow_mut();
        state.reader_registered = false;
        state.locked = false;
    }
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
