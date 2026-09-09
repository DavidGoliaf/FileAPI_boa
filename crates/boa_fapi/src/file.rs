//! Native File data, constructor and getters.

use std::sync::Arc;

use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, JsString, JsSymbol, JsValue, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::limits::FileApiLimits;
use boa_gc::{Finalize, Trace};
use bytes::Bytes;

use crate::blob::constructor_prototype;
use crate::brand;
use crate::clock::Clock;
use crate::error::type_error;
use crate::extension::snapshot;
use crate::webidl::{
    FileOptions, PartsCollector, arg, convert_sequence, process_converted, usv_string,
};

/// The internal File brand: Blob payload plus immutable `name`/`lastModified`.
///
/// Carries no Boa types; the garbage collector never traces its contents.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct FileNative {
    #[unsafe_ignore_trace]
    data: Arc<BlobData>,
    #[unsafe_ignore_trace]
    name: String,
    last_modified: i64,
}

impl FileNative {
    /// Builds the native state of a File.
    pub(crate) fn new(data: Arc<BlobData>, name: String, last_modified: i64) -> Self {
        Self {
            data,
            name,
            last_modified,
        }
    }

    /// Returns the shared immutable payload.
    pub(crate) fn blob_data(&self) -> &Arc<BlobData> {
        &self.data
    }

    /// Returns the immutable file name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns the immutable `lastModified` timestamp.
    pub(crate) fn last_modified(&self) -> i64 {
        self.last_modified
    }
}

/// Normalizes a file name: USVString semantics, then `/` becomes `:`.
///
/// No basename computation is performed; host paths never become names.
pub(crate) fn normalize_file_name(name: &str) -> String {
    name.replace('/', ":")
}

/// Builds the native state for a File from host-provided bytes.
///
/// `name` is already a Rust string; the same slash replacement as the JS
/// constructor applies. When `last_modified` is `None`, the injected clock
/// provides the timestamp.
pub(crate) fn native_from_bytes(
    bytes: Bytes,
    name: &str,
    media_type: &str,
    last_modified: Option<i64>,
    clock: &dyn Clock,
    limits: &FileApiLimits,
) -> Result<FileNative, boa_fapi_core::file_api_error::FileApiError> {
    let data = crate::blob::data_from_bytes(bytes, media_type, limits)?;
    let timestamp = last_modified.unwrap_or_else(|| clock.now_unix_millis());
    Ok(FileNative::new(data, normalize_file_name(name), timestamp))
}

/// Builds the native state for a File over an existing [`BlobData`].
///
/// Used by host imports and advanced host integrations: the payload already
/// carries its immutable snapshot, and only the display name/timestamp are
/// attached here. No basename is computed; `display_name` is the only name
/// JS observes.
pub(crate) fn native_from_data(
    data: std::sync::Arc<boa_fapi_core::blob::BlobData>,
    display_name: &str,
    last_modified: Option<i64>,
    clock: &dyn Clock,
) -> FileNative {
    let timestamp = last_modified.unwrap_or_else(|| clock.now_unix_millis());
    FileNative::new(data, normalize_file_name(display_name), timestamp)
}

/// The `File` constructor: `new File(fileBits, fileName, options?)`.
pub(crate) fn constructor(
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("File constructor requires 'new'"));
    };
    // Web IDL: `fileBits` and `fileName` are required arguments.
    if args.len() < 2 {
        return Err(type_error(
            "File constructor requires at least two arguments",
        ));
    }
    // Web IDL argument order: `fileBits` sequence conversion first
    // (typed conversion with conversion-time snapshots, no `endings`
    // yet), then `fileName` USVString, then the options dictionary, then
    // the options-dependent processing step.
    let limits = specs.limits().clone();
    let converted = convert_sequence(&arg(args, 0), true, &limits, context)?;
    // Required `fileName`: USVString, then every `/` becomes `:`.
    let file_name = usv_string(&arg(args, 1), context)?;
    let file_name = normalize_file_name(&file_name);

    let options = FileOptions::parse(&arg(args, 2), context)?;
    // `NewTarget.prototype` is observable and therefore follows every
    // argument conversion, including the options dictionary.
    let prototype = constructor_prototype(&target, specs.file_proto(), context)?;
    let mut collector = PartsCollector::new(limits);
    process_converted(converted, options.blob.endings, &mut collector)?;
    let data = Arc::new(collector.into_blob_data(&options.blob.media_type)?);

    let timestamp = options
        .last_modified
        .unwrap_or_else(|| specs.now_unix_millis());
    let native = FileNative::new(data, file_name, timestamp);

    Ok(JsValue::from(JsObject::from_proto_and_data(
        prototype, native,
    )))
}

/// The `name` getter: `readonly attribute DOMString name`.
pub(crate) fn name_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let (_, name, _) = brand::require_file(this)?;
    Ok(JsValue::from(JsString::from(name)))
}

/// The `lastModified` getter: `readonly attribute long long lastModified`.
pub(crate) fn last_modified_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let (_, _, last_modified) = brand::require_file(this)?;
    Ok(JsValue::from(last_modified))
}

/// Registers the File members on the built prototype (Web IDL attributes).
pub(crate) fn init_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (key, getter) in [
        (js_string!("name"), NativeFunction::from_fn_ptr(name_getter)),
        (
            js_string!("lastModified"),
            NativeFunction::from_fn_ptr(last_modified_getter),
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

    // [Symbol.toStringTag] = "File".
    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("File"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;

    Ok(())
}
