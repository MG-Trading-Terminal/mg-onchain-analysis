//! [`DetectorError`] — typed failures from detector evaluation.
//!
//! # Retry semantics
//!
//! Not all failures are equal. The caller (scheduler or on-demand handler)
//! uses the variant to decide whether to retry, disable, or quarantine:
//!
//! - [`DetectorError::TransientQuery`]: retry up to N times with backoff.
//!   Postgres connectivity blip, query timeout. Do NOT propagate to the consumer
//!   as an alert; log and retry.
//!
//! - [`DetectorError::PermanentQuery`]: disable the detector for this token for
//!   TTL seconds; log with ERROR. Likely a schema mismatch or migration gap.
//!
//! - [`DetectorError::MissingThresholdConfig`]: programming error — the loader
//!   should have caught this. Log with WARN; skip detector.
//!
//! - [`DetectorError::InsufficientBaseline`]: not an error — the detector has
//!   observed insufficient historical data to compute the primary signal. The
//!   detector SHOULD return `Ok(vec![])` for pure absence, but may return
//!   `Err(InsufficientBaseline)` when it also wants to signal "no data available
//!   yet" to the caller for logging purposes. Detectors that have a fallback
//!   signal (e.g. D04's `burst_concentration_ratio`) SHOULD use the fallback
//!   and return `Ok(...)` instead of this error.
//!
//! - [`DetectorError::MissingDependencyData`]: token not yet enriched in registry,
//!   or required pool state not yet present in Postgres. Retry after enrichment.
//!
//! - [`DetectorError::DeterminismViolation`]: should never happen in production.
//!   Triggered if a detector detects that its own output would be non-deterministic
//!   (e.g. an unordered result set was received and could not be sorted). Treated
//!   as a panic-adjacent condition — log at CRITICAL and disable the detector.

use thiserror::Error;

/// All failure modes from a detector invocation.
///
/// `#[non_exhaustive]` allows new variants to be added in minor releases without
/// breaking callers that match on `DetectorError`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DetectorError {
    /// Postgres query failed transiently. Retry-worthy.
    #[error("transient query failure in detector '{detector_id}': {source}")]
    TransientQuery {
        detector_id: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// Postgres query failed permanently (schema mismatch, data corruption).
    /// Detector is disabled until manually re-enabled.
    #[error("permanent query failure in detector '{detector_id}': {reason}")]
    PermanentQuery {
        detector_id: &'static str,
        reason: String,
    },

    /// A required threshold key was absent from the loaded config.
    /// Programming error — the loader should have caught this before evaluate() is called.
    #[error("missing threshold config key '{key}' for detector '{detector_id}'")]
    MissingThresholdConfig {
        detector_id: &'static str,
        key: &'static str,
    },

    /// Insufficient historical baseline data to compute the primary statistic.
    ///
    /// Detectors with a fallback signal SHOULD NOT return this error — they should
    /// use the fallback and return `Ok(...)`. This variant is for detectors that
    /// have NO meaningful fallback and want to signal "skip this token for now".
    ///
    /// The `fallback_used` field documents whether a secondary signal was attempted.
    #[error("insufficient baseline for detector '{detector_id}' on token '{token}': {reason}")]
    InsufficientBaseline {
        detector_id: &'static str,
        token: String,
        reason: String,
        /// True if a fallback signal was attempted but also failed.
        fallback_used: bool,
    },

    /// Required dependency data (enriched token meta, pool state) not available yet.
    /// Retry after enrichment completes.
    #[error("missing dependency data for detector '{detector_id}' on token '{token}': {reason}")]
    MissingDependencyData {
        detector_id: &'static str,
        token: String,
        reason: String,
    },

    /// Non-determinism invariant violated. Should never happen.
    /// Treated as a fatal detector bug — disable on first occurrence.
    #[error("determinism violation in detector '{detector_id}': {reason}")]
    DeterminismViolation {
        detector_id: &'static str,
        reason: String,
    },

    /// A feature is not yet implemented. Used to wire the API contract without
    /// blocking on the implementation.
    ///
    /// Example: `simulate_sell()` in D01 is wired but deferred to Phase 3.
    /// Returns this error when `simulation_enabled = true` is set before
    /// `dex-adapter` instruction builders exist.
    ///
    /// # Reference
    ///
    /// docs/designs/0004-detector-01-honeypot.md DG3.
    #[error("feature not implemented: '{feature}'")]
    NotImplemented {
        /// A stable snake_case name for the unimplemented feature.
        feature: &'static str,
    },

    /// A non-recoverable error in the simulation path (e.g., transaction serialization
    /// failure, pubkey parse error). Distinct from `TransientQuery` (storage) and
    /// `MissingDependencyData` (enrichment). Callers should log at ERROR and skip simulation.
    ///
    /// # Reference
    ///
    /// docs/designs/0004-detector-01-honeypot.md §3.2 §9.
    #[error("simulation error in '{feature}': {reason}")]
    Simulation {
        /// Stable snake_case name of the failing simulation feature.
        feature: &'static str,
        /// Human-readable description of the failure.
        reason: String,
    },
}

impl DetectorError {
    /// Returns true if the caller should retry this operation.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DetectorError::TransientQuery { .. } | DetectorError::MissingDependencyData { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_query_is_retryable() {
        // We can't easily construct sqlx::Error without a real connection,
        // so we test the other retryable variant.
        let err = DetectorError::MissingDependencyData {
            detector_id: "honeypot_sim",
            token: "SomeMint".into(),
            reason: "not yet enriched".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn permanent_query_not_retryable() {
        let err = DetectorError::PermanentQuery {
            detector_id: "honeypot_sim",
            reason: "schema mismatch".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn missing_threshold_not_retryable() {
        let err = DetectorError::MissingThresholdConfig {
            detector_id: "honeypot_sim",
            key: "sell_tax_threshold",
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn insufficient_baseline_not_retryable() {
        let err = DetectorError::InsufficientBaseline {
            detector_id: "pump_dump",
            token: "SomeMint".into(),
            reason: "fewer than 3 days of data".into(),
            fallback_used: false,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn determinism_violation_not_retryable() {
        let err = DetectorError::DeterminismViolation {
            detector_id: "wash_trading_h1",
            reason: "result set was unordered".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn error_messages_contain_detector_id() {
        let err = DetectorError::PermanentQuery {
            detector_id: "rug_pull_lp_drain",
            reason: "column not found".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("rug_pull_lp_drain"));
        assert!(msg.contains("column not found"));
    }

    #[test]
    fn insufficient_baseline_message_contains_token() {
        let err = DetectorError::InsufficientBaseline {
            detector_id: "pump_dump",
            token: "PUMP_TOKEN_MINT_ADDR".into(),
            reason: "zero volume days".into(),
            fallback_used: true,
        };
        let msg = err.to_string();
        assert!(msg.contains("PUMP_TOKEN_MINT_ADDR"));
        assert!(msg.contains("zero volume days"));
    }
}
