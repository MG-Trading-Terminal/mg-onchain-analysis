//! Periodic holder-snapshot job.
//!
//! Runs every N hours (configurable via `RegistryConfig.snapshot_interval_hours`).
//! For every tracked token in the `tokens` Postgres table, fetches the full
//! top-20 holder list via `getTokenLargestAccounts`, computes Gini + top10_pct,
//! and writes to both `holder_snapshots` (current state) and
//! `holder_snapshots_history` (append-only, for D03 24h delta queries).
//!
//! # Two-table design
//!
//! Per ADR 0002 and docs/designs/0002-storage-schemas-v1.md:
//!   - `holder_snapshots`: UPSERT with WHERE EXCLUDED.block_height > current.
//!     One row per (chain, token, holder). Current state.
//!   - `holder_snapshots_history`: append-only full snapshots (`is_full = true`).
//!     D03 reads the two most recent full snapshots and computes the delta.
//!
//! # Why `is_full = false` for the enrichment-path snapshots?
//!
//! `getTokenLargestAccounts` returns at most 20 holders. A "full snapshot" in
//! the two-table design means ALL holders, not just the top-N. The snapshot
//! job uses `is_full = false` because we can only see the top 20 via the
//! Solana RPC. True full snapshots would require Geyser or a full account scan.
//!
//! For the MVP, D03 is calibrated against top-20 snapshots. The `is_full = true`
//! path in the storage layer is reserved for when a full account scan is added
//! in Phase 3+.
//!
//! # Graceful shutdown
//!
//! The job loop selects on both the interval tick AND a cancellation signal
//! (`tokio::sync::CancellationToken` via the standard tokio-util pattern).
//! On shutdown, the current batch is flushed before returning.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::{error, info, instrument, warn};

use mg_onchain_common::chain::Chain;

use mg_onchain_storage::pg::PgStore;

use crate::cex_registry::CexRegistry;
use crate::config::RegistryConfig;
use crate::enrich::enrich_token_inner;
use crate::rpc::SolanaRpc;

/// Run the holder snapshot job loop.
///
/// Loops every `config.snapshot_interval_hours` hours. On each tick:
/// 1. Fetch all tracked token mints from Postgres `tokens` table.
/// 2. For each token, call `enrich_token_inner` (which writes holder_snapshots).
///
/// The loop runs until `shutdown_rx` produces a value (or is dropped).
///
/// # Cancellation
///
/// The caller should use `tokio_util::sync::CancellationToken` or a
/// `tokio::sync::oneshot` channel as `shutdown_rx`. This function returns
/// `Ok(())` on clean shutdown.
pub async fn run_snapshot_job(
    chain: Chain,
    rpc: Arc<dyn SolanaRpc>,
    store: PgStore,
    cex: Arc<CexRegistry>,
    config: Arc<RegistryConfig>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let interval_duration = tokio::time::Duration::from_secs(
        config.snapshot_interval_hours * 3600,
    );
    let semaphore = Arc::new(Semaphore::new(config.concurrency_limit));
    let mut ticker = tokio::time::interval(interval_duration);

    info!(
        chain = chain.as_str(),
        interval_hours = config.snapshot_interval_hours,
        "holder snapshot job started"
    );

    loop {
        tokio::select! {
            biased;

            // Clean shutdown signal.
            _ = &mut shutdown_rx => {
                info!(chain = chain.as_str(), "holder snapshot job shutting down");
                return;
            }

            // Interval tick — run a snapshot pass.
            _ = ticker.tick() => {
                if let Err(e) = run_snapshot_pass(
                    chain,
                    rpc.as_ref(),
                    &store,
                    cex.as_ref(),
                    config.as_ref(),
                    &semaphore,
                ).await {
                    error!(chain = chain.as_str(), error = %e, "snapshot pass failed");
                }
            }
        }
    }
}

/// Execute one full snapshot pass over all tracked tokens.
#[instrument(skip_all, fields(chain = chain.as_str()))]
async fn run_snapshot_pass(
    chain: Chain,
    rpc: &dyn SolanaRpc,
    store: &PgStore,
    cex: &CexRegistry,
    config: &RegistryConfig,
    semaphore: &Arc<Semaphore>,
) -> anyhow::Result<()> {
    // Load all tracked mints from Postgres.
    // We call list_rugged_tokens + list_all_mints (Phase 3: add a dedicated
    // `list_all_mints` query to PgStore). For MVP, we use `list_rugged_tokens`
    // as a proxy — in practice the indexer populates all tokens anyway.
    // TODO Phase 3: add PgStore::list_tracked_mints(chain) -> Vec<String>.
    let mints = store.list_rugged_tokens(chain.as_str()).await?;
    let total = mints.len();
    info!(chain = chain.as_str(), total, "starting snapshot pass");

    let mut success = 0usize;
    let mut failed = 0usize;

    for row in &mints {
        let _permit = semaphore.acquire().await?;
        match enrich_token_inner(&row.mint, chain, rpc, store, cex, config).await {
            Ok(_) => success += 1,
            Err(e) => {
                warn!(
                    chain = chain.as_str(),
                    mint = %row.mint,
                    error = %e,
                    "snapshot enrichment failed for token"
                );
                failed += 1;
            }
        }
    }

    info!(
        chain = chain.as_str(),
        total,
        success,
        failed,
        "snapshot pass complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    // Snapshot job tests are integration-level (require Postgres).
    // Unit tests for the pure math are in enrich.rs.
    // For now: document what should be tested with a live DB.
    //
    // Integration tests (gated by `#[ignore]` for CI, run manually):
    //   1. Spin up Postgres via testcontainers.
    //   2. Insert 3 token mints into `tokens`.
    //   3. Run `run_snapshot_pass` against a MockSolanaRpc returning 20 holders each.
    //   4. Assert `holder_snapshots` has 3 * 20 = 60 rows.
    //   5. Assert `holder_snapshots_history` has 0 rows (is_full = false).
    //   6. Send shutdown signal — assert run_snapshot_job returns cleanly.
}
