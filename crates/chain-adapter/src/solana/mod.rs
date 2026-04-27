//! Solana chain adapter — Yellowstone gRPC implementation.
//!
//! # Overview
//!
//! [`SolanaAdapter`] implements the [`ChainAdapter`] trait for Solana using the
//! Yellowstone gRPC Geyser plugin protocol. It is provider-agnostic per ADR 0001 §D2:
//! the same struct connects to Helius LaserStream, Triton Dragon's Mouth, or a
//! self-hosted validator running the plugin — provider selection is config-only.
//!
//! # Version pinning
//!
//! This adapter was written against:
//! - `yellowstone-grpc-client = "13.1"`
//! - `yellowstone-grpc-proto = "12.2"`
//! - `tonic = "0.14"`
//! - `solana-sdk = "4"`
//!
//! If you bump these, verify that `GeyserGrpcClient::build_from_shared`,
//! `subscribe_once`, and the proto message field names haven't changed.
//! Check `CHANGELOG.md` entry for this task.
//!
//! # Known gaps (Phase 1)
//!
//! - Token-2022 transfer hook analysis: Phase 1 emits `Transfer` with
//!   `token_program = Token-2022` flag but does not decode hook output.
//!   See `decode.rs` `FLAG: TOKEN_2022_HOOK_ANALYSIS`.
//!
//! - DEX-specific pool state: `Swap` events carry DEX identity (`DexKind`) but
//!   pool reserve state is not decoded. That belongs in `crates/dex-adapter`.
//!
//! - Full `TokenMeta` enrichment: `symbol`, `name`, `top_holders`, `markets`
//!   require RPC calls. `token-registry` crate enriches these in Phase 2.
//!
//! - The `rpc_endpoint` backfill path uses a minimal `reqwest`-based JSON-RPC
//!   client. In Phase 2 this may be replaced by `solana-client` if more RPC
//!   methods are needed.

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

/// Solana chain adapter backed by Yellowstone gRPC.
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
    ///     endpoint: Url::parse("http://localhost:10000").unwrap(),
    ///     auth_token: None,
    ///     commitment: CommitmentConfig::Confirmed,
    ///     reconnect: ReconnectPolicy::default(),
    ///     filters: SubscribeFiltersConfig::default(),
    ///     rpc_endpoint: None,
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
        use yellowstone_grpc_client::GeyserGrpcClient;

        let endpoint_str = self.config.endpoint.as_str().trim_end_matches('/');

        let mut builder = GeyserGrpcClient::build_from_shared(endpoint_str.to_owned())
            .map_err(|e| AdapterError::Config(format!("invalid endpoint: {e}")))?;

        builder = builder
            .x_token(self.config.auth_token.clone())
            .map_err(|e| AdapterError::Config(format!("failed to set auth token: {e}")))?;

        let mut client = builder
            .connect()
            .await
            .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

        client
            .health_check()
            .await
            .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

        Ok(())
    }

    async fn tip(&self) -> Result<BlockRef, AdapterError> {
        use yellowstone_grpc_client::GeyserGrpcClient;

        let endpoint_str = self.config.endpoint.as_str().trim_end_matches('/');

        let mut builder = GeyserGrpcClient::build_from_shared(endpoint_str.to_owned())
            .map_err(|e| AdapterError::Config(format!("invalid endpoint: {e}")))?;

        builder = builder
            .x_token(self.config.auth_token.clone())
            .map_err(|e| AdapterError::Config(format!("failed to set auth token: {e}")))?;

        let mut client = builder
            .connect()
            .await
            .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

        let slot_response = client
            .get_slot(Some(self.config.commitment.to_proto()))
            .await
            .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

        Ok(BlockRef::new(Chain::Solana, slot_response.slot))
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
            endpoint: Url::parse("http://localhost:10000").unwrap(),
            auth_token: None,
            commitment: CommitmentConfig::Confirmed,
            reconnect: ReconnectPolicy::default(),
            filters: SubscribeFiltersConfig::default(),
            rpc_endpoint: None,
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
        // We cannot test content without a real gRPC endpoint.
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
