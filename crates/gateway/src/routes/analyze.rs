//! `POST /v1/tokens/analyze` — on-demand full detector run + risk scoring.
//!
//! Runs all seven detectors (D01–D07) concurrently, aggregates via the scoring
//! engine, caches the result, and returns a `TokenRiskReport`.
//!
//! # Concurrency guard
//!
//! If an analyze for the same `(chain, mint)` is already in flight, returns 409.
//! This prevents duplicate detector runs for the same token under load.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use mg_onchain_common::chain::{Address, BlockRef, Chain};
use mg_onchain_detectors::context::{DetectorContext, DetectorWindow};
use mg_onchain_dex_adapter::pool_accounts::HttpPoolAccountProvider;
use mg_onchain_detectors::rpc::SolanaRpc;
use mg_onchain_detectors::{
    ConcentrationDetector, Detector, HoneypotDetector, MintBurnAnomalyDetector,
    PumpDumpDetector, RugPullDetector, WashTradingDetector, WithdrawWithheldDetector,
};
use mg_onchain_scoring::types::SkipReason;
use mg_onchain_scoring::TokenRiskReport;
use mg_onchain_token_registry::rpc::{
    DecodedMint, RawAccount, SignatureInfo, SimulatedTransaction, TokenAccountBalance,
};
use mg_onchain_token_registry::RegistryError;

use crate::auth::{AuthClaims, scopes};
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// No-op Solana RPC — used for D01 in Phase 1 (simulation deferred to Phase 3)
// ---------------------------------------------------------------------------

/// Stub RPC implementation that satisfies the `SolanaRpc` trait bound on
/// `HoneypotDetector` while Phase-3 simulation is not yet implemented.
///
/// All methods return an error so that if simulation is accidentally enabled
/// in config, the detector logs a skip rather than panicking.
///
/// Kept for test use only — production code uses `HttpPoolAccountProvider`.
#[allow(dead_code)]
struct NoopSolanaRpc;

#[async_trait]
impl SolanaRpc for NoopSolanaRpc {
    async fn get_mint_account(&self, _mint: &str) -> Result<Option<DecodedMint>, RegistryError> {
        Err(RegistryError::Internal("simulation not implemented (Phase 3)".into()))
    }
    async fn get_token_largest_accounts(
        &self,
        _mint: &str,
        _commitment: &str,
    ) -> Result<Vec<TokenAccountBalance>, RegistryError> {
        Err(RegistryError::Internal("simulation not implemented (Phase 3)".into()))
    }
    async fn get_token_account_owner(
        &self,
        _token_account: &str,
    ) -> Result<Option<String>, RegistryError> {
        Err(RegistryError::Internal("simulation not implemented (Phase 3)".into()))
    }
    async fn get_first_signature(
        &self,
        _address: &str,
    ) -> Result<Option<SignatureInfo>, RegistryError> {
        Err(RegistryError::Internal("simulation not implemented (Phase 3)".into()))
    }
    async fn simulate_transaction(
        &self,
        _tx_base64: &str,
        _sig_verify: bool,
        _replace_recent_blockhash: bool,
        _commitment: &str,
        _accounts_to_track: &[&str],
    ) -> Result<SimulatedTransaction, RegistryError> {
        // Phase C will replace this wiring with a real `HttpSolanaRpc` injected
        // from AppState. Until then this stub ensures the detector's
        // `simulation_enabled=true` path surfaces a skip rather than a panic.
        Err(RegistryError::Internal(
            "NoopSolanaRpc: simulate_transaction not wired (Phase C of P6-4)".into(),
        ))
    }
    async fn get_account_raw(
        &self,
        _address: &str,
    ) -> Result<Option<RawAccount>, RegistryError> {
        Err(RegistryError::Internal(
            "NoopSolanaRpc: get_account_raw not wired (B2 will swap to HttpSolanaRpc)".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AnalyzeRequest {
    pub chain: Chain,
    pub mint: String,
    pub window_hours: Option<u32>,
}

#[derive(Serialize)]
pub struct AnalyzeResponse {
    pub report: TokenRiskReport,
    pub analysis_duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[instrument(skip(state, claims), fields(chain = %req.chain.as_str(), mint = %req.mint))]
pub async fn analyze_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Json(req): Json<AnalyzeRequest>,
) -> Result<Json<AnalyzeResponse>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::WRITE_ANALYZE)?;

    // Rate-limit: use analyze bucket.
    state.rate_limiter.check_analyze(&claims.0.sub)?;

    // Validate window_hours.
    let window_hours = req.window_hours.unwrap_or(24);
    if !(1..=168).contains(&window_hours) {
        return Err(GatewayError::SemanticError(
            format!("window_hours must be between 1 and 168, got {window_hours}"),
        ));
    }

    // Validate mint address.
    Address::parse(req.chain, &req.mint)
        .map_err(|e| GatewayError::InvalidInput(format!("invalid mint address: {e}")))?;

    // Cache check: return cached report if fresh.
    if let Some(entry) = state.risk_cache.get(req.chain, &req.mint).await {
        let age = entry.inserted_at.elapsed().as_secs();
        state.metrics.scoring_cache_hits_total.inc();
        let report = (*entry.report).clone();
        return Ok(Json(AnalyzeResponse {
            report,
            analysis_duration_ms: 0,
            cache_age_seconds: Some(age),
        }));
    }
    state.metrics.scoring_cache_misses_total.inc();

    // In-flight guard.
    let in_flight_key = format!("{}/{}", req.chain.as_str(), req.mint);
    {
        let mut guard = state.in_flight_analyzes.lock().unwrap();
        if guard.contains(&in_flight_key) {
            return Err(GatewayError::AnalyzeInFlight {
                chain: req.chain,
                mint: req.mint.clone(),
            });
        }
        guard.insert(in_flight_key.clone());
    }

    state.metrics.analyze_in_flight.inc();

    let result = run_analyze(&state, req.chain, &req.mint, window_hours).await;

    state.metrics.analyze_in_flight.dec();
    {
        let mut guard = state.in_flight_analyzes.lock().unwrap();
        guard.remove(&in_flight_key);
    }

    let (report, duration_ms) = result?;

    // Cache the result.
    state
        .risk_cache
        .insert(req.chain, req.mint.clone(), Arc::new(report.clone()))
        .await;

    Ok(Json(AnalyzeResponse {
        report,
        analysis_duration_ms: duration_ms,
        cache_age_seconds: None,
    }))
}

// ---------------------------------------------------------------------------
// Core analyze logic (extracted for reuse by GET /risk on cache miss)
// ---------------------------------------------------------------------------

/// Run all detectors concurrently and score the results.
///
/// Returns `(TokenRiskReport, duration_ms)`.
pub async fn run_analyze(
    state: &AppState,
    chain: Chain,
    mint: &str,
    window_hours: u32,
) -> Result<(TokenRiskReport, u64), GatewayError> {
    let start = Instant::now();
    let now = Utc::now();

    // Compute window.
    let window_end = now;
    let window_start = window_end
        - chrono::Duration::hours(window_hours as i64);

    // Enrich token metadata.
    let meta = state
        .registry
        .enrich(mint, chain)
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("registry enrich error: {e}")))?;

    // Build address.
    let token_address = Address::parse(chain, mint)
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("address parse: {e}")))?;

    // Build DetectorWindow — use placeholder block refs since gateway doesn't
    // maintain a block-height index (design 0011 §2: gateway reads ingested data).
    let window = DetectorWindow {
        start: window_start,
        end: window_end,
        block_start: BlockRef::new(chain, 0),
        block_end: BlockRef::new(chain, u64::MAX),
    };

    let ctx = DetectorContext {
        token: &token_address,
        chain,
        window,
        observed_at: now,
        store: &state.store,
        registry: &state.registry,
        config: &state.detector_config,
        zero_address: chain_zero_address(chain),
    };

    // Run all 7 detectors concurrently (D01–D07).
    // D01 requires Arc<dyn SolanaRpc> + Arc<dyn PoolAccountProvider>.
    // Production path: use the registry's underlying RPC client (HttpSolanaRpc) and
    // the real HttpPoolAccountProvider (Sprint 9 Track B2 wiring).
    // ADR 0003 (self-sovereign): the registry's RPC endpoint is configured in
    // config/service.toml [registry] section — never Helius/Triton/Alchemy by default.
    let rpc = state.registry.rpc();
    let d01 = HoneypotDetector::new(
        state.detector_config.honeypot_sim.clone(),
        rpc.clone(),
        Arc::new(HttpPoolAccountProvider::new(rpc)),
    );
    let d02 = RugPullDetector::new(state.detector_config.rug_pull_lp_drain.clone());
    let d03 = ConcentrationDetector::new(state.detector_config.holder_concentration.clone());
    let d04 = PumpDumpDetector::new(state.detector_config.pump_dump.clone());
    let d05 = WashTradingDetector::new(state.detector_config.wash_trading_h1.clone());
    let d06 = MintBurnAnomalyDetector::new(state.detector_config.mint_burn_anomaly.clone());
    let d07 = WithdrawWithheldDetector;

    let (r01, r02, r03, r04, r05, r06, r07) = tokio::join!(
        d01.evaluate(&ctx),
        d02.evaluate(&ctx),
        d03.evaluate(&ctx),
        d04.evaluate(&ctx),
        d05.evaluate(&ctx),
        d06.evaluate(&ctx),
        d07.evaluate(&ctx),
    );

    let mut all_events = Vec::new();
    let mut skip_reasons: Vec<SkipReason> = Vec::new();

    for (result, id) in [
        (r01, "honeypot_sim"),
        (r02, "rug_pull_lp_drain"),
        (r03, "holder_concentration"),
        (r04, "pump_dump"),
        (r05, "wash_trading_h1"),
        (r06, "mint_burn_anomaly"),
        (r07, "withdraw_withheld_drain"),
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

    // Score.
    let report = state.scoring.score(&all_events, &meta, (window_start, window_end), &skip_reasons, now);

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok((report, duration_ms))
}

fn chain_zero_address(chain: Chain) -> &'static str {
    if chain.is_evm() {
        "0x0000000000000000000000000000000000000000"
    } else {
        "11111111111111111111111111111111"
    }
}
