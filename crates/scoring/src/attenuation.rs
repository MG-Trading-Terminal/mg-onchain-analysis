//! Attenuation stack — post-aggregation score reduction for established/verified tokens.
//!
//! # Design
//!
//! After computing `base_score` via the weighted-sum formula, a series of multiplicative
//! attenuation factors reduce the final `overall_score` for tokens where the prior
//! probability of fraud is lower than the average shitcoin.
//!
//! These are NOT detector-level suppressions. They are a SECOND PASS on top of
//! per-detector `is_established_protocol` suppression that happens inside detectors.
//!
//! # Factor precedence (spec §5)
//!
//! A1 (`jup_strict`) and A2 (`jup_verified`) are mutually exclusive: only one fires
//! per token (whichever applies). A3 (`established_protocol`) and A4 (`token_age`)
//! are independent and stack multiplicatively with the jup factor.
//!
//! ```text
//! jup_factor = jup_strict_multiplier   if jup_strict
//!            = jup_verified_multiplier if jup_verified && !jup_strict
//!            = 1.0                     otherwise
//!
//! overall_score = base_score × jup_factor × established_factor × age_factor
//! ```
//!
//! All factors are in (0, 1]; `overall_score` is clamped to [0, 1] by the caller.

use mg_onchain_common::token::TokenMeta;
use mg_onchain_detectors::is_established_protocol;

use crate::config::ScoringConfig;

/// Result of computing all attenuation factors for a token.
///
/// Stored in [`TokenRiskReport`] for consumer transparency (`base_score` vs `overall_score`).
#[derive(Debug, Clone)]
pub struct AttenuationFactors {
    /// A1 or A2 Jupiter factor (mutually exclusive). 1.0 if neither applies.
    pub jup_factor: f64,
    /// A3 established-protocol factor. 1.0 if token is not an established protocol.
    pub established_factor: f64,
    /// A4 token-age factor. 1.0 if age attenuation is disabled or age is unknown.
    pub age_factor: f64,
    /// Final combined multiplier = jup × established × age.
    pub combined: f64,
}

/// Compute all attenuation factors for a token given its metadata and config.
///
/// # Arguments
///
/// * `meta` — current token state from the token registry.
/// * `config` — scoring configuration with multiplier values.
///
/// # Returns
///
/// [`AttenuationFactors`] with the combined multiplier and individual breakdown.
pub fn compute_attenuation(meta: &TokenMeta, config: &ScoringConfig) -> AttenuationFactors {
    // A1 / A2 — Jupiter listing (mutually exclusive).
    let jup_factor = if meta.verification.jup_strict {
        config.jup_strict_multiplier
    } else if meta.verification.jup_verified {
        config.jup_verified_multiplier
    } else {
        1.0
    };

    // A3 — established protocol (independent from jup factor; both may apply).
    let established_factor = if is_established_protocol(meta) {
        config.established_protocol_multiplier
    } else {
        1.0
    };

    // A4 — token age (disabled by default when both multipliers are 1.0).
    let age_days = meta.detected_at.map(|detected| {
        let now_approx = meta.updated_at;
        let diff = now_approx.signed_duration_since(detected);
        diff.num_seconds() as f64 / 86_400.0
    });
    let age_factor = config.token_age.multiplier_for_age(age_days);

    let combined = jup_factor * established_factor * age_factor;

    AttenuationFactors {
        jup_factor,
        established_factor,
        age_factor,
        combined,
    }
}

/// Apply attenuation to a base score, clamping the result to `[0.0, 1.0]`.
pub fn apply_attenuation(base_score: f64, factors: &AttenuationFactors) -> f64 {
    (base_score * factors.combined).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ScoringConfig, TokenAgeAttenuationConfig};
    use chrono::Utc;
    use mg_onchain_common::chain::{Address, Chain};
    use mg_onchain_common::token::{JupiterVerification, TokenMeta};
    use rust_decimal::Decimal;

    fn make_meta(jup_verified: bool, jup_strict: bool) -> TokenMeta {
        // wSOL: not in KNOWN_PROTOCOL_MINTS; rugcheck_score=50 (above thresholds for
        // established-protocol branches 2/2b). Ensures is_established_protocol=false.
        let mint =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        TokenMeta {
            mint,
            chain: Chain::Solana,
            symbol: None,
            name: None,
            decimals: 6,
            token_program: None,
            total_supply_raw: 0,
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
            total_holders: 0,
            markets: vec![],
            total_market_liquidity_usd: Decimal::ZERO,
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            rugged: false,
            verification: JupiterVerification { jup_verified, jup_strict },
            rugcheck_score: Some(50), // Above all established-protocol score thresholds.
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: Utc::now(),
        }
    }

    fn default_cfg() -> ScoringConfig {
        ScoringConfig::default_calibrated()
    }

    #[test]
    fn no_flags_means_no_attenuation() {
        let meta = make_meta(false, false);
        let factors = compute_attenuation(&meta, &default_cfg());
        assert_eq!(factors.jup_factor, 1.0);
        assert_eq!(factors.established_factor, 1.0);
        assert_eq!(factors.combined, 1.0);
        assert_eq!(apply_attenuation(0.83, &factors), 0.83);
    }

    #[test]
    fn jup_strict_applies_0_30_multiplier() {
        let meta = make_meta(false, true); // jup_strict
        let factors = compute_attenuation(&meta, &default_cfg());
        assert_eq!(factors.jup_factor, 0.30);
        // For RAVE base 0.827: 0.827 × 0.30 ≈ 0.248
        let score = apply_attenuation(0.827, &factors);
        assert!(score < 0.26, "jup_strict should reduce RAVE below 0.26; got {score}");
    }

    #[test]
    fn jup_verified_only_applies_0_60_multiplier() {
        let meta = make_meta(true, false); // jup_verified, not strict
        let factors = compute_attenuation(&meta, &default_cfg());
        assert_eq!(factors.jup_factor, 0.60);
        assert_eq!(factors.established_factor, 1.0); // rugcheck_score=50, no strict
    }

    #[test]
    fn jup_strict_wins_over_verified() {
        // When both would apply (jup_strict=true implies jup_verified=true in practice),
        // only the strict multiplier fires.
        let meta = make_meta(true, true); // both set
        let factors = compute_attenuation(&meta, &default_cfg());
        assert_eq!(factors.jup_factor, 0.30, "jup_strict must win over jup_verified");
    }

    #[test]
    fn established_protocol_stacks_with_jup_strict() {
        // RAY: jup_strict=false, rugcheck=56, BUT in KNOWN_PROTOCOL_MINTS → established.
        let ray_mint =
            Address::parse(Chain::Solana, "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R")
                .unwrap();
        let mut meta = make_meta(false, false);
        meta.mint = ray_mint;
        meta.rugcheck_score = Some(56);

        let factors = compute_attenuation(&meta, &default_cfg());
        assert_eq!(factors.jup_factor, 1.0);
        assert_eq!(
            factors.established_factor, 0.50,
            "RAY in whitelist → established_protocol=true → 0.50"
        );
        assert_eq!(factors.combined, 0.50);
    }

    #[test]
    fn jup_strict_and_established_both_stack() {
        // A token that is jup_strict AND established: both A1 and A3 apply.
        // We use MPLX-like parameters: jup_strict=true → established branch 1 fires too.
        let meta = make_meta(false, true); // jup_strict
        let factors = compute_attenuation(&meta, &default_cfg());
        // rugcheck_score=50, not in whitelist, jup_strict=true →
        // established branch 1 fires (jup_strict implies established)
        assert_eq!(factors.jup_factor, 0.30);
        assert_eq!(
            factors.established_factor, 0.50,
            "jup_strict token is also established_protocol → 0.50 factor"
        );
        assert!((factors.combined - 0.15).abs() < 1e-9, "0.30 × 0.50 = 0.15");
    }

    #[test]
    fn age_factor_disabled_by_default() {
        let meta = make_meta(false, false);
        let cfg = default_cfg();
        let factors = compute_attenuation(&meta, &cfg);
        assert_eq!(factors.age_factor, 1.0, "age attenuation disabled by default");
    }

    #[test]
    fn age_factor_applied_when_enabled() {
        let meta = make_meta(false, false);
        let mut cfg = default_cfg();
        cfg.token_age = TokenAgeAttenuationConfig {
            young_cutoff_days: 30,
            mature_cutoff_days: 365,
            young_multiplier: 1.0,
            mature_multiplier: 0.75,
        };
        // meta.detected_at = None → age unknown → multiplier = 1.0 (conservative)
        let factors = compute_attenuation(&meta, &cfg);
        assert_eq!(factors.age_factor, 1.0);
    }

    #[test]
    fn apply_attenuation_clamps_to_one() {
        // attenuation factor > 1.0 (shouldn't happen with valid config but guard tested)
        let factors = AttenuationFactors {
            jup_factor: 1.0,
            established_factor: 1.0,
            age_factor: 1.0,
            combined: 1.5,
        };
        assert_eq!(apply_attenuation(0.9, &factors), 1.0);
    }

    #[test]
    fn apply_attenuation_clamps_to_zero() {
        let factors = AttenuationFactors {
            jup_factor: 0.0,
            established_factor: 1.0,
            age_factor: 1.0,
            combined: 0.0,
        };
        assert_eq!(apply_attenuation(0.9, &factors), 0.0);
    }
}
