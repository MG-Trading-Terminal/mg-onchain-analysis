//! Top-evidence ranking: select the highest-impact evidence entries across all events.
//!
//! # Algorithm (spec §7)
//!
//! 1. Collect all `(key, Decimal)` pairs from `evidence.metrics` across every
//!    `AnomalyEvent` in the window.
//! 2. Assign a sort key: `(severity_ordinal DESC, confidence DESC, key ASC)`.
//!    The `key ASC` tiebreaker guarantees determinism.
//! 3. Deduplicate by `(detector_id, key)`: keep only the highest-ranked occurrence
//!    of each unique (detector, metric-key) pair.
//! 4. Take the top N entries.
//!
//! # Determinism
//!
//! No `HashMap` is used. All intermediate collections are `BTreeMap` or sorted `Vec`.
//! The stable sort over `(severity_ordinal DESC, confidence DESC, key ASC)` guarantees
//! identical output given identical inputs.

use std::collections::BTreeMap;

use mg_onchain_common::anomaly::{AnomalyEvent, Severity};

use crate::types::EvidenceHighlight;

/// Severity as a sortable ordinal (higher is more severe).
fn severity_ordinal(s: Severity) -> u8 {
    match s {
        Severity::Info => 0,
        Severity::Low => 1,
        Severity::Medium => 2,
        Severity::High => 3,
        Severity::Critical => 4,
        // Exhaustive: Severity is #[non_exhaustive] in common but all variants are covered.
        // If a new variant is added, the compiler will warn on the wildcard below.
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

/// A candidate evidence entry before deduplication.
#[derive(Debug)]
struct Candidate {
    detector_id: String,
    severity: Severity,
    confidence_val: f64,
    key: String,
    value: String,
    note: Option<String>,
}

/// Rank and select the top `max_count` evidence entries across all events.
///
/// # Arguments
///
/// * `events` — all events in the scoring window (pre-sorted by caller for determinism).
/// * `max_count` — maximum number of highlights to return.
///
/// # Returns
///
/// A `Vec<EvidenceHighlight>` of at most `max_count` entries in ranked order:
/// highest severity first, then highest confidence, then alphabetical key as tiebreaker.
pub fn rank_top_evidence(events: &[AnomalyEvent], max_count: usize) -> Vec<EvidenceHighlight> {
    if max_count == 0 || events.is_empty() {
        return Vec::new();
    }

    // Collect all (detector_id, key) → best candidate.
    // Use BTreeMap<(detector_id, key), Candidate> for deterministic deduplication.
    let mut best: BTreeMap<(String, String), Candidate> = BTreeMap::new();

    for event in events {
        for (key, decimal_val) in &event.evidence.metrics {
            let candidate = Candidate {
                detector_id: event.detector_id.clone(),
                severity: event.severity,
                confidence_val: event.confidence.value(),
                key: key.clone(),
                value: decimal_val.to_string(),
                note: event.evidence.notes.first().cloned(),
            };

            let map_key = (event.detector_id.clone(), key.clone());

            match best.get(&map_key) {
                None => {
                    best.insert(map_key, candidate);
                }
                Some(existing) => {
                    // Replace if the new candidate ranks higher.
                    // Ranking: severity DESC, confidence DESC.
                    let new_sev = severity_ordinal(candidate.severity);
                    let old_sev = severity_ordinal(existing.severity);
                    let should_replace = (new_sev, candidate.confidence_val)
                        > (old_sev, existing.confidence_val);
                    if should_replace {
                        best.insert(map_key, candidate);
                    }
                }
            }
        }
    }

    // Collect into a Vec and sort by (severity DESC, confidence DESC, key ASC).
    let mut candidates: Vec<Candidate> = best.into_values().collect();
    candidates.sort_by(|a, b| {
        let sev_cmp = severity_ordinal(b.severity).cmp(&severity_ordinal(a.severity));
        if sev_cmp != std::cmp::Ordering::Equal {
            return sev_cmp;
        }
        let conf_cmp = b
            .confidence_val
            .partial_cmp(&a.confidence_val)
            .unwrap_or(std::cmp::Ordering::Equal);
        if conf_cmp != std::cmp::Ordering::Equal {
            return conf_cmp;
        }
        // Tiebreaker: key ASC (then detector_id ASC) for determinism.
        let key_cmp = a.key.cmp(&b.key);
        if key_cmp != std::cmp::Ordering::Equal {
            return key_cmp;
        }
        a.detector_id.cmp(&b.detector_id)
    });

    // Take top N and convert.
    candidates
        .into_iter()
        .take(max_count)
        .map(|c| {
            // Safety: confidence values on AnomalyEvent are guaranteed valid [0,1].
            let confidence = mg_onchain_common::anomaly::Confidence::new(c.confidence_val)
                .unwrap_or(mg_onchain_common::anomaly::Confidence::ZERO);
            EvidenceHighlight {
                detector_id: c.detector_id,
                severity: c.severity,
                confidence,
                key: c.key,
                value: c.value,
                note: c.note,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_common::make_event;
    use mg_onchain_common::anomaly::{Evidence, Severity};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[test]
    fn empty_events_returns_empty() {
        let result = rank_top_evidence(&[], 5);
        assert!(result.is_empty());
    }

    #[test]
    fn max_count_zero_returns_empty() {
        let e = make_event("pump_dump", 0.9, Severity::Critical);
        let result = rank_top_evidence(&[e], 0);
        assert!(result.is_empty());
    }

    #[test]
    fn critical_outranks_low_regardless_of_confidence() {
        // Spec §7: severity dominates over confidence.
        let mut e_low = make_event("wash_trading_h1", 0.99, Severity::Low);
        e_low.evidence = Evidence::new().with_metric("wash_trading_h1/metric_a", dec!(0.99));

        let mut e_crit = make_event("pump_dump", 0.55, Severity::Critical);
        e_crit.evidence = Evidence::new().with_metric("pump_dump/metric_b", dec!(0.55));

        let result = rank_top_evidence(&[e_low, e_crit], 5);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].severity, Severity::Critical, "Critical must rank first");
        assert_eq!(result[1].severity, Severity::Low, "Low must rank second");
    }

    #[test]
    fn same_severity_higher_confidence_ranks_first() {
        let mut e_low_conf = make_event("pump_dump", 0.60, Severity::High);
        e_low_conf.evidence = Evidence::new().with_metric("pump_dump/vol_spike", dec!(1.5));

        let mut e_high_conf = make_event("holder_concentration", 0.90, Severity::High);
        e_high_conf.evidence =
            Evidence::new().with_metric("holder_concentration/gini", dec!(0.85));

        let result = rank_top_evidence(&[e_low_conf, e_high_conf], 5);
        assert_eq!(result[0].confidence.value(), 0.90, "higher confidence ranks first");
        assert_eq!(result[1].confidence.value(), 0.60);
    }

    #[test]
    fn key_asc_tiebreaker_is_deterministic() {
        // Same severity and confidence — key ASC must decide.
        let mut e1 = make_event("pump_dump", 0.80, Severity::High);
        e1.evidence = Evidence::new()
            .with_metric("pump_dump/zzz_metric", dec!(1))
            .with_metric("pump_dump/aaa_metric", dec!(2));

        let result = rank_top_evidence(&[e1], 5);
        // aaa < zzz → aaa should rank first
        assert_eq!(result[0].key, "pump_dump/aaa_metric");
        assert_eq!(result[1].key, "pump_dump/zzz_metric");
    }

    #[test]
    fn deduplication_keeps_best_by_severity_then_confidence() {
        // Two events for the same detector+key — keep the one with higher severity.
        let mut e_low_sev = make_event("pump_dump", 0.99, Severity::Low);
        e_low_sev.evidence = Evidence::new().with_metric("pump_dump/vol_spike", dec!(99));

        let mut e_high_sev = make_event("pump_dump", 0.50, Severity::Critical);
        e_high_sev.evidence = Evidence::new().with_metric("pump_dump/vol_spike", dec!(50));

        let result = rank_top_evidence(&[e_low_sev, e_high_sev], 5);
        // Only one entry for pump_dump/vol_spike; the Critical one wins.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].severity, Severity::Critical);
    }

    #[test]
    fn max_count_limits_output() {
        let mut events = Vec::new();
        for i in 0..10u32 {
            let mut e = make_event("pump_dump", 0.5, Severity::Medium);
            e.evidence = Evidence::new()
                .with_metric(format!("pump_dump/metric_{i:02}"), Decimal::from(i));
            events.push(e);
        }
        let result = rank_top_evidence(&events, 3);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn note_propagated_from_event() {
        let mut e = make_event("pump_dump", 0.9, Severity::High);
        e.evidence = Evidence::new()
            .with_metric("pump_dump/vol_spike", dec!(3.5))
            .with_note("creator dumped 94% in 2 txs");

        let result = rank_top_evidence(&[e], 5);
        assert_eq!(result[0].note.as_deref(), Some("creator dumped 94% in 2 txs"));
    }

    #[test]
    fn value_is_decimal_string() {
        let mut e = make_event("holder_concentration", 0.95, Severity::Critical);
        e.evidence =
            Evidence::new().with_metric("holder_concentration/gini", dec!(0.847));

        let result = rank_top_evidence(&[e], 5);
        // Decimal::to_string() of dec!(0.847) = "0.847"
        assert_eq!(result[0].value, "0.847");
    }

    #[test]
    fn no_metrics_produces_no_highlights() {
        let e = make_event("honeypot_sim", 0.05, Severity::Info);
        // Evidence has no metrics.
        let result = rank_top_evidence(&[e], 5);
        assert!(result.is_empty());
    }
}
