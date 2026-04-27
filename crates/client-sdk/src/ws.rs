//! WebSocket client with auto-reconnect and subscription replay.
//!
//! # Protocol
//!
//! 1. Connect to `GET /v1/ws/stream` with `Authorization: Bearer <token>`.
//! 2. Send `{"action":"subscribe", ...}` with filter parameters.
//! 3. Receive `{"type":"subscribed", ...}` acknowledgement.
//! 4. Receive streamed `event`, `report`, `ping`, `lag_notice`, `closing`, or
//!    `replay_truncated` frames.
//! 5. Respond to `ping` with `{"type":"pong"}`.
//!
//! # Auto-reconnect
//!
//! On disconnect, the client retries with exponential backoff up to
//! `max_reconnect_attempts`. On each reconnect, the last-seen event ID is sent
//! as `resume_from` so the server can replay any missed events.
//!
//! If `resume_from` is rejected with a `replay_truncated` frame (event window
//! expired), the SDK emits `StreamMessage::ResumeFailed` and restarts from the
//! current live tip.
//!
//! # Heartbeat
//!
//! The gateway sends a `{"type":"ping"}` every 30 seconds. The SDK replies with
//! `{"type":"pong"}` automatically. If no ping is received within 90 seconds
//! (3× heartbeat interval), the SDK closes and reconnects.
//!
//! # Backpressure
//!
//! The SDK does not buffer beyond a single in-flight frame. Gateway `lag_notice`
//! frames are forwarded as `StreamMessage::LagNotice` to the consumer. The
//! consumer is responsible for deciding whether to reconnect.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, instrument, warn};
use url::Url;

use crate::auth::BearerToken;
use crate::error::ClientError;
use crate::types::{AnomalyFilter, StreamMessage};

// ---------------------------------------------------------------------------
// WS reconnect defaults
// ---------------------------------------------------------------------------

/// Default maximum reconnect attempts before giving up and closing the stream.
pub const DEFAULT_MAX_RECONNECT_ATTEMPTS: u32 = 10;
/// Base delay for reconnect backoff.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
/// Maximum delay cap for reconnect backoff.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// Heartbeat timeout: if no ping within this window, reconnect.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------
// WS message types (server → client)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TypedMessage {
    #[serde(rename = "type")]
    msg_type: String,
}

#[derive(Deserialize)]
struct EventFrame {
    event: Value,
}

#[derive(Deserialize)]
struct ReportFrame {
    report: Value,
    previous_score: f64,
    delta: f64,
}

#[derive(Deserialize)]
struct LagFrame {
    dropped: u64,
}

#[derive(Deserialize)]
struct ReplayTruncatedFrame {
    from_id: String,
}

// ---------------------------------------------------------------------------
// WS message types (client → server)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SubscribeMsg<'a> {
    action: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    chain: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detector_ids: Option<&'a [String]>,
    severity_min: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_from: Option<&'a str>,
}

#[derive(Serialize)]
struct PongMsg {
    #[serde(rename = "type")]
    msg_type: &'static str,
}

// ---------------------------------------------------------------------------
// AnomalyStream
// ---------------------------------------------------------------------------

/// A live anomaly event stream returned from `subscribe_anomalies`.
///
/// Implements `futures::Stream<Item = Result<StreamMessage, ClientError>>`.
/// Yields `None` when the maximum reconnect attempts are exhausted.
pub struct AnomalyStream {
    inner: tokio::sync::mpsc::Receiver<Result<StreamMessage, ClientError>>,
}

impl AnomalyStream {
    /// Receive the next stream message.
    ///
    /// Returns `None` when the stream has been permanently closed (max reconnect
    /// attempts exhausted or unrecoverable error).
    pub async fn next(&mut self) -> Option<Result<StreamMessage, ClientError>> {
        self.inner.recv().await
    }
}

// ---------------------------------------------------------------------------
// Connection config
// ---------------------------------------------------------------------------

/// Internal config for the WS connection task.
pub(crate) struct WsConfig {
    pub ws_url: Url,
    pub token: Arc<RwLock<BearerToken>>,
    pub filter: AnomalyFilter,
    pub max_reconnect_attempts: u32,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Spawn the WebSocket connection task and return an `AnomalyStream`.
///
/// The connection task runs in the background under `tokio::spawn`. It handles
/// connect, subscribe, heartbeat, and reconnect autonomously. `AnomalyStream`
/// provides a channel-backed `next()` method for the consumer.
pub(crate) fn spawn_ws_stream(config: WsConfig) -> AnomalyStream {
    // Bounded channel — 128 items. If the consumer is slow, back-pressure kicks in
    // and the WS receive loop will block, which is intentional.
    let (tx, rx) = tokio::sync::mpsc::channel(128);
    tokio::spawn(ws_connection_loop(config, tx));
    AnomalyStream { inner: rx }
}

// ---------------------------------------------------------------------------
// WS connection loop (background task)
// ---------------------------------------------------------------------------

#[instrument(skip(config, tx), fields(url = %config.ws_url))]
async fn ws_connection_loop(
    config: WsConfig,
    tx: tokio::sync::mpsc::Sender<Result<StreamMessage, ClientError>>,
) {
    let mut last_event_id: Option<String> = None;
    let mut reconnect_count: u32 = 0;

    loop {
        // Compute reconnect delay (not applied on the first connect).
        if reconnect_count > 0 {
            if reconnect_count > config.max_reconnect_attempts {
                warn!(reconnect_count, "max reconnect attempts exhausted; closing stream");
                let _ = tx
                    .send(Err(ClientError::WebSocketClosed {
                        code: None,
                        reason: format!(
                            "max reconnect attempts ({}) exhausted",
                            config.max_reconnect_attempts
                        ),
                    }))
                    .await;
                return;
            }

            let delay = reconnect_backoff(reconnect_count - 1);
            debug!(reconnect_count, ?delay, "reconnect backoff");
            tokio::time::sleep(delay).await;

            // Signal the consumer that we reconnected.
            if tx.send(Ok(StreamMessage::Reconnected)).await.is_err() {
                return; // Consumer dropped the receiver; exit quietly.
            }
        }

        // Build the WebSocket URL with auth token in query param (header not
        // available during WS upgrade in some clients).
        let ws_url = match build_ws_url(&config) {
            Ok(u) => u,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        };

        info!(url = %ws_url, reconnect_count, "connecting to WebSocket");

        let (ws_stream, _response) = match connect_ws(&ws_url, &config.token).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(reconnect_count, error = %e, "WebSocket connect failed");
                reconnect_count += 1;
                continue;
            }
        };

        let (mut sink, mut stream) = ws_stream.split();

        // Send subscribe message.
        let subscribe_msg = build_subscribe_msg(&config.filter, last_event_id.as_deref());
        let subscribe_json = match serde_json::to_string(&subscribe_msg) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(Err(ClientError::SerdeError(e))).await;
                return;
            }
        };

        if let Err(e) = sink.send(Message::Text(subscribe_json.into())).await {
            warn!(reconnect_count, error = %e, "failed to send subscribe message");
            reconnect_count += 1;
            continue;
        }

        // Process messages until disconnect or error.
        let mut last_ping = Instant::now();
        // All break paths set this to true; false is the unreachable default.
        #[allow(unused_assignments)]
        let mut connection_failed = false;

        loop {
            // Heartbeat timeout guard.
            let remaining = HEARTBEAT_TIMEOUT.saturating_sub(last_ping.elapsed());
            if remaining.is_zero() {
                warn!("heartbeat timeout — reconnecting");
                connection_failed = true;
                break;
            }

            let next_msg = timeout(remaining.min(Duration::from_secs(5)), stream.next()).await;

            let ws_msg = match next_msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "WebSocket error");
                    connection_failed = true;
                    break;
                }
                Ok(None) => {
                    debug!("WebSocket stream ended");
                    connection_failed = true;
                    break;
                }
                Err(_elapsed) => {
                    // Timeout checking — loop back to re-check heartbeat.
                    continue;
                }
            };

            match ws_msg {
                Message::Text(text) => {
                    let text_str: &str = text.as_str();
                    // Determine message type without full parse.
                    let typed: TypedMessage = match serde_json::from_str(text_str) {
                        Ok(t) => t,
                        Err(e) => {
                            warn!(error = %e, "failed to parse WS message type");
                            continue;
                        }
                    };

                    match typed.msg_type.as_str() {
                        "subscribed" => {
                            debug!("subscription acknowledged");
                        }

                        "event" => {
                            let frame: EventFrame = match serde_json::from_str(text_str) {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!(error = %e, "failed to parse event frame");
                                    continue;
                                }
                            };

                            // Track last event ID for resume_from on reconnect.
                            if let Some(id) = frame.event.get("id").and_then(|v| v.as_str()) {
                                last_event_id = Some(id.to_owned());
                            }

                            if tx.send(Ok(StreamMessage::Anomaly(frame.event))).await.is_err() {
                                return; // Consumer dropped.
                            }
                        }

                        "report" => {
                            let frame: ReportFrame = match serde_json::from_str(text_str) {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!(error = %e, "failed to parse report frame");
                                    continue;
                                }
                            };
                            if tx
                                .send(Ok(StreamMessage::RiskUpdate {
                                    report: frame.report,
                                    previous_score: frame.previous_score,
                                    delta: frame.delta,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }

                        "ping" => {
                            last_ping = Instant::now();
                            let pong = PongMsg { msg_type: "pong" };
                            if let Ok(pong_json) = serde_json::to_string(&pong) {
                                let _ = sink.send(Message::Text(pong_json.into())).await;
                            }
                        }

                        "lag_notice" => {
                            let frame: LagFrame = match serde_json::from_str(text_str) {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!(error = %e, "failed to parse lag_notice");
                                    continue;
                                }
                            };
                            warn!(dropped = frame.dropped, "lag notice from server");
                            if tx
                                .send(Ok(StreamMessage::LagNotice { dropped: frame.dropped }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }

                        "replay_truncated" => {
                            let frame: ReplayTruncatedFrame =
                                match serde_json::from_str(text_str) {
                                    Ok(f) => f,
                                    Err(e) => {
                                        warn!(error = %e, "failed to parse replay_truncated");
                                        continue;
                                    }
                                };
                            warn!(from_id = %frame.from_id, "resume_from rejected (window expired)");
                            // Clear last_event_id so next reconnect starts from live tip.
                            last_event_id = None;
                            if tx
                                .send(Ok(StreamMessage::ResumeFailed {
                                    lost_window: frame.from_id,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }

                        "closing" => {
                            debug!("server sent closing notice");
                            connection_failed = true;
                            break;
                        }

                        other => {
                            debug!(msg_type = other, "unrecognised WS message type");
                        }
                    }
                }

                Message::Close(frame) => {
                    let code: Option<u16> = frame.as_ref().map(|f| u16::from(f.code));
                    let reason = frame
                        .as_ref()
                        .map(|f| f.reason.to_string())
                        .unwrap_or_default();
                    debug!(code, reason = %reason, "WebSocket close frame received");
                    connection_failed = true;
                    break;
                }

                Message::Ping(payload) => {
                    last_ping = Instant::now();
                    let _ = sink.send(Message::Pong(payload)).await;
                }

                Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                    // Ignore binary frames and pong echoes from the server.
                }
            }
        }

        if connection_failed {
            reconnect_count += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_ws_url(config: &WsConfig) -> Result<Url, ClientError> {
    // The caller passes the HTTP base URL; convert scheme to ws/wss.
    let mut url = config.ws_url.clone();
    // Determine the new scheme before mutably borrowing `url`.
    let new_scheme: &'static str = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        "wss" | "ws" => {
            // Already a WS scheme — no conversion needed.
            return Ok(url);
        }
        _ => {
            return Err(ClientError::UrlError(url::ParseError::IdnaError));
        }
    };
    url.set_scheme(new_scheme)
        .map_err(|_| ClientError::UrlError(url::ParseError::RelativeUrlWithoutBase))?;
    Ok(url)
}

async fn connect_ws(
    url: &Url,
    token: &Arc<RwLock<BearerToken>>,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    ClientError,
> {
    let token_val = {
        let t = token.read().await;
        t.expose().to_owned()
    };

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(ClientError::WebSocketProtocol)?;

    // Attach the Authorization header to the upgrade request.
    request.headers_mut().insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token_val}"))
            .map_err(|_| ClientError::WebSocketClosed {
                code: None,
                reason: "invalid token characters for Authorization header".into(),
            })?,
    );

    connect_async_tls_with_config(request, None, false, None)
        .await
        .map_err(ClientError::WebSocketProtocol)
}

fn build_subscribe_msg<'a>(
    filter: &'a AnomalyFilter,
    resume_from: Option<&'a str>,
) -> SubscribeMsg<'a> {
    SubscribeMsg {
        action: "subscribe",
        chain: filter.chain.as_ref().map(|c| c.as_str()),
        tokens: filter.tokens.as_deref(),
        detector_ids: filter.detector_ids.as_deref(),
        severity_min: filter
            .severity_min
            .as_ref()
            .map(severity_str)
            .unwrap_or("info"),
        resume_from,
    }
}

fn severity_str(s: &crate::types::Severity) -> &'static str {
    use crate::types::Severity;
    match s {
        Severity::Info => "info",
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
        _ => "info",
    }
}

fn reconnect_backoff(attempt: u32) -> Duration {
    let exponent = attempt.min(6);
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let cap_ms = (RECONNECT_BASE_DELAY.as_millis() as u64)
        .saturating_mul(multiplier)
        .min(RECONNECT_MAX_DELAY.as_millis() as u64);
    // Simple deterministic jitter: use attempt as seed.
    let jitter_ms = crate::retry_jitter(attempt as u64, cap_ms);
    Duration::from_millis(jitter_ms)
}
