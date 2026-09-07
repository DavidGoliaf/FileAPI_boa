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
//! async reader. Filesystem sources, blob URLs, clone, full DOM/Workers
//! runtime, and the WPT harness are not included.
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

mod blob;
mod brand;
mod file;
mod file_list;
#[cfg(feature = "dom-shim")]
mod filereader;
#[cfg(feature = "dom-shim")]
mod filereader_sync;
#[cfg(feature = "dom-shim")]
mod package;
mod promise_read;
#[cfg(feature = "streams-shim")]
mod streams;
mod webidl;

mod tests;

pub use clock::{Clock, SystemClock};
pub use error::RegisterError;
pub use extension::{
    FileApiEnvironment, FileApiExtension, FileApiExtensionBuilder, FileApiHandle, HostFileOptions,
};
