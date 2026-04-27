//! Output types for `crates/scoring`.
//!
//! All output structs implement `Serialize + Deserialize` for the three delivery
//! modes (in-process crate, REST JSON, WebSocket frame) per ADR 0001 §D8.
//!
//! # Determinism
//!
//! `TokenRiskReport` uses `BTreeMap` for `per_detector` and `Vec` with deterministic
//! ordering for `top_evidence`. The only non-deterministic field is `computed_at`,
//! which captures wall-clock time and is intentionally excluded from determinism checks.
//!
//! # Serialization
//!
//! `rename_all = "camelCase"` matches the project-wide convention from
//! `crates/common` (`AnomalyEvent`, `TokenMeta`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use mg_onchain_common::anomaly::{Confidence, Severity};
use mg_onchain_common::chain::{Address, Chain};

use crate::config::ScoringConfig;

// ---------------------------------------------------------------------------
// TokenRiskReport
// ---------------------------------------------------------------------------

/// Aggregated token risk assessment from all detector outputs in a time window.
///
/// The primary output of [`crate::ScoringEngine::score`].
///
/// # Consumer guidance (spec §15 OQ5)
///
/// `overall_score` drives ranking and filtering. `overall_severity` drives per-token
/// action policy. Consumers MUST check both:
///
/// - `overall_score > 0.7` — High risk / block
/// - `overall_score ∈ [0.4, 0.7]` — Medium risk / review
/// - `overall_score < 0.4` — Low risk / proceed
/// - `overall_severity == Critical` — immediate action regardless of score
///
/// These thresholds live in consumer config, NOT in `ScoringConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRiskReport {
    /// Token address in chain-canonical form.
    pub token: Address,

    /// Which chain the token lives on.
    pub chain: Chain,

    /// Time window this report covers (`(window_start, window_end)`).
    pub window: (DateTime<Utc>, DateTime<Utc>),

    /// Wall-clock time the report was produced. The ONLY non-deterministic field.
    /// Two calls with the same inputs but different `observed_at` will produce
    /// identical output in all fields EXCEPT this one.
    pub computed_at: DateTime<Utc>,

    /// Overall risk score ∈ [0.0, 1.0], after attenuation multipliers.
    /// Use for portfolio-level ranking and threshold filtering.
    pub overall_score: Confidence,

    /// Pre-attenuation score before jup/established/age multipliers.
    /// Useful for debugging and consumer-side attenuation overrides.
    pub base_score: Confidence,

    /// Worst-case severity across all fired events (where confidence ≥ inconclusive_floor).
    ///
    /// # OQ3 resolution
    ///
    /// `overall_severity` excludes events with `confidence < inconclusive_floor`
    /// (default 0.30). Very-low-confidence events are classified as inconclusive noise
    /// and MUST NOT drive severity escalation. Rationale: a Critical event at 0.05
    /// confidence is noise — escalating severity for it would cause constant false alarms.
    /// When ALL events are below the floor, severity defaults to `Severity::Info`.
    pub overall_severity: Severity,

    /// Per-detector breakdown. Keys are canonical detector IDs (`snake_case`).
    /// `BTreeMap` guarantees deterministic alphabetical key ordering.
    ///
    /// All 7 known detectors appear as keys even if no events fired (fired_events=0). (D07 added P6-0)
    pub per_detector: BTreeMap<String, DetectorScore>,

    /// Top N evidence highlights across all events, ranked by severity then confidence.
    /// N = `config_snapshot.evidence_highlight_count` (default 5).
    pub top_evidence: Vec<EvidenceHighlight>,

    /// Signal counts across all events in the window.
    pub signal_counts: SignalCounts,

    /// Coverage: which detectors produced events vs which were skipped and why.
    pub coverage: CoverageReport,

    /// The exact `ScoringConfig` that produced this report. Stored for reproducibility
    /// auditing — consumers can verify any report by replaying with this config.
    pub config_snapshot: ScoringConfig,
}

// ---------------------------------------------------------------------------
// DetectorScore
// ---------------------------------------------------------------------------

/// Per-detector breakdown within a `TokenRiskReport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorScore {
    /// Canonical detector ID, e.g. `"holder_concentration"`.
    pub detector_id: String,

    /// Number of `AnomalyEvent` entries from this detector where
    /// `effective_confidence >= inconclusive_floor`.
    pub fired_events: u32,

    /// Events where `effective_confidence < inconclusive_floor` (classified as
    /// inconclusive rather than fired).
    pub inconclusive_events: u32,

    /// Events from this detector that were suppressed at the detector level.
    /// Sourced from `event.evidence.metrics["<detector_id>/suppressed_count"]` on
    /// the highest-confidence event. Defaults to 0 if the metric key is absent.
    pub suppressed_events: u32,

    /// Maximum confidence among fired events — raw value, no decay applied.
    /// Zero if no events fired.
    pub max_confidence: Confidence,

    /// Time-decay-weighted average confidence across fired events.
    /// Uses `effective_confidence = raw_confidence × decay_factor` per event,
    /// then averages. Zero if no events fired.
    pub weighted_confidence: Confidence,

    /// Maximum severity among fired events. `Severity::Info` if no events fired.
    pub severity: Severity,

    /// Top 3 `(metric_key, metric_value)` evidence pairs, ranked by severity + confidence.
    pub evidence_summary: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// EvidenceHighlight
// ---------------------------------------------------------------------------

/// A single high-impact evidence entry surfaced in `TokenRiskReport::top_evidence`.
///
/// # Key format
///
/// `key` uses the detector-prefixed convention from `crates/common` §Evidence metric
/// key convention: `"<detector_id>/<metric_name>"`, e.g. `"rug_pull_lp_drain/lp_removed_pct"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceHighlight {
    /// The detector that produced this evidence.
    pub detector_id: String,

    /// The severity of the event this evidence came from.
    pub severity: Severity,

    /// Raw confidence of the event (no decay). Used for display, not scoring.
    pub confidence: Confidence,

    /// Full metric key with detector prefix, e.g. `"holder_concentration/gini_delta_24h"`.
    pub key: String,

    /// String-encoded Decimal metric value, e.g. `"0.92"`.
    pub value: String,

    /// First note from `AnomalyEvent.evidence.notes`, if any.
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// SignalCounts
// ---------------------------------------------------------------------------

/// Event counts across all detectors in the scoring window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalCounts {
    /// Events with `confidence >= inconclusive_floor`. These drive `overall_score`.
    pub fired: u32,

    /// Events with `confidence < inconclusive_floor`. Recorded but do not drive score.
    pub inconclusive: u32,

    /// Events at `Severity::Info` (detector-internal floor). These are structural
    /// state readings that fell below the detector's own signal threshold. They are
    /// in the event stream as coverage evidence, not as anomaly signals.
    pub suppressed_info: u32,
}

// ---------------------------------------------------------------------------
// CoverageReport
// ---------------------------------------------------------------------------

/// Which detectors had the opportunity to fire in this window.
///
/// Gives consumers the critical distinction between:
/// - "this detector fired zero events because the signal is clean" (`detectors_run`)
/// - "this detector was never run because the required data was absent" (`detectors_skipped`)
///
/// The second case is a data gap that reduces trust in `overall_score`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    /// Detector IDs that produced ≥1 `AnomalyEvent` in the window.
    /// Sorted alphabetically (deterministic).
    pub detectors_run: Vec<String>,

    /// Detectors that the caller skipped, with the reason.
    pub detectors_skipped: Vec<SkipReason>,

    /// Informational completeness ratio: (detectors_run + detectors_skipped_with_reason)
    /// / total_known_detectors. Range [0.0, 1.0]. 1.0 = full coverage.
    ///
    /// This is informational only — does NOT affect `overall_score`.
    pub coverage_completeness: f32,
}

/// Why a detector was not run in this scoring window.
///
/// # OQ2 resolution
///
/// This is a typed struct (not a free-form string) for type safety. The `reason`
/// field carries a human-readable description that the caller populates from the
/// scheduler's knowledge of what went wrong (missing data, config flag, etc.).
/// Using a struct rather than an enum avoids tight coupling between scheduling logic
/// and the scoring crate's type definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipReason {
    /// Which detector was skipped.
    pub detector_id: String,

    /// Human-readable reason, e.g. `"no swap events in window"`,
    /// `"insufficient holder snapshots: only 1 snapshot, need ≥2 for delta signals"`,
    /// `"D01 simulation disabled (config: simulation_enabled=false)"`.
    pub reason: String,
}
