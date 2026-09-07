//! Host-controlled lifecycle shutdown for the registered File API runtime.
//!
//! [`ShutdownFlag`] is a cheap shared closed-state cell owned by the
//! [`FileApiHandle`](crate::FileApiHandle). [`FileApiHandle::shutdown`]
//! flips it idempotently: pending filesystem reads observe the shared
//! cancellation token, new host operations are rejected before touching
//! JS state, and late Boa jobs find the closed state and settle nothing
//! against a destroyed context.
//!
//! Shutdown owns no Blob URL store or structured-clone lifetime (both are
//! M6 scope): the extension points stay reserved here without
//! implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use boa_engine::Context;

use boa_fapi_core::cancellation::CancellationToken;

/// Shared closed-state plus the runtime cancellation token.
///
/// Cloned into every `fs` import so that pending filesystem reads share
/// one cancellation source; cloned into the handle so shutdown is
/// idempotent and atomic with respect to new host operations.
#[derive(Clone, Debug)]
pub(crate) struct ShutdownFlag {
    closed: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl ShutdownFlag {
    /// Creates a fresh open flag with its own cancellation token.
    pub(crate) fn new() -> Self {
        Self {
            closed: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    /// Returns `true` after shutdown.
    pub(crate) fn is_shutdown(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Returns the shared runtime cancellation token.
    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Transitions to closed and cancels pending work. Idempotent.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }
}

/// Performs the host-controlled shutdown.
///
/// Idempotent: repeated calls neither panic nor enqueue callbacks. New
/// reads, materializations, stream pulls, and FileReader jobs observe the
/// closed state and settle nothing after it.
pub(crate) fn shutdown_runtime(
    flag: &ShutdownFlag,
    _context: &mut Context,
) -> Result<(), crate::error::RegisterError> {
    flag.close();
    Ok(())
}
