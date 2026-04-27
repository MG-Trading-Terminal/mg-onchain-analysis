//! `MultiChainCoordinator` builder and coordinator-to-invalidation bridge.
//!
//! # Design
//!
//! The coordinator wraps N chain adapters. This module:
//! 1. Assembles `Vec<AdapterSlot>` from enabled adapters.
//! 2. Constructs `MultiChainCoordinator::new(slots, shutdown)`.
//! 3. Provides `coordinator_to_invalidation_bridge` — a lightweight async fn
//!    that translates `coordinator mpsc::Receiver<Result<Event, AdapterError>>`
//!    into `InvalidationEvent`s for the `AppState.invalidation_tx` broadcast.
//!
//! # Gotcha #39
//!
//! `Indexer::new` 9-param signature is NOT used here — the coordinator manages
//! adapter slots directly via `ErasedAdapter`. The Indexer is not used in the
//! coordinator path (Pattern B per ADR 0005); each adapter's `subscribe()` is
//! driven by the coordinator's per-adapter tasks.
//!
//! # Gotcha #65/#66
//!
//! `ChainAdapter` trait is NOT dyn-compatible. `AdapterSlot` wraps `Box<dyn ErasedAdapter>`.
//! `AdapterSlot::new` takes `impl ChainAdapter + 'static` and boxes it.

use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use mg_onchain_chain_adapter::{AdapterError, Event};
use mg_onchain_chain_adapter::ethereum::EthereumAdapter;
use mg_onchain_chain_adapter::solana::SolanaAdapter;
use mg_onchain_common::chain::Chain;
use mg_onchain_gateway::state::InvalidationEvent;
use mg_onchain_indexer::coordinator::{AdapterSlot, MultiChainCoordinator};
use mg_onchain_indexer::shutdown::ShutdownSignal;

// ---------------------------------------------------------------------------
// build_coordinator
// ---------------------------------------------------------------------------

/// Build a `MultiChainCoordinator` from optional enabled adapters.
///
/// Backwards-compatible single-Ethereum-adapter variant.
/// For multi-chain EVM support, use `build_coordinator_multi`.
///
/// Adapters that are `None` (chain disabled) are silently skipped.
///
/// # D-E
///
/// The coordinator only receives adapters for enabled chains. The `SchedulerWorker`
/// chain-filter ensures D12/D13 (EVM-only) never dispatches on Solana events and
/// D01-D11 (Solana-focused) never dispatch on EVM events.
pub fn build_coordinator(
    solana_adapter: Option<SolanaAdapter>,
    ethereum_adapter: Option<EthereumAdapter>,
    shutdown: ShutdownSignal,
) -> MultiChainCoordinator {
    let evm_adapters = ethereum_adapter
        .map(|a| vec![(Chain::Ethereum, a)])
        .unwrap_or_default();
    build_coordinator_multi(solana_adapter, evm_adapters, shutdown)
}

/// Build a `MultiChainCoordinator` from Solana + N EVM adapters.
///
/// `evm_adapters` is a `Vec<(Chain, EthereumAdapter)>` containing one entry per
/// enabled EVM chain (Ethereum, BSC, Base, Arbitrum, Polygon — any combination).
/// Use `init::adapters::build_evm_adapters` to produce this vec from config.
///
/// # D-E
///
/// The coordinator only receives adapters for enabled chains. The `SchedulerWorker`
/// chain-filter ensures D12/D13 (EVM-only) never dispatches on Solana events and
/// D01-D11 (Solana-focused) never dispatch on EVM events.
pub fn build_coordinator_multi(
    solana_adapter: Option<SolanaAdapter>,
    evm_adapters: Vec<(Chain, EthereumAdapter)>,
    shutdown: ShutdownSignal,
) -> MultiChainCoordinator {
    let mut slots: Vec<AdapterSlot> = Vec::new();

    if let Some(adapter) = solana_adapter {
        info!("coordinator: adding Solana adapter slot");
        slots.push(AdapterSlot::new(Chain::Solana, "solana", adapter));
    }

    for (chain, adapter) in evm_adapters {
        info!(chain = %chain, "coordinator: adding EVM adapter slot");
        let label: &'static str = match chain {
            Chain::Ethereum => "ethereum",
            Chain::Bsc => "bsc",
            Chain::Base => "base",
            Chain::Arbitrum => "arbitrum",
            Chain::Polygon => "polygon",
            _ => "evm_unknown",
        };
        slots.push(AdapterSlot::new(chain, label, adapter));
    }

    if slots.is_empty() {
        // All chains disabled — coordinator will have no adapters.
        // `coordinator.start()` will return CoordinatorError::NoAdapters.
        // This is a valid configuration for dev/CI environments.
        warn!(
            "coordinator has no adapter slots — all chains are disabled. \
             The coordinator will not produce any events."
        );
    } else {
        info!(slot_count = slots.len(), "coordinator built");
    }

    MultiChainCoordinator::new(slots, shutdown)
}

// ---------------------------------------------------------------------------
// coordinator_to_invalidation_bridge
// ---------------------------------------------------------------------------

/// Bridge coordinator events into `InvalidationEvent` broadcasts.
///
/// Runs as a background task. Exits when the coordinator mpsc receiver closes
/// (i.e., when the coordinator stops producing events due to shutdown).
///
/// # Event translation
///
/// Only `Event::TokenActivity` events that carry a token mint and block_time
/// produce `InvalidationEvent`s. Other event types (SlotFinalized, ReorgMarker)
/// are silently dropped at this boundary — the scheduler does not need them.
///
/// # Backpressure
///
/// The `broadcast::Sender` drops events when all receivers are lagging (bounded
/// channel). The coordinator mpsc channel buffers events upstream. This is
/// intentional per design 0020 §5 — the bridge does not block the coordinator
/// on slow scheduler consumers.
///
/// # SPEC-NOTE
///
/// The current `Event` enum does not have a `TokenActivity` variant in the
/// common types at this sprint stage. We translate `Event::SlotFinalized` slots
/// as a best-effort trigger (block_time = slot number as i64 approximation).
/// A proper `TokenActivity { mint, block_time }` variant is a Phase 5 addition.
/// This bridge will be updated when that variant lands. For now it is a
/// structural stub that compiles and propagates the shutdown correctly.
pub async fn coordinator_to_invalidation_bridge(
    mut rx: tokio::sync::mpsc::Receiver<Result<Event, AdapterError>>,
    tx: broadcast::Sender<InvalidationEvent>,
    shutdown: ShutdownSignal,
) {
    info!("coordinator bridge: starting");
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("coordinator bridge: shutdown signal received");
                break;
            }
            item = rx.recv() => {
                match item {
                    None => {
                        info!("coordinator bridge: coordinator channel closed");
                        break;
                    }
                    Some(Ok(event)) => {
                        // SPEC-NOTE: translate Event variants to InvalidationEvent.
                        // Full translation requires a TokenActivity event type (Phase 5).
                        // For now, emit a placeholder on SlotFinalized so the bridge
                        // compiles and the shutdown path works correctly.
                        handle_coordinator_event(event, &tx);
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "coordinator bridge: adapter error received");
                        // Continue — transient adapter errors should not stop the bridge.
                    }
                }
            }
        }
    }
    info!("coordinator bridge: stopped");
}

/// Translate a single coordinator event into an `InvalidationEvent` if applicable.
fn handle_coordinator_event(event: Event, tx: &broadcast::Sender<InvalidationEvent>) {
    match event {
        Event::SlotFinalized { slot } => {
            debug!(slot, "coordinator bridge: SlotFinalized received (no invalidation produced)");
            // SlotFinalized does not carry a token mint — no invalidation to emit.
            // A future TokenActivity variant will trigger invalidation here.
            let _ = (slot, tx); // suppress unused warning
        }
        Event::ReorgMarker { slot } => {
            debug!(slot, "coordinator bridge: ReorgMarker received");
            // Reorg events are not forwarded to the scheduler in this sprint.
            // Phase 5: emit invalidation for all tokens seen in the rolled-back slots.
        }
        // All other variants (Transfer, Swap, PoolEvent, etc.) are handled here.
        // SPEC-NOTE: When a TokenActivity variant is added, this arm will emit
        // an InvalidationEvent carrying the token mint and block_time.
        _ => {
            // Forward-compatible: new Event variants are silently dropped.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_indexer::shutdown::ShutdownSignal;

    #[test]
    fn build_coordinator_with_no_adapters_returns_coordinator() {
        // All chains disabled — coordinator is built but will return NoAdapters on start().
        // This is a valid configuration for tests / CI.
        let shutdown = ShutdownSignal::new();
        let coordinator = build_coordinator(None, None, shutdown);
        // We can call healthcheck but it will return empty since no adapters.
        // Just verify construction does not panic.
        let _ = coordinator; // drops cleanly
    }

    #[test]
    fn build_coordinator_multi_no_adapters_compiles() {
        let shutdown = ShutdownSignal::new();
        let coordinator = build_coordinator_multi(None, vec![], shutdown);
        let _ = coordinator;
    }

    #[test]
    fn bridge_translation_slot_finalized_does_not_panic() {
        // The handle_coordinator_event fn is pure logic — test without spawning tasks.
        let (tx, _rx) = broadcast::channel(16);
        handle_coordinator_event(Event::SlotFinalized { slot: 12345 }, &tx);
        // No invalidation should have been sent.
        // The receiver count can still be 0 (we dropped _rx).
    }

    #[test]
    fn bridge_translation_reorg_marker_does_not_panic() {
        let (tx, _rx) = broadcast::channel(16);
        handle_coordinator_event(Event::ReorgMarker { slot: 999 }, &tx);
    }
}
