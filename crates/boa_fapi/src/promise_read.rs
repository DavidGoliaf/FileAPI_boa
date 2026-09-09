//! Promise-returning `Blob` reads: `text()`, `arrayBuffer()`, `bytes()`.
//!
//! Each method validates the Blob brand synchronously, reserves quota,
//! creates a pending `Promise`, and submits a [`FileIoTask`](crate::io::FileIoTask)
//! to the context [`FileIoExecutor`](crate::io::FileIoExecutor). The method
//! returns the pending promise before any blocking read runs. A worker
//! materializes bytes without Boa and pushes a Rust-only
//! [`FileIoCompletion`](crate::io::FileIoCompletion); the host drives
//! [`FileApiHandle::poll_io`](crate::FileApiHandle::poll_io) followed by
//! `Context::run_jobs()`, and a Boa job packages and settles the promise.
//! Memory-only blobs resolve through the same queue: the worker still
//! materializes, and settlement still runs as a separate Boa job.
//!
//! `File` inherits these methods through `Blob.prototype`; no copies are
//! registered on `File.prototype`.

use std::sync::Arc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::job::{Job, PromiseJob};
use boa_engine::object::builtins::{JsArrayBuffer, JsPromise, JsUint8Array};
use boa_engine::{Context, JsResult, JsString, JsValue};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;

use crate::brand;
#[cfg(not(feature = "dom-shim"))]
use crate::error::range_error;
use crate::error::{js_read_error, type_error};

/// The read mode captured by a promise job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadMode {
    /// Decode materialized bytes as UTF-8 with replacement semantics.
    Text,
    /// Package materialized bytes into a fresh `ArrayBuffer`.
    ArrayBuffer,
    /// Package materialized bytes into a fresh `Uint8Array` (offset 0).
    Bytes,
}

/// GC-safe pending read: resolvers plus the packaging mode.
///
/// The resolvers travel inside the Boa [`PromiseJob`] capture (traced by
/// the job), never inside a worker task or completion. The worker returns
/// only a Rust DTO; this payload lives in a `PendingReads` table keyed by
/// operation id until `poll_io` settles it.
#[derive(Clone)]
struct PendingRead {
    /// The promise resolvers (JS functions, Boa thread only).
    resolvers: ResolvingFunctions,
    /// The packaging mode for this read.
    mode: ReadMode,
    /// Logical blob size at submit time (telemetry `size` only).
    size: u64,
}

/// Per-context table of pending promise reads awaiting `poll_io`.
///
/// Keyed by opaque operation id; entries hold only Boa-side resolvers and
/// the read mode. Planned for M9-C/M9-D reuse without special-casing: the
/// key is the operation id, the value stays a Boa-side resolver bundle.
#[derive(Default)]
struct PendingReads {
    reads: std::collections::HashMap<u64, PendingRead>,
}

/// Submits the read and returns the pending promise.
///
/// `require_blob` must have succeeded before calling: brand failures are
/// synchronous `TypeError`s and never reach this function.
///
/// Lifecycle: brand/Web IDL validation and size preflight run on the Boa
/// thread first; quota is reserved; the promise and operation record are
/// created; a filesystem-backed materialization task goes to the
/// `FileIoExecutor` and the pending promise returns immediately. Memory
/// reads use the same path (no synchronous `materialize` here). Quota is
/// released exactly once at success, submit failure, I/O error,
/// cancellation or shutdown.
pub(crate) fn read_promise(
    data: Arc<BlobData>,
    limits: &FileApiLimits,
    mode: ReadMode,
    context: &mut Context,
) -> JsResult<JsValue> {
    use crate::io::ReserveError;

    let specs = crate::extension::snapshot(context)?;
    if specs.shutdown.is_shutdown() {
        // Post-shutdown reads reject without creating work: the pending
        // promise settles through one Boa job after `run_jobs()`.
        let (promise, resolvers) = JsPromise::new_pending(context);
        let value: JsValue = promise.into();
        reject_with(&FileApiError::Cancelled, &resolvers.reject, context)?;
        return Ok(value);
    }
    // Size preflight on the Boa thread (before quota reservation): an
    // over-limit blob settles through the typed error path with no worker
    // contact and no quota held, still through one Boa job.
    if data.size() > limits.max_materialize_bytes {
        use boa_fapi_core::error::ResourceLimitKind;
        let (promise, resolvers) = JsPromise::new_pending(context);
        let value: JsValue = promise.into();
        reject_with(
            &FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes),
            &resolvers.reject,
            context,
        )?;
        return Ok(value);
    }
    let bridge = specs.io_bridge();
    let (operation_id, token) = match bridge.reserve() {
        Ok(reserved) => reserved,
        Err(ReserveError::Shutdown) => {
            let (promise, resolvers) = JsPromise::new_pending(context);
            let value: JsValue = promise.into();
            reject_with(&FileApiError::Cancelled, &resolvers.reject, context)?;
            return Ok(value);
        }
        Err(ReserveError::QuotaFull | ReserveError::CompletionFull) => {
            // Queue-full is a typed resource error: the promise settles
            // through the normal error path, quota was never consumed.
            let (promise, resolvers) = JsPromise::new_pending(context);
            let value: JsValue = promise.into();
            reject_with(&FileApiError::TooManyReads, &resolvers.reject, context)?;
            return Ok(value);
        }
    };
    let (promise, resolvers) = JsPromise::new_pending(context);
    let value: JsValue = promise.into();
    pending_mut(context).map(|table| {
        table.reads.insert(
            operation_id.get(),
            PendingRead {
                resolvers,
                mode,
                size: data.size(),
            },
        );
    })?;
    let task = bridge.task_for(operation_id, token, Arc::clone(&data), limits.clone());
    if let Err(error) = bridge.submit_guarded(task) {
        // Submit failure: release the reservation exactly once and settle
        // the pending read through the typed error path (one Boa job).
        bridge.unreserve(operation_id);
        if let Some(pending) = take_pending_resolvers(context, operation_id.get()) {
            let mapped = match error {
                crate::io::FileIoSubmitError::WorkerLost => FileApiError::Internal,
                crate::io::FileIoSubmitError::QueueFull
                | crate::io::FileIoSubmitError::Shutdown => FileApiError::TooManyReads,
            };
            reject_with(&mapped, &pending.resolvers.reject, context)?;
        }
        return Ok(value);
    }
    Ok(value)
}

/// `Blob.prototype.text()`: decode the blob as UTF-8 with replacement.
pub(crate) fn text(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let limits = snapshot_limits(context)?;
    read_promise(data, &limits, ReadMode::Text, context)
}

/// `Blob.prototype.arrayBuffer()`: fresh `ArrayBuffer` copy of the bytes.
pub(crate) fn array_buffer(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let limits = snapshot_limits(context)?;
    read_promise(data, &limits, ReadMode::ArrayBuffer, context)
}

/// `Blob.prototype.bytes()`: fresh `Uint8Array` (offset 0) copy of the bytes.
pub(crate) fn bytes(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let limits = snapshot_limits(context)?;
    read_promise(data, &limits, ReadMode::Bytes, context)
}

/// Reads the registered limits snapshot for the current context.
fn snapshot_limits(context: &Context) -> JsResult<FileApiLimits> {
    Ok(crate::extension::snapshot(context)?.limits().clone())
}

/// Returns the per-context pending-read table, creating it on first use.
///
/// `PendingReads` holds `ResolvingFunctions` (`JsFunction`s) and therefore
/// must live in the GC-traced `HostDefined` area: `insert_data` stores it
/// there (see `Context::insert_data` → `HostDefined<dyn Any>`), so the
/// resolvers stay rooted until `poll_io` settles them.
fn pending_mut(context: &mut Context) -> JsResult<&mut PendingReads> {
    if context.get_data::<PendingReads>().is_none() {
        let _ = context.insert_data::<PendingReads>(PendingReads::default());
    }
    let table = context
        .host_defined_mut()
        .get_mut::<PendingReads>()
        .ok_or_else(|| type_error("the promise read queue is unavailable"))?;
    Ok(table)
}

/// Removes a pending read without settling it.
#[allow(dead_code)]
fn remove_pending(context: &mut Context, operation: u64) {
    if let Some(table) = context.host_defined_mut().get_mut::<PendingReads>() {
        table.reads.remove(&operation);
    }
}

/// Takes the pending read for `operation`, if still present.
fn take_pending_resolvers(context: &mut Context, operation: u64) -> Option<PendingRead> {
    context
        .host_defined_mut()
        .get_mut::<PendingReads>()
        .and_then(|table| table.reads.remove(&operation))
}

/// Settles one I/O completion from the Boa thread.
///
/// Called only from `poll_io` after context/generation/shutdown
/// validation: packages the bytes (or the typed error) and enqueues a Boa
/// settlement job. Never calls user JS directly and never runs under a
/// mutex.
pub(crate) fn settle_completion(
    operation_id: crate::io::FileIoOperationId,
    result: Result<bytes::Bytes, FileApiError>,
    size: u64,
    context: &mut Context,
) -> JsResult<()> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    let Some(pending) = take_pending_resolvers(context, operation_id.get()) else {
        // Stale completion (cancelled, restarted, or shut down): drop it.
        return Ok(());
    };
    let PendingRead {
        resolvers,
        mode,
        size: pending_size,
    } = pending;
    let _ = (size, pending_size);
    match result {
        Ok(bytes) => {
            let value = match package_bytes(mode, &bytes, context) {
                Ok(value) => value,
                Err(error) => {
                    #[cfg(feature = "tracing")]
                    {
                        let specs_hash = crate::extension::snapshot(context)
                            .ok()
                            .map(|specs| crate::observability::environment_hash_for_specs(&specs))
                            .unwrap_or(0);
                        let chunks = if bytes.is_empty() { 0 } else { 1 };
                        crate::observability::emit(
                            "promise_read",
                            pending_size,
                            crate::observability::elapsed_ms(trace_start),
                            chunks,
                            "error",
                            specs_hash,
                        );
                    }
                    let realm = context.realm().clone();
                    let job = PromiseJob::with_realm(
                        move |context: &mut Context| -> JsResult<JsValue> {
                            resolvers.reject.call(
                                &JsValue::undefined(),
                                &[error_to_value(error, context)],
                                context,
                            )?;
                            Ok(JsValue::undefined())
                        },
                        realm,
                    );
                    context.enqueue_job(Job::PromiseJob(job));
                    return Ok(());
                }
            };
            #[cfg(feature = "tracing")]
            {
                let specs_hash = crate::extension::snapshot(context)
                    .ok()
                    .map(|specs| crate::observability::environment_hash_for_specs(&specs))
                    .unwrap_or(0);
                let chunks = if pending_size == 0 { 0 } else { 1 };
                crate::observability::emit(
                    "promise_read",
                    pending_size,
                    crate::observability::elapsed_ms(trace_start),
                    chunks,
                    "ok",
                    specs_hash,
                );
            }
            let realm = context.realm().clone();
            let job = PromiseJob::with_realm(
                move |context: &mut Context| -> JsResult<JsValue> {
                    resolvers
                        .resolve
                        .call(&JsValue::undefined(), &[value], context)?;
                    Ok(JsValue::undefined())
                },
                realm,
            );
            context.enqueue_job(Job::PromiseJob(job));
            Ok(())
        }
        Err(error) => {
            #[cfg(feature = "tracing")]
            {
                let specs_hash = crate::extension::snapshot(context)
                    .ok()
                    .map(|specs| crate::observability::environment_hash_for_specs(&specs))
                    .unwrap_or(0);
                crate::observability::emit(
                    "promise_read",
                    pending_size,
                    crate::observability::elapsed_ms(trace_start),
                    0,
                    crate::observability::result_class_for_core(Some(&error)),
                    specs_hash,
                );
            }
            let realm = context.realm().clone();
            let job = PromiseJob::with_realm(
                move |context: &mut Context| -> JsResult<JsValue> {
                    reject_with(&error, &resolvers.reject, context)?;
                    Ok(JsValue::undefined())
                },
                realm,
            );
            context.enqueue_job(Job::PromiseJob(job));
            Ok(())
        }
    }
}

/// Rejects with the M4-A `DOMException` mapping for a materialization
/// failure: `ResourceLimit` → same-realm `QuotaExceededError`, every other
/// core failure → the centrally mapped `DOMException` name. Without the
/// `dom-shim` feature (M4-A off) the pre-M4 mapping applies instead:
/// `MaterializeBytes` → `RangeError`, every other failure → plain `Error`.
///
/// Settles through exactly one Boa promise job (never synchronously): the
/// caller created a pending promise and this helper enqueues its
/// settlement, so the M3 pending-then-`run_jobs` shape holds on every
/// path, including fast preflights. Worker completions settle through
/// `settle_completion` → one Boa job instead.
fn reject_with(
    error: &FileApiError,
    reject: &boa_engine::object::builtins::JsFunction,
    context: &mut Context,
) -> JsResult<()> {
    // Settles through exactly one Boa promise job: the caller created a
    // pending promise, so `run_jobs()` delivers the rejection and the M3
    // pending-then-settle shape holds on every path.
    let reason: JsValue = {
        #[cfg(feature = "dom-shim")]
        {
            let (name, message) = crate::dom::map_core_error(error);
            crate::extension::snapshot(context)
                .ok()
                .and_then(|specs| specs.dom_specs())
                .map(|dom| JsValue::from(crate::dom::construct_exception(&dom, name, message)))
                .unwrap_or_else(|| js_read_error(context))
        }
        #[cfg(not(feature = "dom-shim"))]
        {
            match error {
                FileApiError::ResourceLimit(
                    boa_fapi_core::error::ResourceLimitKind::MaterializeBytes,
                ) => range_error("blob size exceeds the materialization limit")
                    .into_opaque(context)
                    .map_or_else(|_| js_read_error(context), JsValue::from),
                _ => js_read_error(context),
            }
        }
    };
    let reject = reject.clone();
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            reject.call(&JsValue::undefined(), &[reason], context)?;
            Ok(JsValue::undefined())
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(())
}

/// Converts an engine packaging error into a rejection reason value.
fn error_to_value(error: boa_engine::JsError, context: &mut Context) -> JsValue {
    error
        .into_opaque(context)
        .map_or_else(|_| js_read_error(context), JsValue::from)
}

/// Packages materialized bytes into the mode-specific fresh JS value.
///
/// Every call allocates an independent backing store: mutating one result
/// can never affect another read or the source blob.
fn package_bytes(mode: ReadMode, bytes: &bytes::Bytes, context: &mut Context) -> JsResult<JsValue> {
    match mode {
        ReadMode::Text => {
            // UTF-8 with replacement: invalid/truncated sequences → U+FFFD.
            Ok(JsValue::from(JsString::from(
                String::from_utf8_lossy(bytes).into_owned(),
            )))
        }
        ReadMode::ArrayBuffer => {
            let len = bytes.len();
            let buffer = JsArrayBuffer::new(len, context)?;
            buffer
                .data_mut()
                .as_deref_mut()
                .ok_or_else(|| type_error("fresh ArrayBuffer is detached"))?
                .copy_from_slice(bytes);
            Ok(buffer.into())
        }
        ReadMode::Bytes => {
            let array = JsUint8Array::from_iter(bytes.iter().copied(), context)?;
            Ok(array.into())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Child-module proof of the M4-A rejection-type mapping through the
    //! real `read_promise` → `PromiseJob` → `Context::run_jobs()` path.
    //!
    //! After M4-A the `DOMException` exists, so both limit and non-limit
    //! failures reject with the central mapped `DOMException`. The blob
    //! carries a controlled `ByteSource` that is structurally valid (`len()`
    //! covers the segment) but fails reads with `FileApiError::Cancelled`.
    //! The source type exists only in this test module: no production hook,
    //! no public arbitrary-source API.
    //!
    //! Prototype identity (`instanceof`) cannot be proven from Rust alone —
    //! `name` is writable — so each test attaches a JavaScript rejection
    //! handler in the same `Context` before jobs run and reads back its
    //! observable verdict after `run_jobs()`. The extension is registered
    //! first so the same-realm `DOMException` prototype exists.

    use super::*;
    use boa_engine::{Source, js_string};
    use boa_fapi_core::cancellation::CancellationToken;
    use boa_fapi_core::snapshot::SnapshotState;
    use boa_fapi_core::source::ByteSource;
    use boa_fapi_core::source::memory::MemorySource;

    /// A source that passes `from_segments` validation but fails every read
    /// with the non-limit error under test.
    struct FailingSource {
        len: u64,
    }

    impl ByteSource for FailingSource {
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
            Err(FileApiError::Cancelled)
        }
    }

    fn failing_blob() -> Arc<BlobData> {
        let source: Arc<dyn ByteSource> = Arc::new(FailingSource { len: 3 });
        Arc::new(
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
        )
    }

    /// Enqueues a `read_promise` job and exposes its promise to JS as
    /// `globalThis.probe`, with a rejection handler recording the
    /// realm-local verdict into `globalThis.verdict`.
    ///
    /// The extension must be registered before calling: the rejection is a
    /// same-realm `DOMException`, so the verdict also probes
    /// `DOMException` inheritance from `Error`.
    ///
    /// Returns the input metadata for the unchanged-blob assertion.
    ///
    /// The M9-B host loop is `poll_io` + `run_jobs()`: this helper drives
    /// both so the verdict reflects the settled promise.
    fn enqueue_probe(
        context: &mut Context,
        data: &Arc<BlobData>,
        limits: &FileApiLimits,
    ) -> (u64, usize) {
        let promise =
            read_promise(Arc::clone(data), limits, ReadMode::Text, context).expect("enqueue read");
        context
            .register_global_property(
                js_string!("probe"),
                promise,
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
                                 ? 'dom:' + error.name + ':' + error.message \
                                     + ':' + (error instanceof Error) \
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

    /// Registers the extension into `context` (same-realm `DOMException`).
    fn register(context: &mut Context) {
        crate::extension::FileApiExtension::builder()
            .build()
            .register(context)
            .expect("registration failed");
    }

    /// Reads back the JS handler verdict after jobs have run.
    fn js_verdict(context: &mut Context) -> String {
        context
            .eval(Source::from_bytes("globalThis.verdict"))
            .expect("read verdict")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped()
    }

    /// Drives the M9-B host loop for `context`: `poll_io` then jobs,
    /// until quiescent. The default executor runs separately, so polling
    /// alone cannot guarantee it receives a time slice on every supported
    /// platform. The bounded wait is a test hang guard, not a synchronous
    /// I/O path.
    fn drive(context: &mut Context) {
        let handle = poll_handle(context);
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
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
                while handle.has_pending_io() {
                    let _ = handle.poll_io(context);
                    if !handle.has_pending_io() {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }
    }

    /// Returns the handle of the registration in `context`.
    ///
    /// Unit tests register exactly once, so the identity-aware repeat
    /// `register` of a *different* built extension would be rejected;
    /// instead this helper re-reads the stored specs snapshot: the handle
    /// carries the same context id and shutdown flag, which is all
    /// `poll_io` needs. (Integration tests keep the real handle from
    /// `register`.)
    fn poll_handle(context: &Context) -> crate::FileApiHandle {
        crate::extension::snapshot(context)
            .expect("registered")
            .test_handle()
    }

    #[test]
    fn non_limit_error_rejects_with_mapped_dom_exception() {
        let data = failing_blob();
        let context = &mut Context::default();
        register(context);
        let (size_before, segments_before) =
            enqueue_probe(context, &data, &FileApiLimits::default());
        // Pending before the queue runs: the handler has not observed the
        // rejection yet.
        assert_eq!(js_verdict(context), "pending");
        drive(context);
        // Prototype identity proven in the originating realm: the M4-A
        // central mapping turns `Cancelled` into `AbortError`, inheriting
        // from `Error` — never a plain `Error` or `RangeError`.
        assert_eq!(
            js_verdict(context),
            "dom:AbortError:the read was aborted:true"
        );
        // No fulfilled text value and unchanged input metadata.
        assert_eq!(data.size(), size_before);
        assert_eq!(data.segment_count(), segments_before);
    }

    #[test]
    fn limit_error_rejects_with_quota_exceeded_dom_exception() {
        // Same job path with an over-limit blob rejects with the M4-A
        // `ResourceLimit` mapping (`QuotaExceededError`), proving the two
        // mappings are distinct in the same JS-realm style.
        let big = BlobData::from_segments(
            vec![boa_fapi_core::blob::BlobSegment {
                source: Arc::new(MemorySource::new(bytes::Bytes::copy_from_slice(b"hello"))),
                offset: 0,
                len: 5,
            }],
            "",
            &FileApiLimits::default(),
        )
        .expect("valid blob");
        let mut tight = FileApiLimits::default();
        tight.max_materialize_bytes = 4;
        let data = Arc::new(big);
        let context = &mut Context::default();
        register(context);
        let (size_before, segments_before) = enqueue_probe(context, &data, &tight);
        assert_eq!(js_verdict(context), "pending");
        drive(context);
        assert_eq!(
            js_verdict(context),
            "dom:QuotaExceededError:the operation exceeds the configured quota:true",
            "limit mapping must reject with QuotaExceededError"
        );
        assert_eq!(data.size(), size_before);
        assert_eq!(data.segment_count(), segments_before);
    }

    #[test]
    fn js_realm_verdict_helper_distinguishes_dom_exception() {
        // Self-check: the verdict helper distinguishes a `DOMException`
        // from a plain `Error` in the same realm (guards against a vacuous
        // `instanceof` assertion above).
        let context = &mut Context::default();
        register(context);
        let value = context
            .eval(Source::from_bytes(
                "var e = new DOMException('x', 'NotReadableError'); \
                 (e instanceof DOMException) && (e instanceof Error) \
                 && !(e instanceof RangeError) && e.name === 'NotReadableError';",
            ))
            .expect("eval");
        assert_eq!(value.as_boolean(), Some(true));
    }
}
