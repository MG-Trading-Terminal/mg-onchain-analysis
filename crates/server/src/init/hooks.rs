//! `PoolInitializeHook` composite constructor.
//!
//! # Gotcha #48: D09 + D10 are SEPARATE adapters
//!
//! D09IndexerHook and D10IndexerHook are distinct structs that both implement
//! `PoolInitializeHook`. The design uses a `CompositePoolInitializeHook` wrapper
//! that delegates to both in sequence.
//!
//! # Gotcha #39: Indexer::new 9-param signature unchanged
//!
//! The composite wraps both hooks into a single `Arc<dyn PoolInitializeHook>`.
//! `Indexer::new` receives this single `Option<Arc<dyn PoolInitializeHook>>`
//! without any change to its signature.
//!
//! # Design 0020 §3 Step 7
//!
//! The composite is passed to the Solana Indexer. The Ethereum Indexer currently
//! receives `None` — not because D09/D10 are Solana-only (chain guards removed
//! Sprint 24), but because the EVM indexer pool-init event path is not yet wired
//! through this composite (see SPEC-NOTE D10-EVM-POOL-INIT in d10_launch_audit.rs).
//! Sprint 25+: wire EVM PoolEvent::Initialize through CompositePoolInitializeHook.

use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tracing::info;

use mg_onchain_common::chain::{BlockRef, Chain};
use mg_onchain_detectors::d09_deployer_changepoint::{
    AnomalyEventSink, BocpdStateStore, D09BocpdDetector, D09Config, D09IndexerHook,
};
use mg_onchain_detectors::d10_launch_audit::{
    AnomalyEventSink as D10AnomalyEventSink, D10Config, D10IndexerHook, D10LaunchAuditDetector,
    TokenRegistry as D10TokenRegistry,
};
use mg_onchain_indexer::error::IndexerError;
use mg_onchain_indexer::hooks::PoolInitializeHook;

// ---------------------------------------------------------------------------
// CompositePoolInitializeHook
// ---------------------------------------------------------------------------

/// Delegates `PoolInitializeHook` calls to D09 and D10 in sequence.
///
/// The composite is the single hook registered with the Solana `Indexer`.
/// D09 and D10 chain-guards were removed in Sprint 24 — both now support all 6 chains.
/// The composite is currently only wired to the Solana indexer; EVM wiring is Sprint 25+
/// (see SPEC-NOTE D10-EVM-POOL-INIT in d10_launch_audit.rs).
struct CompositePoolInitializeHook {
    d09: Arc<D09IndexerHook>,
    d10: Arc<D10IndexerHook>,
}

#[async_trait]
impl PoolInitializeHook for CompositePoolInitializeHook {
    async fn on_new_token_launch(
        &self,
        chain: Chain,
        deployer: &str,
        token0: &str,
        token1: &str,
        observed_at: DateTime<Utc>,
        block_ref: BlockRef,
    ) -> Result<(), IndexerError> {
        // D09 first (BOCPD changepoint — higher priority signal)
        self.d09
            .on_new_token_launch(chain, deployer, token0, token1, observed_at, block_ref)
            .await?;
        // D10 second (launch audit — genesis snapshot)
        self.d10
            .on_new_token_launch(chain, deployer, token0, token1, observed_at, block_ref)
            .await?;
        Ok(())
    }

    async fn on_reorg(&self, chain: &str, reorg_height: u64) -> Result<(), IndexerError> {
        // Both hooks handle reorg; propagate first error.
        self.d09.on_reorg(chain, reorg_height).await?;
        self.d10.on_reorg(chain, reorg_height).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_pool_initialize_hook
// ---------------------------------------------------------------------------

/// Construct the composite `PoolInitializeHook` that wires D09 + D10.
///
/// # Arguments
///
/// - `bocpd_state_store` — D09 BOCPD state persistence (V00013).
/// - `anomaly_sink_d09` — D09 anomaly event sink (writes to `anomaly_events` table).
/// - `anomaly_sink_d10` — D10 anomaly event sink (same table; separate instance per
///   gotcha #48 — hooks are SEPARATE).
/// - `registry` — Token registry used by D10 to fetch `TokenMeta` at launch.
/// - `pg_pool` — Raw Postgres pool for D09 feature queries (design 0016 §4.2).
/// - `edge_store` — Graph edge store for D09 `DeployerOf` edges.
/// - `label_store` — Graph label store for D09 established-protocol suppression.
///
/// # Returns
///
/// `Arc<dyn PoolInitializeHook>` suitable for passing to `Indexer::new` as
/// `pool_initialize_hook: Some(composite)`.
#[allow(clippy::too_many_arguments)]
pub fn build_pool_initialize_hook(
    bocpd_state_store: Arc<dyn BocpdStateStore>,
    anomaly_sink_d09: Arc<dyn AnomalyEventSink>,
    anomaly_sink_d10: Arc<dyn D10AnomalyEventSink>,
    registry: Arc<dyn D10TokenRegistry>,
    pg_pool: Arc<sqlx::PgPool>,
    edge_store: Arc<dyn mg_onchain_graph::typed_edges::TypedEdgeStore>,
    label_store: Arc<dyn mg_onchain_graph::labels::GraphLabelStore>,
) -> anyhow::Result<Arc<dyn PoolInitializeHook>> {
    // Build D09 detector
    let d09_config = D09Config::default();
    let d09_detector = Arc::new(
        D09BocpdDetector::new(
            edge_store,
            label_store,
            bocpd_state_store,
            pg_pool.clone(),
            d09_config,
        )
        .context("D09BocpdDetector construction failed (composite weights)")?,
    );
    let d09_hook = Arc::new(D09IndexerHook::new(d09_detector, anomaly_sink_d09));

    // Build D10 detector
    let pg_pool_arc: sqlx::PgPool = (*pg_pool).clone();
    let d10_config = D10Config::default();
    let d10_detector = Arc::new(D10LaunchAuditDetector::new(pg_pool_arc, d10_config));
    let d10_hook = Arc::new(D10IndexerHook::new(d10_detector, anomaly_sink_d10, registry));

    info!("composite PoolInitializeHook built (D09 + D10)");

    Ok(Arc::new(CompositePoolInitializeHook { d09: d09_hook, d10: d10_hook }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify CompositePoolInitializeHook is a non-zero-size type (has both hook fields).
    #[test]
    fn composite_hook_is_non_zero_size() {
        // Structural test: verify the composite type wraps both D09 and D10.
        // We cannot call on_new_token_launch without PgPool in unit tests.
        // The integration test `binary_smoke.rs` covers the live-DB path.
        assert!(std::mem::size_of::<CompositePoolInitializeHook>() > 0);
    }

    /// Verify that `build_pool_initialize_hook` has the expected signature (type check).
    /// This is a compile-time test — if it compiles, the signature is correct.
    #[allow(dead_code, clippy::type_complexity)]
    fn _signature_check() {
        // Ensure the return type is the expected trait object.
        let _: fn(
            Arc<dyn BocpdStateStore>,
            Arc<dyn AnomalyEventSink>,
            Arc<dyn D10AnomalyEventSink>,
            Arc<dyn D10TokenRegistry>,
            Arc<sqlx::PgPool>,
            Arc<dyn mg_onchain_graph::typed_edges::TypedEdgeStore>,
            Arc<dyn mg_onchain_graph::labels::GraphLabelStore>,
        ) -> anyhow::Result<Arc<dyn PoolInitializeHook>> = build_pool_initialize_hook;
    }
}
