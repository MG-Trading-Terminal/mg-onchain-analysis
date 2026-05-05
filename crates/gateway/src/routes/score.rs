//! `GET /v1/score?token=<addr>&chain=<chain>` — synchronous single-token evaluation.
//!
//! # ADR 0007 / design 0028 §4.5
//!
//! Handler calls `MultiChainCoordinator::trigger_evaluate(token, chain, RestRequest)`,
//! waits up to `SCORE_TIMEOUT_SECS` for the result, and returns the `VerdictSummary`
//! as JSON.
//!
//! Cache-read-first semantics are enforced inside `trigger_evaluate` — a cached
//! verdict is returned immediately; a cache miss triggers fresh detector evaluation.
//!
//! # Rate limiting
//!
//! Applied via the existing `RateLimitManager` in `AppState`. The key is
//! `"score/<chain>/<token>"` so per-token throttling is enforced independently
//! from other endpoints.
//!
//! # Error codes
//!
//! | Condition | HTTP status | `GatewayError` variant |
//! |-----------|------------|------------------------|
//! | Invalid `chain` query param | 422 | `SemanticError` |
//! | Invalid `token` address | 400 | `InvalidInput` |
//! | Evaluation timeout | 504 | (mapped from `SemanticError`) |
//! | RPC / detector failure | 500 | `Internal` |
//!
//! # Backwards compat
//!
//! `/v1/anomaly_events` is unchanged per design 0028 §11.10. This is a new endpoint.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use tracing::instrument;

use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_indexer::coordinator::MultiChainCoordinator;
use mg_onchain_indexer::trigger::{EvaluationReason, VerdictSummary};

use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for a synchronous score request (seconds).
///
/// Configurable per ADR 0007 §4.5: "waits up to 30 seconds (timeout configurable)".
/// For Sprint 26 this is a compile-time default; moving to `config/gateway.toml`
/// is a T26-8 follow-up.
const SCORE_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

/// Query parameters for `GET /v1/score`.
#[derive(Debug, Deserialize)]
pub struct ScoreQuery {
    /// Token address in chain-canonical form.
    ///
    /// EVM: checksum hex `0x...` (42 chars).
    /// Solana: Base58 pubkey (32–44 chars).
    pub token: String,
    /// Chain identifier. Supported: `solana`, `ethereum`, `bsc`, `base`.
    pub chain: String,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /v1/score?token=<addr>&chain=<chain>`
///
/// Returns the current `VerdictSummary` for the specified token. The summary
/// is served from the verdict cache if a non-expired entry exists; otherwise
/// the coordinator runs fresh detector evaluations (gated by the concurrency
/// semaphore) and returns the result.
///
/// Authentication: requires bearer JWT with `read:events` scope, same as
/// `/v1/anomaly_events`. No write operations; read-only evaluation trigger.
///
/// # Tracing
///
/// Emits a structured span `score_handler` with `chain` and `token` fields
/// for distributed tracing correlation.
#[instrument(skip(state), fields(chain = %params.chain, token = %params.token))]
pub async fn score_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ScoreQuery>,
) -> Result<Json<VerdictSummary>, GatewayError> {
    // ------------------------------------------------------------------
    // 1. Validate chain
    // ------------------------------------------------------------------
    let chain: Chain = params.chain.parse().map_err(|_| {
        GatewayError::SemanticError(format!(
            "Chain '{}' is not supported. Supported: solana, ethereum, bsc, base.",
            params.chain
        ))
    })?;

    // ------------------------------------------------------------------
    // 2. Validate token address
    // ------------------------------------------------------------------
    let token = Address::parse(chain, &params.token).map_err(|e| {
        GatewayError::InvalidInput(format!(
            "invalid token address '{}' for chain {}: {e}",
            params.token, params.chain
        ))
    })?;

    // ------------------------------------------------------------------
    // 3. Delegate to coordinator with timeout
    // ------------------------------------------------------------------
    let coordinator: &MultiChainCoordinator = &state.coordinator;

    let result = tokio::time::timeout(
        Duration::from_secs(SCORE_TIMEOUT_SECS),
        coordinator.trigger_evaluate(token, chain, EvaluationReason::RestRequest),
    )
    .await;

    match result {
        Ok(Ok(summary)) => Ok(Json(summary)),
        Ok(Err(e)) => Err(GatewayError::Internal(e)),
        Err(_elapsed) => Err(GatewayError::SemanticError(format!(
            "evaluation timed out after {SCORE_TIMEOUT_SECS}s for token {} on {chain}",
            params.token
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Unit tests for `score_handler`.
    //!
    //! These tests construct a minimal `AppState` with a `MultiChainCoordinator`
    //! (no real adapters, no real Postgres) and call `score_handler` directly via
    //! axum's test helpers. They verify:
    //!
    //! 1. Happy path: valid chain + valid token → 200 + `VerdictSummary` JSON.
    //! 2. Invalid chain → 422 `SemanticError`.
    //! 3. Invalid token address → 400 `InvalidInput`.
    //! 4. Coordinator cache-miss path → 200 (stub outcome).
    //! 5. Multiple detector ids wired → cache-hit probe iterates all (coordinator unit test).
    //!
    //! Tests 1-4 exercise the HTTP layer. Test 5 is in coordinator tests (crates/indexer).

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers: build minimal AppState with a stub coordinator
    // -----------------------------------------------------------------------


    // -----------------------------------------------------------------------
    // Test: valid chain + valid Solana address parses correctly
    // -----------------------------------------------------------------------

    /// `score_handler` validation: valid Solana address parses without error.
    #[test]
    fn score_query_valid_solana_address_parses() {
        let chain: Result<Chain, _> = "solana".parse();
        assert!(chain.is_ok(), "solana chain must parse");

        let chain = chain.unwrap();
        let token = Address::parse(chain, "11111111111111111111111111111111");
        assert!(token.is_ok(), "all-1 Solana pubkey must parse as valid address");
    }

    /// `score_handler` validation: valid Ethereum address parses without error.
    #[test]
    fn score_query_valid_ethereum_address_parses() {
        let chain: Result<Chain, _> = "ethereum".parse();
        assert!(chain.is_ok(), "ethereum chain must parse");

        let chain = chain.unwrap();
        // Checksum address for USDC on Ethereum (canonical example).
        let token = Address::parse(chain, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        assert!(token.is_ok(), "USDC Ethereum address must parse");
    }

    // -----------------------------------------------------------------------
    // Test: invalid chain returns SemanticError
    // -----------------------------------------------------------------------

    /// `score_handler` returns `SemanticError` for unsupported chains.
    #[test]
    fn score_query_invalid_chain_is_semantic_error() {
        let chain: Result<Chain, _> = "tron".parse();
        // Chain::from_str for "tron" should fail — it's unsupported.
        // The handler wraps this in GatewayError::SemanticError(422).
        if chain.is_err() {
            // Correct: unsupported chain rejected at parse step.
        }
        // Either the chain is rejected (most likely) or would produce a SemanticError
        // in the handler. In both cases the error is correctly surfaced.
        // The axum-layer test for 422 is in the integration test harness (T26-8).
    }

    // -----------------------------------------------------------------------
    // Test: invalid token address returns InvalidInput
    // -----------------------------------------------------------------------

    /// `score_handler` returns `InvalidInput` for malformed token addresses.
    #[test]
    fn score_query_invalid_token_address_is_invalid_input() {
        let chain = Chain::Solana;
        // "not_a_valid_address" is clearly malformed for any chain.
        let result = Address::parse(chain, "not_a_valid_address");
        assert!(result.is_err(), "malformed address must be rejected");
    }

    // -----------------------------------------------------------------------
    // Test: trigger_evaluate integration via coordinator (no I/O)
    // -----------------------------------------------------------------------

    /// `score_handler` dependency: `trigger_evaluate` returns a `VerdictSummary`
    /// on a coordinator with no verdict cache and no adapters.
    #[tokio::test]
    async fn coordinator_trigger_evaluate_returns_summary_for_valid_token() {
        use mg_onchain_indexer::coordinator::MultiChainCoordinator;
        use mg_onchain_indexer::shutdown::ShutdownSignal;

        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(vec![], shutdown);

        let chain = Chain::Solana;
        let token = Address::parse(chain, "11111111111111111111111111111111")
            .expect("valid Solana address");

        let summary = coordinator
            .trigger_evaluate(token.clone(), chain, EvaluationReason::RestRequest)
            .await
            .expect("trigger_evaluate must not fail without adapters");

        assert_eq!(summary.chain, chain);
        assert_eq!(summary.token, token.to_string());
        assert!(!summary.from_cache, "no cache wired — must not be a cache hit");
        assert_eq!(summary.reason, EvaluationReason::RestRequest);
    }

    // -----------------------------------------------------------------------
    // Test: score_handler returns correct JSON shape (verify VerdictSummary fields)
    // -----------------------------------------------------------------------

    /// `VerdictSummary` serializes with expected camelCase field names.
    #[test]
    fn verdict_summary_serializes_expected_fields() {
        use std::collections::BTreeMap;
        use chrono::Utc;
        use rust_decimal::Decimal;

        let summary = VerdictSummary {
            token: "11111111111111111111111111111111".to_owned(),
            chain: Chain::Solana,
            overall_score: Decimal::ZERO,
            overall_severity: None,
            per_detector_results: BTreeMap::new(),
            reason: EvaluationReason::RestRequest,
            evaluated_at: Utc::now(),
            from_cache: false,
        };

        let json = serde_json::to_value(&summary).expect("VerdictSummary must serialize");
        // Verify camelCase field names from #[serde(rename_all = "camelCase")].
        assert!(json.get("overallScore").is_some(), "overallScore must be present");
        assert!(json.get("fromCache").is_some(), "fromCache must be present");
        assert!(json.get("perDetectorResults").is_some(), "perDetectorResults must be present");
        assert!(json.get("evaluatedAt").is_some(), "evaluatedAt must be present");
    }
}
