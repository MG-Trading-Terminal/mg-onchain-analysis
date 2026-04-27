//! `ScoringConfig` — all scoring parameters, loaded from `config/scoring.toml`.
//!
//! Every parameter follows the `{ value, rationale, refs }` three-key convention
//! established in `config/detectors.toml` (P2-4). All numeric values are deserialized
//! from that keyed struct into the flat Rust fields below.
//!
//! # Validation
//!
//! [`ScoringConfig::validate`] is called after deserialization.  It asserts that
//! `detector_weights` sums to `1.0 ± 1e-3` (spec §9).  Any other out-of-range
//! value also produces an error rather than silently producing a nonsensical score.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// TOML helper — every leaf parameter is wrapped in { value, ... }
// ---------------------------------------------------------------------------

/// A single TOML parameter: `{ value = N, rationale = "...", refs = [...] }`.
/// We only care about `value` at runtime; `rationale` and `refs` are advisory.
#[derive(Debug, Clone, Deserialize)]
struct Param<T> {
    value: T,
}

// ---------------------------------------------------------------------------
// Top-level TOML shape
// ---------------------------------------------------------------------------

/// Raw TOML deserialization target.  All leaves are [`Param<T>`] to match the
/// `{ value, rationale, refs }` file convention.
#[derive(Debug, Deserialize)]
struct RawConfig {
    detector_weights: RawDetectorWeights,
    decay_half_life_hours: Param<f64>,
    state_based_detectors: Vec<String>,
    jup_strict_multiplier: Param<f64>,
    jup_verified_multiplier: Param<f64>,
    established_protocol_multiplier: Param<f64>,
    token_age: RawTokenAge,
    inconclusive_floor: Param<f64>,
    evidence_highlight_count: Param<u64>,
}

#[derive(Debug, Deserialize)]
struct RawDetectorWeights {
    honeypot_sim: Param<f64>,
    rug_pull_lp_drain: Param<f64>,
    holder_concentration: Param<f64>,
    pump_dump: Param<f64>,
    wash_trading_h1: Param<f64>,
    mint_burn_anomaly: Param<f64>,
    withdraw_withheld_drain: Param<f64>,
}

#[derive(Debug, Deserialize)]
struct RawTokenAge {
    young_cutoff_days: u32,
    mature_cutoff_days: u32,
    young_multiplier: Param<f64>,
    mature_multiplier: Param<f64>,
}

// ---------------------------------------------------------------------------
// Public config types
// ---------------------------------------------------------------------------

/// Per-detector importance weights for the aggregation formula.
///
/// Must sum to 1.0 (validated at load time — see [`ScoringConfig::from_toml`]).
/// Sum tolerance: ±0.001 per spec §9.
///
/// Weights are calibrated against RAVE (target ≈0.77 post-P6-0) and WET (target ≈0.29
/// post-P6-0) probes. Sprint 5 weights: D03=0.35, D04=0.35 (sum 0.70 without D07).
/// Sprint 6 P6-0 rebalance: D03=0.32, D04=0.32, D07=0.06 (GAP-SCORE-01 closure).
/// Source: `research/token-probes/rave-FeqiF7TE.md §3`, `wet-WETZjtp.md §3`,
///         `SESSION-KICKOFF.md §P6-0`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct DetectorWeights {
    pub honeypot_sim: f64,
    pub rug_pull_lp_drain: f64,
    pub holder_concentration: f64,
    pub pump_dump: f64,
    pub wash_trading_h1: f64,
    pub mint_burn_anomaly: f64,
    /// D07 Token-2022 Withdraw-Withheld Drain. Added P6-0 (GAP-SCORE-01 closure).
    /// Weight 0.06 carved from D03+D04 (0.35→0.32 each). See SESSION-KICKOFF.md §P6-0.
    pub withdraw_withheld_drain: f64,
}

impl DetectorWeights {
    /// Sum of all weights; must be 1.0 ± 0.001.
    pub fn sum(&self) -> f64 {
        self.honeypot_sim
            + self.rug_pull_lp_drain
            + self.holder_concentration
            + self.pump_dump
            + self.wash_trading_h1
            + self.mint_burn_anomaly
            + self.withdraw_withheld_drain
    }

    /// Look up the weight for a given detector ID.
    ///
    /// Returns `None` for unknown detector IDs — callers treat unknown detectors
    /// as weight 0 (they contribute evidence but not to the weighted sum formula).
    pub fn for_detector(&self, detector_id: &str) -> Option<f64> {
        match detector_id {
            "honeypot_sim" | "honeypot_sim_static" => Some(self.honeypot_sim),
            "rug_pull_lp_drain" | "rug_pull_lp_drain_latent" => Some(self.rug_pull_lp_drain),
            "holder_concentration" => Some(self.holder_concentration),
            "pump_dump" => Some(self.pump_dump),
            "wash_trading_h1" => Some(self.wash_trading_h1),
            "mint_burn_anomaly" | "mint_burn_anomaly_static" => Some(self.mint_burn_anomaly),
            // D07 — added P6-0 (GAP-SCORE-01 closure). Event-based; decays normally.
            "withdraw_withheld_drain" => Some(self.withdraw_withheld_drain),
            _ => None,
        }
    }

    /// All known canonical detector IDs (base names used for per_detector map keys).
    pub fn canonical_ids() -> &'static [&'static str] {
        &[
            "honeypot_sim",
            "rug_pull_lp_drain",
            "holder_concentration",
            "pump_dump",
            "wash_trading_h1",
            "mint_burn_anomaly",
            // D07 — added P6-0 (GAP-SCORE-01 closure).
            "withdraw_withheld_drain",
        ]
    }
}

/// Token-age attenuation piecewise-linear function parameters.
///
/// Set `young_multiplier == mature_multiplier == 1.0` to disable (the default per
/// spec §5 A4 — calibration shows WET at 137 days already hits the target range
/// without age discounting).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct TokenAgeAttenuationConfig {
    /// Age < this value → `young_multiplier` applied (no discount for new tokens).
    pub young_cutoff_days: u32,
    /// Age ≥ this value → `mature_multiplier` applied.
    pub mature_cutoff_days: u32,
    /// Multiplier for tokens younger than `young_cutoff_days`. Default 1.0 (no discount).
    /// DO NOT lower below 1.0 — new tokens are the highest-risk category.
    pub young_multiplier: f64,
    /// Multiplier for tokens older than `mature_cutoff_days`. Default 1.0 (disabled).
    /// Set to 0.75 to apply a 25% discount for tokens > 365 days.
    pub mature_multiplier: f64,
}

impl TokenAgeAttenuationConfig {
    /// Compute the age multiplier for a token of `age_days`.
    ///
    /// Returns 1.0 if age is unknown (`None`) — no discount is earned without proof of maturity.
    ///
    /// Piecewise:
    /// - `age < young_cutoff` → `young_multiplier`
    /// - `young_cutoff ≤ age < mature_cutoff` → linear interpolation
    /// - `age ≥ mature_cutoff` → `mature_multiplier`
    pub fn multiplier_for_age(&self, age_days: Option<f64>) -> f64 {
        let age = match age_days {
            Some(a) => a,
            None => return 1.0, // Unknown age → no discount (conservative)
        };

        if age < self.young_cutoff_days as f64 {
            return self.young_multiplier;
        }

        if age >= self.mature_cutoff_days as f64 {
            return self.mature_multiplier;
        }

        // Linear interpolation in the middle band.
        let span = (self.mature_cutoff_days - self.young_cutoff_days) as f64;
        let t = (age - self.young_cutoff_days as f64) / span;
        self.young_multiplier + t * (self.mature_multiplier - self.young_multiplier)
    }
}

/// Full scoring configuration.
///
/// Loaded from `config/scoring.toml` via [`ScoringConfig::from_toml`].
/// Validated at load time: `detector_weights` must sum to 1.0 ± 0.001.
///
/// The config is stored in [`crate::TokenRiskReport::config_snapshot`] for
/// reproducibility auditing — every report carries the exact config that produced it.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ScoringConfig {
    /// Per-detector importance weights. Sum must be 1.0 ± 0.001.
    pub detector_weights: DetectorWeights,

    /// Exponential decay half-life in hours for event-based signals.
    /// State-based signals always use decay = 1.0 (ignores this value).
    pub decay_half_life_hours: f64,

    /// Detector IDs classified as state-based (decay = 1.0 always).
    /// Any detector_id NOT in this list is event-based (decays with half-life above).
    pub state_based_detectors: Vec<String>,

    /// Multiplicative score reduction for `jup_strict == true` tokens. Default 0.30.
    pub jup_strict_multiplier: f64,

    /// Multiplicative score reduction for `jup_verified == true && !jup_strict` tokens.
    /// Default 0.60.
    pub jup_verified_multiplier: f64,

    /// Multiplicative score reduction when `is_established_protocol()` is true.
    /// Default 0.50.
    pub established_protocol_multiplier: f64,

    /// Token-age attenuation. Both multipliers default to 1.0 (disabled).
    pub token_age: TokenAgeAttenuationConfig,

    /// Events with `confidence < inconclusive_floor` are classified as inconclusive.
    /// Default 0.30 per spec OQ3 resolution.
    pub inconclusive_floor: f64,

    /// Maximum entries in `TokenRiskReport::top_evidence`. Default 5.
    pub evidence_highlight_count: usize,
}

impl ScoringConfig {
    /// Load and validate from a TOML string.
    ///
    /// Returns `Err` if:
    /// - The TOML is malformed.
    /// - `detector_weights` do not sum to 1.0 ± 0.001.
    /// - Any multiplier is outside (0.0, 1.0].
    pub fn from_toml(toml_src: &str) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(toml_src).context("failed to parse scoring.toml")?;

        let cfg = Self {
            detector_weights: DetectorWeights {
                honeypot_sim: raw.detector_weights.honeypot_sim.value,
                rug_pull_lp_drain: raw.detector_weights.rug_pull_lp_drain.value,
                holder_concentration: raw.detector_weights.holder_concentration.value,
                pump_dump: raw.detector_weights.pump_dump.value,
                wash_trading_h1: raw.detector_weights.wash_trading_h1.value,
                mint_burn_anomaly: raw.detector_weights.mint_burn_anomaly.value,
                withdraw_withheld_drain: raw.detector_weights.withdraw_withheld_drain.value,
            },
            decay_half_life_hours: raw.decay_half_life_hours.value,
            state_based_detectors: raw.state_based_detectors,
            jup_strict_multiplier: raw.jup_strict_multiplier.value,
            jup_verified_multiplier: raw.jup_verified_multiplier.value,
            established_protocol_multiplier: raw.established_protocol_multiplier.value,
            token_age: TokenAgeAttenuationConfig {
                young_cutoff_days: raw.token_age.young_cutoff_days,
                mature_cutoff_days: raw.token_age.mature_cutoff_days,
                young_multiplier: raw.token_age.young_multiplier.value,
                mature_multiplier: raw.token_age.mature_multiplier.value,
            },
            inconclusive_floor: raw.inconclusive_floor.value,
            evidence_highlight_count: raw.evidence_highlight_count.value as usize,
        };

        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate invariants. Called by [`from_toml`] and useful for programmatic
    /// construction in tests.
    pub fn validate(&self) -> Result<()> {
        let sum = self.detector_weights.sum();
        if (sum - 1.0_f64).abs() > 1e-3 {
            bail!(
                "detector_weights sum {sum:.6} is not within ±0.001 of 1.0; \
                 re-derive weights from calibration probes before deploying"
            );
        }

        if self.decay_half_life_hours <= 0.0 {
            bail!("decay_half_life_hours must be > 0; got {}", self.decay_half_life_hours);
        }

        if self.jup_strict_multiplier <= 0.0 || self.jup_strict_multiplier > 1.0 {
            bail!(
                "jup_strict_multiplier must be in (0, 1]; got {}",
                self.jup_strict_multiplier
            );
        }

        if self.jup_verified_multiplier <= 0.0 || self.jup_verified_multiplier > 1.0 {
            bail!(
                "jup_verified_multiplier must be in (0, 1]; got {}",
                self.jup_verified_multiplier
            );
        }

        if self.established_protocol_multiplier <= 0.0
            || self.established_protocol_multiplier > 1.0
        {
            bail!(
                "established_protocol_multiplier must be in (0, 1]; got {}",
                self.established_protocol_multiplier
            );
        }

        if self.inconclusive_floor < 0.0 || self.inconclusive_floor > 1.0 {
            bail!("inconclusive_floor must be in [0, 1]; got {}", self.inconclusive_floor);
        }

        Ok(())
    }

    /// Construct the default calibrated config programmatically.
    ///
    /// Matches `config/scoring.toml` exactly.  Used in tests and when no config file
    /// is available.  The `validate()` call is intentional — the defaults MUST pass.
    pub fn default_calibrated() -> Self {
        // P6-0 (GAP-SCORE-01 closure): D03+D04 rebalanced 0.35→0.32 each to accommodate
        // D07=0.06. Sum: 0.015+0.20+0.32+0.32+0.07+0.015+0.06 = 1.000.
        let cfg = Self {
            detector_weights: DetectorWeights {
                honeypot_sim: 0.015,
                rug_pull_lp_drain: 0.20,
                holder_concentration: 0.32,
                pump_dump: 0.32,
                wash_trading_h1: 0.07,
                mint_burn_anomaly: 0.015,
                withdraw_withheld_drain: 0.06,
            },
            decay_half_life_hours: 72.0,
            state_based_detectors: vec![
                "honeypot_sim_static".into(),
                "rug_pull_lp_drain_latent".into(),
                "holder_concentration".into(),
                "mint_burn_anomaly_static".into(),
            ],
            jup_strict_multiplier: 0.30,
            jup_verified_multiplier: 0.60,
            established_protocol_multiplier: 0.50,
            token_age: TokenAgeAttenuationConfig {
                young_cutoff_days: 30,
                mature_cutoff_days: 365,
                young_multiplier: 1.0,
                mature_multiplier: 1.0,
            },
            inconclusive_floor: 0.30,
            evidence_highlight_count: 5,
        };
        cfg.validate().expect("default_calibrated config must pass validation");
        cfg
    }

    /// Returns `true` if this detector ID is classified as state-based (no decay).
    pub fn is_state_based(&self, detector_id: &str) -> bool {
        self.state_based_detectors.iter().any(|id| id == detector_id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_calibrated_passes_validation() {
        let cfg = ScoringConfig::default_calibrated();
        assert!((cfg.detector_weights.sum() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn weights_summing_to_below_tolerance_fail_validation() {
        let mut cfg = ScoringConfig::default_calibrated();
        // P6-0: 0.015 + 0.20 + 0.32 + 0.32 + 0.07 + 0.015 + 0.06 = 1.0
        // Remove mint_burn_anomaly entirely → sum = 0.985, outside ±0.001.
        cfg.detector_weights.mint_burn_anomaly = 0.000;
        assert!(cfg.validate().is_err(), "sum≈0.985 should fail validation");
    }

    #[test]
    fn weights_summing_within_tolerance_accepted() {
        let mut cfg = ScoringConfig::default_calibrated();
        // Adjust to sum ≈ 1.0005 (within ±0.001 tolerance): set mint_burn_anomaly to 0.0155
        // 0.015+0.20+0.32+0.32+0.07+0.0155+0.06 = 1.0005
        cfg.detector_weights.mint_burn_anomaly = 0.0155;
        assert!(
            cfg.validate().is_ok(),
            "sum≈1.0005 is within ±0.001 and must pass"
        );
    }

    #[test]
    fn weights_summing_to_above_tolerance_fail_validation() {
        let mut cfg = ScoringConfig::default_calibrated();
        // Raise mint_burn_anomaly to 0.020 → sum = 1.005 — outside ±0.001
        // 0.015+0.20+0.32+0.32+0.07+0.020+0.06 = 1.005
        cfg.detector_weights.mint_burn_anomaly = 0.020;
        assert!(cfg.validate().is_err(), "sum=1.005 should fail validation");
    }

    #[test]
    fn from_toml_with_correct_weights_succeeds() {
        // P6-0: all 7 detector weights present; sum = 1.000.
        let toml = r#"
state_based_detectors = ["honeypot_sim_static", "holder_concentration"]

[detector_weights.honeypot_sim]
value = 0.015

[detector_weights.rug_pull_lp_drain]
value = 0.20

[detector_weights.holder_concentration]
value = 0.32

[detector_weights.pump_dump]
value = 0.32

[detector_weights.wash_trading_h1]
value = 0.07

[detector_weights.mint_burn_anomaly]
value = 0.015

[detector_weights.withdraw_withheld_drain]
value = 0.06

[decay_half_life_hours]
value = 72.0

[jup_strict_multiplier]
value = 0.30

[jup_verified_multiplier]
value = 0.60

[established_protocol_multiplier]
value = 0.50

[token_age]
young_cutoff_days = 30
mature_cutoff_days = 365

[token_age.young_multiplier]
value = 1.0

[token_age.mature_multiplier]
value = 1.0

[inconclusive_floor]
value = 0.30

[evidence_highlight_count]
value = 5
"#;
        let cfg = ScoringConfig::from_toml(toml).expect("valid config");
        assert_eq!(cfg.evidence_highlight_count, 5);
        assert_eq!(cfg.decay_half_life_hours, 72.0);
        assert_eq!(cfg.detector_weights.withdraw_withheld_drain, 0.06);
    }

    #[test]
    fn from_toml_with_bad_weight_sum_fails() {
        // P6-0: all 7 detector weights present; mint_burn_anomaly raised to 0.020 → sum=1.005.
        let toml = r#"
state_based_detectors = []

[detector_weights.honeypot_sim]
value = 0.015

[detector_weights.rug_pull_lp_drain]
value = 0.20

[detector_weights.holder_concentration]
value = 0.32

[detector_weights.pump_dump]
value = 0.32

[detector_weights.wash_trading_h1]
value = 0.07

[detector_weights.mint_burn_anomaly]
value = 0.020

[detector_weights.withdraw_withheld_drain]
value = 0.06

[decay_half_life_hours]
value = 72.0

[jup_strict_multiplier]
value = 0.30

[jup_verified_multiplier]
value = 0.60

[established_protocol_multiplier]
value = 0.50

[token_age]
young_cutoff_days = 30
mature_cutoff_days = 365

[token_age.young_multiplier]
value = 1.0

[token_age.mature_multiplier]
value = 1.0

[inconclusive_floor]
value = 0.30

[evidence_highlight_count]
value = 5
"#;
        let err = ScoringConfig::from_toml(toml).unwrap_err();
        assert!(
            err.to_string().contains("1.005"),
            "error should mention the actual sum: {err}"
        );
    }

    #[test]
    fn token_age_disabled_by_default() {
        let cfg = ScoringConfig::default_calibrated();
        // Both multipliers = 1.0 means age never changes the score.
        assert_eq!(cfg.token_age.young_multiplier, 1.0);
        assert_eq!(cfg.token_age.mature_multiplier, 1.0);
        // For any age, multiplier should be 1.0.
        assert_eq!(cfg.token_age.multiplier_for_age(Some(0.0)), 1.0);
        assert_eq!(cfg.token_age.multiplier_for_age(Some(500.0)), 1.0);
        assert_eq!(cfg.token_age.multiplier_for_age(None), 1.0);
    }

    #[test]
    fn token_age_interpolation() {
        let age_cfg = TokenAgeAttenuationConfig {
            young_cutoff_days: 30,
            mature_cutoff_days: 365,
            young_multiplier: 1.0,
            mature_multiplier: 0.75,
        };
        // Young → 1.0
        assert_eq!(age_cfg.multiplier_for_age(Some(0.0)), 1.0);
        assert_eq!(age_cfg.multiplier_for_age(Some(29.0)), 1.0);
        // Mature → 0.75
        assert_eq!(age_cfg.multiplier_for_age(Some(365.0)), 0.75);
        assert_eq!(age_cfg.multiplier_for_age(Some(1000.0)), 0.75);
        // Mid → interpolated (at 197.5 days = midpoint of 30..365)
        let mid = age_cfg.multiplier_for_age(Some(197.5));
        assert!((mid - 0.875).abs() < 1e-9, "mid={mid}");
        // Unknown → 1.0 (no discount)
        assert_eq!(age_cfg.multiplier_for_age(None), 1.0);
    }

    #[test]
    fn for_detector_weight_lookup() {
        let cfg = ScoringConfig::default_calibrated();
        assert_eq!(cfg.detector_weights.for_detector("honeypot_sim"), Some(0.015));
        assert_eq!(cfg.detector_weights.for_detector("honeypot_sim_static"), Some(0.015));
        // P6-0: holder_concentration rebalanced 0.35→0.32
        assert_eq!(cfg.detector_weights.for_detector("holder_concentration"), Some(0.32));
        // D07 — added P6-0 (GAP-SCORE-01 closure)
        assert_eq!(cfg.detector_weights.for_detector("withdraw_withheld_drain"), Some(0.06));
        assert_eq!(cfg.detector_weights.for_detector("unknown_detector_xyz"), None);
    }

    #[test]
    fn is_state_based_correct() {
        let cfg = ScoringConfig::default_calibrated();
        assert!(cfg.is_state_based("honeypot_sim_static"));
        assert!(cfg.is_state_based("holder_concentration"));
        assert!(!cfg.is_state_based("honeypot_sim")); // event-based variant decays
        assert!(!cfg.is_state_based("pump_dump"));
    }
}
