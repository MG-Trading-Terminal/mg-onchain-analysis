//! Storage crate error type.
//!
//! All public functions in `crates/storage` return `Result<_, StorageError>`.
//! Callers that want an `anyhow::Error` can use `.map_err(anyhow::Error::from)`.

use thiserror::Error;

/// Unified error type for all storage operations (Postgres).
#[derive(Debug, Error)]
pub enum StorageError {
    /// A Postgres (sqlx) error.
    #[error("postgres error: {0}")]
    Postgres(#[from] sqlx::Error),

    /// A Postgres migration error.
    #[error("postgres migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// A serialization / deserialization error.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A configuration / initialization error (bad URL, missing credentials, etc.).
    #[error("configuration error: {0}")]
    Config(String),

    /// Checkpoint not found or corrupt.
    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    /// A generic error not covered by the variants above.
    #[error("storage error: {0}")]
    Other(String),
}
