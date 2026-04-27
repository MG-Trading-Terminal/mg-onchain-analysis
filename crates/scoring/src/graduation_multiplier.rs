//! Graduation-recency amplifier for D02 and D04 detector confidence.
//!
//! # Design
//!
//! Bonding-curve tokens that have recently graduated carry elevated pump-and-dump
//! and rug-pull risk in the post-graduation window. Karbalaii 2025 finds that 70%
//! of pump events have an accumulation phase, which systematically occurs in the
//! hours after a token gains liquidity on a permanent DEX.
//!
//! The multiplier is applied BEFORE the per-event confidence cap (`min(amplified, cap)`).
//! It is multiplicative on the base confidence, not additive — this preserves the
//! relative ordering of signals at low confidence values.
//!
//! # Formula
//!
//! ```text
//! age_hours = (observed_at - graduation_time).num_hours()
//! multiplier = match age_hours {
//!     0..1  → graduation_multiplier_tier1  (default 1.50 — first hour: peak P&D risk)
//!     1..24 → graduation_multiplier_tier2  (default 1.30 — first day)
//!     24..72 → graduation_multiplier_tier3 (default 1.15 — first 3 days)
//!     72..168 → graduation_multiplier_tier4 (default 1.05 — first week)
//!     _     → 1.0                           (mature — no amplification)
//! }
//! amplified = (base_confidence * multiplier).min(cap)
//! ```
//!
//! # Reference
//!
//! Karbalaii 2025 arXiv:2504.15790: "70% of pump events have accumulation phase;
//! 70% of pre-event volume within 1h of announcement."
//! REFERENCES.md: D04/pump_dump — "Pump & dump — structure".
//!
//! # Time-source discipline (gotcha #22 / #28)
//!
//! `observed_at` MUST be from block_time, not `Utc::now()`.
//! The graduation multiplier is called in the detector evaluate path where
//! `ctx.observed_at` is block_time (always — gotcha #28 invariant).
//!
//! # f64 rationale
//!
//! The multiplier operates on `f64` confidence values (same type as `Confidence::value()`).
//! Monetary amounts are not involved; `f64` is appropriate here (CLAUDE.md allows f64
//! for non-monetary detector internals).

use chrono::{DateTime, Utc};

use mg_onchain_token_registry::graduation::GraduationInfo;

/// Tier multiplier breakpoints and their default values.
///
/// All defaults match the Threshold values in `config/detectors.toml`
/// `[graduation_recency]` section. Operators can tune via config.
#[derive(Debug, Clone)]
pub struct GraduationMultiplierConfig {
    /// Multiplier for tokens < 1h post-graduation. Default: 1.50.
    pub tier1_under_1h: f64,
    /// Multiplier for tokens 1h–24h post-graduation. Default: 1.30.
    pub tier2_under_24h: f64,
    /// Multiplier for tokens 24h–72h post-graduation. Default: 1.15.
    pub tier3_under_72h: f64,
    /// Multiplier for tokens 72h–168h (1 week) post-graduation. Default: 1.05.
    pub tier4_under_1w: f64,
    /// Multiplier for tokens > 168h post-graduation. Default: 1.0 (no amplification).
    pub tier5_mature: f64,
}

impl Default for GraduationMultiplierConfig {
    fn default() -> Self {
        Self {
            tier1_under_1h: 1.50,
            tier2_under_24h: 1.30,
            tier3_under_72h: 1.15,
            tier4_under_1w: 1.05,
            tier5_mature: 1.0,
        }
    }
}

/// Compute the graduation-recency multiplier for a token.
///
/// Returns a factor in `[1.0, tier1_under_1h]`. When `graduation_info` is `None`
/// (token has no graduation metadata), returns `1.0` (no amplification).
///
/// # Arguments
///
/// * `graduation_info` — optional graduation metadata from `tokens.metadata_jsonb`.
/// * `observed_at` — observation timestamp from block_time (gotcha #28 — NOT Utc::now()).
/// * `config` — per-tier multiplier values (from `config/detectors.toml`).
///
/// # Panics
///
/// Never panics. Negative duration (observed_at < graduation_time) returns 1.0
/// (conservative — no amplification for future-dated graduations; possible under reorg).
pub fn graduation_recency_multiplier(
    graduation_info: Option<&GraduationInfo>,
    observed_at: DateTime<Utc>,
    config: &GraduationMultiplierConfig,
) -> f64 {
    let Some(info) = graduation_info else {
        return 1.0;
    };

    let duration = observed_at.signed_duration_since(info.graduation_time);
    // Reorg protection: negative age → conservative no-amplification.
    if duration.num_seconds() < 0 {
        return 1.0;
    }

    let age_hours = duration.num_hours();

    if age_hours < 1 {
        config.tier1_under_1h
    } else if age_hours < 24 {
        config.tier2_under_24h
    } else if age_hours < 72 {
        config.tier3_under_72h
    } else if age_hours < 168 {
        config.tier4_under_1w
    } else {
        config.tier5_mature
    }
}

/// Apply the graduation-recency multiplier to a base confidence, clamping to [0.0, cap].
///
/// Formula: `amplified = (base_confidence * multiplier).min(cap)`
///
/// The cap MUST be the per-signal confidence ceiling (0.95 for D04 Signal A,
/// 0.85 for D04 Signal B, 1.0 for D02 Signal A). Applying before the cap
/// respects the signal-level ceiling invariant.
///
/// # Arguments
///
/// * `base_confidence` — raw confidence from the detector signal (before multiplication).
/// * `graduation_info` — optional graduation metadata.
/// * `observed_at` — block_time (gotcha #28).
/// * `config` — per-tier multiplier config.
/// * `cap` — per-signal confidence ceiling (typically 0.95).
pub fn apply_graduation_multiplier(
    base_confidence: f64,
    graduation_info: Option<&GraduationInfo>,
    observed_at: DateTime<Utc>,
    config: &GraduationMultiplierConfig,
    cap: f64,
) -> f64 {
    let multiplier = graduation_recency_multiplier(graduation_info, observed_at, config);
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

    fn graduation_at(t: DateTime<Utc>) -> GraduationInfo {
        GraduationInfo {
            launchpad: Launchpad::PumpFun,
            graduation_time: t,
            graduation_block: 300_000_000,
            graduation_tx: TxHash::solana_from_base58(
                "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF52i4pnkjW8kqxG3dGbwMtm",
            )
            .unwrap(),
            initial_liquidity_usd_at_grad: Decimal::ZERO,
        }
    }

    fn base_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap()
    }

    fn default_cfg() -> GraduationMultiplierConfig {
        GraduationMultiplierConfig::default()
    }

    // ---- graduation_recency_multiplier tests ----

    /// No graduation info → 1.0 multiplier (no amplification).
    #[test]
    fn no_graduation_info_returns_one() {
        let multiplier = graduation_recency_multiplier(None, base_time(), &default_cfg());
        assert_eq!(multiplier, 1.0, "None graduation_info must return 1.0");
    }

    /// Token < 1h post-graduation → tier1 multiplier (1.50 default).
    #[test]
    fn under_1h_returns_tier1_multiplier() {
        let grad_time = base_time();
        let observed = grad_time + Duration::minutes(30); // 30 min after graduation
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.50, "< 1h must return tier1 = 1.50");
    }

    /// Token exactly 1h post-graduation → tier2 multiplier (1.30 default).
    #[test]
    fn exactly_1h_returns_tier2_multiplier() {
        let grad_time = base_time();
        let observed = grad_time + Duration::hours(1);
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.30, "exactly 1h must return tier2 = 1.30");
    }

    /// Token 12h post-graduation → tier2 (1.30 default).
    #[test]
    fn half_day_returns_tier2_multiplier() {
        let grad_time = base_time();
        let observed = grad_time + Duration::hours(12);
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.30, "12h must return tier2 = 1.30");
    }

    /// Token 24h+ post-graduation → tier3 (1.15 default).
    #[test]
    fn one_day_returns_tier3_multiplier() {
        let grad_time = base_time();
        let observed = grad_time + Duration::hours(24);
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.15, "24h must return tier3 = 1.15");
    }

    /// Token 7+ days post-graduation → mature (1.0 default — no amplification).
    #[test]
    fn mature_token_returns_no_amplification() {
        let grad_time = base_time();
        let observed = grad_time + Duration::hours(200); // > 168h
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.0, "mature token must return 1.0 (no amplification)");
    }

    /// Reorg guard: observed_at < graduation_time → 1.0 (conservative).
    #[test]
    fn negative_age_returns_no_amplification() {
        let grad_time = base_time();
        let observed = grad_time - Duration::hours(1); // observed BEFORE graduation
        let info = graduation_at(grad_time);
        let multiplier = graduation_recency_multiplier(Some(&info), observed, &default_cfg());
        assert_eq!(multiplier, 1.0, "negative age (reorg case) must return 1.0");
    }

    // ---- apply_graduation_multiplier tests ----

    /// Cap is respected: amplified confidence <= cap.
    #[test]
    fn cap_is_respected() {
        let grad_time = base_time();
        let observed = grad_time + Duration::minutes(10); // < 1h → 1.50x
        let info = graduation_at(grad_time);
        // base = 0.70 * 1.50 = 1.05 → capped at 0.95
        let result = apply_graduation_multiplier(0.70, Some(&info), observed, &default_cfg(), 0.95);
        assert_eq!(result, 0.95, "amplified confidence must be capped at 0.95");
    }

    /// No graduation → confidence unchanged.
    #[test]
    fn no_graduation_no_change() {
        let result = apply_graduation_multiplier(0.70, None, base_time(), &default_cfg(), 0.95);
        assert_eq!(result, 0.70, "no graduation must leave confidence unchanged");
    }

    /// Amplification when fresh (< 1h): 0.60 * 1.50 = 0.90 (below 0.95 cap).
    #[test]
    fn fresh_graduation_amplifies_threshold_confidence() {
        let grad_time = base_time();
        let observed = grad_time + Duration::minutes(45);
        let info = graduation_at(grad_time);
        let result = apply_graduation_multiplier(0.60, Some(&info), observed, &default_cfg(), 0.95);
        // 0.60 * 1.50 = 0.90 < 0.95 cap
        assert!(
            (result - 0.90).abs() < 1e-10,
            "fresh graduation: 0.60 * 1.50 = 0.90; got {result}"
        );
    }
}
