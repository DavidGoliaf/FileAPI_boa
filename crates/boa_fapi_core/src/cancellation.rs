//! Cancellation token for cooperative cancellation of File API operations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cooperative cancellation token.
///
/// Cloned tokens share the same underlying cancellation flag.
/// `cancel()` is idempotent; once cancelled, the token stays cancelled.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates a new, non-cancelled token.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Cancels the token. Idempotent — calling multiple times is safe.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Returns `true` if the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}
