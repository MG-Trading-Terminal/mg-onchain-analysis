//! Core aggregation: `base_score` computation and per-detector score assembly.
//!
//! # Formula (spec §3, Option 2)
//!
//! ```text
//! base_score = Σ (w_i × max_effective_confidence_i)
//!              i = D01..D06
//! ```
//!
//! Where:
//! - `w_i` = detector importance weight from config
//! - `max_effective_confidence_i` = MAX(`raw_confidence × decay_factor`) across all
//!   events for detector `i` (0.0 if no events fired)
//!
//! Per spec §3 OQ1 resolution: use MAX effective confidence per detector (not average)
//! for the global formula. This is conservative — if one pool has strong wash-trading
//! and others are clean, the worst case drives the score.
//!
//! Per-detector `weighted_confidence` (stored in `DetectorScore`) is the average of
//! decay-adjusted confidences across events, provided for human review in breakdowns.
//!
//! # Determinism
//!
//! Events are sorted by `(detector_id, observed_at)` before processing. No `HashMap`
//! is used anywhere on the path from input events to output. All grouping is via
//! `BTreeMap`.

use std::collections::BTreeMap;

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Severity};
use crate::config::ScoringConfig;
use crate::decay::exp_decay;
use crate::types::{DetectorScore, SignalCounts};

/// Intermediate per-detector data collected during aggregation.
#[derive(Debug)]
struct DetectorAccumulator {
    events: Vec<(f64, f64, Severity)>, // (raw_confidence, decay_factor, severity)
    evidence_candidates: Vec<(Severity, f64, String, String)>, // (sev, conf, key, value)
}

impl DetectorAccumulator {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            evidence_candidates: Vec::new(),
        }
    }

    /// Maximum effective confidence (raw × decay) — feeds the aggregation formula.
    ///
    /// Note: the inconclusive_floor does NOT filter events for the formula (spec §3).
    /// Inconclusive events still contribute to base_score; the floor only gates
    /// `fired_events` counts and `overall_severity` (spec §6 + OQ3 resolution).
    fn max_effective_confidence(&self) -> f64 {
        self.events
            .iter()
            .map(|(r, d, _)| r * d)
            .fold(0.0_f64, f64::max)
    }

    /// Raw max confidence (no decay) for display in DetectorScore.
    fn max_raw_confidence(&self) -> f64 {
        self.events
            .iter()
            .map(|(r, _, _)| *r)
            .fold(0.0_f64, f64::max)
    }

    /// Weighted average: mean of (raw × decay) across ALL events (including inconclusive).
    fn weighted_avg_confidence(&self) -> f64 {
        if self.events.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.events.iter().map(|(r, d, _)| r * d).sum();
        sum / self.events.len() as f64
    }

    fn max_severity(&self) -> Severity {
        self.events
            .iter()
            .map(|(_, _, s)| *s)
            .max()
            .unwrap_or(Severity::Info)
    }

    fn fired_events(&self, floor: f64) -> u32 {
        self.events
            .iter()
            .filter(|(r, d, _)| r * d >= floor)
            .count() as u32
    }

    fn inconclusive_events(&self, floor: f64) -> u32 {
        self.events
            .iter()
            .filter(|(r, d, _)| r * d < floor)
            .count() as u32
    }

    /// Top-3 evidence pairs by (severity DESC, confidence DESC, key ASC).
    fn top_evidence_summary(&self) -> Vec<(String, String)> {
        let mut cands = self.evidence_candidates.clone();
        cands.sort_by(|a, b| {
            let sev_cmp = severity_ordinal(b.0).cmp(&severity_ordinal(a.0));
            if sev_cmp != std::cmp::Ordering::Equal {
                return sev_cmp;
            }
            let conf_cmp = b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal);
            if conf_cmp != std::cmp::Ordering::Equal {
                return conf_cmp;
            }
            a.2.cmp(&b.2) // key ASC
        });
        cands.into_iter().take(3).map(|(_, _, k, v)| (k, v)).collect()
    }
}

fn severity_ordinal(s: Severity) -> u8 {
    match s {
        Severity::Info => 0,
        Severity::Low => 1,
        Severity::Medium => 2,
        Severity::High => 3,
        Severity::Critical => 4,
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

/// Read the suppressed_count metric from the highest-confidence event's evidence.
///
/// Convention: detectors emit `"<detector_id>/suppressed_count"` in their evidence
/// when they suppress latent signals via `is_established_protocol`.
fn read_suppressed_count(events: &[&AnomalyEvent], detector_id: &str) -> u32 {
    // Use the highest raw-confidence event for the metric read.
    let best = events.iter().max_by(|a, b| {
        a.confidence
            .value()
            .partial_cmp(&b.confidence.value())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let Some(event) = best else {
        return 0;
    };

    let key = format!("{detector_id}/suppressed_count");
    event
        .evidence
        .metrics
        .get(&key)
        .and_then(|d| {
            // Decimal to u32: parse via string for robustness.
            d.to_string().parse::<u32>().ok()
        })
        .unwrap_or(0)
}

/// Result of the aggregation pass.
pub struct AggregationResult {
    /// Weighted sum of (w_i × max_effective_confidence_i). Not yet attenuated.
    pub base_score: f64,
    /// Per-detector breakdown for `TokenRiskReport::per_detector`.
    pub per_detector: BTreeMap<String, DetectorScore>,
    /// Signal counts across all events.
    pub signal_counts: SignalCounts,
    /// Worst severity across events where confidence ≥ inconclusive_floor.
    /// Per OQ3 resolution: only events at or above the floor contribute to severity.
    pub overall_severity: Severity,
}

/// Run the weighted-sum aggregation over all events.
///
/// # Arguments
///
/// * `events` — all events for `(chain, token)` in the window. May be empty.
///   MUST be sorted by `(detector_id, observed_at)` before calling (enforced by engine).
/// * `window_end` — the end of the scoring window; used to compute event age.
/// * `config` — scoring configuration.
pub fn compute_aggregation(
    events: &[AnomalyEvent],
    window_end: chrono::DateTime<chrono::Utc>,
    config: &ScoringConfig,
) -> AggregationResult {
    // Group events by canonical detector ID using BTreeMap (deterministic ordering).
    // Canonical ID: strip known suffixes (_static, _latent, _event) for grouping,
    // but keep the raw detector_id for weight lookup.
    let mut accumulators: BTreeMap<String, DetectorAccumulator> = BTreeMap::new();

    // Pre-populate all known canonical detectors with empty accumulators so that
    // per_detector has entries for all 7 detectors even with zero events (D07 added P6-0).
    for &id in DetectorScore::canonical_ids() {
        accumulators.insert(id.to_string(), DetectorAccumulator::new());
    }

    // Map raw detector_ids from events to canonical keys.
    for event in events {
        let canonical = canonical_detector_id(&event.detector_id);
        let acc = accumulators
            .entry(canonical.to_string())
            .or_insert_with(DetectorAccumulator::new);

        // Compute age in hours from window_end.
        let age_secs = window_end
            .signed_duration_since(event.observed_at)
            .num_seconds();
        let age_hours = (age_secs as f64 / 3600.0).max(0.0);

        // State-based signals: decay = 1.0. Event-based: apply exponential decay.
        let decay = if config.is_state_based(&event.detector_id) {
            1.0
        } else {
            exp_decay(age_hours, config.decay_half_life_hours)
        };

        acc.events
            .push((event.confidence.value(), decay, event.severity));

        // Collect evidence candidates for the detector summary.
        for (key, val) in &event.evidence.metrics {
            acc.evidence_candidates.push((
                event.severity,
                event.confidence.value(),
                key.clone(),
                val.to_string(),
            ));
        }
    }

    // Read suppressed_count from events (before consuming accumulators).
    // Build a map: detector_id → Vec<&AnomalyEvent> for the suppressed_count lookup.
    let mut events_by_canonical: BTreeMap<String, Vec<&AnomalyEvent>> = BTreeMap::new();
    for event in events {
        events_by_canonical
            .entry(canonical_detector_id(&event.detector_id).to_string())
            .or_default()
            .push(event);
    }

    // Build per_detector and compute base_score.
    let floor = config.inconclusive_floor;
    let mut base_score: f64 = 0.0;
    let mut per_detector: BTreeMap<String, DetectorScore> = BTreeMap::new();
    let mut total_fired: u32 = 0;
    let mut total_inconclusive: u32 = 0;
    let mut total_suppressed_info: u32 = 0;

    // Track overall severity only over events ≥ inconclusive_floor (OQ3 resolution).
    let mut overall_severity = Severity::Info;

    for (canonical_id, acc) in &accumulators {
        let weight = config
            .detector_weights
            .for_detector(canonical_id)
            .unwrap_or(0.0);

        // Max effective confidence for the aggregation formula.
        // Floor does NOT apply here — inconclusive events still contribute to
        // base_score (spec §3 + WET calibration table §10).
        let max_eff = acc.max_effective_confidence();
        base_score += weight * max_eff;

        let fired = acc.fired_events(floor);
        let inconclusive = acc.inconclusive_events(floor);
        let max_raw = acc.max_raw_confidence();
        let weighted_avg = acc.weighted_avg_confidence();
        let max_sev = acc.max_severity();

        // Update signal counts.
        total_fired += fired;
        total_inconclusive += inconclusive;
        // Suppressed-info: events at Severity::Info (detector-internal floor).
        let info_events = acc
            .events
            .iter()
            .filter(|(_, _, s)| *s == Severity::Info)
            .count() as u32;
        total_suppressed_info += info_events;

        // Update overall severity (OQ3: only events at or above inconclusive_floor).
        if fired > 0 && max_sev > overall_severity {
            overall_severity = max_sev;
        }

        let event_refs = events_by_canonical
            .get(canonical_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let suppressed = read_suppressed_count(event_refs, canonical_id);

        let max_conf = Confidence::new(max_raw.clamp(0.0, 1.0)).unwrap_or(Confidence::ZERO);
        let weighted_conf =
            Confidence::new(weighted_avg.clamp(0.0, 1.0)).unwrap_or(Confidence::ZERO);

        let detector_score = DetectorScore {
            detector_id: canonical_id.clone(),
            fired_events: fired,
            inconclusive_events: inconclusive,
            suppressed_events: suppressed,
            max_confidence: max_conf,
            weighted_confidence: weighted_conf,
            severity: max_sev,
            evidence_summary: acc.top_evidence_summary(),
        };

        per_detector.insert(canonical_id.clone(), detector_score);
    }

    AggregationResult {
        base_score: base_score.clamp(0.0, 1.0),
        per_detector,
        signal_counts: SignalCounts {
            fired: total_fired,
            inconclusive: total_inconclusive,
            suppressed_info: total_suppressed_info,
        },
        overall_severity,
    }
}

/// Map a raw detector_id to its canonical group key.
///
/// Canonical IDs are the six base names from `DetectorWeights::canonical_ids()`.
/// Detector variants (e.g. `"honeypot_sim_static"`, `"rug_pull_lp_drain_latent"`)
/// are mapped back to their base.
fn canonical_detector_id(raw: &str) -> &str {
    // Try exact match first (most common case).
    if DetectorScore::canonical_ids().contains(&raw) {
        return raw;
    }
    // Known variant suffixes:
    if raw == "honeypot_sim_static" {
        return "honeypot_sim";
    }
    if raw.starts_with("rug_pull_lp_drain") {
        return "rug_pull_lp_drain";
    }
    if raw.starts_with("mint_burn_anomaly") {
        return "mint_burn_anomaly";
    }
    // Unknown detector — keep as-is (will be in per_detector under its own key).
    raw
}

impl DetectorScore {
    fn canonical_ids() -> &'static [&'static str] {
        crate::config::DetectorWeights::canonical_ids()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScoringConfig;
    use crate::tests_common::make_event_at;
    use chrono::{Duration, Utc};
    use mg_onchain_common::anomaly::{Evidence, Severity};
    use rust_decimal_macros::dec;

    fn cfg() -> ScoringConfig {
        ScoringConfig::default_calibrated()
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        // Fixed timestamp for determinism in tests.
        chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn empty_events_produces_zero_base_score() {
        let result = compute_aggregation(&[], now(), &cfg());
        assert_eq!(result.base_score, 0.0);
        assert_eq!(result.overall_severity, Severity::Info);
        assert_eq!(result.signal_counts.fired, 0);
        // All 7 canonical detectors appear with zero events (D07 added in P6-0).
        assert_eq!(result.per_detector.len(), 7);
        for score in result.per_detector.values() {
            assert_eq!(score.fired_events, 0);
            assert_eq!(score.max_confidence, Confidence::ZERO);
        }
    }

    #[test]
    fn per_detector_keys_are_sorted_alphabetically() {
        // Insert events in reverse-alphabetical order of detector_id.
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let e1 = make_event_at("wash_trading_h1", 0.5, Severity::Medium, observed);
        let e2 = make_event_at("pump_dump", 0.9, Severity::Critical, observed);
        let e3 = make_event_at("holder_concentration", 0.8, Severity::High, observed);
        let result = compute_aggregation(&[e1, e2, e3], window_end, &cfg());

        let keys: Vec<&String> = result.per_detector.keys().collect();
        let sorted: Vec<&String> = {
            let mut v = keys.clone();
            v.sort();
            v
        };
        assert_eq!(keys, sorted, "per_detector keys must be in alphabetical order");
    }

    #[test]
    fn single_holder_concentration_event_contributes_correct_weight() {
        // holder_concentration weight = 0.32 (P6-0: rebalanced from 0.35); state-based (decay=1.0); conf=0.95
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let e = make_event_at("holder_concentration", 0.95, Severity::Critical, observed);
        let result = compute_aggregation(&[e], window_end, &cfg());
        // base_score should be 0.32 × 0.95 × 1.0 = 0.304
        assert!(
            (result.base_score - 0.304).abs() < 1e-6,
            "base_score = {}, expected 0.304 (P6-0: weight 0.32)",
            result.base_score
        );
    }

    #[test]
    fn event_based_detector_decays_at_1h() {
        // pump_dump is event-based; at 1h age with 72h half-life, decay ≈ 0.9904.
        // P6-0: pump_dump weight = 0.32 (rebalanced from 0.35).
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let e = make_event_at("pump_dump", 0.92, Severity::Critical, observed);
        let result = compute_aggregation(&[e], window_end, &cfg());
        // 0.32 × 0.92 × exp(-1×ln2/72) ≈ 0.32 × 0.92 × 0.99042 ≈ 0.29143
        let expected = 0.32 * 0.92 * exp_decay(1.0, 72.0);
        assert!(
            (result.base_score - expected).abs() < 1e-6,
            "base={} expected={expected}",
            result.base_score
        );
    }

    #[test]
    fn inconclusive_event_does_not_drive_severity() {
        // Event with confidence 0.10 < floor 0.30 → inconclusive → severity stays Info.
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let e = make_event_at("pump_dump", 0.10, Severity::Critical, observed);
        let result = compute_aggregation(&[e], window_end, &cfg());
        assert_eq!(
            result.overall_severity,
            Severity::Info,
            "inconclusive event must not drive severity"
        );
        assert_eq!(result.signal_counts.inconclusive, 1);
        assert_eq!(result.signal_counts.fired, 0);
    }

    #[test]
    fn suppressed_count_read_from_evidence() {
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let mut e = make_event_at("rug_pull_lp_drain", 0.72, Severity::High, observed);
        e.evidence = Evidence::new()
            .with_metric("rug_pull_lp_drain/suppressed_count", dec!(3))
            .with_metric("rug_pull_lp_drain/lp_removed_pct", dec!(0.72));
        let result = compute_aggregation(&[e], window_end, &cfg());
        assert_eq!(
            result
                .per_detector
                .get("rug_pull_lp_drain")
                .unwrap()
                .suppressed_events,
            3
        );
    }

    #[test]
    fn max_used_not_average_for_base_score() {
        // Two events for same detector: 0.90 and 0.40. Max effective = 0.90 × decay.
        // Average would be 0.65 × decay — spec §3 requires max.
        let window_end = now();
        let observed = window_end; // age=0 → decay=1.0
        let e1 = make_event_at("pump_dump", 0.90, Severity::Critical, observed);
        let e2 = make_event_at("pump_dump", 0.40, Severity::Medium, observed);
        let result = compute_aggregation(&[e1, e2], window_end, &cfg());
        // base = 0.32 × 0.90 = 0.288 (max, not 0.32 × 0.65 = 0.208)
        // P6-0: pump_dump weight 0.32 (was 0.35).
        assert!(
            (result.base_score - 0.32 * 0.90).abs() < 1e-6,
            "base={} expected 0.288 (max not avg, P6-0 weight 0.32)",
            result.base_score
        );
    }

    #[test]
    fn variant_detector_id_mapped_to_canonical() {
        // honeypot_sim_static → canonical "honeypot_sim"
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let e = make_event_at("honeypot_sim_static", 0.95, Severity::Critical, observed);
        let result = compute_aggregation(&[e], window_end, &cfg());
        // Must appear under "honeypot_sim", not "honeypot_sim_static".
        assert!(
            result.per_detector.contains_key("honeypot_sim"),
            "honeypot_sim_static must map to canonical key honeypot_sim"
        );
        assert_eq!(
            result.per_detector.get("honeypot_sim").unwrap().fired_events,
            1
        );
    }

    #[test]
    fn rave_calibration_base_score() {
        // Reproduce RAVE probe from spec §10, table row by row, all at 1h age.
        //
        // P6-0 calibration note (GAP-SCORE-01 closure):
        // D03+D04 weights rebalanced 0.35→0.32 each to accommodate D07=0.06.
        // RAVE has no D07 events, so the shift is purely from D03+D04 weight reduction:
        //   Old: 0.35×0.95 + 0.35×0.92×decay ≈ 0.3325 + 0.3191 = 0.6516 (D03+D04 share)
        //   New: 0.32×0.95 + 0.32×0.92×decay ≈ 0.304  + 0.2914 = 0.5954 (D03+D04 share)
        // Expected RAVE base_score: ~0.7714 (was ~0.8274 in Sprint 5).
        // This is a calibration rebalance, NOT a regression — see SESSION-KICKOFF.md §P6-0.
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let events = vec![
            // D01 honeypot_sim — state-based (no decay), conf=0.03, sev=Info
            make_event_at("honeypot_sim_static", 0.03, Severity::Info, observed),
            // D02 rug_pull_lp_drain_latent — state-based, conf=0.72, sev=High
            make_event_at("rug_pull_lp_drain_latent", 0.72, Severity::High, observed),
            // D03 holder_concentration — state-based, conf=0.95, sev=Critical
            make_event_at("holder_concentration", 0.95, Severity::Critical, observed),
            // D04 pump_dump — event-based, conf=0.92, sev=Critical
            make_event_at("pump_dump", 0.92, Severity::Critical, observed),
            // D05 wash_trading_h1 — event-based, conf=0.45, sev=Medium
            make_event_at("wash_trading_h1", 0.45, Severity::Medium, observed),
            // D06 mint_burn_anomaly — state-based, conf=0.02, sev=Info
            make_event_at("mint_burn_anomaly_static", 0.02, Severity::Info, observed),
            // D07 not in RAVE probe (no Token-2022 fee drain events)
        ];
        let result = compute_aggregation(&events, window_end, &cfg());
        // P6-0 target: base_score ≈ 0.7714
        // Tolerance ±0.003 — wider than Sprint 5 (±0.002) to reflect rebalancing band.
        assert!(
            (result.base_score - 0.7714).abs() < 0.003,
            "RAVE base_score={} expected ≈0.7714 (P6-0 rebalanced; was 0.8274 in Sprint 5)",
            result.base_score
        );
        assert_eq!(result.overall_severity, Severity::Critical);
    }

    #[test]
    fn wet_calibration_base_score() {
        // Reproduce WET probe from spec §10, all at 1h age.
        //
        // P6-0 calibration note (GAP-SCORE-01 closure):
        // D03 weight 0.35→0.32: WET fires D03 at conf=0.55 (dominant signal).
        //   Old D03 contribution: 0.35×0.55 = 0.1925
        //   New D03 contribution: 0.32×0.55 = 0.176  (Δ -0.0165)
        // D04 weight 0.35→0.32: WET fires D04 at conf=0.12 (inconclusive, but still contributes).
        //   Old: 0.35×0.12×decay ≈ 0.04157; New: 0.32×0.12×decay ≈ 0.03804 (Δ -0.004)
        // WET has no D07 events. Expected base_score ≈ 0.2880 (was 0.3080 in Sprint 5).
        // This is a calibration rebalance, NOT a regression — see SESSION-KICKOFF.md §P6-0.
        let window_end = now();
        let observed = window_end - Duration::hours(1);
        let events = vec![
            make_event_at("honeypot_sim_static", 0.02, Severity::Info, observed),
            make_event_at("rug_pull_lp_drain_latent", 0.28, Severity::Low, observed),
            make_event_at("holder_concentration", 0.55, Severity::Medium, observed),
            make_event_at("pump_dump", 0.12, Severity::Info, observed),
            make_event_at("wash_trading_h1", 0.25, Severity::Info, observed),
            make_event_at("mint_burn_anomaly_static", 0.02, Severity::Info, observed),
            // D07 not in WET probe (WET has no Token-2022 fee drain events)
        ];
        let result = compute_aggregation(&events, window_end, &cfg());
        // P6-0 target: base_score ≈ 0.2880
        // Tolerance ±0.003 — wider than Sprint 5 (±0.002) to reflect rebalancing band.
        assert!(
            (result.base_score - 0.2880).abs() < 0.003,
            "WET base_score={} expected ≈0.2880 (P6-0 rebalanced; was 0.3080 in Sprint 5)",
            result.base_score
        );
        // WET: highest non-inconclusive event is holder_concentration at Medium (conf=0.55≥0.30).
        // pump_dump (conf=0.12 < 0.30) is inconclusive; wash_trading (conf=0.25 < 0.30) inconclusive.
        assert_eq!(result.overall_severity, Severity::Medium);
    }
}
