//! WebSocket handlers.
//!
//! - `mod.rs` (`ws_stream_handler`) — `GET /v1/ws/stream`: anomaly event push.
//! - `watchlist` (`watchlist_ws_handler`) — `GET /v1/watchlist`: real-time verdict push
//!   (T26-6, ADR 0007 / design 0028 §4.5).
//!
//! # `ws_stream_handler` connection lifecycle
//!
//! 1. JWT validation (from Authorization header or ?token= param).
//! 2. WebSocket upgrade.
//! 3. Client sends `{"action":"subscribe", ...}`.
//! 4. Server sends `{"type":"subscribed", ...}`.
//! 5. Server polls `anomaly_events` every 500ms and pushes new events.
//! 6. Server sends `{"type":"ping"}` every 30s; client must respond with `{"type":"pong"}`.
//! 7. On buffer overflow: server sends `{"type":"lag_notice", ...}`.
//! 8. On disconnect/shutdown: server sends `{"type":"closing", ...}`.

pub mod watchlist;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::response::IntoResponse;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, warn};

use crate::auth::{self, AuthClaims};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Helper: build a WS text message from a serializable value
// ---------------------------------------------------------------------------

fn ws_text(v: &impl serde::Serialize) -> Message {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string());
    Message::Text(s.into())
}

fn ws_text_str(s: String) -> Message {
    Message::Text(s.into())
}

// ---------------------------------------------------------------------------
// WebSocket message types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ClientAction {
    pub action: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
pub struct SubscribeMessage {
    pub chain: Option<String>,
    pub tokens: Option<Vec<String>>,
    pub detector_ids: Option<Vec<String>>,
    #[serde(default = "default_severity_min")]
    pub severity_min: String,
    pub resume_from: Option<String>,
}

fn default_severity_min() -> String { "info".to_string() }

#[derive(Serialize)]
struct SubscribedMsg<'a> {
    #[serde(rename = "type")]
    msg_type: &'a str,
    subscription_id: &'a str,
    effective_filters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct PingMsg {
    #[serde(rename = "type")]
    msg_type: &'static str,
}

#[derive(Serialize)]
struct LagNoticeMsg {
    #[serde(rename = "type")]
    msg_type: &'static str,
    dropped: usize,
    buffer_capacity: usize,
    recommendation: &'static str,
}

#[derive(Serialize)]
struct ClosingMsg {
    #[serde(rename = "type")]
    msg_type: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
struct ReplayTruncatedMsg<'a> {
    #[serde(rename = "type")]
    msg_type: &'static str,
    from_id: &'a str,
    message: &'a str,
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

// ---------------------------------------------------------------------------
// HTTP Upgrade handler
// ---------------------------------------------------------------------------

pub async fn ws_stream_handler(
    State(state): State<Arc<AppState>>,
    Query(_query): Query<WsQuery>,
    claims: AuthClaims,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Check read:events scope.
    if let Err(e) = auth::scopes::require_scope(&claims.0.scopes, auth::scopes::scope::READ_EVENTS) {
        return e.into_response();
    }

    state.metrics.ws_active_connections.inc();
    let state_clone = state.clone();
    let state_dec = state.clone();
    let sub = claims.0.sub.clone();

    ws.on_upgrade(move |socket| async move {
        handle_ws_connection(socket, state_clone, sub).await;
        state_dec.metrics.ws_active_connections.dec();
    })
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

async fn handle_ws_connection(mut socket: WebSocket, state: Arc<AppState>, subject: String) {
    let ws_config = &state.config.gateway.ws;
    let heartbeat_interval = Duration::from_secs(ws_config.heartbeat_interval_seconds);
    let heartbeat_timeout = Duration::from_secs(ws_config.heartbeat_timeout_seconds);
    let send_buffer_capacity = ws_config.send_buffer_capacity;
    let lag_notice_threshold = ws_config.lag_notice_threshold;

    let (tx, mut rx): (mpsc::Sender<serde_json::Value>, mpsc::Receiver<serde_json::Value>) =
        mpsc::channel(send_buffer_capacity);

    let mut subscription: Option<SubscribeMessage> = None;
    let mut subscription_id = String::new();
    let mut last_event_id: i64 = 0;
    let mut last_ping = Instant::now();
    let mut last_pong = Instant::now();
    let mut dropped_count: usize = 0;

    let mut broadcast_rx = state.invalidation_tx.subscribe();

    let poll_interval = Duration::from_millis(state.config.gateway.ws.poll_interval_ms);
    let mut poll_ticker = time::interval(poll_interval);
    let mut heartbeat_ticker = time::interval(heartbeat_interval);

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let text_str = text.as_str().to_string();

                        // Check if it's a pong response.
                        if let Ok(action) = serde_json::from_str::<ClientAction>(&text_str) {
                            if action.msg_type.as_deref() == Some("pong") {
                                last_pong = Instant::now();
                                continue;
                            }
                            if action.action.as_deref() == Some("subscribe")
                                && let Ok(sub_msg) = serde_json::from_str::<SubscribeMessage>(&text_str)
                            {
                                subscription_id = format!("sub_{}", hex::encode(&uuid::Uuid::new_v4().as_bytes()[..3]));
                                subscription = Some(sub_msg.clone());

                                // Handle resume_from.
                                if let Some(ref from_id_str) = sub_msg.resume_from {
                                    if let Ok(from_id) = from_id_str.parse::<i64>() {
                                        let lookback = Utc::now() - chrono::Duration::minutes(state.config.gateway.ws.replay_lookback_minutes as i64);
                                        match state.store.fetch_anomaly_events_paginated(
                                            sub_msg.chain.as_deref(), None, None,
                                            &sub_msg.severity_min,
                                            Some(lookback), Utc::now(),
                                            None, Some(from_id), 500,
                                        ).await {
                                            Ok(rows) => {
                                                for row in rows {
                                                    let frame = serde_json::json!({
                                                        "type": "replay",
                                                        "subscription_id": subscription_id,
                                                        "event": row.to_json_value()
                                                    });
                                                    last_event_id = row.id;
                                                    let msg_text = frame.to_string();
                                                    if socket.send(ws_text_str(msg_text)).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "replay fetch error"),
                                        }
                                    } else {
                                        let trunc = ReplayTruncatedMsg {
                                            msg_type: "replay_truncated",
                                            from_id: from_id_str,
                                            message: "Cannot replay: invalid event ID format.",
                                        };
                                        if socket.send(ws_text(&trunc)).await.is_err() {
                                            return;
                                        }
                                    }
                                }

                                let filters = serde_json::json!({
                                    "chain": sub_msg.chain,
                                    "tokens": sub_msg.tokens,
                                    "detector_ids": sub_msg.detector_ids,
                                    "severity_min": sub_msg.severity_min,
                                });
                                let ack = SubscribedMsg {
                                    msg_type: "subscribed",
                                    subscription_id: &subscription_id,
                                    effective_filters: &filters,
                                };
                                if socket.send(ws_text(&ack)).await.is_err() {
                                    return;
                                }
                            }
                            if action.action.as_deref() == Some("unsubscribe") {
                                subscription = None;
                                let msg_text = r#"{"type":"unsubscribed"}"#.to_string();
                                let _ = socket.send(ws_text_str(msg_text)).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }

            _ = heartbeat_ticker.tick() => {
                // Check pong timeout.
                if last_ping.elapsed() > heartbeat_timeout && last_pong < last_ping {
                    debug!("WebSocket pong timeout — closing");
                    let closing = ClosingMsg { msg_type: "closing", reason: "idle_timeout" };
                    let _ = socket.send(ws_text(&closing)).await;
                    break;
                }
                let ping = PingMsg { msg_type: "ping" };
                if socket.send(ws_text(&ping)).await.is_err() {
                    break;
                }
                last_ping = Instant::now();
            }

            _ = poll_ticker.tick() => {
                if subscription.is_none() {
                    continue;
                }
                let sub = subscription.as_ref().unwrap();

                let to = Utc::now();
                match state.store.fetch_anomaly_events_paginated(
                    sub.chain.as_deref(), None, None,
                    &sub.severity_min,
                    None, to,
                    None, Some(last_event_id), 100,
                ).await {
                    Ok(new_rows) => {
                        for row in new_rows.into_iter().rev() {
                            if let Some(ref tokens) = sub.tokens
                                && !tokens.is_empty()
                                && !tokens.iter().any(|t| t == &row.token)
                            {
                                continue;
                            }
                            if let Some(ref dids) = sub.detector_ids
                                && !dids.is_empty()
                                && !dids.iter().any(|d| d == &row.detector_id)
                            {
                                continue;
                            }

                            last_event_id = last_event_id.max(row.id);

                            let frame = serde_json::json!({
                                "type": "event",
                                "subscription_id": subscription_id,
                                "event": row.to_json_value()
                            });

                            match tx.try_send(frame) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    dropped_count += 1;
                                    if dropped_count >= lag_notice_threshold {
                                        let lag = LagNoticeMsg {
                                            msg_type: "lag_notice",
                                            dropped: dropped_count,
                                            buffer_capacity: send_buffer_capacity,
                                            recommendation: "Reduce subscription scope or increase processing speed",
                                        };
                                        state.metrics.ws_lag_notices_total.inc();
                                        let _ = socket.send(ws_text(&lag)).await;
                                        dropped_count = 0;
                                    }
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "ws event poll error"),
                }
            }

            Some(frame) = rx.recv() => {
                let msg_text = frame.to_string();
                if socket.send(ws_text_str(msg_text)).await.is_err() {
                    break;
                }
            }

            Ok(_inval) = broadcast_rx.recv() => {
                // Phase 3: re-score and push report delta.
            }
        }
    }

    debug!(subject = %subject, "WebSocket connection closed");
}
