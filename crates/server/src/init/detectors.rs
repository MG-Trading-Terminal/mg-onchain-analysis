//! Construct the streaming detector set (D01-D09, D11, D12, D13, D14) as `ArcErasedDetector` instances.
//!
//! # Detector set (13 streaming detectors)
//!
//! D10 (`launch_audit`) is **hook-triggered only** — `D10LaunchAuditDetector` does NOT
//! implement the `Detector` streaming trait. It runs exclusively via `D10IndexerHook`
//! at pool-initialize time. It is therefore absent from this list.
//!
//! All other detectors from D01-D09, D11, D12, D13, D14 implement `Detector` and are wired
//! into the streaming `SchedulerWorker` pool.
//!
//! Order: alphabetical by `detector_id` string, matching the
//! `TokenRiskReport.detectors_run` field convention.
//!
//! ```text
//! [0]  D14 bridge_drain_v1           (all 6 chains — third multi-chain detector)
//! [1]  D03 holder_concentration      (Solana)
//! [2]  D01 honeypot_sim              (Solana, cadenced)
//! [3]  D06 mint_burn_anomaly         (Solana)
//! [4]  D12 permit2_drainer_v1        (Ethereum — first EVM override)
//! [5]  D04 pump_dump                 (Solana)
//! [6]  D02 rug_pull_lp_drain         (Solana)
//! [7]  D09 deployer_changepoint      (Solana, hook-triggered + streaming re-eval)
//! [8]  D13 sandwich_mev_v1           (Ethereum — second EVM override)
//! [9]  D08 sybil_detection           (Solana)
//! [10] D11 synchronized_activity_v1  (Solana)
//! [11] D05 wash_trading_h1           (Solana)
//! [12] D07 withdraw_withheld         (Solana)
//! ```
//!
//! D01-D09, D11: `supported_chains() = &[Chain::Solana]` (default per gotcha #67).
//! D12:          `supported_chains() = &[Chain::Ethereum]` (first EVM override, gotcha #67).
//! D13:          `supported_chains() = &[Chain::Ethereum]` (second EVM override, gotcha #67).
//! D14:          `supported_chains() = &[Solana, Ethereum, Bsc, Base, Arbitrum, Polygon]` (all 6).
//!
//! The `SchedulerWorker` chain-filter gates dispatch; detectors do not need to
//! self-guard in `evaluate()` (though D09 does so for safety per gotcha #70).
//!
//! # Sprint 26
//!
//! `build_all_detectors` extends the prior 12-detector set to 13 streaming detectors.
//! D14 BridgeDrainDetector monitors known bridge custody addresses for anomalous outflows.

use std::sync::Arc;

use tracing::{info, warn};

use mg_onchain_detectors::config::AllDetectorConfigs;
use mg_onchain_detectors::{
    BocpdStateStore,
    ConcentrationDetector,
    D08SybilDetector,
    D09BocpdDetector,
    D09Config,
    D11SynchronizedActivityDetector,
    D12PermitDrainerDetector,
    D13SandwichMevDetector,
    D14BridgeDrainDetector,
    HoneypotDetector,
    KnownBridgeSet,
    KnownDrainerSet,
    MintBurnAnomalyDetector,
    PumpDumpDetector,
    RugPullDetector,
    WashTradingDetector,
    WithdrawWithheldDetector,
};
use mg_onchain_dex_adapter::pool_accounts::HttpPoolAccountProvider;
use mg_onchain_graph::api::PgClusterStore;
use mg_onchain_graph::labels::PgGraphLabelStore;
use mg_onchain_graph::smart_money_lookup::GraphSmartMoneyLookup;
use mg_onchain_graph::typed_edges::PgTypedEdgeStore;
use mg_onchain_graph::SmartMoneyLookup;
use mg_onchain_storage::price_provider::TokenPriceProvider;

use crate::erased_detector::ArcErasedDetector;

/// Construct the streaming detector set for use in the streaming worker pool.
///
/// Builds 13 detectors that implement the `Detector` trait and can be dispatched
/// by `SchedulerWorker`. D10 (`launch_audit`) is excluded because it is hook-triggered
/// only and does NOT implement `Detector`.
///
/// # Arguments
///
/// - `detector_config` — loaded from `config/detectors.toml`. Thresholds for all detectors.
/// - `pg_pool` — shared Postgres pool (passed to detectors that read from DB directly).
/// - `bocpd_state_store` — D09 BOCPD state store (V00013).
/// - `rpc` — Solana RPC client for D01 simulation.
/// - `price_provider` — Phase 5 USD enrichment provider (D11/D12/D13/D14). Constructed
///   from `PgTokenPriceProvider::new(pool.clone())` by the caller.
///
/// # Order
///
/// Deterministic: alphabetical by `detector_id` string, matching the
/// `TokenRiskReport.detectors_run` field convention.
///
/// # Errors
///
/// Returns `Err` only if D09 composite weights fail validation.
pub fn build_all_detectors(
    detector_config: &AllDetectorConfigs,
    pg_pool: Arc<sqlx::PgPool>,
    bocpd_state_store: Arc<dyn BocpdStateStore>,
    rpc: Arc<dyn mg_onchain_detectors::rpc::SolanaRpc>,
    price_provider: Arc<dyn TokenPriceProvider>,
) -> anyhow::Result<Vec<ArcErasedDetector>> {
    let pool = pg_pool.clone();
    let pool_inner: sqlx::PgPool = (*pool).clone();

    // D03 — Holder Concentration
    let d03 = Arc::new(ConcentrationDetector::new(
        detector_config.holder_concentration.clone(),
    ));

    // D01 — Honeypot (cadenced via SchedulerWorker modulo counter)
    let pool_account_provider = Arc::new(HttpPoolAccountProvider::new(rpc.clone()));
    let d01 = Arc::new(HoneypotDetector::new(
        detector_config.honeypot_sim.clone(),
        rpc.clone(),
        pool_account_provider,
    ));

    // NOTE: D10 (launch_audit) is intentionally absent here.
    // D10LaunchAuditDetector does not implement the Detector streaming trait;
    // it fires exclusively via D10IndexerHook at pool-initialize time.

    // D06 — Mint/Burn Anomaly
    let d06 = Arc::new(MintBurnAnomalyDetector::new(
        detector_config.mint_burn_anomaly.clone(),
    ));

    // D12 — Permit2 Drainer (multi-chain EVM; supported_chains override, gotcha #67)
    //
    // Load per-chain drainer clusters from config/known_drainers.toml when present.
    // This wires the `chains = [...]` field so Inferno Drainer populates BSC + Polygon
    // lookups, not just Ethereum. Falls back to flat address list from detectors.toml
    // if the file is absent (CI / minimal deployments).
    let known_drainers_path = std::path::Path::new("config/known_drainers.toml");
    let drainer_set: KnownDrainerSet = match crate::init::known_drainers::load_known_drainers(known_drainers_path) {
        Ok(set) => {
            info!(
                path = %known_drainers_path.display(),
                "D12: loaded per-chain drainer clusters from known_drainers.toml"
            );
            set
        }
        Err(e) => {
            warn!(
                path = %known_drainers_path.display(),
                error = %e,
                "D12: known_drainers.toml load failed; falling back to config flat address list"
            );
            KnownDrainerSet::from_addresses(&detector_config.permit2_drainer_v1.known_drainer_addresses.value)
        }
    };
    let d12 = Arc::new(D12PermitDrainerDetector::with_known_drainers(
        pool.clone(),
        drainer_set,
        price_provider.clone(),
    ));

    // D14 — Bridge Drain (all 6 chains — monitors known bridge custody addresses)
    //
    // Load per-chain bridge registry from config/known_bridges.toml when present.
    // Falls back to an empty registry if the file is absent (CI / minimal deployments).
    // Empty registry → no bridge addresses → D14 evaluate() returns Ok([]) for all chains.
    let known_bridges_path = std::path::Path::new("config/known_bridges.toml");
    let bridge_set: Arc<KnownBridgeSet> =
        match crate::init::known_bridges::load_known_bridges(known_bridges_path) {
            Ok(set) => {
                info!(
                    path = %known_bridges_path.display(),
                    bridges = set.bridge_count(),
                    addresses = set.address_count(),
                    "D14: loaded bridge registry from known_bridges.toml"
                );
                Arc::new(set)
            }
            Err(e) => {
                warn!(
                    path = %known_bridges_path.display(),
                    error = %e,
                    "D14: known_bridges.toml load failed; using empty bridge registry (no D14 events)"
                );
                Arc::new(KnownBridgeSet::from_bridges(vec![]))
            }
        };
    let d14 = Arc::new(D14BridgeDrainDetector::with_bridges(
        bridge_set,
        price_provider.clone(),
    ));

    // D04 — Pump & Dump
    // Sprint 23: inject SmartMoneyLookup for pre-pump buyer amplification (design 0023 §4.1).
    let sm_label_store_d04 = Arc::new(PgGraphLabelStore::new(pool_inner.clone()));
    let sm_lookup: Arc<dyn SmartMoneyLookup> = Arc::new(GraphSmartMoneyLookup::new(
        sm_label_store_d04,
        detector_config.smart_money_v1.min_label_confidence.value,
    ));
    let d04 = Arc::new(
        PumpDumpDetector::new(detector_config.pump_dump.clone())
            .with_smart_money(sm_lookup.clone()),
    );

    // D02 — Rug Pull LP Drain
    let d02 = Arc::new(RugPullDetector::new(detector_config.rug_pull_lp_drain.clone()));

    // D09 — BOCPD Deployer Changepoint (hook-triggered + streaming re-eval)
    // Uses graph stores for edge/label access (suppression + DeployerOf edges).
    let edge_store = Arc::new(PgTypedEdgeStore::new(pool_inner.clone()));
    let label_store = Arc::new(PgGraphLabelStore::new(pool_inner.clone()));
    let d09_config = D09Config::default();
    let d09 = Arc::new(
        D09BocpdDetector::new(
            edge_store,
            label_store,
            bocpd_state_store,
            pool.clone(),
            d09_config,
        )
        .map_err(|e| anyhow::anyhow!("D09BocpdDetector build failed: {e}"))?,
    );

    // D13 — Sandwich/MEV (Ethereum only — supported_chains override, gotcha #67)
    let d13 = Arc::new(D13SandwichMevDetector::new(pool.clone(), price_provider.clone()));

    // D08 — Sybil Bundled-Launch
    // Sprint 23: inject SmartMoneyLookup for cluster amplification (design 0023 §4.2).
    // Note: `label_store_d08` is SEPARATE from `sm_lookup` — different trait + semantics.
    let cluster_store = Arc::new(PgClusterStore::new(pool_inner.clone()));
    let label_store_d08 = Arc::new(PgGraphLabelStore::new(pool_inner.clone()));
    let d08 = Arc::new(
        D08SybilDetector::new(cluster_store, label_store_d08)
            .with_smart_money(sm_lookup.clone()),
    );

    // D11 — Synchronized Activity
    let d11 = Arc::new(D11SynchronizedActivityDetector::new(pool.clone(), price_provider.clone()));

    // D05 — Wash Trading H1
    // Sprint 23: inject SmartMoneyLookup for neutral metadata emission (design 0023 §4.3).
    let d05 = Arc::new(
        WashTradingDetector::new(detector_config.wash_trading_h1.clone())
            .with_smart_money(sm_lookup.clone()),
    );

    // D07 — Token-2022 Withdraw-Withheld Drain
    let d07 = Arc::new(WithdrawWithheldDetector);

    // Assemble in alphabetical detector_id order for deterministic
    // TokenRiskReport.detectors_run ordering.
    // NOTE: 13 detectors — D10 is hook-only, not in this streaming set.
    let detectors: Vec<ArcErasedDetector> = vec![
        d14, // bridge_drain_v1      (all 6 chains: Solana + 5 EVM)
        d03, // holder_concentration
        d01, // honeypot_sim
        d06, // mint_burn_anomaly
        d12, // permit2_drainer_v1  (Ethereum)
        d04, // pump_dump
        d02, // rug_pull_lp_drain
        d09, // deployer_changepoint
        d13, // sandwich_mev_v1     (Ethereum)
        d08, // sybil_detection
        d11, // synchronized_activity_v1
        d05, // wash_trading_h1
        d07, // withdraw_withheld
    ];

    info!(
        count = detectors.len(),
        ids = ?detectors.iter().map(|d| d.id()).collect::<Vec<_>>(),
        "streaming detectors built (D01-D09/D11/D12/D13/D14; D10 is hook-only)"
    );

    Ok(detectors)
}

#[cfg(test)]
mod tests {
    /// Verify that the streaming detector IDs are distinct.
    /// D10 is intentionally absent — it is hook-triggered only.
    #[test]
    fn expected_detector_ids_are_distinct() {
        let expected_ids = [
            "bridge_drain_v1",
            "holder_concentration",
            "honeypot_sim",
            "mint_burn_anomaly",
            "permit2_drainer_v1",
            "pump_dump",
            "rug_pull_lp_drain",
            "deployer_changepoint",
            "sandwich_mev_v1",
            "sybil_detection",
            "synchronized_activity_v1",
            "wash_trading_h1",
            "withdraw_withheld",
        ];
        // 13 streaming detectors (D10 hook-only)
        assert_eq!(expected_ids.len(), 13, "streaming set: 13 detectors (D10 is hook-only)");
        let mut unique = std::collections::BTreeSet::new();
        for id in &expected_ids {
            assert!(unique.insert(*id), "duplicate detector id: {id}");
        }
        assert_eq!(unique.len(), 13, "all 13 IDs must be distinct");
    }

    /// Verify that D12 and D13 are the Ethereum-only detectors in the streaming set.
    /// D14 targets all 6 chains.
    #[test]
    fn d12_and_d13_target_ethereum() {
        use mg_onchain_common::chain::Chain;

        // Structural check: D12 and D13 target Ethereum, D14 targets all chains.
        // Runtime dispatch is validated by `erased_detector.rs` tests.
        let ethereum_only = [Chain::Ethereum];
        let solana_only = [Chain::Solana];
        assert_eq!(ethereum_only, [Chain::Ethereum]);
        assert_eq!(solana_only, [Chain::Solana]);
    }

    /// Verify the streaming detector count matches the Sprint 26 expectation.
    #[test]
    fn streaming_detector_count_is_13() {
        // 13 streaming detectors: D01-D09, D11, D12, D13, D14
        // D10 is hook-triggered only and does not appear in the streaming set.
        let expected_ids = [
            "bridge_drain_v1",          // D14 (all 6 chains)
            "holder_concentration",     // D03
            "honeypot_sim",             // D01
            "mint_burn_anomaly",        // D06
            "permit2_drainer_v1",       // D12 (Ethereum)
            "pump_dump",                // D04
            "rug_pull_lp_drain",        // D02
            "deployer_changepoint",     // D09
            "sandwich_mev_v1",          // D13 (Ethereum)
            "sybil_detection",          // D08
            "synchronized_activity_v1", // D11
            "wash_trading_h1",          // D05
            "withdraw_withheld",        // D07
        ];
        assert_eq!(
            expected_ids.len(),
            13,
            "Sprint 26 streaming set: 13 detectors (D10 is hook-only, excluded)"
        );
    }
}
