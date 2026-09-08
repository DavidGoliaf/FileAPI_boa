//! Registration errors and JS-boundary error construction for the File API extension.

use boa_engine::{Context, JsError, JsNativeError};
use boa_fapi_core::error::ResourceLimitKind;
use boa_fapi_core::file_api_error::FileApiError;

/// Errors returned by [`FileApiExtension::register`](crate::FileApiExtension::register).
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The extension is already registered in this context.
    ///
    /// Returned when a *different* registration identity attempts to
    /// register on an already-registered context (see the identity-aware
    /// rule on `FileApiExtension::register`): the first registration is
    /// left unchanged and no global is reinstalled.
    #[error("the File API extension is already registered in this context")]
    AlreadyRegistered,
    /// The global object is not extensible, so the globals cannot be installed.
    #[error("the global object is not extensible")]
    GlobalNotExtensible,
    /// A required global name is already an own property of the global object.
    #[error("global name `{0}` is already defined")]
    NameConflict(String),
    /// The streams shim is disabled but a host stream adapter is required.
    ///
    /// Returned before any `globalThis` mutation when the `streams-shim`
    /// Cargo feature is off or `streams_shim(false)` was configured: this
    /// milestone implements no host stream adapter.
    #[error("the streams shim is disabled and no host stream adapter is available")]
    StreamsShimDisabled,
    /// The DOM shim is disabled but `FileReader` requires it.
    ///
    /// Returned before any `globalThis` mutation when the `dom-shim`
    /// Cargo feature is off or `dom_shim(false)` was configured: this
    /// milestone implements no host DOM adapter.
    #[error("the DOM shim is disabled and no host DOM adapter is available")]
    DomShimDisabled,
    /// A host structured-clone bridge speaks a foreign encoding version.
    ///
    /// Returned before any `globalThis` mutation when the explicitly
    /// registered [`CloneAdapter`](crate::CloneAdapter) reports a version
    /// other than the stable M6 encoding: the bridge is left unregistered
    /// and no clone globals are installed.
    #[error("the structured-clone bridge `{0}` speaks an incompatible version")]
    CloneBridgeIncompatible(String),
    /// A Boa engine error occurred while building or installing the classes.
    #[error(transparent)]
    Js(#[from] JsError),
}

/// Builds a JS `TypeError` with the given message.
pub(crate) fn type_error(message: &str) -> JsError {
    JsNativeError::typ().with_message(message.to_owned()).into()
}

/// Builds a JS `RangeError` with the given message.
pub(crate) fn range_error(message: &str) -> JsError {
    JsNativeError::range()
        .with_message(message.to_owned())
        .into()
}

/// Builds a plain JS `Error` for an unmapped failure fallback.
///
/// Carries no path, source, or body detail. Used only when the DOM shim is
/// unexpectedly absent (registration always installs it, so this path is
/// unreachable in practice) or when an engine error cannot be wrapped.
pub(crate) fn js_read_error(context: &mut Context) -> boa_engine::JsValue {
    JsNativeError::error()
        .with_message("blob read failed")
        .into_opaque(context)
        .into()
}

/// Maps a core [`FileApiError`] onto the matching JS error kind.
///
/// Resource limits on blob size and segment count surface as `RangeError`
/// (spec requirement: size limit overflow never wraps, it is a synchronous
/// range error); every other core failure is reported as a `TypeError`.
pub(crate) fn js_from_core(error: FileApiError) -> JsError {
    match error {
        FileApiError::ResourceLimit(
            ResourceLimitKind::BlobSize | ResourceLimitKind::BlobSegments,
        ) => range_error(&format!("blob limit exceeded: {error}")),
        _ => type_error(&format!("blob operation failed: {error}")),
    }
}
