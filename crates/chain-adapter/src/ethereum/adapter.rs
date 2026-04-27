//! `EthereumAdapter` — `ChainAdapter` trait implementation for Ethereum.
//!
//! # Sprint 15 scope (skeleton)
//!
//! This sprint delivers the compile-green skeleton. `ChainAdapter` methods that
//! require full RPC wiring are stubbed with TODO comments and safe defaults (empty
//! streams, `Ok(None)`, etc.). Full implementation is Sprint 16.
//!
//! # Design
//!
//! `EthereumAdapter` holds:
//! - `rpc: Arc<dyn EthereumRpc + Send + Sync>` — injectable RPC backend (real or mock)
//! - `reorg_buffer_depth: u64` — default 12 per ADR 0004; buffer is `depth + 4` margin
//! - `checkpoint_store: Arc<dyn CheckpointStore>` — reuses the same trait from Solana adapter
//!
//! The subscribe stream wraps `EthereumRpc::subscribe_new_heads` and applies the
//! depth-12 confirmation window before emitting events to the indexer.
//!
//! # Reorg handling
//!
//! The `ReorgBuffer` (reorg.rs) tracks the last 16 block headers. On each new head,
//! it checks whether the parent_hash matches the current tip. Discrepancies trigger
//! `Event::ReorgMarker { slot: block_number }` emissions.
//!
//! # Finality
//!
//! Two-tier per ADR 0004 §Finality:
//! - **Safe (hot path):** depth 12 blocks (~2.4 min). Events emitted to indexer.
//! - **Finalized:** `finalized` block tag (~12.8 min). Used for checkpoint_save.
//!
//! # ExEx path (Sprint 16+)
//!
//! The current adapter uses `eth_subscribe("newHeads")` + `eth_getLogs` polling.
//! Sprint 16 will add an ExEx path behind a feature flag that replaces the subscribe
//! stream with `ChainCommitted`/`ChainReverted` notifications (eliminating the
//! ReorgBuffer hash-tracking entirely for the ExEx path).

use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tracing::{info, warn};

use mg_onchain_common::chain::{BlockRef, Chain};

use crate::{
    error::AdapterError,
    ethereum::rpc::EthereumRpc,
    solana::checkpoint::CheckpointStore,
    ChainAdapter, Checkpoint, Event, SubscribeFilter,
};

// ---------------------------------------------------------------------------
// EthereumAdapter
// ---------------------------------------------------------------------------

/// EVM chain adapter backed by a self-hosted Reth-equivalent node via WebSocket JSON-RPC.
///
/// Supports all EVM-compatible chains: Ethereum, BSC, Base, Arbitrum, Polygon.
///
/// Create via [`EthereumAdapter::new`], then use [`ChainAdapter::subscribe`] or
/// [`ChainAdapter::backfill`].
///
/// Thread-safe: `Arc` over the inner RPC client and checkpoint store.
pub struct EthereumAdapter {
    /// Injectable RPC backend — `WsRpcClient` in production, `MockEthereumRpc` in tests.
    ///
    /// `allow(dead_code)`: used by Sprint-16 subscribe/backfill/tip/health_check impls.
    #[allow(dead_code)]
    rpc: Arc<dyn EthereumRpc + Send + Sync>,
    /// The EVM chain identity tag (set at construction; validated to be an EVM chain).
    chain: Chain,
    /// Hot-path confirmation depth. Default: 12 (CLAUDE.md §Ethereum/EVM).
    ///
    /// Events are buffered until the producing block reaches this depth.
    /// Kept as a field (not a constant) so tests can lower it and per-chain
    /// operators can override (e.g. Polygon may warrant higher depth).
    reorg_depth: u64,
    /// Checkpoint persistence backend.
    checkpoint_store: Arc<dyn CheckpointStore>,
}

impl EthereumAdapter {
    /// Create a new `EthereumAdapter` for the given EVM chain.
    ///
    /// # Arguments
    ///
    /// - `chain` — Must be an EVM-compatible chain (`Ethereum`, `Bsc`, `Base`,
    ///   `Arbitrum`, or `Polygon`). Panics in debug mode if a non-EVM chain is passed;
    ///   in release mode the value is stored as-is (adapter will produce no events
    ///   for non-EVM chains since no EVM RPC server can answer).
    /// - `rpc` — RPC backend (inject `WsRpcClient::new(url)` in production,
    ///   `MockEthereumRpc::new()` in tests).
    /// - `checkpoint_store` — checkpoint persistence (inject `FileCheckpointStore` or
    ///   `InMemoryCheckpointStore`).
    ///
    /// The `reorg_depth` defaults to 12. Use `with_reorg_depth` to override for tests.
    pub fn new(
        chain: Chain,
        rpc: impl EthereumRpc + 'static,
        checkpoint_store: impl CheckpointStore + 'static,
    ) -> Self {
        debug_assert!(
            chain.is_evm(),
            "EthereumAdapter::new called with non-EVM chain {chain}; \
             only Ethereum/Bsc/Base/Arbitrum/Polygon are supported"
        );
        Self {
            rpc: Arc::new(rpc),
            chain,
            reorg_depth: 12,
            checkpoint_store: Arc::new(checkpoint_store),
        }
    }

    /// Override the reorg depth (useful for tests; default is 12).
    pub fn with_reorg_depth(mut self, depth: u64) -> Self {
        self.reorg_depth = depth;
        self
    }

    /// Return the configured reorg depth.
    pub fn reorg_depth(&self) -> u64 {
        self.reorg_depth
    }

    /// Return the chain tag (the EVM chain this adapter was constructed for).
    pub fn chain(&self) -> Chain {
        self.chain
    }
}

// ---------------------------------------------------------------------------
// ChainAdapter implementation
// ---------------------------------------------------------------------------

impl ChainAdapter for EthereumAdapter {
    fn subscribe(
        &self,
        _filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        // TODO(sprint-16): implement eth_subscribe("newHeads") + eth_getLogs polling loop.
        // The stream should:
        // 1. Call rpc.subscribe_new_heads() to get the live head stream.
        // 2. For each new head, push to ReorgBuffer. If reorg detected, emit ReorgMarker events.
        // 3. Once the confirmed block is at depth >= reorg_depth, call rpc.get_logs(filter)
        //    and emit Transfer / Swap / PoolEvent from decoded logs.
        // 4. On stream drop or error, apply exponential backoff and reconnect.
        //
        // Sprint 15: return an empty stream so the adapter compiles and tests run.
        info!(chain = %self.chain, reorg_depth = self.reorg_depth, "subscribe: stub (Sprint 16)");
        Box::pin(futures::stream::empty())
    }

    fn backfill(
        &self,
        range: RangeInclusive<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        // TODO(sprint-16): implement eth_getLogs backfill in chunks of batch_size_blocks.
        // Constraints:
        // - chunk size <= 1000 blocks (Reth default; configurable via EthereumAdapterConfig)
        // - must not race with subscribe on the same block range
        // - emit events in block order (ascending block_number, ascending log_index)
        info!(
            chain = %self.chain,
            start = range.start(),
            end = range.end(),
            "backfill: stub (Sprint 16)"
        );
        Box::pin(futures::stream::empty())
    }

    async fn checkpoint_save(&self, checkpoint: &Checkpoint) -> Result<(), AdapterError> {
        self.checkpoint_store.save(checkpoint).map_err(|e| {
            warn!(
                block = checkpoint.slot,
                error = %e,
                "ethereum checkpoint save failed"
            );
            e
        })
    }

    async fn checkpoint_load(&self) -> Result<Option<Checkpoint>, AdapterError> {
        self.checkpoint_store.load()
    }

    async fn health_check(&self) -> Result<(), AdapterError> {
        // TODO(sprint-16): call rpc.get_latest_block_number() and verify it is non-zero.
        // Sprint 15: return Ok(()) unconditionally (no network required).
        Ok(())
    }

    async fn tip(&self) -> Result<BlockRef, AdapterError> {
        // TODO(sprint-16): call rpc.get_latest_block_number() and wrap in BlockRef.
        // Sprint 15: return a sentinel block 0 so the adapter compiles.
        Ok(BlockRef::new(self.chain, 0))
    }

    /// Override: return the chain-aware Ethereum subscribe filter.
    ///
    /// ADR 0005 Decision 5: the Ethereum adapter returns a chain-specific filter so
    /// `Indexer::run` does not pass Solana program IDs to the EVM subscription.
    ///
    /// Uses `evm_default_for_chain(self.chain)` which extends the universal 13-topic
    /// base with protocol-specific topic0 hashes:
    /// - BSC: + PancakeSwap V3 Swap topic0
    /// - Base: + Aerodrome Swap topic0
    /// - Ethereum / Arbitrum / Polygon: universal base only
    ///
    /// EVM log filtering is done per-block via `eth_getLogs` using topic0 constants
    /// from `decoder.rs`.
    fn default_filter(&self) -> SubscribeFilter {
        SubscribeFilter::evm_default_for_chain(self.chain)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ethereum::rpc::MockEthereumRpc;
    use crate::solana::checkpoint::InMemoryCheckpointStore;

    fn make_adapter() -> EthereumAdapter {
        EthereumAdapter::new(Chain::Ethereum, MockEthereumRpc::new(), InMemoryCheckpointStore::new())
    }

    #[test]
    fn adapter_chain_is_ethereum() {
        let adapter = make_adapter();
        assert_eq!(adapter.chain(), Chain::Ethereum);
    }

    #[test]
    fn adapter_chain_bsc() {
        let adapter = EthereumAdapter::new(Chain::Bsc, MockEthereumRpc::new(), InMemoryCheckpointStore::new());
        assert_eq!(adapter.chain(), Chain::Bsc);
    }

    #[test]
    fn adapter_chain_base() {
        let adapter = EthereumAdapter::new(Chain::Base, MockEthereumRpc::new(), InMemoryCheckpointStore::new());
        assert_eq!(adapter.chain(), Chain::Base);
    }

    #[test]
    fn adapter_chain_arbitrum() {
        let adapter = EthereumAdapter::new(Chain::Arbitrum, MockEthereumRpc::new(), InMemoryCheckpointStore::new());
        assert_eq!(adapter.chain(), Chain::Arbitrum);
    }

    #[test]
    fn adapter_chain_polygon() {
        let adapter = EthereumAdapter::new(Chain::Polygon, MockEthereumRpc::new(), InMemoryCheckpointStore::new());
        assert_eq!(adapter.chain(), Chain::Polygon);
    }

    #[test]
    fn adapter_default_reorg_depth_is_12() {
        let adapter = make_adapter();
        assert_eq!(adapter.reorg_depth(), 12);
    }

    #[test]
    fn adapter_with_reorg_depth_override() {
        let adapter = make_adapter().with_reorg_depth(6);
        assert_eq!(adapter.reorg_depth(), 6);
    }

    #[tokio::test]
    async fn checkpoint_roundtrip() {
        let adapter = make_adapter();
        let cp = Checkpoint {
            slot: 20_000_001,
            last_signature: None,
        };
        adapter.checkpoint_save(&cp).await.unwrap();
        let loaded = adapter.checkpoint_load().await.unwrap().expect("checkpoint must exist");
        assert_eq!(loaded.slot, 20_000_001);
    }

    #[tokio::test]
    async fn checkpoint_load_empty() {
        let adapter = make_adapter();
        let cp = adapter.checkpoint_load().await.unwrap();
        assert!(cp.is_none());
    }

    #[tokio::test]
    async fn health_check_returns_ok_stub() {
        let adapter = make_adapter();
        adapter.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn tip_returns_block_ref_with_correct_chain() {
        let adapter = make_adapter();
        let tip = adapter.tip().await.unwrap();
        assert_eq!(tip.chain, Chain::Ethereum);
    }

    #[tokio::test]
    async fn tip_reflects_chain_param() {
        let adapter = EthereumAdapter::new(Chain::Bsc, MockEthereumRpc::new(), InMemoryCheckpointStore::new());
        let tip = adapter.tip().await.unwrap();
        assert_eq!(tip.chain, Chain::Bsc);
    }

    #[tokio::test]
    async fn subscribe_returns_stream_stub() {
        use futures::StreamExt;
        let adapter = make_adapter();
        let mut stream = adapter.subscribe(SubscribeFilter::default());
        // Sprint-15 stub: empty stream terminates immediately.
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn backfill_returns_stream_stub() {
        use futures::StreamExt;
        let adapter = make_adapter();
        let mut stream = adapter.backfill(1_000_000..=1_001_000);
        assert!(stream.next().await.is_none());
    }
}
