//! WebSocket mock tests.
//!
//! These tests spin up a real tokio-tungstenite WS server and verify:
//! - 3 anomaly events + 1 lag_notice + 1 ping → SDK yields 4 messages in order.
//! - Auto-reconnect: server disconnects after 2 events; SDK reconnects; 2 more events;
//!   stream yields 4 total events plus a Reconnected notification.
//!
//! # Why a real WS server
//!
//! wiremock does not support WebSocket. We spin up a bare tokio-tungstenite
//! listener on an ephemeral port for each test.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mg_onchain_client_sdk::{
    OnchainAnalysisClient,
    types::{AnomalyFilter, StreamMessage},
};
use mg_onchain_common::chain::Chain;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async};
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bind a random TCP port and return the listener + the ws:// URL.
async fn bind_ws_server() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind WS test server");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://127.0.0.1:{}", addr.port());
    (listener, url)
}

/// Accept exactly one WebSocket connection from the listener.
async fn accept_one(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("accept");
    accept_async(stream).await.expect("WS handshake")
}

/// Build an SDK client pointed at the given WS base URL (http scheme).
fn make_ws_client(ws_base_url: &str) -> OnchainAnalysisClient {
    // The SDK converts http→ws internally, so we pass the http equivalent.
    let http_url = ws_base_url.replace("ws://", "http://");
    OnchainAnalysisClient::builder()
        .base_url(&http_url)
        .bearer_token("test-token")
        .timeout(Duration::from_secs(5))
        .max_reconnect_attempts(5)
        .build()
        .expect("client build")
}

/// Build a text frame JSON message.
fn text_msg(v: &serde_json::Value) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

// ---------------------------------------------------------------------------
// Test: 3 events + 1 lag_notice + 1 ping → 5 messages (4 from stream: 3 anomaly + 1 lag)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_receives_events_and_lag_notice() {
    let (listener, ws_url) = bind_ws_server().await;

    // Server task
    tokio::spawn(async move {
        let mut ws = accept_one(&listener).await;

        // Consume the subscribe message from the client.
        let _ = ws.next().await;

        // Send subscribed acknowledgement.
        let _ = ws
            .send(text_msg(&serde_json::json!({
                "type": "subscribed",
                "subscription_id": "sub_test",
                "effective_filters": {}
            })))
            .await;

        // Send 3 anomaly event frames.
        for i in 1u32..=3 {
            let _ = ws
                .send(text_msg(&serde_json::json!({
                    "type": "event",
                    "subscription_id": "sub_test",
                    "event": {
                        "id": format!("ev-{i}"),
                        "detectorId": "rug_pull_lp_drain",
                        "confidence": 0.9,
                        "severity": "high"
                    }
                })))
                .await;
        }

        // Send a lag_notice.
        let _ = ws
            .send(text_msg(&serde_json::json!({
                "type": "lag_notice",
                "dropped": 42,
                "buffer_capacity": 1000,
                "recommendation": "slow down"
            })))
            .await;

        // Send a ping (SDK should auto-pong).
        let _ = ws.send(Message::Ping(vec![].into())).await;

        // Let the server idle until the client closes.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = ws.close(None).await;
    });

    let client = make_ws_client(&ws_url);
    let mut stream = client
        .subscribe_anomalies(AnomalyFilter {
            chain: Some(Chain::Solana),
            ..Default::default()
        })
        .await
        .expect("subscribe should succeed");

    let mut anomaly_count = 0;
    let mut lag_count = 0;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await
        else {
            break;
        };

        match msg.expect("stream item should be Ok") {
            StreamMessage::Anomaly(_) => anomaly_count += 1,
            StreamMessage::LagNotice { dropped } => {
                assert_eq!(dropped, 42);
                lag_count += 1;
            }
            StreamMessage::Reconnected => {} // ignore reconnect notifications
            other => {
                // RiskUpdate or ResumeFailed are unexpected here.
                panic!("unexpected stream message: {other:?}");
            }
        }

        if anomaly_count == 3 && lag_count == 1 {
            break;
        }
    }

    assert_eq!(anomaly_count, 3, "expected 3 anomaly events");
    assert_eq!(lag_count, 1, "expected 1 lag_notice");
}

// ---------------------------------------------------------------------------
// Test: auto-reconnect — server disconnects after 2 events; SDK reconnects;
//       receives 2 more events; stream yields all 4 events.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_auto_reconnect_after_disconnect() {
    let (listener, ws_url) = bind_ws_server().await;

    // Server task: two connections, 2 events each.
    tokio::spawn(async move {
        // Connection 1: send 2 events then close.
        {
            let mut ws = accept_one(&listener).await;
            let _ = ws.next().await; // consume subscribe

            let _ = ws
                .send(text_msg(&serde_json::json!({"type":"subscribed","subscription_id":"s1","effective_filters":{}})))
                .await;

            for i in 1u32..=2 {
                let _ = ws
                    .send(text_msg(&serde_json::json!({
                        "type": "event",
                        "subscription_id": "s1",
                        "event": { "id": format!("conn1-ev-{i}"), "detectorId": "pump_dump", "confidence": 0.7 }
                    })))
                    .await;
            }

            // Disconnect abruptly.
            drop(ws);
        }

        // Small pause so the SDK has time to notice the disconnect.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connection 2: send 2 more events.
        {
            let mut ws = accept_one(&listener).await;
            let _ = ws.next().await; // consume subscribe (with resume_from)

            let _ = ws
                .send(text_msg(&serde_json::json!({"type":"subscribed","subscription_id":"s2","effective_filters":{}})))
                .await;

            for i in 1u32..=2 {
                let _ = ws
                    .send(text_msg(&serde_json::json!({
                        "type": "event",
                        "subscription_id": "s2",
                        "event": { "id": format!("conn2-ev-{i}"), "detectorId": "pump_dump", "confidence": 0.7 }
                    })))
                    .await;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = ws.close(None).await;
        }
    });

    let client = make_ws_client(&ws_url);
    let mut stream = client
        .subscribe_anomalies(AnomalyFilter::default())
        .await
        .expect("subscribe should succeed");

    let mut anomaly_count = 0;
    let mut reconnect_seen = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);

    while tokio::time::Instant::now() < deadline {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(3), stream.next()).await
        else {
            break;
        };

        match msg.expect("stream item should be Ok") {
            StreamMessage::Anomaly(_) => {
                anomaly_count += 1;
                if anomaly_count == 4 {
                    break;
                }
            }
            StreamMessage::Reconnected => reconnect_seen = true,
            _ => {}
        }
    }

    assert_eq!(anomaly_count, 4, "expected 4 total anomaly events (2 per connection)");
    assert!(reconnect_seen, "expected at least one Reconnected notification");
}
