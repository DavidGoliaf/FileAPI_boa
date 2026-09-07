//! Host-controlled lifecycle shutdown for the registered File API runtime.
//!
//! [`ShutdownFlag`] is a cheap shared closed-state cell owned by the
//! [`FileApiHandle`](crate::FileApiHandle). [`FileApiHandle::shutdown`]
//! flips it idempotently, runs every registered filesystem closer (which
//! drops OS handles immediately — see below), cancels pending work, and
//! makes late Boa jobs settle nothing against a destroyed context.
//!
//! Handle release model: `boa_fapi` never owns an `FsRegistry` directly
//! (that would couple the engine crate to `std::fs` I/O types). Instead
//! every `fs` import registers a closer with its registry *and* with the
//! shared [`ShutdownFlag`]; shutdown drains the flag's closer list and
//! each closer calls [`FsRegistry::close_all`] on its registry, so OS
//! handles are dropped at shutdown — not deferred to registry
//! destruction. Closers are idempotent (`close_all` is), so duplicate
//! tracking of one registry is harmless.
//!
//! Shutdown owns no Blob URL store or structured-clone lifetime (both are
//! M6 scope): the extension points stay reserved here without
//! implementation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use boa_engine::Context;

use boa_fapi_core::cancellation::CancellationToken;

/// Shared closed-state plus the runtime cancellation token.
///
/// Cloned into every `fs` import so that pending filesystem reads share
/// one cancellation source; cloned into the handle so shutdown is
/// idempotent and atomic with respect to new host operations. The
/// `closers` list holds one entry per tracked import; each entry fires at
/// most once, even across repeated shutdowns.
#[derive(Clone, Debug)]
pub(crate) struct ShutdownFlag {
    inner: Arc<ShutdownInner>,
}

#[derive(Debug)]
struct ShutdownInner {
    closed: AtomicBool,
    cancel: CancellationToken,
    closers: Mutex<Vec<ShutdownCloser>>,
}

/// One shutdown closer: drops OS handles of a single tracked registry.
struct ShutdownCloser {
    fire: Option<Box<dyn Fn() + Send>>,
}

impl std::fmt::Debug for ShutdownCloser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownCloser")
            .field("pending", &self.fire.is_some())
            .finish()
    }
}

impl ShutdownFlag {
    /// Creates a fresh open flag with its own cancellation token.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ShutdownInner {
                closed: AtomicBool::new(false),
                cancel: CancellationToken::new(),
                closers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Returns `true` after shutdown.
    pub(crate) fn is_shutdown(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Returns the shared runtime cancellation token.
    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Tracks a closer to run at shutdown.
    ///
    /// Typically `move || registry.close_all()`. Never runs user code at
    /// registration time. If shutdown already happened, the closer fires
    /// immediately instead of leaking.
    pub(crate) fn track(&self, closer: impl Fn() + Send + 'static) {
        if self.is_shutdown() {
            closer();
            return;
        }
        if let Ok(mut closers) = self.inner.closers.lock() {
            // Recheck under the lock: a concurrent `close()` may have
            // drained the list already.
            if self.is_shutdown() {
                drop(closers);
                closer();
            } else {
                closers.push(ShutdownCloser {
                    fire: Some(Box::new(closer)),
                });
            }
        }
    }

    /// Transitions to closed, runs every tracked closer exactly once, and
    /// cancels pending work. Idempotent.
    pub(crate) fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let closers = self
            .inner
            .closers
            .lock()
            .map(|mut closers| std::mem::take(&mut *closers))
            .unwrap_or_default();
        for mut closer in closers {
            if let Some(fire) = closer.fire.take() {
                fire();
            }
        }
        self.inner.cancel.cancel();
    }
}

/// Performs the host-controlled shutdown.
///
/// Idempotent: repeated calls neither panic nor enqueue callbacks, and
/// tracked closers fire at most once in total. New reads,
/// materializations, stream pulls, and FileReader jobs observe the closed
/// state and settle nothing after it.
pub(crate) fn shutdown_runtime(
    flag: &ShutdownFlag,
    _context: &mut Context,
) -> Result<(), crate::error::RegisterError> {
    flag.close();
    Ok(())
}
