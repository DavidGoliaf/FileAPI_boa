//! `FileApiExtension`, its builder, atomic registration and the host handle.
//!
//! Registration is atomic: constructor and prototype objects are built
//! first, every preflight runs before `globalThis` changes, and a failed
//! install rolls back so that none of the three globals remains installed.
//! The chosen re-registration rule is **(b)**: every second call to
//! `register` on the same context returns [`RegisterError::AlreadyRegistered`].

use std::sync::Arc;

use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, js_string};
use boa_fapi_core::limits::FileApiLimits;
use boa_gc::{Finalize, Trace};
use bytes::Bytes;

use crate::blob::{self, BlobNative};
use crate::brand;
use crate::clock::{Clock, SystemClock};
use crate::error::{RegisterError, js_from_core};
use crate::file;
use crate::file_list;

/// Immutable extension configuration (M2 subset: clock and limits only).
#[derive(Clone)]
pub(crate) struct ExtensionConfig {
    /// Clock for `File.lastModified` defaults.
    pub(crate) clock: Arc<dyn Clock>,
    /// M1 resource limits for blob construction and slicing.
    pub(crate) limits: FileApiLimits,
    /// Whether the M3-B streams shim is registered.
    pub(crate) streams_shim: bool,
    /// Whether the M4-A DOM shim (`EventTarget`, `Event`, `ProgressEvent`,
    /// `DOMException`, `FileReader`) is registered.
    pub(crate) dom_shim: bool,
    /// The host-controlled environment descriptor. Only worker descriptors
    /// install `FileReaderSync`.
    pub(crate) environment: FileApiEnvironment,
}

/// The host-controlled environment descriptor selecting which globals the
/// registration installs.
///
/// The descriptor is an explicit host choice: it is never derived from the
/// thread ID, the `Context` type, or the presence of a host callback. The
/// default is [`FileApiEnvironment::Window`], so existing M4-A users get no
/// new global without changing their configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileApiEnvironment {
    /// A window-like environment: no `FileReaderSync` global is installed.
    #[default]
    Window,
    /// A dedicated worker: the normative `FileReaderSync` is installed.
    DedicatedWorker,
    /// A shared worker: the normative `FileReaderSync` is installed.
    SharedWorker,
    /// A service worker: no `FileReaderSync` global is installed (the
    /// service-worker capability is explicitly forbidden).
    ServiceWorker,
}

impl FileApiEnvironment {
    /// Returns `true` for the two worker descriptors that install the
    /// normative `FileReaderSync`.
    ///
    /// Only meaningful with the `dom-shim` feature: without it no sync
    /// surface exists to gate.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn file_reader_sync_enabled(&self) -> bool {
        matches!(self, Self::DedicatedWorker | Self::SharedWorker)
    }
}

/// The registered classes and configuration of a context.
#[derive(Clone)]
pub(crate) struct RegisteredSpecs {
    /// Blob constructor and prototype.
    pub(crate) blob: StandardConstructor,
    /// File constructor and prototype.
    pub(crate) file: StandardConstructor,
    /// FileList prototype (no public constructor exists).
    pub(crate) file_list_proto: JsObject,
    /// Streams shim constructors/prototypes (present when enabled).
    #[cfg(feature = "streams-shim")]
    pub(crate) streams: Option<crate::streams::StreamSpecs>,
    /// DOM shim constructors/prototypes (present when enabled).
    #[cfg(feature = "dom-shim")]
    pub(crate) dom: Option<crate::dom::DomSpecs>,
    /// FileReader constructor/prototype (present when the DOM shim is on).
    #[cfg(feature = "dom-shim")]
    pub(crate) filereader: Option<crate::filereader::FileReaderSpecs>,
    /// `FileReaderSync` constructor/prototype (present only for worker
    /// environment descriptors with the DOM shim on).
    #[cfg(feature = "dom-shim")]
    pub(crate) sync_reader: Option<crate::filereader_sync::FileReaderSyncSpecs>,
    /// Shared shutdown flag (M5 `fs` lifecycle). Cloned into the handle;
    /// every filesystem-backed read observes the same closed state.
    #[cfg(feature = "fs")]
    pub(crate) shutdown: crate::lifecycle::ShutdownFlag,
    /// Extension configuration.
    pub(crate) config: ExtensionConfig,
}

impl RegisteredSpecs {
    /// Returns the Blob interface prototype.
    pub(crate) fn blob_proto(&self) -> JsObject {
        self.blob.prototype()
    }

    /// Returns the File interface prototype.
    pub(crate) fn file_proto(&self) -> JsObject {
        self.file.prototype()
    }

    /// Returns the configured resource limits.
    pub(crate) fn limits(&self) -> &FileApiLimits {
        &self.config.limits
    }

    /// Reads the current time from the injected clock.
    pub(crate) fn now_unix_millis(&self) -> i64 {
        self.config.clock.now_unix_millis()
    }

    /// Reads the current time from the injected clock as an event time stamp.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn clock_millis(&self) -> f64 {
        self.config.clock.now_unix_millis() as f64
    }

    /// Returns the `EventTarget` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_event_target_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.event_target.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `Event` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_event_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.event.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `ProgressEvent` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_progress_event_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.progress_event.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `DOMException` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_exception_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.dom_exception.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `FileReader` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn filereader_proto(&self) -> JsObject {
        self.filereader
            .as_ref()
            .map(|spec| spec.reader.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `FileReaderSync` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn sync_reader_proto(&self) -> JsObject {
        self.sync_reader
            .as_ref()
            .map(|spec| spec.sync.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the cloned DOM specs when the shim is registered.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_specs(&self) -> Option<crate::dom::DomSpecs> {
        self.dom.clone()
    }

    /// Returns the environment descriptor this context was registered with.
    pub(crate) fn environment(&self) -> FileApiEnvironment {
        self.config.environment
    }
}

/// Clones the registration state out of the context so that callers never
/// hold borrows across further `Context` use.
pub(crate) fn snapshot(context: &Context) -> JsResult<RegisteredSpecs> {
    let Some(specs) = context.get_data::<RegisteredSpecs>() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("the File API extension is not registered in this context")
            .into());
    };
    Ok(specs.clone())
}

/// Builder for a [`FileApiExtension`].
#[derive(Clone, Default)]
pub struct FileApiExtensionBuilder {
    clock: Option<Arc<dyn Clock>>,
    limits: Option<FileApiLimits>,
    streams_shim: Option<bool>,
    dom_shim: Option<bool>,
    environment: Option<FileApiEnvironment>,
}

impl FileApiExtensionBuilder {
    /// Injects a [`Clock`] used for `File.lastModified` defaults.
    ///
    /// Tests inject a deterministic fake; the default is the system clock.
    pub fn clock(&mut self, clock: Arc<dyn Clock>) -> &mut Self {
        self.clock = Some(clock);
        self
    }

    /// Overrides the resource limits applied to blob construction.
    ///
    /// Defaults to [`FileApiLimits::default`].
    pub fn limits(&mut self, limits: FileApiLimits) -> &mut Self {
        self.limits = Some(limits);
        self
    }

    /// Enables or disables the M3-B streams shim registration.
    ///
    /// Defaults to `true`. When `false` (or the `streams-shim` Cargo
    /// feature is off), `register` fails before touching `globalThis`
    /// because no host stream adapter is implemented in this milestone.
    pub fn streams_shim(&mut self, enabled: bool) -> &mut Self {
        self.streams_shim = Some(enabled);
        self
    }

    /// Enables or disables the M4-A DOM shim registration.
    ///
    /// Defaults to `true`. When `false` (or the `dom-shim` Cargo feature
    /// is off), `register` fails before touching `globalThis` because no
    /// host DOM adapter is implemented in this milestone.
    pub fn dom_shim(&mut self, enabled: bool) -> &mut Self {
        self.dom_shim = Some(enabled);
        self
    }

    /// Selects the host-controlled environment descriptor.
    ///
    /// Defaults to [`FileApiEnvironment::Window`]. Only
    /// [`FileApiEnvironment::DedicatedWorker`] and
    /// [`FileApiEnvironment::SharedWorker`] install the normative
    /// `FileReaderSync`; `Window` and `ServiceWorker` install no such
    /// global (not even an `undefined` shim). The descriptor is an
    /// explicit host choice and is never inferred.
    pub fn environment(&mut self, environment: FileApiEnvironment) -> &mut Self {
        self.environment = Some(environment);
        self
    }

    /// Creates the extension.
    #[must_use]
    pub fn build(&self) -> FileApiExtension {
        FileApiExtension {
            config: ExtensionConfig {
                clock: self.clock.clone().unwrap_or_else(|| Arc::new(SystemClock)),
                limits: self.limits.clone().unwrap_or_default(),
                streams_shim: self.streams_shim.unwrap_or(true),
                dom_shim: self.dom_shim.unwrap_or(true),
                environment: self.environment.unwrap_or_default(),
            },
        }
    }
}

/// The File API extension for Boa contexts.
#[derive(Clone)]
pub struct FileApiExtension {
    config: ExtensionConfig,
}

impl FileApiExtension {
    /// Returns a new builder.
    #[must_use]
    pub fn builder() -> FileApiExtensionBuilder {
        FileApiExtensionBuilder::default()
    }

    /// Registers `Blob`, `File` and the internal `FileList` machinery.
    ///
    /// The registration is atomic: when any preflight or installation step
    /// fails, none of the globals is left installed. A second call on the
    /// same context returns [`RegisterError::AlreadyRegistered`].
    pub fn register(&self, context: &mut Context) -> Result<FileApiHandle, RegisterError> {
        // Re-registration rule (b): every second call is rejected.
        if context.has_data::<RegisteredSpecs>() {
            return Err(RegisterError::AlreadyRegistered);
        }

        // Streams shim availability is checked before any globalThis
        // mutation: without the shim there is no host stream adapter.
        // The flag and the feature must both agree: the flag opts out at
        // runtime, the feature opts out at compile time. The combined
        // condition keeps both live in every feature configuration.
        let shim_available = self.config.streams_shim && cfg!(feature = "streams-shim");
        if !shim_available {
            return Err(RegisterError::StreamsShimDisabled);
        }

        // DOM shim availability is checked before any globalThis mutation:
        // without the shim there is no host DOM adapter. Same flag/feature
        // combination as the streams shim.
        let dom_available = self.config.dom_shim && cfg!(feature = "dom-shim");
        if !dom_available {
            return Err(RegisterError::DomShimDisabled);
        }

        // Host limits are validated before any globalThis mutation: an
        // invalid configuration fails with a typed error and installs
        // nothing. This is the full `FileApiLimits::validate()` contract,
        // including the M3-B stream chunk-size range.
        if let Err(error) = self.config.limits.validate() {
            let message = format!("invalid FileApiLimits: {error}");
            return Err(match error {
                boa_fapi_core::file_api_error::FileApiError::ResourceLimit(_) => {
                    RegisterError::Js(crate::error::range_error(&message))
                }
                _ => RegisterError::Js(crate::error::type_error(&message)),
            });
        }

        // Build phase: no observable state changes yet. Constructors and
        // prototypes are ordinary objects until installed.
        let blob_spec = build_blob_class(context)?;
        let file_spec = build_file_class(context, blob_spec.prototype())?;
        let file_list_proto = build_file_list_prototype(context)?;
        #[cfg(feature = "streams-shim")]
        let stream_specs = crate::streams::build_stream_specs(context)?;
        #[cfg(feature = "dom-shim")]
        let error_prototype = context.intrinsics().constructors().error().prototype();
        #[cfg(feature = "dom-shim")]
        let dom_specs = crate::dom::build_dom_specs(context, error_prototype)?;
        #[cfg(feature = "dom-shim")]
        let filereader_specs =
            crate::filereader::build_filereader_specs(context, dom_specs.event_target.prototype())?;
        // `FileReaderSync` is built only for worker descriptors; window
        // and service-worker contexts install no such global at all.
        #[cfg(feature = "dom-shim")]
        let sync_specs = if self.config.environment.file_reader_sync_enabled() {
            Some(crate::filereader_sync::build_sync_specs(context)?)
        } else {
            None
        };
        // Preflight: extensibility and every own global name.
        let global = context.global_object();
        if !global.is_extensible(context).map_err(RegisterError::Js)? {
            return Err(RegisterError::GlobalNotExtensible);
        }
        let keys = global
            .own_property_keys(context)
            .map_err(RegisterError::Js)?;
        // `FileReaderSync` participates in the preflight only when the
        // worker capability installs it; otherwise the name is untouched.
        #[cfg(feature = "dom-shim")]
        let sync_enabled = self.config.environment.file_reader_sync_enabled();
        for name in [
            "Blob",
            "File",
            "FileList",
            #[cfg(feature = "streams-shim")]
            "ReadableStream",
            #[cfg(feature = "streams-shim")]
            "ReadableStreamDefaultReader",
            #[cfg(feature = "dom-shim")]
            "EventTarget",
            #[cfg(feature = "dom-shim")]
            "Event",
            #[cfg(feature = "dom-shim")]
            "ProgressEvent",
            #[cfg(feature = "dom-shim")]
            "DOMException",
            #[cfg(feature = "dom-shim")]
            "FileReader",
        ] {
            let key = PropertyKey::from(js_string!(name));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict(name.to_owned()));
            }
        }
        #[cfg(feature = "dom-shim")]
        if sync_enabled {
            let key = PropertyKey::from(js_string!("FileReaderSync"));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict("FileReaderSync".to_owned()));
            }
        }

        // Install phase with rollback.
        if let Err(error) = install_globals(
            context,
            &blob_spec,
            &file_spec,
            #[cfg(feature = "streams-shim")]
            &stream_specs,
            #[cfg(feature = "dom-shim")]
            &dom_specs,
            #[cfg(feature = "dom-shim")]
            &filereader_specs,
            #[cfg(feature = "dom-shim")]
            sync_specs.as_ref(),
        ) {
            rollback_globals(
                context,
                #[cfg(feature = "dom-shim")]
                sync_specs.is_some(),
            )?;
            return Err(RegisterError::Js(error));
        }

        let specs = RegisteredSpecs {
            blob: blob_spec,
            file: file_spec,
            file_list_proto,
            #[cfg(feature = "streams-shim")]
            streams: Some(stream_specs),
            #[cfg(feature = "dom-shim")]
            dom: Some(dom_specs),
            #[cfg(feature = "dom-shim")]
            filereader: Some(filereader_specs),
            #[cfg(feature = "dom-shim")]
            sync_reader: sync_specs,
            #[cfg(feature = "fs")]
            shutdown: crate::lifecycle::ShutdownFlag::new(),
            config: self.config.clone(),
        };
        context.insert_data::<RegisteredSpecs>(specs.clone());

        Ok(FileApiHandle {
            specs: specs.clone(),
            #[cfg(feature = "fs")]
            shutdown: specs.shutdown.clone(),
        })
    }
}

/// Builds the `Blob` constructor/prototype pair.
fn build_blob_class(context: &mut Context) -> JsResult<StandardConstructor> {
    use boa_engine::native_function::NativeFunction;

    let mut builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(blob::constructor));
    builder.name("Blob");
    // Web IDL: both constructor arguments are optional, so `length` is the
    // shortest effective overload (0).
    builder.length(0);
    let spec = builder.build();
    blob::init_prototype(&spec.prototype(), context)?;
    Ok(spec)
}

/// Builds the `File` constructor/prototype pair.
///
/// `File.prototype` inherits from `Blob.prototype`.
fn build_file_class(
    context: &mut Context,
    blob_prototype: JsObject,
) -> JsResult<StandardConstructor> {
    use boa_engine::native_function::NativeFunction;

    let mut builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(file::constructor));
    builder.name("File");
    // Web IDL: `fileBits` and `fileName` are required, `options` optional.
    builder.length(2);
    builder.inherit(blob_prototype);
    let spec = builder.build();
    file::init_prototype(&spec.prototype(), context)?;
    Ok(spec)
}

/// Builds the FileList prototype object (no public constructor exists).
fn build_file_list_prototype(context: &mut Context) -> JsResult<JsObject> {
    let object_prototype = context.intrinsics().constructors().object().prototype();
    let prototype = JsObject::from_proto_and_data(object_prototype, OrdinaryPrototype);
    file_list::init_prototype(&prototype, context)?;
    Ok(prototype)
}

/// Marker native data for plain prototype objects.
#[derive(Debug, Trace, Finalize, JsData)]
struct OrdinaryPrototype;

/// Installs `Blob` and `File` on the global object (Web IDL attributes:
/// writable, non-enumerable, configurable). `FileList` installs nothing.
/// The streams shim installs `ReadableStream` and
/// `ReadableStreamDefaultReader` the same way; the DOM shim installs
/// `EventTarget`, `Event`, `ProgressEvent`, `DOMException` and `FileReader`;
/// worker environments additionally install `FileReaderSync`.
fn install_globals(
    context: &mut Context,
    blob_spec: &StandardConstructor,
    file_spec: &StandardConstructor,
    #[cfg(feature = "streams-shim")] stream_specs: &crate::streams::StreamSpecs,
    #[cfg(feature = "dom-shim")] dom_specs: &crate::dom::DomSpecs,
    #[cfg(feature = "dom-shim")] filereader_specs: &crate::filereader::FileReaderSpecs,
    #[cfg(feature = "dom-shim")] sync_specs: Option<&crate::filereader_sync::FileReaderSyncSpecs>,
) -> JsResult<()> {
    let global = context.global_object();
    for (name, constructor) in [
        ("Blob", blob_spec.constructor()),
        ("File", file_spec.constructor()),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "streams-shim")]
    for (name, constructor) in [
        ("ReadableStream", stream_specs.stream.constructor()),
        (
            "ReadableStreamDefaultReader",
            stream_specs.reader.constructor(),
        ),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "dom-shim")]
    for (name, constructor) in [
        ("EventTarget", dom_specs.event_target.constructor()),
        ("Event", dom_specs.event.constructor()),
        ("ProgressEvent", dom_specs.progress_event.constructor()),
        ("DOMException", dom_specs.dom_exception.constructor()),
        ("FileReader", filereader_specs.reader.constructor()),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "dom-shim")]
    if let Some(sync) = sync_specs {
        global.define_property_or_throw(
            js_string!("FileReaderSync"),
            PropertyDescriptor::builder()
                .value(sync.sync.constructor())
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    Ok(())
}

/// Removes partially installed globals after a failed install.
///
/// `remove_sync` mirrors the worker capability: when the failed
/// registration would have installed `FileReaderSync`, its name is rolled
/// back as well; otherwise the name is left untouched.
fn rollback_globals(
    context: &mut Context,
    #[cfg(feature = "dom-shim")] remove_sync: bool,
) -> Result<(), RegisterError> {
    let global = context.global_object();
    for name in [
        "Blob",
        "File",
        #[cfg(feature = "streams-shim")]
        "ReadableStream",
        #[cfg(feature = "streams-shim")]
        "ReadableStreamDefaultReader",
        #[cfg(feature = "dom-shim")]
        "EventTarget",
        #[cfg(feature = "dom-shim")]
        "Event",
        #[cfg(feature = "dom-shim")]
        "ProgressEvent",
        #[cfg(feature = "dom-shim")]
        "DOMException",
        #[cfg(feature = "dom-shim")]
        "FileReader",
    ] {
        // The property was just defined as configurable, so deletion succeeds
        // on ordinary globals. A hostile exotic global may still refuse; the
        // reported error then reflects the rollback failure.
        global
            .delete_property_or_throw(js_string!(name), context)
            .map_err(RegisterError::Js)?;
    }
    #[cfg(feature = "dom-shim")]
    if remove_sync {
        global
            .delete_property_or_throw(js_string!("FileReaderSync"), context)
            .map_err(RegisterError::Js)?;
    }
    Ok(())
}

/// Opaque handle to a registered File API extension.
///
/// The handle owns the registered constructors/prototypes and configuration,
/// allowing the host to create Blob/File/FileList objects without JS.
/// After [`FileApiHandle::shutdown`] the handle rejects every new host
/// operation; already-created JS objects keep their payload but their
/// filesystem reads fail on the next chunk boundary.
#[derive(Clone)]
pub struct FileApiHandle {
    specs: RegisteredSpecs,
    #[cfg(feature = "fs")]
    shutdown: crate::lifecycle::ShutdownFlag,
}

#[cfg(feature = "fs")]
impl FileApiHandle {
    /// Returns `true` after [`FileApiHandle::shutdown`].
    fn is_shutdown(&self) -> bool {
        self.shutdown.is_shutdown()
    }

    /// Fails with a `TypeError` when the runtime is shut down.
    fn reject_if_shutdown(&self) -> JsResult<()> {
        if self.is_shutdown() {
            return Err(crate::error::type_error(
                "the File API runtime is shut down",
            ));
        }
        Ok(())
    }
}

impl FileApiHandle {
    /// Creates a `Blob` from host bytes.
    ///
    /// The bytes are wrapped in an immutable memory source; the media type
    /// is normalized by the M1 rules. After `shutdown` the call fails
    /// before touching JS state.
    pub fn blob_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        media_type: &str,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        #[cfg(feature = "fs")]
        self.reject_if_shutdown()?;
        let data = blob::data_from_bytes(bytes.into(), media_type, self.specs.limits())
            .map_err(js_from_core)?;
        Ok(blob::create_instance(
            BlobNative::new(data),
            self.specs.blob_proto().clone(),
        ))
    }

    /// Creates a `File` from host bytes.
    ///
    /// `name` is a display name: no basename is computed, but `/` is
    /// replaced by `:` like the JS constructor. `options.last_modified` of
    /// `None` reads the injected clock. After `shutdown` the call fails
    /// before touching JS state.
    pub fn file_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        name: &str,
        options: HostFileOptions,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        #[cfg(feature = "fs")]
        self.reject_if_shutdown()?;
        let native = file::native_from_bytes(
            bytes.into(),
            name,
            &options.media_type,
            options.last_modified,
            self.specs.config.clock.as_ref(),
            self.specs.limits(),
        )
        .map_err(js_from_core)?;
        Ok(JsObject::from_proto_and_data(
            self.specs.file_proto().clone(),
            native,
        ))
    }

    /// Creates a `File` from a pre-authorized filesystem resource.
    ///
    /// The host passes an opaque [`FileResource`](boa_fapi_core::policy::FileResource)
    /// handle (already open, read-only, capability-checked) plus the only
    /// name JS observes, `display_name` (no basename is computed from any
    /// secret host location; `/` becomes `:` like the JS constructor). The
    /// resource is validated (live snapshot matches the import snapshot,
    /// plus a preflight size check against `max_blob_size`) before any
    /// JS-visible object exists, so a denial leaves no partial state. Name
    /// normalization matches the M1/M2 `File` behavior. Without the `fs`
    /// Cargo feature this method does not exist; the memory API and
    /// registration keep working unchanged.
    ///
    /// Two enforced boundaries (recorded in the ADR):
    ///
    /// - signature adaptation: the target shape takes `&dyn FileResource`,
    ///   but `ByteSource: 'static` cannot borrow it, so this method takes
    ///   `Arc<dyn FileResource>` ownership instead. The backing
    ///   `ArcResourceSource` revalidates the live snapshot before every
    ///   read and verifies exact bytes afterwards, with no partial result
    ///   and no location/identity disclosure.
    /// - shutdown handle release: the caller additionally passes the
    ///   `FsRegistry` that owns the resource, so the import can register a
    ///   shutdown closer (`close_all`) with it. [`FileApiHandle::shutdown`]
    ///   then drops OS handles immediately instead of deferring release to
    ///   registry destruction.
    #[cfg(feature = "fs")]
    pub fn file_from_resource(
        &self,
        registry: &boa_fapi_fs::FsRegistry,
        resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
        display_name: &str,
        options: HostFileOptions,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        use std::sync::Arc;
        self.reject_if_shutdown()?;
        // Authorize before any JS-visible object exists. The grant check
        // uses the live snapshot so a resource that changed between open
        // and import is denied with no partial state.
        let live = resource
            .current_snapshot()
            .map_err(crate::error::js_from_core)?;
        let grant = boa_fapi_core::policy::FileGrant::new(
            resource.resource_id(),
            resource.import_snapshot(),
        );
        // A grant whose import snapshot no longer matches the live state
        // is stale: deny without creating anything.
        if grant.snapshot != live {
            return Err(crate::error::js_from_core(
                boa_fapi_core::file_api_error::FileApiError::SnapshotChanged,
            ));
        }
        // Enforced (not advisory): live-handle imports exist only on
        // platforms with a strong open-handle identity. On Unix the
        // live-vs-import comparison detects replacement; elsewhere a
        // filesystem-backed resource is refused outright — hosts must use
        // `copy_on_import` (immutable memory bytes via `file_from_bytes`)
        // or deny the import. Memory snapshots are always valid.
        //
        // The check runs only when the platform reports a weak identity so
        // that `platform_has_strong_identity` stays mockable in unit tests
        // without changing production behavior.
        #[cfg(not(unix))]
        if matches!(
            grant.snapshot,
            boa_fapi_core::snapshot::SnapshotState::Filesystem(_)
        ) && !boa_fapi_fs::platform_has_strong_identity()
        {
            return Err(crate::error::js_from_core(
                boa_fapi_core::file_api_error::FileApiError::PermissionDenied,
            ));
        }
        #[cfg(unix)]
        {
            let _ = &grant;
        }
        // Track the registry for handle release at shutdown: `close_all`
        // is idempotent, so tracking once per import is harmless even when
        // several imports share one registry.
        let tracked = registry.clone();
        self.shutdown.track(move || tracked.close_all());
        let adapter: Arc<dyn boa_fapi_core::source::ByteSource> =
            Arc::new(ArcResourceSource::new(resource, self.shutdown.clone()));
        let data = blob::data_from_fs_source(adapter, &options.media_type, self.specs.limits())
            .map_err(js_from_core)?;
        let native = file::native_from_data(
            data,
            display_name,
            options.last_modified,
            self.specs.config.clock.as_ref(),
        );
        Ok(JsObject::from_proto_and_data(
            self.specs.file_proto().clone(),
            native,
        ))
    }

    /// Creates a `FileList` from File objects.
    ///
    /// Every element is brand-validated as a File before any output object
    /// is created; a non-File element fails without partial state. Only
    /// explicit `File` objects are accepted: this never enumerates a
    /// directory. After `shutdown` the call fails before touching JS state.
    pub fn file_list(
        &self,
        files: impl IntoIterator<Item = JsObject>,
        context: &mut Context,
    ) -> JsResult<JsObject> {
        #[cfg(feature = "fs")]
        self.reject_if_shutdown()?;
        let mut validated = Vec::new();
        for file in files {
            brand::require_file_object(&file)?;
            validated.push(file);
        }
        file_list::create(validated, &self.specs.file_list_proto, context)
    }

    /// Shuts down the registered File API runtime.
    ///
    /// Idempotent: repeated calls neither panic nor enqueue callbacks.
    /// Atomic with respect to new host-created `File`/resource operations
    /// (they are rejected once closed). Pending filesystem reads observe
    /// the shared cancellation; every tracked registry runs `close_all`,
    /// so OS handles are dropped immediately (not deferred to registry
    /// destruction); new reads, materializations, stream pulls, and
    /// FileReader jobs after shutdown settle nothing against a destroyed
    /// context. No locations or identities leak into queues, errors, or JS
    /// objects.
    #[cfg(feature = "fs")]
    pub fn shutdown(&self, context: &mut Context) -> Result<(), RegisterError> {
        crate::lifecycle::shutdown_runtime(&self.shutdown, context)
    }

    /// Returns the environment descriptor this handle was registered with.
    ///
    /// Worker descriptors installed the normative `FileReaderSync`;
    /// `Window` and `ServiceWorker` did not.
    pub fn environment(&self) -> FileApiEnvironment {
        self.specs.environment()
    }
}

/// Shutdown-aware [`ByteSource`](boa_fapi_core::source::ByteSource) over an
/// owned host [`FileResource`](boa_fapi_core::policy::FileResource).
///
/// Captures the import snapshot and length once; every `read_range`
/// validates, in order: caller cancellation, the runtime shutdown flag,
/// checked range arithmetic, the live snapshot against the import snapshot
/// (before reading), then exact byte-count verification (after reading).
/// Never returns partial bytes; never touches Boa from completion; never
/// exposes paths or identities.
#[cfg(feature = "fs")]
struct ArcResourceSource {
    resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
    import_snapshot: boa_fapi_core::snapshot::SnapshotState,
    len: u64,
    shutdown: crate::lifecycle::ShutdownFlag,
}

#[cfg(feature = "fs")]
impl ArcResourceSource {
    /// Captures the import snapshot and length without creating JS state.
    fn new(
        resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
        shutdown: crate::lifecycle::ShutdownFlag,
    ) -> Self {
        let import_snapshot = resource.import_snapshot();
        let len = match &import_snapshot {
            boa_fapi_core::snapshot::SnapshotState::Filesystem(state) => state.size(),
            _ => 0,
        };
        Self {
            resource,
            import_snapshot,
            len,
            shutdown,
        }
    }
}

#[cfg(feature = "fs")]
impl boa_fapi_core::source::ByteSource for ArcResourceSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        self.import_snapshot.clone()
    }

    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &boa_fapi_core::cancellation::CancellationToken,
    ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
        use boa_fapi_core::file_api_error::FileApiError;
        if cancel.is_cancelled() || self.shutdown.cancel_token().is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        if self.shutdown.is_shutdown() {
            return Err(FileApiError::Cancelled);
        }
        if range.start > range.end {
            return Err(FileApiError::InvalidRange);
        }
        let len_u64 = range
            .end
            .checked_sub(range.start)
            .ok_or(FileApiError::InvalidRange)?;
        if range.end > self.len {
            return Err(FileApiError::InvalidRange);
        }
        let len = usize::try_from(len_u64).map_err(|_| {
            FileApiError::ResourceLimit(boa_fapi_core::error::ResourceLimitKind::MaterializeBytes)
        })?;
        if len == 0 {
            return Ok(bytes::Bytes::new());
        }
        // Snapshot validation before the read: replacement, truncation,
        // deletion, or permission change fails here with no bytes out.
        let live = self.resource.current_snapshot()?;
        if live != self.import_snapshot {
            return Err(FileApiError::SnapshotChanged);
        }
        if cancel.is_cancelled() || self.shutdown.cancel_token().is_cancelled() {
            return Err(FileApiError::Cancelled);
        }
        let bytes = self.resource.read_at(range.start, len)?;
        if bytes.len() != len {
            return Err(FileApiError::InvalidRange);
        }
        // Post-read identity confirmation: a replacement racing the read
        // surfaces here (or on the next chunk), never as partial old bytes.
        let after = self.resource.current_snapshot()?;
        if after != self.import_snapshot {
            return Err(FileApiError::SnapshotChanged);
        }
        Ok(bytes::Bytes::from(bytes))
    }
}

/// Options for [`FileApiHandle::file_from_bytes`].
#[derive(Debug, Clone, Default)]
pub struct HostFileOptions {
    /// Normalized by the M1 MIME rules.
    pub media_type: String,
    /// `None` reads the injected clock.
    pub last_modified: Option<i64>,
}
