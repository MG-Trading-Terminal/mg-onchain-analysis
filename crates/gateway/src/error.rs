//! `GatewayError` — RFC 7807 problem details error type for the gateway.
//!
//! Every handler returns `Result<Json<T>, GatewayError>`.
//! `GatewayError` implements `axum::response::IntoResponse`, which serializes to
//! an RFC 7807 JSON body with `Content-Type: application/problem+json`.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;
use tracing::error;

use mg_onchain_common::chain::Chain;

// ---------------------------------------------------------------------------
// GatewayError
// ---------------------------------------------------------------------------

/// All gateway error variants.
///
/// `#[non_exhaustive]` to allow adding new variants without breaking downstream
/// crates that match on this enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// 400 — Malformed address, missing required field, unparseable input.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// 401 — Missing, expired, or invalid JWT.
    #[error("unauthenticated")]
    Unauthenticated,

    /// 403 — Valid JWT but missing required scope.
    #[error("unauthorized: scope '{0}' required")]
    Unauthorized(String),

    /// 404 — Token not in registry, no cached report.
    #[error("token not found: {chain}/{mint}")]
    TokenNotFound { chain: Chain, mint: String },

    /// 409 — Same `(chain, mint)` analyze already in flight.
    #[error("analyze already in flight for {chain}/{mint}")]
    AnalyzeInFlight { chain: Chain, mint: String },

    /// 409 generic conflict (e.g. username already exists).
    #[error("conflict: {0}")]
    Conflict(String),

    /// 422 — Well-formed but semantically invalid (unsupported chain, window OOB).
    #[error("semantic error: {0}")]
    SemanticError(String),

    /// 429 — Rate limit exceeded. Carries retry-after duration.
    #[error("rate limited")]
    RateLimited { retry_after: Duration },

    /// 500 — Unexpected internal error. Full error is logged; wire response is generic.
    #[error("internal: {0:#}")]
    Internal(#[from] anyhow::Error),

    /// 503 — A required backend component is unhealthy.
    #[error("component unhealthy: {0}")]
    Unhealthy(String),
}

// ---------------------------------------------------------------------------
// Wire shape
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// IntoResponse
// ---------------------------------------------------------------------------

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        match self {
            GatewayError::InvalidInput(detail) => problem_response(
                StatusCode::BAD_REQUEST,
                "https://mg-onchain/errors/invalid-input",
                "Invalid input",
                &detail,
                None,
            ),

            GatewayError::Unauthenticated => problem_response(
                StatusCode::UNAUTHORIZED,
                "https://mg-onchain/errors/unauthenticated",
                "Authentication required",
                "Missing, expired, or invalid JWT. Re-authenticate via POST /v1/auth/token.",
                None,
            ),

            GatewayError::Unauthorized(scope) => problem_response(
                StatusCode::FORBIDDEN,
                "https://mg-onchain/errors/unauthorized",
                "Insufficient scope",
                &format!("Scope '{scope}' is required for this endpoint."),
                None,
            ),

            GatewayError::TokenNotFound { chain, mint } => problem_response(
                StatusCode::NOT_FOUND,
                "https://mg-onchain/errors/token-not-found",
                "Token not found",
                &format!("No risk data for {}/{mint}.", chain.as_str()),
                None,
            ),

            GatewayError::AnalyzeInFlight { chain, mint } => problem_response(
                StatusCode::CONFLICT,
                "https://mg-onchain/errors/analyze-in-flight",
                "Analyze already in flight",
                &format!(
                    "An analyze for {}/{mint} is already running. Retry in ~500ms.",
                    chain.as_str()
                ),
                None,
            ),

            GatewayError::Conflict(detail) => problem_response(
                StatusCode::CONFLICT,
                "https://mg-onchain/errors/conflict",
                "Conflict",
                &detail,
                None,
            ),

            GatewayError::SemanticError(detail) => problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "https://mg-onchain/errors/semantic-error",
                "Semantic error",
                &detail,
                None,
            ),

            GatewayError::RateLimited { retry_after } => {
                let secs = retry_after.as_secs().max(1);
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::RETRY_AFTER,
                    HeaderValue::from_str(&secs.to_string()).unwrap_or(HeaderValue::from_static("1")),
                );
                let body = json!({
                    "type": "https://mg-onchain/errors/rate-limited",
                    "title": "Rate limit exceeded",
                    "status": 429u16,
                    "detail": format!("Rate limit exceeded. Retry after {secs} seconds."),
                    "instance": "",
                    "trace_id": "",
                });
                let mut resp = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
                resp.headers_mut().extend(headers);
                resp
            }

            GatewayError::Internal(err) => {
                // Log full error but do NOT expose internals on the wire.
                error!(error = %err, "internal gateway error");
                problem_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "https://mg-onchain/errors/internal-error",
                    "Internal server error",
                    "An unexpected error occurred. Please contact support with the trace_id.",
                    None,
                )
            }

            GatewayError::Unhealthy(component) => problem_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "https://mg-onchain/errors/component-unhealthy",
                "Component unhealthy",
                &format!("{component} is unavailable. Retry after recovery."),
                None,
            ),
        }
    }
}

fn problem_response(
    status: StatusCode,
    type_uri: &str,
    title: &str,
    detail: &str,
    instance: Option<&str>,
) -> Response {
    let body = json!({
        "type": type_uri,
        "title": title,
        "status": status.as_u16(),
        "detail": detail,
        "instance": instance.unwrap_or(""),
        "trace_id": "",
    });
    let mut resp = (status, axum::Json(body)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    resp
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn invalid_input_is_400() {
        let resp = GatewayError::InvalidInput("bad mint".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let json = body_json(resp).await;
        assert_eq!(json["status"], 400);
        assert_eq!(json["type"], "https://mg-onchain/errors/invalid-input");
    }

    #[tokio::test]
    async fn unauthenticated_is_401() {
        let resp = GatewayError::Unauthenticated.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(resp).await;
        assert_eq!(json["status"], 401);
    }

    #[tokio::test]
    async fn unauthorized_is_403() {
        let resp = GatewayError::Unauthorized("write:analyze".into()).into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let json = body_json(resp).await;
        assert_eq!(json["status"], 403);
        assert!(json["detail"].as_str().unwrap().contains("write:analyze"));
    }

    #[tokio::test]
    async fn rate_limited_has_retry_after_header() {
        let resp = GatewayError::RateLimited {
            retry_after: Duration::from_secs(23),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(retry_after, "23");
    }

    #[tokio::test]
    async fn internal_error_does_not_expose_message() {
        let err = GatewayError::Internal(anyhow::anyhow!("secret db password"));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = body_json(resp).await;
        // Wire response must NOT contain the internal error message.
        let detail = json["detail"].as_str().unwrap_or("");
        assert!(!detail.contains("secret db password"));
    }

    #[tokio::test]
    async fn problem_detail_has_required_fields() {
        let resp = GatewayError::SemanticError("chain 'tron' not supported".into()).into_response();
        let json = body_json(resp).await;
        assert!(json.get("type").is_some());
        assert!(json.get("title").is_some());
        assert!(json.get("status").is_some());
        assert!(json.get("detail").is_some());
        assert!(json.get("instance").is_some());
        assert!(json.get("trace_id").is_some());
    }
}
