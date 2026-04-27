//! Yellowstone gRPC subscribe stream: request builder + stream wiring.
//!
//! # Stream lifecycle
//!
//! ```text
//! SolanaAdapter::subscribe(filter)
//!   └─ build_subscribe_request(filter, config)
//!   └─ connect_and_subscribe(config)
//!   └─ reconnect loop (reconnect.rs)
//!       └─ GeyserGrpcClient::subscribe_once(request)
//!           └─ process_update(update)  ← decode.rs
//!               → Event::Transfer / Event::Swap / Event::PoolEvent
//!               → Event::ReorgMarker   (slot skipped/dead)
//!               → Event::SlotFinalized (slot status = FINALIZED)
//! ```
//!
//! # Reorg handling
//!
//! Yellowstone delivers `SubscribeUpdateSlot` messages with a `SlotStatus` field.
//! The adapter tracks the most recent `confirmed` slot. When a slot transitions to
//! `dead` (or is never seen as `finalized` within the reorg window), the adapter
//! emits `Event::ReorgMarker { slot }` so consumers can evict buffered events.
//!
//! State machine per slot:
//! - `SLOT_PROCESSED` → seen (fast, unstable)
//! - `SLOT_CONFIRMED` → buffer events (hot path, ≈2 slots from processed tip)
//! - `SLOT_FINALIZED` → emit `SlotFinalized`, events are immutable
//! - `SLOT_DEAD` → emit `ReorgMarker`, consumers must evict
//!
//! Slots that are `SLOT_CONFIRMED` but never `SLOT_FINALIZED` within the configurable
//! confirmation window indicate a reorg. The adapter does NOT track this window
//! internally in Phase 1 — it relies on the Yellowstone stream delivering `SLOT_DEAD`
//! or the absence of `SLOT_FINALIZED`. The indexer (Task after this) implements the
//! full confirmation-window logic.
//!
//! # Provider auth
//!
//! Both Helius and Triton use the `x-token` gRPC metadata mechanism via
//! `GeyserGrpcBuilder::x_token(token)`. Helius also accepts this via the
//! `x-api-key` HTTP header, but the gRPC metadata path is provider-agnostic
//! and works for both. Self-hosted validators typically need no token.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, SlotStatus, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions,
};

use crate::{
    error::AdapterError,
    solana::{
        config::SolanaAdapterConfig,
        decode::{decode_transaction, SplInstruction, TxDecodeInput},
    },
    Event, SubscribeFilter,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build a live event stream from the Yellowstone gRPC subscription.
///
/// Spawns a background tokio task that:
/// 1. Connects to the configured gRPC endpoint.
/// 2. Sends a `SubscribeRequest` built from `filter`.
/// 3. Decodes each `SubscribeUpdate` into `Event` values.
/// 4. Reconnects automatically on disconnect (via `reconnect.rs`).
///
/// The returned `Stream` yields `Result<Event, AdapterError>`. The stream
/// terminates only when the sender side is dropped (on fatal unrecoverable error)
/// or when the caller drops the receiver.
///
/// The `resume_slot` parameter is set as `from_slot` in the `SubscribeRequest`
/// so the server replays any events since the last checkpoint. Pass `None` to
/// start from the current tip.
pub fn build_subscribe_stream(
    config: Arc<SolanaAdapterConfig>,
    filter: SubscribeFilter,
    resume_slot: Option<u64>,
) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
    let (tx, rx) = mpsc::channel::<Result<Event, AdapterError>>(1024);

    tokio::spawn(async move {
        info!(
            endpoint = %config.endpoint,
            commitment = ?config.commitment,
            resume_slot = ?resume_slot,
            "starting Yellowstone gRPC subscribe stream"
        );

        let mut consecutive_errors = 0u32;

        loop {
            match connect_and_stream(&config, &filter, resume_slot, &tx).await {
                Ok(()) => {
                    // Stream ended cleanly (provider closed) — treat as reconnectable.
                    warn!("gRPC stream ended cleanly — reconnecting");
                    consecutive_errors += 1;
                }
                Err(e) if e.is_reconnectable() => {
                    warn!(error = %e, consecutive_errors, "gRPC stream error — reconnecting");
                    consecutive_errors += 1;
                }
                Err(e) => {
                    error!(error = %e, "fatal gRPC error — terminating subscribe stream");
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

            // Compute and apply reconnect delay.
            let delay_ms = (config.reconnect.base_delay_ms as u128)
                .saturating_mul(1u128 << consecutive_errors.min(30))
                .min(config.reconnect.max_delay_ms as u128) as u64;

            info!(delay_ms, "waiting before reconnect");
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    });

    Box::pin(ReceiverStream::new(rx))
}

// ---------------------------------------------------------------------------
// Connection + stream processing
// ---------------------------------------------------------------------------

/// Connect to the Yellowstone gRPC endpoint and stream events until the connection drops.
async fn connect_and_stream(
    config: &SolanaAdapterConfig,
    filter: &SubscribeFilter,
    resume_slot: Option<u64>,
    tx: &mpsc::Sender<Result<Event, AdapterError>>,
) -> Result<(), AdapterError> {
    let endpoint_str = config.endpoint.as_str().trim_end_matches('/');

    // Build the gRPC client.
    let mut client_builder = GeyserGrpcClient::build_from_shared(endpoint_str.to_owned())
        .map_err(|e| AdapterError::Config(format!("invalid endpoint '{}': {e}", config.endpoint)))?;

    // Auth token — works for both Helius (x-api-key) and Triton (x-token).
    // x_token() takes Option<T> — pass None to skip auth for self-hosted.
    client_builder = client_builder
        .x_token(config.auth_token.clone())
        .map_err(|e| AdapterError::Config(format!("failed to set auth token: {e}")))?;

    let mut client = client_builder
        .connect()
        .await
        .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

    info!(endpoint = endpoint_str, "connected to Yellowstone gRPC");

    // Build the subscribe request.
    let request = build_subscribe_request(filter, &config.commitment, resume_slot);

    let mut stream = client
        .subscribe_once(request)
        .await
        .map_err(|e| AdapterError::GrpcClient(e.to_string()))?;

    // Process incoming updates.
    while let Some(update_result) = stream.next().await {
        let update = update_result.map_err(AdapterError::Transport)?;

        match update.update_oneof {
            Some(UpdateOneof::Transaction(tx_update)) => {
                match process_transaction_update(tx_update, update.created_at.as_ref()) {
                    Ok(events) => {
                        for event in events {
                            if tx.send(Ok(event)).await.is_err() {
                                // Receiver dropped — caller closed the stream.
                                info!("subscribe stream receiver dropped — stopping");
                                return Ok(());
                            }
                        }
                    }
                    Err(e) if e.is_skippable() => {
                        debug!(error = %e, "skipping malformed transaction update");
                    }
                    Err(e) => {
                        warn!(error = %e, "non-skippable decode error in transaction update");
                    }
                }
            }

            Some(UpdateOneof::Slot(slot_update)) => {
                process_slot_update(&slot_update, tx).await;
            }

            Some(UpdateOneof::Ping(_)) => {
                debug!("received ping from Yellowstone — stream healthy");
            }

            Some(UpdateOneof::Pong(_)) => {}

            Some(UpdateOneof::Account(_account_update)) => {
                // Account updates are used by token-registry for metadata enrichment.
                // Phase 1: log only. Token-registry will consume these in Phase 2.
                debug!("account update received (token-registry enrichment deferred to Phase 2)");
            }

            Some(UpdateOneof::BlockMeta(_)) | Some(UpdateOneof::Block(_)) | Some(UpdateOneof::Entry(_)) => {
                // Not subscribed to block/entry updates in our filter.
            }

            Some(UpdateOneof::TransactionStatus(_)) => {}

            None => {
                warn!("received empty update_oneof from Yellowstone");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SubscribeRequest builder
// ---------------------------------------------------------------------------

/// Build a Yellowstone `SubscribeRequest` from a `SubscribeFilter` and config.
///
/// The request encodes:
/// - Transaction filter: include transactions that involve any of the listed program IDs.
/// - Account filter: stream account updates for accounts owned by SPL Token / Token-2022.
/// - Slot updates: for reorg detection (included when `filter.include_slot_updates`).
/// - Commitment level: from config.
/// - from_slot: resume position (if provided).
pub fn build_subscribe_request(
    filter: &SubscribeFilter,
    commitment: &crate::solana::config::CommitmentConfig,
    resume_slot: Option<u64>,
) -> SubscribeRequest {
    let mut request = SubscribeRequest {
        commitment: Some(commitment.to_proto() as i32),
        from_slot: resume_slot,
        ..Default::default()
    };

    // Transaction filter: include txs that involve our target programs.
    // `account_include` = any transaction that references these accounts in its account list.
    if !filter.program_ids.is_empty() {
        request.transactions.insert(
            "spl_and_dex".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: filter.program_ids.clone(),
                account_exclude: vec![],
                account_required: vec![],
                signature: None,
            },
        );
    }

    // Account filter: stream account state changes for token accounts.
    if !filter.account_owners.is_empty() {
        request.accounts.insert(
            "token_accounts".to_string(),
            SubscribeRequestFilterAccounts {
                owner: filter.account_owners.clone(),
                account: vec![],
                filters: vec![],
                nonempty_txn_signature: Some(true),
            },
        );
    }

    // Slot updates: required for reorg detection.
    if filter.include_slot_updates {
        request.slots.insert(
            "all_slots".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(false), // receive all slot statuses
                interslot_updates: Some(false),
            },
        );
    }

    request
}

// ---------------------------------------------------------------------------
// Update processors
// ---------------------------------------------------------------------------

/// Process a `SubscribeUpdateTransaction` → zero or more `Event` values.
fn process_transaction_update(
    tx_update: yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    _created_at: Option<&yellowstone_grpc_proto::prost_types::Timestamp>,
) -> Result<Vec<Event>, AdapterError> {
    let tx_info = tx_update.transaction.ok_or(AdapterError::MissingField {
        field: "transaction",
        context: "SubscribeUpdateTransaction",
    })?;

    let signature_bytes = &tx_info.signature;
    if signature_bytes.len() != 64 {
        return Err(AdapterError::DecodeError {
            context: "SubscribeUpdateTransaction.signature",
            reason: format!("expected 64 bytes, got {}", signature_bytes.len()),
        });
    }

    let signature_b58 = bs58::encode(signature_bytes).into_string();

    let transaction = tx_info.transaction.ok_or(AdapterError::MissingField {
        field: "transaction.transaction",
        context: "SubscribeUpdateTransactionInfo",
    })?;

    let meta = tx_info.meta.ok_or(AdapterError::MissingField {
        field: "transaction.meta",
        context: "SubscribeUpdateTransactionInfo",
    })?;

    // Extract block_time from the slot — Yellowstone provides it in slot updates,
    // not directly in transaction updates. We use `None` here; the slot update
    // stream populates block_time separately. In practice, downstream consumers
    // use the block_time from `BlockRef` + a separate slot→time index.
    let block_time: Option<i64> = None;

    // Resolve account keys from the transaction message.
    let message = transaction.message.ok_or(AdapterError::MissingField {
        field: "transaction.message",
        context: "SubscribeUpdateTransaction",
    })?;

    let slot = tx_update.slot;

    // Collect all account pubkeys: static keys + loaded writable + loaded readonly.
    let mut account_keys: Vec<solana_sdk::pubkey::Pubkey> = message
        .account_keys
        .iter()
        .map(|k| {
            let mut arr = [0u8; 32];
            let len = k.len().min(32);
            arr[..len].copy_from_slice(&k[..len]);
            solana_sdk::pubkey::Pubkey::from(arr)
        })
        .collect();

    // Loaded addresses (from address lookup tables)
    for k in &meta.loaded_writable_addresses {
        let mut arr = [0u8; 32];
        let len = k.len().min(32);
        arr[..len].copy_from_slice(&k[..len]);
        account_keys.push(solana_sdk::pubkey::Pubkey::from(arr));
    }
    for k in &meta.loaded_readonly_addresses {
        let mut arr = [0u8; 32];
        let len = k.len().min(32);
        arr[..len].copy_from_slice(&k[..len]);
        account_keys.push(solana_sdk::pubkey::Pubkey::from(arr));
    }

    // Build simplified SplInstruction list from proto compiled instructions.
    let instructions: Vec<SplInstruction> = message
        .instructions
        .iter()
        .filter_map(|ix| {
            let program_idx = ix.program_id_index as usize;
            let program_pubkey = account_keys.get(program_idx)?;
            let program_id = program_pubkey.to_string();

            let accounts: Vec<String> = ix
                .accounts
                .iter()
                .filter_map(|&acct_idx| {
                    account_keys.get(acct_idx as usize).map(|pk| pk.to_string())
                })
                .collect();

            Some(SplInstruction {
                program_id,
                accounts,
                data: ix.data.clone(),
            })
        })
        .collect();

    // Build inner instructions map: outer_ix_index → Vec<SplInstruction>.
    let mut inner_instructions: HashMap<u32, Vec<SplInstruction>> = HashMap::new();
    for inner_group in &meta.inner_instructions {
        let outer_idx = inner_group.index;
        let inner_ixs: Vec<SplInstruction> = inner_group
            .instructions
            .iter()
            .filter_map(|ix| {
                // Inner instructions use program_id_index into the same account_keys.
                let program_idx = ix.program_id_index as usize;
                let program_pubkey = account_keys.get(program_idx)?;
                let program_id = program_pubkey.to_string();

                let accounts: Vec<String> = ix
                    .accounts
                    .iter()
                    .filter_map(|&acct_idx| {
                        account_keys.get(acct_idx as usize).map(|pk| pk.to_string())
                    })
                    .collect();

                Some(SplInstruction {
                    program_id,
                    accounts,
                    data: ix.data.clone(),
                })
            })
            .collect();

        inner_instructions.insert(outer_idx, inner_ixs);
    }

    let input = TxDecodeInput {
        slot,
        block_time,
        signature: &signature_b58,
        account_keys: &account_keys,
        instructions: &instructions,
        inner_instructions: &inner_instructions,
    };

    decode_transaction(&input)
}

/// Process a `SubscribeUpdateSlot` and send reorg / finalization markers to the channel.
async fn process_slot_update(
    slot_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateSlot,
    tx: &mpsc::Sender<Result<Event, AdapterError>>,
) {
    let slot = slot_update.slot;
    let status = slot_update.status;

    if status == SlotStatus::SlotFinalized as i32 {
        debug!(slot, "slot finalized");
        let _ = tx.send(Ok(Event::SlotFinalized { slot })).await;
    } else if status == SlotStatus::SlotDead as i32 {
        warn!(
            slot,
            dead_error = ?slot_update.dead_error,
            "slot dead — emitting ReorgMarker"
        );
        let _ = tx.send(Ok(Event::ReorgMarker { slot })).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::config::CommitmentConfig;

    fn default_filter() -> SubscribeFilter {
        SubscribeFilter::solana_default()
    }

    // --- build_subscribe_request ---

    #[test]
    fn build_request_includes_transaction_filter() {
        let filter = default_filter();
        let req = build_subscribe_request(&filter, &CommitmentConfig::Confirmed, None);

        assert!(
            req.transactions.contains_key("spl_and_dex"),
            "must include transaction filter"
        );
        let tx_filter = &req.transactions["spl_and_dex"];
        assert_eq!(tx_filter.vote, Some(false), "must exclude vote transactions");
        assert_eq!(tx_filter.failed, Some(false), "must exclude failed transactions");
        assert!(
            tx_filter.account_include.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
            "must include SPL Token program"
        );
    }

    #[test]
    fn build_request_includes_account_filter() {
        let filter = default_filter();
        let req = build_subscribe_request(&filter, &CommitmentConfig::Confirmed, None);

        assert!(
            req.accounts.contains_key("token_accounts"),
            "must include account filter"
        );
        let acct_filter = &req.accounts["token_accounts"];
        assert!(
            acct_filter.owner.contains(&"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
        );
    }

    #[test]
    fn build_request_includes_slot_filter_when_enabled() {
        let filter = SubscribeFilter { include_slot_updates: true, ..Default::default() };
        let req = build_subscribe_request(&filter, &CommitmentConfig::Confirmed, None);
        assert!(req.slots.contains_key("all_slots"));
    }

    #[test]
    fn build_request_no_slot_filter_when_disabled() {
        let filter = SubscribeFilter { include_slot_updates: false, ..Default::default() };
        let req = build_subscribe_request(&filter, &CommitmentConfig::Confirmed, None);
        assert!(req.slots.is_empty(), "slot filter must be absent when disabled");
    }

    #[test]
    fn build_request_sets_commitment_confirmed() {
        let req = build_subscribe_request(
            &default_filter(),
            &CommitmentConfig::Confirmed,
            None,
        );
        use yellowstone_grpc_proto::geyser::CommitmentLevel;
        assert_eq!(req.commitment, Some(CommitmentLevel::Confirmed as i32));
    }

    #[test]
    fn build_request_sets_from_slot() {
        let req = build_subscribe_request(
            &default_filter(),
            &CommitmentConfig::Confirmed,
            Some(12345),
        );
        assert_eq!(req.from_slot, Some(12345));
    }

    #[test]
    fn build_request_no_from_slot_when_none() {
        let req = build_subscribe_request(
            &default_filter(),
            &CommitmentConfig::Confirmed,
            None,
        );
        assert!(req.from_slot.is_none());
    }

    #[test]
    fn build_request_empty_filter_no_tx_filter() {
        let filter = SubscribeFilter {
            program_ids: vec![],
            account_owners: vec![],
            include_slot_updates: false,
            evm_contract_addresses: vec![],
        };
        let req = build_subscribe_request(&filter, &CommitmentConfig::Confirmed, None);
        assert!(req.transactions.is_empty());
        assert!(req.accounts.is_empty());
        assert!(req.slots.is_empty());
    }
}
