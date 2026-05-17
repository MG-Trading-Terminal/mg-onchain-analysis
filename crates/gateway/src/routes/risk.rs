//! `GET /v1/tokens/{chain}/{mint}/risk` — cached or freshly-computed risk report.
//!
//! On cache hit: returns cached report immediately with `cache_age_seconds`.
//! On cache miss: queries anomaly_events + runs scoring (no detector run).
//! Returns 404 if no data exists for the token.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use serde::Serialize;
use tracing::instrument;

use mg_onchain_common::chain::Chain;
use mg_onchain_scoring::TokenRiskReport;

use crate::auth::{AuthClaims, scopes};
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Response type
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct RiskResponse {
    pub report: TokenRiskReport,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[instrument(skip(state, claims), fields(chain = %chain_str, mint = %mint))]
pub async fn get_token_risk_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Path((chain_str, mint)): Path<(String, String)>,
) -> Result<Json<RiskResponse>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::READ_RISK)?;

    // Rate-limit: default bucket.
    state.rate_limiter.check_default(&claims.0.sub)?;

    let chain = parse_chain(&chain_str)?;

    // Cache check.
    if let Some(entry) = state.risk_cache.get(chain, &mint).await {
        let age = entry.inserted_at.elapsed().as_secs();
        state.metrics.scoring_cache_hits_total.inc();
        let report = (*entry.report).clone();
        return Ok(Json(RiskResponse {
            report,
            cached: true,
            cache_age_seconds: Some(age),
        }));
    }
    state.metrics.scoring_cache_misses_total.inc();

    // Cache miss: compute from Postgres events + scoring (no detector run).
    let report = compute_risk_from_events(&state, chain, &mint).await?;

    // Cache for subsequent calls.
    state
        .risk_cache
        .insert(chain, mint.clone(), Arc::new(report.clone()))
        .await;

    Ok(Json(RiskResponse {
        report,
        cached: false,
        cache_age_seconds: None,
    }))
}

/// Compute a `TokenRiskReport` from existing `anomaly_events` rows (last 24h)
/// without running detectors. Used by `GET .../risk` on cache miss.
async fn compute_risk_from_events(
    state: &AppState,
    chain: Chain,
    mint: &str,
) -> Result<TokenRiskReport, GatewayError> {
    let now = Utc::now();
    let window_start = now - chrono::Duration::hours(24);

    // Fetch recent events from Postgres.
    let rows = state
        .store
        .fetch_anomaly_events_paginated(
            Some(chain.as_str()),
            Some(mint),
            None,
            "info",
            Some(window_start),
            now,
            None,
            None,
            500,
        )
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("fetch events: {e}")))?;

    if rows.is_empty() {
        // No events found AND no cache entry → 404.
        return Err(GatewayError::TokenNotFound {
            chain,
            mint: mint.to_string(),
        });
    }

    // Reconstruct AnomalyEvent values from rows.
    let events = reconstruct_events(chain, &rows)?;

    // Enrich token metadata.
    let meta = state
        .registry
        .enrich(mint, chain)
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("registry enrich: {e}")))?;

    let report = state
        .scoring
        .score(&events, &meta, (window_start, now), &[], now);

    Ok(report)
}

fn reconstruct_events(
    chain: Chain,
    rows: &[mg_onchain_storage::pg::AnomalyEventRow],
) -> Result<Vec<mg_onchain_common::anomaly::AnomalyEvent>, GatewayError> {
    use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
    use mg_onchain_common::chain::{Address, BlockRef};

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let token = Address::parse(chain, &row.token).map_err(|e| {
            GatewayError::Internal(anyhow::anyhow!("address parse from row: {e}"))
        })?;

        let confidence = Confidence::new(row.confidence).map_err(|e| {
            GatewayError::Internal(anyhow::anyhow!("confidence out of range: {e}"))
        })?;

        let severity = match row.severity.as_str() {
            "info" => Severity::Info,
            "low" => Severity::Low,
            "medium" => Severity::Medium,
            "high" => Severity::High,
            "critical" => Severity::Critical,
            other => {
                tracing::warn!(severity = other, "unknown severity in DB — defaulting to info");
                Severity::Info
            }
        };

        // Deserialize evidence from stored JSONB.
        let evidence: Evidence = serde_json::from_value(row.evidence.clone())
            .unwrap_or_default();

        events.push(AnomalyEvent {
            detector_id: row.detector_id.clone(),
            token,
            chain,
            confidence,
            severity,
            evidence,
            observed_at: row.observed_at,
            window: (
                BlockRef::new(chain, row.window_start_height as u64),
                BlockRef::new(chain, row.window_end_height as u64),
            ),
            ingested_at: row.ingested_at,
            oak_technique_id: row.oak_technique_id.clone(),
        });
    }
    Ok(events)
}

pub fn parse_chain(s: &str) -> Result<Chain, GatewayError> {
    match s {
        "solana" => Ok(Chain::Solana),
        "ethereum" => Ok(Chain::Ethereum),
        "bsc" => Ok(Chain::Bsc),
        "base" => Ok(Chain::Base),
        other => Err(GatewayError::SemanticError(
            format!("Chain '{other}' is not supported. Supported: solana, ethereum, bsc, base."),
        )),
    }
}
