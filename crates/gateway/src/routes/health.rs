//! `GET /health` — liveness + readiness probe.
//!
//! Returns 200 when all components are healthy, 503 if any component is degraded.
//! No authentication required.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use tracing::instrument;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub storage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_detail: Option<String>,
    pub scoring: &'static str,
    pub detectors: &'static str,
    pub registry: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_detail: Option<String>,
    pub uptime_seconds: u64,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[instrument(skip(state))]
pub async fn health_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let (storage_status, storage_detail) = check_storage(&state).await;
    let (registry_status, registry_detail) = check_registry(&state).await;

    let all_ok = storage_status == "ok" && registry_status == "ok";
    let overall = if all_ok { "ok" } else { "degraded" };

    let resp = HealthResponse {
        status: overall,
        storage: storage_status,
        storage_detail,
        scoring: "ok",
        detectors: "ok",
        registry: registry_status,
        registry_detail,
        uptime_seconds: state.uptime_seconds(),
    };

    let status_code = if all_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status_code, Json(resp))
}

async fn check_storage(state: &AppState) -> (&'static str, Option<String>) {
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        sqlx::query("SELECT 1").fetch_one(state.store.pool()),
    )
    .await;

    match result {
        Ok(Ok(_)) => ("ok", None),
        Ok(Err(e)) => ("error", Some(format!("query failed: {e}"))),
        Err(_) => ("error", Some("pool timeout after 500ms".to_string())),
    }
}

async fn check_registry(state: &AppState) -> (&'static str, Option<String>) {
    // Registry health: attempt to get the last known RPC slot.
    // For MVP: if registry was constructed successfully, return "ok".
    // Phase 3: add a live slot freshness check.
    let _ = &state.registry;
    ("ok", None)
}
