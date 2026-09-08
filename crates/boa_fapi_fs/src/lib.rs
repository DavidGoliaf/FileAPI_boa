//! Filesystem-backed [`ByteSource`](boa_fapi_core::source::ByteSource)
//! with capability and snapshot security.
//!
//! The host opens a read-only resource **before** any JS `File` exists and
//! registers it with [`FsRegistry::register`], which snapshots the opaque
//! platform identity on the open handle (safe Rust only: `Metadata`-derived
//! identity hash, size, and modification time). The host then imports the
//! resource through the [`FileAccessPolicy`](boa_fapi_core::policy::FileAccessPolicy)
//! boundary; readers ([`FileSource`])
//! validate the live snapshot against the import snapshot before the first
//! chunk and before every new range operation.
//!
//! Security properties (no location ever crosses this boundary):
//!
//! - no filesystem location in any public type, method, or error;
//! - truncation, replacement, deletion, rename, permission loss, locks, and
//!   short reads map to typed
//!   [`FileApiError`](boa_fapi_core::file_api_error::FileApiError)s
//!   (JS sees only `NotReadableError` via the central mapping);
//! - `read_range` returns exactly the requested bytes or fails with no
//!   partial result;
//! - after [`FileSource::close`]/registry [`FsRegistry::close`]/
//!   [`FsRegistry::close_all`] or runtime shutdown (via closers registered
//!   with [`FsRegistry::on_shutdown`] and fired by
//!   [`FsRegistry::run_closers`]), OS handles are dropped immediately and
//!   reads fail; no callback touches a destroyed context.
//!
//! On platforms without a strong open-handle identity (Windows and other
//! non-Unix targets) direct imports are refused: hosts must use
//! [`open_copy_on_import`] (see [`platform_has_strong_identity`]).
#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

mod capability;
mod identity;
mod policy;
mod source;

pub use capability::{FsRegistry, RegisteredResource};
pub use identity::platform_has_strong_identity;
pub use policy::{DenyRawPathPolicy, RegistryPolicy, RootConfinedPolicy};
pub use source::{FileSource, HostFileSource, open_copy_on_import};
