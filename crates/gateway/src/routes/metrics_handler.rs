//! `GET /metrics` — Prometheus text-format metrics endpoint.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;
use crate::state::AppState;

/// `GET /metrics` — returns Prometheus text format.
pub async fn metrics_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Response, GatewayError> {
    let text = state
        .metrics
        .encode_text()
        .map_err(GatewayError::Internal)?;

    let mut resp = (StatusCode::OK, text).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    Ok(resp)
}
