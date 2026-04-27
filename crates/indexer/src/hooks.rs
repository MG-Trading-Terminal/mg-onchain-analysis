//! `PoolInitializeHook` — extension point for reacting to new pool initializations
//! and their reorg reversals from within the indexer event loop.
//!
//! # Design
//!
//! The indexer already fires `GraphIndexerWriter::on_pool_event` on every
//! `PoolEvent::Initialize`. `PoolInitializeHook` is the parallel extension point
//! for detector-level side effects (e.g. D09 BOCPD state updates) that must run
//! at the same moment — immediately after the pool is observed, not in a scheduled
//! batch later.
//!
//! # Object safety
//!
//! The trait uses `async_trait` so it is dyn-compatible. The indexer stores it as
//! `Option<Arc<dyn PoolInitializeHook>>`.
//!
//! # Fail-loud semantics
//!
//! Both methods return `Result<(), IndexerError>`. The indexer propagates errors
//! from `on_new_token_launch` and `on_reorg` with `?`, matching the fail-loud
//! pattern already used for `graph_writer.on_pool_event`.
//!
//! # Time source discipline (gotcha #22 / #28)
//!
//! `observed_at` is always `pe.block_time` — derived from the block header,
//! never `Utc::now()`. Callers in `crates/indexer/src/lib.rs` enforce this.

use chrono::{DateTime, Utc};

use mg_onchain_common::chain::{BlockRef, Chain};

use crate::error::IndexerError;

// ---------------------------------------------------------------------------
// PoolInitializeHook trait
// ---------------------------------------------------------------------------

/// Hook called by the indexer event loop on `PoolEvent::Initialize` and on reorg.
///
/// Implement this trait to react to new token launches and reorg reversals
/// from within the indexer pipeline, without touching the indexer core.
///
/// See `crates/detectors/src/d09_deployer_changepoint.rs::D09IndexerHook`
/// for the reference implementation.
#[async_trait::async_trait]
pub trait PoolInitializeHook: Send + Sync {
    /// Called immediately after the graph writer processes a `PoolEvent::Initialize`.
    ///
    /// `chain` is the chain the pool was created on.
    /// `deployer` is `pe.actor.as_str()` — the wallet that signed the Initialize tx.
    /// `token0` / `token1` are the two tokens in the pool (from `PoolEventKind::Initialize`).
    /// `observed_at` is `pe.block_time` — NEVER `Utc::now()`.
    /// `block_ref` is `pe.block.clone()` — the block height and chain tag.
    ///
    /// # Errors
    ///
    /// Return `Err(IndexerError)` on transient failures. The indexer will propagate
    /// the error and stop the run loop (fail-loud, matching graph-writer semantics).
    async fn on_new_token_launch(
        &self,
        chain: Chain,
        deployer: &str,
        token0: &str,
        token1: &str,
        observed_at: DateTime<Utc>,
        block_ref: BlockRef,
    ) -> Result<(), IndexerError>;

    /// Called in the reorg handling path, after `handle_reorg` and alongside
    /// `graph_writer.on_reorg`.
    ///
    /// `chain` is the chain string (e.g. `"solana"`).
    /// `reorg_height` is the first invalidated slot/block number.
    ///
    /// # Errors
    ///
    /// Return `Err(IndexerError)` on transient failures. The indexer propagates.
    async fn on_reorg(&self, chain: &str, reorg_height: u64) -> Result<(), IndexerError>;
}
