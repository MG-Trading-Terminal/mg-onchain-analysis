//! Coverage report construction.
//!
//! `CoverageReport` tells consumers which detectors had the opportunity to fire,
//! which did fire, and which were skipped and why.
//!
//! # Key distinction
//!
//! - `detectors_run` = detectors that produced ≥1 `AnomalyEvent` in the window.
//!   (Includes detectors that fired at very low confidence or Info severity.)
//! - `detectors_skipped` = detectors the caller explicitly skipped (no data, disabled, etc.).
//!
//! A detector that ran but emitted `Ok(vec![])` falls into neither list — it
//! is neither "run" (no events) nor "skipped" (it ran). The caller should pass
//! it as a `SkipReason` with reason `"no events emitted"` if it wants coverage
//! to reflect the clean-signal case.

use std::collections::BTreeSet;

use mg_onchain_common::anomaly::AnomalyEvent;

use crate::config::DetectorWeights;
use crate::types::{CoverageReport, SkipReason};

/// Build a `CoverageReport` from the event slice and caller-supplied skip reasons.
///
/// # Arguments
///
/// * `events` — all events for this token in the window (any order; we derive
///   `detectors_run` by unique detector_id extraction).
/// * `skip_reasons` — caller-supplied skip metadata from the scheduler/gateway.
/// * `weights` — `DetectorWeights` whose canonical IDs define the total known count.
pub fn build_coverage_report(
    events: &[AnomalyEvent],
    skip_reasons: &[SkipReason],
    weights: &DetectorWeights,
) -> CoverageReport {
    // detectors_run: unique detector_ids that produced at least one event.
    // BTreeSet for deterministic alphabetical ordering.
    let detectors_run: Vec<String> = events
        .iter()
        .map(|e| e.detector_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    // detectors_skipped: verbatim from caller (already sorted by caller convention).
    let detectors_skipped: Vec<SkipReason> = skip_reasons.to_vec();

    // Coverage completeness: fraction of known detectors covered.
    // "Covered" = appeared in detectors_run OR has a skip reason.
    let total_known = DetectorWeights::canonical_ids().len();
    let covered_run: BTreeSet<String> = detectors_run.iter().cloned().collect();
    let covered_skipped: BTreeSet<String> =
        skip_reasons.iter().map(|s| s.detector_id.clone()).collect();
    let covered = covered_run.union(&covered_skipped).count();

    let coverage_completeness = if total_known == 0 {
        1.0_f32
    } else {
        (covered as f32 / total_known as f32).clamp(0.0, 1.0)
    };

    // Suppress unused-variable warning: `weights` is used to access canonical_ids.
    let _ = weights;

    CoverageReport {
        detectors_run,
        detectors_skipped,
        coverage_completeness,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScoringConfig;
    use crate::tests_common::make_event;
    use mg_onchain_common::anomaly::Severity;

    fn weights() -> DetectorWeights {
        ScoringConfig::default_calibrated().detector_weights
    }

    #[test]
    fn no_events_no_skips_means_zero_coverage() {
        let report = build_coverage_report(&[], &[], &weights());
        assert!(report.detectors_run.is_empty());
        assert!(report.detectors_skipped.is_empty());
        assert_eq!(report.coverage_completeness, 0.0_f32);
    }

    #[test]
    fn single_event_detector_appears_in_run() {
        let e = make_event("pump_dump", 0.9, Severity::Critical);
        let report = build_coverage_report(&[e], &[], &weights());
        assert_eq!(report.detectors_run, vec!["pump_dump"]);
        assert!(report.detectors_skipped.is_empty());
    }

    #[test]
    fn duplicate_detector_id_deduplicated() {
        let e1 = make_event("pump_dump", 0.9, Severity::Critical);
        let e2 = make_event("pump_dump", 0.7, Severity::High);
        let report = build_coverage_report(&[e1, e2], &[], &weights());
        assert_eq!(report.detectors_run.len(), 1);
        assert_eq!(report.detectors_run[0], "pump_dump");
    }

    #[test]
    fn detectors_run_sorted_alphabetically() {
        let e1 = make_event("wash_trading_h1", 0.5, Severity::Medium);
        let e2 = make_event("holder_concentration", 0.9, Severity::Critical);
        let e3 = make_event("pump_dump", 0.8, Severity::High);
        let report = build_coverage_report(&[e1, e2, e3], &[], &weights());
        assert_eq!(
            report.detectors_run,
            vec!["holder_concentration", "pump_dump", "wash_trading_h1"]
        );
    }

    #[test]
    fn skip_reasons_passed_through_verbatim() {
        let skips = vec![
            SkipReason {
                detector_id: "honeypot_sim".into(),
                reason: "simulation disabled".into(),
            },
            SkipReason {
                detector_id: "mint_burn_anomaly".into(),
                reason: "no mint events in window".into(),
            },
        ];
        let report = build_coverage_report(&[], &skips, &weights());
        assert_eq!(report.detectors_skipped.len(), 2);
        assert_eq!(report.detectors_skipped[0].detector_id, "honeypot_sim");
    }

    #[test]
    fn coverage_completeness_all_seven_covered() {
        // 7 canonical detectors (D01–D07); all 7 produce events (P6-0 / GAP-SCORE-01 closure).
        let events = vec![
            make_event("honeypot_sim", 0.03, Severity::Info),
            make_event("rug_pull_lp_drain", 0.72, Severity::High),
            make_event("holder_concentration", 0.95, Severity::Critical),
            make_event("pump_dump", 0.92, Severity::Critical),
            make_event("wash_trading_h1", 0.45, Severity::Medium),
            make_event("mint_burn_anomaly", 0.02, Severity::Info),
            make_event("withdraw_withheld_drain", 0.65, Severity::Medium),
        ];
        let report = build_coverage_report(&events, &[], &weights());
        assert_eq!(
            report.coverage_completeness, 1.0,
            "all 7 canonical detectors covered"
        );
    }

    #[test]
    fn coverage_completeness_partial() {
        // P6-0: 7 canonical detectors total.
        // 3 run + 1 skipped = 4/7 ≈ 0.571
        let events = vec![
            make_event("pump_dump", 0.9, Severity::Critical),
            make_event("holder_concentration", 0.8, Severity::High),
            make_event("wash_trading_h1", 0.4, Severity::Medium),
        ];
        let skips = vec![SkipReason {
            detector_id: "honeypot_sim".into(),
            reason: "disabled".into(),
        }];
        let report = build_coverage_report(&events, &skips, &weights());
        // 4 covered out of 7 (D01 skipped + D03+D04+D05 ran; D02/D06/D07 neither ran nor skipped)
        assert!((report.coverage_completeness - 4.0 / 7.0).abs() < 0.001);
    }
}
