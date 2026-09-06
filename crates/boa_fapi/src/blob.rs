//! Native Blob data, constructor, getters and `slice`.

use std::sync::Arc;

use boa_engine::object::JsObject;
use boa_engine::object::PROTOTYPE;
use boa_engine::property::PropertyKey;
use boa_engine::{Context, JsData, JsResult, JsString, JsValue, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::limits::FileApiLimits;
use boa_fapi_core::source::ByteSource;
use boa_fapi_core::source::memory::MemorySource;
use boa_gc::{Finalize, Trace};
use bytes::Bytes;

use crate::brand;
use crate::error::{js_from_core, type_error};
use crate::extension::snapshot;
use crate::webidl::{
    BlobOptions, arg, blob_parts, collect_parts, dom_string, optional_clamped_long_long,
};

/// The internal Blob brand: immutable segmented bytes plus a media type.
///
/// The native data owns the shared immutable [`BlobData`]; it holds no Boa
/// types, so the garbage collector never needs to trace its contents.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct BlobNative {
    #[unsafe_ignore_trace]
    data: Arc<BlobData>,
}

impl BlobNative {
    /// Wraps `data` as native Blob state.
    pub(crate) fn new(data: Arc<BlobData>) -> Self {
        Self { data }
    }

    /// Returns the shared immutable payload.
    pub(crate) fn blob_data(&self) -> &Arc<BlobData> {
        &self.data
    }
}

/// Builds the core payload for a host-created Blob from `bytes`.
pub(crate) fn data_from_bytes(
    bytes: Bytes,
    media_type: &str,
    limits: &FileApiLimits,
) -> Result<Arc<BlobData>, boa_fapi_core::file_api_error::FileApiError> {
    let source: Arc<dyn ByteSource> = Arc::new(MemorySource::new(bytes));
    let len = source.len();
    let segment = boa_fapi_core::blob::BlobSegment {
        source,
        offset: 0,
        len,
    };
    BlobData::from_segments(vec![segment], media_type, limits).map(Arc::new)
}

/// Creates a Blob instance object carrying `data`.
pub(crate) fn create_instance(data: BlobNative, prototype: JsObject) -> JsObject {
    JsObject::from_proto_and_data(prototype, data)
}

/// The `Blob` constructor: `new Blob(blobParts?, options?)`.
pub(crate) fn constructor(
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("Blob constructor requires 'new'"));
    };
    let prototype = constructor_prototype(&target, specs.blob_proto(), context)?;

    let parts = blob_parts(&arg(args, 0), context)?;
    let options = BlobOptions::parse(&arg(args, 1), context)?;
    let collector = collect_parts(parts.as_ref(), options.endings, specs.limits(), context)?;
    let data = collector.into_blob_data(&options.media_type)?;

    Ok(JsValue::from(create_instance(
        BlobNative::new(Arc::new(data)),
        prototype,
    )))
}

/// Resolves the [[Prototype]] for a newly constructed instance.
///
/// Follows OrdinaryCreateFromConstructor: `newTarget.prototype` when it is an
/// object, the interface prototype otherwise.
pub(crate) fn constructor_prototype(
    new_target: &JsObject,
    fallback: JsObject,
    context: &mut Context,
) -> JsResult<JsObject> {
    let proto = new_target.get(PROTOTYPE, context)?;
    Ok(proto.as_object().unwrap_or(fallback))
}

/// The `size` getter: `readonly attribute unsigned long long size`.
pub(crate) fn size_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    // M1 limits cap `size` far below 2^53, so the conversion is exact.
    let size = data.size() as f64;
    Ok(JsValue::from(size))
}

/// The `type` getter: `readonly attribute DOMString type`.
pub(crate) fn type_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    Ok(JsValue::from(JsString::from(data.media_type().to_owned())))
}

/// `Blob.prototype.slice(start?, end?, contentType?)`.
pub(crate) fn slice(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let data = brand::require_blob(this)?;
    let specs = snapshot(context)?;

    let start = optional_clamped_long_long(&arg(args, 0), context)?;
    let end = optional_clamped_long_long(&arg(args, 1), context)?;
    let content_type_arg = arg(args, 2);
    let content_type = if content_type_arg.is_undefined() {
        None
    } else {
        Some(dom_string(&content_type_arg, context)?)
    };

    let sliced = data
        .slice(start, end, content_type.as_deref(), specs.limits())
        .map_err(js_from_core)?;

    Ok(JsValue::from(create_instance(
        BlobNative::new(Arc::new(sliced)),
        specs.blob_proto(),
    )))
}

/// Registers the Blob members on the built prototype (Web IDL attributes).
pub(crate) fn init_prototype(
    prototype: &JsObject,
    context: &mut Context,
) -> boa_engine::JsResult<()> {
    use boa_engine::native_function::NativeFunction;
    use boa_engine::property::PropertyDescriptor;

    // size / type: getter-only accessor attributes.
    for (key, getter) in [
        (js_string!("size"), NativeFunction::from_fn_ptr(size_getter)),
        (js_string!("type"), NativeFunction::from_fn_ptr(type_getter)),
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

    // slice: writable, enumerable, configurable method.
    let slice_function = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(slice),
    )
    .name(js_string!("slice"))
    .length(0)
    .constructor(false)
    .build();
    prototype.define_property_or_throw(
        js_string!("slice"),
        PropertyDescriptor::builder()
            .value(slice_function)
            .writable(true)
            .enumerable(true)
            .configurable(true),
        context,
    )?;

    // [Symbol.toStringTag] = "Blob" (writable: false, enumerable: false,
    // configurable: true, per Web IDL interface prototype objects).
    let tag_key = PropertyKey::from(boa_engine::JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("Blob"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;

    Ok(())
}
