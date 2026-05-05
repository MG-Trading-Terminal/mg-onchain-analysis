//! Minimal JSON-RPC 2.0 over WebSocket client — chain-agnostic.
//!
//! Implements the subset of JSON-RPC 2.0 needed by any chain adapter:
//!
//! - `request_raw(method, params)` — send a request, await the correlated response.
//! - `subscribe(method, params)` — send a subscribe request, return a stream of
//!   push notifications.  The subscription method is caller-supplied:
//!   - Ethereum: `"eth_subscribe"` (push method: `"eth_subscription"`)
//!   - Solana:   `"programSubscribe"` (push method: `"programNotification"`)
//!     and the other `*Subscribe` / `*Notification` pairs.
//!
//! # Architecture
//!
//! ```text
//! JsonRpcClient (Clone-able Arc wrapper)
//!   └─ JsonRpcInner (shared state)
//!         ├─ write_tx: mpsc::Sender<Message>  ─► WS sink task
//!         ├─ pending:  Mutex<HashMap<id, oneshot::Sender<Value>>>
//!         └─ subs:     Mutex<HashMap<sub_id_key, mpsc::Sender<Value>>>
//!
//! WS reader task (tokio::spawn)
//!   reads frames → dispatch:
//!     { id, result }  → oneshot reply
//!     { method: <any push method>, params: { subscription, result } }
//!                      → mpsc for that subscription id
//! ```
//!
//! One `tokio::spawn` per client runs the reader loop.  The writer sends frames
//! through a bounded mpsc channel.
//!
//! # Chain differences handled here
//!
//! The JSON-RPC 2.0 framing is identical across chains.  What differs:
//! - **Method names**: `eth_subscribe` vs `programSubscribe` vs `accountSubscribe`, etc.
//!   These are passed as `&str` parameters — the client does not hard-code any chain method.
//! - **Subscription ID type**: Ethereum returns a hex string (`"0x…"`); Solana returns a
//!   `u64` integer.  Both are represented as [`SubscriptionId`] and stringified for the
//!   internal subscription dispatch map.
//! - **Push notification method name**: Ethereum pushes `eth_subscription`; Solana pushes
//!   `programNotification`, `accountNotification`, etc.  The read pump dispatches on ANY
//!   frame that has `params.subscription` and no top-level `id`, regardless of method name.
//!
//! # Reference
//!
//! Dispatch architecture informed by `alloy_pubsub` / `alloy_rpc_client`
//! (MIT/Apache-2.0).  No code was copied; structure is independently reimplemented
//! from the JSON-RPC 2.0 spec (https://www.jsonrpc.org/specification).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use tracing::{debug, error, warn};

// ---------------------------------------------------------------------------
// SubscriptionId — chain-agnostic subscription identifier
// ---------------------------------------------------------------------------

/// A subscription identifier returned by a `*Subscribe` JSON-RPC call.
///
/// Different chains use different ID types:
/// - **Ethereum** (`eth_subscribe`): the node returns a hex string, e.g. `"0xabc123…"`.
/// - **Solana** (`programSubscribe`, `accountSubscribe`, etc.): the node returns a
///   `u64` integer, e.g. `12345`.
///
/// The `Display` impl produces a unique string key used internally to route push
/// notifications to the correct receiver channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubscriptionId {
    /// Ethereum-style hex string subscription ID (e.g. `"0xabc123"`).
    Hex(String),
    /// Solana-style numeric subscription ID (e.g. `12345`).
    Numeric(u64),
}

impl SubscriptionId {
    /// Return the string form used as the dispatch map key.
    ///
    /// - `Hex(s)` → `s` as-is (already a unique string)
    /// - `Numeric(n)` → decimal string representation of `n`
    pub fn as_dispatch_key(&self) -> String {
        match self {
            SubscriptionId::Hex(s) => s.clone(),
            SubscriptionId::Numeric(n) => n.to_string(),
        }
    }
}

impl std::fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriptionId::Hex(s) => write!(f, "{s}"),
            SubscriptionId::Numeric(n) => write!(f, "{n}"),
        }
    }
}

/// Parse a [`SubscriptionId`] from a `serde_json::Value`.
///
/// - A JSON string is treated as [`SubscriptionId::Hex`].
/// - A JSON unsigned integer is treated as [`SubscriptionId::Numeric`].
/// - Anything else returns `None`.
fn parse_subscription_id(v: &Value) -> Option<SubscriptionId> {
    if let Some(s) = v.as_str() {
        return Some(SubscriptionId::Hex(s.to_string()));
    }
    if let Some(n) = v.as_u64() {
        return Some(SubscriptionId::Numeric(n));
    }
    None
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request.
#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: &'a Value,
}

/// JSON-RPC 2.0 response frame (success, error, or push notification).
///
/// We deserialise every incoming frame into this struct and branch on
/// whether it carries a top-level `id` (call response) or only `method`
/// + `params.subscription` (push notification).
#[derive(Deserialize, Debug)]
struct RpcFrame {
    /// Present on call responses; absent on push notifications.
    #[serde(default)]
    id: Option<Value>,
    /// Present on successful call responses.
    result: Option<Value>,
    /// Present on JSON-RPC error responses.
    error: Option<Value>,
    /// Present on push notifications.  The value is chain-specific
    /// (e.g. `"eth_subscription"` for Ethereum, `"programNotification"` for Solana).
    method: Option<String>,
    /// Present on push notifications.  Contains `subscription` (the sub ID)
    /// and `result` (the notification payload).
    params: Option<Value>,
}

// ---------------------------------------------------------------------------
// JsonRpcInner — shared state
// ---------------------------------------------------------------------------

struct JsonRpcInner {
    /// Bounded channel to the WS write task.
    ///
    /// Capacity 256: enough for in-flight requests under burst; back-pressure
    /// beyond 256 is intentional (prevents unbounded queue in slow-RPC scenarios).
    write_tx: mpsc::Sender<Message>,
    /// Monotonically increasing request id.
    next_id: AtomicU64,
    /// Pending request reply channels, keyed by request id.
    ///
    /// `Mutex<HashMap<…>>` is acceptable here: lock is held only for
    /// insert/remove, never across `.await`.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// Active subscription channels, keyed by subscription id dispatch key.
    ///
    /// The dispatch key is the string produced by [`SubscriptionId::as_dispatch_key`]:
    /// - Ethereum: the raw hex string returned by `eth_subscribe`.
    /// - Solana: the decimal string of the `u64` returned by `*Subscribe`.
    subs: Mutex<HashMap<String, mpsc::Sender<Value>>>,
}

// ---------------------------------------------------------------------------
// JsonRpcClient — public API
// ---------------------------------------------------------------------------

/// Minimal JSON-RPC 2.0 over WebSocket client.
///
/// Chain-agnostic: method names, parameter shapes, and subscription ID types
/// are all caller-supplied.  This type knows nothing about Ethereum or Solana
/// beyond the JSON-RPC 2.0 wire format.
///
/// `Clone` is cheap — the inner state is reference-counted.
#[derive(Clone)]
pub struct JsonRpcClient {
    inner: Arc<JsonRpcInner>,
}

impl JsonRpcClient {
    /// Open a WebSocket connection and start the background reader loop.
    ///
    /// On success, the returned client is ready to accept `request_raw` and
    /// `subscribe` calls immediately.  The background task runs until the
    /// WS stream ends or all clones of the client are dropped.
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let url = ws_url.to_string();

        let (ws_stream, _response) = connect_async(&url)
            .await
            .map_err(|e| format!("WS connect failed: {e}"))?;

        let (write_half, read_half) = ws_stream.split();

        // Bounded channel for outgoing writes (capacity 256).
        let (write_tx, write_rx) = mpsc::channel::<Message>(256);

        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        // Spawn the write pump.
        tokio::spawn(write_pump(write_rx, write_half));

        // Spawn the read/dispatch loop.
        tokio::spawn(read_pump(read_half, Arc::clone(&inner)));

        debug!(%url, "JsonRpcClient connected");
        Ok(Self { inner })
    }

    /// Send a JSON-RPC request and wait for the correlated response.
    ///
    /// Returns the `result` field of the response deserialized as a raw
    /// [`Value`].  Returns an error string if the response contains a
    /// JSON-RPC `error` object, or if the transport is closed.
    ///
    /// This method is chain-agnostic: `method` can be any JSON-RPC method
    /// string (`"eth_blockNumber"`, `"getSlot"`, etc.).
    pub async fn request_raw(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Value, String> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel::<Result<Value, String>>();

        // Register before sending so there is no race.
        {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.insert(id, tx);
        }

        // Serialise and send.
        let req = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let text = serde_json::to_string(&req)
            .map_err(|e| format!("serialize request: {e}"))?;

        if self.inner.write_tx.send(Message::Text(text.into())).await.is_err() {
            // Channel closed — remove from pending.
            self.inner.pending.lock().unwrap().remove(&id);
            return Err("WS write channel closed".to_string());
        }

        // Wait for the response.
        match rx.await {
            Ok(result) => result,
            Err(_) => {
                // Sender dropped (read pump closed).
                Err("WS read pump closed before response".to_string())
            }
        }
    }

    /// Send a subscribe request and return a channel receiver for push notifications.
    ///
    /// `method` is the subscribe method name — caller-supplied and chain-specific:
    /// - Ethereum: `"eth_subscribe"` (with `params = ["newHeads"]`, `["logs", {...}]`, etc.)
    /// - Solana: `"programSubscribe"`, `"accountSubscribe"`, `"logsSubscribe"`,
    ///   `"signatureSubscribe"` (with the relevant program or account pubkey as params)
    ///
    /// Returns `(SubscriptionId, Receiver<Value>)` where:
    /// - [`SubscriptionId`] is the server-assigned subscription identifier.
    ///   - Ethereum: [`SubscriptionId::Hex`] (e.g. `"0xabc123…"`)
    ///   - Solana:   [`SubscriptionId::Numeric`] (e.g. `12345`)
    /// - Each [`Value`] received on the channel is the `result` field inside the
    ///   push notification's `params` object.
    ///
    /// The subscription channel capacity is 64; notifications are dropped
    /// (with a warning) if the consumer is too slow — identical to the
    /// bounded-channel contract used elsewhere in this codebase.
    pub async fn subscribe(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<(SubscriptionId, mpsc::Receiver<Value>), String> {
        // The subscribe call returns the subscription ID as the `result`.
        let sub_id_val = self.request_raw(method, params).await?;

        let sub_id = parse_subscription_id(&sub_id_val).ok_or_else(|| {
            format!("{method} returned unrecognised subscription id type: {sub_id_val}")
        })?;

        let dispatch_key = sub_id.as_dispatch_key();

        let (sub_tx, sub_rx) = mpsc::channel::<Value>(64);

        {
            let mut subs = self.inner.subs.lock().unwrap();
            subs.insert(dispatch_key.clone(), sub_tx);
        }

        debug!(%method, sub_id = %dispatch_key, "subscription registered");
        Ok((sub_id, sub_rx))
    }
}

// ---------------------------------------------------------------------------
// Background pump tasks
// ---------------------------------------------------------------------------

/// Write pump: forwards `Message` values from the mpsc channel to the WS sink.
///
/// Terminates when the channel is closed (all `JsonRpcClient` clones dropped,
/// or the `write_tx` in `JsonRpcInner` is dropped with the inner).
async fn write_pump<S>(
    mut rx: mpsc::Receiver<Message>,
    mut sink: S,
) where
    S: SinkExt<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    while let Some(msg) = rx.recv().await {
        if let Err(e) = sink.send(msg).await {
            error!(error = %e, "WS write error — closing write pump");
            break;
        }
    }
    // Flush on exit (best effort).
    let _ = sink.flush().await;
}

/// Read pump: receives WS frames and dispatches to pending requests or subscriptions.
///
/// This is the heart of the client.  It owns the read half of the WS stream and
/// loops until the stream ends or returns an error.
///
/// Dispatch rules:
/// 1. Frame has numeric `id` and `result` → deliver to pending oneshot.
/// 2. Frame has numeric `id` and `error`  → deliver error to pending oneshot.
/// 3. Frame has NO top-level `id` and has `params.subscription` (any value type) →
///    forward `params.result` to the mpsc for that subscription id.
///    This handles both:
///    - Ethereum push notifications (`method = "eth_subscription"`,
///      `params.subscription` is a hex string)
///    - Solana push notifications (`method = "programNotification"` etc.,
///      `params.subscription` is a `u64`)
/// 4. Anything else → warn and discard.
async fn read_pump<S>(mut stream: S, inner: Arc<JsonRpcInner>)
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg_result) = stream.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "WS read error — closing read pump");
                break;
            }
        };

        let text = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => {
                match String::from_utf8(b.into()) {
                    Ok(s) => s.into(),
                    Err(_) => {
                        warn!("WS: received non-UTF8 binary frame, skipping");
                        continue;
                    }
                }
            }
            // Ping/Pong are handled automatically by tungstenite.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                debug!("WS: received Close frame");
                break;
            }
            Message::Frame(_) => continue,
        };

        let frame: RpcFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, raw = %text, "WS: failed to parse JSON-RPC frame");
                continue;
            }
        };

        // -----------------------------------------------------------------
        // Dispatch: push notification
        //
        // A push notification has no top-level `id` and carries
        // `params.subscription` (either a string or a u64).
        //
        // We dispatch on the subscription ID regardless of the `method` field
        // value, making the read pump chain-agnostic.  Examples:
        //   Ethereum: method="eth_subscription", params.subscription="0xabc…"
        //   Solana:   method="programNotification", params.subscription=12345
        // -----------------------------------------------------------------
        let is_response = matches!(&frame.id, Some(Value::Number(_)));

        if !is_response
            && let Some(params) = &frame.params
        {
            let sub_id_raw = params.get("subscription");
            let result = params.get("result").cloned();

            if let (Some(sub_id_val), Some(result)) = (sub_id_raw, result)
                && let Some(sub_id) = parse_subscription_id(sub_id_val)
            {
                let dispatch_key = sub_id.as_dispatch_key();
                let sender = inner.subs.lock().unwrap().get(&dispatch_key).cloned();
                if let Some(tx) = sender {
                    if tx.try_send(result).is_err() {
                        warn!(
                            sub_id = %dispatch_key,
                            method = ?frame.method,
                            "WS subscription channel full or closed — dropping notification"
                        );
                    }
                } else {
                    debug!(
                        sub_id = %dispatch_key,
                        method = ?frame.method,
                        "WS: received notification for unknown sub id"
                    );
                }
                continue;
            }
        }

        // -----------------------------------------------------------------
        // Dispatch: call response
        // -----------------------------------------------------------------
        let id_u64 = match &frame.id {
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        };

        if let Some(id) = id_u64 {
            let reply_tx = inner.pending.lock().unwrap().remove(&id);
            if let Some(tx) = reply_tx {
                let payload = if let Some(err) = frame.error {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("JSON-RPC error")
                        .to_string();
                    Err(msg)
                } else {
                    Ok(frame.result.unwrap_or(Value::Null))
                };
                // Ignore send errors (caller timed out / dropped).
                let _ = tx.send(payload);
            } else {
                warn!(id, "WS: response for unknown request id");
            }
        } else {
            debug!(method = ?frame.method, "WS: unhandled frame (no id, no subscription)");
        }
    }

    // Pump exited — flush all pending requests with an error so callers unblock.
    let mut pending = inner.pending.lock().unwrap();
    for (_id, tx) in pending.drain() {
        let _ = tx.send(Err("WS connection closed".to_string()));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc as tokio_mpsc;

    /// Verify that the read pump correctly dispatches a call response to a
    /// pending oneshot receiver.
    #[tokio::test]
    async fn read_pump_dispatches_call_response() {
        use futures_util::stream;

        let raw_response = r#"{"jsonrpc":"2.0","id":1,"result":"0x14c4a40"}"#;
        let messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Text(raw_response.into()))];

        let (write_tx, _write_rx) = tokio_mpsc::channel(4);
        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(2),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        // Register a pending oneshot for id=1 before dispatching.
        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        inner.pending.lock().unwrap().insert(1, tx);

        let mock_stream = stream::iter(messages);
        read_pump(mock_stream, Arc::clone(&inner)).await;

        let result = rx.await.expect("oneshot must receive");
        assert_eq!(
            result.unwrap(),
            Value::String("0x14c4a40".to_string()),
            "response result must match"
        );
    }

    /// Verify that the read pump dispatches an Ethereum-style push notification
    /// (method="eth_subscription", string subscription ID) to the registered mpsc channel.
    #[tokio::test]
    async fn read_pump_dispatches_ethereum_subscription_notification() {
        use futures_util::stream;

        let raw_notification = r#"{
            "jsonrpc":"2.0",
            "method":"eth_subscription",
            "params":{
                "subscription":"0xabc123",
                "result":{"number":"0x1","hash":"0xdeadbeef"}
            }
        }"#;

        let messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Text(raw_notification.into()))];

        let (write_tx, _write_rx) = tokio_mpsc::channel(4);
        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        // Register using the Hex variant's dispatch key ("0xabc123").
        let sub_id = SubscriptionId::Hex("0xabc123".to_string());
        let (sub_tx, mut sub_rx) = tokio_mpsc::channel::<Value>(4);
        inner.subs.lock().unwrap().insert(sub_id.as_dispatch_key(), sub_tx);

        let mock_stream = stream::iter(messages);
        read_pump(mock_stream, Arc::clone(&inner)).await;

        let notification = sub_rx.recv().await.expect("subscription must receive");
        assert_eq!(
            notification["number"],
            Value::String("0x1".to_string()),
            "notification result must contain block number"
        );
    }

    /// Verify that the read pump dispatches a Solana-style push notification
    /// (method="programNotification", numeric subscription ID) to the registered mpsc channel.
    #[tokio::test]
    async fn read_pump_dispatches_solana_subscription_notification() {
        use futures_util::stream;

        let raw_notification = r#"{
            "jsonrpc":"2.0",
            "method":"programNotification",
            "params":{
                "subscription":12345,
                "result":{"context":{"slot":123456789},"value":{"pubkey":"ABC","account":{"data":"base64data","lamports":1000000}}}
            }
        }"#;

        let messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Text(raw_notification.into()))];

        let (write_tx, _write_rx) = tokio_mpsc::channel(4);
        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        // Register using the Numeric variant's dispatch key ("12345").
        let sub_id = SubscriptionId::Numeric(12345);
        let (sub_tx, mut sub_rx) = tokio_mpsc::channel::<Value>(4);
        inner.subs.lock().unwrap().insert(sub_id.as_dispatch_key(), sub_tx);

        let mock_stream = stream::iter(messages);
        read_pump(mock_stream, Arc::clone(&inner)).await;

        let notification = sub_rx.recv().await.expect("subscription must receive");
        assert!(
            notification["context"]["slot"].as_u64().is_some(),
            "Solana notification result must contain slot in context"
        );
    }

    /// Verify that the read pump delivers an error to the pending oneshot
    /// when the JSON-RPC response contains an `error` field.
    #[tokio::test]
    async fn read_pump_dispatches_error_response() {
        use futures_util::stream;

        let raw_error = r#"{
            "jsonrpc":"2.0",
            "id":7,
            "error":{"code":-32000,"message":"execution reverted"}
        }"#;

        let messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> =
            vec![Ok(Message::Text(raw_error.into()))];

        let (write_tx, _write_rx) = tokio_mpsc::channel(4);
        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(8),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        inner.pending.lock().unwrap().insert(7, tx);

        let mock_stream = stream::iter(messages);
        read_pump(mock_stream, Arc::clone(&inner)).await;

        let result = rx.await.expect("oneshot must receive");
        let err_msg = result.unwrap_err();
        assert_eq!(err_msg, "execution reverted");
    }

    /// Verify that pending oneshots receive a connection-closed error when the
    /// read pump exits with outstanding requests.
    #[tokio::test]
    async fn read_pump_flushes_pending_on_close() {
        use futures_util::stream;

        // Empty stream — simulates immediate close.
        let messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>> = vec![];

        let (write_tx, _write_rx) = tokio_mpsc::channel(4);
        let inner = Arc::new(JsonRpcInner {
            write_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });

        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        inner.pending.lock().unwrap().insert(42, tx);

        let mock_stream = stream::iter(messages);
        read_pump(mock_stream, Arc::clone(&inner)).await;

        let result = rx.await.expect("oneshot must receive flush error");
        assert!(result.is_err(), "closed connection must produce an error");
    }

    // -----------------------------------------------------------------------
    // SubscriptionId tests
    // -----------------------------------------------------------------------

    #[test]
    fn subscription_id_hex_dispatch_key() {
        let id = SubscriptionId::Hex("0xabc123def".to_string());
        assert_eq!(id.as_dispatch_key(), "0xabc123def");
        assert_eq!(id.to_string(), "0xabc123def");
    }

    #[test]
    fn subscription_id_numeric_dispatch_key() {
        let id = SubscriptionId::Numeric(99_999);
        assert_eq!(id.as_dispatch_key(), "99999");
        assert_eq!(id.to_string(), "99999");
    }

    #[test]
    fn parse_subscription_id_from_string() {
        let v = Value::String("0xdeadbeef".to_string());
        let id = parse_subscription_id(&v).unwrap();
        assert_eq!(id, SubscriptionId::Hex("0xdeadbeef".to_string()));
    }

    #[test]
    fn parse_subscription_id_from_u64() {
        let v = Value::Number(serde_json::Number::from(42_u64));
        let id = parse_subscription_id(&v).unwrap();
        assert_eq!(id, SubscriptionId::Numeric(42));
    }

    #[test]
    fn parse_subscription_id_from_null_returns_none() {
        assert!(parse_subscription_id(&Value::Null).is_none());
    }
}
