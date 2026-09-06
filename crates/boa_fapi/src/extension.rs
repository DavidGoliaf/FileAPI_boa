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

    /// Creates the extension.
    #[must_use]
    pub fn build(&self) -> FileApiExtension {
        FileApiExtension {
            config: ExtensionConfig {
                clock: self.clock.clone().unwrap_or_else(|| Arc::new(SystemClock)),
                limits: self.limits.clone().unwrap_or_default(),
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

        // Build phase: no observable state changes yet. Constructors and
        // prototypes are ordinary objects until installed.
        let blob_spec = build_blob_class(context)?;
        let file_spec = build_file_class(context, blob_spec.prototype())?;
        let file_list_proto = build_file_list_prototype(context)?;
        // Preflight: extensibility and every own global name.
        let global = context.global_object();
        if !global.is_extensible(context).map_err(RegisterError::Js)? {
            return Err(RegisterError::GlobalNotExtensible);
        }
        let keys = global
            .own_property_keys(context)
            .map_err(RegisterError::Js)?;
        for name in ["Blob", "File", "FileList"] {
            let key = PropertyKey::from(js_string!(name));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict(name.to_owned()));
            }
        }

        // Install phase with rollback.
        if let Err(error) = install_globals(context, &blob_spec, &file_spec) {
            rollback_globals(context)?;
            return Err(RegisterError::Js(error));
        }

        let specs = RegisteredSpecs {
            blob: blob_spec,
            file: file_spec,
            file_list_proto,
            config: self.config.clone(),
        };
        context.insert_data::<RegisteredSpecs>(specs.clone());

        Ok(FileApiHandle { specs })
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
fn install_globals(
    context: &mut Context,
    blob_spec: &StandardConstructor,
    file_spec: &StandardConstructor,
) -> JsResult<()> {
    let global = context.global_object();
    for (name, spec) in [("Blob", blob_spec), ("File", file_spec)] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(spec.constructor())
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    Ok(())
}

/// Removes partially installed globals after a failed install.
fn rollback_globals(context: &mut Context) -> Result<(), RegisterError> {
    let global = context.global_object();
    for name in ["Blob", "File"] {
        // The property was just defined as configurable, so deletion succeeds
        // on ordinary globals. A hostile exotic global may still refuse; the
        // reported error then reflects the rollback failure.
        global
            .delete_property_or_throw(js_string!(name), context)
            .map_err(RegisterError::Js)?;
    }
    Ok(())
}

/// Opaque handle to a registered File API extension.
///
/// The handle owns the registered constructors/prototypes and configuration,
/// allowing the host to create Blob/File/FileList objects without JS.
#[derive(Clone)]
pub struct FileApiHandle {
    specs: RegisteredSpecs,
}

impl FileApiHandle {
    /// Creates a `Blob` from host bytes.
    ///
    /// The bytes are wrapped in an immutable memory source; the media type
    /// is normalized by the M1 rules.
    pub fn blob_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        media_type: &str,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
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
    /// `None` reads the injected clock.
    pub fn file_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        name: &str,
        options: HostFileOptions,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
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

    /// Creates a `FileList` from File objects.
    ///
    /// Every element is brand-validated as a File before any output object
    /// is created; a non-File element fails without partial state.
    pub fn file_list(
        &self,
        files: impl IntoIterator<Item = JsObject>,
        context: &mut Context,
    ) -> JsResult<JsObject> {
        let mut validated = Vec::new();
        for file in files {
            brand::require_file_object(&file)?;
            validated.push(file);
        }
        file_list::create(validated, &self.specs.file_list_proto, context)
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
