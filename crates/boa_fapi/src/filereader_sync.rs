//! Synchronous `FileReaderSync` for worker environments (M4-B).
//!
//! Implements the normative `FileReaderSync` surface over the shared
//! memory-backed packaging in [`crate::package`]: four fully synchronous
//! read methods returning the result or throwing a same-realm
//! `DOMException`. No `Promise`, no `FileReader` state machine, no
//! `ProgressEvent`, no Boa job, no File Reading task, and no
//! `Context::run_jobs()` inside any method.
//!
//! Every operation preflights before touching the source: Blob/File brand
//! and size first, then `size > max_sync_read_bytes` as `QuotaExceededError`
//! (the fixed M4-A mapping choice), then the checked data-URL length —
//! only then does the bounded `BlobData::materialize` run. Source failures
//! map through the central [`crate::dom::map_core_error`] table with no
//! path, source, or byte details and no partial JS result. The async
//! `max_concurrent_reads_per_global` quota is never touched.
//!
//! The constructor is installed only for the `DedicatedWorker` and
//! `SharedWorker` environment descriptors (see
//! [`crate::FileApiEnvironment`]); `Window` and `ServiceWorker` contexts
//! never see the global name.

use std::sync::Arc;

use boa_engine::Context;
use boa_engine::JsValue;
use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::object::builtins::JsArrayBuffer;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{JsData, JsError, JsResult, JsString, JsSymbol, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::dom::{self, DomSpecs};
use crate::error::type_error;
use crate::package::{self, TextEncoding};
use crate::webidl::dom_string;

/// Constructor/prototype pair installed as the `FileReaderSync` global in
/// worker environments.
#[derive(Clone)]
pub(crate) struct FileReaderSyncSpecs {
    /// `FileReaderSync` constructor.
    pub(crate) sync: StandardConstructor,
}

/// Builds the `FileReaderSync` class pair without touching any global.
///
/// Installation stays atomic in `extension.rs`.
pub(crate) fn build_sync_specs(context: &mut Context) -> JsResult<FileReaderSyncSpecs> {
    use boa_engine::native_function::NativeFunction;

    let mut builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(filereader_sync_constructor),
    );
    builder.name("FileReaderSync");
    builder.length(0);
    let sync = builder.build();
    init_sync_prototype(&sync.prototype(), context)?;
    Ok(FileReaderSyncSpecs { sync })
}

/// The internal `FileReaderSync` brand: stateless.
///
/// Sync reads hold no operation state: every method runs to completion on
/// the calling stack. JS can never forge the brand: only the constructor
/// creates branded objects.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct FileReaderSyncNative;

/// Validates that `this` carries the `FileReaderSync` brand.
fn require_sync(this: &JsValue) -> JsResult<JsObject> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a FileReaderSync"));
    };
    if object.is::<FileReaderSyncNative>() {
        return Ok(object.clone());
    }
    Err(type_error("illegal invocation: expected a FileReaderSync"))
}

/// `new FileReaderSync()`: allowed only with `new`.
fn filereader_sync_constructor(
    new_target: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = crate::extension::snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("FileReaderSync constructor requires 'new'"));
    };
    let prototype =
        crate::blob::constructor_prototype(&target, specs.sync_reader_proto(), context)?;
    Ok(JsObject::from_proto_and_data(prototype, FileReaderSyncNative).into())
}

/// Throws a same-realm `DOMException` for a core failure.
fn throw_mapped(dom: &DomSpecs, error: &FileApiError) -> JsError {
    let (name, message) = dom::map_core_error(error);
    JsError::from_opaque(JsValue::from(dom::construct_exception(dom, name, message)))
}

/// Throws a same-realm `DOMException` with an explicit name.
fn throw_named(dom: &DomSpecs, name: &str, message: &str) -> JsError {
    JsError::from_opaque(JsValue::from(dom::construct_exception(dom, name, message)))
}

/// Rejects when `args` holds no Blob/​File.
fn blob_arg(args: &[JsValue]) -> JsResult<Arc<BlobData>> {
    if args.is_empty() {
        return Err(type_error("read requires a Blob argument"));
    }
    brand::require_blob(&args[0])
}

/// The shared synchronous preamble: brand, argument, label, size, and
/// length preflights, then the bounded materialization.
///
/// The order is fixed and documented: brand → Blob argument → encoding
/// label → sync-size limit → source read → (data-URL length inside the
/// caller, before its output allocation). No `ByteSource::read_range` runs
/// and no output buffer is allocated before the brand, label, and size
/// preflights succeed; no partial JS result ever escapes. The async
/// `max_concurrent_reads_per_global` quota is never consulted.
fn read_bytes_sync(
    this: &JsValue,
    args: &[JsValue],
    label: Option<&str>,
    context: &mut Context,
) -> JsResult<(bytes::Bytes, DomSpecs, TextEncoding)> {
    let _ = require_sync(this)?;
    let data = blob_arg(args)?;
    let specs = crate::extension::snapshot(context)?;
    let dom = specs
        .dom_specs()
        .ok_or_else(|| type_error("the DOM shim is not registered"))?;
    let limits = specs.limits().clone();
    // Encoding labels resolve before the size preflight (same order as the
    // async `readAsText`): an unknown label throws `EncodingError` even
    // for an oversized blob, with no read and no partial result.
    let Some(encoding) = package::resolve_label(label) else {
        return Err(throw_named(&dom, "EncodingError", "unknown text encoding"));
    };
    // Sync-size preflight before any source read or output allocation.
    if data.size() > limits.max_sync_read_bytes {
        return Err(throw_named(
            &dom,
            "QuotaExceededError",
            "the read exceeds the synchronous read limit",
        ));
    }
    let bytes = data
        .materialize(&limits, &CancellationToken::new())
        .map_err(|error| throw_mapped(&dom, &error))?;
    Ok((bytes, dom, encoding))
}

/// `readAsArrayBuffer(blob)`: fresh independent `ArrayBuffer` or a
/// same-realm `DOMException`. `length = 1`.
fn read_as_array_buffer_sync(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let (bytes, _, _) = read_bytes_sync(this, args, None, context)?;
    let buffer = JsArrayBuffer::new(bytes.len(), context)?;
    buffer
        .data_mut()
        .as_deref_mut()
        .ok_or_else(|| type_error("fresh ArrayBuffer is detached"))?
        .copy_from_slice(&bytes);
    Ok(buffer.into())
}

/// `readAsBinaryString(blob)`: one code unit per byte, or a same-realm
/// `DOMException`. `length = 1`.
fn read_as_binary_string_sync(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let (bytes, _, _) = read_bytes_sync(this, args, None, context)?;
    Ok(JsValue::from(JsString::from(
        package::package_binary_string(&bytes),
    )))
}

/// `readAsText(blob, encoding?)`: decoded text, or a same-realm
/// `DOMException`. `length = 1` (the encoding is optional).
fn read_as_text_sync(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let label = if args.len() >= 2 && !args[1].is_undefined() {
        Some(dom_string(&args[1], context)?)
    } else {
        None
    };
    let (bytes, _, encoding) = read_bytes_sync(this, args, label.as_deref(), context)?;
    Ok(JsValue::from(JsString::from(package::decode_text(
        &encoding, &bytes,
    ))))
}

/// `readAsDataURL(blob)`: exact data URL, or a same-realm `DOMException`.
/// `length = 1`.
fn read_as_data_url_sync(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let _ = require_sync(this)?;
    let data = blob_arg(args)?;
    let specs = crate::extension::snapshot(context)?;
    let dom = specs
        .dom_specs()
        .ok_or_else(|| type_error("the DOM shim is not registered"))?;
    let limits = specs.limits().clone();
    if data.size() > limits.max_sync_read_bytes {
        return Err(throw_named(
            &dom,
            "QuotaExceededError",
            "the read exceeds the synchronous read limit",
        ));
    }
    // Checked data-URL length before any source read or output allocation.
    let media_type = data.media_type().to_owned();
    if package::data_url_len(&media_type, data.size())
        .is_none_or(|total| total > limits.max_data_url_output)
    {
        return Err(throw_named(
            &dom,
            "QuotaExceededError",
            "data URL output exceeds the configured limit",
        ));
    }
    let bytes = data
        .materialize(&limits, &CancellationToken::new())
        .map_err(|error| throw_mapped(&dom, &error))?;
    let url = package::package_data_url(&media_type, &bytes, limits.max_data_url_output)
        .map_err(|error| throw_mapped(&dom, &error))?;
    Ok(JsValue::from(JsString::from(url)))
}

/// Registers the `FileReaderSync` prototype members: exactly the four
/// read methods (no `readyState`, `result`, `error`, `abort`, handlers,
/// or Promise API).
fn init_sync_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method) in [
        (
            js_string!("readAsArrayBuffer"),
            NativeFunction::from_fn_ptr(read_as_array_buffer_sync),
        ),
        (
            js_string!("readAsBinaryString"),
            NativeFunction::from_fn_ptr(read_as_binary_string_sync),
        ),
        (
            js_string!("readAsText"),
            NativeFunction::from_fn_ptr(read_as_text_sync),
        ),
        (
            js_string!("readAsDataURL"),
            NativeFunction::from_fn_ptr(read_as_data_url_sync),
        ),
    ] {
        let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), method)
            .name(name.clone())
            .length(1)
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
            .value(js_string!("FileReaderSync"))
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
    //! Child-module proof of the sync failure and preflight paths with
    //! controlled `ByteSource`s through real `Context` calls.
    //!
    //! Short/long/failing sources are unreachable via public constructors
    //! (ranges are validated up front), so these source types exist only in
    //! this test module: no production hook, no public arbitrary-source
    //! API. Each test wraps the payload in a real branded JS `Blob`, calls
    //! the real sync method, and asserts the JS-observable return or
    //! same-realm `DOMException` — synchronously, with no jobs involved.

    use super::*;
    use boa_engine::{Source, js_string};
    use boa_fapi_core::cancellation::CancellationToken;
    use boa_fapi_core::limits::FileApiLimits;
    use boa_fapi_core::snapshot::SnapshotState;
    use boa_fapi_core::source::ByteSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A source that counts `read_range` calls and serves exact bytes.
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

    /// A source declaring 5 bytes but returning 4 (short response).
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

    /// A source declaring 5 bytes but returning 8 (long response).
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

    /// Registers the extension in a worker environment.
    fn setup_worker() -> Context {
        let mut context = Context::default();
        crate::extension::FileApiExtension::builder()
            .environment(crate::FileApiEnvironment::DedicatedWorker)
            .build()
            .register(&mut context)
            .expect("registration failed");
        context
    }

    /// Wraps `data` in a real branded JS `Blob` reachable as `srcBlob`,
    /// with a real `FileReaderSync` reachable as `sync`.
    fn publish_pair(context: &mut Context, data: Arc<BlobData>) {
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
        let sync = JsObject::from_proto_and_data(specs.sync_reader_proto(), FileReaderSyncNative);
        context
            .register_global_property(
                js_string!("sync"),
                sync,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish sync reader");
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

    /// Evaluates `source` expecting a thrown same-realm `DOMException`
    /// with the given name.
    fn assert_throws_dom(context: &mut Context, source: &str, name: &str) {
        let error = context
            .eval(Source::from_bytes(source))
            .expect_err("expected a DOMException throw");
        let message = format!("{error}");
        assert!(
            message.contains("DOMException"),
            "expected DOMException for {source}, got: {message}"
        );
        let observed: String = context
            .eval(Source::from_bytes(&format!(
                "(function () {{ try {{ {source}; }} catch (e) {{ \
                   return (e instanceof DOMException) + ':' + e.name + ':' + (e instanceof Error); }} \
                   return 'no-throw'; }})()"
            )))
            .expect("verdict eval")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped();
        assert_eq!(observed, format!("true:{name}:true"));
    }

    #[test]
    fn short_source_throws_not_readable_error() {
        let mut context = setup_worker();
        publish_pair(&mut context, blob_over(Arc::new(ShortSource), 5));
        assert_throws_dom(
            &mut context,
            "sync.readAsArrayBuffer(srcBlob)",
            "NotReadableError",
        );
    }

    #[test]
    fn long_source_throws_not_readable_error() {
        let mut context = setup_worker();
        publish_pair(&mut context, blob_over(Arc::new(LongSource), 5));
        assert_throws_dom(&mut context, "sync.readAsText(srcBlob)", "NotReadableError");
    }

    #[test]
    fn failing_source_throws_not_readable_error() {
        let mut context = setup_worker();
        publish_pair(&mut context, blob_over(Arc::new(FailSource { len: 3 }), 3));
        assert_throws_dom(
            &mut context,
            "sync.readAsBinaryString(srcBlob)",
            "NotReadableError",
        );
        assert_throws_dom(
            &mut context,
            "sync.readAsDataURL(srcBlob)",
            "NotReadableError",
        );
    }

    #[test]
    fn rejected_sync_preflight_performs_zero_source_reads() {
        // A blob larger than `max_sync_read_bytes` is rejected before any
        // `read_range` call and before any output allocation.
        let reads = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource {
            data: bytes::Bytes::from(vec![9u8; 128]),
            reads: Arc::clone(&reads),
        });
        let mut context = Context::default();
        let limits = FileApiLimits {
            max_sync_read_bytes: 64,
            ..FileApiLimits::default()
        };
        crate::extension::FileApiExtension::builder()
            .environment(crate::FileApiEnvironment::SharedWorker)
            .limits(limits)
            .build()
            .register(&mut context)
            .expect("registration failed");
        publish_pair(&mut context, blob_over(source, 128));
        assert_throws_dom(
            &mut context,
            "sync.readAsArrayBuffer(srcBlob)",
            "QuotaExceededError",
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "rejected preflight must not read from the source"
        );
    }

    #[test]
    fn sync_success_paths_match_async_packaging() {
        let mut context = setup_worker();
        publish_pair(
            &mut context,
            blob_over(
                Arc::new(CountingSource {
                    data: bytes::Bytes::copy_from_slice(b"hi"),
                    reads: Arc::new(AtomicUsize::new(0)),
                }),
                2,
            ),
        );
        let value: String = context
            .eval(Source::from_bytes(
                "var ab = sync.readAsArrayBuffer(srcBlob); \
                 var bs = sync.readAsBinaryString(srcBlob); \
                 var tx = sync.readAsText(srcBlob); \
                 var du = sync.readAsDataURL(srcBlob); \
                 (ab instanceof ArrayBuffer) + ':' + ab.byteLength + ':' \
                 + (bs.length) + ':' + bs.charCodeAt(0) + ':' + bs.charCodeAt(1) + ':' \
                 + tx + ':' + du",
            ))
            .expect("sync reads")
            .as_string()
            .expect("verdict string")
            .to_std_string_escaped();
        assert_eq!(
            value, "true:2:2:104:105:hi:data:;base64,aGk=",
            "sync packaging must match the async representations"
        );
    }
}
