//! Solana chain adapter — standard JSON-RPC 2.0 + WebSocket implementation.
//!
//! # Overview
//!
//! [`SolanaAdapter`] implements the [`ChainAdapter`] trait for Solana using standard
//! Solana JSON-RPC 2.0 over HTTP and WebSocket, per ADR 0007 / design 0028.
//!
//! Sprint 26 (T26-2) replaced the Yellowstone gRPC Geyser plugin path with this
//! standard JSON-RPC adapter.  The operational model change is:
//!
//! - **Before (Sprint 25)**: continuous Yellowstone gRPC firehose via `GeyserClient`,
//!   requiring a 256–512 GB RAM validator-class Solana node.
//! - **After (Sprint 26)**: pull-based JSON-RPC queries against a standard Agave
//!   RPC-only node (~64–128 GB RAM) using:
//!   - `logsSubscribe` / `programSubscribe` WebSocket subscriptions for live events.
//!   - `getSignaturesForAddress` + `getTransaction` + `getBlock` HTTP calls for backfill.
//!   - `getHealth` / `getSlot` for liveness and tip queries.
//!
//! # Configuration
//!
//! Two URL fields replace the single gRPC `endpoint`:
//! - `http_url` — JSON-RPC HTTP endpoint (port 8899 by default on Agave).
//! - `ws_url`   — JSON-RPC WebSocket endpoint (port 8900 by default on Agave).
//!
//! # Known gaps (Phase 1 / Sprint 26)
//!
//! - `logsSubscribe` provides the transaction signature but not raw instruction bytes.
//!   Full `Event::Transfer` / `Event::Swap` emission from the live stream is deferred
//!   to the detector evaluation path via `getTransaction` (T26-4).
//!   The subscribe stream currently emits `Event::SlotFinalized` as a signal that
//!   a relevant slot has activity.
//!
//! - DEX-specific pool state reconstruction (Raydium reserves, Orca tick state) belongs
//!   in `crates/dex-adapter` (Phase 2).
//!
//! - Full `TokenMeta` enrichment requires `getAccountInfo` calls; that belongs in
//!   `crates/token-registry` (Phase 2).

pub mod backfill;
pub mod checkpoint;
pub mod config;
pub mod decode;
pub mod reconnect;
pub mod subscribe;
pub mod token2022;

use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tracing::{info, warn};

use mg_onchain_common::chain::{BlockRef, Chain};

use crate::{
    error::AdapterError,
    solana::{
        checkpoint::CheckpointStore,
        config::SolanaAdapterConfig,
        subscribe::build_subscribe_stream,
        backfill::build_backfill_stream,
    },
    ChainAdapter, Checkpoint, Event, SubscribeFilter,
};

// ---------------------------------------------------------------------------
// SolanaAdapter
// ---------------------------------------------------------------------------

/// Solana chain adapter backed by standard JSON-RPC 2.0 + WebSocket.
///
/// Create via [`SolanaAdapter::new`], then call [`ChainAdapter::subscribe`] or
/// [`ChainAdapter::backfill`].
///
/// Thread-safe: `Clone` produces a new handle sharing the same config and
/// checkpoint store. Safe to use across tokio tasks.
pub struct SolanaAdapter {
    config: Arc<SolanaAdapterConfig>,
    checkpoint_store: Arc<dyn CheckpointStore>,
}

impl SolanaAdapter {
    /// Create a new `SolanaAdapter` with the given config and checkpoint store.
    ///
    /// # Checkpoint store injection
    ///
    /// Production code passes a [`checkpoint::FileCheckpointStore`]. Test code
    /// passes an [`checkpoint::InMemoryCheckpointStore`].
    ///
    /// ```ignore
    /// use mg_onchain_chain_adapter::solana::{
    ///     SolanaAdapter,
    ///     config::{SolanaAdapterConfig, CommitmentConfig, ReconnectPolicy, SubscribeFiltersConfig},
    ///     checkpoint::FileCheckpointStore,
    /// };
    /// use url::Url;
    ///
    /// let config = SolanaAdapterConfig {
    ///     http_url: Url::parse("http://127.0.0.1:8899").unwrap(),
    ///     ws_url:   Url::parse("ws://127.0.0.1:8900").unwrap(),
    ///     auth_token: None,
    ///     commitment: CommitmentConfig::Confirmed,
    ///     reconnect: ReconnectPolicy::default(),
    ///     filters: SubscribeFiltersConfig::default(),
    ///     checkpoint_path: "./checkpoints/solana.json".into(),
    /// };
    ///
    /// let adapter = SolanaAdapter::new(
    ///     config,
    ///     FileCheckpointStore::new("./checkpoints/solana.json"),
    /// );
    /// ```
    pub fn new(
        config: SolanaAdapterConfig,
        checkpoint_store: impl CheckpointStore + 'static,
    ) -> Self {
        Self {
            config: Arc::new(config),
            checkpoint_store: Arc::new(checkpoint_store),
        }
    }
}

// ---------------------------------------------------------------------------
// ChainAdapter implementation
// ---------------------------------------------------------------------------

impl ChainAdapter for SolanaAdapter {
    fn subscribe(
        &self,
        filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        // Load resume slot from checkpoint (non-blocking; checkpoint_store::load is sync).
        let resume_slot = self
            .checkpoint_store
            .load()
            .ok()
            .flatten()
            .map(|cp| cp.slot);

        if let Some(slot) = resume_slot {
            info!(slot, "resuming subscribe from checkpoint");
        }

        build_subscribe_stream(Arc::clone(&self.config), filter, resume_slot)
    }

    fn backfill(
        &self,
        range: RangeInclusive<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        info!(
            start = range.start(),
            end = range.end(),
            "starting backfill stream"
        );
        build_backfill_stream(Arc::clone(&self.config), range)
    }

    async fn checkpoint_save(&self, checkpoint: &Checkpoint) -> Result<(), AdapterError> {
        self.checkpoint_store.save(checkpoint).map_err(|e| {
            warn!(slot = checkpoint.slot, error = %e, "checkpoint save failed");
            e
        })
    }

    async fn checkpoint_load(&self) -> Result<Option<Checkpoint>, AdapterError> {
        self.checkpoint_store.load()
    }

    async fn health_check(&self) -> Result<(), AdapterError> {
        subscribe::health_check_connection(&self.config).await
    }

    async fn tip(&self) -> Result<BlockRef, AdapterError> {
        let slot = subscribe::get_tip_slot(&self.config).await?;
        Ok(BlockRef::new(Chain::Solana, slot))
    }

    /// Override: return the Solana-specific SPL token + DEX program filter.
    ///
    /// ADR 0005 Decision 5: each adapter overrides `default_filter()` so
    /// `Indexer::run` is chain-agnostic with respect to filter construction.
    fn default_filter(&self) -> SubscribeFilter {
        SubscribeFilter::solana_default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::{checkpoint::InMemoryCheckpointStore, config::*};
    use url::Url;

    fn test_config() -> SolanaAdapterConfig {
        SolanaAdapterConfig {
            http_url: Url::parse("http://127.0.0.1:8899").unwrap(),
            ws_url: Url::parse("ws://127.0.0.1:8900").unwrap(),
            auth_token: None,
            commitment: CommitmentConfig::Confirmed,
            reconnect: ReconnectPolicy::default(),
            filters: SubscribeFiltersConfig::default(),
            checkpoint_path: "/tmp/test_checkpoint.json".into(),
        }
    }

    fn make_adapter() -> SolanaAdapter {
        SolanaAdapter::new(test_config(), InMemoryCheckpointStore::new())
    }

    // --- Checkpoint roundtrip via ChainAdapter ---

    #[tokio::test]
    async fn checkpoint_save_and_load_roundtrip() {
        let adapter = make_adapter();
        let cp = Checkpoint {
            slot: 42_000_000,
            last_signature: Some("TestSig1111".into()),
        };
        adapter.checkpoint_save(&cp).await.unwrap();
        let loaded = adapter.checkpoint_load().await.unwrap().expect("must have checkpoint");
        assert_eq!(loaded.slot, 42_000_000);
        assert_eq!(loaded.last_signature.as_deref(), Some("TestSig1111"));
    }

    #[tokio::test]
    async fn checkpoint_load_returns_none_on_fresh_adapter() {
        let adapter = make_adapter();
        let cp = adapter.checkpoint_load().await.unwrap();
        assert!(cp.is_none());
    }

    // --- subscribe: stream type test (no network) ---

    #[tokio::test]
    async fn subscribe_returns_a_stream() {
        // Verify the method compiles and returns a stream.
        // We cannot test content without a real JSON-RPC WebSocket endpoint.
        let adapter = make_adapter();
        let _stream = adapter.subscribe(SubscribeFilter::solana_default());
        // If we got here without panic, the type is correct.
    }

    // --- backfill: stream type test (no network) ---

    #[tokio::test]
    async fn backfill_returns_a_stream() {
        let adapter = make_adapter();
        let _stream = adapter.backfill(100..=200);
    }

    // --- Subscribe filter contains known programs ---

    #[test]
    fn solana_default_filter_contains_token_2022() {
        let filter = SubscribeFilter::solana_default();
        assert!(
            filter.program_ids.contains(&"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".to_string()),
            "default filter must include Token-2022 program"
        );
    }

    #[test]
    fn solana_default_filter_includes_slot_updates() {
        let filter = SubscribeFilter::solana_default();
        assert!(filter.include_slot_updates, "default filter must enable slot updates for reorg detection");
    }
}
