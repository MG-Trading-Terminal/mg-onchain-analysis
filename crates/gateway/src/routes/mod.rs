//! Route builder — assembles the full axum Router with all routes and middleware.

pub mod admin;
pub mod analyze;
pub mod analyze_v2;
pub mod auth_handler;
pub mod detectors_handler;
pub mod events;
pub mod health;
pub mod metrics_handler;
pub mod risk;
pub mod score;

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::state::AppState;
use crate::ws;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Public / unauthenticated
        .route("/health", get(health::health_handler))
        .route("/metrics", get(metrics_handler::metrics_handler))
        .route("/v1/auth/token", post(auth_handler::issue_token_handler))
        .route("/v1/.well-known/jwks.json", get(auth_handler::jwks_handler))
        // Authenticated REST endpoints
        .route("/v1/tokens/analyze", post(analyze::analyze_handler))
        // Sprint 25: all-13-detector on-demand endpoint (Track 2).
        .route("/v1/analyze", post(analyze_v2::analyze_v2_handler))
        .route("/v1/tokens/{chain}/{mint}/risk", get(risk::get_token_risk_handler))
        .route("/v1/anomaly_events", get(events::list_anomaly_events_handler))
        .route("/v1/detectors", get(detectors_handler::list_detectors_handler))
        // Sprint 26 T26-6: Pull-based query engine REST + WS (ADR 0007 / design 0028 §4.5).
        // NOTE: /v1/anomaly_events is preserved unchanged per design 0028 §11.10 (backwards compat).
        .route("/v1/score", get(score::score_handler))
        // Admin endpoints
        .route("/v1/admin/cache/{chain}/{mint}", delete(admin::invalidate_cache_handler))
        .route("/v1/admin/users", post(admin::create_user_handler))
        // WebSocket: existing stream + new watchlist (T26-6)
        .route("/v1/ws/stream", get(ws::ws_stream_handler))
        .route("/v1/watchlist", get(ws::watchlist::watchlist_ws_handler))
        // Tower middleware stack
        .layer(
            tower::ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

