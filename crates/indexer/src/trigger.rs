//! Trigger system for the on-demand query engine (ADR 0007 / design 0028 §7).
//!
//! Under the pull-based model, the indexer's primary input is NOT a continuous chain
//! event stream but a bounded `mpsc::Receiver<IndexerTrigger>` channel. Three trigger
//! types are defined:
//!
//! - `EvaluateToken` — synchronous, single-token evaluation; used by REST `/v1/score`.
//! - `RescoreWatchlist` — fan-out to all watchlist tokens; used by the periodic rescore worker.
//! - `ScanForLaunches` — query factory programs for new pools; used by the periodic discovery worker.
//!
//! # Design reference
//!
//! `docs/designs/0028-lightweight-query-engine-deployment.md` §7.1 + §7.2.
//! `docs/adr/0007-pull-based-query-engine.md` §3 (Rule A trigger types).
//!
//! # Verdict cache read-first protocol
//!
//! `EvaluateToken` checks the `VerdictCacheStore` for a fresh non-expired entry before
//! running any detectors. If `expires_at > now()`, the cached result is returned
//! immediately via the oneshot reply channel. On cache miss / expiry, detectors run
//! and the result is upserted into the cache.
//!
//! # Bounded concurrency
//!
//! `MultiChainCoordinator::trigger_evaluate` uses a `tokio::sync::Semaphore` to limit
//! concurrent evaluations to `max_concurrent_evaluations` (config, default 8).
//! Callers block on semaphore acquire — backpressure propagates naturally to the REST
//! handler or periodic worker that dispatched the trigger.
//!
//! # Determinism
//!
//! `VerdictSummary` uses `BTreeMap` for `per_detector_results` (alphabetical key order).
//! `evaluated_at` is `chrono::DateTime<Utc>` from the clock — the ONLY non-deterministic
//! field, analogous to `TokenRiskReport::computed_at` in the scoring crate.
//! The rest of the fields are deterministic given identical chain state.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use mg_onchain_common::anomaly::AnomalyEvent;
use mg_onchain_common::chain::{Address, Chain};

// ---------------------------------------------------------------------------
// EvaluationReason
// ---------------------------------------------------------------------------

/// Why a `trigger_evaluate` call was initiated.
///
/// Recorded in `verdict_cache.reason` for audit purposes. Does NOT affect
/// detector logic — purely an audit trail field.
///
/// ADR 0007 §3 Rule A: four trigger types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationReason {
    /// Triggered by a REST `GET /v1/score` request.
    RestRequest,
    /// Triggered by the periodic watchlist rescore worker.
    WatchlistScan,
    /// Triggered by the periodic rescore ticker (N-minute cadence).
    PeriodicRescore,
    /// Triggered by the new-launch discovery worker on pool discovery.
    NewLaunchDiscovery,
}

impl EvaluationReason {
    /// Returns a static string for storage / logging.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RestRequest => "rest_request",
            Self::WatchlistScan => "watchlist_scan",
            Self::PeriodicRescore => "periodic_rescore",
            Self::NewLaunchDiscovery => "new_launch_discovery",
        }
    }
}

// ---------------------------------------------------------------------------
// DetectorOutcome
// ---------------------------------------------------------------------------

/// Result from a single detector run inside `trigger_evaluate`.
///
/// Stored in `VerdictSummary::per_detector_results` keyed by detector id.
/// Serialized as JSONB in `verdict_cache.detector_results`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorOutcome {
    /// Detector id (e.g. `"honeypot_sim"`, `"pump_dump"`).
    pub detector_id: String,
    /// Maximum confidence from all `AnomalyEvent`s returned by this detector.
    /// `Decimal::ZERO` if the detector returned no events.
    pub confidence: Decimal,
    /// Severity string (e.g. `"HIGH"`, `"MEDIUM"`) of the highest-confidence event.
    /// `None` when the detector returned no events.
    pub severity: Option<String>,
    /// Whether the result was served from verdict cache (true) or freshly computed (false).
    pub cached: bool,
    /// Anomaly events emitted by this detector (may be empty).
    pub events: Vec<AnomalyEvent>,
}

// ---------------------------------------------------------------------------
// VerdictSummary
// ---------------------------------------------------------------------------

/// Aggregated result returned by `MultiChainCoordinator::trigger_evaluate`.
///
/// This is the type that the REST `/v1/score` endpoint receives and serializes
/// into its response body (T26-6 dependency).
///
/// # Determinism
///
/// `per_detector_results` is `BTreeMap<String, DetectorOutcome>` — alphabetically
/// ordered by detector id. Two calls with identical chain state return identical
/// output in all fields except `evaluated_at` (wall-clock, intentionally non-deterministic).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictSummary {
    /// Token address in chain-canonical form.
    pub token: String,
    /// Chain the token lives on.
    pub chain: Chain,
    /// Overall risk score in `[0.0, 1.0]`. Weighted combination of per-detector
    /// confidence values. `Decimal::ZERO` if no detectors fired.
    pub overall_score: Decimal,
    /// Overall severity (highest severity among all detector events).
    /// `None` if no detectors fired.
    pub overall_severity: Option<String>,
    /// Per-detector breakdown. Keyed by detector id (BTreeMap for determinism).
    pub per_detector_results: BTreeMap<String, DetectorOutcome>,
    /// Why this evaluation was initiated.
    pub reason: EvaluationReason,
    /// Wall-clock time the evaluation completed (or when the cache entry was read).
    pub evaluated_at: DateTime<Utc>,
    /// Cache hit: true if this verdict was served from `verdict_cache` without
    /// re-running detectors.
    pub from_cache: bool,
}

// ---------------------------------------------------------------------------
// IndexerTrigger
// ---------------------------------------------------------------------------

/// One-way command dispatched to the coordinator's trigger channel.
///
/// `EvaluateToken` carries a `oneshot::Sender<Result<VerdictSummary, anyhow::Error>>`
/// for synchronous request/response semantics. The other variants are fire-and-forget.
///
/// The channel is bounded (`mpsc::channel(TRIGGER_CHANNEL_CAP)`). Senders that fill
/// the channel block until the coordinator drains it — natural backpressure.
pub enum IndexerTrigger {
    /// Evaluate all relevant detectors for a single token.
    ///
    /// The coordinator returns the result via the `reply` channel.
    /// If no `reply` is provided, the evaluation runs but the result is
    /// discarded after cache write (used by `RescoreWatchlist` fan-out).
    EvaluateToken {
        chain: Chain,
        token: Address,
        reason: EvaluationReason,
        reply: Option<oneshot::Sender<Result<VerdictSummary, String>>>,
    },

    /// Fetch all tokens from the watchlist and enqueue an `EvaluateToken` for each.
    ///
    /// The coordinator handles this internally by reading the watchlist from storage
    /// and sending `EvaluateToken` for each entry. No external reply channel.
    RescoreWatchlist,

    /// Query factory programs for newly created pools and enqueue `EvaluateToken`
    /// for each discovered token not already evaluated within its TTL.
    ScanForLaunches,
}

// ---------------------------------------------------------------------------
// VerdictCacheConfig
// ---------------------------------------------------------------------------

/// Per-detector TTL (minutes) read from `config/detectors.toml [verdict_cache.ttl_minutes]`.
///
/// The indexer loads this at startup and uses it to compute
/// `expires_at = now() + ttl_for_detector_id(detector_id)` when upserting a
/// `CachedVerdict`.
///
/// Keys are config-side detector ids (`d01_honeypot_v1`, …) mapped at lookup time
/// to runtime `Detector::id()` constants. The mapping table is embedded in the TOML
/// comment block in `config/detectors.toml` and in the `TTL_LOOKUP` constant below.
///
/// Default TTL (for any detector not in the table): 60 minutes (slow-moving class).
#[derive(Debug, Clone)]
pub struct VerdictCacheConfig {
    /// Per-detector TTL in minutes. Key = runtime `Detector::id()` value.
    ttl_by_detector_id: BTreeMap<String, i64>,
}

impl VerdictCacheConfig {
    /// Build from the raw `BTreeMap<String, u64>` deserialized from TOML.
    ///
    /// The TOML uses config-side ids (`d01_honeypot_v1`). This constructor translates
    /// them to runtime detector ids using the static mapping in `TTL_LOOKUP`.
    pub fn from_toml_map(raw: &std::collections::HashMap<String, u64>) -> Self {
        let mut ttl_by_detector_id = BTreeMap::new();
        for (config_key, &minutes) in raw {
            let runtime_id = config_key_to_detector_id(config_key);
            ttl_by_detector_id.insert(runtime_id.to_owned(), minutes as i64);
        }
        Self { ttl_by_detector_id }
    }

    /// Build from an explicit `BTreeMap` (used in tests).
    pub fn from_btree(map: BTreeMap<String, i64>) -> Self {
        Self {
            ttl_by_detector_id: map,
        }
    }

    /// Return the TTL `Duration` for a given runtime `Detector::id()`.
    ///
    /// Falls back to 60 minutes if the detector id is not in the config map.
    /// The fallback is conservative (slow-moving class) — an unknown detector
    /// is treated as a slow-moving signal.
    pub fn ttl_for(&self, detector_id: &str) -> Duration {
        let minutes = self
            .ttl_by_detector_id
            .get(detector_id)
            .copied()
            .unwrap_or(60);
        Duration::minutes(minutes)
    }
}

impl Default for VerdictCacheConfig {
    /// Defaults per ADR 0007 §9.5 TTL classes.
    fn default() -> Self {
        let mut map = BTreeMap::new();
        // Fast-moving signals (5 min)
        map.insert("pump_dump".to_owned(), 5i64);
        map.insert("wash_trading_h1".to_owned(), 5i64);
        map.insert("synchronized_activity_v1".to_owned(), 5i64);
        map.insert("sandwich_mev_v1".to_owned(), 5i64);
        // Honeypot (15 min)
        map.insert("honeypot_sim".to_owned(), 15i64);
        // Slow-moving signals (60 min) — remaining detectors inherit the default fallback
        map.insert("rug_pull_lp_drain".to_owned(), 60i64);
        map.insert("holder_concentration".to_owned(), 60i64);
        map.insert("mint_burn_anomaly".to_owned(), 60i64);
        map.insert("withdraw_withheld_drain".to_owned(), 60i64);
        map.insert("sybil_detection".to_owned(), 60i64);
        map.insert("deployer_changepoint".to_owned(), 60i64);
        map.insert("launch_audit".to_owned(), 60i64);
        map.insert("permit2_drainer_v1".to_owned(), 60i64);
        map.insert("bridge_drain_v1".to_owned(), 60i64);
        Self {
            ttl_by_detector_id: map,
        }
    }
}

/// Translate a TOML config-side detector id to the runtime `Detector::id()` value.
///
/// The TOML uses a versioned naming convention (`d01_honeypot_v1`) while the
/// runtime `DETECTOR_ID` constants use descriptive names (`honeypot_sim`).
/// This function provides the static mapping. Unknown config keys are passed
/// through unchanged (forward-compatible: new detectors added later will have
/// their config key used directly if no mapping is defined).
fn config_key_to_detector_id(config_key: &str) -> &str {
    match config_key {
        "d01_honeypot_v1" => "honeypot_sim",
        "d02_rug_pull_v1" => "rug_pull_lp_drain",
        "d03_concentration_v1" => "holder_concentration",
        "d04_pump_dump_v1" => "pump_dump",
        "d05_wash_trading_v1" => "wash_trading_h1",
        "d06_mint_burn_v1" => "mint_burn_anomaly",
        "d07_withdraw_withheld_v1" => "withdraw_withheld_drain",
        "d08_sybil_v1" => "sybil_detection",
        "d09_bocpd_deployer_v1" => "deployer_changepoint",
        "d10_launch_audit_v1" => "launch_audit",
        "d11_synchronized_v1" => "synchronized_activity_v1",
        "d12_permit2_drainer_v1" => "permit2_drainer_v1",
        "d13_sandwich_mev_v1" => "sandwich_mev_v1",
        // Forward-compatible: unknown keys pass through unchanged.
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // VerdictCacheConfig::default() provides expected TTLs
    // -----------------------------------------------------------------------

    #[test]
    fn default_ttl_fast_signals_are_5_minutes() {
        let cfg = VerdictCacheConfig::default();
        assert_eq!(cfg.ttl_for("pump_dump"), Duration::minutes(5));
        assert_eq!(cfg.ttl_for("wash_trading_h1"), Duration::minutes(5));
        assert_eq!(cfg.ttl_for("synchronized_activity_v1"), Duration::minutes(5));
        assert_eq!(cfg.ttl_for("sandwich_mev_v1"), Duration::minutes(5));
    }

    #[test]
    fn default_ttl_honeypot_is_15_minutes() {
        let cfg = VerdictCacheConfig::default();
        assert_eq!(cfg.ttl_for("honeypot_sim"), Duration::minutes(15));
    }

    #[test]
    fn default_ttl_slow_signals_are_60_minutes() {
        let cfg = VerdictCacheConfig::default();
        assert_eq!(cfg.ttl_for("rug_pull_lp_drain"), Duration::minutes(60));
        assert_eq!(cfg.ttl_for("holder_concentration"), Duration::minutes(60));
        assert_eq!(cfg.ttl_for("mint_burn_anomaly"), Duration::minutes(60));
        assert_eq!(cfg.ttl_for("sybil_detection"), Duration::minutes(60));
    }

    #[test]
    fn default_ttl_unknown_detector_falls_back_to_60_minutes() {
        let cfg = VerdictCacheConfig::default();
        assert_eq!(cfg.ttl_for("some_future_detector_v99"), Duration::minutes(60));
    }

    // -----------------------------------------------------------------------
    // VerdictCacheConfig::from_toml_map translates config keys correctly
    // -----------------------------------------------------------------------

    #[test]
    fn from_toml_map_translates_config_keys_to_runtime_ids() {
        let mut raw: HashMap<String, u64> = HashMap::new();
        raw.insert("d01_honeypot_v1".to_owned(), 15);
        raw.insert("d04_pump_dump_v1".to_owned(), 5);
        raw.insert("d02_rug_pull_v1".to_owned(), 60);

        let cfg = VerdictCacheConfig::from_toml_map(&raw);
        assert_eq!(cfg.ttl_for("honeypot_sim"), Duration::minutes(15));
        assert_eq!(cfg.ttl_for("pump_dump"), Duration::minutes(5));
        assert_eq!(cfg.ttl_for("rug_pull_lp_drain"), Duration::minutes(60));
    }

    #[test]
    fn from_toml_map_unknown_key_passthrough() {
        let mut raw: HashMap<String, u64> = HashMap::new();
        raw.insert("future_detector_v1".to_owned(), 30);

        let cfg = VerdictCacheConfig::from_toml_map(&raw);
        assert_eq!(cfg.ttl_for("future_detector_v1"), Duration::minutes(30));
    }

    // -----------------------------------------------------------------------
    // EvaluationReason::as_str round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn evaluation_reason_as_str_values_are_correct() {
        assert_eq!(EvaluationReason::RestRequest.as_str(), "rest_request");
        assert_eq!(EvaluationReason::WatchlistScan.as_str(), "watchlist_scan");
        assert_eq!(EvaluationReason::PeriodicRescore.as_str(), "periodic_rescore");
        assert_eq!(EvaluationReason::NewLaunchDiscovery.as_str(), "new_launch_discovery");
    }

    // -----------------------------------------------------------------------
    // VerdictSummary BTreeMap key order
    // -----------------------------------------------------------------------

    #[test]
    fn verdict_summary_per_detector_results_is_sorted() {
        // BTreeMap iteration order must be alphabetical.
        let mut results = BTreeMap::new();
        results.insert("pump_dump".to_owned(), DetectorOutcome {
            detector_id: "pump_dump".to_owned(),
            confidence: Decimal::ZERO,
            severity: None,
            cached: false,
            events: vec![],
        });
        results.insert("honeypot_sim".to_owned(), DetectorOutcome {
            detector_id: "honeypot_sim".to_owned(),
            confidence: Decimal::ZERO,
            severity: None,
            cached: false,
            events: vec![],
        });
        results.insert("rug_pull_lp_drain".to_owned(), DetectorOutcome {
            detector_id: "rug_pull_lp_drain".to_owned(),
            confidence: Decimal::ZERO,
            severity: None,
            cached: false,
            events: vec![],
        });

        let mut keys = results.keys();
        // Alphabetical: honeypot_sim < pump_dump < rug_pull_lp_drain
        assert_eq!(keys.next().unwrap(), "honeypot_sim");
        assert_eq!(keys.next().unwrap(), "pump_dump");
        assert_eq!(keys.next().unwrap(), "rug_pull_lp_drain");
    }
}
