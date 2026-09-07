//! Minimal `URL` shim for `URL.createObjectURL()` / `URL.revokeObjectURL()`.
//!
//! The shim owns no URL parsing: `blob:` shape checks live in
//! [`boa_fapi_core::blob_url`]. It owns exactly one global (`URL`, a plain
//! object — never a constructor) with two static methods. URL creation
//! reads the calling context's registered environment descriptor and
//! storage partition, draws CSPRNG bytes from the configured entropy
//! source, and stores the shared payload in the context's [`BlobUrlStore`];
//! resolution and revocation go through the same store with the same
//! same-partition checks. In the service-worker environment creation is
//! forbidden and the shim still installs (both methods throw the same
//! network-error equivalent for creation attempts).

use std::sync::Arc;

use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, JsSymbol, JsValue, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::blob_url::{BlobUrlError, EnvironmentKey, format_blob_url, format_uuid_v4};
use boa_gc::{Finalize, Trace};

use crate::brand;
use crate::error::type_error;
use crate::extension::SharedUrlStore;

/// Constructor/prototype pair installed as the `URL` global.
///
/// `URL` is a namespace object here, not a constructor: the `url-shim` is
/// explicitly not a WHATWG URL implementation and must never be mistaken
/// for one (see the crate ADR for the `uuid`/`getrandom` discussion).
#[derive(Clone)]
pub(crate) struct UrlSpecs {
    /// The installed `URL` namespace object.
    pub(crate) url: JsObject,
}

/// Builds the `URL` namespace object without touching any global.
///
/// Installation stays atomic in `extension.rs`.
pub(crate) fn build_url_specs(context: &mut Context) -> JsResult<UrlSpecs> {
    let object_prototype = context.intrinsics().constructors().object().prototype();
    let url = JsObject::from_proto_and_data(object_prototype, UrlNative);
    init_url_namespace(&url, context)?;
    Ok(UrlSpecs { url })
}

/// Brand of the `URL` namespace object.
#[derive(Debug, Trace, Finalize, JsData)]
pub(crate) struct UrlNative;

/// Requires the `URL` namespace brand (illegal-invocation guard).
fn require_url(this: &JsValue) -> JsResult<()> {
    let Some(object) = this.as_object() else {
        return Err(type_error("illegal invocation: expected URL"));
    };
    if object.is::<UrlNative>() {
        return Ok(());
    }
    Err(type_error("illegal invocation: expected URL"))
}

/// Reads one Blob/File payload from the single object argument.
///
/// `FileList`, arbitrary objects, forged objects and any `MediaSource`
/// shape fail with a synchronous `TypeError` through the single brand
/// gate; no partial URL is ever created.
fn url_blob_arg(args: &[JsValue]) -> JsResult<Arc<BlobData>> {
    let Some(first) = args.first() else {
        return Err(type_error("URL.createObjectURL requires a Blob argument"));
    };
    brand::require_blob(first).map_err(|_| type_error("URL.createObjectURL requires a Blob"))
}

/// `URL.createObjectURL(blob)`: brand → environment → quota → entropy → store.
///
/// Creation draws 16 CSPRNG bytes from the context's configured entropy
/// source, formats them as v4 UUID, serializes
/// `blob:<serialized-origin>/<uuid>` and inserts it under the caller's
/// full environment key. On UUID collision the generator retries with
/// fresh entropy (bounded); the live entry is never overwritten. Limit,
/// shutdown and failure semantics mirror the host `create_blob_url` path
/// exactly (shared helper), so JS and host creation cannot diverge. No Boa
/// job is enqueued; the string is returned in the calling realm.
fn create_object_url(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    require_url(this)?;
    let data = url_blob_arg(args)?;
    let specs = crate::extension::snapshot(context)?;
    let url = crate::extension::create_url_for_specs(&specs, &data)
        .map_err(|error| crate::url_shim::map_url_error(&error))?;
    Ok(JsValue::from(js_string!(url.as_str())))
}

/// `URL.revokeObjectURL(url)`: idempotent, oracle-free, always `undefined`.
///
/// Ownership-blind by specified `revokeObjectURL` semantics: any
/// well-formed URL removes its entry regardless of who asks (revoke is
/// not a gated read). Malformed input and unknown URLs are silent
/// no-ops. Either way nothing is reported, so revoke can never reveal
/// whether an entry exists. Like creation, this never enqueues a Boa job.
fn revoke_object_url(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    require_url(this)?;
    let specs = crate::extension::snapshot(context)?;
    let Some(first) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Ok(url) = first.to_string(context) else {
        return Ok(JsValue::undefined());
    };
    let url = url.to_std_string_lossy();
    specs.url_store().revoke(&url);
    Ok(JsValue::undefined())
}

/// Registers the two static methods plus `[Symbol.toStringTag] = "URL"`.
fn init_url_namespace(url: &JsObject, context: &mut Context) -> JsResult<()> {
    use boa_engine::native_function::NativeFunction;

    for (name, method, length) in [
        (
            js_string!("createObjectURL"),
            NativeFunction::from_fn_ptr(create_object_url),
            1,
        ),
        (
            js_string!("revokeObjectURL"),
            NativeFunction::from_fn_ptr(revoke_object_url),
            1,
        ),
    ] {
        let function = boa_engine::object::FunctionObjectBuilder::new(context.realm(), method)
            .name(name.clone())
            .length(length)
            .constructor(false)
            .build();
        url.define_property_or_throw(
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
    url.define_property_or_throw(
        tag_key,
        PropertyDescriptor::builder()
            .value(js_string!("URL"))
            .writable(false)
            .enumerable(false)
            .configurable(true),
        context,
    )?;
    Ok(())
}

/// Maps a [`BlobUrlError`] to the JS-visible failure.
///
/// Brand failures are plain `TypeError`s (wrong object kind); every URL
/// dereference failure — malformed, unknown, foreign, revoked, collision,
/// quota, shutdown, entropy — is the same network-error equivalent
/// (`TypeError` with one fixed message), so callers can never distinguish
/// a foreign entry from a missing one. No token, UUID, origin internals,
/// existence bit or host metadata ever reaches the message.
pub(crate) fn map_url_error(error: &BlobUrlError) -> boa_engine::JsError {
    match error {
        BlobUrlError::InvalidObject => type_error("URL.createObjectURL requires a Blob"),
        BlobUrlError::Forbidden => type_error("URL creation is not allowed in this context"),
        _ => type_error("blob URL is not available"),
    }
}

/// Serializes and inserts a store URL for `data` under `owner`.
///
/// Retries a bounded number of times on UUID collision with fresh entropy
/// per attempt; the live entry is never overwritten. The all-zero block
/// is the entropy-failure sentinel (see `UrlEntropySource`) and maps to
/// `EntropyUnavailable` before any store write. Quota and shape failures
/// surface without partial state.
pub(crate) fn insert_url(
    store: &SharedUrlStore,
    origin: &str,
    owner: &EnvironmentKey,
    data: &Arc<BlobData>,
    entropy: &dyn crate::extension::UrlEntropySource,
    cap: usize,
) -> Result<String, BlobUrlError> {
    for _ in 0..8 {
        let raw = entropy.fill_16();
        if raw == [0_u8; 16] {
            return Err(BlobUrlError::EntropyUnavailable);
        }
        let uuid = format_uuid_v4(raw);
        let url = format_blob_url(origin, &uuid);
        match store.insert_capped(url.clone(), owner.clone(), Arc::clone(data), cap) {
            Ok(()) => return Ok(url),
            Err(BlobUrlError::Collision) => continue,
            Err(other) => return Err(other),
        }
    }
    Err(BlobUrlError::Collision)
}
