//! WPT test harness for the File API (M7).
//!
//! Library modules of the deterministic conformance runner:
//!
//! - [`manifest`] — `wpt-manifest.json` schema (pinned source, files with
//!   SHA-256, per-subtest expectations with capability gaps);
//! - [`inventory`] — pinned `wpt-inventory.json` and the primary
//!   disposition bijection;
//! - [`accounting`] — canonical run model and the `expectations_match` /
//!   `release_green` verdicts;
//! - [`hash`] — self-contained SHA-256 integrity helper;
//! - [`harness`] — minimal `testharness.js`-compatible prelude
//!   (`test`/`async_test`/`promise_test`, assertions, `done`/`step`);
//! - [`runner`] — per-test Boa `Context` execution with explicit
//!   `run_jobs()` pumping and one terminal status per test/subtest;
//! - [`report`] — deterministic JSON/JUnit serialization (sorted keys,
//!   no timestamps, no absolute paths, no secrets).

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod accounting;
pub mod harness;
pub mod hash;
pub mod inventory;
pub mod manifest;
pub mod report;
pub mod runner;
