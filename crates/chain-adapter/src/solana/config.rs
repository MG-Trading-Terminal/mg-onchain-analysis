//! Configuration types for the Solana JSON-RPC + WebSocket adapter.
//!
//! # Standard JSON-RPC + WebSocket (ADR 0007)
//!
//! Sprint 26 (T26-2) replaced the Yellowstone gRPC path with standard Solana
//! JSON-RPC 2.0 over WebSocket.  The config now mirrors `EthereumAdapterConfig`:
//! two URL fields (one HTTP for one-shot RPC calls, one WS for subscriptions) plus
//! commitment and reconnect settings.
//!
//! | Field | Purpose | Default |
//! |-------|---------|---------|
//! | `http_url` | HTTP JSON-RPC endpoint for `getBlock`, `getTransaction`, `getSlot`, etc. | `http://127.0.0.1:8899` |
//! | `ws_url`   | WebSocket endpoint for `programSubscribe`, `logsSubscribe`, etc.          | `ws://127.0.0.1:8900`  |
//! | `commitment` | Solana commitment level for subscriptions and queries | `confirmed` |
//! | `reconnect` | Exponential-backoff reconnect policy | see `ReconnectPolicy::default` |
//! | `filters`   | Which programs / account owners to subscribe to | Solana token + DEX set |
//!
//! Standard Agave RPC-only node exposes JSON-RPC on port 8899 and WebSocket on
//! port 8900 by default.  Self-hosted operators can override via TOML config.
//!
//! Private RPC endpoints may require an `Authorization: Bearer <token>` HTTP header.
//! Set `auth_token` and the adapter will attach it on every HTTP request.
//! The same token is used as a query-param on the WS URL if the node requires it
//! (non-standard; most nodes do not).

use serde::{Deserialize, Serialize};
use url::Url;

/// Configuration for the Solana JSON-RPC + WebSocket adapter.
///
/// Load via TOML (see `config/adapters.toml.example`), then construct
/// `SolanaAdapter::new(config, checkpoint_store)`.
///
/// # Example (TOML)
///
/// ```toml
/// [solana]
/// http_url = "http://127.0.0.1:8899"
/// ws_url   = "ws://127.0.0.1:8900"
/// commitment = "confirmed"
/// checkpoint_path = "./checkpoints/solana.json"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaAdapterConfig {
    /// HTTP JSON-RPC endpoint.
    ///
    /// Used for one-shot requests: `getBlock`, `getTransaction`, `getSlot`,
    /// `getSignaturesForAddress`, `getAccountInfo`, `getHealth`, etc.
    ///
    /// Default: `http://127.0.0.1:8899` (standard Agave RPC HTTP port).
    #[serde(default = "default_http_url")]
    pub http_url: Url,

    /// WebSocket endpoint.
    ///
    /// Used for push subscriptions: `programSubscribe`, `logsSubscribe`,
    /// `accountSubscribe`, `signatureSubscribe`.
    ///
    /// Default: `ws://127.0.0.1:8900` (standard Agave RPC WebSocket port).
    #[serde(default = "default_ws_url")]
    pub ws_url: Url,

    /// Optional Bearer token for private RPC endpoints.
    ///
    /// When set, the adapter attaches `Authorization: Bearer <token>` to every
    /// HTTP request. Most self-hosted nodes do not require auth.
    ///
    /// NEVER commit real tokens. This field is `None` in the `.example` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// Solana commitment level for live subscriptions and block queries.
    ///
    /// - `Confirmed` — hot path; fast, but events can be rolled back if the slot is
    ///   skipped. The adapter emits `ReorgMarker` when a confirmed slot goes dead.
    /// - `Finalized` — immutable; ~32 slots behind confirmed. Use for detector-critical
    ///   inputs that cannot tolerate reorg noise.
    ///
    /// Per CLAUDE.md §Multi-Chain Rules / Solana: use `Confirmed` for hot path,
    /// `Finalized` for immutable records.
    #[serde(default = "default_commitment")]
    pub commitment: CommitmentConfig,

    /// Reconnect and retry policy.
    #[serde(default)]
    pub reconnect: ReconnectPolicy,

    /// Subscription filter — which programs / account owners to subscribe to.
    #[serde(default)]
    pub filters: SubscribeFiltersConfig,

    /// Path to the file-backed checkpoint file.
    ///
    /// Default: `./checkpoints/solana.json`. The file is created if it does not exist.
    /// Set to a volume-mounted path in Docker deployments.
    #[serde(default = "default_checkpoint_path")]
    pub checkpoint_path: String,
}

fn default_http_url() -> Url {
    Url::parse("http://127.0.0.1:8899").expect("default http url is valid")
}

fn default_ws_url() -> Url {
    Url::parse("ws://127.0.0.1:8900").expect("default ws url is valid")
}

fn default_commitment() -> CommitmentConfig {
    CommitmentConfig::Confirmed
}

fn default_checkpoint_path() -> String {
    "./checkpoints/solana.json".into()
}

/// Solana commitment levels exposed in adapter config.
///
/// Maps to the Solana JSON-RPC `commitment` parameter string in subscribe and query calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitmentConfig {
    Processed,
    Confirmed,
    Finalized,
}

impl CommitmentConfig {
    /// Convert to the Solana JSON-RPC string representation.
    ///
    /// Used as the `"commitment"` value in JSON-RPC request params:
    /// `{"encoding": "base64", "commitment": "<level>"}`.
    pub fn as_str(self) -> &'static str {
        match self {
            CommitmentConfig::Processed => "processed",
            CommitmentConfig::Confirmed => "confirmed",
            CommitmentConfig::Finalized => "finalized",
        }
    }
}

/// Reconnect and retry policy for the JSON-RPC WebSocket stream.
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
/// Narrow this list to reduce stream volume in production.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeFiltersConfig {
    /// Solana program IDs (Base58) to include in the `logsSubscribe` mentions filter.
    ///
    /// For `programSubscribe` calls the adapter iterates this list and opens one
    /// subscription per program ID.
    #[serde(default = "default_program_ids")]
    pub program_ids: Vec<String>,

    /// Account owner program IDs for the `programSubscribe` account filter.
    #[serde(default = "default_account_owners")]
    pub account_owners: Vec<String>,

    /// Whether to track slot confirmations for reorg detection.
    ///
    /// When `true` the adapter polls `getSlot({commitment: "finalized"})` periodically
    /// and emits `ReorgMarker` events when the confirmed tip diverges from the
    /// finalized tip by more than the configured reorg window.
    ///
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
    fn commitment_as_str_confirmed() {
        assert_eq!(CommitmentConfig::Confirmed.as_str(), "confirmed");
    }

    #[test]
    fn commitment_as_str_finalized() {
        assert_eq!(CommitmentConfig::Finalized.as_str(), "finalized");
    }

    #[test]
    fn commitment_as_str_processed() {
        assert_eq!(CommitmentConfig::Processed.as_str(), "processed");
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
    fn solana_adapter_config_default_urls() {
        let config = SolanaAdapterConfig {
            http_url: default_http_url(),
            ws_url: default_ws_url(),
            auth_token: None,
            commitment: CommitmentConfig::Confirmed,
            reconnect: ReconnectPolicy::default(),
            filters: SubscribeFiltersConfig::default(),
            checkpoint_path: "/tmp/test.json".into(),
        };
        assert_eq!(config.http_url.as_str(), "http://127.0.0.1:8899/");
        assert_eq!(config.ws_url.as_str(), "ws://127.0.0.1:8900/");
    }

    #[test]
    fn solana_adapter_config_serde_minimal() {
        // Minimal JSON round-trip: only override urls, rest use serde defaults.
        let json_str = r#"{
            "http_url": "http://rpc.example.com:8899",
            "ws_url":   "ws://rpc.example.com:8900"
        }"#;
        let config: SolanaAdapterConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(config.http_url.as_str(), "http://rpc.example.com:8899/");
        assert_eq!(config.ws_url.as_str(), "ws://rpc.example.com:8900/");
        assert_eq!(config.commitment, CommitmentConfig::Confirmed);
        assert!(config.auth_token.is_none());
    }
}
