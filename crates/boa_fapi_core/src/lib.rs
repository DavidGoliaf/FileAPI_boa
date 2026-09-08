//! `boa_fapi_core` — Platform-independent File API data model and algorithms.
//!
//! This crate implements the core data structures and algorithms for the
//! [File API](https://www.w3.org/TR/FileAPI/) without depending on Boa,
//! JavaScript types, DOM types, or any platform-specific I/O.
//!
//! # Modules
//!
//! - [`blob`] — Segmented immutable byte storage with File API slice semantics.
//! - [`cancellation`] — Cooperative cancellation tokens.
//! - [`endings`] — Line ending normalization.
//! - [`file_api_error`] — File API error types.
//! - [`error`] — Resource limit kind enum.
//! - [`limits`] — Configurable resource limits.
//! - [`mime`] — MIME type normalization.
//! - [`policy`] — Filesystem capability/policy boundary (Boa-free).
//! - [`snapshot`] — Snapshot state enum.
//! - [`source`] — Byte source abstraction and memory implementation.

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod blob;
pub mod cancellation;
pub mod endings;
pub mod error;
pub mod file_api_error;
pub mod limits;
pub mod mime;
pub mod policy;
pub mod snapshot;
pub mod source;
