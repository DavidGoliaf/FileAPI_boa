//! Native FileList data, indexed properties and `item`.

use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, JsSymbol, JsValue, js_string};
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::error::{range_error, type_error};
use crate::webidl::{arg, unsigned_long};

/// The internal FileList brand: the number of contained files.
///
/// The File objects themselves live in the object's own indexed property
/// slots (traced by the object), which preserves object identity without
/// storing Boa types inside the native data.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct FileListNative {
    len: usize,
}

impl FileListNative {
    /// Returns the number of files held by the list.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// Maximum number of files in a list: canonical array indices end at 2^32-2.
const MAX_FILES: usize = (u32::MAX - 1) as usize;

/// Creates a FileList object from already brand-validated File objects.
///
/// Every element becomes an own indexed property with attributes
/// `{ writable: false, enumerable: true, configurable: false }`, preserving
/// the host input order.
pub(crate) fn create(
    files: Vec<JsObject>,
    prototype: &JsObject,
    context: &mut Context,
) -> JsResult<JsObject> {
    let count = files.len();
    if count > MAX_FILES {
        return Err(range_error("too many files in the FileList"));
    }
    let list = JsObject::from_proto_and_data(prototype.clone(), FileListNative { len: count });
    for (index, file) in files.into_iter().enumerate() {
        // `count <= u32::MAX - 1`, so every index is a canonical array index.
        let key = PropertyKey::from(index as u32);
        list.define_property_or_throw(
            key,
            PropertyDescriptor::builder()
                .value(file)
                .writable(false)
                .enumerable(true)
                .configurable(false),
            context,
        )?;
    }
    Ok(list)
}

/// The `length` getter: `readonly attribute unsigned long length`.
pub(crate) fn length_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let len = brand::require_file_list(this)?;
    // The count is far below 2^53, so the conversion is exact.
    Ok(JsValue::from(len as f64))
}

/// `FileList.prototype.item(index)`.
pub(crate) fn item(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let len = brand::require_file_list(this)?;
    let index = unsigned_long(&arg(args, 0), context)?;
    // The count is at most `u32::MAX - 1`, so a plain widening comparison is
    // exact on every supported target.
    if u64::from(index) >= len as u64 {
        return Ok(JsValue::null());
    }
    // The indexed property exists and is non-configurable, so this always
    // returns the exact same File object for an in-range index.
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a FileList"));
    };
    object.get(PropertyKey::from(index), context)
}

/// Registers the FileList members on the prototype.
///
/// Web IDL "define the iteration methods": an interface with an indexed
/// property getter receives `prototype[Symbol.iterator]` aliased to
/// `%Array.prototype.values%` (same function object, `{ writable: true,
/// enumerable: false, configurable: true }`). No `entries`/`keys`/
/// `values`/`forEach` are added: FileList declares no value-iterator.
pub(crate) fn init_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    // length: getter-only accessor attribute.
    let function = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(length_getter),
    )
    .name(js_string!("length"))
    .length(0)
    .constructor(false)
    .build();
    prototype.define_property_or_throw(
        js_string!("length"),
        PropertyDescriptor::builder()
            .get(function)
            .enumerable(true)
            .configurable(true),
        context,
    )?;

    // item: writable, enumerable, configurable method (1 required argument).
    let item_function = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(item),
    )
    .name(js_string!("item"))
    .length(1)
    .constructor(false)
    .build();
    prototype.define_property_or_throw(
        js_string!("item"),
        PropertyDescriptor::builder()
            .value(item_function)
            .writable(true)
            .enumerable(true)
            .configurable(true),
        context,
    )?;

    // Indexed iteration: the same function object as
    // `%Array.prototype.values%`, so borrowed-call semantics stay exactly
    // the generic Array iterator semantics (no FileList-specific brand
    // check beyond what the shared iterator performs).
    let array_values = context
        .intrinsics()
        .constructors()
        .array()
        .prototype()
        .get(js_string!("values"), context)?;
    prototype.define_property_or_throw(
        PropertyKey::from(JsSymbol::iterator()),
        PropertyDescriptor::builder()
            .value(array_values)
            .writable(true)
            .enumerable(false)
            .configurable(true),
        context,
    )?;

    // [Symbol.toStringTag] = "FileList".
    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("FileList"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;

    Ok(())
}
