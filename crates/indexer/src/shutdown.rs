//! Graceful-shutdown listener.
//!
//! `ShutdownSignal` wraps a `tokio_util::sync::CancellationToken`. The run loop
//! selects on `signal.cancelled()` alongside the event stream. When a SIGINT /
//! Ctrl-C arrives (or the token is cancelled programmatically in tests), the
//! loop breaks out, flushes all buffers, saves the final checkpoint, and returns.
//!
//! # CancellationToken over tokio::signal directly
//!
//! Using `CancellationToken` decouples the run loop from a specific OS signal.
//! Tests can cancel the token without spawning a signal handler. Production code
//! installs the signal handler once in `server/main.rs` and passes the token down.
//!
//! # SIGTERM
//!
//! `tokio::signal::ctrl_c()` catches SIGINT (Ctrl-C) and, on Windows, Ctrl-Break.
//! On Unix, SIGTERM (the standard Docker stop signal) requires a separate handler:
//! `tokio::signal::unix::signal(SignalKind::terminate())`. Both are installed by
//! `ShutdownSignal::from_os_signals` — the indexer shuts down cleanly under both.

use tokio_util::sync::CancellationToken;
use tracing::info;

/// A lightweight wrapper around a `CancellationToken` that represents a
/// pending-or-received shutdown request.
///
/// Clone is cheap (internally an `Arc`). Pass a clone to each task that needs
/// to observe shutdown.
#[derive(Clone)]
pub struct ShutdownSignal {
    token: CancellationToken,
}

impl ShutdownSignal {
    /// Create a `ShutdownSignal` that is NOT yet cancelled.
    ///
    /// Use `signal.cancel()` to trigger shutdown, or install OS handlers via
    /// `ShutdownSignal::from_os_signals`.
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Create a `ShutdownSignal` and spawn a background task that cancels it on
    /// SIGINT (Ctrl-C) or SIGTERM.
    ///
    /// Returns the signal and the task handle. Drop the handle to cancel the
    /// signal-listener task (e.g., after the main loop exits).
    pub fn from_os_signals() -> Self {
        let signal = Self::new();
        let token = signal.token.clone();
        tokio::spawn(async move {
            // Install both SIGINT and SIGTERM handlers.
            let ctrl_c = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to install SIGINT handler");
            };

            #[cfg(unix)]
            let terminate = async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler")
                    .recv()
                    .await;
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            tokio::select! {
                _ = ctrl_c => {
                    info!("SIGINT received — initiating graceful shutdown");
                }
                _ = terminate => {
                    info!("SIGTERM received — initiating graceful shutdown");
                }
            }
            token.cancel();
        });
        signal
    }

    /// True if shutdown has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Returns a future that resolves when shutdown is requested.
    ///
    /// Use in `tokio::select!`:
    ///
    /// ```ignore
    /// tokio::select! {
    ///     event = stream.next() => { /* handle event */ }
    ///     _ = signal.cancelled() => { break; }
    /// }
    /// ```
    pub fn cancelled(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
        self.token.cancelled()
    }

    /// Cancel the signal (trigger shutdown).
    ///
    /// Idempotent — safe to call multiple times.
    pub fn cancel(&self) {
        self.token.cancel();
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}
