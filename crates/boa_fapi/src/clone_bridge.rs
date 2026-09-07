//! Host structured-clone bridge surface (M6, feature `structured-clone`).
//!
//! This module owns no encoding: the versioned bytes live in
//! [`boa_fapi_core::clone`]. It owns the JS-visible consequence of the
//! bridge instead — today deliberately *nothing*: the M6 bridge is a
//! host-side capability (`encode`/`decode` through the registered
//! [`CloneAdapter`](crate::CloneAdapter)), not new JS globals. Keeping JS
//! globals out is a normative choice, not a gap: no `structuredClone`
//! global, no `Blob.prototype.clone`, no IDB constructor may appear here,
//! and the guards pin that absence.
//!
//! What the module does own:
//!
//! - the fake in-memory bridge used by the M6 integration tests (encode
//!   and decode through the core versioned codec, version-checked);
//! - the version-compatibility predicate shared by registration and the
//!   handle entry points, so both fail identically on a foreign version.

use boa_fapi_core::clone::{CLONE_ENCODING_VERSION, CloneError, FileApiClonePayload};

use crate::extension::{CloneAdapter, CloneBridgeDescriptor};

/// Returns `true` when the bridge speaks the stable M6 encoding version.
///
/// Shared by `register` (preflight before any `globalThis` mutation) and
/// the handle entry points, so both fail identically on a foreign version.
#[allow(dead_code)]
pub(crate) fn bridge_compatible(descriptor: &CloneBridgeDescriptor) -> bool {
    descriptor.version == CLONE_ENCODING_VERSION
}

/// Fake in-memory bridge for M6 integration tests.
///
/// Stores nothing: `encode` runs the core versioned encoder,
/// `decode` runs the core checked decoder. A foreign `version` makes every
/// call fail with [`CloneError::UnsupportedVersion`] before touching any
/// payload — the same predicate registration applies before mutating
/// `globalThis`.
///
/// Re-exported for host embedders as a reference bridge implementation;
/// the M6 suites exercise the equivalent local `TestBridge` instead.
#[derive(Debug, Clone)]
pub struct FakeCloneBridge {
    descriptor: CloneBridgeDescriptor,
}

impl FakeCloneBridge {
    /// Creates a bridge speaking the current encoding version.
    #[allow(dead_code)]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            descriptor: CloneBridgeDescriptor {
                name: name.into(),
                version: CLONE_ENCODING_VERSION,
            },
        }
    }

    /// Creates a bridge speaking a foreign version (rejection fixture).
    #[allow(dead_code)]
    pub fn with_version(name: impl Into<String>, version: u32) -> Self {
        Self {
            descriptor: CloneBridgeDescriptor {
                name: name.into(),
                version,
            },
        }
    }
}

impl CloneAdapter for FakeCloneBridge {
    fn descriptor(&self) -> CloneBridgeDescriptor {
        self.descriptor.clone()
    }

    fn encode(&self, payload: &FileApiClonePayload) -> Result<Vec<u8>, CloneError> {
        if !bridge_compatible(&self.descriptor) {
            return Err(CloneError::UnsupportedVersion);
        }
        payload.encode()
    }

    fn decode(&self, bytes: &[u8]) -> Result<FileApiClonePayload, CloneError> {
        if !bridge_compatible(&self.descriptor) {
            return Err(CloneError::UnsupportedVersion);
        }
        FileApiClonePayload::decode(bytes)
    }
}
