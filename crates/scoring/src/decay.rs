//! Exponential decay helper for event-based detector signals.
//!
//! # Formula
//!
//! ```text
//! decay(age_hours, half_life_hours) = exp(- age_hours × ln(2) / half_life_hours)
//! ```
//!
//! At `age = 0h`: decay = 1.0.
//! At `age = half_life_hours`: decay = 0.5.
//! At `age = 2 × half_life_hours`: decay = 0.25.
//!
//! # Rationale
//!
//! Event-based signals (pump volume spike, wash-trading round trips) are tied to
//! specific on-chain activity that becomes less actionable over time. A pump that
//! occurred 120 hours ago is less relevant than one from 6 hours ago.
//!
//! Default `half_life_hours = 72` (3 days). Source: Chainalysis 2025 — average
//! pump-and-dump cycle duration 6.23 days; half of that is ~3 days, meaning an
//! event 3 days old retains 50% weight (still meaningful, no longer dominant).
//! LROO 2026 confirms >95% of rugged tokens complete the drain within 1–3 days.
//!
//! State-based signals always use `decay = 1.0` (not handled here — caller
//! responsibility per [`crate::config::ScoringConfig::is_state_based`]).

/// Compute the exponential decay factor for an event that is `age_hours` old.
///
/// # Arguments
///
/// * `age_hours` — age of the event from `window.end - event.observed_at` in hours.
///   Negative values are clamped to 0.0 (future-dated events treated as current).
/// * `half_life_hours` — half-life parameter from config. Must be > 0.
///
/// # Returns
///
/// A value in `(0.0, 1.0]`. At age=0 returns 1.0 exactly (no floating-point error).
///
/// # Panics
///
/// Does not panic. Invalid `half_life_hours` (≤ 0) produces a logically incorrect
/// result; callers must validate config before calling (see [`ScoringConfig::validate`]).
pub fn exp_decay(age_hours: f64, half_life_hours: f64) -> f64 {
    // Clamp negative ages (future events treated as present).
    let age = age_hours.max(0.0);

    // At age=0, return exactly 1.0 without floating-point rounding.
    if age == 0.0 {
        return 1.0;
    }

    // decay = exp(-age * ln(2) / half_life)
    // = 2^(-age / half_life)
    // Using the 2^x form avoids an extra ln(2) multiplication chain but both are
    // numerically equivalent. We use the exp form for clarity with the spec formula.
    (-(age * std::f64::consts::LN_2) / half_life_hours).exp()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_at_age_zero_is_exactly_one() {
        assert_eq!(exp_decay(0.0, 72.0), 1.0);
    }

    #[test]
    fn decay_at_half_life_is_approximately_half() {
        let d = exp_decay(72.0, 72.0);
        assert!((d - 0.5).abs() < 1e-12, "exp_decay(72,72) = {d}, expected ≈0.5");
    }

    #[test]
    fn decay_at_double_half_life_is_approximately_quarter() {
        let d = exp_decay(144.0, 72.0);
        assert!((d - 0.25).abs() < 1e-12, "exp_decay(144,72) = {d}, expected ≈0.25");
    }

    #[test]
    fn decay_at_one_hour_with_72h_half_life() {
        // Calibration anchor: 1h age used in RAVE and WET probe tables.
        // exp(-1 * ln(2) / 72) ≈ 0.9904
        let d = exp_decay(1.0, 72.0);
        assert!((d - 0.9904).abs() < 1e-4, "exp_decay(1,72) = {d}");
    }

    #[test]
    fn negative_age_clamped_to_zero_returns_one() {
        assert_eq!(exp_decay(-10.0, 72.0), 1.0, "negative age must be clamped to 0");
    }

    #[test]
    fn decay_is_strictly_decreasing() {
        let ages = [0.0_f64, 1.0, 24.0, 48.0, 72.0, 144.0, 720.0];
        let decays: Vec<f64> = ages.iter().map(|&a| exp_decay(a, 72.0)).collect();
        for w in decays.windows(2) {
            assert!(w[0] > w[1], "decay should be strictly decreasing: {w:?}");
        }
    }

    #[test]
    fn decay_result_always_in_range_0_1() {
        for age in [0.0, 1.0, 72.0, 144.0, 10_000.0] {
            let d = exp_decay(age, 72.0);
            assert!(
                (0.0..=1.0).contains(&d),
                "decay {d} out of [0,1] for age={age}"
            );
        }
    }

    #[test]
    fn decay_config_pin_72h_half_life() {
        // Pin the default half-life value per spec §4.
        let half_life = 72.0_f64;
        // At 3 days (the half-life), decay must be 0.5 ± 1e-12.
        assert!((exp_decay(half_life, half_life) - 0.5).abs() < 1e-12);
    }
}
