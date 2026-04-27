//! Shared signal-computation helpers for all detectors.
//!
//! # Contents
//!
//! - [`sigmoid`] — standard sigmoid function used in confidence normalization.
//! - [`severity_from_confidence`] — DG5 confidence→severity ladder (all detectors).
//! - [`gini_descending`] — Gini coefficient over a descending-sorted balance slice.
//! - [`top_n_pct`] — Top-N percentage (sum of top N / total).
//!
//! # Determinism note
//!
//! All functions here are pure (no I/O, no wall-clock, no RNG). Output is
//! fully determined by inputs. This is required by the Detector trait contract
//! (see `docs/designs/0003-detector-trait.md`).

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use mg_onchain_common::anomaly::Severity;

// ---------------------------------------------------------------------------
// Sigmoid
// ---------------------------------------------------------------------------

/// Standard sigmoid function: `1 / (1 + exp(-x))`.
///
/// Used for confidence normalization in detector formulas (per
/// `docs/designs/0004-detector-01-honeypot.md` §6).
///
/// The output is in `(0.0, 1.0)` exclusive. For typical inputs used in
/// detector formulas (`x` in the range `[-3, 3]`), outputs stay well within
/// `[0.05, 0.95]`.
///
/// # Why f64
///
/// `f64` is the correct type here: this is a probability mapping function,
/// not a monetary amount. Per CLAUDE.md: "NEVER f64 for prices, amounts,
/// supplies, liquidity" — this is none of those.
#[inline]
pub fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ---------------------------------------------------------------------------
// Severity ladder (DG5)
// ---------------------------------------------------------------------------

/// Map a detector confidence value to a [`Severity`] level.
///
/// This is the DG5 confidence → severity ladder, shared by all six MVP
/// detectors. Individual detectors may compute severity differently (e.g. D01
/// takes simulation result into account), but this function provides the
/// default band-based mapping.
///
/// # Bands
///
/// | Confidence      | Severity  |
/// |-----------------|-----------|
/// | `[0.00, 0.20)`  | `Info`    |
/// | `[0.20, 0.40)`  | `Low`     |
/// | `[0.40, 0.60)`  | `Medium`  |
/// | `[0.60, 0.80)`  | `High`    |
/// | `[0.80, 1.00]`  | `Critical`|
///
/// Reference: docs/designs/0004-detector-01-honeypot.md DG5 (pre-authorised).
pub fn severity_from_confidence(c: f64) -> Severity {
    if c < 0.20 {
        Severity::Info
    } else if c < 0.40 {
        Severity::Low
    } else if c < 0.60 {
        Severity::Medium
    } else if c < 0.80 {
        Severity::High
    } else {
        Severity::Critical
    }
}

// ---------------------------------------------------------------------------
// Gini coefficient and top-N percentage (D03 helpers)
// ---------------------------------------------------------------------------

/// Gini coefficient over a balance slice (order-independent).
///
/// Uses the standard 1-based sorted-array formula:
///
/// ```text
/// Gini = (2 * Σ_{i=1}^{n} i * x_i) / (n * Σ x_i)  -  (n + 1) / n
/// ```
///
/// where `x_i` are balances sorted in **ascending** order (index 1 = smallest).
/// The input slice is sorted ascending internally; the `_descending` suffix is
/// a reminder that the caller may pass a descending-ordered Vec (from ORDER BY
/// balance DESC) and this function will re-sort internally.
///
/// Calibration: `[0, 0, 0, 100]` → 0.75. `[25, 25, 25, 25]` → 0.0.
///
/// # Returns
///
/// `Decimal::ZERO` if `balances.len() < 2` (population too small).
/// Otherwise, a value in `[0, 1]` where 0 = perfect equality, 1 = perfect inequality.
///
/// # Precision
///
/// Uses `Decimal` arithmetic throughout per `CLAUDE.md` §no-f64-for-money.
/// The formula does not involve prices or monetary amounts, but we use `Decimal`
/// for consistency and to avoid precision surprises when evidence values are
/// persisted to Postgres `NUMERIC` columns.
///
/// # References
///
/// Brown (2023) §3; formula derivation follows the standard Lorenz curve
/// definition (see https://eprint.iacr.org/2023/1493.pdf).
pub fn gini_descending(balances: &[Decimal]) -> Decimal {
    let n = balances.len();
    if n < 2 {
        return Decimal::ZERO;
    }

    // Sort ascending (smallest first) — Gini formula requires ascending order.
    let mut sorted = balances.to_vec();
    sorted.sort_unstable();

    let total: Decimal = sorted.iter().copied().sum();
    if total == Decimal::ZERO {
        return Decimal::ZERO;
    }

    // Standard 1-based ascending formula:
    //   Gini = (2 * Σ_{i=1}^{n} i * x_i) / (n * total) - (n+1)/n
    //
    // Using 0-based iteration: i_1based = i_0based + 1
    //
    // Calibration: [0,0,0,100] ascending → Σ i*x_i = 4*100 = 400
    //   Gini = (2*400)/(4*100) - 5/4 = 2.0 - 1.25 = 0.75 ✓
    let n_dec = Decimal::from_usize(n).unwrap_or(Decimal::ONE);
    let n_plus_1 = Decimal::from_usize(n + 1).unwrap_or(n_dec + Decimal::ONE);
    let mut weighted_sum = Decimal::ZERO;
    for (i, &val) in sorted.iter().enumerate() {
        let i1 = Decimal::from_usize(i + 1).unwrap_or(Decimal::ONE); // 1-based
        weighted_sum += i1 * val;
    }

    let two = Decimal::from_u32(2).unwrap_or(Decimal::ONE);
    let gini = (two * weighted_sum) / (n_dec * total) - n_plus_1 / n_dec;
    // Clamp to [0, 1] to handle any rounding artifacts.
    gini.max(Decimal::ZERO).min(Decimal::ONE)
}

/// Top-N percentage (inclusive): `sum(top N balances) / sum(all balances)`.
///
/// The caller passes `balances_desc` ordered **descending** (largest first).
/// If `n >= balances_desc.len()`, returns 1.0 (all holders are in the top-N).
/// If `balances_desc` is empty, returns `Decimal::ZERO`.
///
/// # References
///
/// Used for Signal 2 (`top10_pct_now`) and Signal 3 (`absolute_top10_ceiling`)
/// in D03. Definition matches the RugCheck "top 10 holder percent" metric.
pub fn top_n_pct(balances_desc: &[Decimal], n: usize) -> Decimal {
    if balances_desc.is_empty() {
        return Decimal::ZERO;
    }

    let total: Decimal = balances_desc.iter().copied().sum();
    if total == Decimal::ZERO {
        return Decimal::ZERO;
    }

    let top_n_end = n.min(balances_desc.len());
    let top_sum: Decimal = balances_desc[..top_n_end].iter().copied().sum();

    (top_sum / total).min(Decimal::ONE)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- sigmoid ---

    #[test]
    fn sigmoid_zero_is_half() {
        let s = sigmoid(0.0);
        assert!((s - 0.5).abs() < 1e-12, "sigmoid(0) must be 0.5, got {s}");
    }

    #[test]
    fn sigmoid_large_positive_approaches_one() {
        let s = sigmoid(100.0);
        assert!(s > 0.999, "sigmoid(100) must be near 1.0, got {s}");
    }

    #[test]
    fn sigmoid_large_negative_approaches_zero() {
        let s = sigmoid(-100.0);
        assert!(s < 0.001, "sigmoid(-100) must be near 0.0, got {s}");
    }

    #[test]
    fn sigmoid_symmetry() {
        let x = 1.5;
        let a = sigmoid(x);
        let b = sigmoid(-x);
        assert!(
            (a + b - 1.0).abs() < 1e-12,
            "sigmoid(x) + sigmoid(-x) must equal 1.0"
        );
    }

    /// Cross-check the specific values cited in the analyst spec §6:
    /// raw=0.25 (freeze only) → sigmoid(-0.55) ≈ 0.37
    #[test]
    fn sigmoid_freeze_only_raw_score() {
        // raw=0.25 (freeze only) → sigmoid(0.25/0.55 - 1.0) = sigmoid(-0.545...)
        let raw = 0.25_f64;
        let x = raw / 0.55 - 1.0;
        let s = sigmoid(x);
        // Spec says ≈ 0.37; check within 0.02 band
        assert!(
            (s - 0.37).abs() < 0.02,
            "freeze_only static_conf should be ≈0.37, got {s:.4}"
        );
    }

    /// raw=0 → x = 0/0.55 - 1.0 = -1.0 → sigmoid(-1.0) ≈ 0.27 per spec
    ///
    /// The formula `sigmoid(raw / 0.55 - 1.0)` with `raw=0` gives argument `-1.0`
    /// (not `-1.818` as one might naively assume — `0/0.55 = 0.0`, not `1/0.55`).
    /// Background confidence for a token with zero signals is ≈0.27 (Severity::Low).
    #[test]
    fn sigmoid_zero_raw_score() {
        let x = 0.0_f64 / 0.55 - 1.0; // = 0 - 1 = -1.0
        let s = sigmoid(x);
        assert!(
            (s - 0.269).abs() < 0.002,
            "zero raw score should give background ≈0.269, got {s:.4}"
        );
    }

    // --- severity_from_confidence ---

    #[test]
    fn severity_band_info() {
        assert_eq!(severity_from_confidence(0.00), Severity::Info);
        assert_eq!(severity_from_confidence(0.15), Severity::Info);
        assert_eq!(severity_from_confidence(0.19), Severity::Info);
    }

    #[test]
    fn severity_band_low() {
        assert_eq!(severity_from_confidence(0.20), Severity::Low);
        assert_eq!(severity_from_confidence(0.30), Severity::Low);
        assert_eq!(severity_from_confidence(0.39), Severity::Low);
    }

    #[test]
    fn severity_band_medium() {
        assert_eq!(severity_from_confidence(0.40), Severity::Medium);
        assert_eq!(severity_from_confidence(0.50), Severity::Medium);
        assert_eq!(severity_from_confidence(0.59), Severity::Medium);
    }

    #[test]
    fn severity_band_high() {
        assert_eq!(severity_from_confidence(0.60), Severity::High);
        assert_eq!(severity_from_confidence(0.70), Severity::High);
        assert_eq!(severity_from_confidence(0.79), Severity::High);
    }

    #[test]
    fn severity_band_critical() {
        assert_eq!(severity_from_confidence(0.80), Severity::Critical);
        assert_eq!(severity_from_confidence(0.85), Severity::Critical);
        assert_eq!(severity_from_confidence(1.00), Severity::Critical);
    }

    /// Key band boundaries from the briefing: 0.15→Info, 0.50→Medium, 0.85→Critical
    #[test]
    fn severity_briefing_examples() {
        assert_eq!(severity_from_confidence(0.15), Severity::Info);
        assert_eq!(severity_from_confidence(0.50), Severity::Medium);
        assert_eq!(severity_from_confidence(0.85), Severity::Critical);
    }

    // --- gini_descending ---

    /// Single element → Decimal::ZERO (population too small).
    #[test]
    fn gini_single_element_returns_zero() {
        let balances = vec![Decimal::new(100, 0)];
        assert_eq!(gini_descending(&balances), Decimal::ZERO);
    }

    /// Empty slice → Decimal::ZERO.
    #[test]
    fn gini_empty_returns_zero() {
        assert_eq!(gini_descending(&[]), Decimal::ZERO);
    }

    /// Perfect equality [25, 25, 25, 25] → Gini = 0.0.
    ///
    /// Each holder has equal balance; the Lorenz curve coincides with
    /// the line of equality. Gini = 0.
    #[test]
    fn gini_equal_distribution_is_zero() {
        let balances = vec![
            Decimal::new(25, 0),
            Decimal::new(25, 0),
            Decimal::new(25, 0),
            Decimal::new(25, 0),
        ];
        let g = gini_descending(&balances);
        // Allow tiny rounding tolerance from Decimal arithmetic.
        assert!(
            g.abs() < Decimal::new(1, 4), // < 0.0001
            "equal distribution must produce Gini ≈ 0.0, got {g}"
        );
    }

    /// Extreme inequality [100, 0, 0, 0] → Gini close to 0.75.
    ///
    /// With 4 holders and one holding everything:
    /// Gini = 1 - (2 * Σ weight_i * x_i) / (n * total)
    /// sorted asc: [0, 0, 0, 100]
    /// weight_i (n-i): [4, 3, 2, 1]
    /// weighted_sum = 4*0 + 3*0 + 2*0 + 1*100 = 100
    /// Gini = 1 - (2*100)/(4*100) = 1 - 200/400 = 0.75
    #[test]
    fn gini_extreme_inequality_near_075() {
        let balances = vec![
            Decimal::new(100, 0),
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        ];
        let g = gini_descending(&balances);
        let expected = Decimal::new(75, 2); // 0.75
        assert!(
            (g - expected).abs() < Decimal::new(1, 4),
            "extreme inequality (4 holders, 1 holds all) → Gini ≈ 0.75, got {g}"
        );
    }

    /// Two-element [100, 0] → Gini = 0.50.
    ///
    /// Textbook: n=2, perfect inequality → Gini = 0.50 (Lorenz area = 0.5).
    /// 1-based formula: Σ i*x_i = 1*0 + 2*100 = 200
    ///   Gini = (2*200)/(2*100) - 3/2 = 2.0 - 1.5 = 0.50 ✓
    #[test]
    fn gini_two_elements_unequal() {
        let balances = vec![Decimal::new(100, 0), Decimal::ZERO];
        let g = gini_descending(&balances);
        let expected = Decimal::new(5, 1); // 0.50
        assert!(
            (g - expected).abs() < Decimal::new(1, 4),
            "n=2 perfect inequality must give Gini=0.50, got {g}"
        );
    }

    /// Spec §3.2 calibration: perfect inequality across 4 holders.
    /// [100, 0, 0, 0] in any order → gini ≈ 0.75.
    #[test]
    fn gini_spec_calibration_extreme() {
        // The briefing says gini_descending(&[100, 0, 0, 0]) → very high (~0.75)
        let g = gini_descending(&[
            Decimal::new(100, 0),
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        ]);
        assert!(
            g > Decimal::new(5, 1), // > 0.50
            "extreme inequality Gini must be > 0.50 (very high), got {g}"
        );
    }

    // --- top_n_pct ---

    /// top_n_pct(&[50, 30, 20], 2) → 0.80.
    #[test]
    fn top_n_pct_basic() {
        let balances = vec![
            Decimal::new(50, 0),
            Decimal::new(30, 0),
            Decimal::new(20, 0),
        ];
        let pct = top_n_pct(&balances, 2);
        let expected = Decimal::new(80, 2); // 0.80
        assert_eq!(pct, expected, "top_2_pct([50,30,20]) must be 0.80");
    }

    /// top_n_pct with n >= len → 1.0.
    #[test]
    fn top_n_pct_all_holders() {
        let balances = vec![Decimal::new(50, 0), Decimal::new(50, 0)];
        let pct = top_n_pct(&balances, 10);
        assert_eq!(pct, Decimal::ONE, "top_n with n>len must return 1.0");
    }

    /// top_n_pct on empty slice → 0.0.
    #[test]
    fn top_n_pct_empty() {
        assert_eq!(top_n_pct(&[], 10), Decimal::ZERO);
    }

    /// top_n_pct with all zeros → 0.0 (avoid division by zero).
    #[test]
    fn top_n_pct_all_zeros() {
        let balances = vec![Decimal::ZERO, Decimal::ZERO];
        assert_eq!(top_n_pct(&balances, 1), Decimal::ZERO);
    }
}
