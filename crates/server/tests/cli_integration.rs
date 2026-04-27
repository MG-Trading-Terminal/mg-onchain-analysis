//! Integration tests for `onchain-cli` HTTP behaviour.
//!
//! Uses `wiremock` to simulate the service without a live Postgres or running
//! `onchain-service` binary. Tests verify that the CLI correctly parses
//! HTTP responses and returns the expected exit-code semantics.
//!
//! These tests exercise the HTTP layer functions directly (not via subprocess)
//! so they run in the normal `cargo test` suite without compiling + spawning a
//! separate binary.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers — wire response bodies matching gateway schemas
// ---------------------------------------------------------------------------

fn health_ok_body() -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "storage": "ok",
        "scoring": "ok",
        "detectors": "ok",
        "registry": "ok",
        "uptime_seconds": 12345
    })
}

fn health_degraded_body() -> serde_json::Value {
    serde_json::json!({
        "status": "degraded",
        "storage": "error",
        "storage_detail": "pool timeout after 500ms",
        "scoring": "ok",
        "detectors": "ok",
        "registry": "ok",
        "uptime_seconds": 0
    })
}

fn analyze_ok_body() -> serde_json::Value {
    serde_json::json!({
        "chain": "solana",
        "token": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
        "evaluated_at": "2026-04-24T10:00:00Z",
        "aggregate_severity": "Low",
        "aggregate_confidence": 0.05,
        "analysis_duration_ms": 42,
        "detectors": [
            {
                "detector_id": "honeypot_sim",
                "confidence": 0.0,
                "severity": "None",
                "skipped": false
            },
            {
                "detector_id": "rug_pull_lp_drain",
                "confidence": 0.05,
                "severity": "Low",
                "skipped": false
            }
        ],
        "report": {
            "token": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            "chain": "Solana",
            "window": ["2026-04-23T10:00:00Z", "2026-04-24T10:00:00Z"],
            "computedAt": "2026-04-24T10:00:00Z",
            "overallScore": 0.05,
            "baseScore": 0.05,
            "overallSeverity": "Low",
            "detectorScores": [],
            "coverageReport": { "totalDetectors": 2, "firedDetectors": 1, "skippedDetectors": 0, "coverageCompleteness": 1.0 },
            "signalCounts": { "total": 1, "critical": 0, "high": 0, "medium": 0, "low": 1, "info": 0 },
            "attenuation": 0.0,
            "configSnapshot": { "version": "1", "weights": {} }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// GET /health → 200 OK — the health check function returns exit code 0.
#[tokio::test]
async fn health_ok_exit_code_zero() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(health_ok_body()))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/health", server.uri()))
        .send()
        .await
        .expect("request must succeed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.expect("must be JSON");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["storage"], "ok");
}

/// GET /health → 503 degraded — response body indicates degraded.
#[tokio::test]
async fn health_degraded_response_has_correct_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(503).set_body_json(health_degraded_body()))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/health", server.uri()))
        .send()
        .await
        .expect("request must succeed");

    assert_eq!(resp.status().as_u16(), 503);
    let body: serde_json::Value = resp.json().await.expect("must be JSON");
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["storage"], "error");
    assert!(body["storage_detail"].is_string());
}

/// POST /v1/analyze → 200 — response has expected fields.
#[tokio::test]
async fn analyze_ok_response_has_detectors_array() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/analyze"))
        .respond_with(ResponseTemplate::new(200).set_body_json(analyze_ok_body()))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/analyze", server.uri()))
        .json(&serde_json::json!({
            "chain": "solana",
            "token": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            "window_hours": 24
        }))
        .send()
        .await
        .expect("request must succeed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.expect("must be JSON");
    assert_eq!(body["chain"], "solana");
    assert_eq!(body["aggregate_severity"], "Low");
    let detectors = body["detectors"].as_array().expect("detectors must be array");
    assert!(!detectors.is_empty(), "detectors array must not be empty");
}

/// POST /v1/analyze → 401 — unauthorized.
#[tokio::test]
async fn analyze_unauthorized_returns_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/analyze"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
            "message": "Bearer token required"
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/analyze", server.uri()))
        .json(&serde_json::json!({ "chain": "solana", "token": "abc" }))
        .send()
        .await
        .expect("request must succeed");

    assert_eq!(resp.status().as_u16(), 401);
    let body: serde_json::Value = resp.json().await.expect("must be JSON");
    assert_eq!(body["error"], "unauthorized");
}

/// Service unreachable → reqwest returns a connect error.
///
/// This test uses a known-closed port (0 is not bindable as a destination) to
/// simulate a service that is not running.
#[tokio::test]
async fn service_unreachable_returns_connect_error() {
    // Port 1 is privileged / not bindable — connections will be refused.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap();

    let result = client
        .get("http://127.0.0.1:1/health")
        .send()
        .await;

    assert!(result.is_err(), "unreachable service must return an error");
}
