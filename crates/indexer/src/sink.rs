//! `EventSink` — the storage abstraction the indexer writes through.
//!
//! Production code wires in `PgEventSink` (backed by `storage::PgStore`).
//! Tests use `MockEventSink` which records calls in memory.
//!
//! # Why a separate trait instead of using `PgStore` directly?
//!
//! Two reasons:
//! 1. **Testability.** Unit tests for the batcher, router, and reorg handler do
//!    not need a real Postgres connection. `MockEventSink` records calls to a
//!    `Vec` behind `Arc<Mutex<_>>` and tests assert on it.
//! 2. **Dependency inversion.** The indexer crate depends on the `EventSink`
//!    abstraction, not on `storage::PgStore` concretely. This makes it possible
//!    to swap the underlying storage without changing the indexer logic.
//!
//! # Rust 2024 native async traits
//!
//! This trait uses native `async fn` in trait methods (Rust 1.75+, stabilised in
//! the 2024 edition default feature set). No `async_trait` macro needed.
//! Object safety: the trait is NOT object-safe because of the `async fn` methods
//! (they implicitly return `impl Future<Output = ...>`). Callers that need
//! `dyn EventSink` must use a wrapper or monomorphise. For the indexer the
//! concrete type is known at compile time (monomorphised via generics) so
//! object safety is not required.

use mg_onchain_common::event::{PoolEvent, Swap, Transfer};
use mg_onchain_common::token::{HolderSnapshot, TokenMeta};

use crate::error::IndexerError;

// ---------------------------------------------------------------------------
// EventSink trait
// ---------------------------------------------------------------------------

/// Write interface that the indexer calls to persist events and perform reorg
/// deletions.
///
/// All methods take slices of typed events and must be idempotent (the Postgres
/// implementation uses `ON CONFLICT DO NOTHING`).
///
/// The `delete_from_slot` method is called on reorg: it removes all events at
/// or after a given block height from the three mutable event tables. It must
/// NOT delete from `holder_snapshots` (idempotent UPSERT guard) or
/// `anomaly_events` (detectors re-run on post-reorg state).
pub trait EventSink {
    /// Write a batch of transfers.
    fn insert_transfers(
        &self,
        transfers: &[Transfer],
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;

    /// Write a batch of swaps.
    fn insert_swaps(
        &self,
        swaps: &[Swap],
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;

    /// Write a batch of pool events.
    fn insert_pool_events(
        &self,
        events: &[PoolEvent],
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;

    /// Upsert holder snapshots (current state + history for full snapshots).
    fn upsert_holder_snapshots(
        &self,
        snapshots: &[HolderSnapshot],
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;

    /// Upsert a single `TokenMeta` event into the `tokens` table.
    ///
    /// Low-volume: called once per newly-seen mint address as events flow through
    /// the router. Does not batch — individual upsert per event is correct here.
    ///
    /// Fields `permanent_delegate` and `transfer_hook_program` from `TokenMeta`
    /// are NOT yet stored in the `tokens` schema (Phase-3 TLV gap — see P3-1
    /// report). They are silently dropped until V00005 migration + TLV decoder
    /// land. All other fields are persisted.
    fn upsert_token_meta(
        &self,
        meta: &TokenMeta,
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;

    /// Delete all mutable events (transfers, swaps, pool_events) where
    /// `block_height >= from_slot`.
    ///
    /// Also deletes `holder_snapshots_history` rows in the same range.
    ///
    /// Called during reorg handling. `chain` must be the canonical chain string
    /// (e.g. `"solana"`).
    fn delete_from_slot(
        &self,
        chain: &str,
        from_slot: u64,
    ) -> impl std::future::Future<Output = Result<(), IndexerError>> + Send;
}

// ---------------------------------------------------------------------------
// PgEventSink — production implementation wrapping PgStore
// ---------------------------------------------------------------------------

/// Production `EventSink` backed by `mg_onchain_storage::PgStore`.
///
/// All writes use the existing `insert_*` and `upsert_*` methods in `pg.rs`.
/// Those methods use `ON CONFLICT DO NOTHING` (or the block_height guard for
/// holder snapshots), making this sink idempotent on duplicate writes.
#[derive(Clone)]
pub struct PgEventSink {
    pg: mg_onchain_storage::PgStore,
}

impl PgEventSink {
    /// Construct from an existing `PgStore`.
    pub fn new(pg: mg_onchain_storage::PgStore) -> Self {
        Self { pg }
    }
}

impl EventSink for PgEventSink {
    async fn insert_transfers(&self, transfers: &[Transfer]) -> Result<(), IndexerError> {
        self.pg
            .insert_transfers(transfers)
            .await
            .map_err(IndexerError::Storage)
    }

    async fn insert_swaps(&self, swaps: &[Swap]) -> Result<(), IndexerError> {
        self.pg
            .insert_swaps(swaps)
            .await
            .map_err(IndexerError::Storage)
    }

    async fn insert_pool_events(&self, events: &[PoolEvent]) -> Result<(), IndexerError> {
        self.pg
            .insert_pool_events(events)
            .await
            .map_err(IndexerError::Storage)
    }

    async fn upsert_holder_snapshots(
        &self,
        snapshots: &[HolderSnapshot],
    ) -> Result<(), IndexerError> {
        self.pg
            .upsert_holder_snapshots(snapshots)
            .await
            .map_err(IndexerError::Storage)
    }

    async fn upsert_token_meta(&self, meta: &TokenMeta) -> Result<(), IndexerError> {
        let chain = meta.chain.as_str();
        let mint = meta.mint.as_str();
        let symbol = meta.symbol.as_deref();
        let name = meta.name.as_deref();
        let decimals = meta.decimals as i16;
        let token_program = meta.token_program.as_ref().map(|a| a.as_str().to_owned());
        let total_supply_raw = meta.total_supply_raw;
        let circulating_supply_raw = meta.circulating_supply_raw;
        let mint_authority = meta.mint_authority.as_ref().map(|a| a.as_str().to_owned());
        let freeze_authority = meta
            .freeze_authority
            .as_ref()
            .map(|a| a.as_str().to_owned());
        let creator = meta.creator.as_ref().map(|a| a.as_str().to_owned());
        let creator_balance_raw = meta.creator_balance_raw;
        let total_holders = meta.total_holders as i64;
        let total_market_liquidity_usd = meta.total_market_liquidity_usd.to_string();
        let jup_verified = meta.verification.jup_verified;
        let jup_strict = meta.verification.jup_strict;
        let rugged = meta.rugged;
        let rugcheck_score = meta.rugcheck_score.map(|s| s as i32);
        let launchpad = meta.launchpad.as_deref();
        let deploy_platform = meta.deploy_platform.as_deref();
        let detected_at = meta.detected_at;

        self.pg
            .upsert_token(
                chain,
                mint,
                symbol,
                name,
                decimals,
                token_program.as_deref(),
                total_supply_raw,
                circulating_supply_raw,
                mint_authority.as_deref(),
                freeze_authority.as_deref(),
                creator.as_deref(),
                creator_balance_raw,
                total_holders,
                &total_market_liquidity_usd,
                jup_verified,
                jup_strict,
                rugged,
                rugcheck_score,
                launchpad,
                deploy_platform,
                detected_at,
                meta.permanent_delegate.as_ref().map(|a| a.as_str()),
                meta.transfer_hook_program.as_ref().map(|a| a.as_str()),
                meta.non_transferable,
                meta.confidential_transfer,
            )
            .await
            .map_err(IndexerError::Storage)
    }

    async fn delete_from_slot(&self, chain: &str, from_slot: u64) -> Result<(), IndexerError> {
        use sqlx::PgPool;
        let pool: &PgPool = self.pg.pool();

        // DELETE from each mutable event table.
        // block_height is a BIGINT; from_slot fits since u64 <= i64::MAX in
        // practice for Solana (current slot ~310M, far below 2^63).
        let slot_i64 = from_slot as i64;

        sqlx::query("DELETE FROM transfers WHERE chain = $1 AND block_height >= $2")
            .bind(chain)
            .bind(slot_i64)
            .execute(pool)
            .await
            .map_err(mg_onchain_storage::StorageError::from)
            .map_err(IndexerError::Storage)?;

        sqlx::query("DELETE FROM swaps WHERE chain = $1 AND block_height >= $2")
            .bind(chain)
            .bind(slot_i64)
            .execute(pool)
            .await
            .map_err(mg_onchain_storage::StorageError::from)
            .map_err(IndexerError::Storage)?;

        sqlx::query("DELETE FROM pool_events WHERE chain = $1 AND block_height >= $2")
            .bind(chain)
            .bind(slot_i64)
            .execute(pool)
            .await
            .map_err(mg_onchain_storage::StorageError::from)
            .map_err(IndexerError::Storage)?;

        // holder_snapshots_history is append-only — delete the reorged range.
        sqlx::query("DELETE FROM holder_snapshots_history WHERE chain = $1 AND block_height >= $2")
            .bind(chain)
            .bind(slot_i64)
            .execute(pool)
            .await
            .map_err(mg_onchain_storage::StorageError::from)
            .map_err(IndexerError::Storage)?;

        // holder_snapshots (current state): NOT deleted — the
        // `WHERE EXCLUDED.block_height > holder_snapshots.block_height` UPSERT
        // guard makes re-emission idempotent. After the reorg, the adapter will
        // re-emit snapshots for the post-reorg state and the guard will update
        // only if the incoming block_height is newer.

        Ok(())
    }
}
