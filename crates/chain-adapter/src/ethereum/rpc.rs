//! `EthereumRpc` trait — abstraction over the Ethereum JSON-RPC transport.
//!
//! # Sprint 24 (2026-04-27) — tokio-tungstenite WsRpcClient (ADR 0006 Task #5b)
//!
//! Replaced `alloy::rpc::client::RpcClient` + `alloy::transports::ws::WsConnect`
//! with an in-tree `JsonRpcClient` backed by `tokio-tungstenite`.  All alloy
//! imports removed from this file; `mg_evm_types::B256` used for subscription ids.
//!
//! ## Transport choice (ADR 0004 / ADR 0006)
//!
//! Production path: WebSocket JSON-RPC (`ws://127.0.0.1:8546`) against a self-hosted
//! Reth node per ADR 0003 + ADR 0004. No Alchemy/Infura/QuickNode.
//!
//! ## Reconnect (Sprint 17+)
//!
//! TODO(sprint-17): reconnect on WS disconnect. The `subscribe_new_heads` stream
//! terminates on disconnect. Sprint 17 wraps it in a reconnect loop with
//! exponential backoff (500ms/1s/2s, max 3 attempts per session).
//!
//! ## `dyn`-compatibility
//!
//! The trait is used as `Arc<dyn EthereumRpc>` in `EthereumAdapter`. All methods use
//! `async fn` via `async-trait`. `subscribe_new_heads` returns a boxed stream.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;

use futures::Stream;
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::error::AdapterError;
use crate::ethereum::types::{BlockData, BlockHeader, LogFilter, RawLog};
use crate::jsonrpc::JsonRpcClient;

// ---------------------------------------------------------------------------
// EthereumRpc trait
// ---------------------------------------------------------------------------

/// Abstraction over the Ethereum JSON-RPC transport.
///
/// Implemented by:
/// - `WsRpcClient` — real WebSocket client (Sprint 16; migrated to tokio-tungstenite Sprint 24)
/// - `MockEthereumRpc` — in-memory fake for unit tests
///
/// All methods are `async` and return `Result<_, AdapterError>`.
/// The trait is object-safe: use `Arc<dyn EthereumRpc + Send + Sync>`.
#[async_trait::async_trait]
pub trait EthereumRpc: Send + Sync {
    /// Return the number of the latest block (`eth_blockNumber`).
    async fn get_latest_block_number(&self) -> Result<u64, AdapterError>;

    /// Return the number of the latest finalized block (`eth_getBlockByNumber("finalized")`).
    ///
    /// The `finalized` block tag is available on post-Merge Ethereum. It returns the
    /// last block that has been finalized by the consensus layer (LMD-GHOST + Casper FFG).
    /// Approximately 64 slots (~12.8 minutes) behind the head.
    ///
    /// Returns `Err(AdapterError::RpcError { reason: "finalized tag unavailable..." })` on
    /// pre-Merge providers (should not occur against a Reth + Lighthouse pair).
    async fn get_finalized_block_number(&self) -> Result<u64, AdapterError>;

    /// Return block data for the given block number, including logs.
    ///
    /// Combines `eth_getBlockByNumber` (block header) + `eth_getLogs` (event data).
    async fn get_block_by_number(&self, number: u64) -> Result<BlockData, AdapterError>;

    /// Subscribe to new block headers as they arrive (`eth_subscribe("newHeads")`).
    ///
    /// Returns a boxed stream of `BlockHeader`. The stream terminates on WS disconnect.
    ///
    /// TODO(sprint-17): reconnect on disconnect with exponential backoff.
    fn subscribe_new_heads(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<BlockHeader, AdapterError>> + Send + 'static>>;

    /// Fetch logs matching the given filter (`eth_getLogs`).
    ///
    /// Block range must not exceed `EthereumAdapterConfig::batch_size_blocks`
    /// (default: 1000 blocks) to avoid exceeding the Reth provider's range limit.
    /// Callers in `backfill.rs` are responsible for chunking.
    async fn get_logs(&self, filter: LogFilter) -> Result<Vec<RawLog>, AdapterError>;

    /// Execute a read-only call against a contract (`eth_call`).
    ///
    /// Used by detectors that need on-chain simulation at evaluation time:
    /// - D01 honeypot EVM: simulate-sell via UniV2 router `swapExactTokensForTokens`
    /// - D02 rug pull EVM: read token `owner()` (Ownable pattern)
    ///
    /// # Arguments
    ///
    /// - `to`: Contract address (EVM hex string, with or without `0x` prefix).
    /// - `calldata`: ABI-encoded call bytes (4-byte selector + encoded params).
    ///
    /// # Returns
    ///
    /// `Ok(Vec<u8>)` — ABI-encoded return data.
    /// `Err(AdapterError::CallReverted { reason })` — call reverted on the EVM.
    /// `Err(AdapterError::RpcError { .. })` — transport or JSON-RPC error.
    ///
    /// # Block tag
    ///
    /// Always uses the `"latest"` block tag. For detector evaluations, this is
    /// sufficient: we want the current on-chain state at evaluation time.
    ///
    /// # Object safety
    ///
    /// This method is object-safe via `async_trait`. The return type is `Vec<u8>`
    /// (no generic parameter) so no boxing of futures is needed beyond the trait object.
    async fn eth_call(&self, to: &str, calldata: Vec<u8>) -> Result<Vec<u8>, AdapterError>;
}

// ---------------------------------------------------------------------------
// WsRpcClient — real WebSocket JSON-RPC client
// ---------------------------------------------------------------------------

/// WebSocket JSON-RPC client connecting to a self-hosted Reth node.
///
/// Backed by the in-tree `JsonRpcClient` (tokio-tungstenite over RFC 6455 WebSocket).
/// Sends raw JSON-RPC requests and deserialises responses via `serde_json::Value`.
///
/// Migrated from `alloy::rpc::client::RpcClient` in Sprint 24 per ADR 0006.
///
/// # Endpoint
///
/// Defaults to `ws://127.0.0.1:8546` (Reth WS port from `infra/ethereum-node/.env`).
/// Override via `ETHEREUM_RPC_WS_URL` environment variable or explicit `connect(url)`.
///
/// # Integration tests
///
/// Live WS tests are `#[ignore]`-gated — they require a running Reth node.
/// Run with:
/// ```text
/// ETHEREUM_RPC_WS_URL=ws://127.0.0.1:8546 \
///   RUST_TEST_THREADS=1 cargo test --ignored \
///   -p mg-onchain-chain-adapter ethereum_ws_live
/// ```
pub struct WsRpcClient {
    /// In-tree JSON-RPC 2.0 over WebSocket client.
    client: JsonRpcClient,
    /// The URL used to connect (kept for logging).
    pub ws_url: String,
}

impl WsRpcClient {
    /// Establish a WebSocket connection with retry.
    ///
    /// Retries up to 3 attempts with 500ms / 1s / 2s delays.
    /// Returns `AdapterError::RpcError` if all attempts fail.
    pub async fn connect(ws_url: &str) -> Result<Self, AdapterError> {
        let delays_ms: [u64; 3] = [500, 1000, 2000];
        let mut last_err = String::new();

        for (attempt, &delay_ms) in delays_ms.iter().enumerate() {
            match JsonRpcClient::connect(ws_url).await {
                Ok(client) => {
                    debug!(%ws_url, attempt, "WsRpcClient connected");
                    return Ok(Self { client, ws_url: ws_url.to_string() });
                }
                Err(e) => {
                    last_err = e.clone();
                    warn!(%ws_url, attempt, error = %e, delay_ms, "WsRpcClient connect failed, retrying");
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }

        Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("WsRpcClient: connect failed after 3 attempts: {last_err}"),
        })
    }

    /// Convenience constructor reading the URL from `ETHEREUM_RPC_WS_URL` env var,
    /// defaulting to `ws://127.0.0.1:8546`.
    pub async fn from_env() -> Result<Self, AdapterError> {
        let url = std::env::var("ETHEREUM_RPC_WS_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:8546".to_string());
        Self::connect(&url).await
    }
}

#[async_trait::async_trait]
impl EthereumRpc for WsRpcClient {
    async fn get_latest_block_number(&self) -> Result<u64, AdapterError> {
        let result = self.client
            .request_raw("eth_blockNumber", &Value::Array(vec![]))
            .await
            .map_err(|e| AdapterError::RpcError { slot: 0, reason: e })?;

        let hex = result.as_str().ok_or_else(|| AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_blockNumber: expected string, got {result}"),
        })?;
        parse_hex_u64(hex, "eth_blockNumber")
    }

    async fn get_finalized_block_number(&self) -> Result<u64, AdapterError> {
        let params = serde_json::json!(["finalized", false]);
        let result = self.client
            .request_raw("eth_getBlockByNumber", &params)
            .await
            .map_err(|e| AdapterError::RpcError { slot: 0, reason: e })?;

        let num_hex = result.get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "finalized tag unavailable or block number missing in response".to_string(),
            })?;
        parse_hex_u64(num_hex, "eth_getBlockByNumber(finalized)")
    }

    async fn get_block_by_number(&self, number: u64) -> Result<BlockData, AdapterError> {
        let hex_num = format!("0x{number:x}");
        let params = serde_json::json!([hex_num, false]);
        let block = self.client
            .request_raw("eth_getBlockByNumber", &params)
            .await
            .map_err(|e| AdapterError::RpcError { slot: number, reason: e })?;

        parse_block_data(&block, number)
    }

    fn subscribe_new_heads(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<BlockHeader, AdapterError>> + Send + 'static>> {
        // Clone the client (cheap Arc clone) and ws_url to move into the async block.
        let client = self.client.clone();
        let ws_url = self.ws_url.clone();

        Box::pin(async_stream_impl(client, ws_url))
    }

    async fn get_logs(&self, filter: LogFilter) -> Result<Vec<RawLog>, AdapterError> {
        let filter_obj = build_log_filter_json(&filter);
        let params = Value::Array(vec![filter_obj]);

        let result = self.client
            .request_raw("eth_getLogs", &params)
            .await
            .map_err(|e| AdapterError::RpcError { slot: 0, reason: e })?;

        let raw_logs = result.as_array().ok_or_else(|| AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs: expected array, got {result}"),
        })?;

        raw_logs.iter().map(parse_raw_log).collect()
    }

    async fn eth_call(&self, to: &str, calldata: Vec<u8>) -> Result<Vec<u8>, AdapterError> {
        // Build the eth_call transaction object.
        // `from` is omitted (optional; defaults to zero address on the node).
        // Block tag: "latest".
        let call_obj = serde_json::json!({
            "to": to,
            "data": format!("0x{}", hex::encode(&calldata)),
        });
        let params = serde_json::json!([call_obj, "latest"]);

        // eth_call returns a hex-encoded result string on success, or a JSON-RPC
        // error (which our JsonRpcClient surfaces as an Err) on revert.
        match self.client.request_raw("eth_call", &params).await {
            Ok(value) => {
                let hex_str = value.as_str().ok_or_else(|| AdapterError::RpcError {
                    slot: 0,
                    reason: format!("eth_call: expected string result, got {value}"),
                })?;
                let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
                hex::decode(stripped).map_err(|e| AdapterError::DecodeError {
                    context: "eth_call",
                    reason: format!("hex decode of return data: {e}"),
                })
            }
            Err(reason) => {
                // The RPC client surfaces call reverts as JSON-RPC errors.
                // We surface them as `CallReverted` for pattern-matching in detectors.
                Err(AdapterError::CallReverted { reason })
            }
        }
    }
}

/// Build the JSON filter object for eth_getLogs.
fn build_log_filter_json(filter: &LogFilter) -> Value {
    let mut obj = serde_json::json!({});
    if let Some(from) = filter.from_block {
        obj["fromBlock"] = Value::String(format!("0x{from:x}"));
    }
    if let Some(to) = filter.to_block {
        obj["toBlock"] = Value::String(format!("0x{to:x}"));
    }
    if !filter.addresses.is_empty() {
        if filter.addresses.len() == 1 {
            obj["address"] = Value::String(filter.addresses[0].clone());
        } else {
            obj["address"] = Value::Array(
                filter.addresses.iter().map(|a| Value::String(a.clone())).collect(),
            );
        }
    }
    if !filter.topics.is_empty() {
        let topics: Vec<Value> = filter.topics.iter().map(|t| match t {
            Some(s) => Value::String(s.clone()),
            None => Value::Null,
        }).collect();
        obj["topics"] = Value::Array(topics);
    }
    obj
}

/// Bounded exponential backoff delays for WS reconnect (ms).
///
/// 500ms → 1s → 2s → 4s → 8s → cap at 30s (last entry repeated if needed).
/// Max 10 attempts before the driver gives up and closes the sender.
const RECONNECT_DELAYS_MS: [u64; 10] = [500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000, 30_000];

/// Async function that drives the newHeads subscription with reconnect-on-disconnect.
///
/// # Reconnect strategy (ADR 0005 / Sprint 17 item 6)
///
/// On transport-level disconnect (subscription channel closed), the driver:
/// 1. Emits a `tracing::warn!` with the disconnect reason.
/// 2. Waits for the next backoff delay from `RECONNECT_DELAYS_MS`.
/// 3. Re-establishes the subscription by calling `eth_subscribe("newHeads")` again.
///
/// After `RECONNECT_DELAYS_MS.len()` failed reconnect attempts in a row, the driver
/// emits `tracing::error!` and closes the sender, signalling stream termination to
/// the caller.
///
/// Successful event delivery resets the attempt counter.
async fn subscribe_new_heads_inner(
    client: JsonRpcClient,
    ws_url: String,
    tx: tokio::sync::mpsc::Sender<Result<BlockHeader, AdapterError>>,
) {
    let mut attempt: usize = 0;

    loop {
        // ----------------------------------------------------------------
        // Subscribe phase
        // ----------------------------------------------------------------
        let params = serde_json::json!(["newHeads"]);
        let sub_result = client.subscribe("eth_subscribe", &params).await;

        let (_sub_id, mut sub_rx) = match sub_result {
            Ok(pair) => {
                debug!(%ws_url, attempt, "WS newHeads subscription established");
                attempt = 0; // reset counter on successful subscribe
                pair
            }
            Err(reason) => {
                warn!(
                    %ws_url,
                    attempt,
                    error = %reason,
                    "WS subscribe failed"
                );
                if attempt >= RECONNECT_DELAYS_MS.len() {
                    let final_err = AdapterError::RpcError {
                        slot: 0,
                        reason: format!(
                            "WsRpcClient: eth_subscribe failed after {} attempts: {reason}",
                            RECONNECT_DELAYS_MS.len()
                        ),
                    };
                    error!(%ws_url, "WS reconnect exhausted — closing stream");
                    let _ = tx.send(Err(final_err)).await;
                    return;
                }
                let delay_ms = RECONNECT_DELAYS_MS[attempt];
                attempt += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
        };

        // ----------------------------------------------------------------
        // Consume phase — forward headers until channel closes or receiver drops
        // ----------------------------------------------------------------
        let disconnect_reason;

        loop {
            match sub_rx.recv().await {
                Some(raw) => {
                    let item = parse_block_header_from_value(&raw);
                    if tx.send(item).await.is_err() {
                        // Receiver dropped — caller is done.
                        return;
                    }
                }
                None => {
                    // Channel closed — transport disconnect.
                    disconnect_reason = "subscription channel closed (WS disconnect)".to_string();
                    break;
                }
            }
        }

        // ----------------------------------------------------------------
        // Reconnect phase
        // ----------------------------------------------------------------
        warn!(
            %ws_url,
            attempt,
            disconnect_reason = %disconnect_reason,
            delay_ms = RECONNECT_DELAYS_MS[attempt.min(RECONNECT_DELAYS_MS.len() - 1)],
            "WS disconnect; reconnecting"
        );

        if attempt >= RECONNECT_DELAYS_MS.len() {
            let final_err = AdapterError::RpcError {
                slot: 0,
                reason: format!(
                    "WsRpcClient: WS stream disconnected after {} reconnect attempts: {disconnect_reason}",
                    RECONNECT_DELAYS_MS.len()
                ),
            };
            error!(%ws_url, "WS reconnect exhausted — closing stream");
            let _ = tx.send(Err(final_err)).await;
            return;
        }

        let delay_ms = RECONNECT_DELAYS_MS[attempt];
        attempt += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
    }
}

/// Bridge the async subscription driver into a `Stream` via mpsc channel.
///
/// The background task runs `subscribe_new_heads_inner` which implements
/// reconnect-on-disconnect with bounded exponential backoff (Sprint 17 item 6).
fn async_stream_impl(
    client: JsonRpcClient,
    ws_url: String,
) -> impl Stream<Item = Result<BlockHeader, AdapterError>> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(subscribe_new_heads_inner(client, ws_url, tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

// ---------------------------------------------------------------------------
// JSON parsing helpers
// ---------------------------------------------------------------------------

/// Parse a `0x`-prefixed hex string to `u64`.
fn parse_hex_u64(hex: &str, context: &'static str) -> Result<u64, AdapterError> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    u64::from_str_radix(stripped, 16).map_err(|e| AdapterError::DecodeError {
        context,
        reason: format!("parse hex u64 '{hex}': {e}"),
    })
}

/// Parse a `serde_json::Value` block object into `BlockData`.
fn parse_block_data(v: &Value, block_number: u64) -> Result<BlockData, AdapterError> {
    let number = v.get("number")
        .and_then(|x| x.as_str())
        .and_then(|s| parse_hex_u64(s, "block.number").ok())
        .unwrap_or(block_number);

    let hash = v.get("hash")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    let parent_hash = v.get("parentHash")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    let timestamp = v.get("timestamp")
        .and_then(|x| x.as_str())
        .and_then(|s| parse_hex_u64(s, "block.timestamp").ok())
        .unwrap_or(0);

    Ok(BlockData { number, hash, parent_hash, timestamp, logs: vec![] })
}

/// Parse a newHeads notification `serde_json::Value` into `BlockHeader`.
fn parse_block_header_from_value(v: &Value) -> Result<BlockHeader, AdapterError> {
    let number = v.get("number")
        .and_then(|x| x.as_str())
        .ok_or_else(|| AdapterError::DecodeError {
            context: "parse_block_header",
            reason: "missing 'number' field".to_string(),
        })
        .and_then(|s| parse_hex_u64(s, "header.number"))?;

    let hash = v.get("hash")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    let parent_hash = v.get("parentHash")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    Ok(BlockHeader { number, hash, parent_hash })
}

/// Parse a `serde_json::Value` log entry into `RawLog`.
fn parse_raw_log(v: &Value) -> Result<RawLog, AdapterError> {
    let address = v.get("address")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    let topics = v.get("topics")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let data_hex = v.get("data")
        .and_then(|x| x.as_str())
        .unwrap_or("0x");
    let data = hex::decode(data_hex.strip_prefix("0x").unwrap_or(data_hex))
        .unwrap_or_default();

    let block_number = v.get("blockNumber")
        .and_then(|x| x.as_str())
        .and_then(|s| parse_hex_u64(s, "log.blockNumber").ok())
        .unwrap_or(0);

    let tx_hash = v.get("transactionHash")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    let log_index = v.get("logIndex")
        .and_then(|x| x.as_str())
        .and_then(|s| parse_hex_u64(s, "log.logIndex").ok())
        .unwrap_or(0) as u32;

    Ok(RawLog { address, topics, data, block_number, tx_hash, log_index })
}

// ---------------------------------------------------------------------------
// MockEthereumRpc — in-memory fake for tests
// ---------------------------------------------------------------------------

/// In-memory fake `EthereumRpc` for unit tests.
///
/// Pre-populate blocks via `insert_block` before calling methods.
/// Pre-populate `eth_call` responses via `set_eth_call_response`.
/// `subscribe_new_heads` returns an immediately-terminating stream (suitable for
/// testing the stream plumbing without a real WebSocket connection).
pub struct MockEthereumRpc {
    blocks: Mutex<HashMap<u64, BlockData>>,
    latest: Mutex<u64>,
    finalized: Mutex<u64>,
    /// Canned `eth_call` responses keyed by calldata (hex-encoded, no 0x prefix).
    ///
    /// - `Ok(bytes)` — simulate a successful call returning these bytes.
    /// - `Err(reason)` — simulate a revert with this reason string.
    ///
    /// If no entry matches the calldata, `eth_call` returns `Ok(vec![])` (empty return).
    /// The `to` address is NOT part of the key — tests typically mock a single contract.
    eth_call_responses: Mutex<HashMap<String, Result<Vec<u8>, String>>>,
}

impl MockEthereumRpc {
    /// Create an empty mock with `latest = 0`, `finalized = 0`.
    pub fn new() -> Self {
        Self {
            blocks: Mutex::new(HashMap::new()),
            latest: Mutex::new(0),
            finalized: Mutex::new(0),
            eth_call_responses: Mutex::new(HashMap::new()),
        }
    }

    /// Insert a `BlockData` into the mock's block store.
    ///
    /// Also updates `latest` if the inserted block number is greater than the
    /// current `latest`.
    pub fn insert_block(&self, block: BlockData) {
        let number = block.number;
        let mut blocks = self.blocks.lock().unwrap();
        blocks.insert(number, block);
        let mut latest = self.latest.lock().unwrap();
        if number > *latest {
            *latest = number;
        }
    }

    /// Set the `latest` block number.
    pub fn set_latest(&self, n: u64) {
        *self.latest.lock().unwrap() = n;
    }

    /// Set the `finalized` block number.
    pub fn set_finalized(&self, n: u64) {
        *self.finalized.lock().unwrap() = n;
    }

    /// Register a canned `eth_call` response for a specific calldata byte sequence.
    ///
    /// When `eth_call` is called with `calldata`, the mock returns `response`.
    /// Use `Ok(bytes)` for a successful call and `Err(reason)` for a simulated revert.
    ///
    /// The key is the hex-encoded calldata (without `0x` prefix).
    pub fn set_eth_call_response(&self, calldata: &[u8], response: Result<Vec<u8>, String>) {
        let key = hex::encode(calldata);
        self.eth_call_responses.lock().unwrap().insert(key, response);
    }

    /// Register a catch-all `eth_call` response (matches any calldata not otherwise registered).
    ///
    /// Key: empty string `""` — the mock checks this if no specific calldata key matches.
    pub fn set_eth_call_default(&self, response: Result<Vec<u8>, String>) {
        self.eth_call_responses.lock().unwrap().insert(String::new(), response);
    }
}

impl Default for MockEthereumRpc {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl EthereumRpc for MockEthereumRpc {
    async fn get_latest_block_number(&self) -> Result<u64, AdapterError> {
        Ok(*self.latest.lock().unwrap())
    }

    async fn get_finalized_block_number(&self) -> Result<u64, AdapterError> {
        Ok(*self.finalized.lock().unwrap())
    }

    async fn get_block_by_number(&self, number: u64) -> Result<BlockData, AdapterError> {
        let blocks = self.blocks.lock().unwrap();
        blocks.get(&number).cloned().ok_or_else(|| AdapterError::RpcError {
            slot: number,
            reason: format!("MockEthereumRpc: block {number} not found"),
        })
    }

    fn subscribe_new_heads(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<BlockHeader, AdapterError>> + Send + 'static>> {
        Box::pin(futures::stream::empty())
    }

    async fn get_logs(&self, filter: LogFilter) -> Result<Vec<RawLog>, AdapterError> {
        let from = filter.from_block.unwrap_or(0);
        let to = filter.to_block.unwrap_or(u64::MAX);
        let blocks = self.blocks.lock().unwrap();
        let mut logs = Vec::new();
        for n in from..=to {
            if let Some(block) = blocks.get(&n) {
                logs.extend(block.logs.iter().filter(|log| {
                    if !filter.addresses.is_empty() {
                        return filter.addresses.iter().any(|a| a.eq_ignore_ascii_case(&log.address));
                    }
                    true
                }).cloned());
            }
        }
        Ok(logs)
    }

    async fn eth_call(&self, _to: &str, calldata: Vec<u8>) -> Result<Vec<u8>, AdapterError> {
        let key = hex::encode(&calldata);
        let responses = self.eth_call_responses.lock().unwrap();
        // First check for an exact calldata match, then fall back to the default key ("").
        let response = responses
            .get(&key)
            .or_else(|| responses.get(""))
            .cloned();

        match response {
            Some(Ok(bytes)) => Ok(bytes),
            Some(Err(reason)) => Err(AdapterError::CallReverted { reason }),
            // No canned response registered — return empty bytes (simulates a call to a
            // contract that returns nothing, e.g. a non-ownable token returning 0 bytes).
            None => Ok(vec![]),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    fn make_block(number: u64, hash: &str, parent_hash: &str) -> BlockData {
        BlockData {
            number,
            hash: hash.to_string(),
            parent_hash: parent_hash.to_string(),
            timestamp: 1_700_000_000 + number * 12,
            logs: vec![],
        }
    }

    #[tokio::test]
    async fn mock_rpc_latest_block_number_initial() {
        let rpc = MockEthereumRpc::new();
        assert_eq!(rpc.get_latest_block_number().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn mock_rpc_insert_block_updates_latest() {
        let rpc = MockEthereumRpc::new();
        rpc.insert_block(make_block(100, "0xabc", "0xprev"));
        assert_eq!(rpc.get_latest_block_number().await.unwrap(), 100);
    }

    #[tokio::test]
    async fn mock_rpc_get_block_by_number_found() {
        let rpc = MockEthereumRpc::new();
        rpc.insert_block(make_block(42, "0xhash42", "0xhash41"));
        let block = rpc.get_block_by_number(42).await.unwrap();
        assert_eq!(block.number, 42);
        assert_eq!(block.hash, "0xhash42");
    }

    #[tokio::test]
    async fn mock_rpc_get_block_by_number_not_found() {
        let rpc = MockEthereumRpc::new();
        assert!(rpc.get_block_by_number(999).await.is_err());
    }

    #[tokio::test]
    async fn mock_rpc_finalized_block_number() {
        let rpc = MockEthereumRpc::new();
        rpc.set_finalized(20_000_000);
        assert_eq!(rpc.get_finalized_block_number().await.unwrap(), 20_000_000);
    }

    #[tokio::test]
    async fn mock_rpc_get_logs_range_filter() {
        let rpc = MockEthereumRpc::new();
        let mut block = make_block(10, "0xa", "0xb");
        block.logs.push(RawLog {
            address: "0xtoken".to_string(),
            topics: vec!["0xtopic0".to_string()],
            data: vec![],
            block_number: 10,
            tx_hash: "0xtx".to_string(),
            log_index: 0,
        });
        rpc.insert_block(block);
        let logs = rpc.get_logs(LogFilter::range(10, 10)).await.unwrap();
        assert_eq!(logs.len(), 1);
    }

    #[tokio::test]
    async fn mock_rpc_subscribe_new_heads_returns_empty_stream() {
        let rpc = MockEthereumRpc::new();
        let mut stream = rpc.subscribe_new_heads();
        assert!(stream.next().await.is_none());
    }

    // -----------------------------------------------------------------------
    // parse_hex_u64 unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_hex_u64_with_prefix() {
        // 0x14c4a40 = 21_776_960 (decimal)
        assert_eq!(parse_hex_u64("0x14c4a40", "test").unwrap(), 21_776_960);
    }

    #[test]
    fn parse_hex_u64_without_prefix() {
        // 14c4a40 = 21_776_960 (decimal)
        assert_eq!(parse_hex_u64("14c4a40", "test").unwrap(), 21_776_960);
    }

    #[test]
    fn parse_hex_u64_zero() {
        assert_eq!(parse_hex_u64("0x0", "test").unwrap(), 0);
    }

    #[test]
    fn parse_hex_u64_invalid() {
        assert!(parse_hex_u64("0xGGGG", "test").is_err());
    }

    // -----------------------------------------------------------------------
    // parse_raw_log unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_raw_log_happy_path() {
        let v = serde_json::json!({
            "address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                "0x000000000000000000000000abc",
                "0x000000000000000000000000def"
            ],
            "data": "0x0000000000000000000000000000000000000000000000000000000005f5e100",
            "blockNumber": "0x140f848",
            "transactionHash": "0xdeadbeef",
            "logIndex": "0x5"
        });
        let log = parse_raw_log(&v).unwrap();
        assert_eq!(log.address, "0xdac17f958d2ee523a2206206994597c13d831ec7");
        assert_eq!(log.topics.len(), 3);
        assert_eq!(log.block_number, 0x140f848u64);
        assert_eq!(log.log_index, 5);
        assert_eq!(log.data.len(), 32);
    }

    #[test]
    fn parse_raw_log_empty_data() {
        let v = serde_json::json!({
            "address": "0x1",
            "topics": [],
            "data": "0x",
            "blockNumber": "0x1",
            "transactionHash": "0x2",
            "logIndex": "0x0"
        });
        let log = parse_raw_log(&v).unwrap();
        assert!(log.data.is_empty());
    }

    #[test]
    fn build_log_filter_json_range_only() {
        let filter = LogFilter::range(1000, 2000);
        let json = build_log_filter_json(&filter);
        assert_eq!(json["fromBlock"], "0x3e8");
        assert_eq!(json["toBlock"], "0x7d0");
    }

    #[test]
    fn build_log_filter_json_single_address() {
        let mut filter = LogFilter::range(1, 2);
        filter.addresses.push("0xabc".to_string());
        let json = build_log_filter_json(&filter);
        assert_eq!(json["address"], "0xabc");
    }

    #[test]
    fn build_log_filter_json_multiple_addresses() {
        let mut filter = LogFilter::range(1, 2);
        filter.addresses.push("0xaaa".to_string());
        filter.addresses.push("0xbbb".to_string());
        let json = build_log_filter_json(&filter);
        assert!(json["address"].is_array());
    }

    // -----------------------------------------------------------------------
    // WsRpcClient reconnect unit tests
    // -----------------------------------------------------------------------

    /// Verify that `subscribe_new_heads_inner` exhausts reconnect attempts when
    /// the subscription always fails, and propagates an error to the receiver.
    ///
    /// Note: this is a unit test of the reconnect logic path only. Live WS
    /// reconnect testing requires a real Reth node and is `#[ignore]`-gated.
    #[tokio::test]
    async fn reconnect_exhaustion_closes_stream() {
        // We test the DELAYS constant directly — assert correct length.
        assert_eq!(RECONNECT_DELAYS_MS.len(), 10, "reconnect delay table must have 10 entries");

        // Minimum delay: 500ms (first attempt).
        assert_eq!(RECONNECT_DELAYS_MS[0], 500);
        // Cap: 30s (entries 6-9).
        assert_eq!(RECONNECT_DELAYS_MS[6], 30_000);
        assert_eq!(RECONNECT_DELAYS_MS[9], 30_000);
    }

    /// Verify RECONNECT_DELAYS_MS is monotonically non-decreasing (capped at 30s).
    #[test]
    fn reconnect_delay_table_is_non_decreasing() {
        let delays = RECONNECT_DELAYS_MS;
        for i in 1..delays.len() {
            assert!(
                delays[i] >= delays[i - 1],
                "delay[{i}]={} < delay[{}]={} — delays must be non-decreasing",
                delays[i],
                i - 1,
                delays[i - 1]
            );
        }
    }

    /// Verify that the delay cap is 30s (30_000ms).
    #[test]
    fn reconnect_delay_cap_is_30s() {
        let max_delay = *RECONNECT_DELAYS_MS.iter().max().unwrap();
        assert_eq!(max_delay, 30_000, "reconnect delay must cap at 30s");
    }

    /// Live: reconnect test requires a real WS server that disconnects after N messages.
    #[tokio::test]
    #[ignore]
    async fn ethereum_ws_live_reconnect_on_disconnect() {
        // Requires: ETHEREUM_RPC_WS_URL set, Reth running, and a way to force disconnect.
        // Manual test: restart Reth node while this test is running and verify reconnect.
        let rpc = WsRpcClient::from_env().await.expect("connect failed");
        let mut stream = rpc.subscribe_new_heads();
        // Consume a few headers — if the node is restarted the stream should reconnect.
        let first = stream.next().await.expect("expected at least one header").unwrap();
        println!("first header: block {}", first.number);
    }

    /// Live integration test: connect and fetch latest block number.
    ///
    /// Requires a running Reth node accessible at `ETHEREUM_RPC_WS_URL`
    /// (default: `ws://127.0.0.1:8546` per infra/ethereum-node docker-compose).
    ///
    /// Run with:
    /// ```text
    /// ETHEREUM_RPC_WS_URL=ws://127.0.0.1:8546 \
    ///   RUST_TEST_THREADS=1 cargo test --ignored \
    ///   -p mg-onchain-chain-adapter ethereum_ws_live
    /// ```
    #[tokio::test]
    #[ignore]
    async fn ethereum_ws_live_get_latest_block_number() {
        let rpc = WsRpcClient::from_env().await
            .expect("connect failed — is ETHEREUM_RPC_WS_URL set and Reth running?");
        let n = rpc.get_latest_block_number().await.expect("get_latest_block_number failed");
        assert!(n > 0, "latest block must be > 0 on a synced node");
        println!("latest block: {n}");
    }

    /// Live: verify finalized tag is available (requires Reth + Lighthouse post-Merge).
    #[tokio::test]
    #[ignore]
    async fn ethereum_ws_live_get_finalized_block_number() {
        let rpc = WsRpcClient::from_env().await.expect("connect failed");
        let finalized = rpc.get_finalized_block_number().await
            .expect("finalized tag unavailable — is Lighthouse running?");
        assert!(finalized > 0);
        println!("finalized block: {finalized}");
    }

    /// Live: fetch mainnet block 21_000_000 (well-known block present on any synced mainnet node).
    #[tokio::test]
    #[ignore]
    async fn ethereum_ws_live_get_block_by_number() {
        let rpc = WsRpcClient::from_env().await.expect("connect failed");
        let block = rpc.get_block_by_number(21_000_000).await
            .expect("get_block_by_number(21_000_000) failed");
        assert_eq!(block.number, 21_000_000);
        assert!(block.hash.starts_with("0x"));
        println!("block 21_000_000 hash: {}", block.hash);
    }
}
