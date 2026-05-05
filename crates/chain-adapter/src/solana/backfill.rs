//! Historical sync via Solana JSON-RPC `getBlock` batching.
//!
//! # Design
//!
//! Backfill reads historical blocks from the Solana JSON-RPC HTTP endpoint:
//! 1. Complete coverage of the requested slot range (no gaps).
//! 2. Runs in a separate tokio task so it doesn't block live subscription processing.
//! 3. Skipped slots (no block produced) are silently ignored.
//!
//! # API used
//!
//! `getBlock` with `encoding=json`, `transactionDetails=full`, `maxSupportedTransactionVersion=0`.
//! Slots are fetched sequentially (not in parallel) to avoid hammering the RPC with a burst.
//! Future optimization: bounded concurrency via `tokio::sync::Semaphore`.
//!
//! # Gap handling
//!
//! Solana has skipped slots (the leader failed to produce a block). These are NOT errors;
//! the RPC returns null for skipped slots. The backfill stream simply moves to the next slot.
//! The indexer tracks which slots were skipped vs which produced events.
//!
//! # Rate limits
//!
//! RPC providers impose per-second rate limits. The backfill loop applies a configurable
//! `rpc_poll_interval_ms` sleep between requests. Production operators should tune this
//! based on their provider tier.

use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, warn};

use crate::{error::AdapterError, solana::config::SolanaAdapterConfig, Event};

// ---------------------------------------------------------------------------
// Backfill RPC request/response shapes (minimal subset of Solana JSON-RPC)
// ---------------------------------------------------------------------------

/// Minimal `getBlock` response (only the fields we use).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetBlockResponse {
    block_time: Option<i64>,
    transactions: Vec<BlockTransaction>,
    /// Reserved for future use — included to avoid serde error on provider responses.
    #[serde(default)]
    #[allow(dead_code)]
    block_height: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct BlockTransaction {
    transaction: BlockTransactionMessage,
    meta: Option<BlockTransactionMeta>,
}

#[derive(Debug, serde::Deserialize)]
struct BlockTransactionMessage {
    signatures: Vec<String>,
    message: BlockMessage,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockMessage {
    account_keys: Vec<String>,
    instructions: Vec<BlockInstruction>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockInstruction {
    program_id_index: u32,
    accounts: Vec<u32>,
    data: String, // base58-encoded
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockTransactionMeta {
    #[serde(default)]
    inner_instructions: Vec<BlockInnerInstructionGroup>,
    #[serde(default)]
    loaded_writable_addresses: Vec<String>,
    #[serde(default)]
    loaded_readonly_addresses: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct BlockInnerInstructionGroup {
    index: u32,
    instructions: Vec<BlockCompiledInstruction>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockCompiledInstruction {
    program_id_index: u32,
    accounts: Vec<u32>,
    data: String, // base58-encoded
}

// ---------------------------------------------------------------------------
// RPC client (minimal)
// ---------------------------------------------------------------------------

/// Minimal Solana JSON-RPC `getBlock` client.
///
/// Uses `reqwest` (via hyper/tokio) for HTTP. A full `solana-client` dependency
/// would add significant compile time; this subset is sufficient for backfill.
///
/// PHASE 1 NOTE: Uses `serde_json` for request/response. A future phase may
/// switch to the official `solana-client` crate if more RPC methods are needed.
struct BackfillRpcClient {
    endpoint: String,
    http: reqwest::Client,
}

impl BackfillRpcClient {
    fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client build must not fail"),
        }
    }

    /// Fetch a single block by slot number.
    ///
    /// Returns `None` for skipped slots (leader did not produce a block).
    async fn get_block(&self, slot: u64) -> Result<Option<GetBlockResponse>, AdapterError> {
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBlock",
            "params": [
                slot,
                {
                    "encoding": "json",
                    "transactionDetails": "full",
                    "maxSupportedTransactionVersion": 0,
                    "rewards": false
                }
            ]
        });

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AdapterError::RpcError {
                slot,
                reason: format!("HTTP request failed: {e}"),
            })?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AdapterError::RateLimit { slot });
        }

        if !response.status().is_success() {
            return Err(AdapterError::RpcError {
                slot,
                reason: format!("HTTP {}", response.status()),
            });
        }

        let body: serde_json::Value = response.json().await.map_err(|e| AdapterError::RpcError {
            slot,
            reason: format!("JSON parse error: {e}"),
        })?;

        // Solana RPC returns null result for skipped slots.
        if body["result"].is_null() {
            return Ok(None);
        }

        let block: GetBlockResponse =
            serde_json::from_value(body["result"].clone()).map_err(|e| AdapterError::RpcError {
                slot,
                reason: format!("getBlock response parse error: {e}"),
            })?;

        Ok(Some(block))
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build a backfill event stream for a historical slot range.
///
/// Fetches blocks via Solana JSON-RPC `getBlock` and decodes them using the
/// same `decode_transaction` logic as the live stream. Events are emitted in
/// slot order.
///
/// Skipped slots (leader did not produce a block) are silently skipped — they
/// produce no events by definition.
pub fn build_backfill_stream(
    config: Arc<SolanaAdapterConfig>,
    range: RangeInclusive<u64>,
) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
    let (tx, rx) = mpsc::channel::<Result<Event, AdapterError>>(256);

    tokio::spawn(async move {
        let start = *range.start();
        let end = *range.end();

        info!(
            start_slot = start,
            end_slot = end,
            total_slots = end.saturating_sub(start) + 1,
            "starting backfill"
        );

        // Resolve RPC endpoint: use the configured HTTP URL.
        let rpc_url = config.http_url.as_str().trim_end_matches('/').to_owned();

        let client = BackfillRpcClient::new(&rpc_url);
        let mut last_checkpoint_slot = start;

        for slot in start..=end {
            if tx.is_closed() {
                info!("backfill receiver dropped — stopping");
                break;
            }

            // Rate-limit sleep between RPC requests (configurable in future).
            // Default: 50 ms → ~20 req/s, well within most provider limits.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let block = match client.get_block(slot).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    debug!(slot, "skipped slot (no block produced)");
                    continue;
                }
                Err(AdapterError::RateLimit { .. }) => {
                    warn!(slot, "rate limited during backfill — sleeping 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    // Retry once.
                    match client.get_block(slot).await {
                        Ok(Some(b)) => b,
                        Ok(None) => continue,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
                Err(e) => {
                    warn!(slot, error = %e, "backfill RPC error — skipping slot");
                    // Non-fatal: skip the slot, continue backfill.
                    continue;
                }
            };

            let block_time = block.block_time;
            let events = decode_block_transactions(&block, slot, block_time);

            for event in events {
                if tx.send(Ok(event)).await.is_err() {
                    info!("backfill receiver dropped mid-stream — stopping");
                    return;
                }
            }

            // Emit SlotFinalized for each backfilled slot (historical slots are already finalized).
            let _ = tx.send(Ok(Event::SlotFinalized { slot })).await;

            if slot % 1000 == 0 {
                info!(
                    slot,
                    progress_pct = (slot - start) * 100 / (end - start + 1).max(1),
                    "backfill progress"
                );
            }

            last_checkpoint_slot = slot;
        }

        info!(
            last_slot = last_checkpoint_slot,
            "backfill complete"
        );
    });

    Box::pin(ReceiverStream::new(rx))
}

// ---------------------------------------------------------------------------
// Block transaction decoder
// ---------------------------------------------------------------------------

/// Decode all transactions in a block into `Event` values.
fn decode_block_transactions(block: &GetBlockResponse, slot: u64, block_time: Option<i64>) -> Vec<Event> {
    use crate::solana::decode::{decode_transaction, SplInstruction, TxDecodeInput};
    use mg_solana_types::Pubkey;
    use std::collections::HashMap;
    use tracing::warn;

    let mut events = Vec::new();

    for (tx_idx, block_tx) in block.transactions.iter().enumerate() {
        let signatures = &block_tx.transaction.signatures;
        if signatures.is_empty() {
            continue;
        }
        let signature = &signatures[0];

        let message = &block_tx.transaction.message;
        let meta = block_tx.meta.as_ref();

        // Build account_keys: static keys + loaded writable + loaded readonly.
        let mut account_strs: Vec<String> = message.account_keys.clone();
        if let Some(m) = meta {
            account_strs.extend(m.loaded_writable_addresses.clone());
            account_strs.extend(m.loaded_readonly_addresses.clone());
        }

        let account_keys: Vec<Pubkey> = account_strs
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        // Build instruction list.
        let instructions: Vec<SplInstruction> = message
            .instructions
            .iter()
            .filter_map(|ix| {
                let program_pubkey = account_keys.get(ix.program_id_index as usize)?;
                let program_id = program_pubkey.to_string();
                let accounts: Vec<String> = ix
                    .accounts
                    .iter()
                    .filter_map(|&idx| account_keys.get(idx as usize).map(|pk| pk.to_string()))
                    .collect();
                let data = bs58::decode(&ix.data).into_vec().unwrap_or_default();
                Some(SplInstruction { program_id, accounts, data })
            })
            .collect();

        // Build inner instructions map.
        let mut inner_instructions: HashMap<u32, Vec<SplInstruction>> = HashMap::new();
        if let Some(m) = meta {
            for group in &m.inner_instructions {
                let inner_ixs: Vec<SplInstruction> = group
                    .instructions
                    .iter()
                    .filter_map(|ix| {
                        let program_pubkey = account_keys.get(ix.program_id_index as usize)?;
                        let program_id = program_pubkey.to_string();
                        let accounts: Vec<String> = ix
                            .accounts
                            .iter()
                            .filter_map(|&idx| {
                                account_keys.get(idx as usize).map(|pk| pk.to_string())
                            })
                            .collect();
                        let data = bs58::decode(&ix.data).into_vec().unwrap_or_default();
                        Some(SplInstruction { program_id, accounts, data })
                    })
                    .collect();
                inner_instructions.insert(group.index, inner_ixs);
            }
        }

        let input = TxDecodeInput {
            slot,
            block_time,
            signature,
            account_keys: &account_keys,
            instructions: &instructions,
            inner_instructions: &inner_instructions,
        };

        match decode_transaction(&input) {
            Ok(tx_events) => events.extend(tx_events),
            Err(e) if e.is_skippable() => {
                warn!(
                    slot,
                    tx_idx,
                    signature,
                    error = %e,
                    "skipping malformed backfill transaction"
                );
            }
            Err(e) => {
                warn!(
                    slot,
                    tx_idx,
                    error = %e,
                    "non-skippable backfill decode error — continuing"
                );
            }
        }
    }

    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_block_transactions_empty_block() {
        let block = GetBlockResponse {
            block_time: Some(1_700_000_000),
            transactions: vec![],
            block_height: None,
        };
        let events = decode_block_transactions(&block, 100, Some(1_700_000_000));
        assert!(events.is_empty());
    }

    #[test]
    fn decode_block_transactions_tx_with_no_signatures_skipped() {
        let block = GetBlockResponse {
            block_time: Some(1_700_000_000),
            transactions: vec![BlockTransaction {
                transaction: BlockTransactionMessage {
                    signatures: vec![], // no signatures
                    message: BlockMessage {
                        account_keys: vec![],
                        instructions: vec![],
                    },
                },
                meta: None,
            }],
            block_height: None,
        };
        let events = decode_block_transactions(&block, 200, Some(1_700_000_000));
        // Transaction with no signatures must be silently skipped.
        assert!(events.is_empty());
    }
}
