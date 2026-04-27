//! Graduation-recency amplifier for D02 and D04 confidence.
//!
//! # Design
//!
//! This module is the detector-side half of the graduation-recency amplification.
//! The scoring-side version (which operates on fully-composed `AnomalyEvent` outputs)
//! lives in `crates/scoring/src/graduation_multiplier.rs`.
//!
//! Detectors apply this amplifier INSIDE their `evaluate()` function to the raw
//! confidence value before emitting the `AnomalyEvent`. This is consistent with
//! the smart-money amplifier pattern (Sprint 23, `smart_money_amplifier.rs`).
//!
//! # Formula
//!
//! ```text
//! age_hours = (observed_at - graduation_time).num_hours()
//! multiplier = match age_hours {
//!     0..1   → tier1_under_1h   (default 1.50)
//!     1..24  → tier2_under_24h  (default 1.30)
//!     24..72 → tier3_under_72h  (default 1.15)
//!     72..168 → tier4_under_1w  (default 1.05)
//!     _      → 1.0              (mature, no amplification)
//! }
//! amplified = (base_confidence * multiplier).min(cap)
//! ```
//!
//! # Reference
//!
//! Karbalaii 2025 arXiv:2504.15790: "70% of pump events have accumulation phase"
//! REFERENCES.md entry: D04/pump_dump "Pump & dump — structure"
//!
//! # Time-source discipline (gotcha #22/#28)
//!
//! `observed_at` MUST be `ctx.observed_at` (derived from block_time).
//! This module never calls `Utc::now()`.

use chrono::{DateTime, Utc};

use mg_onchain_token_registry::graduation::GraduationInfo;

// Default tier multipliers — mirrored in `config/detectors.toml [graduation_recency]`.
// These are NOT hardcoded thresholds — they are DEFAULT values that operators can
// override via TOML. The config fields are on D02/D04 config structs.
const DEFAULT_TIER1: f64 = 1.50; // < 1h
const DEFAULT_TIER2: f64 = 1.30; // 1h–24h
const DEFAULT_TIER3: f64 = 1.15; // 24h–72h
const DEFAULT_TIER4: f64 = 1.05; // 72h–168h
const DEFAULT_TIER5: f64 = 1.0;  // >= 168h (no amplification)

/// Per-tier multiplier config, read from detector config at evaluate time.
///
/// Default values match `config/detectors.toml [graduation_recency]`.
/// Operators tune via TOML; detectors pass the config-sourced values here.
#[derive(Debug, Clone, Copy)]
pub struct GraduationAmplifierTiers {
    pub tier1_under_1h: f64,
    pub tier2_under_24h: f64,
    pub tier3_under_72h: f64,
    pub tier4_under_1w: f64,
    pub tier5_mature: f64,
}

impl Default for GraduationAmplifierTiers {
    fn default() -> Self {
        Self {
            tier1_under_1h: DEFAULT_TIER1,
            tier2_under_24h: DEFAULT_TIER2,
            tier3_under_72h: DEFAULT_TIER3,
            tier4_under_1w: DEFAULT_TIER4,
            tier5_mature: DEFAULT_TIER5,
        }
    }
}

/// Compute the graduation-recency multiplier.
///
/// Returns a factor in `[1.0, tier1_under_1h]`. When `graduation_info` is `None`,
/// returns `1.0` (no amplification). Negative duration (reorg edge case) returns `1.0`.
///
/// # Arguments
///
/// * `graduation_info` — optional `GraduationInfo` from `token.graduation_metadata`.
/// * `observed_at` — block_time from `ctx.observed_at` (gotcha #28 — NOT Utc::now()).
/// * `tiers` — per-tier multiplier values from detector config.
pub fn graduation_recency_multiplier(
    graduation_info: Option<&GraduationInfo>,
    observed_at: DateTime<Utc>,
    tiers: &GraduationAmplifierTiers,
) -> f64 {
    let Some(info) = graduation_info else {
        return 1.0;
    };

    let duration = observed_at.signed_duration_since(info.graduation_time);
    if duration.num_seconds() < 0 {
        // Reorg protection: observed_at before graduation_time — conservative.
        return 1.0;
    }

    let age_hours = duration.num_hours();
    if age_hours < 1 {
        tiers.tier1_under_1h
    } else if age_hours < 24 {
        tiers.tier2_under_24h
    } else if age_hours < 72 {
        tiers.tier3_under_72h
    } else if age_hours < 168 {
        tiers.tier4_under_1w
    } else {
        tiers.tier5_mature
    }
}

/// Apply graduation-recency amplification to a base confidence value.
///
/// Formula: `(base_confidence * multiplier).min(cap).max(0.0)`
///
/// The `cap` parameter is the per-signal ceiling (e.g. 0.95 for D04 Signal A,
/// 0.85 for D04 Signal B, 1.0 for D02 Signal A). This mirrors the smart-money
/// amplifier's cap semantics from Sprint 23.
pub fn apply_graduation_amplifier(
    base_confidence: f64,
    graduation_info: Option<&GraduationInfo>,
    observed_at: DateTime<Utc>,
    tiers: &GraduationAmplifierTiers,
    cap: f64,
) -> f64 {
    let multiplier = graduation_recency_multiplier(graduation_info, observed_at, tiers);
    (base_confidence * multiplier).min(cap).max(0.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use rust_decimal::Decimal;
    use mg_onchain_common::chain::TxHash;
    use mg_onchain_token_registry::graduation::{GraduationInfo, Launchpad};

    fn grad(t: DateTime<Utc>) -> GraduationInfo {
        GraduationInfo {
            launchpad: Launchpad::PumpFun,
            graduation_time: t,
            graduation_block: 1,
            graduation_tx: TxHash::solana_from_base58(
                "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF52i4pnkjW8kqxG3dGbwMtm",
            )
            .unwrap(),
            initial_liquidity_usd_at_grad: Decimal::ZERO,
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 0, 0, 0).unwrap()
    }

    fn tiers() -> GraduationAmplifierTiers {
        GraduationAmplifierTiers::default()
    }

    #[test]
    fn none_returns_one() {
        assert_eq!(graduation_recency_multiplier(None, t0(), &tiers()), 1.0);
    }

    #[test]
    fn under_1h_returns_tier1() {
        let g = grad(t0());
        let obs = t0() + Duration::minutes(59);
        assert_eq!(graduation_recency_multiplier(Some(&g), obs, &tiers()), 1.50);
    }

    #[test]
    fn at_1h_returns_tier2() {
        let g = grad(t0());
        let obs = t0() + Duration::hours(1);
        assert_eq!(graduation_recency_multiplier(Some(&g), obs, &tiers()), 1.30);
    }

    #[test]
    fn at_24h_returns_tier3() {
        let g = grad(t0());
        let obs = t0() + Duration::hours(24);
        assert_eq!(graduation_recency_multiplier(Some(&g), obs, &tiers()), 1.15);
    }

    #[test]
    fn over_168h_returns_mature() {
        let g = grad(t0());
        let obs = t0() + Duration::hours(200);
        assert_eq!(graduation_recency_multiplier(Some(&g), obs, &tiers()), 1.0);
    }

    #[test]
    fn negative_duration_returns_one() {
        let g = grad(t0());
        let obs = t0() - Duration::hours(1);
        assert_eq!(graduation_recency_multiplier(Some(&g), obs, &tiers()), 1.0);
    }

    /// D04 confidence amplification — 0.60 * 1.50 = 0.90 (< 0.95 cap).
    #[test]
    fn d04_fresh_graduation_amplifies_confidence() {
        let g = grad(t0());
        let obs = t0() + Duration::minutes(30);
        let result = apply_graduation_amplifier(0.60, Some(&g), obs, &tiers(), 0.95);
        assert!((result - 0.90).abs() < 1e-10, "got {result}");
    }

    /// D02 confidence amplification — 0.75 * 1.30 = 0.975 → capped at 1.0.
    #[test]
    fn d02_graduation_amplification_respects_cap() {
        let g = grad(t0());
        let obs = t0() + Duration::hours(12); // tier2 → 1.30x
        // 0.75 * 1.30 = 0.975 > 1.0 not possible, but 0.75 * 1.30 = 0.975 < 1.0
        let result = apply_graduation_amplifier(0.75, Some(&g), obs, &tiers(), 1.0);
        assert!((result - 0.975).abs() < 1e-10, "got {result}");
    }

    /// Cap always respected even when multiplier is large.
    #[test]
    fn cap_always_respected() {
        let g = grad(t0());
        let obs = t0() + Duration::minutes(1); // tier1 → 1.50x
        // 0.80 * 1.50 = 1.20 → must be capped at 0.95
        let result = apply_graduation_amplifier(0.80, Some(&g), obs, &tiers(), 0.95);
        assert_eq!(result, 0.95, "cap must be respected: 0.80 * 1.50 = 1.20 → 0.95");
    }
}
