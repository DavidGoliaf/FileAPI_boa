//! Promise-returning `Blob` reads: `text()`, `arrayBuffer()`, `bytes()`.
//!
//! Each method validates the Blob brand synchronously, creates a pending
//! `Promise`, and enqueues a [`PromiseJob`] that materializes the blob,
//! packages the result, and settles the promise. Nothing settles on the
//! calling JS stack: settlement is observable only after the embedder runs
//! `Context::run_jobs()`. Jobs never call `run_jobs()` themselves.
//!
//! `File` inherits these methods through `Blob.prototype`; no copies are
//! registered on `File.prototype`.

use std::sync::Arc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::job::{Job, PromiseJob};
use boa_engine::object::builtins::{JsArrayBuffer, JsPromise, JsUint8Array};
use boa_engine::{Context, JsResult, JsString, JsValue};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::limits::FileApiLimits;

use crate::brand;
use crate::error::{js_read_error, range_error, type_error};

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

/// GC-safe job payload: shared immutable blob data plus owned limits.
///
/// Holds no `Context`, `JsValue`, `JsObject`, callback, or mutable state.
/// The promise resolvers travel with the Boa [`PromiseJob`] capture as
/// `JsFunction`s (traced by the job), not inside this payload.
#[derive(Clone, Debug)]
struct ReadRequest {
    /// The blob content to materialize inside the job.
    data: Arc<BlobData>,
    /// Immutable limits snapshot for the materialization call.
    limits: FileApiLimits,
    /// The packaging mode for this read.
    mode: ReadMode,
}

/// Enqueues the settlement job and returns the pending promise.
///
/// `require_blob` must have succeeded before calling: brand failures are
/// synchronous `TypeError`s and never reach this function.
pub(crate) fn read_promise(
    data: Arc<BlobData>,
    limits: &FileApiLimits,
    mode: ReadMode,
    context: &mut Context,
) -> JsResult<JsValue> {
    let (promise, resolvers) = JsPromise::new_pending(context);
    let request = ReadRequest {
        data,
        limits: limits.clone(),
        mode,
    };
    let realm = context.realm().clone();
    let job = PromiseJob::with_realm(
        move |context: &mut Context| -> JsResult<JsValue> {
            settle_read(&request, &resolvers, context)
        },
        realm,
    );
    context.enqueue_job(Job::PromiseJob(job));
    Ok(promise.into())
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

/// Runs inside the promise job: materializes, packages, and settles once.
///
/// Materialization failure rejects: `MaterializeBytes` limit as `RangeError`,
/// every other core/read error as a plain `Error` without path/source/body
/// detail. Packaging failure rejects with the engine error. The promise is
/// settled exactly once; the blob is never mutated.
fn settle_read(
    request: &ReadRequest,
    resolvers: &ResolvingFunctions,
    context: &mut Context,
) -> JsResult<JsValue> {
    let cancel = CancellationToken::new();
    let bytes = match request.data.materialize(&request.limits, &cancel) {
        Ok(bytes) => bytes,
        Err(error) => {
            reject_with(&error, &resolvers.reject, context)?;
            return Ok(JsValue::undefined());
        }
    };
    let value = match package_bytes(request.mode, &bytes, context) {
        Ok(value) => value,
        Err(error) => {
            resolvers.reject.call(
                &JsValue::undefined(),
                &[error_to_value(error, context)],
                context,
            )?;
            return Ok(JsValue::undefined());
        }
    };
    resolvers
        .resolve
        .call(&JsValue::undefined(), &[value], context)?;
    Ok(JsValue::undefined())
}

/// Rejects with the M3-mandated error kind for a materialization failure.
fn reject_with(
    error: &FileApiError,
    reject: &boa_engine::object::builtins::JsFunction,
    context: &mut Context,
) -> JsResult<()> {
    let reason: JsValue = match error {
        FileApiError::ResourceLimit(ResourceLimitKind::MaterializeBytes) => {
            range_error("blob size exceeds the materialization limit")
                .into_opaque(context)
                .map_or_else(|_| js_read_error(context), JsValue::from)
        }
        _ => js_read_error(context),
    };
    reject.call(&JsValue::undefined(), &[reason], context)?;
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
    //! Child-module proof of the rejection-type mapping through the real
    //! `read_promise` → `PromiseJob` → `Context::run_jobs()` path.
    //!
    //! The blob carries a controlled `ByteSource` that is structurally
    //! valid (`len()` covers the segment) but fails reads with
    //! `FileApiError::Cancelled`. The source type exists only in this
    //! test module: no production hook, no public arbitrary-source API.
    //!
    //! Prototype identity (`instanceof`) cannot be proven from Rust alone —
    //! `name` is writable — so each test attaches a JavaScript rejection
    //! handler in the same `Context` before jobs run and reads back its
    //! observable verdict after `run_jobs()`.

    use super::*;
    use boa_engine::{Source, js_string};
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
    /// Returns the input metadata for the unchanged-blob assertion.
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

    /// Reads back the JS handler verdict after jobs have run.
    fn js_verdict(context: &mut Context) -> String {
        context
            .eval(Source::from_bytes("globalThis.verdict"))
            .expect("read verdict")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped()
    }

    #[test]
    fn non_limit_error_rejects_with_plain_error_not_range_error() {
        let data = failing_blob();
        let context = &mut Context::default();
        let (size_before, segments_before) =
            enqueue_probe(context, &data, &FileApiLimits::default());
        // Pending before the queue runs: the handler has not observed the
        // rejection yet.
        assert_eq!(js_verdict(context), "pending");
        context.run_jobs().expect("run_jobs");
        // Prototype identity proven in the originating realm: a plain
        // `Error` with the M3 message, never a `RangeError`.
        assert_eq!(js_verdict(context), "error:Error:blob read failed");
        // No fulfilled text value and unchanged input metadata.
        assert_eq!(data.size(), size_before);
        assert_eq!(data.segment_count(), segments_before);
    }

    #[test]
    fn non_limit_rejection_mapping_is_distinct_from_limit_mapping() {
        // Same job path with an over-limit blob rejects as `RangeError`,
        // proving the two mappings are distinct in the same JS-realm style.
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
        let (size_before, segments_before) = enqueue_probe(context, &data, &tight);
        assert_eq!(js_verdict(context), "pending");
        context.run_jobs().expect("run_jobs");
        assert_eq!(
            js_verdict(context),
            "range",
            "limit mapping must reject with RangeError"
        );
        assert_eq!(data.size(), size_before);
        assert_eq!(data.segment_count(), segments_before);
    }

    #[test]
    fn js_realm_verdict_helper_distinguishes_range_error() {
        // Self-check: the verdict helper distinguishes a plain `Error`
        // from a `RangeError` in the same realm (guards against a vacuous
        // `instanceof` assertion above).
        let context = &mut Context::default();
        let value = context
            .eval(Source::from_bytes(
                "var e = new RangeError('x'); \
                 (e instanceof RangeError) && (e instanceof Error);",
            ))
            .expect("eval");
        assert_eq!(value.as_boolean(), Some(true));
    }
}
