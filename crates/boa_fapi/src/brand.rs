//! Brand checks for native File API data.
//!
//! These functions are the only gates between JS and the native data. Brands
//! are the internal native data types themselves: they cannot be forged from
//! script because no JS-visible mechanism can attach or copy them.

use std::sync::Arc;

use boa_engine::{JsResult, JsValue};
use boa_fapi_core::blob::BlobData;

use crate::blob::BlobNative;
use crate::error::type_error;
use crate::file::FileNative;

/// Validates that `this` carries the internal Blob brand and returns its data.
///
/// `File` objects pass the Blob brand. Forged or foreign objects — including
/// objects that copied public properties or that fake a constructor — fail
/// with a synchronous `TypeError`.
pub(crate) fn require_blob(this: &JsValue) -> JsResult<Arc<BlobData>> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a Blob"));
    };
    if let Some(native) = object.downcast_ref::<BlobNative>() {
        return Ok(native.blob_data().clone());
    }
    if let Some(native) = object.downcast_ref::<FileNative>() {
        return Ok(native.blob_data().clone());
    }
    Err(type_error("illegal invocation: expected a Blob"))
}

/// Validates that `this` carries the internal File brand.
///
/// Returns the blob payload, the immutable `name` and `lastModified`.
pub(crate) fn require_file(this: &JsValue) -> JsResult<(Arc<BlobData>, String, i64)> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a File"));
    };
    if let Some(native) = object.downcast_ref::<FileNative>() {
        return Ok((
            native.blob_data().clone(),
            native.name().to_owned(),
            native.last_modified(),
        ));
    }
    Err(type_error("illegal invocation: expected a File"))
}

/// Validates that `object` carries the internal File brand (host-side check).
pub(crate) fn require_file_object(object: &boa_engine::JsObject) -> JsResult<()> {
    if object.is::<FileNative>() {
        return Ok(());
    }
    Err(type_error("expected a File object"))
}

/// Validates that `this` carries the internal FileList brand, returning the
/// number of files it holds.
pub(crate) fn require_file_list(this: &JsValue) -> JsResult<usize> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected a FileList"));
    };
    if let Some(native) = object.downcast_ref::<crate::file_list::FileListNative>() {
        return Ok(native.len());
    }
    Err(type_error("illegal invocation: expected a FileList"))
}
