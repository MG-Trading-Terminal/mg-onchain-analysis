//! `ScoringEngine` — the primary public entry point.
//!
//! Orchestrates aggregation → attenuation → report assembly.
//! All computation is pure: no I/O, no clock reads (except forwarding `observed_at`
//! to `computed_at`), no randomness.

use chrono::{DateTime, Utc};
use tracing::instrument;

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence};
use mg_onchain_common::token::TokenMeta;

use crate::aggregate::compute_aggregation;
use crate::attenuation::{apply_attenuation, compute_attenuation};
use crate::config::ScoringConfig;
use crate::coverage::build_coverage_report;
use crate::evidence::rank_top_evidence;
use crate::types::{SkipReason, TokenRiskReport};

/// The stateless scoring engine.
///
/// Holds an immutable `ScoringConfig` reference. Callers construct one engine per
/// config and reuse it for all `score()` calls. Construction is cheap (no I/O).
///
/// # Thread safety
///
/// `ScoringEngine` is `Send + Sync`. Multiple threads may call `score()` concurrently
/// on the same engine without coordination — all state is in the config (immutable)
/// and the inputs (caller-owned).
pub struct ScoringEngine {
    pub config: ScoringConfig,
}

impl ScoringEngine {
    /// Construct a new engine with the given config.
    ///
    /// The config MUST have already been validated (via [`ScoringConfig::from_toml`] or
    /// [`ScoringConfig::validate`]). Construction does not re-validate.
    pub fn new(config: ScoringConfig) -> Self {
        Self { config }
    }

    /// Aggregate a vector of `AnomalyEvent` values for a single `(chain, token)` pair
    /// into a `TokenRiskReport`.
    ///
    /// # Pure-function contract
    ///
    /// - No I/O.
    /// - `observed_at` is the ONLY wall-clock read; it populates `computed_at` only.
    /// - All other output fields are deterministic given identical inputs.
    ///
    /// # Arguments
    ///
    /// * `events` — all detector events for `(chain, token)` in `window`. May be empty.
    ///   May arrive in any order; this function sorts before processing.
    /// * `meta` — current token metadata from the token registry.
    /// * `window` — `(window_start, window_end)` the events were collected over.
    /// * `skip_reasons` — detectors that the caller did NOT run, with reasons.
    ///   Passed through verbatim into `CoverageReport`.
    /// * `observed_at` — wall-clock time of this call. Populates `computed_at` only.
    #[instrument(
        level = "debug",
        skip(self, events, meta, skip_reasons),
        fields(
            chain = %meta.chain.as_str(),
            token = %meta.mint.as_str(),
            event_count = events.len(),
        )
    )]
    pub fn score(
        &self,
        events: &[AnomalyEvent],
        meta: &TokenMeta,
        window: (DateTime<Utc>, DateTime<Utc>),
        skip_reasons: &[SkipReason],
        observed_at: DateTime<Utc>,
    ) -> TokenRiskReport {
        // Sort events for determinism before any processing.
        // Stable sort preserves relative order of events with identical key.
        let mut sorted_events: Vec<&AnomalyEvent> = events.iter().collect();
        sorted_events.sort_by(|a, b| {
            a.detector_id
                .cmp(&b.detector_id)
                .then_with(|| a.observed_at.cmp(&b.observed_at))
        });
        // Clone into an owned vec for the aggregation functions (which take &[AnomalyEvent]).
        let sorted_owned: Vec<AnomalyEvent> = sorted_events.into_iter().cloned().collect();

        // 1. Weighted-sum aggregation.
        let agg = compute_aggregation(&sorted_owned, window.1, &self.config);

        // 2. Attenuation stack.
        let factors = compute_attenuation(meta, &self.config);
        let overall_score_raw = apply_attenuation(agg.base_score, &factors);

        // 3. Build Confidence values (both are already clamped to [0,1]).
        let overall_score =
            Confidence::new(overall_score_raw).unwrap_or(Confidence::ZERO);
        let base_score_conf =
            Confidence::new(agg.base_score.clamp(0.0, 1.0)).unwrap_or(Confidence::ZERO);

        // 4. Top evidence.
        let top_evidence =
            rank_top_evidence(&sorted_owned, self.config.evidence_highlight_count);

        // 5. Coverage report.
        let coverage = build_coverage_report(
            &sorted_owned,
            skip_reasons,
            &self.config.detector_weights,
        );

        TokenRiskReport {
            token: meta.mint.clone(),
            chain: meta.chain,
            window,
            computed_at: observed_at,
            overall_score,
            base_score: base_score_conf,
            overall_severity: agg.overall_severity,
            per_detector: agg.per_detector,
            top_evidence,
            signal_counts: agg.signal_counts,
            coverage,
            config_snapshot: self.config.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScoringConfig;
    use crate::tests_common::{make_event_at, make_meta};
    use crate::types::SkipReason;
    use chrono::Duration;
    use mg_onchain_common::anomaly::Severity;

    fn engine() -> ScoringEngine {
        ScoringEngine::new(ScoringConfig::default_calibrated())
    }

    fn fixed_now() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        let end = fixed_now();
        let start = end - Duration::hours(24);
        (start, end)
    }

    // ------------------------------------------------------------------ //
    // Calibration anchor: RAVE                                             //
    // ------------------------------------------------------------------ //

    #[test]
    fn rave_calibration_anchor() {
        // Spec §10 anchor 1: overall_score ∈ [0.74, 0.80], severity = Critical.
        // No attenuation (no jup flags, not established protocol).
        //
        // P6-0 calibration note (GAP-SCORE-01 closure — see SESSION-KICKOFF.md §P6-0):
        // D03+D04 weights rebalanced 0.35→0.32 each to accommodate D07=0.06.
        // RAVE has no D07 events; expected base_score ≈ 0.7714 (was ≈ 0.8274 in Sprint 5).
        // Band widened from [0.80, 0.86] to [0.74, 0.80] to reflect the rebalancing.
        // This is a deliberate calibration shift, NOT a regression.
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90)); // high score → not established
        let events = vec![
            make_event_at("honeypot_sim_static", 0.03, Severity::Info, observed),
            make_event_at("rug_pull_lp_drain_latent", 0.72, Severity::High, observed),
            make_event_at("holder_concentration", 0.95, Severity::Critical, observed),
            make_event_at("pump_dump", 0.92, Severity::Critical, observed),
            make_event_at("wash_trading_h1", 0.45, Severity::Medium, observed),
            make_event_at("mint_burn_anomaly_static", 0.02, Severity::Info, observed),
            // D07 not in RAVE probe (no Token-2022 fee drain events)
        ];
        let report = eng.score(&events, &meta, (win_start, win_end), &[], fixed_now());
        let score = report.overall_score.value();
        assert!(
            (0.74..=0.80).contains(&score),
            "RAVE overall_score={score:.4} not in [0.74, 0.80] \
             (P6-0 rebalanced from [0.80, 0.86]; see SESSION-KICKOFF.md §P6-0)"
        );
        assert_eq!(report.overall_severity, Severity::Critical, "RAVE severity must be Critical");
        // base_score must equal overall_score (no attenuation applied).
        assert!(
            (report.base_score.value() - report.overall_score.value()).abs() < 1e-9,
            "no attenuation applied; base_score must equal overall_score"
        );
    }

    // ------------------------------------------------------------------ //
    // Calibration anchor: WET                                              //
    // ------------------------------------------------------------------ //

    #[test]
    fn wet_calibration_anchor() {
        // Spec §10 anchor 2: overall_score ∈ [0.27, 0.35], severity = Medium.
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90));
        let events = vec![
            make_event_at("honeypot_sim_static", 0.02, Severity::Info, observed),
            make_event_at("rug_pull_lp_drain_latent", 0.28, Severity::Low, observed),
            make_event_at("holder_concentration", 0.55, Severity::Medium, observed),
            make_event_at("pump_dump", 0.12, Severity::Info, observed),
            make_event_at("wash_trading_h1", 0.25, Severity::Info, observed),
            make_event_at("mint_burn_anomaly_static", 0.02, Severity::Info, observed),
        ];
        let report = eng.score(&events, &meta, (win_start, win_end), &[], fixed_now());
        let score = report.overall_score.value();
        assert!(
            (0.27..=0.35).contains(&score),
            "WET overall_score={score:.4} not in [0.27, 0.35]"
        );
        assert_eq!(report.overall_severity, Severity::Medium, "WET severity must be Medium");
    }

    // ------------------------------------------------------------------ //
    // Empty events                                                         //
    // ------------------------------------------------------------------ //

    #[test]
    fn empty_events_produces_zero_score_and_info_severity() {
        let eng = engine();
        let meta = make_meta(false, false, Some(90));
        let report = eng.score(&[], &meta, window(), &[], fixed_now());
        assert_eq!(report.overall_score.value(), 0.0);
        assert_eq!(report.overall_severity, Severity::Info);
        assert_eq!(report.signal_counts.fired, 0);
        // All 7 canonical detectors present with zero events (D07 added in P6-0).
        assert_eq!(report.per_detector.len(), 7);
        for ds in report.per_detector.values() {
            assert_eq!(ds.fired_events, 0);
        }
    }

    // ------------------------------------------------------------------ //
    // Attenuation: jup_strict reduces score                                //
    // ------------------------------------------------------------------ //

    #[test]
    fn jup_strict_reduces_rave_score_to_below_0_26() {
        // Spec §14: jup_strict must reduce RAVE base (≈0.771 post-P6-0; was ≈0.827) to ≤ 0.26.
        // Attenuated: 0.771 × jup_strict(0.30) × established_protocol(0.50) ≈ 0.116 < 0.26. Passes.
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        // jup_strict=true; rugcheck_score=50 (above established-protocol thresholds)
        // → established_factor also fires (jup_strict is branch 1 of is_established_protocol)
        let meta = make_meta(false, true, Some(50));
        let events = vec![
            make_event_at("honeypot_sim_static", 0.03, Severity::Info, observed),
            make_event_at("rug_pull_lp_drain_latent", 0.72, Severity::High, observed),
            make_event_at("holder_concentration", 0.95, Severity::Critical, observed),
            make_event_at("pump_dump", 0.92, Severity::Critical, observed),
            make_event_at("wash_trading_h1", 0.45, Severity::Medium, observed),
            make_event_at("mint_burn_anomaly_static", 0.02, Severity::Info, observed),
        ];
        let report = eng.score(&events, &meta, (win_start, win_end), &[], fixed_now());
        let base = report.base_score.value();
        let overall = report.overall_score.value();
        assert!(
            overall < 0.26,
            "jup_strict RAVE: overall_score={overall:.4} must be < 0.26 (base={base:.4})"
        );
    }

    // ------------------------------------------------------------------ //
    // Determinism                                                          //
    // ------------------------------------------------------------------ //

    #[test]
    fn identical_inputs_produce_identical_output() {
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90));
        let events = vec![
            make_event_at("holder_concentration", 0.95, Severity::Critical, observed),
            make_event_at("pump_dump", 0.92, Severity::Critical, observed),
        ];
        let now = fixed_now();
        let r1 = eng.score(&events, &meta, (win_start, win_end), &[], now);
        let r2 = eng.score(&events, &meta, (win_start, win_end), &[], now);
        // Serialize both and compare (excludes computed_at from comparison since
        // both calls use the same `now` in this test).
        let j1 = serde_json::to_string(&r1).unwrap();
        let j2 = serde_json::to_string(&r2).unwrap();
        assert_eq!(j1, j2, "identical inputs must produce identical JSON output");
    }

    #[test]
    fn different_computed_at_but_same_scores() {
        // Two calls with different `observed_at` must produce identical scores.
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90));
        let events = vec![make_event_at(
            "holder_concentration",
            0.80,
            Severity::High,
            observed,
        )];
        let now1 = fixed_now();
        let now2 = fixed_now() + Duration::seconds(60);
        let r1 = eng.score(&events, &meta, (win_start, win_end), &[], now1);
        let r2 = eng.score(&events, &meta, (win_start, win_end), &[], now2);
        assert_eq!(r1.overall_score.value(), r2.overall_score.value());
        assert_eq!(r1.overall_severity, r2.overall_severity);
        // computed_at differs:
        assert_ne!(r1.computed_at, r2.computed_at);
    }

    // ------------------------------------------------------------------ //
    // BTreeMap ordering                                                    //
    // ------------------------------------------------------------------ //

    #[test]
    fn per_detector_keys_alphabetically_sorted() {
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90));
        // Feed events in reverse-alphabetical detector_id order.
        let events = vec![
            make_event_at("wash_trading_h1", 0.5, Severity::Medium, observed),
            make_event_at("pump_dump", 0.9, Severity::Critical, observed),
            make_event_at("holder_concentration", 0.8, Severity::High, observed),
        ];
        let report = eng.score(&events, &meta, (win_start, win_end), &[], fixed_now());
        let keys: Vec<&str> = report.per_detector.keys().map(String::as_str).collect();
        let mut expected = keys.clone();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    // ------------------------------------------------------------------ //
    // Coverage report                                                      //
    // ------------------------------------------------------------------ //

    #[test]
    fn skip_reasons_appear_in_coverage() {
        let eng = engine();
        let meta = make_meta(false, false, Some(90));
        let skip = vec![SkipReason {
            detector_id: "honeypot_sim".into(),
            reason: "simulation disabled".into(),
        }];
        let report = eng.score(&[], &meta, window(), &skip, fixed_now());
        assert_eq!(report.coverage.detectors_skipped.len(), 1);
        assert_eq!(report.coverage.detectors_skipped[0].detector_id, "honeypot_sim");
    }

    // ------------------------------------------------------------------ //
    // Config snapshot                                                      //
    // ------------------------------------------------------------------ //

    #[test]
    fn config_snapshot_stored_in_report() {
        let eng = engine();
        let meta = make_meta(false, false, Some(90));
        let report = eng.score(&[], &meta, window(), &[], fixed_now());
        // Weights in snapshot must match the engine's config.
        assert_eq!(
            report.config_snapshot.detector_weights.holder_concentration,
            eng.config.detector_weights.holder_concentration
        );
    }

    // ------------------------------------------------------------------ //
    // All-suppressed / inconclusive                                        //
    // ------------------------------------------------------------------ //

    #[test]
    fn all_inconclusive_events_produce_zero_effective_score() {
        let eng = engine();
        let (win_start, win_end) = window();
        let observed = win_end - Duration::hours(1);
        let meta = make_meta(false, false, Some(90));
        // All confidences < 0.30 (inconclusive_floor).
        let events = vec![
            make_event_at("pump_dump", 0.10, Severity::Critical, observed),
            make_event_at("holder_concentration", 0.15, Severity::High, observed),
        ];
        let report = eng.score(&events, &meta, (win_start, win_end), &[], fixed_now());
        // Inconclusive events still contribute to base_score through max_effective_confidence.
        // BUT overall_severity must stay Info because all are below floor.
        assert_eq!(report.overall_severity, Severity::Info);
        assert_eq!(report.signal_counts.fired, 0);
        assert_eq!(report.signal_counts.inconclusive, 2);
    }
}
