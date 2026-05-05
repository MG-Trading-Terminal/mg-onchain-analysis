//! `GET /v1/watchlist` — WebSocket subscription for real-time verdict push.
//!
//! # ADR 0007 / design 0028 §4.5 (WS push)
//!
//! Consumers upgrade to a WebSocket connection. After the upgrade, the handler
//! subscribes to `MultiChainCoordinator::subscribe_verdicts()` — a broadcast
//! channel that receives a `VerdictSummary` every time `trigger_evaluate` completes
//! (whether from cache or fresh evaluation).
//!
//! # Message flow
//!
//! 1. Client connects to `GET /v1/watchlist`.
//! 2. Server upgrades to WebSocket.
//! 3. Server sends `{"type":"ready"}`.
//! 4. Client optionally sends `{"type":"filter","tokens":["<addr>","..."],"chain":"<chain>"}`.
//!    Unfiltered clients receive ALL verdict updates.
//! 5. Server pushes `VerdictSummary` as JSON frames whenever a verdict is produced.
//! 6. Server sends `{"type":"ping"}` every 30s; expects `{"type":"pong"}` within 60s.
//! 7. On disconnect / pong timeout: server closes the WebSocket cleanly.
//!
//! # Backpressure / lag
//!
//! The broadcast channel is sized at `VERDICT_BROADCAST_CAP` (256) entries.
//! If a receiver falls behind by more than that many verdicts, it receives
//! `tokio::sync::broadcast::error::RecvError::Lagged` — the handler sends a
//! `{"type":"lag_notice","missed":<N>}` frame and continues. This matches the
//! existing slow-consumer policy in `crates/gateway/src/ws/mod.rs`.
//!
//! # Authentication
//!
//! Requires JWT with `read:events` scope (same as `/v1/anomaly_events` and `/v1/ws/stream`).
//!
//! # Filter semantics
//!
//! Filtering is client-side (the broadcast channel carries all verdicts). When no
//! filter is set, the client receives verdicts for every token that was evaluated.
//! Filter messages are idempotent and replace the prior filter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use mg_onchain_indexer::trigger::VerdictSummary;

use crate::auth::{self, AuthClaims};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Heartbeat interval: server sends Ping frames every N seconds.
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// Pong timeout: if no Pong received within N seconds after last Ping, disconnect.
const PONG_TIMEOUT_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Client message types
// ---------------------------------------------------------------------------

/// A message sent from the client to the server over the watchlist WS.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Set or replace the active filter.
    Filter {
        /// Optional list of token addresses to watch. Empty / absent = watch all.
        #[serde(default)]
        tokens: Vec<String>,
        /// Optional chain filter. Absent = all chains.
        chain: Option<String>,
    },
    /// Pong response to a server Ping.
    Pong,
    /// Explicit unsubscribe — server will close the connection.
    Unsubscribe,
}

// ---------------------------------------------------------------------------
// Server message types
// ---------------------------------------------------------------------------

/// Serialized to the WS wire by the server.
#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    /// Sent immediately after upgrade — signals the connection is ready.
    Ready,
    /// Heartbeat ping — client must respond with `{"type":"pong"}`.
    Ping,
    /// A `VerdictSummary` update.
    Verdict {
        #[serde(flatten)]
        summary: &'a VerdictSummary,
    },
    /// Sent when the broadcast receiver lagged (missed N verdicts).
    LagNotice { missed: u64 },
    /// Sent when the connection is about to close.
    Closing { reason: &'static str },
}

// ---------------------------------------------------------------------------
// Active filter state
// ---------------------------------------------------------------------------

/// Current subscription filter for one WS connection.
#[derive(Default, Clone, Debug)]
struct Filter {
    tokens: Vec<String>,
    chain: Option<String>,
}

impl Filter {
    /// Returns `true` if the given `VerdictSummary` passes this filter.
    fn matches(&self, summary: &VerdictSummary) -> bool {
        // Chain filter.
        if self.chain.as_deref().is_some_and(|c| c != summary.chain.as_str()) {
            return false;
        }
        // Token filter.
        if !self.tokens.is_empty() && !self.tokens.contains(&summary.token) {
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// HTTP upgrade handler
// ---------------------------------------------------------------------------

/// `GET /v1/watchlist` — WebSocket upgrade for real-time verdict push.
pub async fn watchlist_ws_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Require read:events scope (same as /v1/anomaly_events).
    if let Err(e) = auth::scopes::require_scope(&claims.0.scopes, auth::scopes::scope::READ_EVENTS) {
        return e.into_response();
    }

    state.metrics.ws_active_connections.inc();
    let subject = claims.0.sub.clone();
    let state_dec = state.clone();

    ws.on_upgrade(move |socket| async move {
        handle_watchlist_connection(socket, state, subject).await;
        state_dec.metrics.ws_active_connections.dec();
    })
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

async fn handle_watchlist_connection(mut socket: WebSocket, state: Arc<AppState>, subject: String) {
    // Subscribe to the verdict broadcast channel.
    let mut verdict_rx: broadcast::Receiver<VerdictSummary> =
        state.coordinator.subscribe_verdicts();

    let heartbeat_interval = Duration::from_secs(HEARTBEAT_INTERVAL_SECS);
    let pong_timeout = Duration::from_secs(PONG_TIMEOUT_SECS);

    let mut last_ping = Instant::now();
    let mut last_pong = Instant::now();
    let mut filter = Filter::default();
    let mut heartbeat_ticker = tokio::time::interval(heartbeat_interval);

    // Send the initial "ready" frame.
    if send_server_msg(&mut socket, &ServerMessage::Ready).await.is_err() {
        debug!(subject = %subject, "watchlist WS: client disconnected before ready frame");
        return;
    }

    loop {
        tokio::select! {
            biased;

            // ------------------------------------------------------------------
            // Inbound client messages
            // ------------------------------------------------------------------
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(ClientMessage::Pong) => {
                                last_pong = Instant::now();
                            }
                            Ok(ClientMessage::Filter { tokens, chain }) => {
                                filter = Filter { tokens, chain };
                                debug!(
                                    subject = %subject,
                                    filter_tokens = filter.tokens.len(),
                                    filter_chain = ?filter.chain,
                                    "watchlist WS: filter updated"
                                );
                            }
                            Ok(ClientMessage::Unsubscribe) => {
                                let _ = send_server_msg(
                                    &mut socket,
                                    &ServerMessage::Closing { reason: "client_unsubscribe" },
                                )
                                .await;
                                break;
                            }
                            Err(_) => {
                                // Ignore malformed messages — do not disconnect on parse failure.
                                debug!(subject = %subject, "watchlist WS: ignored malformed client message");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        // axum's WS layer auto-responds to Ping; we log it here for debugging.
                        debug!(subject = %subject, ping_len = data.len(), "watchlist WS: received Ping frame");
                    }
                    _ => {}
                }
            }

            // ------------------------------------------------------------------
            // Verdict broadcast push
            // ------------------------------------------------------------------
            recv_result = verdict_rx.recv() => {
                match recv_result {
                    Ok(summary) => {
                        if filter.matches(&summary)
                            && send_server_msg(&mut socket, &ServerMessage::Verdict { summary: &summary })
                                .await
                                .is_err()
                        {
                            // Client disconnected while we were sending.
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        warn!(
                            subject = %subject,
                            missed,
                            "watchlist WS: broadcast receiver lagged — sending lag_notice"
                        );
                        if send_server_msg(&mut socket, &ServerMessage::LagNotice { missed })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The coordinator was dropped — close the WS connection.
                        let _ = send_server_msg(
                            &mut socket,
                            &ServerMessage::Closing { reason: "coordinator_closed" },
                        )
                        .await;
                        break;
                    }
                }
            }

            // ------------------------------------------------------------------
            // Heartbeat ticker
            // ------------------------------------------------------------------
            _ = heartbeat_ticker.tick() => {
                // Check pong timeout.
                if last_ping.elapsed() > pong_timeout && last_pong < last_ping {
                    debug!(subject = %subject, "watchlist WS: pong timeout — closing");
                    let _ = send_server_msg(
                        &mut socket,
                        &ServerMessage::Closing { reason: "idle_timeout" },
                    )
                    .await;
                    break;
                }

                if send_server_msg(&mut socket, &ServerMessage::Ping).await.is_err() {
                    break;
                }
                last_ping = Instant::now();
            }
        }
    }

    debug!(subject = %subject, "watchlist WS: connection closed");
}

// ---------------------------------------------------------------------------
// Helper: serialise + send one server message
// ---------------------------------------------------------------------------

async fn send_server_msg(socket: &mut WebSocket, msg: &ServerMessage<'_>) -> Result<(), ()> {
    let text = serde_json::to_string(msg).unwrap_or_else(|_| r#"{"type":"error"}"#.to_owned());
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Filter::matches — unit tests (pure logic, no I/O)
    // -----------------------------------------------------------------------

    fn make_summary(token: &str, chain: mg_onchain_common::chain::Chain) -> VerdictSummary {
        use std::collections::BTreeMap;
        use chrono::Utc;
        use rust_decimal::Decimal;
        use mg_onchain_indexer::trigger::EvaluationReason;

        VerdictSummary {
            token: token.to_owned(),
            chain,
            overall_score: Decimal::ZERO,
            overall_severity: None,
            per_detector_results: BTreeMap::new(),
            reason: EvaluationReason::WatchlistScan,
            evaluated_at: Utc::now(),
            from_cache: false,
        }
    }

    #[test]
    fn filter_empty_matches_all() {
        let filter = Filter::default();
        let summary = make_summary(
            "11111111111111111111111111111111",
            mg_onchain_common::chain::Chain::Solana,
        );
        assert!(filter.matches(&summary), "empty filter must match all verdicts");
    }

    #[test]
    fn filter_token_list_matches_listed_token() {
        let filter = Filter {
            tokens: vec!["TokenAAA".to_owned()],
            chain: None,
        };
        let matching = make_summary("TokenAAA", mg_onchain_common::chain::Chain::Solana);
        let non_matching = make_summary("TokenBBB", mg_onchain_common::chain::Chain::Solana);
        assert!(filter.matches(&matching), "listed token must match");
        assert!(!filter.matches(&non_matching), "unlisted token must not match");
    }

    #[test]
    fn filter_chain_matches_correct_chain() {
        let filter = Filter {
            tokens: vec![],
            chain: Some("ethereum".to_owned()),
        };
        let eth_summary = make_summary(
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            mg_onchain_common::chain::Chain::Ethereum,
        );
        let sol_summary = make_summary(
            "11111111111111111111111111111111",
            mg_onchain_common::chain::Chain::Solana,
        );
        assert!(filter.matches(&eth_summary), "ethereum filter must match ethereum verdict");
        assert!(!filter.matches(&sol_summary), "ethereum filter must not match solana verdict");
    }

    #[test]
    fn filter_chain_and_token_must_both_match() {
        let filter = Filter {
            tokens: vec!["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_owned()],
            chain: Some("ethereum".to_owned()),
        };
        // Correct chain, correct token.
        let matching = make_summary(
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            mg_onchain_common::chain::Chain::Ethereum,
        );
        // Correct chain, wrong token.
        let wrong_token = make_summary(
            "0xdAC17F958D2ee523a2206206994597C13D831ec7",
            mg_onchain_common::chain::Chain::Ethereum,
        );
        // Wrong chain, correct token.
        let wrong_chain = make_summary(
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            mg_onchain_common::chain::Chain::Solana,
        );
        assert!(filter.matches(&matching));
        assert!(!filter.matches(&wrong_token));
        assert!(!filter.matches(&wrong_chain));
    }

    // -----------------------------------------------------------------------
    // ServerMessage serializes correctly
    // -----------------------------------------------------------------------

    #[test]
    fn server_message_ready_serializes_type_field() {
        let json = serde_json::to_value(ServerMessage::Ready).unwrap();
        assert_eq!(json["type"], "ready");
    }

    #[test]
    fn server_message_lag_notice_includes_missed_count() {
        let json = serde_json::to_value(ServerMessage::LagNotice { missed: 42 }).unwrap();
        assert_eq!(json["type"], "lag_notice");
        assert_eq!(json["missed"], 42);
    }

    #[test]
    fn server_message_closing_includes_reason() {
        let json = serde_json::to_value(ServerMessage::Closing { reason: "idle_timeout" }).unwrap();
        assert_eq!(json["type"], "closing");
        assert_eq!(json["reason"], "idle_timeout");
    }

    // -----------------------------------------------------------------------
    // WS connect/disconnect lifecycle (no real WS — channel simulation)
    // -----------------------------------------------------------------------

    /// When the broadcast channel is closed (coordinator dropped), the WS handler
    /// loop exits cleanly. This test simulates the broadcast-closed path by creating
    /// a sender, subscribing a receiver, dropping the sender, then checking that
    /// the receiver returns `RecvError::Closed`.
    #[tokio::test]
    async fn broadcast_channel_closed_produces_recv_error_closed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<VerdictSummary>(16);
        // Drop the sender — channel is now closed.
        drop(tx);

        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => {
                // Correct: channel closed → handler exits loop.
            }
            other => panic!("expected RecvError::Closed, got {other:?}"),
        }
    }

    /// When more than `VERDICT_BROADCAST_CAP` verdicts accumulate without a receiver
    /// consuming them, the next `recv()` returns `RecvError::Lagged`.
    #[tokio::test]
    async fn broadcast_channel_lag_produces_recv_error_lagged() {
        use std::collections::BTreeMap;
        use chrono::Utc;
        use rust_decimal::Decimal;
        use mg_onchain_indexer::trigger::EvaluationReason;

        let (tx, mut rx) = tokio::sync::broadcast::channel::<VerdictSummary>(4);

        let summary = VerdictSummary {
            token: "11111111111111111111111111111111".to_owned(),
            chain: mg_onchain_common::chain::Chain::Solana,
            overall_score: Decimal::ZERO,
            overall_severity: None,
            per_detector_results: BTreeMap::new(),
            reason: EvaluationReason::WatchlistScan,
            evaluated_at: Utc::now(),
            from_cache: false,
        };

        // Send more than the buffer capacity without consuming.
        for _ in 0..6 {
            let _ = tx.send(summary.clone());
        }

        // Receiver must report Lagged (missed entries).
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => {
                assert!(n > 0, "lagged error must report at least 1 missed entry");
            }
            Ok(_) => {
                // May receive one before the lag — check the next.
                match rx.recv().await {
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        assert!(n > 0);
                    }
                    other => panic!("expected Lagged at some point, got {other:?}"),
                }
            }
            other => panic!("expected Lagged or Ok, got {other:?}"),
        }
    }
}
