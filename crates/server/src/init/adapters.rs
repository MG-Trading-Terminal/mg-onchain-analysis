//! Chain adapter constructors.
//!
//! # D-E: Per-chain enable flags
//!
//! `build_solana_adapter` constructs the Solana JSON-RPC 2.0 + WebSocket adapter.
//! `build_evm_adapters` constructs one `EthereumAdapter` per enabled EVM chain
//! (Ethereum, BSC, Base, Arbitrum, Polygon) using the per-chain WS endpoint.
//! `build_ethereum_adapter` is the single-chain variant (kept for backwards compat).
//!
//! Callers check `config.chains.<chain>.enabled` before calling these; the
//! functions themselves do not enforce enablement — that is `main.rs`'s job.
//!
//! # ADR 0003: Self-sovereign defaults
//!
//! Default endpoints are localhost — no 3rd-party SaaS in production hot path.
//!
//! # Gotcha #39: Indexer::new 9-param signature unchanged
//!
//! These constructors build `ChainAdapter` implementations only; Indexer wiring
//! is in `init::coordinator`.
//!
//! # SPEC-NOTE: Multi-chain EVM
//!
//! EVM chains (Ethereum/BSC/Base/Arbitrum/Polygon) all use the same Permit2 contract
//! `0x000000000022D473030F116dDEE9F6B43aC78BA3` (deterministic CREATE2 deployment).
//! PancakeSwap V2/V3 on BSC are UniV2/V3 forks — existing decoders work without changes.

use anyhow::Context as _;
use tracing::info;

use mg_onchain_chain_adapter::ethereum::{
    adapter::EthereumAdapter,
    rpc::WsRpcClient,
};
use mg_onchain_chain_adapter::solana::{
    SolanaAdapter,
    checkpoint::FileCheckpointStore,
    config::{CommitmentConfig, ReconnectPolicy, SolanaAdapterConfig, SubscribeFiltersConfig},
};
use mg_onchain_common::chain::Chain;
use url::Url;

use crate::config::{ChainsConfig, EvmChainConfig, SolanaChainConfig};

// ---------------------------------------------------------------------------
// Solana adapter
// ---------------------------------------------------------------------------

/// Construct a `SolanaAdapter` from the given chain config.
///
/// Uses a `FileCheckpointStore` backed by `config.checkpoint_path`.
/// HTTP and WS endpoints default to localhost:8899/8900 (ADR 0003).
///
/// # D-E
///
/// Only called when `config.chains.solana.enabled = true` (D-E).
pub fn build_solana_adapter(config: &SolanaChainConfig) -> anyhow::Result<SolanaAdapter> {
    let http_url = Url::parse(&config.http_url)
        .with_context(|| format!("invalid Solana http_url: {}", config.http_url))?;
    let ws_url = Url::parse(&config.ws_url)
        .with_context(|| format!("invalid Solana ws_url: {}", config.ws_url))?;

    let adapter_config = SolanaAdapterConfig {
        http_url,
        ws_url,
        auth_token: None, // ADR 0003: self-hosted — no auth token required
        commitment: CommitmentConfig::Confirmed, // CLAUDE.md: Confirmed for hot path
        reconnect: ReconnectPolicy::default(),
        filters: SubscribeFiltersConfig::default(),
        checkpoint_path: config.checkpoint_path.clone(),
    };

    let checkpoint_store =
        FileCheckpointStore::new(&config.checkpoint_path);

    info!(
        http_url = %config.http_url,
        ws_url = %config.ws_url,
        checkpoint = %config.checkpoint_path,
        "building Solana adapter"
    );

    Ok(SolanaAdapter::new(adapter_config, checkpoint_store))
}

// ---------------------------------------------------------------------------
// EVM adapters (multi-chain)
// ---------------------------------------------------------------------------

/// Construct one `EthereumAdapter` per enabled EVM chain.
///
/// Loops over `config.enabled_evm_chains()` and connects a `WsRpcClient` per chain.
/// Returns a `Vec<(Chain, EthereumAdapter)>` in the order chains are defined
/// in `ChainsConfig` (Ethereum → BSC → Base → Arbitrum → Polygon).
///
/// # D-E
///
/// Only enabled chains produce an adapter slot. Disabled chains are skipped.
///
/// # Errors
///
/// Returns the first connection error encountered. All chains must be
/// reachable at startup — a failed connection for any enabled chain is fatal.
pub async fn build_evm_adapters(
    config: &ChainsConfig,
) -> anyhow::Result<Vec<(Chain, EthereumAdapter)>> {
    let mut adapters = Vec::new();
    for (chain, chain_cfg) in config.enabled_evm_chains() {
        let adapter = build_evm_adapter_for_chain(chain, chain_cfg).await?;
        adapters.push((chain, adapter));
    }
    Ok(adapters)
}

/// Construct an `EthereumAdapter` for a single EVM chain.
///
/// Internal helper used by both `build_evm_adapters` (multi-chain) and
/// `build_ethereum_adapter` (single-chain legacy path).
async fn build_evm_adapter_for_chain(
    chain: Chain,
    config: &EvmChainConfig,
) -> anyhow::Result<EthereumAdapter> {
    info!(
        chain = %chain,
        ws_url = %config.ws_url,
        reorg_depth = config.reorg_depth,
        checkpoint = %config.checkpoint_path,
        "building EVM adapter (WsRpcClient)"
    );

    let rpc = WsRpcClient::connect(&config.ws_url)
        .await
        .with_context(|| format!(
            "failed to connect WsRpcClient to {} for chain {chain}",
            config.ws_url
        ))?;

    let checkpoint_store =
        mg_onchain_chain_adapter::solana::checkpoint::FileCheckpointStore::new(
            &config.checkpoint_path,
        );

    Ok(EthereumAdapter::new(chain, rpc, checkpoint_store)
        .with_reorg_depth(config.reorg_depth))
}

/// Construct a single `EthereumAdapter` for Ethereum mainnet.
///
/// Kept for backwards compat with callers that pass only the Ethereum config.
/// Prefer `build_evm_adapters` for multi-chain production use.
pub async fn build_ethereum_adapter(
    config: &EvmChainConfig,
) -> anyhow::Result<EthereumAdapter> {
    build_evm_adapter_for_chain(Chain::Ethereum, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChainsConfig, EvmChainConfig, SolanaChainConfig};

    #[test]
    fn solana_adapter_builds_from_default_config() {
        let config = SolanaChainConfig::default();
        // Construction must succeed for the default self-hosted endpoint.
        // No network call is made at construction time.
        let result = build_solana_adapter(&config);
        assert!(result.is_ok(), "Solana adapter must build from defaults");
    }

    #[test]
    fn solana_adapter_rejects_invalid_http_url() {
        let config = SolanaChainConfig {
            http_url: "not_a_valid_url !!!".to_string(),
            ..Default::default()
        };
        let result = build_solana_adapter(&config);
        assert!(result.is_err(), "invalid http_url must fail");
    }

    #[test]
    fn solana_adapter_rejects_invalid_ws_url() {
        let config = SolanaChainConfig {
            ws_url: "not_a_valid_url !!!".to_string(),
            ..Default::default()
        };
        let result = build_solana_adapter(&config);
        assert!(result.is_err(), "invalid ws_url must fail");
    }

    #[test]
    fn evm_config_defaults_are_correct() {
        let config = EvmChainConfig::default();
        // D-E: EVM chains disabled by default
        assert!(!config.enabled, "EVM chain must be disabled by default (D-E)");
        // Reorg depth per ADR 0004
        assert_eq!(config.reorg_depth, 12, "EVM reorg_depth must default to 12 per ADR 0004");
        // Self-hosted endpoint per ADR 0003
        assert!(
            config.ws_url.starts_with("ws://"),
            "EVM ws_url must use ws:// scheme (self-hosted per ADR 0003)"
        );
    }

    #[test]
    fn build_evm_adapters_returns_empty_for_all_disabled() {
        // Verify that when no EVM chains are enabled, the function returns an empty vec.
        // This is a synchronous check of enabled_evm_chains() — no network call needed.
        let config = ChainsConfig::default();
        assert!(
            config.enabled_evm_chains().is_empty(),
            "no EVM adapters when all chains disabled"
        );
    }
}
