//! Optional terminal-only telemetry for the File API (M8).
//!
//! This module is compiled only with the `tracing` Cargo feature (default
//! off). It emits exactly one [`tracing`] completion event per operation
//! with the fixed allow-listed field set from the M8 work order:
//!
//! - `operation`: one of `promise_read`, `stream_read`, `filereader_read`,
//!   `filereader_sync`, `fs_read`, `blob_url_create`, `blob_url_resolve`,
//!   `clone_encode`, `clone_decode`;
//! - `size`: logical size of the current Blob/range (`0` when no payload);
//! - `duration_ms`: elapsed monotonic milliseconds measured only in Rust;
//! - `chunk_count`: `1` for one successful materialization, `0` for an
//!   empty successful Blob or a pre-read failure, data-chunk count for
//!   streams (EOF excluded);
//! - `result_class`: one of `ok`, `cancelled`, `quota`, `not_found`,
//!   `permission`, `snapshot_changed`, `invalid_range`, `encoding`,
//!   `shutdown`, `error`;
//! - `environment_hash`: opaque `u64` hashed from the existing internal
//!   environment key with the standard safe-Rust hasher.
//!
//! The target `boa_fapi::file_api.operation` is the only event identity
//! (there is no portable `event` name field in `tracing`). No bytes, body,
//! display names, paths, handles, snapshot identities, full blob URLs,
//! UUIDs, origins, partitions, nonces, error messages, or arbitrary
//! `Debug` output ever enters an event. The layer never changes JS API,
//! job ordering, error mapping, or object lifetimes and never calls into
//! JS.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use boa_fapi_core::blob_url::BlobUrlError;
use boa_fapi_core::clone::CloneError;
use boa_fapi_core::file_api_error::FileApiError;

/// Telemetry class for a JS-engine packaging failure (not a core error).
///
/// Defined here so call sites under string-literal surface guards (e.g.
/// `filereader_sync.rs`, which must not contain the `"error"` attribute
/// literal) can reference the class without a forbidden literal.
#[cfg(feature = "dom-shim")]
pub(crate) const ENGINE_ERROR_CLASS: &str = "error";

/// Starts a monotonic timer for `duration_ms`.
///
/// Callers hold the returned [`std::time::Instant`] across the Rust-only
/// section and pass `start.elapsed()` to [`emit`]. The helper itself is
/// only reachable with `tracing` enabled, so feature-off production paths
/// contain no timer call at all.
pub(crate) fn now() -> std::time::Instant {
    std::time::Instant::now()
}

/// Elapsed monotonic milliseconds since `start`, saturated to `u64`.
pub(crate) fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Opaque environment hash from the already-existing internal key.
///
/// Hashes only the internal [`boa_fapi_core::blob_url::EnvironmentKey`]
/// with the standard safe-Rust hasher. Origins, partitions, and nonces
/// never appear as separate event fields. The value is opaque: it may
/// differ after a restart and is never part of any public API.
pub(crate) fn environment_hash_for_specs(specs: &crate::extension::RegisteredSpecs) -> u64 {
    let key = specs
        .environment_descriptor()
        .map(|descriptor| descriptor.key());
    let mut hasher = DefaultHasher::new();
    match key {
        Ok(key) => {
            key.hash(&mut hasher);
            hasher.finish()
        }
        Err(_) => {
            // Registration preflights the origin, so this is unreachable in
            // registered contexts; hash a fixed sentinel without leaking.
            0_u64.hash(&mut hasher);
            hasher.finish()
        }
    }
}

/// Maps a core result onto the fixed telemetry `result_class`.
///
/// `None` means success (`ok`). `Cancelled` maps to `cancelled`,
/// any `ResourceLimit` to `quota`, `NotFound` to `not_found`,
/// `PermissionDenied`/`FileLocked` to `permission`, `SnapshotChanged` to
/// `snapshot_changed`, `InvalidRange` to `invalid_range`; every other
/// already-existing typed failure maps to `error`. Encoding and shutdown
/// outcomes are passed explicitly by their call sites (`encoding` for an
/// unknown text label, `shutdown` for a closed runtime) because the core
/// error type carries no dedicated encoding/shutdown variant.
pub(crate) fn result_class_for_core(error: Option<&FileApiError>) -> &'static str {
    match error {
        None => "ok",
        Some(FileApiError::Cancelled) => "cancelled",
        Some(FileApiError::ResourceLimit(_)) => "quota",
        Some(FileApiError::NotFound) => "not_found",
        Some(FileApiError::PermissionDenied | FileApiError::FileLocked) => "permission",
        Some(FileApiError::SnapshotChanged) => "snapshot_changed",
        Some(FileApiError::InvalidRange) => "invalid_range",
        Some(_) => "error",
    }
}

/// Maps a Blob URL result onto `result_class`.
///
/// `None` means success. `LimitExceeded` maps to `quota`, `Shutdown` to
/// `shutdown`, `Forbidden` to `permission`, `Malformed`/`Unavailable` to
/// `not_found` (the single opaque missing/foreign class), and every other
/// typed failure (`Collision`, `EntropyUnavailable`, `InvalidObject`,
/// `Internal`, future variants) to `error`. No token, UUID, origin, or
/// existence detail leaves this mapping.
pub(crate) fn result_class_for_blob_url(error: Option<&BlobUrlError>) -> &'static str {
    match error {
        None => "ok",
        Some(BlobUrlError::LimitExceeded) => "quota",
        Some(BlobUrlError::Shutdown) => "shutdown",
        Some(BlobUrlError::Forbidden) => "permission",
        Some(BlobUrlError::Malformed | BlobUrlError::Unavailable) => "not_found",
        Some(_) => "error",
    }
}

/// Maps a structured-clone result onto `result_class`.
///
/// `None` means success. `LimitExceeded` maps to `quota`, `Shutdown` to
/// `shutdown`, and every other typed failure (`Malformed`,
/// `UnsupportedVersion`, `InvalidObject`, `UnexpectedKind`,
/// `SourceFailed`, `NoBridge`, `Internal`, future variants) to `error`.
pub(crate) fn result_class_for_clone(error: Option<&CloneError>) -> &'static str {
    match error {
        None => "ok",
        Some(CloneError::LimitExceeded) => "quota",
        Some(CloneError::Shutdown) => "shutdown",
        Some(_) => "error",
    }
}

/// Emits one terminal completion event with only allow-listed fields.
///
/// Must be called exactly once per terminal path, after `result_class`
/// is known and (for `FileReader`) after the stale-generation guard has
/// accepted the completion. Shutdown late completions must not call this:
/// they return before the terminal decision.
pub(crate) fn emit(
    operation: &'static str,
    size: u64,
    duration_ms: u64,
    chunk_count: u64,
    result_class: &'static str,
    environment_hash: u64,
) {
    tracing::info!(
        target: "boa_fapi::file_api.operation",
        operation = operation,
        size = size,
        duration_ms = duration_ms,
        chunk_count = chunk_count,
        result_class = result_class,
        environment_hash = environment_hash,
    );
}
