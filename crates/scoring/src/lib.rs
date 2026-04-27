//! `mg-onchain-scoring` — Token risk score aggregation.
//!
//! Combines detector [`AnomalyEvent`] outputs for a single `(chain, token, window)` tuple
//! into a single [`TokenRiskReport`] using a weighted-sum formula with time decay and
//! attenuation multipliers.
//!
//! # Design (spec `docs/designs/0010-scoring.md`)
//!
//! - **Option 2 weighted-sum aggregation:** `base_score = Σ (w_i × max_confidence_i × decay_i)`.
//!   Weights calibrated against RAVE (target 0.83/Critical) and WET (target 0.31/Medium) probes.
//! - **Exponential time decay** for event-based signals; state-based signals use decay=1.0.
//! - **Attenuation stack:** Jupiter strict/verified, established-protocol, token-age multipliers.
//! - **Pure function:** no I/O, no clock reads (except `observed_at` for `computed_at`),
//!   deterministic given identical inputs.
//!
//! # Module layout
//!
//! ```text
//! crates/scoring/src/
//!   lib.rs        — Public API (this file); re-exports + test_common helpers
//!   config.rs     — ScoringConfig, DetectorWeights, TokenAgeAttenuationConfig
//!   engine.rs     — ScoringEngine::score() — primary entry point
//!   aggregate.rs  — Weighted-sum aggregation: base_score + per-detector breakdown
//!   decay.rs      — exp_decay() pure function
//!   attenuation.rs — Attenuation factor stack (jup/established/age)
//!   evidence.rs   — rank_top_evidence() — EvidenceHighlight ranking
//!   coverage.rs   — CoverageReport construction
//!   types.rs      — TokenRiskReport, DetectorScore, EvidenceHighlight, SignalCounts, etc.
//! ```
//!
//! # OQ resolutions (see spec §15)
//!
//! - **OQ1** (max vs avg for multi-event detectors): MAX effective confidence per detector
//!   feeds the aggregation formula. Conservative: worst case for a given detector type.
//!   `weighted_confidence` (average) is in `DetectorScore` for human breakdowns only.
//! - **OQ2** (`SkipReason` typed vs free-form): `SkipReason` is a struct with `detector_id`
//!   and `reason: String`. Typed ID for programmatic routing; free-form reason for human audit.
//! - **OQ3** (`overall_severity` floor): excludes events below `inconclusive_floor` (0.30).
//!   Very-low-confidence events are noise and MUST NOT drive severity escalation.
//! - **OQ4** (state-based detector IDs): detector IDs are in `config/scoring.toml`
//!   `state_based_detectors` list. No compile-time contract enforcement in P5-1;
//!   see DG4 in the spec for the Phase 6 plan.
//! - **OQ5** (action thresholds): NOT in `ScoringConfig`. Consumer config only.
//!   See spec §15 for recommended bands (`>0.7` = block, `0.4–0.7` = review, `<0.4` = proceed).

pub mod aggregate;
pub mod attenuation;
pub mod config;
pub mod coverage;
pub mod decay;
pub mod engine;
pub mod evidence;
pub mod graduation_multiplier;
pub mod types;

// Re-export primary public types at crate root.
pub use config::{DetectorWeights, ScoringConfig, TokenAgeAttenuationConfig};
pub use engine::ScoringEngine;
pub use graduation_multiplier::{
    GraduationMultiplierConfig, apply_graduation_multiplier, graduation_recency_multiplier,
};
pub use types::{
    CoverageReport, DetectorScore, EvidenceHighlight, SignalCounts, SkipReason, TokenRiskReport,
};

// ---------------------------------------------------------------------------
// Test helpers (available to all sub-modules via #[cfg(test)])
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests_common {
    use chrono::{DateTime, Utc};
    use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
    use mg_onchain_common::chain::{Address, BlockRef, Chain};
    use mg_onchain_common::token::{JupiterVerification, TokenMeta};
    use rust_decimal::Decimal;

    /// Construct a minimal `AnomalyEvent` for testing.
    ///
    /// All fields not specified are set to safe defaults. `observed_at` is `Utc::now()`
    /// — callers that need precise age control should use [`make_event_at`] instead.
    pub fn make_event(detector_id: &str, confidence: f64, severity: Severity) -> AnomalyEvent {
        make_event_at(detector_id, confidence, severity, Utc::now())
    }

    /// Construct a minimal `AnomalyEvent` with a specific `observed_at` timestamp.
    ///
    /// Use this when testing time-decay or age-dependent logic.
    pub fn make_event_at(
        detector_id: &str,
        confidence: f64,
        severity: Severity,
        observed_at: DateTime<Utc>,
    ) -> AnomalyEvent {
        let token =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let block = BlockRef::new(Chain::Solana, 300_000_000);
        AnomalyEvent {
            detector_id: detector_id.to_string(),
            token,
            chain: Chain::Solana,
            confidence: Confidence::new(confidence).expect("test confidence in [0,1]"),
            severity,
            evidence: Evidence::new(),
            observed_at,
            window: (block, block),
            ingested_at: Utc::now(),
        }
    }

    /// Construct a minimal `TokenMeta` for testing.
    ///
    /// `jup_verified` and `jup_strict` control the Jupiter verification flags.
    /// `rugcheck_score` controls the RugCheck normalised score (None = unknown → defaults to 100).
    ///
    /// The mint address used (`So11111111111111111111111111111111111111112` = wSOL) is NOT in
    /// `KNOWN_PROTOCOL_MINTS` and has `rugcheck_score=50` by default, so `is_established_protocol`
    /// returns false unless the caller explicitly sets jup flags or score.
    pub fn make_meta(jup_verified: bool, jup_strict: bool, rugcheck_score: Option<u32>) -> TokenMeta {
        let mint =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        TokenMeta {
            mint,
            chain: Chain::Solana,
            symbol: None,
            name: None,
            decimals: 6,
            token_program: None,
            total_supply_raw: 1_000_000_000_000,
            circulating_supply_raw: None,
            mint_authority: None,
            freeze_authority: None,
            creator: None,
            creator_balance_raw: 0,
            transfer_fee: None,
            permanent_delegate: None,
            transfer_hook_program: None,
            non_transferable: false,
            confidential_transfer: false,
            top_holders: vec![],
            total_holders: 1_000,
            markets: vec![],
            total_market_liquidity_usd: Decimal::new(50_000, 0),
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            rugged: false,
            verification: JupiterVerification { jup_verified, jup_strict },
            rugcheck_score,
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: Utc::now(),
        }
    }
}
