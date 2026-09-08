//! Minimal self-contained DOM shim for the M4-A milestone.
//!
//! Registers exactly four globals: `EventTarget`, `Event`, `ProgressEvent`
//! and `DOMException` (the private `filereader` module adds `FileReader`).
//! No host DOM adapter exists: when the `dom-shim` Cargo feature is off or
//! [`crate::FileApiExtensionBuilder::dom_shim`] is `false`, registration
//! fails with [`crate::RegisterError::DomShimDisabled`] before any
//! `globalThis` mutation.
//!
//! Brand model: every object carries native data (`EventTargetNative`,
//! `EventNative`, `ProgressEventNative`, `DomExceptionNative`) holding only
//! GC-visible values (`JsFunction` listeners, `JsObject` targets) or plain
//! Rust data. JS can never forge the brand: only the constructors create
//! branded objects, and every method validates the brand synchronously with
//! a `TypeError` for borrowed or forged receivers.
//!
//! Listener model: `addEventListener` deduplicates on the tuple
//! `(type, callback, capture)`; `capture` is accepted but dispatch is
//! at-target only. A listener exception never stops dispatch of the
//! remaining listeners: direct `dispatchEvent` reports the first exception
//! as a synchronous throw after every listener ran, while internal
//! `FileReader` dispatches stash the first exception so the terminal
//! FileReading job can surface it as a JS job error.

use boa_engine::Context;
use boa_engine::JsValue;
use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::object::builtins::JsFunction;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{JsData, JsResult, JsString, JsSymbol, js_string};
use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;
use boa_gc::{Finalize, Trace};

use crate::error::type_error;
use crate::webidl::{arg, dom_string};

/// Constructor/prototype pairs installed as globals by the DOM shim.
///
/// `FileReader` lives in the private `filereader` module; its prototype
/// inherits from the `EventTarget` prototype stored here.
#[derive(Clone)]
pub(crate) struct DomSpecs {
    /// `EventTarget` shim constructor.
    pub(crate) event_target: StandardConstructor,
    /// `Event` shim constructor.
    pub(crate) event: StandardConstructor,
    /// `ProgressEvent` shim constructor.
    pub(crate) progress_event: StandardConstructor,
    /// `DOMException` shim constructor.
    pub(crate) dom_exception: StandardConstructor,
}

/// Builds the four DOM class pairs without touching any global.
///
/// Installation stays atomic in `extension.rs`. `error_prototype` is the
/// realm's `Error.prototype`, parent of `DOMException.prototype`.
pub(crate) fn build_dom_specs(
    context: &mut Context,
    error_prototype: JsObject,
) -> JsResult<DomSpecs> {
    use boa_engine::native_function::NativeFunction;

    let mut target_builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(event_target_constructor),
    );
    target_builder.name("EventTarget");
    target_builder.length(0);
    let event_target = target_builder.build();
    init_event_target_prototype(&event_target.prototype(), context)?;

    let mut event_builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(event_constructor));
    event_builder.name("Event");
    event_builder.length(1);
    let event = event_builder.build();
    init_event_prototype(&event.prototype(), context)?;

    let mut progress_builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(progress_event_constructor),
    );
    progress_builder.name("ProgressEvent");
    progress_builder.length(1);
    progress_builder.inherit(event.prototype());
    let progress_event = progress_builder.build();
    init_progress_event_prototype(&progress_event.prototype(), context)?;

    let mut exception_builder = ConstructorBuilder::new(
        context,
        NativeFunction::from_fn_ptr(dom_exception_constructor),
    );
    exception_builder.name("DOMException");
    // Web IDL: `constructor(optional DOMString message = "",
    // optional DOMString name = "Error")` — no required arguments.
    exception_builder.length(0);
    exception_builder.inherit(error_prototype);
    let dom_exception = exception_builder.build();
    init_dom_exception_prototype(&dom_exception.prototype(), context)?;

    Ok(DomSpecs {
        event_target,
        event,
        progress_event,
        dom_exception,
    })
}

/// One registered event listener: the dedupe tuple is
/// `(event_type, callback, capture)`.
#[derive(Debug, Clone, Trace, Finalize)]
pub(crate) struct ListEntry {
    /// The event type this listener observes.
    pub(crate) event_type: String,
    /// The callable invoked with the event.
    pub(crate) callback: JsFunction,
    /// Accepted for Web IDL compatibility; dispatch is at-target only.
    pub(crate) capture: bool,
}

impl ListEntry {
    /// Returns `true` when both entries are the same dedupe tuple.
    pub(crate) fn same_tuple(&self, other: &ListEntry) -> bool {
        self.event_type == other.event_type
            && self.capture == other.capture
            && JsObject::equals(
                &JsObject::from(self.callback.clone()),
                &JsObject::from(other.callback.clone()),
            )
    }
}

/// Native brand data of an `EventTarget` object: its listener list.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct EventTargetNative {
    /// Registered listeners in registration order.
    pub(crate) listeners: Vec<ListEntry>,
}

/// Native brand data of an `Event` object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct EventNative {
    /// The event type.
    event_type: String,
    /// The dispatch target (`None` before dispatch).
    target: Option<JsObject>,
    /// The current target during dispatch (`None` outside dispatch).
    current_target: Option<JsObject>,
    /// Whether the event bubbles.
    bubbles: bool,
    /// Whether `preventDefault` takes effect.
    cancelable: bool,
    /// Set by `preventDefault` only when `cancelable`.
    default_prevented: bool,
    /// Set by `stopImmediatePropagation`; stops further listeners.
    stopped: bool,
    /// Creation time from the injected clock (milliseconds since the epoch).
    time_stamp: f64,
}

/// Native brand data of a `ProgressEvent` object: an `Event` plus progress.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct ProgressEventNative {
    /// The event type.
    event_type: String,
    /// The dispatch target (`None` before dispatch).
    target: Option<JsObject>,
    /// The current target during dispatch (`None` outside dispatch).
    current_target: Option<JsObject>,
    /// Whether the event bubbles.
    bubbles: bool,
    /// Whether `preventDefault` takes effect.
    cancelable: bool,
    /// Set by `preventDefault` only when `cancelable`.
    default_prevented: bool,
    /// Set by `stopImmediatePropagation`; stops further listeners.
    stopped: bool,
    /// Creation time from the injected clock (milliseconds since the epoch).
    time_stamp: f64,
    /// Whether `loaded`/`total` carry meaningful values.
    length_computable: bool,
    /// Bytes delivered so far.
    loaded: f64,
    /// Total bytes of the operation.
    total: f64,
}

/// Native brand data of a `DOMException` object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct DomExceptionNative {
    /// The exception name, e.g. `"NotReadableError"`.
    name: String,
    /// Generic message without path, byte content, or source details.
    message: String,
}

/// The fixed M4-A mapping from core failures to `DOMException` names.
///
/// `ResourceLimit` always maps to `QuotaExceededError` (the fixed M4-A
/// choice, whatever the limit kind); `Cancelled` maps to `AbortError`.
/// The message carries no path, byte content, or source details.
pub(crate) fn map_core_error(error: &FileApiError) -> (&'static str, &'static str) {
    match error {
        FileApiError::NotFound => ("NotFoundError", "the resource was not found"),
        FileApiError::UnsafeFile | FileApiError::TooManyReads | FileApiError::PermissionDenied => {
            ("SecurityError", "access to the resource is not allowed")
        }
        FileApiError::SnapshotChanged
        | FileApiError::FileLocked
        | FileApiError::InvalidRange
        | FileApiError::Internal => ("NotReadableError", "blob read failed"),
        FileApiError::ResourceLimit(ResourceLimitKind::DataUrlOutput) => (
            "QuotaExceededError",
            "data URL output exceeds the configured limit",
        ),
        FileApiError::ResourceLimit(_) => (
            "QuotaExceededError",
            "the operation exceeds the configured quota",
        ),
        FileApiError::Cancelled => ("AbortError", "the read was aborted"),
        // `FileApiError` is non-exhaustive: future core variants (e.g. an
        // I/O kind) must surface as a generic read failure, never leak
        // paths, bytes, or source details.
        _ => ("NotReadableError", "blob read failed"),
    }
}

/// Constructs a same-realm `DOMException` instance for `name`/`message`.
pub(crate) fn construct_exception(specs: &DomSpecs, name: &str, message: &str) -> JsObject {
    JsObject::from_proto_and_data(
        specs.dom_exception.prototype(),
        DomExceptionNative {
            name: name.to_owned(),
            message: message.to_owned(),
        },
    )
}

/// Constructs a same-realm `DOMException` for a core failure.
#[allow(dead_code)]
pub(crate) fn exception_from_core(specs: &DomSpecs, error: &FileApiError) -> JsObject {
    let (name, message) = map_core_error(error);
    construct_exception(specs, name, message)
}

/// Validates that `this` carries the `EventTarget` brand.
///
/// `FileReader` objects inherit the EventTarget interface: their listeners
/// live in the FileReader native data instead.
pub(crate) fn require_event_target(this: &JsValue) -> JsResult<JsObject> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected an EventTarget"));
    };
    if object.is::<EventTargetNative>() {
        return Ok(object.clone());
    }
    if object.is::<crate::filereader::FileReaderNative>() {
        return Ok(object.clone());
    }
    Err(type_error("illegal invocation: expected an EventTarget"))
}

/// Reads the listener list of a plain `EventTarget`-brand object.
///
/// Returns `None` for `FileReader` objects (their listeners live in the
/// FileReader native data and are handled by the FileReader dispatch).
fn target_listeners(object: &JsObject) -> Option<Vec<ListEntry>> {
    object
        .downcast_ref::<EventTargetNative>()
        .map(|native| native.listeners.clone())
}

/// Extracts a callable event listener from `value`.
///
/// `null`/`undefined` is ignored (returns `None` without throwing, per the
/// Web IDL `EventListener` conversion); any other non-callable value is a
/// synchronous `TypeError`.
pub(crate) fn to_listener(value: &JsValue) -> JsResult<Option<JsFunction>> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    let Some(object) = value.as_object() else {
        return Err(type_error("the event listener must be a function or null"));
    };
    JsFunction::from_object(object.clone()).map_or_else(
        || Err(type_error("the event listener must be a function or null")),
        |function| Ok(Some(function)),
    )
}

/// Converts the `capture` options value to a boolean.
///
/// Accepts a bare boolean or a dictionary with a `capture` member;
/// `undefined`/`null` means `false`.
pub(crate) fn to_capture(value: &JsValue, context: &mut Context) -> JsResult<bool> {
    if value.is_null_or_undefined() {
        return Ok(false);
    }
    if let Some(boolean) = value.as_boolean() {
        return Ok(boolean);
    }
    let Some(object) = value.as_object() else {
        return Ok(value.to_boolean());
    };
    Ok(object.get(js_string!("capture"), context)?.to_boolean())
}

/// `new EventTarget()`: allowed only with `new`.
fn event_target_constructor(
    new_target: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(target) = new_target.as_object() else {
        return Err(type_error("EventTarget constructor requires 'new'"));
    };
    let prototype = crate::blob::constructor_prototype(
        &target,
        crate::extension::snapshot(context)?.dom_event_target_proto(),
        context,
    )?;
    Ok(JsObject::from_proto_and_data(
        prototype,
        EventTargetNative {
            listeners: Vec::new(),
        },
    )
    .into())
}

/// Parses the `EventInit` members `bubbles`/`cancelable` from `options`.
fn parse_event_init(options: &JsValue, context: &mut Context) -> JsResult<(bool, bool)> {
    if options.is_null_or_undefined() {
        return Ok((false, false));
    }
    let Some(object) = options.as_object() else {
        return Err(type_error("the event init value is not an object"));
    };
    Ok((
        object.get(js_string!("bubbles"), context)?.to_boolean(),
        object.get(js_string!("cancelable"), context)?.to_boolean(),
    ))
}

/// `new Event(type, options?)`: allowed only with `new`.
fn event_constructor(
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = crate::extension::snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("Event constructor requires 'new'"));
    };
    if arg(args, 0).is_undefined() {
        return Err(type_error("Event constructor requires a type"));
    }
    let event_type = dom_string(&arg(args, 0), context)?;
    let (bubbles, cancelable) = parse_event_init(&arg(args, 1), context)?;
    let prototype = crate::blob::constructor_prototype(&target, specs.dom_event_proto(), context)?;
    let time_stamp = specs.clock_millis();
    Ok(JsObject::from_proto_and_data(
        prototype,
        EventNative {
            event_type,
            target: None,
            current_target: None,
            bubbles,
            cancelable,
            default_prevented: false,
            stopped: false,
            time_stamp,
        },
    )
    .into())
}

/// `new ProgressEvent(type, options?)`: allowed only with `new`.
fn progress_event_constructor(
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = crate::extension::snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("ProgressEvent constructor requires 'new'"));
    };
    if arg(args, 0).is_undefined() {
        return Err(type_error("ProgressEvent constructor requires a type"));
    }
    let event_type = dom_string(&arg(args, 0), context)?;
    let options = arg(args, 1);
    let (bubbles, cancelable) = parse_event_init(&options, context)?;
    let (length_computable, loaded, total) = if options.is_null_or_undefined() {
        (false, 0.0, 0.0)
    } else {
        let Some(object) = options.as_object() else {
            return Err(type_error("the event init value is not an object"));
        };
        (
            object
                .get(js_string!("lengthComputable"), context)?
                .to_boolean(),
            object
                .get(js_string!("loaded"), context)?
                .to_number(context)?,
            object
                .get(js_string!("total"), context)?
                .to_number(context)?,
        )
    };
    let prototype =
        crate::blob::constructor_prototype(&target, specs.dom_progress_event_proto(), context)?;
    let time_stamp = specs.clock_millis();
    Ok(JsObject::from_proto_and_data(
        prototype,
        ProgressEventNative {
            event_type,
            target: None,
            current_target: None,
            bubbles,
            cancelable,
            default_prevented: false,
            stopped: false,
            time_stamp,
            length_computable,
            loaded,
            total,
        },
    )
    .into())
}

/// `new DOMException(message?, name?)`: allowed only with `new`.
fn dom_exception_constructor(
    new_target: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let specs = crate::extension::snapshot(context)?;
    let Some(target) = new_target.as_object() else {
        return Err(type_error("DOMException constructor requires 'new'"));
    };
    let message = if arg(args, 0).is_undefined() {
        String::new()
    } else {
        dom_string(&arg(args, 0), context)?
    };
    let name = if arg(args, 1).is_undefined() {
        String::from("Error")
    } else {
        dom_string(&arg(args, 1), context)?
    };
    let prototype =
        crate::blob::constructor_prototype(&target, specs.dom_exception_proto(), context)?;
    Ok(JsObject::from_proto_and_data(prototype, DomExceptionNative { name, message }).into())
}

/// Creates a `ProgressEvent` for a FileReader dispatch.
///
/// `target` becomes both `target` and `currentTarget`; `bubbles` and
/// `cancelable` are `false`, so `preventDefault` never changes the result.
/// The time stamp is passed in by the caller (the FileReading job already
/// consumed its clock tick for throttle ordering); creating the event never
/// reads the clock itself, so progress throttling stays exact.
pub(crate) fn create_progress_event(
    specs: &crate::extension::RegisteredSpecs,
    event_type: &str,
    loaded: u64,
    total: u64,
    target: JsObject,
    time_stamp: f64,
) -> JsObject {
    JsObject::from_proto_and_data(
        specs.dom_progress_event_proto(),
        ProgressEventNative {
            event_type: event_type.to_owned(),
            target: Some(target.clone()),
            current_target: Some(target),
            bubbles: false,
            cancelable: false,
            default_prevented: false,
            stopped: false,
            time_stamp,
            length_computable: true,
            loaded: loaded as f64,
            total: total as f64,
        },
    )
}

/// Returns the event type of an `Event` or `ProgressEvent` object.
pub(crate) fn event_type_of(event: &JsObject) -> Option<String> {
    if let Some(native) = event.downcast_ref::<EventNative>() {
        return Some(native.event_type.clone());
    }
    if let Some(native) = event.downcast_ref::<ProgressEventNative>() {
        return Some(native.event_type.clone());
    }
    None
}

/// Returns `true` when the event's `defaultPrevented` flag is set.
pub(crate) fn event_default_prevented(event: &JsObject) -> bool {
    if let Some(native) = event.downcast_ref::<EventNative>() {
        return native.default_prevented;
    }
    if let Some(native) = event.downcast_ref::<ProgressEventNative>() {
        return native.default_prevented;
    }
    false
}

/// Returns `true` when the event's dispatch was stopped.
pub(crate) fn event_stopped(event: &JsObject) -> bool {
    if let Some(native) = event.downcast_ref::<EventNative>() {
        return native.stopped;
    }
    if let Some(native) = event.downcast_ref::<ProgressEventNative>() {
        return native.stopped;
    }
    false
}

/// Points an event at `target` for dispatch (both `target`/`currentTarget`).
pub(crate) fn retarget_event(event: &JsObject, target: &JsObject) {
    if let Some(mut native) = event.downcast_mut::<EventNative>() {
        native.target = Some(target.clone());
        native.current_target = Some(target.clone());
    } else if let Some(mut native) = event.downcast_mut::<ProgressEventNative>() {
        native.target = Some(target.clone());
        native.current_target = Some(target.clone());
    }
}

/// Invokes `on<type>` handlers and listeners for `event` on `target`.
///
/// The `on<type>` property handler runs first (when callable), then the
/// registered listeners of `event_type` in registration order. A listener
/// exception never stops dispatch of the remaining listeners; the first
/// exception is returned after every listener ran.
/// `stopImmediatePropagation` stops the remaining listeners without
/// producing an error.
pub(crate) fn invoke_event(
    target: &JsObject,
    listeners: &[ListEntry],
    event_type: &str,
    event: &JsValue,
    context: &mut Context,
) -> JsResult<Option<boa_engine::JsError>> {
    let mut first_error = None;
    let handler_key = JsString::from(format!("on{event_type}"));
    if let Ok(handler) = target.get(handler_key, context)
        && let Some(object) = handler.as_object()
        && let Some(function) = JsFunction::from_object(object.clone())
    {
        let this = JsValue::from(target.clone());
        if let Err(error) = function.call(&this, std::slice::from_ref(event), context) {
            first_error = Some(error);
        }
    }
    let event_object = event.as_object();
    for entry in listeners.iter().filter(|e| e.event_type == event_type) {
        if let Some(object) = event_object.clone()
            && event_stopped(&object)
        {
            break;
        }
        let this = JsValue::from(target.clone());
        if let Err(error) = entry
            .callback
            .call(&this, std::slice::from_ref(event), context)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    Ok(first_error)
}

/// `EventTarget.prototype.addEventListener(type, callback, options?)`.
fn add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event_target(this)?;
    let event_type = dom_string(&arg(args, 0), context)?;
    let Some(callback) = to_listener(&arg(args, 1))? else {
        return Ok(JsValue::undefined());
    };
    let capture = to_capture(&arg(args, 2), context)?;
    let entry = ListEntry {
        event_type,
        callback,
        capture,
    };
    if let Some(mut native) = object.downcast_mut::<EventTargetNative>() {
        if !native.listeners.iter().any(|e| e.same_tuple(&entry)) {
            native.listeners.push(entry);
        }
        return Ok(JsValue::undefined());
    }
    crate::filereader::add_reader_listener(&object, entry)?;
    Ok(JsValue::undefined())
}

/// `EventTarget.prototype.removeEventListener(type, callback, options?)`.
fn remove_event_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event_target(this)?;
    let event_type = dom_string(&arg(args, 0), context)?;
    let Some(callback) = to_listener(&arg(args, 1))? else {
        return Ok(JsValue::undefined());
    };
    let capture = to_capture(&arg(args, 2), context)?;
    let probe = ListEntry {
        event_type,
        callback,
        capture,
    };
    if let Some(mut native) = object.downcast_mut::<EventTargetNative>() {
        if let Some(index) = native.listeners.iter().position(|e| e.same_tuple(&probe)) {
            native.listeners.remove(index);
        }
        return Ok(JsValue::undefined());
    }
    crate::filereader::remove_reader_listener(&object, &probe);
    Ok(JsValue::undefined())
}

/// `EventTarget.prototype.dispatchEvent(event)`.
///
/// Synchronous; returns `!defaultPrevented`. A listener exception is
/// reported as a throw only after every remaining listener ran.
fn dispatch_event(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let object = require_event_target(this)?;
    let event_value = arg(args, 0);
    let Some(event_object) = event_value.as_object() else {
        return Err(type_error("dispatchEvent requires an Event"));
    };
    let Some(event_type) = event_type_of(&event_object) else {
        return Err(type_error("dispatchEvent requires an Event"));
    };
    retarget_event(&event_object, &object);
    let listeners = if let Some(list) = target_listeners(&object) {
        list
    } else {
        crate::filereader::reader_listeners(&object)
    };
    let first_error = invoke_event(&object, &listeners, &event_type, &event_value, context)?;
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(JsValue::from(!event_default_prevented(&event_object)))
}

/// Validates that `this` carries the `Event` brand (either flavor).
fn require_event(this: &JsValue) -> JsResult<JsObject> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected an Event"));
    };
    if object.is::<EventNative>() || object.is::<ProgressEventNative>() {
        return Ok(object.clone());
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// Validates that `this` carries the `ProgressEvent` brand.
fn require_progress_event(this: &JsValue) -> JsResult<JsObject> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a ProgressEvent"));
    };
    if object.is::<ProgressEventNative>() {
        return Ok(object.clone());
    }
    Err(type_error("illegal invocation: expected a ProgressEvent"))
}

/// Validates that `this` carries the `DOMException` brand.
pub(crate) fn require_dom_exception(this: &JsValue) -> JsResult<(String, String)> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a DOMException"));
    };
    if let Some(native) = object.downcast_ref::<DomExceptionNative>() {
        return Ok((native.name.clone(), native.message.clone()));
    }
    Err(type_error("illegal invocation: expected a DOMException"))
}

/// The `type` getter for both event flavors.
fn event_type_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(JsValue::from(JsString::from(native.event_type.clone())));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(JsValue::from(JsString::from(native.event_type.clone())));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `target` getter for both event flavors.
fn event_target_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(native.target.clone().map_or(JsValue::null(), JsValue::from));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(native.target.clone().map_or(JsValue::null(), JsValue::from));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `currentTarget` getter for both event flavors.
fn event_current_target_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(native
            .current_target
            .clone()
            .map_or(JsValue::null(), JsValue::from));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(native
            .current_target
            .clone()
            .map_or(JsValue::null(), JsValue::from));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `bubbles` getter for both event flavors.
fn event_bubbles_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(JsValue::from(native.bubbles));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(JsValue::from(native.bubbles));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `cancelable` getter for both event flavors.
fn event_cancelable_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(JsValue::from(native.cancelable));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(JsValue::from(native.cancelable));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `defaultPrevented` getter for both event flavors.
fn event_default_prevented_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(JsValue::from(native.default_prevented));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(JsValue::from(native.default_prevented));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `timeStamp` getter for both event flavors.
fn event_time_stamp_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(native) = object.downcast_ref::<EventNative>() {
        return Ok(JsValue::from(native.time_stamp));
    }
    if let Some(native) = object.downcast_ref::<ProgressEventNative>() {
        return Ok(JsValue::from(native.time_stamp));
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// `Event.prototype.preventDefault()`: sets the flag only when cancelable.
fn event_prevent_default(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(mut native) = object.downcast_mut::<EventNative>() {
        if native.cancelable {
            native.default_prevented = true;
        }
        return Ok(JsValue::undefined());
    }
    if let Some(mut native) = object.downcast_mut::<ProgressEventNative>() {
        if native.cancelable {
            native.default_prevented = true;
        }
        return Ok(JsValue::undefined());
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// `Event.prototype.stopImmediatePropagation()`: stops further listeners.
fn event_stop_immediate(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_event(this)?;
    if let Some(mut native) = object.downcast_mut::<EventNative>() {
        native.stopped = true;
        return Ok(JsValue::undefined());
    }
    if let Some(mut native) = object.downcast_mut::<ProgressEventNative>() {
        native.stopped = true;
        return Ok(JsValue::undefined());
    }
    Err(type_error("illegal invocation: expected an Event"))
}

/// The `loaded` getter.
fn progress_loaded_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_progress_event(this)?;
    let Some(native) = object.downcast_ref::<ProgressEventNative>() else {
        return Err(type_error("illegal invocation: expected a ProgressEvent"));
    };
    Ok(JsValue::from(native.loaded))
}

/// The `total` getter.
fn progress_total_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_progress_event(this)?;
    let Some(native) = object.downcast_ref::<ProgressEventNative>() else {
        return Err(type_error("illegal invocation: expected a ProgressEvent"));
    };
    Ok(JsValue::from(native.total))
}

/// The `lengthComputable` getter.
fn progress_length_computable_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let object = require_progress_event(this)?;
    let Some(native) = object.downcast_ref::<ProgressEventNative>() else {
        return Err(type_error("illegal invocation: expected a ProgressEvent"));
    };
    Ok(JsValue::from(native.length_computable))
}

/// The `DOMException` `name` getter.
fn dom_exception_name_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let (name, _) = require_dom_exception(this)?;
    Ok(JsValue::from(JsString::from(name)))
}

/// The `DOMException` `message` getter.
fn dom_exception_message_getter(
    this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    let (_, message) = require_dom_exception(this)?;
    Ok(JsValue::from(JsString::from(message)))
}

/// Registers a getter-only accessor attribute on `prototype`.
fn define_getter(
    prototype: &JsObject,
    name: &str,
    getter: boa_engine::native_function::NativeFunction,
    context: &mut Context,
) -> JsResult<()> {
    let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), getter)
        .name(js_string!(name))
        .length(0)
        .constructor(false)
        .build();
    prototype.define_property_or_throw(
        js_string!(name),
        PropertyDescriptor::builder()
            .get(function)
            .enumerable(true)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Registers a method on `prototype` (writable, non-enumerable,
/// configurable) with the exact Web IDL `length`.
fn define_method(
    prototype: &JsObject,
    name: &str,
    length: usize,
    method: boa_engine::native_function::NativeFunction,
    context: &mut Context,
) -> JsResult<()> {
    let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), method)
        .name(js_string!(name))
        .length(length)
        .constructor(false)
        .build();
    prototype.define_property_or_throw(
        js_string!(name),
        PropertyDescriptor::builder()
            .value(function)
            .writable(true)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Defines `[Symbol.toStringTag]` on `prototype`.
fn define_tag(prototype: &JsObject, tag: &str, context: &mut Context) -> JsResult<()> {
    let tag_key = PropertyKey::from(JsSymbol::to_string_tag());
    prototype.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!(tag))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Registers the `EventTarget` prototype members.
fn init_event_target_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method, length) in [
        (
            "addEventListener",
            NativeFunction::from_fn_ptr(add_event_listener),
            2,
        ),
        (
            "removeEventListener",
            NativeFunction::from_fn_ptr(remove_event_listener),
            2,
        ),
        (
            "dispatchEvent",
            NativeFunction::from_fn_ptr(dispatch_event),
            1,
        ),
    ] {
        define_method(prototype, name, length, method, context)?;
    }
    define_tag(prototype, "EventTarget", context)?;
    Ok(())
}

/// Registers the `Event` prototype members.
fn init_event_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    define_getter(
        prototype,
        "type",
        NativeFunction::from_fn_ptr(event_type_getter),
        context,
    )?;
    define_getter(
        prototype,
        "target",
        NativeFunction::from_fn_ptr(event_target_getter),
        context,
    )?;
    define_getter(
        prototype,
        "currentTarget",
        NativeFunction::from_fn_ptr(event_current_target_getter),
        context,
    )?;
    define_getter(
        prototype,
        "bubbles",
        NativeFunction::from_fn_ptr(event_bubbles_getter),
        context,
    )?;
    define_getter(
        prototype,
        "cancelable",
        NativeFunction::from_fn_ptr(event_cancelable_getter),
        context,
    )?;
    define_getter(
        prototype,
        "defaultPrevented",
        NativeFunction::from_fn_ptr(event_default_prevented_getter),
        context,
    )?;
    define_getter(
        prototype,
        "timeStamp",
        NativeFunction::from_fn_ptr(event_time_stamp_getter),
        context,
    )?;
    define_method(
        prototype,
        "preventDefault",
        0,
        NativeFunction::from_fn_ptr(event_prevent_default),
        context,
    )?;
    define_method(
        prototype,
        "stopImmediatePropagation",
        0,
        NativeFunction::from_fn_ptr(event_stop_immediate),
        context,
    )?;
    define_tag(prototype, "Event", context)?;
    Ok(())
}

/// Registers the `ProgressEvent` prototype members (inherits `Event`).
fn init_progress_event_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    define_getter(
        prototype,
        "lengthComputable",
        NativeFunction::from_fn_ptr(progress_length_computable_getter),
        context,
    )?;
    define_getter(
        prototype,
        "loaded",
        NativeFunction::from_fn_ptr(progress_loaded_getter),
        context,
    )?;
    define_getter(
        prototype,
        "total",
        NativeFunction::from_fn_ptr(progress_total_getter),
        context,
    )?;
    define_tag(prototype, "ProgressEvent", context)?;
    Ok(())
}

/// Registers the `DOMException` prototype members (inherits `Error`).
fn init_dom_exception_prototype(prototype: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    define_getter(
        prototype,
        "name",
        NativeFunction::from_fn_ptr(dom_exception_name_getter),
        context,
    )?;
    define_getter(
        prototype,
        "message",
        NativeFunction::from_fn_ptr(dom_exception_message_getter),
        context,
    )?;
    // `Error.prototype` already carries `toString`; the tag brands the
    // output of `Object.prototype.toString`.
    define_tag(prototype, "DOMException", context)?;
    Ok(())
}
