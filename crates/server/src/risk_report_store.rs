//! `TokenRiskReportStore` trait and `PgTokenRiskReportStore` implementation.
//!
//! # Placement rationale
//!
//! The trait lives in `crates/server` (not `crates/storage`) to avoid a
//! dependency cycle. The cycle that would occur if the trait were in
//! `crates/storage`:
//!
//! ```text
//! storage → scoring → detectors → dex-adapter → token-registry → storage
//! ```
//!
//! `crates/server` already depends on both `mg-onchain-storage` (for PgStore)
//! and `mg-onchain-scoring` (for TokenRiskReport), so this is the natural home.
//! The pattern is consistent with how streaming worker logic lives in `crates/server`
//! alongside the code that uses it.
//!
//! # Design reference
//!
//! SESSION-KICKOFF gotchas #30 + #31.
//! Sprint 12 Option C (persistence debt closure).
//! Migration: `migrations/postgres/V00012__token_risk_reports.sql`
//!
//! # Delta-threshold short-circuit (gotcha #30)
//!
//! `upsert_token_risk_report` is called ONLY when the delta check passes in
//! `worker.rs`. When the short-circuit fires, `evaluate_token` returns early
//! before reaching the upsert site. This is a structural guarantee enforced at
//! the call site, not in this module.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::{debug, instrument};

use mg_onchain_common::anomaly::{Confidence, Severity};
use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_scoring::TokenRiskReport;
use mg_onchain_scoring::config::ScoringConfig;
use mg_onchain_scoring::types::{CoverageReport, DetectorScore, EvidenceHighlight, SignalCounts};
use mg_onchain_storage::{PgStore, StorageError};

// ---------------------------------------------------------------------------
// TokenRiskReportStore trait
// ---------------------------------------------------------------------------

/// Read/write API for the `token_risk_reports` table (V00012).
///
/// `Send + Sync` is required for use behind `Arc<dyn TokenRiskReportStore>`
/// across `tokio::spawn` task boundaries (gotcha #27).
///
/// # Best-effort semantics
///
/// Callers in the streaming worker treat persistence as best-effort:
/// errors are logged (`warn!`) and the worker continues. A Postgres outage
/// must NOT stop live scoring (the in-memory `RiskCache` remains the hot path).
#[async_trait]
pub trait TokenRiskReportStore: Send + Sync {
    /// Upsert a risk report.
    ///
    /// `ON CONFLICT (chain, token, window_end) DO UPDATE` — always overwrites.
    /// Scheduler re-runs the same window to pick up late events; latest result wins.
    async fn upsert_token_risk_report(&self, report: &TokenRiskReport) -> Result<(), StorageError>;

    /// Fetch the most recent report for `(chain, token)`, ordered by
    /// `computed_at DESC`.
    ///
    /// Returns `None` if no report has been persisted yet.
    ///
    /// # Future usage
    ///
    /// Intended as a cache-miss fallback for the gateway's analyze route before
    /// triggering a full re-compute. At V00012, the gateway does NOT use this path
    /// yet — that wiring is a follow-up. The method is provided to complete the
    /// trait surface for future callers.
    async fn get_latest_token_risk_report(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<TokenRiskReport>, StorageError>;
}

// ---------------------------------------------------------------------------
// Arc delegation
// ---------------------------------------------------------------------------

/// `Arc<T>` delegates to `T` when `T: TokenRiskReportStore`.
///
/// This allows `Arc<PgTokenRiskReportStore>` to be used as
/// `Arc<dyn TokenRiskReportStore>` without an extra wrapper.
#[async_trait]
impl<T: TokenRiskReportStore + ?Sized> TokenRiskReportStore for Arc<T> {
    async fn upsert_token_risk_report(&self, report: &TokenRiskReport) -> Result<(), StorageError> {
        (**self).upsert_token_risk_report(report).await
    }

    async fn get_latest_token_risk_report(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<TokenRiskReport>, StorageError> {
        (**self).get_latest_token_risk_report(chain, token).await
    }
}

// ---------------------------------------------------------------------------
// PgTokenRiskReportStore
// ---------------------------------------------------------------------------

/// Postgres implementation of [`TokenRiskReportStore`].
///
/// Wraps a `PgPool`; cheap to clone (pool is `Arc`-backed internally).
///
/// # JSONB encoding
///
/// All nested structs (`per_detector`, `top_evidence`, `signal_counts`,
/// `coverage`, `config_snapshot`) are serialised via `serde_json::to_value`.
/// `per_detector` is `BTreeMap<String, DetectorScore>` — alphabetical ordering
/// is guaranteed by the map type, ensuring deterministic JSONB output.
///
/// # Severity encoding
///
/// `Severity` is serialised to its `serde` representation via `serde_json` and
/// stored in the `overall_severity TEXT` column.
#[derive(Debug, Clone)]
pub struct PgTokenRiskReportStore {
    pool: PgPool,
}

impl PgTokenRiskReportStore {
    /// Construct from an existing `PgPool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Construct from a `PgStore`, sharing its pool (no extra connections).
    pub fn from_pg_store(store: &PgStore) -> Self {
        Self {
            pool: store.pool().clone(),
        }
    }
}

#[async_trait]
impl TokenRiskReportStore for PgTokenRiskReportStore {
    #[instrument(skip(self, report), fields(
        chain = %report.chain,
        token = %report.token,
        window_end = %report.window.1,
        overall_score = %report.overall_score.value(),
    ))]
    async fn upsert_token_risk_report(&self, report: &TokenRiskReport) -> Result<(), StorageError> {
        let chain = report.chain.to_string();
        let token = report.token.to_string();
        let window_start = report.window.0;
        let window_end = report.window.1;
        let computed_at = report.computed_at;
        let overall_score = report.overall_score.value();
        let base_score = report.base_score.value();

        // Serialize Severity to its serde string form for the TEXT column.
        let overall_severity = serde_json::to_value(report.overall_severity)
            .map_err(StorageError::Serde)?
            .as_str()
            .unwrap_or("Info")
            .to_string();

        let per_detector =
            serde_json::to_value(&report.per_detector).map_err(StorageError::Serde)?;
        let top_evidence =
            serde_json::to_value(&report.top_evidence).map_err(StorageError::Serde)?;
        let signal_counts =
            serde_json::to_value(&report.signal_counts).map_err(StorageError::Serde)?;
        let coverage = serde_json::to_value(&report.coverage).map_err(StorageError::Serde)?;
        let config_snapshot =
            serde_json::to_value(&report.config_snapshot).map_err(StorageError::Serde)?;

        sqlx::query(
            r#"
            INSERT INTO token_risk_reports (
                chain, token, window_start, window_end, computed_at,
                overall_score, base_score, overall_severity,
                per_detector, top_evidence, signal_counts, coverage, config_snapshot,
                updated_at
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8,
                $9, $10, $11, $12, $13,
                now()
            )
            ON CONFLICT (chain, token, window_end) DO UPDATE SET
                window_start     = EXCLUDED.window_start,
                computed_at      = EXCLUDED.computed_at,
                overall_score    = EXCLUDED.overall_score,
                base_score       = EXCLUDED.base_score,
                overall_severity = EXCLUDED.overall_severity,
                per_detector     = EXCLUDED.per_detector,
                top_evidence     = EXCLUDED.top_evidence,
                signal_counts    = EXCLUDED.signal_counts,
                coverage         = EXCLUDED.coverage,
                config_snapshot  = EXCLUDED.config_snapshot,
                updated_at       = now()
            "#,
        )
        .bind(&chain)
        .bind(&token)
        .bind(window_start)
        .bind(window_end)
        .bind(computed_at)
        .bind(overall_score)
        .bind(base_score)
        .bind(&overall_severity)
        .bind(&per_detector)
        .bind(&top_evidence)
        .bind(&signal_counts)
        .bind(&coverage)
        .bind(&config_snapshot)
        .execute(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        debug!(chain, token, "token_risk_report upserted");
        Ok(())
    }

    #[instrument(skip(self), fields(chain, token))]
    async fn get_latest_token_risk_report(
        &self,
        chain: &str,
        token: &str,
    ) -> Result<Option<TokenRiskReport>, StorageError> {
        use sqlx::Row as _;
        use std::collections::BTreeMap;

        let row = sqlx::query(
            r#"
            SELECT chain, token, window_start, window_end, computed_at,
                   overall_score, base_score, overall_severity,
                   per_detector, top_evidence, signal_counts, coverage, config_snapshot
            FROM token_risk_reports
            WHERE chain = $1 AND token = $2
            ORDER BY computed_at DESC
            LIMIT 1
            "#,
        )
        .bind(chain)
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::Postgres)?;

        let row = match row {
            None => return Ok(None),
            Some(r) => r,
        };

        let chain_val: String = row.try_get("chain").map_err(StorageError::Postgres)?;
        let token_val: String = row.try_get("token").map_err(StorageError::Postgres)?;
        let window_start: DateTime<Utc> = row
            .try_get("window_start")
            .map_err(StorageError::Postgres)?;
        let window_end: DateTime<Utc> =
            row.try_get("window_end").map_err(StorageError::Postgres)?;
        let computed_at: DateTime<Utc> =
            row.try_get("computed_at").map_err(StorageError::Postgres)?;
        let overall_score: f64 = row
            .try_get("overall_score")
            .map_err(StorageError::Postgres)?;
        let base_score: f64 = row.try_get("base_score").map_err(StorageError::Postgres)?;
        let overall_severity_str: String = row
            .try_get("overall_severity")
            .map_err(StorageError::Postgres)?;

        let per_detector_val: serde_json::Value = row
            .try_get("per_detector")
            .map_err(StorageError::Postgres)?;
        let top_evidence_val: serde_json::Value = row
            .try_get("top_evidence")
            .map_err(StorageError::Postgres)?;
        let signal_counts_val: serde_json::Value = row
            .try_get("signal_counts")
            .map_err(StorageError::Postgres)?;
        let coverage_val: serde_json::Value =
            row.try_get("coverage").map_err(StorageError::Postgres)?;
        let config_snapshot_val: serde_json::Value = row
            .try_get("config_snapshot")
            .map_err(StorageError::Postgres)?;

        let chain_parsed: Chain = chain_val
            .parse()
            .map_err(|_| StorageError::Other(format!("unknown chain: {chain_val}")))?;
        let token_addr = Address::parse(chain_parsed, &token_val)
            .map_err(|e| StorageError::Other(format!("invalid token address: {e}")))?;
        let score = Confidence::new(overall_score)
            .map_err(|e| StorageError::Other(format!("invalid overall_score: {e}")))?;
        let bscore = Confidence::new(base_score)
            .map_err(|e| StorageError::Other(format!("invalid base_score: {e}")))?;
        let severity: Severity =
            serde_json::from_value(serde_json::Value::String(overall_severity_str))
                .map_err(StorageError::Serde)?;
        let per_detector: BTreeMap<String, DetectorScore> =
            serde_json::from_value(per_detector_val).map_err(StorageError::Serde)?;
        let top_evidence: Vec<EvidenceHighlight> =
            serde_json::from_value(top_evidence_val).map_err(StorageError::Serde)?;
        let signal_counts: SignalCounts =
            serde_json::from_value(signal_counts_val).map_err(StorageError::Serde)?;
        let coverage: CoverageReport =
            serde_json::from_value(coverage_val).map_err(StorageError::Serde)?;
        let config_snapshot: ScoringConfig =
            serde_json::from_value(config_snapshot_val).map_err(StorageError::Serde)?;

        Ok(Some(TokenRiskReport {
            chain: chain_parsed,
            token: token_addr,
            window: (window_start, window_end),
            computed_at,
            overall_score: score,
            base_score: bscore,
            overall_severity: severity,
            per_detector,
            top_evidence,
            signal_counts,
            coverage,
            config_snapshot,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_common::anomaly::Severity;
    use mg_onchain_scoring::config::ScoringConfig;
    use mg_onchain_scoring::types::{CoverageReport, SignalCounts, TokenRiskReport};
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------------
    // Helper: build a minimal TokenRiskReport for testing (no I/O).
    // -----------------------------------------------------------------------
    fn minimal_report(window_end: DateTime<Utc>, overall_score: f64) -> TokenRiskReport {
        let token =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let window_start = window_end - chrono::Duration::hours(1);

        TokenRiskReport {
            chain: Chain::Solana,
            token,
            window: (window_start, window_end),
            computed_at: window_end,
            overall_score: Confidence::new(overall_score).unwrap(),
            base_score: Confidence::new(overall_score).unwrap(),
            overall_severity: Severity::Medium,
            per_detector: BTreeMap::new(),
            top_evidence: vec![],
            signal_counts: SignalCounts {
                fired: 0,
                inconclusive: 0,
                suppressed_info: 0,
            },
            coverage: CoverageReport {
                detectors_run: vec![],
                detectors_skipped: vec![],
                coverage_completeness: 0.0,
            },
            config_snapshot: ScoringConfig::default_calibrated(),
        }
    }

    // -----------------------------------------------------------------------
    // Verify Severity round-trips through serde_json correctly for the TEXT
    // column encoding used in upsert_token_risk_report.
    // -----------------------------------------------------------------------
    #[test]
    fn severity_serde_roundtrip_for_text_column() {
        let severities = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        for sev in &severities {
            let json_val = serde_json::to_value(sev).unwrap();
            let as_str = json_val.as_str().unwrap().to_string();
            // Round-trip: parse back from a JSON String value.
            let round: Severity =
                serde_json::from_value(serde_json::Value::String(as_str.clone())).unwrap();
            assert_eq!(
                *sev, round,
                "Severity round-trip failed for {:?} (encoded as {as_str:?})",
                sev
            );
        }
    }

    // -----------------------------------------------------------------------
    // Verify that BTreeMap<String, DetectorScore> serialises deterministically.
    // BTreeMap guarantees alphabetical key ordering; JSON output must be stable.
    // -----------------------------------------------------------------------
    #[test]
    fn per_detector_btreemap_is_alphabetically_ordered() {
        use mg_onchain_scoring::types::DetectorScore;
        let mut pd: BTreeMap<String, DetectorScore> = BTreeMap::new();
        pd.insert(
            "rug_pull_lp_drain".to_string(),
            DetectorScore {
                detector_id: "rug_pull_lp_drain".to_string(),
                fired_events: 1,
                inconclusive_events: 0,
                suppressed_events: 0,
                max_confidence: Confidence::new(0.8).unwrap(),
                weighted_confidence: Confidence::new(0.8).unwrap(),
                severity: Severity::High,
                evidence_summary: vec![],
            },
        );
        pd.insert(
            "honeypot_sim".to_string(),
            DetectorScore {
                detector_id: "honeypot_sim".to_string(),
                fired_events: 0,
                inconclusive_events: 0,
                suppressed_events: 0,
                max_confidence: Confidence::new(0.0).unwrap(),
                weighted_confidence: Confidence::new(0.0).unwrap(),
                severity: Severity::Info,
                evidence_summary: vec![],
            },
        );

        let j1 = serde_json::to_string(&pd).unwrap();
        let j2 = serde_json::to_string(&pd).unwrap();
        assert_eq!(j1, j2, "serialisation must be stable across calls");
        // BTreeMap: honeypot_sim < rug_pull_lp_drain alphabetically.
        assert!(
            j1.find("honeypot_sim").unwrap() < j1.find("rug_pull_lp_drain").unwrap(),
            "BTreeMap keys must appear in alphabetical order in JSON"
        );
    }

    // -----------------------------------------------------------------------
    // Verify minimal_report produces a report with consistent fields.
    // -----------------------------------------------------------------------
    #[test]
    fn minimal_report_fields_are_consistent() {
        let now = chrono::Utc::now();
        let r = minimal_report(now, 0.75);
        assert_eq!(r.overall_score.value(), 0.75);
        assert_eq!(r.chain, Chain::Solana);
        assert_eq!(r.window.1, now);
        assert_eq!(r.window.0, now - chrono::Duration::hours(1));
        assert_eq!(r.overall_severity, Severity::Medium);
    }

    // -----------------------------------------------------------------------
    // Docker-gated integration tests — require a live Postgres instance.
    // Run with:
    //   DATABASE_URL=postgres://... cargo test -p mg-onchain-server \
    //     -- --ignored upsert_token_risk_report_roundtrip
    // -----------------------------------------------------------------------

    fn make_report(window_end: DateTime<Utc>, score: f64) -> TokenRiskReport {
        let mut r = minimal_report(window_end, score);
        r.signal_counts = SignalCounts {
            fired: 1,
            inconclusive: 0,
            suppressed_info: 0,
        };
        r.coverage = CoverageReport {
            detectors_run: vec!["rug_pull_lp_drain".to_string()],
            detectors_skipped: vec![],
            coverage_completeness: 0.14,
        };
        r
    }

    async fn maybe_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(PgPool::connect(&url).await.expect("connect to test DB"))
    }

    /// Insert → fetch → verify all fields match.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn upsert_token_risk_report_roundtrip() {
        let pool = maybe_pool().await.expect("DATABASE_URL not set");
        let store = PgTokenRiskReportStore::new(pool);

        let now = Utc::now();
        let report = make_report(now, 0.65);

        store
            .upsert_token_risk_report(&report)
            .await
            .expect("upsert");

        let fetched = store
            .get_latest_token_risk_report("solana", "So11111111111111111111111111111111111111112")
            .await
            .expect("fetch")
            .expect("row must exist after upsert");

        assert_eq!(fetched.chain, report.chain);
        assert_eq!(fetched.token, report.token);
        assert_eq!(fetched.window, report.window);
        assert!((fetched.overall_score.value() - report.overall_score.value()).abs() < 1e-9);
        assert_eq!(fetched.overall_severity, report.overall_severity);
        assert_eq!(fetched.signal_counts.fired, report.signal_counts.fired);
        assert_eq!(
            fetched.coverage.coverage_completeness,
            report.coverage.coverage_completeness
        );
    }

    /// Insert same (chain, token, window_end) twice → no duplicate; latest wins.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn upsert_token_risk_report_idempotent() {
        use sqlx::Row as _;
        let pool = maybe_pool().await.expect("DATABASE_URL not set");
        let store = PgTokenRiskReportStore::new(pool.clone());

        let now = Utc::now();
        let report_v1 = make_report(now, 0.40);
        let mut report_v2 = make_report(now, 0.80); // same window_end, higher score
        report_v2.overall_severity = Severity::High;

        store
            .upsert_token_risk_report(&report_v1)
            .await
            .expect("first upsert");
        store
            .upsert_token_risk_report(&report_v2)
            .await
            .expect("second upsert (ON CONFLICT)");

        // Must have exactly one row.
        let row = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM token_risk_reports \
             WHERE chain = 'solana' AND token = $1 AND window_end = $2",
        )
        .bind("So11111111111111111111111111111111111111112")
        .bind(now)
        .fetch_one(&pool)
        .await
        .expect("count query");
        let count: i64 = row.try_get("cnt").unwrap();
        assert_eq!(
            count, 1,
            "must have exactly one row after two upserts of same window_end"
        );

        // Fetched row must have v2 values.
        let fetched = store
            .get_latest_token_risk_report("solana", "So11111111111111111111111111111111111111112")
            .await
            .expect("fetch")
            .expect("row");
        assert!(
            (fetched.overall_score.value() - 0.80).abs() < 1e-9,
            "second upsert's score must overwrite first"
        );
        assert_eq!(fetched.overall_severity, Severity::High);
    }

    /// Insert 3 reports with different window_ends; get_latest returns highest computed_at.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn get_latest_returns_most_recent() {
        let pool = maybe_pool().await.expect("DATABASE_URL not set");
        let store = PgTokenRiskReportStore::new(pool);

        let t1 = Utc::now() - chrono::Duration::hours(3);
        let t2 = Utc::now() - chrono::Duration::hours(2);
        let t3 = Utc::now() - chrono::Duration::hours(1);

        store
            .upsert_token_risk_report(&make_report(t1, 0.10))
            .await
            .expect("r1");
        store
            .upsert_token_risk_report(&make_report(t2, 0.50))
            .await
            .expect("r2");
        store
            .upsert_token_risk_report(&make_report(t3, 0.90))
            .await
            .expect("r3");

        let fetched = store
            .get_latest_token_risk_report("solana", "So11111111111111111111111111111111111111112")
            .await
            .expect("fetch")
            .expect("row");

        // t3 has the greatest computed_at → should return score 0.90.
        assert!(
            (fetched.overall_score.value() - 0.90).abs() < 1e-9,
            "get_latest must return the most-recent-computed_at report"
        );
        assert_eq!(fetched.window.1, t3);
    }
}
