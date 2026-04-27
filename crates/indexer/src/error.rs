//! Indexer error type.
//!
//! All public functions in `crates/indexer` return `Result<_, IndexerError>`.
//! Callers that want `anyhow::Error` can call `.map_err(anyhow::Error::from)`.

use thiserror::Error;

/// Unified error type for the indexer pipeline.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexerError {
    /// Error received from the chain adapter stream or API call.
    #[error("chain adapter error: {0}")]
    Adapter(#[from] mg_onchain_chain_adapter::AdapterError),

    /// Error writing a batch to Postgres (or reading for reorg deletes).
    #[error("storage error: {0}")]
    Storage(#[from] mg_onchain_storage::StorageError),

    /// Error saving or loading a checkpoint.
    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    /// The event stream ended unexpectedly (adapter returned `None` without shutdown signal).
    #[error("event stream ended unexpectedly")]
    StreamEnded,

    /// Graceful shutdown was requested — not a real error, used as a sentinel
    /// to break out of the run loop cleanly.
    #[error("shutdown requested")]
    Shutdown,

    /// A misconfigured or logically inconsistent field in `IndexerConfig`.
    #[error("configuration error: {0}")]
    Config(String),
}
