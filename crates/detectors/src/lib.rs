//! `mg-onchain-detectors` — Detector trait and anomaly detector framework.
//!
//! # Module layout
//!
//! ```text
//! crates/detectors/src/
//!   lib.rs          — Re-exports and shared helpers (this file)
//!   detector.rs     — The Detector trait definition
//!   context.rs      — DetectorContext: what a detector reads
//!   error.rs        — DetectorError (thiserror, non_exhaustive)
//!   config.rs       — DetectorConfig, per-detector threshold structs, TOML loader
//!   signals.rs      — Shared signal math: sigmoid(), severity_from_confidence()
//!   rpc.rs          — Re-export of SolanaRpc for detector-layer use
//!   mock.rs         — MockPgRunner, test_utils for unit test injection (cfg(test))
//!   d01_honeypot.rs    — D01 Honeypot detector (full implementation)
//!   d02_rug_pull.rs    — D02 LP Rug-Pull detector (full implementation)
//!   d03_concentration.rs — D03 Holder Concentration Shift detector (full implementation)
//!   d04_pump_dump.rs    — D04 Pump & Dump detector (full implementation)
//!   d05_wash_trading.rs — D05 Wash Trading H1 detector (full implementation)
//!   d06_mint_burn.rs    — D06 Mint/Burn Anomaly detector (full implementation)
//!   d07_withdraw_withheld.rs — D07 Token-2022 Withdraw-Withheld Drain detector (full implementation)
//!   d08_sybil.rs             — D08 Sybil Bundled-Launch detector (full implementation)
//!   d11_synchronized_activity.rs — D11 Synchronized-Activity Clustering detector (full implementation)
//!   d13_sandwich_mev.rs      — D13 Sandwich/MEV detector (full implementation, Ethereum)
//! ```
//!
//! # Evidence key convention
//!
//! All `Evidence::metrics` keys are prefixed with `<detector_id>/` followed by
//! an all-ASCII snake_case metric name. Use the [`evidence_key`] helper to
//! construct keys consistently:
//!
//! ```rust
//! use mg_onchain_detectors::{evidence_key, d01_honeypot::DETECTOR_ID};
//!
//! let key = evidence_key(DETECTOR_ID, "buy_sell_ratio");
//! assert_eq!(key, "honeypot_sim/buy_sell_ratio");
//! ```
//!
//! This convention is enforced at the detector implementation level (code review +
//! the `evidence_key` helper) — not at the `common` type level (frozen).
//!
//! # Determinism
//!
//! All detector output paths must be deterministic:
//! - `Evidence::metrics` is `BTreeMap` (alphabetically ordered) — never `HashMap`.
//! - DB result sets must be ORDER BY'd before use.
//! - No wall-clock reads in detector logic.
//! - See `detector.rs` for the full determinism contract.

pub mod config;
pub mod context;
pub mod d01_honeypot;
pub mod d02_rug_pull;
pub mod d03_concentration;
pub mod d04_pump_dump;
pub mod d05_wash_trading;
pub mod d06_mint_burn;
pub mod d07_withdraw_withheld;
pub mod d08_sybil;
pub mod d09_deployer_changepoint;
pub mod d10_launch_audit;
pub mod d11_synchronized_activity;
pub mod d12_permit2_drainer;
pub mod d13_sandwich_mev;
pub mod d14_bridge_drain;
pub mod detector;
pub mod error;
pub mod mock;
pub mod rpc;
pub mod signals;
// Sprint 23 — smart-money amplification helpers shared across D04/D08/D05.
pub mod smart_money_amplifier;
// Sprint 25 — graduation-recency amplification for D02/D04.
pub mod graduation_amplifier;
pub mod token_status;
// Sprint 25 — LP locker registry (EVM: Unicrypt / Team Finance / TrustSwap).
pub mod lockers;

// Re-export primary types at crate root for ergonomic use by consumers.
pub use config::{AllDetectorConfigs, DetectorConfig, load_detector_config};
pub use context::{DetectorContext, DetectorWindow};
pub use d01_honeypot::HoneypotDetector;
pub use d02_rug_pull::RugPullDetector;
pub use d03_concentration::ConcentrationDetector;
pub use d04_pump_dump::PumpDumpDetector;
pub use d05_wash_trading::WashTradingDetector;
pub use d06_mint_burn::MintBurnAnomalyDetector;
pub use d07_withdraw_withheld::WithdrawWithheldDetector;
pub use d08_sybil::D08SybilDetector;
pub use d11_synchronized_activity::D11SynchronizedActivityDetector;
pub use d12_permit2_drainer::{D12PermitDrainerDetector, KnownDrainerSet};
pub use d13_sandwich_mev::D13SandwichMevDetector;
pub use d14_bridge_drain::{D14BridgeDrainDetector, KnownBridgeSet};
pub use d09_deployer_changepoint::{
    AnomalyEventSink, BocpdHyperparams, BocpdState, BocpdStateStore, CompositeWeights,
    D09BocpdDetector, D09Config, D09IndexerHook, ObservationFeatures, PgAnomalyEventSink,
    PgBocpdStateStore, RunSlot,
};
pub use d10_launch_audit::{
    AnomalyEventSink as D10AnomalyEventSink, D10Config, D10IndexerHook, D10LaunchAuditDetector,
    LaunchAuditResult, TokenRegistry as D10TokenRegistry,
};
pub use detector::Detector;
pub use error::DetectorError;
pub use token_status::is_established_protocol;

// ---------------------------------------------------------------------------
// evidence_key helper
// ---------------------------------------------------------------------------

/// Construct a namespaced `Evidence::metrics` key.
///
/// Format: `{detector_id}/{metric_name}`, where `metric_name` should be
/// all-ASCII snake_case (not enforced at runtime — enforced by code review).
///
/// # Examples
///
/// ```rust
/// use mg_onchain_detectors::evidence_key;
///
/// assert_eq!(evidence_key("honeypot_sim", "buy_sell_ratio"), "honeypot_sim/buy_sell_ratio");
/// assert_eq!(evidence_key("rug_pull_lp_drain", "lp_removed_pct"), "rug_pull_lp_drain/lp_removed_pct");
/// ```
#[inline]
pub fn evidence_key(detector_id: &str, metric_name: &str) -> String {
    format!("{detector_id}/{metric_name}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_key_format_is_correct() {
        assert_eq!(
            evidence_key("honeypot_sim", "buy_sell_ratio"),
            "honeypot_sim/buy_sell_ratio"
        );
        assert_eq!(
            evidence_key("rug_pull_lp_drain", "lp_removed_pct"),
            "rug_pull_lp_drain/lp_removed_pct"
        );
        assert_eq!(
            evidence_key("wash_trading_h1", "round_trip_count"),
            "wash_trading_h1/round_trip_count"
        );
    }

    #[test]
    fn evidence_key_matches_design_doc_examples() {
        // Per design 0003 §Per-Detector Instance Metadata evidence key list.
        assert_eq!(
            evidence_key("honeypot_sim", "freeze_authority_active"),
            "honeypot_sim/freeze_authority_active"
        );
        assert_eq!(
            evidence_key("holder_concentration", "gini_delta_24h"),
            "holder_concentration/gini_delta_24h"
        );
        assert_eq!(
            evidence_key("pump_dump", "fallback_used"),
            "pump_dump/fallback_used"
        );
    }

    #[test]
    fn evidence_uses_btreemap_not_hashmap() {
        // The common::Evidence type uses BTreeMap — this test confirms keys come
        // out in alphabetical order when inserted in non-alphabetical order.
        use mg_onchain_common::anomaly::Evidence;
        use rust_decimal::Decimal;

        let ev = Evidence::new()
            .with_metric(
                evidence_key("honeypot_sim", "transfer_fee_bps"),
                Decimal::ZERO,
            )
            .with_metric(
                evidence_key("honeypot_sim", "buy_sell_ratio"),
                Decimal::new(82, 2),
            )
            .with_metric(
                evidence_key("honeypot_sim", "simulate_paths_tested"),
                Decimal::new(3, 0),
            );

        let mut keys = ev.metrics.keys();
        // BTreeMap sorts alphabetically:
        // "honeypot_sim/buy_sell_ratio" < "honeypot_sim/simulate_paths_tested" < "honeypot_sim/transfer_fee_bps"
        assert_eq!(keys.next().unwrap(), "honeypot_sim/buy_sell_ratio");
        assert_eq!(keys.next().unwrap(), "honeypot_sim/simulate_paths_tested");
        assert_eq!(keys.next().unwrap(), "honeypot_sim/transfer_fee_bps");
    }
}
