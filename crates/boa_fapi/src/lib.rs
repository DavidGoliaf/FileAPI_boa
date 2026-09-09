//! Boa bindings for the File API: `Blob`, `File` and `FileList`.
//!
//! This crate is the only workspace member allowed to depend on Boa. It
//! registers the M2 JavaScript surface in a real [`boa_engine::Context`]:
//!
//! - `new Blob(blobParts?, options?)` with USVString, BufferSource and
//!   Blob/File parts, `size`/`type` getters and `Blob.prototype.slice`;
//! - `new File(fileBits, fileName, options?)` inheriting from `Blob`, with
//!   `name`/`lastModified` getters and an injectable [`Clock`];
//! - `FileList` objects created by the host via [`FileApiHandle::file_list`].
//!
//! M3-A adds memory-backed promise reads `Blob.prototype.text()`,
//! `Blob.prototype.arrayBuffer()` and `Blob.prototype.bytes()` (inherited
//! by `File`). Each call returns a pending `Promise` immediately; the read,
//! packaging, and settlement run in a Boa promise job after the embedder
//! calls `Context::run_jobs()`. After M4-A rejections use the central
//! `DOMException` mapping.
//!
//! M3-B adds the capability-checked `ReadableStream` shim for
//! `Blob.prototype.stream()`/`textStream()`.
//!
//! M4-A adds the minimal self-contained `dom-shim` (`EventTarget`, `Event`,
//! `ProgressEvent`, `DOMException`) and the asynchronous `FileReader` for
//! memory-backed `Blob`/`File`. Every read enqueues FileReading jobs
//! delivered through the ordinary `Context::run_jobs()` cycle.
//!
//! M4-B adds the worker-only synchronous `FileReaderSync` for
//! memory-backed `Blob`/`File`, selected explicitly with
//! [`FileApiEnvironment`]. Sync methods run fully on the calling stack
//! with no jobs or events and package through the same helpers as the
//! async reader.
//!
//! M5 adds the capability-based filesystem `File` under the `fs` feature
//! (default on): the host registers an already-open read-only resource in
//! `boa_fapi_fs::FsRegistry` and imports it with
//! [`FileApiHandle::file_from_resource`]. JS observes only content and the
//! explicit display name; every range operation revalidates the opaque
//! snapshot. [`FileApiHandle::shutdown`] cancels pending filesystem work,
//! rejects new operations, and lets late jobs settle nothing after context
//! destruction.
//!
//! M6 adds the isolated Blob URL store with `URL.createObjectURL()` /
//! `URL.revokeObjectURL()` (feature `url-shim`, default on) and the
//! versioned structured-clone bridge for `Blob`/`File`/`FileList`
//! (feature `structured-clone`, default on, no `boa-idb` dependency):
//! URLs serialize as `blob:<serialized-origin>/<uuid-v4>` and resolve
//! only within the same origin *and* storage partition; clone payloads
//! carry materialized immutable bytes plus public metadata, never paths,
//! capabilities, OS handles or snapshot identities. Full DOM/Workers
//! runtime and the WPT harness are not included.
//!
//! The engine-independent data model and algorithms live in `boa_fapi_core`.
//!
//! # Example
//!
//! ```no_run
//! use boa_engine::{Context, Source, js_string};
//! use boa_fapi::FileApiExtension;
//!
//! let extension = FileApiExtension::builder().build();
//! let context = &mut Context::default();
//! let handle = extension.register(context).expect("registration failed");
//!
//! let value = context
//!     .eval(Source::from_bytes(r#"new Blob(["abc"]).size"#))
//!     .expect("evaluation failed");
//! assert_eq!(value.to_number(context).expect("number"), 3.0);
//!
//! // Host-side object creation:
//! let blob = handle
//!     .blob_from_bytes(bytes::Bytes::from_static(b"abc"), "text/plain", context)
//!     .expect("blob creation failed");
//! ```

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod clock;
#[cfg(feature = "dom-shim")]
pub mod dom;
pub mod error;
pub mod extension;
pub mod io;

mod blob;
mod brand;
#[cfg(feature = "structured-clone")]
mod clone_bridge;
mod file;
mod file_list;
#[cfg(feature = "dom-shim")]
mod filereader;
#[cfg(feature = "dom-shim")]
mod filereader_sync;
mod lifecycle;
#[cfg(feature = "tracing")]
mod observability;
#[cfg(feature = "dom-shim")]
mod package;
mod promise_read;
#[cfg(feature = "streams-shim")]
mod streams;
#[cfg(feature = "url-shim")]
mod url_shim;
mod webidl;

mod tests;

pub use clock::{Clock, SystemClock};
pub use error::RegisterError;
pub use extension::{
    CloneAdapter, CloneBridgeDescriptor, FileApiEnvironment, FileApiExtension,
    FileApiExtensionBuilder, FileApiHandle, HostFileOptions, OsEntropy, UrlEntropySource,
};
pub use io::{
    FileApiContextId, FileIoCompletion, FileIoExecutor, FileIoOperationId, FileIoSubmitError,
    FileIoTask, FileIoWake, NoopWake, PollIoError, ThreadedFileIoExecutor,
};
