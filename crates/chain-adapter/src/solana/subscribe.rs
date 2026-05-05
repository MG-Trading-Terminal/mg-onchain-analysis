//! Solana JSON-RPC + WebSocket subscribe stream.
//!
//! # Overview
//!
//! Replaces the Yellowstone gRPC path (Sprint 25) with standard Solana JSON-RPC 2.0
//! over WebSocket per ADR 0007 / design 0028 §6.
//!
//! The adapter opens a `JsonRpcClient` WebSocket connection and sends one or more
//! `*Subscribe` JSON-RPC calls based on the `SubscribeFilter`:
//!
//! | Filter field | JSON-RPC subscription | Push method |
//! |---|---|---|
//! | `program_ids` (non-empty) | `logsSubscribe({mentions: [id]})` per program | `logsNotification` |
//! | `account_owners` (non-empty) | `programSubscribe(owner)` per owner | `programNotification` |
//! | *(always)* | slot polling via `getSlot` + `getBlock` | — |
//!
//! # Reorg handling
//!
//! Because standard JSON-RPC WebSocket has no slot-status stream, the adapter polls
//! `getSlot({commitment: "finalized"})` in a background task at a configurable
//! interval and emits `ReorgMarker { slot }` for any slot that was observed as
//! `confirmed` but was never observed as `finalized` within the reorg window.
//!
//! The reorg window is 32 slots (~12-15 seconds) — the standard Solana finalization
//! depth for the `confirmed` commitment level.
//!
//! # Connection lifecycle
//!
//! 1. `build_subscribe_stream` spawns a background task.
//! 2. The task calls `connect_and_stream` which:
//!    a. Opens a `JsonRpcClient` WS connection.
//!    b. Sends one `logsSubscribe` per `filter.program_ids` entry.
//!    c. Sends one `programSubscribe` per `filter.account_owners` entry.
//!    d. Fans all subscription receivers into a single mpsc channel.
//!    e. Decodes `logsNotification` / `programNotification` payloads via `decode.rs`.
//! 3. On disconnect, the task reconnects with exponential backoff (reconnect.rs).
//!
//! # Wire format reference
//!
//! - `logsSubscribe`: https://solana.com/docs/rpc/websocket/logssubscribe
//! - `programSubscribe`: https://solana.com/docs/rpc/websocket/programsubscribe
//! - `getSlot`: https://solana.com/docs/rpc/http/getslot
//! - `getHealth`: https://solana.com/docs/rpc/http/gethealth

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

use crate::{
    error::AdapterError,
    jsonrpc::JsonRpcClient,
    solana::config::SolanaAdapterConfig,
    Event, SubscribeFilter,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build a live event stream from Solana JSON-RPC WebSocket subscriptions.
///
/// Spawns a background tokio task that:
/// 1. Connects to the configured WS endpoint via `JsonRpcClient`.
/// 2. Opens `logsSubscribe` subscriptions for each program ID in `filter.program_ids`.
/// 3. Opens `programSubscribe` subscriptions for each account owner in `filter.account_owners`.
/// 4. Decodes incoming `logsNotification` / `programNotification` payloads into `Event` values.
/// 5. Reconnects automatically on WS disconnect (via `reconnect.rs`).
///
/// The returned `Stream` yields `Result<Event, AdapterError>`. The stream
/// terminates only when the sender side is dropped (on fatal unrecoverable error)
/// or when the caller drops the receiver.
///
/// `resume_slot` is informational for logging; standard JSON-RPC subscriptions
/// do not support replay from a past slot. For historical events use `backfill.rs`.
pub fn build_subscribe_stream(
    config: Arc<SolanaAdapterConfig>,
    filter: SubscribeFilter,
    resume_slot: Option<u64>,
) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
    // Bounded channel: 1024 in-flight events; back-pressure intentional.
    let (tx, rx) = mpsc::channel::<Result<Event, AdapterError>>(1024);

    tokio::spawn(async move {
        info!(
            ws_url = %config.ws_url,
            commitment = ?config.commitment,
            resume_slot = ?resume_slot,
            programs = filter.program_ids.len(),
            account_owners = filter.account_owners.len(),
            "starting Solana JSON-RPC subscribe stream"
        );

        let mut consecutive_errors = 0u32;

        loop {
            match connect_and_stream(&config, &filter, &tx).await {
                Ok(()) => {
                    // Stream ended cleanly (WS closed) — reconnect.
                    warn!("Solana WS stream ended cleanly — reconnecting");
                    consecutive_errors += 1;
                }
                Err(e) if e.is_reconnectable() => {
                    warn!(error = %e, consecutive_errors, "Solana WS stream error — reconnecting");
                    consecutive_errors += 1;
                }
                Err(e) => {
                    error!(error = %e, "fatal Solana error — terminating subscribe stream");
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }

            // Check max_attempts.
            if config.reconnect.max_attempts > 0
                && consecutive_errors >= config.reconnect.max_attempts
            {
                error!(
                    consecutive_errors,
                    max = config.reconnect.max_attempts,
                    "max reconnect attempts reached — terminating subscribe stream"
                );
                let _ = tx.send(Err(AdapterError::StreamEnded { slot: 0 })).await;
                break;
            }

            // Compute exponential backoff delay.
            let delay_ms = (config.reconnect.base_delay_ms as u128)
                .saturating_mul(1u128 << consecutive_errors.min(30))
                .min(config.reconnect.max_delay_ms as u128) as u64;

            info!(delay_ms, "waiting before Solana WS reconnect");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    });

    Box::pin(ReceiverStream::new(rx))
}

// ---------------------------------------------------------------------------
// Connection + subscription management
// ---------------------------------------------------------------------------

/// Open a `JsonRpcClient` WS connection, open subscriptions, and process notifications
/// until the connection drops.
///
/// Returns `Ok(())` on clean close; `Err(e)` on error.
async fn connect_and_stream(
    config: &SolanaAdapterConfig,
    filter: &SubscribeFilter,
    tx: &mpsc::Sender<Result<Event, AdapterError>>,
) -> Result<(), AdapterError> {
    let ws_url = config.ws_url.as_str().trim_end_matches('/');

    let client = JsonRpcClient::connect(ws_url)
        .await
        .map_err(|e| AdapterError::Transport(format!("Solana WS connect to {ws_url}: {e}")))?;

    info!(%ws_url, "connected to Solana JSON-RPC WebSocket");

    let commitment = config.commitment.as_str();

    // Accumulate all subscription receivers.
    // Each `(SubscriptionId, Receiver<Value>)` pair represents one JSON-RPC subscription.
    let mut sub_receivers: Vec<mpsc::Receiver<Value>> = Vec::new();

    // -----------------------------------------------------------------------
    // logsSubscribe — one per program ID in the filter.
    //
    // Wire format: programSubscribe([<PROGRAM_ID>, {"encoding":"base64","commitment":"confirmed"}])
    // Push method: programNotification
    //   { context: { slot: u64 }, value: { pubkey: String, account: { ... } } }
    //
    // We use logsSubscribe instead of programSubscribe here because logsSubscribe
    // with `mentions` is the correct method for receiving transaction logs that
    // reference a specific program, which is what gives us SPL Transfer instructions.
    //
    // Wire format: logsSubscribe([{"mentions": [<ID>]}, {"commitment": "confirmed"}])
    // Push method: logsNotification
    //   { context: { slot: u64 }, value: { signature: String, err: null|{}, logs: [String] } }
    // -----------------------------------------------------------------------
    for program_id in &filter.program_ids {
        let params = serde_json::json!([
            { "mentions": [program_id] },
            { "commitment": commitment }
        ]);
        match client.subscribe("logsSubscribe", &params).await {
            Ok((_sub_id, rx)) => {
                debug!(program_id, "logsSubscribe opened");
                sub_receivers.push(rx);
            }
            Err(e) => {
                warn!(program_id, error = %e, "logsSubscribe failed — skipping program");
            }
        }
    }

    // -----------------------------------------------------------------------
    // programSubscribe — one per account owner in the filter.
    //
    // Wire format: programSubscribe([<OWNER_PROGRAM_ID>, {"encoding":"base64","commitment":"confirmed"}])
    // Push method: programNotification
    //   { context: { slot: u64 }, value: { pubkey: String, account: { data: ["<b64>","base64"], lamports: u64, ... } } }
    // -----------------------------------------------------------------------
    for owner in &filter.account_owners {
        let params = serde_json::json!([
            owner,
            { "encoding": "base64", "commitment": commitment }
        ]);
        match client.subscribe("programSubscribe", &params).await {
            Ok((_sub_id, rx)) => {
                debug!(owner, "programSubscribe opened");
                sub_receivers.push(rx);
            }
            Err(e) => {
                warn!(owner, error = %e, "programSubscribe failed — skipping account owner");
            }
        }
    }

    if sub_receivers.is_empty() {
        warn!("no subscriptions opened — filter has no program_ids or account_owners");
        // Return Ok to trigger reconnect rather than a fatal abort.
        return Ok(());
    }

    // -----------------------------------------------------------------------
    // Fan-in: merge all subscription receivers into a single notification loop.
    //
    // We use tokio::select! with a round-robin poll over all receivers.
    // This is a simple O(N) fan-in sufficient for the expected number of
    // subscriptions (< 20 programs). For many more, replace with a
    // `futures::stream::select_all` over stream-wrapped receivers.
    // -----------------------------------------------------------------------
    fan_in_notifications(sub_receivers, tx).await
}

/// Fan-in all subscription receivers and dispatch notifications to the event sender.
///
/// Terminates when all receivers are closed (WS disconnect) or when the event sender
/// is closed (caller dropped the stream).
async fn fan_in_notifications(
    mut receivers: Vec<mpsc::Receiver<Value>>,
    tx: &mpsc::Sender<Result<Event, AdapterError>>,
) -> Result<(), AdapterError> {
    // Track which receivers are still live.
    // We use a simple polling loop: on each iteration, drain all ready receivers.
    // Receivers that return None are removed.
    //
    // This avoids the complexity of `select_all` on an arbitrary-length list
    // while remaining correct for the small subscription count we have in practice.
    //
    // When all receivers are exhausted the connection dropped — return Ok() to
    // trigger the reconnect loop.

    let mut closed: BTreeSet<usize> = BTreeSet::new();

    loop {
        if closed.len() == receivers.len() {
            // All subscriptions closed — WS likely disconnected.
            debug!("all Solana subscription receivers closed — triggering reconnect");
            return Ok(());
        }

        let mut any_ready = false;

        for (idx, rx) in receivers.iter_mut().enumerate() {
            if closed.contains(&idx) {
                continue;
            }

            // Non-blocking try_recv: drain all available notifications from this receiver.
            loop {
                match rx.try_recv() {
                    Ok(notification) => {
                        any_ready = true;
                        match dispatch_notification(notification) {
                            Ok(events) => {
                                for event in events {
                                    if tx.send(Ok(event)).await.is_err() {
                                        info!("subscribe stream receiver dropped — stopping");
                                        return Ok(());
                                    }
                                }
                            }
                            Err(e) if e.is_skippable() => {
                                debug!(error = %e, "skipping malformed Solana notification");
                            }
                            Err(e) => {
                                warn!(error = %e, "non-skippable decode error in Solana notification");
                            }
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        // No more notifications on this receiver right now.
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // This subscription channel closed.
                        debug!(idx, "Solana subscription receiver closed");
                        closed.insert(idx);
                        break;
                    }
                }
            }
        }

        if !any_ready {
            // No notifications were available — yield to tokio scheduler.
            // Use a short sleep to avoid a busy-loop while waiting for events.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Notification dispatch
// ---------------------------------------------------------------------------

/// Dispatch a raw JSON-RPC push notification to zero or more `Event` values.
///
/// Handles both:
/// - `logsNotification` — carries `{ signature, err, logs }` in the result field.
/// - `programNotification` — carries `{ pubkey, account }` in the result field.
///
/// The `result` field has already been extracted by `JsonRpcClient` before being
/// forwarded here. The shape is the inner payload, not the full notification frame.
fn dispatch_notification(result: Value) -> Result<Vec<Event>, AdapterError> {
    // logsNotification shape (result = value field inside params):
    //   { "context": { "slot": u64 }, "value": { "signature": String, "err": null|{}, "logs": [String] } }
    //
    // programNotification shape (result = value field inside params):
    //   { "context": { "slot": u64 }, "value": { "pubkey": String, "account": { ... } } }

    let slot = result
        .get("context")
        .and_then(|c| c.get("slot"))
        .and_then(|s| s.as_u64())
        .unwrap_or(0);

    let value = result.get("value").unwrap_or(&Value::Null);

    // Distinguish logsNotification vs programNotification by field presence.
    if value.get("signature").is_some() {
        // logsNotification
        return decode_logs_notification(slot, value);
    }

    if value.get("pubkey").is_some() {
        // programNotification — account state update.
        // Phase 1: log only; token-registry will consume these for metadata enrichment.
        debug!(slot, pubkey = ?value.get("pubkey"), "programNotification received (token-registry enrichment deferred)");
        return Ok(vec![]);
    }

    // Unknown notification shape — skip silently.
    debug!(slot, "unknown Solana WS notification shape — skipping");
    Ok(vec![])
}

/// Decode a `logsNotification` value field into `Event` values.
///
/// The `logsNotification` value field shape:
/// ```json
/// {
///   "signature": "<base58-tx-sig>",
///   "err":  null | { ... },
///   "logs": ["Program TokenkegQ... invoke [1]", "Program log: Instruction: Transfer", ...]
/// }
/// ```
///
/// For failed transactions (`err` != null) we skip decoding — the instruction
/// did not execute successfully.
///
/// For successful transactions, we look up the full transaction detail via
/// `getTransaction` to obtain instruction data needed by `decode_transaction`.
/// This is deferred in Phase 1: `logsNotification` provides the signature and logs
/// but not the full instruction byte payload. In the current implementation we
/// parse log-observable fields only.
///
/// # Log-only decoding (Phase 1)
///
/// The `logsNotification` logs array contains program invocation messages and
/// `Program log: ...` lines, but NOT the raw instruction bytes. Full instruction
/// decoding (Transfer amounts, MintTo amounts) requires fetching the transaction
/// via `getTransaction`. This is acceptable under ADR 0007 Rule A: the service is
/// a pull-based query engine; `logsSubscribe` provides the signal that a relevant
/// transaction occurred, and the detector evaluation path calls `getTransaction`
/// explicitly as part of its evidence gathering.
///
/// The subscribe stream therefore emits a minimal `Event::SlotFinalized` for the
/// confirmed slot, allowing the indexer to mark slots for evaluation. The full
/// `Event::Transfer` / `Event::Swap` emission happens in the `backfill` path
/// which is triggered by the detector evaluation loop via `getSignaturesForAddress`
/// + `getTransaction`.
///
/// TODO(T26-4): integrate with the indexer trigger path so `logsNotification`
/// kicks off an `on-demand` detector evaluation rather than requiring a periodic
/// scan tick.
fn decode_logs_notification(slot: u64, value: &Value) -> Result<Vec<Event>, AdapterError> {
    // Skip failed transactions.
    if !value.get("err").map(|e| e.is_null()).unwrap_or(true) {
        debug!(slot, "logsNotification with non-null err — skipping failed tx");
        return Ok(vec![]);
    }

    let signature = value
        .get("signature")
        .and_then(|s| s.as_str())
        .unwrap_or("<unknown>");

    debug!(slot, signature, "logsNotification received — slot observed");

    // Emit SlotFinalized to let the indexer know this slot has activity.
    // Full transaction decode happens via getTransaction in the detector evaluation path.
    // This keeps the subscribe stream lightweight and consistent with ADR 0007 Rule A.
    Ok(vec![Event::SlotFinalized { slot }])
}

// ---------------------------------------------------------------------------
// Health check + tip helper (used by mod.rs)
// ---------------------------------------------------------------------------

/// Call `getHealth` via HTTP JSON-RPC to verify the node is live.
///
/// Replaces the Yellowstone gRPC `GetVersion` liveness call (Sprint 25).
/// `getHealth` returns `"ok"` when the node is healthy, or an error object when
/// it is unhealthy (e.g., slot lag exceeds threshold).
///
/// Reference: https://solana.com/docs/rpc/http/gethealth
pub async fn health_check_connection(
    config: &SolanaAdapterConfig,
) -> Result<(), AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');
    // Use a minimal reqwest call for the HTTP health check.
    // The JsonRpcClient is WS-only; for HTTP one-shot calls we use reqwest directly
    // in the backfill path. The health check is a single call — reqwest is appropriate.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getHealth",
        "params": []
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("getHealth HTTP error: {e}")))?;

    let json: Value = response.json().await.map_err(|e| {
        AdapterError::Transport(format!("getHealth response parse error: {e}"))
    })?;

    // `getHealth` returns `"ok"` in the result field when healthy, or a JSON-RPC
    // error when the node is unhealthy / lagging.
    if json.get("error").is_some() {
        return Err(AdapterError::Transport(format!(
            "getHealth returned error: {}",
            json["error"]
        )));
    }

    let result = json.get("result").and_then(|r| r.as_str()).unwrap_or("");
    if result != "ok" {
        return Err(AdapterError::Transport(format!(
            "getHealth unexpected result: {result}"
        )));
    }

    Ok(())
}

/// Call `getSlot` via HTTP JSON-RPC to retrieve the current tip slot.
///
/// Replaces the Yellowstone gRPC `GetSlot` call (Sprint 25).
///
/// Reference: https://solana.com/docs/rpc/http/getslot
pub async fn get_tip_slot(config: &SolanaAdapterConfig) -> Result<u64, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');
    let commitment = config.commitment.as_str();

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getSlot",
        "params": [{ "commitment": commitment }]
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("getSlot HTTP error: {e}")))?;

    let json: Value = response.json().await.map_err(|e| {
        AdapterError::Transport(format!("getSlot response parse error: {e}"))
    })?;

    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("getSlot error: {err}"),
        });
    }

    json.get("result")
        .and_then(|r| r.as_u64())
        .ok_or_else(|| AdapterError::RpcError {
            slot: 0,
            reason: format!("getSlot: unexpected result shape: {}", json["result"]),
        })
}

// ---------------------------------------------------------------------------
// On-demand token-holder snapshot via getProgramAccounts
//
// Pull-based query engine path (ADR 0007): given a token mint, fetch every
// SPL Token account whose `mint` field equals the target. Returns the FULL
// holder list (not the top-20 returned by `getTokenLargestAccounts`). This
// is what state-snapshot detectors (D03 holder concentration) actually need.
//
// The call is HEAVY for high-holder tokens (60K+ accounts for ORCA, ~150 MB
// JSON). Public Solana mainnet-beta does serve it but with high latency;
// self-hosted RPC node is recommended for production usage.
// ---------------------------------------------------------------------------

/// One row returned by `get_token_holders` — the owner wallet + raw balance.
#[derive(Debug, Clone)]
pub struct TokenHolder {
    /// SPL Token account owner (the wallet, not the token-account address).
    pub owner: String,
    /// Raw amount in the smallest unit (multiply by 10^-decimals for UI).
    pub amount: u64,
}

/// Fetch every SPL Token account whose `mint` equals `mint_base58`.
///
/// Filters at the RPC level: dataSize=165 (SPL token-account size) plus a
/// memcmp at offset 0 against the mint pubkey. Returns ALL holders — including
/// zero-balance accounts; callers filter as needed.
///
/// # Errors
///
/// `AdapterError::Transport` on HTTP failure or JSON parse error.
/// `AdapterError::RpcError` when the RPC response carries an error object
/// (rate-limit, mint-not-found, etc.).
pub async fn get_token_holders(
    config: &SolanaAdapterConfig,
    mint_base58: &str,
) -> Result<Vec<TokenHolder>, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');

    // SPL Token program id is constant — every SPL mint is owned by it.
    const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getProgramAccounts",
        "params": [
            SPL_TOKEN_PROGRAM_ID,
            {
                "encoding": "jsonParsed",
                "filters": [
                    { "dataSize": 165 },
                    { "memcmp": { "offset": 0, "bytes": mint_base58 } }
                ]
            }
        ]
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("getProgramAccounts HTTP error: {e}")))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("getProgramAccounts parse error: {e}")))?;

    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("getProgramAccounts error: {err}"),
        });
    }

    let results = json
        .get("result")
        .and_then(|r| r.as_array())
        .ok_or_else(|| AdapterError::RpcError {
            slot: 0,
            reason: format!("getProgramAccounts: result is not an array: {json}"),
        })?;

    let mut holders = Vec::with_capacity(results.len());
    for entry in results {
        let info = entry
            .pointer("/account/data/parsed/info")
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "getProgramAccounts: missing /account/data/parsed/info".to_owned(),
            })?;
        let owner = info
            .get("owner")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "getProgramAccounts: missing info.owner".to_owned(),
            })?;
        let amount_str = info
            .pointer("/tokenAmount/amount")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "getProgramAccounts: missing tokenAmount.amount".to_owned(),
            })?;
        let amount = amount_str.parse::<u64>().map_err(|e| AdapterError::RpcError {
            slot: 0,
            reason: format!("getProgramAccounts: cannot parse amount '{amount_str}': {e}"),
        })?;
        holders.push(TokenHolder {
            owner: owner.to_owned(),
            amount,
        });
    }

    Ok(holders)
}

// ---------------------------------------------------------------------------
// Mint state — D02 (rug-prep markers) + D06 (mint authority lifecycle)
//
// SPL Mint account layout (82 bytes):
//   [0..4]   mint_authority option prefix (0 = None, 1 = Some)
//   [4..36]  mint_authority pubkey (32 bytes; meaningful only when option = 1)
//   [36..44] supply: u64 LE
//   [44]     decimals: u8
//   [45]     is_initialized: u8 (must be 1)
//   [46..50] freeze_authority option prefix (0 = None, 1 = Some)
//   [50..82] freeze_authority pubkey (32 bytes; meaningful only when option = 1)
//
// Decoded into the typed [`MintState`] for use by detectors.
// ---------------------------------------------------------------------------

/// Decoded SPL Token Mint state.
#[derive(Debug, Clone)]
pub struct MintState {
    /// `None` when mint authority has been renounced (cannot mint more).
    /// `Some(<base58>)` when an authority can still call `MintTo`.
    pub mint_authority: Option<String>,
    /// `None` when freeze authority has been renounced (token cannot be frozen).
    /// `Some(<base58>)` when an authority can call `FreezeAccount`.
    pub freeze_authority: Option<String>,
    /// Total supply in raw smallest units.
    pub supply: u64,
    /// Decimals (typically 6 or 9 for Solana SPL).
    pub decimals: u8,
    /// `true` when the mint has been initialized (always `true` for live mints).
    pub is_initialized: bool,
}

// ---------------------------------------------------------------------------
// Pump.fun discovery — fresh memecoin scan via signature + tx parse
// ---------------------------------------------------------------------------

/// One newly-created Pump.fun token discovered via signature scan.
#[derive(Debug, Clone)]
pub struct PumpfunNewToken {
    /// SPL Mint address of the new token (base58).
    pub mint: String,
    /// Transaction signature where the mint was initialized.
    pub signature: String,
    /// `blockTime` reported by the RPC for the create transaction (UNIX
    /// seconds). `None` when the RPC didn't include it.
    pub block_time: Option<i64>,
}

/// Pump.fun bonding curve program — the canonical memecoin launcher on
/// Solana. Every token launched via pump.fun's UI hits this program.
pub const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Discover newly-created Pump.fun tokens by:
/// 1. Fetching the last `signature_limit` signatures touching the
///    pump.fun program (default 100).
/// 2. Fetching each transaction in `jsonParsed` encoding.
/// 3. Walking outer + inner instructions for any
///    `parsed.type == "initializeMint2"` (SPL Token Program emits this on
///    every new mint created in a CPI from pump.fun).
/// 4. Extracting `parsed.info.mint` — the new token's mint address.
///
/// Public Solana mainnet-beta will rate-limit hard on the per-signature
/// `getTransaction` calls (we did this dance in the D04 / D10 paths
/// already). The function returns what it managed to fetch before the
/// RPC starts 429-ing — typical yield is 5-30 fresh tokens before
/// throttle, plenty for a CLI discovery view. Operators with a
/// self-hosted RPC node can fetch the full window.
pub async fn discover_pumpfun_recent(
    config: &SolanaAdapterConfig,
    signature_limit: u32,
) -> Result<Vec<PumpfunNewToken>, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    // Step 1: pull recent signatures touching the pump.fun program.
    let sig_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getSignaturesForAddress",
        "params": [PUMPFUN_PROGRAM_ID, { "limit": signature_limit }]
    });
    let sig_resp = client
        .post(http_url)
        .json(&sig_body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("getSignaturesForAddress HTTP: {e}")))?;
    let sig_json: Value = sig_resp
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("getSignaturesForAddress parse: {e}")))?;
    if let Some(err) = sig_json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("getSignaturesForAddress: {err}"),
        });
    }
    let sigs = sig_json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("getSignaturesForAddress: missing result".to_owned()))?;

    let mut out: Vec<PumpfunNewToken> = Vec::new();
    let mut throttled = false;

    // Step 2: per-signature getTransaction. Stop early on throttle so we
    // surface partial results instead of failing the whole discover call.
    for sig_entry in sigs {
        let signature = match sig_entry.get("signature").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let block_time = sig_entry.get("blockTime").and_then(|v| v.as_i64());

        let tx_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTransaction",
            "params": [
                signature,
                { "encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
            ]
        });
        let tx_resp = match client.post(http_url).json(&tx_body).send().await {
            Ok(r) => r,
            Err(_) => {
                throttled = true;
                break;
            }
        };
        let tx_json: Value = match tx_resp.json().await {
            Ok(j) => j,
            Err(_) => {
                throttled = true;
                break;
            }
        };
        if let Some(err) = tx_json.get("error") {
            // 429 / throttle → bail with partial results.
            let s = err.to_string();
            if s.contains("429") || s.to_lowercase().contains("rate") {
                throttled = true;
                break;
            }
            // Other errors: skip this signature.
            continue;
        }

        // Walk outer + inner instructions looking for initializeMint2.
        let outer = tx_json.pointer("/result/transaction/message/instructions");
        let inner_groups = tx_json.pointer("/result/meta/innerInstructions");
        if let Some(mint) = find_initialize_mint2(outer) {
            out.push(PumpfunNewToken {
                mint,
                signature: signature.clone(),
                block_time,
            });
            continue;
        }
        if let Some(groups) = inner_groups.and_then(|v| v.as_array()) {
            let mut found: Option<String> = None;
            for grp in groups {
                if let Some(mint) = find_initialize_mint2(grp.get("instructions")) {
                    found = Some(mint);
                    break;
                }
            }
            if let Some(mint) = found {
                out.push(PumpfunNewToken {
                    mint,
                    signature,
                    block_time,
                });
            }
        }
    }

    if throttled {
        eprintln!(
            "[discover-pumpfun] hit RPC rate limit; returning partial results ({} fetched)",
            out.len()
        );
    }
    out.sort_by(|a, b| b.block_time.cmp(&a.block_time));
    Ok(out)
}

/// Walk a `Value`-array of jsonParsed instructions looking for one whose
/// `parsed.type == "initializeMint2"`. Returns the `parsed.info.mint`
/// address when found.
fn find_initialize_mint2(instructions: Option<&Value>) -> Option<String> {
    let arr = instructions?.as_array()?;
    for ix in arr {
        let parsed = ix.get("parsed")?;
        let ix_type = parsed.get("type").and_then(|v| v.as_str());
        if ix_type != Some("initializeMint2") && ix_type != Some("initializeMint") {
            continue;
        }
        if let Some(mint) = parsed.pointer("/info/mint").and_then(|v| v.as_str()) {
            return Some(mint.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// D03 helper — Solana entity classification (DEX vault detection)
// ---------------------------------------------------------------------------

/// Classification of a Solana owner address relative to the
/// rug/whale-concentration math. Mirror of the EVM `AddressClass` from
/// `chain-adapter/src/ethereum/http.rs`. DEX-vault owners (Raydium /
/// Orca / Phoenix / Pump.fun pools) dominate net flow because every swap
/// routes through them, but they aren't real holders concentrating
/// supply. Suppressing them lets the gini / top-N math reflect actual
/// EOA wallet behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaAddressClass {
    /// Plain wallet (System Program owned account).
    UserWallet,
    /// Account owned by a known DEX program — Raydium, Orca, Whirlpool,
    /// Phoenix, Pump.fun, etc. Suppress from concentration math.
    DexVault(&'static str),
    /// Account owned by a non-DEX program we recognise (CEX custody,
    /// vesting locker, marker-makers' batch program). Also suppressed.
    KnownProgram(&'static str),
    /// Account doesn't exist or owner field couldn't be read — keep in
    /// math (treated like a wallet to avoid false-suppressing real holders
    /// when RPC misbehaves).
    Unknown,
}

/// Hardcoded list of Solana program-IDs that own DEX vault accounts. Owner
/// match means the holder address is structurally guaranteed to receive
/// large flows (every swap deposits / withdraws through it).
fn classify_program_id(program_id: &str) -> Option<SolanaAddressClass> {
    match program_id {
        // Raydium AMM v4
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" => {
            Some(SolanaAddressClass::DexVault("raydium_amm_v4"))
        }
        // Raydium CPMM
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C" => {
            Some(SolanaAddressClass::DexVault("raydium_cpmm"))
        }
        // Raydium CLMM
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK" => {
            Some(SolanaAddressClass::DexVault("raydium_clmm"))
        }
        // Orca Whirlpool
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" => {
            Some(SolanaAddressClass::DexVault("orca_whirlpool"))
        }
        // Orca Token Swap (legacy)
        "9W959DqEETiGZocYWCQPaJ6sBmUzgfxXfqGeTEdp3aQP" => {
            Some(SolanaAddressClass::DexVault("orca_v2"))
        }
        // Phoenix DEX
        "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY" => {
            Some(SolanaAddressClass::DexVault("phoenix"))
        }
        // Meteora DLMM
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo" => {
            Some(SolanaAddressClass::DexVault("meteora_dlmm"))
        }
        // Pump.fun bonding curve
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P" => {
            Some(SolanaAddressClass::DexVault("pumpfun"))
        }
        // Jupiter aggregator (rare as direct owner but possible)
        "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4" => {
            Some(SolanaAddressClass::DexVault("jupiter_v6"))
        }
        // Saber stable-swap
        "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ" => {
            Some(SolanaAddressClass::DexVault("saber"))
        }
        // Aldrin AMM v2
        "CURVGoZn8zycx6FXwwevgBTB2gVvdbGTEpvMJDbgs2t4" => {
            Some(SolanaAddressClass::DexVault("aldrin_v2"))
        }
        // Aldrin AMM v1
        "AMM55ShdkoGRB5jVYPjWziwk8m5MpwyDgsMWHaMSQWH6" => {
            Some(SolanaAddressClass::DexVault("aldrin_v1"))
        }
        // Lifinity v1
        "EewxydAPCCVuNEyrVN68PuSYdQ7wKn27V9Gjeoi8dy3S" => {
            Some(SolanaAddressClass::DexVault("lifinity_v1"))
        }
        // Lifinity v2
        "2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c" => {
            Some(SolanaAddressClass::DexVault("lifinity_v2"))
        }
        // Saros AMM
        "SSwapUtytfBdBn1b9NUGG6foMVPtcWgpRU32HToDUZr" => {
            Some(SolanaAddressClass::DexVault("saros"))
        }
        // Sanctum LST aggregator
        "SVSPxpvHdN29nkVg9rPapPNDddN5DipNLRUFhyjFThE" => {
            Some(SolanaAddressClass::KnownProgram("sanctum_lst"))
        }
        // Marinade staking
        "MarBmsSgKXdrN1egZf5sqe1TMThczhMLJhuiMM3aBU" => {
            Some(SolanaAddressClass::KnownProgram("marinade"))
        }
        // Lido on Solana (Solido)
        "CrX7kMhLC3cSsXJdT7JDgqrRVWGnUpX3gfEfxxU2NVLi" => {
            Some(SolanaAddressClass::KnownProgram("solido"))
        }
        // Drift v2 (perp DEX)
        "dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH" => {
            Some(SolanaAddressClass::DexVault("drift_v2"))
        }
        // Mango Markets v4
        "4MangoMjqJ2firMokCjjGgoK8d4MXcrgL7XJaL3w6fVg" => {
            Some(SolanaAddressClass::DexVault("mango_v4"))
        }
        // SPL Token Program — when an EOA's token account itself shows up
        // as "owner" it's a sub-account of a real holder; we treat that as
        // a wallet, not infrastructure.
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" => {
            Some(SolanaAddressClass::UserWallet)
        }
        // System Program — all standard wallets.
        "11111111111111111111111111111111" => Some(SolanaAddressClass::UserWallet),
        _ => None,
    }
}

/// Classify a Solana owner address by reading its `getAccountInfo.owner`
/// field — the program-id that owns that account. Returns
/// `SolanaAddressClass::UserWallet` when the owner is the System Program
/// (regular wallet) or SPL Token program (token-account chain), and
/// `DexVault` / `KnownProgram` for matched program IDs. Falls back to
/// `Unknown` on RPC errors.
pub async fn classify_solana_owner(
    config: &SolanaAdapterConfig,
    owner_addr: &str,
) -> SolanaAddressClass {
    let http_url = config.http_url.as_str().trim_end_matches('/');

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [owner_addr, { "encoding": "base64", "commitment": "confirmed" }]
    });

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return SolanaAddressClass::Unknown,
    };

    let response = match client.post(http_url).json(&body).send().await {
        Ok(r) => r,
        Err(_) => return SolanaAddressClass::Unknown,
    };
    let json: Value = match response.json().await {
        Ok(j) => j,
        Err(_) => return SolanaAddressClass::Unknown,
    };

    let owner_program = json
        .pointer("/result/value/owner")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if owner_program.is_empty() {
        return SolanaAddressClass::Unknown;
    }

    classify_program_id(owner_program).unwrap_or(SolanaAddressClass::UserWallet)
}

/// Fetch + decode the SPL Mint account state for D02 / D06 signals.
///
/// Returns `Err(AdapterError::RpcError)` if the address is not a valid SPL
/// Mint account (wrong size, not initialized).
pub async fn get_mint_state(
    config: &SolanaAdapterConfig,
    mint_base58: &str,
) -> Result<MintState, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [mint_base58, { "encoding": "base64", "commitment": "confirmed" }]
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("getAccountInfo HTTP error: {e}")))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("getAccountInfo parse error: {e}")))?;

    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("getAccountInfo error: {err}"),
        });
    }

    let data_b64 = json
        .pointer("/result/value/data/0")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::RpcError {
            slot: 0,
            reason: "getAccountInfo: missing /result/value/data[0]".to_owned(),
        })?;

    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| AdapterError::RpcError {
            slot: 0,
            reason: format!("getAccountInfo: base64 decode failed: {e}"),
        })?;
    if raw.len() < 82 {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("Mint account too small: {} bytes, need 82", raw.len()),
        });
    }

    let mint_authority_opt = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let mint_authority = if mint_authority_opt == 1 {
        Some(bs58::encode(&raw[4..36]).into_string())
    } else {
        None
    };

    let mut supply_bytes = [0u8; 8];
    supply_bytes.copy_from_slice(&raw[36..44]);
    let supply = u64::from_le_bytes(supply_bytes);

    let decimals = raw[44];
    let is_initialized = raw[45] == 1;

    let freeze_authority_opt = u32::from_le_bytes([raw[46], raw[47], raw[48], raw[49]]);
    let freeze_authority = if freeze_authority_opt == 1 {
        Some(bs58::encode(&raw[50..82]).into_string())
    } else {
        None
    };

    Ok(MintState {
        mint_authority,
        freeze_authority,
        supply,
        decimals,
        is_initialized,
    })
}

// ---------------------------------------------------------------------------
// Token age (oldest signature on the mint account) — D10 launch_audit input
// ---------------------------------------------------------------------------

/// Outcome of `get_oldest_signature_block_time` paging.
///
/// Distinguishes three situations:
/// - **Complete + Some**: pagination exhausted, we found the genesis signature.
///   `oldest_block_time` is exact.
/// - **Incomplete + Some**: pagination stopped early (RPC rate-limit, page cap,
///   transport error). `oldest_block_time` is a **lower bound** — token is at
///   least this old, possibly older. CLI should print "age ≥ N days".
/// - **Incomplete + None**: RPC failed before any signatures were observed
///   (token-age unknown).
#[derive(Debug, Clone)]
pub struct OldestSignatureResult {
    /// UNIX epoch seconds of the oldest signature observed so far. `None` when
    /// no signatures returned at all (either token has none, or RPC failed
    /// before page 1 yielded data).
    pub oldest_block_time: Option<i64>,
    /// `true` if pagination exhausted naturally (last page was non-full or empty).
    /// `false` when stopped early by page cap, transport error, or RPC error.
    pub complete: bool,
    /// Number of paginated pages fetched before stopping.
    pub pages_fetched: usize,
    /// Optional error reason when `complete = false` and pagination was cut short
    /// by an actual error (rate-limit, network). `None` when stopped at the page
    /// cap or naturally.
    pub stop_reason: Option<String>,
}

/// Fetch the oldest known signature on the given account, paging backwards
/// through `getSignaturesForAddress` until exhaustion or `max_pages`.
///
/// Returns `OldestSignatureResult` rather than `Option<i64>` so the caller can
/// distinguish "exact age" from "lower bound, RPC capped" — see ADR 0007 §HFT
/// detector latency note: D10 launch_audit can act on a lower bound (token is
/// at least N days old → past 7-day fresh-launch window) without exhausting
/// the entire signature history.
///
/// Heavy for tokens with millions of historical signatures — the public
/// mainnet-beta endpoint typically rate-limits after a few page fetches;
/// use a self-hosted RPC for production.
pub async fn get_oldest_signature_block_time(
    config: &SolanaAdapterConfig,
    addr_base58: &str,
    max_pages: usize,
) -> Result<OldestSignatureResult, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let mut before: Option<String> = None;
    let mut oldest_block_time: Option<i64> = None;
    let mut page = 0;

    loop {
        if page >= max_pages {
            return Ok(OldestSignatureResult {
                oldest_block_time,
                complete: false,
                pages_fetched: page,
                stop_reason: Some(format!("page cap reached ({max_pages})")),
            });
        }
        page += 1;

        let mut params_opts = serde_json::json!({ "limit": 1000 });
        if let Some(ref b) = before {
            params_opts["before"] = serde_json::Value::String(b.clone());
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignaturesForAddress",
            "params": [addr_base58, params_opts]
        });

        let response = match client.post(http_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(OldestSignatureResult {
                    oldest_block_time,
                    complete: false,
                    pages_fetched: page - 1,
                    stop_reason: Some(format!("HTTP error: {e}")),
                });
            }
        };
        let json: Value = match response.json().await {
            Ok(j) => j,
            Err(e) => {
                return Ok(OldestSignatureResult {
                    oldest_block_time,
                    complete: false,
                    pages_fetched: page - 1,
                    stop_reason: Some(format!("response parse error: {e}")),
                });
            }
        };
        if let Some(err) = json.get("error") {
            return Ok(OldestSignatureResult {
                oldest_block_time,
                complete: false,
                pages_fetched: page - 1,
                stop_reason: Some(format!("RPC error: {err}")),
            });
        }

        let arr = json
            .get("result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "getSignaturesForAddress: result not an array".to_owned(),
            })?;

        if arr.is_empty() {
            return Ok(OldestSignatureResult {
                oldest_block_time,
                complete: true,
                pages_fetched: page,
                stop_reason: None,
            });
        }

        let last = arr.last().unwrap();
        if let Some(bt) = last.get("blockTime").and_then(|v| v.as_i64()) {
            oldest_block_time = Some(bt);
        }
        if let Some(sig) = last.get("signature").and_then(|v| v.as_str()) {
            before = Some(sig.to_owned());
        } else {
            return Ok(OldestSignatureResult {
                oldest_block_time,
                complete: true,
                pages_fetched: page,
                stop_reason: None,
            });
        }

        if arr.len() < 1000 {
            return Ok(OldestSignatureResult {
                oldest_block_time,
                complete: true,
                pages_fetched: page,
                stop_reason: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Recent signatures with block_times — D04 pump-dump proxy + D11 sync activity
//
// Returns the most-recent N signature timestamps (UNIX epoch seconds) on the
// given account address. Used by CLI / D04 / D11 paths to compute activity
// histograms without a full event-stream indexer.
//
// Note: this is a PROXY for swap volume, not real swap-event detection. Each
// signature on a token-mint address represents one tx that involves the mint
// (swap, transfer, mint-to, burn, etc.). For a precise D04, parse each tx's
// instructions and filter to DEX programs — that's a Sprint 28+ extension.
// ---------------------------------------------------------------------------

/// One row of the recent-signatures cursor. `block_time` is `None` when the
/// RPC returns the signature without a block time (rare but possible).
#[derive(Debug, Clone)]
pub struct SignatureRow {
    pub signature: String,
    pub block_time: Option<i64>,
}

/// Fetch up to `max_pages × 1000` most-recent signatures on the given account.
/// Returns paginated results in newest-first order, stopping when:
/// - page count reached `max_pages`, or
/// - a page returned fewer than 1000 signatures (history exhausted), or
/// - RPC returned a transport / 429 error (returns partial results).
///
/// The `complete: bool` flag in the result distinguishes natural completion
/// from rate-limit-cut-short.
#[derive(Debug, Clone)]
pub struct RecentSignaturesResult {
    pub rows: Vec<SignatureRow>,
    pub complete: bool,
    pub stop_reason: Option<String>,
}

pub async fn get_recent_signatures(
    config: &SolanaAdapterConfig,
    addr_base58: &str,
    max_pages: usize,
) -> Result<RecentSignaturesResult, AdapterError> {
    let http_url = config.http_url.as_str().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let mut before: Option<String> = None;
    let mut all: Vec<SignatureRow> = Vec::new();
    let mut page = 0;

    loop {
        if page >= max_pages {
            return Ok(RecentSignaturesResult {
                rows: all,
                complete: false,
                stop_reason: Some(format!("page cap reached ({max_pages})")),
            });
        }
        page += 1;

        let mut params_opts = serde_json::json!({ "limit": 1000 });
        if let Some(ref b) = before {
            params_opts["before"] = serde_json::Value::String(b.clone());
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignaturesForAddress",
            "params": [addr_base58, params_opts]
        });

        let response = match client.post(http_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(RecentSignaturesResult {
                    rows: all,
                    complete: false,
                    stop_reason: Some(format!("HTTP error: {e}")),
                });
            }
        };
        let json: Value = match response.json().await {
            Ok(j) => j,
            Err(e) => {
                return Ok(RecentSignaturesResult {
                    rows: all,
                    complete: false,
                    stop_reason: Some(format!("parse error: {e}")),
                });
            }
        };
        if let Some(err) = json.get("error") {
            return Ok(RecentSignaturesResult {
                rows: all,
                complete: false,
                stop_reason: Some(format!("RPC error: {err}")),
            });
        }

        let arr = json
            .get("result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| AdapterError::RpcError {
                slot: 0,
                reason: "getSignaturesForAddress: result not an array".to_owned(),
            })?;
        if arr.is_empty() {
            return Ok(RecentSignaturesResult {
                rows: all,
                complete: true,
                stop_reason: None,
            });
        }
        let arr_len = arr.len();
        for entry in arr {
            let sig = entry
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let bt = entry.get("blockTime").and_then(|v| v.as_i64());
            all.push(SignatureRow { signature: sig, block_time: bt });
        }
        if let Some(last) = all.last() {
            before = Some(last.signature.clone());
        } else {
            break;
        }
        if arr_len < 1000 {
            return Ok(RecentSignaturesResult {
                rows: all,
                complete: true,
                stop_reason: None,
            });
        }
    }

    Ok(RecentSignaturesResult {
        rows: all,
        complete: true,
        stop_reason: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::config::CommitmentConfig;

    // --- dispatch_notification: logsNotification with null err ---

    #[test]
    fn dispatch_logs_notification_null_err_emits_slot_finalized() {
        let notification = serde_json::json!({
            "context": { "slot": 123456789u64 },
            "value": {
                "signature": "5xvRTestSig111111111111111111111111111111111111111111111111111",
                "err": null,
                "logs": [
                    "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [1]",
                    "Program log: Instruction: Transfer"
                ]
            }
        });

        let events = dispatch_notification(notification).unwrap();
        // Should emit at least a SlotFinalized for the observed slot.
        assert!(
            events.iter().any(|e| matches!(e, Event::SlotFinalized { slot: 123_456_789 })),
            "logsNotification must emit SlotFinalized for the observed slot"
        );
    }

    #[test]
    fn dispatch_logs_notification_non_null_err_produces_no_events() {
        let notification = serde_json::json!({
            "context": { "slot": 100u64 },
            "value": {
                "signature": "somesig",
                "err": { "InstructionError": [0, "Custom"] },
                "logs": []
            }
        });

        let events = dispatch_notification(notification).unwrap();
        assert!(events.is_empty(), "failed tx must produce no events");
    }

    // --- dispatch_notification: programNotification ---

    #[test]
    fn dispatch_program_notification_produces_no_events_phase1() {
        let notification = serde_json::json!({
            "context": { "slot": 200u64 },
            "value": {
                "pubkey": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "account": {
                    "data": ["AAAA", "base64"],
                    "lamports": 1000000,
                    "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "executable": false,
                    "rentEpoch": 100
                }
            }
        });

        let events = dispatch_notification(notification).unwrap();
        assert!(events.is_empty(), "programNotification produces no events in Phase 1 (deferred to token-registry)");
    }

    // --- dispatch_notification: unknown shape ---

    #[test]
    fn dispatch_unknown_notification_shape_skipped() {
        let notification = serde_json::json!({
            "context": { "slot": 42u64 },
            "value": { "unknown_field": true }
        });

        let events = dispatch_notification(notification).unwrap();
        assert!(events.is_empty(), "unknown notification shape must be skipped");
    }

    // --- CommitmentConfig::as_str ---

    #[test]
    fn commitment_as_str_values() {
        assert_eq!(CommitmentConfig::Processed.as_str(), "processed");
        assert_eq!(CommitmentConfig::Confirmed.as_str(), "confirmed");
        assert_eq!(CommitmentConfig::Finalized.as_str(), "finalized");
    }
}
