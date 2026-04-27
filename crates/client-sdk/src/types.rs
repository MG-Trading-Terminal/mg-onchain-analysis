//! Client-side request / response types mirroring `docs/api/openapi.yaml`.
//!
//! # Re-exports vs local copies
//!
//! - `AnomalyEvent`, `Severity`, `Confidence`, `Evidence` — re-exported from
//!   `mg-onchain-common` (the SDK does depend on the common crate).
//! - `TokenRiskReport` family (`DetectorScore`, `EvidenceHighlight`,
//!   `SignalCounts`, `CoverageReport`, `SkipReason`, `ScoringConfigSnapshot`) —
//!   defined locally here. Rationale: consumers must not pull `mg-onchain-scoring`
//!   (which brings detectors, storage, axum, etc.) just to get the report shape.
//!   The types below are wire-format compatible: `#[serde(rename_all = "camelCase")]`
//!   matches the scoring crate's serialisation so JSON round-trips transparently.
//!
//! # Numeric conventions
//!
//! All monetary / ratio / score fields that could be `f64` in JSON are mapped to
//! `f64` here ONLY when they represent probabilities or normalised scores (0.0..1.0).
//! The `Confidence` wrapper from `common` is used for those. `Decimal` is used for
//! any monetary quantity.
//!
//! `coverage_completeness` is `f32` matching the gateway serialisation.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
pub use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
pub use mg_onchain_common::token::TokenMeta;

// ---------------------------------------------------------------------------
// TokenRiskReport (SDK-local mirror of scoring::types::TokenRiskReport)
// ---------------------------------------------------------------------------

/// Aggregated token risk report returned by `/v1/tokens/analyze` and
/// `/v1/tokens/{chain}/{mint}/risk`.
///
/// Wire-format compatible with `mg-onchain-scoring::TokenRiskReport` — any
/// JSON produced by the gateway deserialises correctly into this type.
///
/// Note: `config_snapshot` is the `ScoringConfigSnapshot` defined below, not the
/// full `ScoringConfig` from the scoring crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRiskReport {
    /// Token address (chain-canonical).
    pub token: Address,
    /// Chain the token lives on.
    pub chain: Chain,
    /// Time window this report covers `(start, end)`.
    pub window: (DateTime<Utc>, DateTime<Utc>),
    /// Wall-clock time when this report was produced.
    pub computed_at: DateTime<Utc>,
    /// Overall risk score ∈ [0.0, 1.0] after attenuation.
    pub overall_score: Confidence,
    /// Pre-attenuation score.
    pub base_score: Confidence,
    /// Worst-case severity across all fired events.
    pub overall_severity: Severity,
    /// Per-detector breakdown. Keys are canonical detector IDs in alphabetical order.
    pub per_detector: BTreeMap<String, DetectorScore>,
    /// Top evidence highlights ranked by (severity DESC, confidence DESC).
    pub top_evidence: Vec<EvidenceHighlight>,
    /// Signal counts across all events in the window.
    pub signal_counts: SignalCounts,
    /// Coverage: which detectors ran vs which were skipped.
    pub coverage: CoverageReport,
    /// Scoring config snapshot used to produce this report.
    pub config_snapshot: ScoringConfigSnapshot,
}

// ---------------------------------------------------------------------------
// DetectorScore
// ---------------------------------------------------------------------------

/// Per-detector breakdown within a `TokenRiskReport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorScore {
    /// Stable detector identifier, e.g. `"rug_pull_lp_drain"`.
    pub detector_id: String,
    /// Events with confidence >= inconclusive_floor.
    pub fired_events: u32,
    /// Events with confidence < inconclusive_floor.
    pub inconclusive_events: u32,
    /// Events suppressed at the detector level.
    pub suppressed_events: u32,
    /// Maximum confidence among fired events (raw, no decay).
    pub max_confidence: Confidence,
    /// Time-decay-weighted average confidence.
    pub weighted_confidence: Confidence,
    /// Maximum severity among fired events.
    pub severity: Severity,
    /// Top 3 evidence key-value pairs.
    pub evidence_summary: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// EvidenceHighlight
// ---------------------------------------------------------------------------

/// A single high-impact evidence entry from `TokenRiskReport::top_evidence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceHighlight {
    /// The detector that produced this evidence.
    pub detector_id: String,
    /// Severity of the source event.
    pub severity: Severity,
    /// Raw confidence of the source event.
    pub confidence: Confidence,
    /// Metric key with detector prefix, e.g. `"rug_pull_lp_drain/lp_removed_pct"`.
    pub key: String,
    /// String-encoded Decimal value.
    pub value: String,
    /// First note from the source event's evidence, if any.
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// SignalCounts
// ---------------------------------------------------------------------------

/// Event counts across all detectors in the scoring window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalCounts {
    /// Events with confidence >= inconclusive_floor.
    pub fired: u32,
    /// Events with confidence < inconclusive_floor.
    pub inconclusive: u32,
    /// Events at Severity::Info (suppressed audit notices).
    pub suppressed_info: u32,
}

// ---------------------------------------------------------------------------
// CoverageReport / SkipReason
// ---------------------------------------------------------------------------

/// Which detectors ran vs were skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    /// Detector IDs that produced ≥1 event.
    pub detectors_run: Vec<String>,
    /// Detectors skipped with reasons.
    pub detectors_skipped: Vec<SkipReason>,
    /// Completeness ratio in [0.0, 1.0].
    pub coverage_completeness: f32,
}

/// Why a detector was not run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipReason {
    /// Detector that was skipped.
    pub detector_id: String,
    /// Human-readable reason.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// ScoringConfigSnapshot
// ---------------------------------------------------------------------------

/// The scoring configuration used to produce a `TokenRiskReport`.
/// SDK-local mirror of `ScoringConfig` — no code from `mg-onchain-scoring` needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoringConfigSnapshot {
    /// Per-detector importance weights (must sum to 1.0 ± 0.001).
    pub detector_weights: BTreeMap<String, f64>,
    /// Exponential decay half-life in hours.
    pub decay_half_life_hours: f64,
    /// Confidence floor below which events are inconclusive.
    pub inconclusive_floor: f64,
    /// Maximum entries in `top_evidence`.
    pub evidence_highlight_count: usize,
    /// Optional: jup_strict attenuation multiplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jup_strict_multiplier: Option<f64>,
    /// Optional: jup_verified attenuation multiplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jup_verified_multiplier: Option<f64>,
    /// Optional: established-protocol attenuation multiplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub established_protocol_multiplier: Option<f64>,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/tokens/analyze`.
#[derive(Debug, Clone, Serialize)]
pub struct AnalyzeRequest {
    /// Chain identifier.
    pub chain: Chain,
    /// Token address in chain-canonical form.
    pub mint: String,
    /// Observation window in hours (1–168). Default 24.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_hours: Option<u32>,
}

/// Response from `POST /v1/tokens/analyze`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeResponse {
    /// The computed or cached risk report.
    pub report: TokenRiskReport,
    /// Total analysis time in milliseconds. 0 if served from cache.
    pub analysis_duration_ms: u64,
    /// Age of cache entry in seconds. Present only when served from cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_seconds: Option<u64>,
}

/// Response from `GET /v1/tokens/{chain}/{mint}/risk`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskResponse {
    /// The risk report.
    pub report: TokenRiskReport,
    /// True if the report came from the in-memory cache.
    pub cached: bool,
    /// Age of cache entry in seconds. Present when `cached = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// AnomalyEventPage
// ---------------------------------------------------------------------------

/// Cursor-paginated response from `GET /v1/anomaly_events`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnomalyEventPage {
    /// Events in this page, as raw JSON values to avoid chain-context issues
    /// with `Address` and `TxHash` deserialization.
    ///
    /// Use `serde_json::from_value::<AnomalyEvent>(event)` or access fields
    /// via the `Value` API.
    pub events: Vec<serde_json::Value>,
    /// Opaque cursor for the next page. `None` when there are no more results.
    pub next_cursor: Option<String>,
    /// Number of events in this page.
    pub total_in_page: usize,
}

// ---------------------------------------------------------------------------
// Events filter
// ---------------------------------------------------------------------------

/// Filter parameters for `list_anomaly_events`.
#[derive(Debug, Clone, Default)]
pub struct EventsFilter {
    /// Filter by chain.
    pub chain: Option<Chain>,
    /// Filter by token address (chain-canonical).
    pub token: Option<String>,
    /// Filter by detector ID.
    pub detector_id: Option<String>,
    /// Minimum severity (inclusive).
    pub severity_min: Option<Severity>,
    /// Inclusive lower bound on `observed_at`.
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `observed_at`.
    pub to: Option<DateTime<Utc>>,
    /// Max events per page (1–500). Default 50.
    pub limit: Option<u32>,
    /// Opaque cursor from the previous page's `next_cursor`.
    pub cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// DetectorInfo (GET /v1/detectors)
// ---------------------------------------------------------------------------

/// Configured detector with its thresholds and references.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorInfo {
    /// Stable snake_case detector identifier.
    pub id: String,
    /// Minimum severity this detector emits.
    pub severity_floor: Severity,
    /// Whether this detector is active in the current config.
    pub enabled: bool,
    /// Named threshold entries from `config/detectors.toml`.
    pub thresholds: BTreeMap<String, serde_json::Value>,
    /// REFERENCES.md entry slugs.
    pub references: Vec<String>,
}

/// Response from `GET /v1/detectors`.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectorListResponse {
    pub detectors: Vec<DetectorInfo>,
}

// ---------------------------------------------------------------------------
// AnomalyFilter (for WebSocket subscribe)
// ---------------------------------------------------------------------------

/// Filter for the `subscribe_anomalies` WebSocket stream.
#[derive(Debug, Clone, Default)]
pub struct AnomalyFilter {
    /// Subscribe to events for this chain only.
    pub chain: Option<Chain>,
    /// Subscribe only to events for these token addresses.
    pub tokens: Option<Vec<String>>,
    /// Subscribe only to these detector IDs.
    pub detector_ids: Option<Vec<String>>,
    /// Minimum severity to receive (inclusive).
    pub severity_min: Option<Severity>,
}

// ---------------------------------------------------------------------------
// StreamMessage
// ---------------------------------------------------------------------------

/// Messages emitted by the `AnomalyStream` returned from `subscribe_anomalies`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StreamMessage {
    /// A new anomaly event matching the subscription filter.
    Anomaly(serde_json::Value),
    /// An updated `TokenRiskReport` push (score delta exceeded threshold).
    RiskUpdate {
        report: serde_json::Value,
        previous_score: f64,
        delta: f64,
    },
    /// The server dropped events due to buffer overflow. The consumer should
    /// reconnect with `resume_from` if gap-free delivery is required.
    LagNotice {
        /// Number of events dropped since the last lag notice.
        dropped: u64,
    },
    /// The SDK successfully reconnected after a disconnect.
    Reconnected,
    /// `resume_from` was rejected (event window expired — > 5 min lookback).
    ResumeFailed {
        /// The `from_id` that was rejected.
        lost_window: String,
    },
}

// ---------------------------------------------------------------------------
// Auth types (POST /v1/auth/token)
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/auth/token`.
#[derive(Debug, Serialize)]
pub struct AuthRequest {
    pub username: String,
    pub password: String,
}

/// Response from `POST /v1/auth/token`.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub scopes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Response from `GET /health`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    pub storage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_detail: Option<String>,
    pub scoring: String,
    pub detectors: String,
    pub registry: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_detail: Option<String>,
    pub uptime_seconds: u64,
}
