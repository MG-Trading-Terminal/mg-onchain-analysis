//! `GET /v1/anomaly_events` — cursor-paginated anomaly event feed.
//!
//! # Cursor design
//!
//! The cursor is a base64-encoded JSON blob encoding `(observed_at, id)`.
//! Postgres query: `WHERE (observed_at, id) < ($cursor_ts, $cursor_id) ORDER BY observed_at DESC, id DESC LIMIT $limit`.
//!
//! This is a stable keyset cursor that does not degrade under concurrent inserts.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::instrument;


use crate::auth::{AuthClaims, scopes};
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub chain: Option<String>,
    pub token: Option<String>,
    pub detector_id: Option<String>,
    pub severity_min: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}

fn default_limit() -> u32 { 50 }

// ---------------------------------------------------------------------------
// Cursor encode / decode
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct CursorPayload {
    oat: DateTime<Utc>,
    id: i64,
}

pub fn encode_cursor(observed_at: DateTime<Utc>, id: i64) -> String {
    let payload = CursorPayload { oat: observed_at, id };
    let json = serde_json::to_vec(&payload).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(&json)
}

pub fn decode_cursor(cursor: &str) -> Result<CursorPayload, GatewayError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| GatewayError::InvalidInput("invalid cursor encoding".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| GatewayError::InvalidInput("invalid cursor payload".into()))
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct AnomalyEventPage {
    pub events: Vec<serde_json::Value>,
    pub next_cursor: Option<String>,
    pub total_in_page: usize,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

#[instrument(skip(state, claims), fields(limit = %query.limit))]
pub async fn list_anomaly_events_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Query(query): Query<EventsQuery>,
) -> Result<Json<AnomalyEventPage>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::READ_EVENTS)?;

    // Clamp limit to [1, 500].
    let limit = query.limit.clamp(1, 500) as i64;

    // Validate chain if provided.
    let chain_str = query.chain.as_deref();
    if let Some(c) = chain_str
        && !matches!(c, "solana" | "ethereum" | "bsc" | "base")
    {
        return Err(GatewayError::SemanticError(
            format!("Chain '{c}' is not supported. Supported: solana, ethereum, bsc, base."),
        ));
    }

    // Parse severity_min.
    let severity_min = match query.severity_min.as_deref().unwrap_or("info") {
        "info" => "info",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "critical" => "critical",
        other => {
            return Err(GatewayError::InvalidInput(
                format!("unknown severity_min '{other}'"),
            ))
        }
    };

    let to = query.to.unwrap_or_else(Utc::now);

    // Decode cursor.
    let (cursor_oat, cursor_id): (Option<DateTime<Utc>>, Option<i64>) =
        if let Some(ref c) = query.cursor {
            let p = decode_cursor(c)?;
            (Some(p.oat), Some(p.id))
        } else {
            (None, None)
        };

    // Fetch from storage.
    let rows = state
        .store
        .fetch_anomaly_events_paginated(
            chain_str,
            query.token.as_deref(),
            query.detector_id.as_deref(),
            severity_min,
            query.from,
            to,
            cursor_oat,
            cursor_id,
            limit + 1, // fetch one extra to detect next page
        )
        .await
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("fetch events error: {e}")))?;

    let has_more = rows.len() as i64 > limit;
    let page_rows: Vec<_> = rows.into_iter().take(limit as usize).collect();

    let next_cursor = if has_more {
        page_rows.last().map(|r| encode_cursor(r.observed_at, r.id))
    } else {
        None
    };

    let total_in_page = page_rows.len();

    // Serialize events to JSON values (the stored JSON blob is already serialized).
    let events: Vec<serde_json::Value> = page_rows
        .into_iter()
        .map(|r| r.to_json_value())
        .collect();

    Ok(Json(AnomalyEventPage {
        events,
        next_cursor,
        total_in_page,
    }))
}
