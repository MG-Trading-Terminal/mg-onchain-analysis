//! Error type for `mg-onchain-graph`.
//!
//! All fallible operations in this crate return `GraphError`. Callers outside
//! this crate convert via `From<GraphError>` or map with `anyhow::Context`.

use thiserror::Error;

/// All errors that can be produced by the graph crate.
///
/// `#[non_exhaustive]` ensures that adding a new variant in Phase 3
/// (e.g. `InsufficientEdges`) is not a breaking change for downstream crates.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GraphError {
    /// A Postgres query or connection error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A numeric field in the DB could not be parsed to the expected Rust type.
    ///
    /// Most commonly: a `NUMERIC(39,0)` lamport column that fails `parse::<u128>()`.
    #[error("failed to parse DB field '{field}': {reason}")]
    ParseField {
        field: &'static str,
        reason: String,
    },

    /// A required configuration key has an invalid value (e.g. zero batch size).
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The cluster store was queried for a chain that has no data yet.
    ///
    /// Not a hard error — callers should treat this as `Ok(None)` / `Ok(vec![])`.
    #[error("no data for chain '{chain}'")]
    NoDataForChain { chain: String },

    /// UUID generation failed (should be unreachable in practice).
    #[error("UUID derivation error: {0}")]
    Uuid(String),

    /// An address string could not be parsed for a graph operation.
    ///
    /// Raised when an address value from the DB or an incoming request fails
    /// chain-specific validation (e.g. invalid Base58 for Solana, non-checksum
    /// EVM address). The string is the invalid address value.
    #[error("invalid address for graph op: {0}")]
    InvalidAddress(String),

    /// A `label_type` string was not recognised.
    ///
    /// Raised by `LabelType::from_db_str` when a value read from
    /// `address_labels.label_type` does not match any known variant.
    /// The string is the unrecognised value.
    #[error("unknown label_type: {0}")]
    UnknownLabelType(String),

    /// Catch-all for internal errors that don't fit a specific variant.
    #[error("internal error: {0}")]
    Internal(String),
}
