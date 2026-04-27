//! `GET /v1/detectors` — read-only introspection of loaded detector configs.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use tracing::instrument;

use crate::auth::{AuthClaims, scopes};
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DetectorListResponse {
    pub detectors: Vec<DetectorInfoResponse>,
}

#[derive(Serialize)]
pub struct DetectorInfoResponse {
    pub id: &'static str,
    pub severity_floor: &'static str,
    pub enabled: bool,
    pub thresholds: serde_json::Value,
    pub references: Vec<String>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[instrument(skip(state, claims))]
pub async fn list_detectors_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
) -> Result<Json<DetectorListResponse>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::READ_EVENTS)?;

    let cfg = &state.detector_config;

    // Build detector info list from the in-memory config.
    // Each detector exposes its thresholds as a JSON value for API consumers.
    let detectors = vec![
        DetectorInfoResponse {
            id: "honeypot_sim",
            severity_floor: "info",
            enabled: cfg.honeypot_sim.simulation_enabled.value,
            thresholds: serde_json::to_value(&cfg.honeypot_sim).unwrap_or_default(),
            references: cfg.honeypot_sim.sell_tax_threshold.refs.clone(),
        },
        DetectorInfoResponse {
            id: "rug_pull_lp_drain",
            severity_floor: "low",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.rug_pull_lp_drain).unwrap_or_default(),
            references: cfg.rug_pull_lp_drain.lp_removal_threshold.refs.clone(),
        },
        DetectorInfoResponse {
            id: "holder_concentration",
            severity_floor: "info",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.holder_concentration).unwrap_or_default(),
            references: cfg.holder_concentration.gini_delta_24h.refs.clone(),
        },
        DetectorInfoResponse {
            id: "pump_dump",
            severity_floor: "info",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.pump_dump).unwrap_or_default(),
            references: cfg.pump_dump.volume_multiplier.refs.clone(),
        },
        DetectorInfoResponse {
            id: "wash_trading_h1",
            severity_floor: "info",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.wash_trading_h1).unwrap_or_default(),
            references: cfg.wash_trading_h1.min_repetitions.refs.clone(),
        },
        DetectorInfoResponse {
            id: "mint_burn_anomaly",
            severity_floor: "info",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.mint_burn_anomaly).unwrap_or_default(),
            references: cfg.mint_burn_anomaly.supply_change_threshold_pct.refs.clone(),
        },
        // D07 — Token-2022 Withdraw-Withheld Drain (P6-0 / GAP-GW-01 closure)
        DetectorInfoResponse {
            id: "withdraw_withheld_drain",
            severity_floor: "info",
            enabled: true,
            thresholds: serde_json::to_value(&cfg.withdraw_withheld).unwrap_or_default(),
            references: cfg.withdraw_withheld.min_extraction_events.refs.clone(),
        },
    ];

    Ok(Json(DetectorListResponse { detectors }))
}
