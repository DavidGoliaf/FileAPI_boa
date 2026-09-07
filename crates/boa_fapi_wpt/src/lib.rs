//! WPT test harness for the File API (M7).
//!
//! Library modules of the deterministic conformance runner:
//!
//! - [`manifest`] — `wpt-manifest.json` schema (pinned source, files with
//!   SHA-256, per-subtest expectations with capability gaps);
//! - [`harness`] — minimal `testharness.js`-compatible prelude
//!   (`test`/`async_test`/`promise_test`, assertions, `done`/`step`);
//! - [`runner`] — per-test Boa `Context` execution with explicit
//!   `run_jobs()` pumping and one terminal status per test/subtest;
//! - [`report`] — deterministic JSON/JUnit serialization (sorted keys,
//!   no timestamps, no absolute paths, no secrets).

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]
// `expect` is allowed in `#[cfg(test)]` unit modules only (test-only
// fixtures with static inputs); production paths return `Result`.
// The workspace lint still denies `expect_used` for non-test targets;
// this crate re-allows it crate-wide because its unit tests live in the
// same files — the guards suite pins that no production line uses it.
#![allow(clippy::expect_used)]

pub mod harness;
pub mod manifest;
pub mod report;
pub mod runner;
