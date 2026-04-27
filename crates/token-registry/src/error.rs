//! Error types for `crates/token-registry`.
//!
//! [`RegistryError`] is the single error type returned by all public APIs.
//! Marked `#[non_exhaustive]` so downstream consumers must handle a wildcard arm;
//! this lets new variants be added in minor releases without breaking callers.

use thiserror::Error;

/// All errors produced by the token-registry crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// RPC call failed after all retries were exhausted.
    #[error("RPC error after {attempts} attempts for {method}: {reason}")]
    RpcExhausted {
        method: &'static str,
        attempts: u32,
        reason: String,
    },

    /// RPC returned HTTP 429 (rate limited). Callers see this only if all
    /// retries also hit rate-limit (unusual — backoff should clear it first).
    #[error("RPC rate-limited (429) for {method}")]
    RpcRateLimited { method: &'static str },

    /// The account data bytes did not match the expected SPL Mint layout.
    #[error("invalid SPL Mint account data for {mint}: {reason}")]
    InvalidMintAccount { mint: String, reason: String },

    /// An address string could not be parsed as a Solana Base58 address.
    #[error("invalid Solana address '{address}': {reason}")]
    InvalidAddress { address: String, reason: String },

    /// Postgres storage error (proxied from `mg-onchain-storage`).
    #[error("storage error: {0}")]
    Storage(#[from] mg_onchain_storage::error::StorageError),

    /// JSON serialization / deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// base64 decode error on account data.
    #[error("base64 decode error for account {account}: {reason}")]
    Base64Decode { account: String, reason: String },

    /// CEX wallet registry file could not be loaded.
    #[error("CEX registry load error: {0}")]
    CexRegistryLoad(String),

    /// Catch-all for unexpected errors (wraps anyhow).
    #[error("registry internal error: {0}")]
    Internal(String),
}

impl RegistryError {
    /// Returns `true` if this error is potentially transient (retry may succeed).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            RegistryError::RpcExhausted { .. } | RegistryError::RpcRateLimited { .. }
        )
    }
}
