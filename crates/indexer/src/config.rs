//! Indexer configuration types.
//!
//! Load from `config/indexer.toml` via `serde` / `toml`:
//!
//! ```toml
//! [batch]
//! size = 500
//! timeout_ms = 2000
//! max_in_flight = 4
//! ```
//!
//! All numeric thresholds have documented defaults — see field-level doc comments.
//! No threshold is hardcoded; changing `config/indexer.toml` is sufficient.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use mg_onchain_chain_adapter::solana::config::SolanaAdapterConfig;
use mg_onchain_storage::StorageConfig;

// ---------------------------------------------------------------------------
// IndexerConfig
// ---------------------------------------------------------------------------

/// Top-level indexer configuration.
///
/// Composed of per-subsystem config structs so each concern can be evolved
/// independently and loaded from separate TOML sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerConfig {
    /// Which chain adapter to use.
    pub adapter: AdapterConfig,

    /// Postgres storage parameters.
    pub storage: StorageConfig,

    /// Batching parameters for the event→storage pipeline.
    #[serde(default)]
    pub batch: BatchConfig,

    /// Unique identifier for this adapter instance, used as the checkpoint key.
    ///
    /// Default: `"solana"`. Multiple indexer instances targeting different
    /// program-filter subsets should use distinct IDs to avoid checkpoint
    /// collisions.
    #[serde(default = "default_adapter_id")]
    pub adapter_id: String,
}

fn default_adapter_id() -> String {
    "solana".into()
}

// ---------------------------------------------------------------------------
// AdapterConfig — which chain adapter to instantiate
// ---------------------------------------------------------------------------

/// Chain adapter variant and its configuration.
///
/// `#[non_exhaustive]` so Phase 4 EVM variants can be added without breaking
/// existing config-parsing code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)] // SolanaAdapterConfig is large by design (gRPC config)
pub enum AdapterConfig {
    /// Solana Yellowstone gRPC adapter.
    Solana(SolanaAdapterConfig),
    /// Ethereum WebSocket JSON-RPC adapter (ADR 0004; Sprint 17 S17-2).
    Ethereum(EthereumAdapterConfig),
}

// ---------------------------------------------------------------------------
// EthereumAdapterConfig
// ---------------------------------------------------------------------------

/// Configuration for the Ethereum WebSocket JSON-RPC adapter.
///
/// Mirrors the fields available in `EthereumAdapter::new` and `WsRpcClient::connect`.
/// All thresholds mirror or extend the ADR 0005 §Step 1 spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthereumAdapterConfig {
    /// WebSocket URL for the self-hosted Reth node.
    ///
    /// Example: `ws://127.0.0.1:8546`.
    /// Populated from `config/adapters.toml` `[ethereum]` section.
    pub rpc_url: String,

    /// Confirmation depth (number of blocks) before events are emitted.
    ///
    /// Default: 12 — matches `EthereumAdapter::with_reorg_depth` default and
    /// CLAUDE.md §Ethereum/EVM ("12 blocks for finality reliability").
    #[serde(default = "default_eth_reorg_depth")]
    pub reorg_depth: u64,

    /// Optional path for a `FileCheckpointStore`.
    ///
    /// `None` uses an in-memory checkpoint store (test / ephemeral use).
    pub checkpoint_path: Option<PathBuf>,
}

fn default_eth_reorg_depth() -> u64 {
    12
}

// ---------------------------------------------------------------------------
// BatchConfig
// ---------------------------------------------------------------------------

/// Controls how events are batched before writing to Postgres.
///
/// The indexer flushes a buffer when EITHER `size` is reached OR `timeout_ms`
/// elapses since the first event entered the buffer — whichever comes first.
/// This bounds both write latency (timeout) and memory (size).
///
/// All defaults are tuned for MVP event rates (hundreds/minute after filtering).
/// Production operators with higher throughput should raise `size` and lower
/// `timeout_ms` to improve write efficiency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchConfig {
    /// Maximum number of events per table before a flush is triggered.
    ///
    /// Default: 500.
    /// Rationale: at MVP rates (~100 events/min) a size-500 batch fills in ~5 min.
    /// The timeout trigger fires first in practice; size is a safety ceiling.
    #[serde(default = "default_batch_size")]
    pub size: usize,

    /// Maximum time (ms) between the first event entering a buffer and a flush.
    ///
    /// Default: 2000 ms.
    /// Rationale: 2 s lag between on-chain event and DB write is acceptable for
    /// MVP detectors that operate over multi-minute windows. If real-time alerting
    /// is needed, lower to 200–500 ms.
    #[serde(default = "default_batch_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum number of un-committed batches that can queue waiting for Postgres.
    ///
    /// Default: 4.
    /// Rationale: each batch is ~500 rows × ~200 bytes ≈ 100 KB in RAM.
    /// 4 batches = ~400 KB ceiling — trivial. The bounded channel provides
    /// backpressure: when Postgres falls behind, the channel fills and the
    /// subscribe loop blocks instead of buffering unboundedly.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            size: default_batch_size(),
            timeout_ms: default_batch_timeout_ms(),
            max_in_flight: default_max_in_flight(),
        }
    }
}

fn default_batch_size() -> usize {
    500
}
fn default_batch_timeout_ms() -> u64 {
    2000
}
fn default_max_in_flight() -> usize {
    4
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_config_defaults() {
        let cfg = BatchConfig::default();
        assert_eq!(cfg.size, 500);
        assert_eq!(cfg.timeout_ms, 2000);
        assert_eq!(cfg.max_in_flight, 4);
    }

    #[test]
    fn batch_config_toml_override() {
        let toml_str = r#"
            size = 100
            timeout_ms = 500
            max_in_flight = 2
        "#;
        let cfg: BatchConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.size, 100);
        assert_eq!(cfg.timeout_ms, 500);
        assert_eq!(cfg.max_in_flight, 2);
    }

    #[test]
    fn batch_config_partial_toml_uses_defaults() {
        let toml_str = r#"size = 250"#;
        let cfg: BatchConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.size, 250);
        assert_eq!(cfg.timeout_ms, 2000); // default
        assert_eq!(cfg.max_in_flight, 4); // default
    }
}
