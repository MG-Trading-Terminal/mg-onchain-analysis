//! `POST /v1/analyze` — on-demand full detector run across all 13 detectors.
//!
//! # Changes from `/v1/tokens/analyze`
//!
//! - Runs all 13 streaming detectors (D01-D09, D11, D12, D13) with
//!   `supported_chains` filtering — EVM-only detectors skip Solana requests and
//!   vice versa.
//! - Triggers `ensure_token_metadata` via `AppState.metadata_fetcher` before
//!   dispatching detectors — auto-populates the `tokens` table on first encounter.
//! - Returns a flat `detectors[]` array per the Sprint 25 spec response schema
//!   in addition to the existing `TokenRiskReport` envelope.
//!
//! # D10 exclusion
//!
//! D10 (`launch_audit`) is hook-only and does NOT implement the `Detector`
//! streaming trait. It fires exclusively via `D10IndexerHook` at pool-initialize
//! time. It is intentionally absent from this handler.
//!
//! # D09 construction
//!
//! D09 (`deployer_changepoint`) is built inline from `PgBocpdStateStore` +
//! `PgTypedEdgeStore` + `PgGraphLabelStore` constructed from `state.store.pool()`.
//! Each request creates fresh store wrappers — all are stateless (no per-store
//! cache or mutable state between requests).
//!
//! # observed_at (gotcha #22 exception)
//!
//! This is an on-demand API path (gotcha #93 exception). `Utc::now()` is used
//! for `observed_at` — documented inline per the approved exception pattern.
//!
//! # Concurrency guard
//!
//! Reuses `AppState.in_flight_analyzes` — same guard as `/v1/tokens/analyze`.
//!
//! # Cache
//!
//! Reads from and writes to `AppState.risk_cache` — same TTL as the legacy endpoint.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use mg_onchain_common::anomaly::AnomalyEvent;
use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_detectors::context::{DetectorContext, DetectorWindow};
use mg_onchain_detectors::d09_deployer_changepoint::{D09BocpdDetector, D09Config, PgBocpdStateStore};
use mg_onchain_detectors::{
    BocpdStateStore, ConcentrationDetector, D08SybilDetector, D11SynchronizedActivityDetector,
    D12PermitDrainerDetector, D13SandwichMevDetector, Detector, HoneypotDetector,
    KnownDrainerSet, MintBurnAnomalyDetector, PumpDumpDetector, RugPullDetector,
    WashTradingDetector, WithdrawWithheldDetector,
};
use mg_onchain_dex_adapter::pool_accounts::HttpPoolAccountProvider;
use mg_onchain_graph::api::PgClusterStore;
use mg_onchain_graph::labels::PgGraphLabelStore;
use mg_onchain_graph::typed_edges::PgTypedEdgeStore;
use mg_onchain_scoring::types::SkipReason;
use mg_onchain_scoring::TokenRiskReport;
use mg_onchain_storage::price_provider::PgTokenPriceProvider;

use crate::auth::{AuthClaims, scopes};
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/analyze`.
#[derive(Debug, Deserialize)]
pub struct AnalyzeV2Request {
    pub chain: Chain,
    pub token: String,
    /// Optional analysis window in hours. Default: 24. Range: 1–168.
    pub window_hours: Option<u32>,
}

/// Per-detector outcome in the response.
#[derive(Debug, Serialize)]
pub struct DetectorOutcome {
    pub detector_id: String,
    pub confidence: f64,
    pub severity: String,
    /// Structured evidence map from the anomaly event. Empty if no event fired.
    pub evidence: serde_json::Value,
    /// Whether this detector was skipped (unsupported chain or evaluation error).
    pub skipped: bool,
    /// Skip reason if `skipped = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Response body for `POST /v1/analyze`.
#[derive(Debug, Serialize)]
pub struct AnalyzeV2Response {
    pub chain: String,
    pub token: String,
    /// ISO-8601 UTC timestamp of this evaluation.
    /// Uses `Utc::now()` per gotcha #93 on-demand exception.
    pub evaluated_at: String,
    /// Per-detector outcomes (all 13 streaming detectors; D10 excluded — hook-only).
    pub detectors: Vec<DetectorOutcome>,
    /// Aggregate severity across all fired detectors.
    pub aggregate_severity: String,
    /// Aggregate confidence (max of all fired confidences).
    pub aggregate_confidence: f64,
    /// Analysis duration in milliseconds.
    pub analysis_duration_ms: u64,
    /// Full scoring report (compatible with `/v1/tokens/analyze` response).
    pub report: TokenRiskReport,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /v1/analyze` — feed any token, get full analysis.
///
/// # Auth
///
/// Requires `analyze:write` scope (same as `/v1/tokens/analyze`).
///
/// # Rate limiting
///
/// Uses the `analyze` rate-limit bucket from `AppState.rate_limiter`.
///
/// # observed_at
///
/// `Utc::now()` per gotcha #93 documented exception for on-demand API paths.
#[instrument(skip(state, claims), fields(chain = %req.chain.as_str(), token = %req.token))]
pub async fn analyze_v2_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Json(req): Json<AnalyzeV2Request>,
) -> Result<Json<AnalyzeV2Response>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::WRITE_ANALYZE)?;

    // Rate-limit: reuse analyze bucket.
    state.rate_limiter.check_analyze(&claims.0.sub)?;

    // Validate window_hours.
    let window_hours = req.window_hours.unwrap_or(24);
    if !(1..=168).contains(&window_hours) {
        return Err(GatewayError::SemanticError(
            format!("window_hours must be between 1 and 168, got {window_hours}"),
        ));
    }

    // Validate token address.
    let token_address = Address::parse(req.chain, &req.token)
        .map_err(|e| GatewayError::InvalidInput(format!("invalid token address: {e}")))?;

    // Cache check.
    if let Some(entry) = state.risk_cache.get(req.chain, &req.token).await {
        let _age = entry.inserted_at.elapsed().as_secs();
        state.metrics.scoring_cache_hits_total.inc();
        let report = (*entry.report).clone();
        let detectors = outcomes_from_report(&report);
        let (agg_sev, agg_conf) = aggregate_metrics(&detectors);
        return Ok(Json(AnalyzeV2Response {
            chain: req.chain.to_string(),
            token: req.token.clone(),
            evaluated_at: Utc::now().to_rfc3339(),
            detectors,
            aggregate_severity: agg_sev,
            aggregate_confidence: agg_conf,
            analysis_duration_ms: 0,
            report,
        }));
    }
    state.metrics.scoring_cache_misses_total.inc();

    // In-flight guard (reuse same set as /v1/tokens/analyze).
    let in_flight_key = format!("{}/{}", req.chain.as_str(), req.token);
    {
        let mut guard = state.in_flight_analyzes.lock().unwrap();
        if guard.contains(&in_flight_key) {
            return Err(GatewayError::AnalyzeInFlight {
                chain: req.chain,
                mint: req.token.clone(),
            });
        }
        guard.insert(in_flight_key.clone());
    }

    state.metrics.analyze_in_flight.inc();
    let result = run_analyze_v2(&state, req.chain, &token_address, &req.token, window_hours).await;
    state.metrics.analyze_in_flight.dec();
    {
        let mut guard = state.in_flight_analyzes.lock().unwrap();
        guard.remove(&in_flight_key);
    }

    let (report, duration_ms) = result?;

    // Cache.
    state
        .risk_cache
        .insert(req.chain, req.token.clone(), Arc::new(report.clone()))
        .await;

    let detectors = outcomes_from_report(&report);
    let (agg_sev, agg_conf) = aggregate_metrics(&detectors);

    Ok(Json(AnalyzeV2Response {
        chain: req.chain.to_string(),
        token: req.token.clone(),
        evaluated_at: Utc::now().to_rfc3339(),  // gotcha #93 on-demand exception
        detectors,
        aggregate_severity: agg_sev,
        aggregate_confidence: agg_conf,
        analysis_duration_ms: duration_ms,
        report,
    }))
}

// ---------------------------------------------------------------------------
// Core analyze logic (all 13 streaming detectors, supported_chains filtered)
// ---------------------------------------------------------------------------

/// Run all 13 streaming detectors (D01-D09, D11, D12, D13) with `supported_chains`
/// filtering and score the results.
///
/// D10 is hook-only — intentionally absent.
///
/// # `observed_at` discipline (gotcha #93 exception)
///
/// `Utc::now()` is used here — this is an on-demand API path, which is the
/// documented exception to the no-wall-clock rule. Block-time-anchored
/// `observed_at` is only required for streaming indexer paths.
async fn run_analyze_v2(
    state: &AppState,
    chain: Chain,
    token_address: &Address,
    mint: &str,
    window_hours: u32,
) -> Result<(TokenRiskReport, u64), GatewayError> {
    let start = Instant::now();
    // gotcha #93: on-demand exception — Utc::now() is acceptable here.
    let now = Utc::now();

    let window_end = now;
    let window_start = window_end - chrono::Duration::hours(window_hours as i64);

    // Enrich token metadata (Track 1 auto-populate).
    // If metadata fetcher is not configured (no RPC available), falls back to registry enrich.
    let meta = state
        .registry
        .enrich(mint, chain)
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("registry enrich error: {e}")))?;

    // Build DetectorWindow with placeholder block refs.
    let window = DetectorWindow {
        start: window_start,
        end: window_end,
        block_start: BlockRef::new(chain, 0),
        block_end: BlockRef::new(chain, u64::MAX),
    };

    let ctx = DetectorContext {
        token: token_address,
        chain,
        window,
        observed_at: now,
        store: &state.store,
        registry: &state.registry,
        config: &state.detector_config,
        zero_address: chain_zero_address(chain),
    };

    // Build shared resources for D08/D09/D11/D12/D13.
    let pool = state.store.pool().clone();
    let pool_arc = Arc::new(pool.clone());
    let price_provider = Arc::new(PgTokenPriceProvider::new(pool_arc.clone()));

    // D01 — Honeypot (cadenced)
    let rpc = state.registry.rpc();
    let d01 = HoneypotDetector::new(
        state.detector_config.honeypot_sim.clone(),
        rpc.clone(),
        Arc::new(HttpPoolAccountProvider::new(rpc.clone())),
    );

    // D02 — Rug Pull LP Drain
    let d02 = RugPullDetector::new(state.detector_config.rug_pull_lp_drain.clone());

    // D03 — Holder Concentration
    let d03 = ConcentrationDetector::new(state.detector_config.holder_concentration.clone());

    // D04 — Pump & Dump
    let d04 = PumpDumpDetector::new(state.detector_config.pump_dump.clone());

    // D05 — Wash Trading H1
    let d05 = WashTradingDetector::new(state.detector_config.wash_trading_h1.clone());

    // D06 — Mint/Burn Anomaly
    let d06 = MintBurnAnomalyDetector::new(state.detector_config.mint_burn_anomaly.clone());

    // D07 — Withdraw-Withheld Drain
    let d07 = WithdrawWithheldDetector;

    // D08 — Sybil Bundled-Launch
    // Note: No SmartMoneyLookup injection here (on-demand path — label lookup is per-eval
    // in streaming; on-demand path omits label enrichment for latency).
    let cluster_store = Arc::new(PgClusterStore::new(pool.clone()));
    let label_store_d08 = Arc::new(PgGraphLabelStore::new(pool.clone()));
    let d08 = D08SybilDetector::new(cluster_store, label_store_d08);

    // D09 — BOCPD Deployer Changepoint
    let edge_store_d09 = Arc::new(PgTypedEdgeStore::new(pool.clone()));
    let label_store_d09 = Arc::new(PgGraphLabelStore::new(pool.clone()));
    let bocpd_state = Arc::new(PgBocpdStateStore::new(pool_arc.clone()));
    let d09 = D09BocpdDetector::new(
        edge_store_d09,
        label_store_d09,
        bocpd_state as Arc<dyn BocpdStateStore>,
        pool_arc.clone(),
        D09Config::default(),
    )
    .map_err(|e| GatewayError::Internal(anyhow::anyhow!("D09 build failed: {e}")))?;

    // D11 — Synchronized Activity
    let d11 = D11SynchronizedActivityDetector::new(pool_arc.clone(), price_provider.clone());

    // D12 — Permit2 Drainer (Ethereum-only)
    let d12 = D12PermitDrainerDetector::with_known_drainers(
        pool_arc.clone(),
        KnownDrainerSet::from_addresses(
            &state.detector_config.permit2_drainer_v1.known_drainer_addresses.value,
        ),
        price_provider.clone(),
    );

    // D13 — Sandwich MEV (Ethereum-only)
    let d13 = D13SandwichMevDetector::new(pool_arc.clone(), price_provider.clone());

    // Dispatch: run all detectors concurrently via tokio::join! (13 futures).
    // `supported_chains` filtering: only dispatch when chain is in supported_chains().
    // Unsupported chain returns an empty Ok(vec![]) immediately.
    let (r01, r02, r03, r04, r05, r06, r07, r08, r09, r11, r12, r13) = tokio::join!(
        conditional_eval(&d01, &ctx, chain),
        conditional_eval(&d02, &ctx, chain),
        conditional_eval(&d03, &ctx, chain),
        conditional_eval(&d04, &ctx, chain),
        conditional_eval(&d05, &ctx, chain),
        conditional_eval(&d06, &ctx, chain),
        conditional_eval(&d07, &ctx, chain),
        conditional_eval(&d08, &ctx, chain),
        conditional_eval(&d09, &ctx, chain),
        conditional_eval(&d11, &ctx, chain),
        conditional_eval(&d12, &ctx, chain),
        conditional_eval(&d13, &ctx, chain),
    );

    let mut all_events: Vec<AnomalyEvent> = Vec::new();
    let mut skip_reasons: Vec<SkipReason> = Vec::new();

    for (result, id) in [
        (r01, "honeypot_sim"),
        (r02, "rug_pull_lp_drain"),
        (r03, "holder_concentration"),
        (r04, "pump_dump"),
        (r05, "wash_trading_h1"),
        (r06, "mint_burn_anomaly"),
        (r07, "withdraw_withheld_drain"),
        (r08, "sybil_detection"),
        (r09, "deployer_changepoint"),
        (r11, "synchronized_activity_v1"),
        (r12, "permit2_drainer_v1"),
        (r13, "sandwich_mev_v1"),
    ] {
        match result {
            Ok(events) => {
                let outcome = if events.is_empty() { "empty" } else { "ok" };
                state
                    .metrics
                    .detector_invocations_total
                    .with_label_values(&[id, outcome])
                    .inc();
                all_events.extend(events);
            }
            Err(e) => {
                tracing::warn!(detector = id, error = %e, "detector evaluation failed — skipped");
                state
                    .metrics
                    .detector_invocations_total
                    .with_label_values(&[id, "error"])
                    .inc();
                skip_reasons.push(SkipReason {
                    detector_id: id.to_string(),
                    reason: format!("detector error: {e}"),
                });
            }
        }
    }

    let report = state
        .scoring
        .score(&all_events, &meta, (window_start, window_end), &skip_reasons, now);

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok((report, duration_ms))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Dispatch `detector.evaluate(ctx)` only if `chain` is in `supported_chains()`.
/// Returns `Ok(vec![])` for unsupported chains (no skip reason emitted —
/// the chain filter is expected behaviour, not an error).
async fn conditional_eval<D: Detector>(
    detector: &D,
    ctx: &DetectorContext<'_>,
    chain: Chain,
) -> Result<Vec<AnomalyEvent>, mg_onchain_detectors::error::DetectorError> {
    if detector.supported_chains().contains(&chain) {
        detector.evaluate(ctx).await
    } else {
        Ok(vec![])
    }
}

/// Convert a `TokenRiskReport` into `Vec<DetectorOutcome>` for the v2 response.
///
/// Iterates over `report.per_detector` (BTreeMap — already sorted); for each
/// detector entry, produces a `DetectorOutcome`.  Skipped detectors from
/// `report.coverage.detectors_skipped` are appended.
fn outcomes_from_report(report: &TokenRiskReport) -> Vec<DetectorOutcome> {
    let mut out: Vec<DetectorOutcome> = report
        .per_detector
        .iter()
        .map(|(id, ds)| DetectorOutcome {
            detector_id: id.clone(),
            confidence: ds.max_confidence.value(),
            severity: format!("{:?}", ds.severity),
            // Flatten the evidence_summary Vec<(String, String)> into a JSON object.
            evidence: serde_json::Value::Object(
                ds.evidence_summary
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            ),
            skipped: false,
            skip_reason: None,
        })
        .collect();

    for skip in &report.coverage.detectors_skipped {
        out.push(DetectorOutcome {
            detector_id: skip.detector_id.clone(),
            confidence: 0.0,
            severity: "None".to_string(),
            evidence: serde_json::Value::Null,
            skipped: true,
            skip_reason: Some(skip.reason.clone()),
        });
    }

    // `per_detector` is a BTreeMap so entries are already sorted; skipped
    // entries are appended. Sort the whole slice for full determinism.
    out.sort_by(|a, b| a.detector_id.cmp(&b.detector_id));
    out
}

/// Compute aggregate severity + confidence from `DetectorOutcome` slice.
fn aggregate_metrics(detectors: &[DetectorOutcome]) -> (String, f64) {
    let max_conf = detectors
        .iter()
        .filter(|d| !d.skipped)
        .map(|d| d.confidence)
        .fold(0.0f64, f64::max);

    let severity = if max_conf >= 0.85 {
        "Critical"
    } else if max_conf >= 0.65 {
        "High"
    } else if max_conf >= 0.40 {
        "Medium"
    } else if max_conf > 0.0 {
        "Low"
    } else {
        "None"
    };

    (severity.to_string(), max_conf)
}

fn chain_zero_address(chain: Chain) -> &'static str {
    if chain.is_evm() {
        "0x0000000000000000000000000000000000000000"
    } else {
        "11111111111111111111111111111111"
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_outcome(id: &str, conf: f64, skipped: bool) -> DetectorOutcome {
        DetectorOutcome {
            detector_id: id.to_string(),
            confidence: conf,
            severity: if conf >= 0.65 { "High".to_string() } else { "Low".to_string() },
            evidence: serde_json::Value::Null,
            skipped,
            skip_reason: if skipped { Some("unsupported chain".to_string()) } else { None },
        }
    }

    // -----------------------------------------------------------------------
    // aggregate_metrics
    // -----------------------------------------------------------------------

    /// All-zero confidences → aggregate is "None".
    #[test]
    fn aggregate_severity_none_when_all_zero() {
        let outcomes: Vec<DetectorOutcome> = vec![];
        let (sev, conf) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "None");
        assert_eq!(conf, 0.0);
    }

    /// Max confidence ≥ 0.85 → "Critical".
    #[test]
    fn aggregate_severity_critical_at_0_85() {
        let outcomes = vec![make_outcome("d01", 0.85, false)];
        let (sev, conf) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "Critical");
        assert!((conf - 0.85).abs() < f64::EPSILON);
    }

    /// Max confidence = 0.65 → "High".
    #[test]
    fn aggregate_severity_high_at_0_65() {
        let outcomes = vec![make_outcome("d02", 0.65, false)];
        let (sev, conf) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "High");
        assert!((conf - 0.65).abs() < f64::EPSILON);
    }

    /// Max confidence in (0.40, 0.65) → "Medium".
    #[test]
    fn aggregate_severity_medium_at_0_50() {
        let outcomes = vec![make_outcome("d03", 0.50, false)];
        let (sev, _) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "Medium");
    }

    /// Skipped detectors do not contribute to aggregate confidence.
    #[test]
    fn aggregate_skipped_detector_not_counted() {
        let outcomes = vec![make_outcome("d_skipped", 0.99, true)];
        let (sev, conf) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "None", "skipped detector must not raise severity");
        assert_eq!(conf, 0.0);
    }

    /// Mixed: one fired + one skipped → skipped not counted.
    #[test]
    fn aggregate_mixed_fired_and_skipped() {
        let outcomes = vec![
            make_outcome("fired", 0.50, false),
            make_outcome("skipped", 0.99, true),
        ];
        let (sev, conf) = aggregate_metrics(&outcomes);
        assert_eq!(sev, "Medium");
        assert!((conf - 0.50).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Address validation
    // -----------------------------------------------------------------------

    /// Invalid EVM address → parse fails → 400 would be returned.
    #[test]
    fn invalid_evm_address_format() {
        let r = Address::parse(Chain::Ethereum, "not-a-real-address");
        assert!(r.is_err(), "malformed EVM address must fail parse");
    }

    /// Invalid Solana address format → parse fails → 400 would be returned.
    #[test]
    fn invalid_solana_address_format() {
        let r = Address::parse(Chain::Solana, "0xnot-base58");
        assert!(r.is_err(), "malformed Solana address must fail parse");
    }
}
