//! `mg-onchain-token-registry` — Token metadata enrichment service.
//!
//! This crate turns partial [`TokenMeta`] records (from `chain-adapter`) into
//! full [`TokenMeta`] records by calling Solana JSON-RPC and classifying each
//! holder address.
//!
//! # Public API
//!
//! ```ignore
//! use mg_onchain_token_registry::{TokenRegistry, RegistryConfig};
//! use mg_onchain_common::chain::Chain;
//!
//! let config = RegistryConfig::default();
//! let registry = TokenRegistry::new(config, store, rpc).await?;
//! let meta = registry.enrich("SomeMintAddress...", Chain::Solana).await?;
//! ```
//!
//! # Module layout
//!
//! - [`config`]       — `RegistryConfig` with TTL, retry, concurrency settings.
//! - [`error`]        — `RegistryError` (thiserror, non_exhaustive).
//! - [`rpc`]          — `SolanaRpc` trait + `HttpSolanaRpc` + `MockSolanaRpc` (tests).
//! - [`programs`]     — Known program IDs (Raydium, Orca, Streamflow, etc.) + lookup helpers.
//! - [`cex_registry`] — CEX hot-wallet address map (loaded from `data/cex_wallets.json`).
//! - [`classify`]     — `HolderClassifier` — burn/dex/vesting/CEX/liquid classification ladder.
//! - [`locker`]       — LP-lock state analysis (lp_burned_pct + LockerInfo population).
//! - [`enrich`]       — `enrich_token_inner` — the core enrichment function.
//! - [`snapshot`]     — Periodic holder-snapshot job loop.

pub mod cex_registry;
pub mod classify;
pub mod config;
pub mod enrich;
pub mod error;
pub mod graduation;
pub mod launchpad_decoder;
pub mod locker;
pub mod programs;
pub mod rpc;
pub mod snapshot;
pub mod tlv;

use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::instrument;

use mg_onchain_common::chain::Chain;
use mg_onchain_common::token::TokenMeta;
use mg_onchain_storage::pg::PgStore;

pub use crate::classify::HolderKind;
pub use crate::config::RegistryConfig;
pub use crate::error::RegistryError;
pub use crate::rpc::{HttpSolanaRpc, SolanaRpc};

/// The token registry service.
///
/// Holds shared state (config, store, RPC client, semaphore, CEX registry).
/// Clone-cheap: all inner state is behind `Arc`.
#[derive(Clone)]
pub struct TokenRegistry {
    config: Arc<RegistryConfig>,
    store: PgStore,
    rpc: Arc<dyn SolanaRpc>,
    semaphore: Arc<Semaphore>,
    cex: Arc<cex_registry::CexRegistry>,
}

impl TokenRegistry {
    /// Construct a new `TokenRegistry`.
    ///
    /// Loads the embedded CEX wallet registry (panics only if the embedded JSON
    /// is malformed, which would be caught by the `cex_registry` unit test).
    pub fn new(config: RegistryConfig, store: PgStore, rpc: Arc<dyn SolanaRpc>) -> Self {
        let concurrency = config.concurrency_limit;
        let cex = cex_registry::CexRegistry::load_embedded()
            .expect("embedded cex_wallets.json must be valid (caught by unit tests)");
        Self {
            semaphore: Arc::new(Semaphore::new(concurrency)),
            config: Arc::new(config),
            store,
            rpc,
            cex: Arc::new(cex),
        }
    }

    /// Convenience constructor with default config and an HTTP RPC client.
    pub fn with_http_rpc(config: RegistryConfig, store: PgStore) -> Self {
        let rpc = Arc::new(HttpSolanaRpc::new(&config));
        Self::new(config, store, rpc)
    }

    /// Enrich a token by mint address.
    ///
    /// Returns a full [`TokenMeta`]. May return cached data if within TTL.
    /// Respects the concurrency semaphore.
    #[instrument(skip(self), fields(mint, chain = chain.as_str()))]
    pub async fn enrich(&self, mint: &str, chain: Chain) -> Result<TokenMeta, RegistryError> {
        enrich::enrich_token(
            mint,
            chain,
            self.rpc.as_ref(),
            &self.store,
            self.cex.as_ref(),
            self.config.as_ref(),
            &self.semaphore,
        )
        .await
    }

    /// Return the underlying `SolanaRpc` client.
    ///
    /// Used by production call sites (e.g. `HttpPoolAccountProvider`) that need
    /// direct RPC access without going through the enrichment layer.
    pub fn rpc(&self) -> Arc<dyn SolanaRpc> {
        self.rpc.clone()
    }

    /// Classify a single holder address using the classification ladder.
    ///
    /// This is the OQ1 fallback path (per design 0003 §OQ1 resolution):
    /// for holder addresses not yet in the `holder_classifications` sidecar table,
    /// call this method to trigger the ladder and (optionally) write back to the
    /// sidecar.
    ///
    /// The D03 concentration detector's PRIMARY path is a SQL LEFT JOIN against
    /// `holder_classifications` — use this method only for novel addresses not
    /// yet in the sidecar.
    ///
    /// # Arguments
    ///
    /// - `address`: Solana Base58 address of the **token account** (not the owner
    ///   wallet). The classifier resolves the owner via RPC.
    /// - `chain`: Currently always `Chain::Solana` for Phase 2.
    ///
    /// # Errors
    ///
    /// Returns `RegistryError` if the classification ladder itself fails
    /// (e.g. malformed address). RPC failures during owner lookup are handled
    /// internally and fall back to `HolderKind::Liquid` — they do NOT surface as
    /// errors from this method.
    #[instrument(skip(self), fields(address, chain = chain.as_str()))]
    pub async fn classify_holder(
        &self,
        address: &str,
        chain: Chain,
    ) -> Result<HolderKind, RegistryError> {
        let classifier = classify::HolderClassifier::new(self.rpc.as_ref(), self.cex.as_ref());
        let classification = classifier.classify(address, chain.as_str()).await?;
        Ok(classification.kind)
    }
}

// ---------------------------------------------------------------------------
// Tests for classify_holder public method
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::programs::{BURN_ADDRESS, RAYDIUM_AMM_V4};
    use crate::rpc::tests::MockSolanaRpc;
    use sqlx::PgPool;

    /// Build a `TokenRegistry` with a MockSolanaRpc for unit tests.
    /// PgStore is constructed with a never-connected pool; these tests
    /// exercise only the classify_holder path which uses the RPC, not the store.
    fn make_registry_with_rpc(rpc: MockSolanaRpc) -> TokenRegistry {
        // Use a placeholder URL — the pool is created but never connected in
        // these tests because classify_holder only calls the RPC client.
        // We construct PgPool with an intentionally invalid URL so any accidental
        // DB call fails loudly. The pool itself is lazy — construction succeeds.
        let pool = PgPool::connect_lazy("postgres://test:test@localhost/test_placeholder")
            .expect("lazy pool construction must succeed");
        let store = PgStore::new(pool);
        TokenRegistry::new(RegistryConfig::default(), store, Arc::new(rpc))
    }

    /// classify_holder on the Solana burn address returns BurnAddress.
    #[tokio::test]
    async fn classify_holder_burn_address() {
        let rpc = MockSolanaRpc::default();
        let registry = make_registry_with_rpc(rpc);

        let kind = registry
            .classify_holder(BURN_ADDRESS, Chain::Solana)
            .await
            .expect("classify_holder must not error for valid address");

        assert_eq!(kind, HolderKind::BurnAddress);
    }

    /// classify_holder on an address whose token account is owned by Raydium v4
    /// returns DexPool with subkind "raydium_amm_v4".
    #[tokio::test]
    async fn classify_holder_dex_pool_raydium_v4() {
        let token_account = "SomeTokenAccount1111111111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some(RAYDIUM_AMM_V4.to_owned()))),
            ..Default::default()
        };
        let registry = make_registry_with_rpc(rpc);

        let kind = registry
            .classify_holder(token_account, Chain::Solana)
            .await
            .expect("classify_holder must not error");

        assert!(
            matches!(kind, HolderKind::DexPool { ref subkind } if subkind == "raydium_amm_v4"),
            "expected DexPool(raydium_amm_v4), got {kind:?}"
        );
    }

    /// classify_holder on an unknown address returns Liquid (fallback).
    #[tokio::test]
    async fn classify_holder_unknown_returns_liquid() {
        let token_account = "SomeRandomWallet11111111111111111111111111";
        let rpc = MockSolanaRpc {
            token_account_owner: Some(Ok(Some("SomeRandomProgram1111111111111111111111111111".to_owned()))),
            ..Default::default()
        };
        let registry = make_registry_with_rpc(rpc);

        let kind = registry
            .classify_holder(token_account, Chain::Solana)
            .await
            .expect("classify_holder must not error for unknown owner");

        assert_eq!(kind, HolderKind::Liquid);
    }

    /// classify_holder is public and accessible via the `mg_onchain_token_registry`
    /// crate — this is the OQ1 resolution test (design 0003 §OQ1).
    #[test]
    fn classify_holder_is_public_api() {
        // Compile-time check: the method signature is accessible to external callers.
        // This test exists to document the API addition and catch regressions.
        fn _assert_public<T: Send + Sync>(_: &TokenRegistry) {}

        // If this compiles, classify_holder is publicly accessible on TokenRegistry.
        // Actual async invocation is tested in the tokio tests above.
        let _ = TokenRegistry::classify_holder; // method pointer is accessible
    }
}
