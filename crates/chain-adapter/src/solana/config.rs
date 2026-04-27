//! Configuration types for the Solana Yellowstone gRPC adapter.
//!
//! # Provider-agnostic design (ADR 0001 §D2)
//!
//! The same `SolanaAdapterConfig` works against all three providers:
//!
//! | Provider | Auth mechanism | Example endpoint |
//! |----------|----------------|-----------------|
//! | Helius LaserStream | `x-api-key` header via `auth_token` | `https://mainnet.helius-rpc.com` |
//! | Triton Dragon's Mouth | gRPC metadata token via `auth_token` | `https://ams1.rpc.triton.one:443` |
//! | Self-hosted | No auth (or custom via `auth_token`) | `http://localhost:10000` |
//!
//! Provider discrimination is NEVER in code — only in this config struct.
//! See `config/adapters.toml.example` for complete examples.
//!
//! # Provider-specific quirks (inline documentation)
//!
//! ## Helius LaserStream
//! - Auth: HTTP header `x-api-key: <token>`. The `yellowstone-grpc-client` builder
//!   supports custom headers via `GeyserGrpcBuilder::x_token(token)`.
//! - Rate limits: 429 / `RESOURCE_EXHAUSTED` gRPC status on overload.
//!   `rate_limit_base_ms` in `ReconnectPolicy` controls the extended backoff.
//! - Endpoint: append `/` (no path) — the builder connects to the root service.
//!   Do NOT append `/v1/` or any REST path.
//!
//! ## Triton Dragon's Mouth
//! - Auth: gRPC metadata key `x-token: <token>`. Same `GeyserGrpcBuilder::x_token`.
//! - Endpoint format: `https://<region>.rpc.triton.one:443` — TLS required.
//!   Set `tls = true` in the config (or omit; TLS is on by default when port=443).
//! - No known rate-limit behavior distinct from generic `RESOURCE_EXHAUSTED`.
//!
//! ## Self-hosted validator
//! - No auth token needed (omit `auth_token` or leave `None`).
//! - Plaintext gRPC supported: `http://localhost:10000`. The builder auto-detects
//!   plaintext vs TLS from the URL scheme.

use serde::{Deserialize, Serialize};
use url::Url;

/// Configuration for the Solana Yellowstone gRPC adapter.
///
/// Load via TOML (see `config/adapters.toml.example`), then construct
/// `SolanaAdapter::new(config)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaAdapterConfig {
    /// gRPC endpoint URL.
    ///
    /// - Helius: `https://mainnet.helius-rpc.com` (or `https://<custom>.helius-rpc.com`)
    /// - Triton: `https://ams1.rpc.triton.one:443` (or other region)
    /// - Self-hosted: `http://localhost:10000`
    pub endpoint: Url,

    /// Authentication token passed via `x-token` gRPC metadata / `x-api-key` HTTP header.
    ///
    /// - Helius: your Helius API key.
    /// - Triton: your Triton access token.
    /// - Self-hosted: omit (`None`).
    ///
    /// NEVER commit real tokens. This field is `None` in the `.example` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// Solana commitment level for the live subscription stream.
    ///
    /// - `Confirmed` — hot path; fast, but events can be rolled back if the slot is
    ///   skipped. The adapter emits `ReorgMarker` when a confirmed slot goes dead.
    /// - `Finalized` — immutable; ~32 slots behind confirmed. Use for detector-critical
    ///   inputs that cannot tolerate reorg noise (e.g., rug-pull LP drain confirmation).
    ///
    /// Per CLAUDE.md §Multi-Chain Rules / Solana: use `Confirmed` for hot path,
    /// `Finalized` for immutable records.
    #[serde(default = "default_commitment")]
    pub commitment: CommitmentConfig,

    /// Reconnect and retry policy.
    #[serde(default)]
    pub reconnect: ReconnectPolicy,

    /// Subscription filter — which programs / accounts to stream.
    #[serde(default)]
    pub filters: SubscribeFiltersConfig,

    /// Solana JSON-RPC endpoint for backfill (`getBlock` calls).
    ///
    /// Separate from the gRPC endpoint because:
    /// - Archive backfill needs a high-data-retention RPC (not all gRPC providers
    ///   retain full block history).
    /// - Some self-hosted setups run gRPC on a different port than JSON-RPC.
    ///
    /// If omitted, the adapter falls back to the gRPC provider's JSON-RPC endpoint
    /// (derived by replacing the port with 8899 for self-hosted, or using the
    /// provider's documented JSON-RPC URL for Helius/Triton).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_endpoint: Option<Url>,

    /// Path to the file-backed checkpoint file.
    ///
    /// Default: `./checkpoints/solana.json`. The file is created if it does not exist.
    /// Set to a volume-mounted path in Docker deployments.
    #[serde(default = "default_checkpoint_path")]
    pub checkpoint_path: String,
}

fn default_commitment() -> CommitmentConfig {
    CommitmentConfig::Confirmed
}

fn default_checkpoint_path() -> String {
    "./checkpoints/solana.json".into()
}

/// Solana commitment levels exposed in adapter config.
///
/// Maps to Yellowstone `CommitmentLevel` proto enum in `subscribe.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitmentConfig {
    Processed,
    Confirmed,
    Finalized,
}

impl CommitmentConfig {
    /// Convert to the Yellowstone proto `CommitmentLevel`.
    pub fn to_proto(self) -> yellowstone_grpc_proto::geyser::CommitmentLevel {
        use yellowstone_grpc_proto::geyser::CommitmentLevel;
        match self {
            CommitmentConfig::Processed => CommitmentLevel::Processed,
            CommitmentConfig::Confirmed => CommitmentLevel::Confirmed,
            CommitmentConfig::Finalized => CommitmentLevel::Finalized,
        }
    }
}

/// Reconnect and retry policy for the Yellowstone gRPC stream.
///
/// Uses exponential backoff with jitter. The formula is:
/// `delay = min(base_delay_ms * 2^attempt, max_delay_ms) + jitter`
/// where `jitter ∈ [0, base_delay_ms]` (full jitter, drawn from the tokio-retry
/// `ExponentialBackoff` strategy with `jitter = true`).
///
/// See `solana/reconnect.rs` for the implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    /// Base delay for the first reconnect attempt in milliseconds.
    /// Default: 500 ms.
    #[serde(default = "default_base_delay_ms")]
    pub base_delay_ms: u64,

    /// Maximum delay cap in milliseconds.
    /// Default: 30_000 ms (30 s).
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,

    /// Maximum number of consecutive reconnect attempts before giving up.
    /// Default: 10. Set to 0 for unlimited retries.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Extended base delay applied when a rate-limit response is received.
    /// Default: 5_000 ms (5 s).
    ///
    /// Rate-limit backoff formula: `min(rate_limit_base_ms * 2^attempt, max_delay_ms)`.
    #[serde(default = "default_rate_limit_base_ms")]
    pub rate_limit_base_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
            max_attempts: default_max_attempts(),
            rate_limit_base_ms: default_rate_limit_base_ms(),
        }
    }
}

fn default_base_delay_ms() -> u64 { 500 }
fn default_max_delay_ms() -> u64 { 30_000 }
fn default_max_attempts() -> u32 { 10 }
fn default_rate_limit_base_ms() -> u64 { 5_000 }

/// Which programs / account owners to include in the subscription.
///
/// Defaults to the full SPL Token + Token-2022 + major DEX program set.
/// Narrow this list to reduce stream volume (and provider costs) in production.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeFiltersConfig {
    /// Solana program IDs (Base58) to include in the transaction filter.
    #[serde(default = "default_program_ids")]
    pub program_ids: Vec<String>,

    /// Account owner program IDs for the account update filter.
    #[serde(default = "default_account_owners")]
    pub account_owners: Vec<String>,

    /// Whether to subscribe to slot metadata updates for reorg detection.
    /// Default: `true`.
    #[serde(default = "default_true")]
    pub include_slot_updates: bool,
}

impl Default for SubscribeFiltersConfig {
    fn default() -> Self {
        Self {
            program_ids: default_program_ids(),
            account_owners: default_account_owners(),
            include_slot_updates: true,
        }
    }
}

fn default_true() -> bool { true }

fn default_program_ids() -> Vec<String> {
    vec![
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(), // SPL Token
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(), // Token-2022
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8".into(), // Raydium AMM v4
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK".into(), // Raydium CLMM
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".into(),  // Orca Whirlpool
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".into(), // Meteora DLMM
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into(),  // PumpFun
    ]
}

fn default_account_owners() -> Vec<String> {
    vec![
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_config_serde_roundtrip() {
        let v = CommitmentConfig::Confirmed;
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#""confirmed""#);
        let back: CommitmentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn reconnect_policy_defaults() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.base_delay_ms, 500);
        assert_eq!(p.max_delay_ms, 30_000);
        assert_eq!(p.max_attempts, 10);
        assert_eq!(p.rate_limit_base_ms, 5_000);
    }

    #[test]
    fn subscribe_filters_default_has_spl_token() {
        let f = SubscribeFiltersConfig::default();
        assert!(
            f.program_ids.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
            "SPL Token Program must be in default filter"
        );
        assert!(
            f.program_ids.contains(&"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".to_string()),
            "Token-2022 must be in default filter"
        );
    }

    #[test]
    fn commitment_to_proto_confirmed() {
        use yellowstone_grpc_proto::geyser::CommitmentLevel;
        let proto = CommitmentConfig::Confirmed.to_proto();
        assert_eq!(proto, CommitmentLevel::Confirmed);
    }
}
