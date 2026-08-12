#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A clonable cancellation signal for one compiler invocation.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates an unset token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. This operation is idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Fails when cancellation was requested.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// One cooperative compiler invocation was cancelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl Error for Cancelled {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_idempotent_cancellation() {
        let token = CancellationToken::new();
        let peer = token.clone();
        assert_eq!(token.check(), Ok(()));
        peer.cancel();
        peer.cancel();
        assert_eq!(token.check(), Err(Cancelled));
    }
}
