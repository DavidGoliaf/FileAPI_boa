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
//! The engine-independent data model and algorithms live in `boa_fapi_core`.
//! M3-M7 APIs (text/arrayBuffer/bytes/stream, FileReader, blob URLs, WPT)
//! are intentionally absent.
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
pub mod error;
pub mod extension;

mod blob;
mod brand;
mod file;
mod file_list;
mod webidl;

mod tests;

pub use clock::{Clock, SystemClock};
pub use error::RegisterError;
pub use extension::{FileApiExtension, FileApiExtensionBuilder, FileApiHandle, HostFileOptions};
