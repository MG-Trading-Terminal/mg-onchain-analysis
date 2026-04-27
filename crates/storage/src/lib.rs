//! Storage adapters: sqlx (Postgres) wrappers.
//!
//! # Overview
//!
//! This crate provides a single Postgres storage tier per ADR 0002:
//!
//! All tables — metadata (`tokens`, `pools`, `deployer_clusters`,
//! `adapter_checkpoints`, `audit`) and event tables (`transfers`, `swaps`,
//! `pool_events`, `holder_snapshots`, `anomaly_events`) — live in Postgres 16
//! with declarative range partitioning on `block_time`.
//!
//! ADR 0001 §D3 selected a dual-tier model (Postgres + ClickHouse). ADR 0002
//! supersedes that decision: at MVP event rates (hundreds/minute after filter),
//! Postgres with partitioning and BRIN indexes handles the volume with headroom.
//! The escape hatch to TimescaleDB is documented in ADR 0002.
//!
//! # Migration tool choice
//!
//! **Postgres:** `sqlx migrate` (sqlx-cli). Migration files live in
//! `migrations/postgres/`. Applied via `sqlx migrate run` or at startup via
//! `StorageHandle::new` when `migrations_auto_apply = true`.
//! Chosen over `refinery` because sqlx is already a dependency and the file
//! format is identical (versioned `.sql` files). Refinery adds no value.
//!
//! # Startup wiring
//!
//! ```rust,no_run
//! use mg_onchain_storage::{StorageConfig, StorageHandle};
//!
//! #[tokio::main]
//! async fn main() {
//!     let cfg = StorageConfig::from_env();
//!     let handle = StorageHandle::new(cfg).await.expect("storage init failed");
//!     // Use handle.pg
//! }
//! ```
//!
//! # Integration tests
//!
//! Tests that require a live database are gated behind `#[ignore]`:
//! ```bash
//! # Start database first:
//! # docker compose up -d pg
//! cargo test -p mg-onchain-storage -- --ignored
//! ```

pub mod checkpoint;
pub mod config;
pub mod error;
pub mod migrations;
pub mod pg;
pub mod price_provider;
pub mod token_metadata;
pub mod wallet_pnl_corpus;

pub use checkpoint::{AsyncCheckpointStore, Checkpoint, InMemoryAsyncCheckpointStore, PgCheckpointStore};
pub use config::StorageConfig;
pub use error::StorageError;
pub use pg::{BridgeTransferRow, PgStore};
pub use price_provider::{PgTokenPriceProvider, TokenPriceProvider};
pub use token_metadata::{MetadataError, TokenMetadata, TokenMetadataFetcher};
#[cfg(any(test, feature = "test-utils"))]
pub use price_provider::MockTokenPriceProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use token_metadata::MockTokenMetadataFetcher;
pub use wallet_pnl_corpus::{PgWalletPnlCorpusStore, WalletPnlCorpusRow, WalletPnlCorpusStore};
#[cfg(any(test, feature = "test-utils"))]
pub use wallet_pnl_corpus::MockWalletPnlCorpusStore;

use sqlx::PgPool;
use tracing::info;

// ---------------------------------------------------------------------------
// StorageHandle — top-level entry point
// ---------------------------------------------------------------------------

/// Fully initialised storage handle providing access to Postgres.
///
/// Constructed via `StorageHandle::new`. If `config.migrations_auto_apply = true`,
/// all pending migrations are applied before the handle is returned.
///
/// Clone is cheap: `PgStore` internally holds an `Arc`-wrapped connection pool.
#[derive(Debug, Clone)]
pub struct StorageHandle {
    /// Postgres storage — all tables (metadata + event tables).
    pub pg: PgStore,
}

impl StorageHandle {
    /// Connect to Postgres and optionally apply migrations.
    pub async fn new(config: StorageConfig) -> Result<Self, StorageError> {
        info!("initialising storage handle");

        // Connect to Postgres.
        let pool = PgPool::connect(&config.postgres_url).await?;
        let pg = PgStore::new(pool.clone());

        if config.migrations_auto_apply {
            migrations::run(&pool).await?;
        }

        Ok(Self { pg })
    }
}

// ---------------------------------------------------------------------------
// Module-level tests (unit only — no DB)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the public re-exports compile and are accessible.
    #[test]
    fn public_api_accessible() {
        // These are compile-time checks — no runtime assertions needed.
        let _: fn() -> _ = || StorageConfig::from_env();
        // InMemoryAsyncCheckpointStore is accessible
        let _store = InMemoryAsyncCheckpointStore::new();
    }

    /// Verify config serde roundtrip at the top level.
    #[test]
    fn storage_config_serde() {
        let toml_str = r#"
            postgres_url = "postgres://user:pass@localhost:5432/test"
            migrations_auto_apply = false
        "#;
        let cfg: StorageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.postgres_url, "postgres://user:pass@localhost:5432/test");
        assert!(!cfg.migrations_auto_apply);
    }
}
