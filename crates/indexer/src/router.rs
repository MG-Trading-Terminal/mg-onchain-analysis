//! Event → batcher dispatcher (router).
//!
//! `route_event` is a pure function: it takes an `Event` and mutably borrows the
//! `EventBatcher`, dispatching the event to the correct per-table buffer.
//!
//! # Responsibility boundary
//!
//! The router does NOT call storage, does NOT decide when to flush, and does NOT
//! manage checkpoints. It is the narrow "sort incoming events into buffers" step.
//!
//! # Unknown variants
//!
//! `Event` is `#[non_exhaustive]`. Any variant not matched here goes to the
//! batcher's `unknown_event_count` counter and is logged at WARN. This is
//! intentional: new chain-adapter event types should not crash the indexer;
//! they should surface as a metric and a log line until the router is updated.
//!
//! # TokenMeta handling
//!
//! `Event::TokenMeta` is low-volume (one per newly-seen mint) and does not go
//! through the batcher. Instead, `route_event` returns a `RouteResult` that
//! signals "upsert this token metadata" to the caller, which handles it
//! synchronously via `pg.upsert_token()`. This avoids batching infrastructure
//! for an event type that arrives at most once per token.

use mg_onchain_chain_adapter::Event;

use crate::batcher::EventBatcher;

// ---------------------------------------------------------------------------
// RouteResult — signals to the caller what additional action is needed
// ---------------------------------------------------------------------------

/// The router's side-channel output alongside the batcher push.
///
/// Most events yield `None` (the batcher push is sufficient). Special variants
/// return structured actions that the caller must handle out-of-band.
#[derive(Debug)]
pub enum RouteResult {
    /// Event was dispatched to the batcher — no further action needed.
    Buffered,

    /// A `TokenMeta` event was extracted. The caller should upsert this
    /// to `tokens` immediately (low volume, no batching needed).
    TokenMeta(Box<mg_onchain_common::token::TokenMeta>),

    /// A `ReorgMarker` was received. The caller must:
    /// 1. Flush all in-flight buffers (commit pending events).
    /// 2. Issue DELETE for block_height >= `slot`.
    /// 3. Rewind checkpoint to `slot - 1`.
    Reorg { slot: u64 },

    /// A `SlotFinalized` was received. The caller may use this to trigger a
    /// flush of events up to this slot. Currently treated like `Buffered` —
    /// the timeout / size trigger handles flushing.
    SlotFinalized { slot: u64 },

    /// Event variant was not recognised (non-exhaustive match).
    /// The batcher's `unknown_event_count` has already been incremented.
    Unknown,
}

// ---------------------------------------------------------------------------
// route_event
// ---------------------------------------------------------------------------

/// Dispatch one `Event` to the correct batcher buffer.
///
/// Returns a `RouteResult` describing any additional action the caller must take.
///
/// This function is pure (no I/O, no allocation beyond the event push) and
/// testable without a real adapter or storage.
pub fn route_event(event: Event, batcher: &mut EventBatcher) -> RouteResult {
    match event {
        Event::Transfer(t) => {
            batcher.push_transfer(t);
            RouteResult::Buffered
        }
        Event::Swap(s) => {
            batcher.push_swap(s);
            RouteResult::Buffered
        }
        Event::PoolEvent(e) => {
            batcher.push_pool_event(e);
            RouteResult::Buffered
        }
        Event::TokenMeta(meta) => RouteResult::TokenMeta(meta),
        Event::ReorgMarker { slot } => RouteResult::Reorg { slot },
        Event::SlotFinalized { slot } => RouteResult::SlotFinalized { slot },
        // `#[non_exhaustive]` catch-all: new chain-adapter variants land here
        // until this router is updated. Logged at WARN by `count_unknown`.
        _ => {
            batcher.count_unknown();
            RouteResult::Unknown
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bs58;
    use chrono::Utc;
    use mg_onchain_chain_adapter::Event;
    use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
    use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind, Swap, Transfer};

    fn chain() -> Chain {
        Chain::Solana
    }

    fn addr(s: &str) -> Address {
        Address::parse(Chain::Solana, s).unwrap()
    }

    fn tx(seed: u8) -> TxHash {
        TxHash::solana_from_base58(&bs58::encode([seed; 64]).into_string()).unwrap()
    }

    fn block(slot: u64) -> BlockRef {
        BlockRef::new(Chain::Solana, slot)
    }

    fn make_transfer(log_index: u32) -> Transfer {
        let zero = addr("11111111111111111111111111111111");
        Transfer {
            chain: chain(),
            tx_hash: tx(1),
            block: block(300_000_000),
            block_time: Utc::now(),
            token: addr("So11111111111111111111111111111111111111112"),
            from: zero.clone(),
            to: zero,
            amount_raw: 1_000_000,
            decimals: 9,
            log_index,
        }
    }

    fn make_swap() -> Swap {
        let a = addr("So11111111111111111111111111111111111111112");
        Swap {
            chain: chain(),
            tx_hash: tx(2),
            block: block(300_000_001),
            block_time: Utc::now(),
            pool: a.clone(),
            dex: DexKind::RaydiumV4,
            sender: a.clone(),
            token_in: a.clone(),
            token_out: a,
            amount_in_raw: 100,
            decimals_in: 9,
            amount_out_raw: 200,
            decimals_out: 6,
            usd_value: None,
            log_index: 0,
        }
    }

    fn make_pool_event() -> PoolEvent {
        let a = addr("So11111111111111111111111111111111111111112");
        PoolEvent {
            chain: chain(),
            tx_hash: tx(3),
            block: block(300_000_002),
            block_time: Utc::now(),
            pool: a.clone(),
            dex: DexKind::RaydiumV4,
            kind: PoolEventKind::Sync {
                reserve0_raw: 1_000,
                reserve1_raw: 2_000,
            },
            actor: a,
            log_index: 0,
        }
    }

    // -------------------------------------------------------------------------
    // Transfer → transfers buffer
    // -------------------------------------------------------------------------

    #[test]
    fn transfer_goes_to_transfers_buffer() {
        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::Transfer(make_transfer(0)), &mut batcher);
        assert!(matches!(result, RouteResult::Buffered));
        assert_eq!(batcher.transfers.len(), 1);
        assert_eq!(batcher.swaps.len(), 0);
        assert_eq!(batcher.pool_events.len(), 0);
    }

    // -------------------------------------------------------------------------
    // Swap → swaps buffer
    // -------------------------------------------------------------------------

    #[test]
    fn swap_goes_to_swaps_buffer() {
        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::Swap(make_swap()), &mut batcher);
        assert!(matches!(result, RouteResult::Buffered));
        assert_eq!(batcher.swaps.len(), 1);
        assert_eq!(batcher.transfers.len(), 0);
    }

    // -------------------------------------------------------------------------
    // PoolEvent → pool_events buffer
    // -------------------------------------------------------------------------

    #[test]
    fn pool_event_goes_to_pool_events_buffer() {
        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::PoolEvent(make_pool_event()), &mut batcher);
        assert!(matches!(result, RouteResult::Buffered));
        assert_eq!(batcher.pool_events.len(), 1);
    }

    // -------------------------------------------------------------------------
    // TokenMeta → RouteResult::TokenMeta (not buffered)
    // -------------------------------------------------------------------------

    #[test]
    fn token_meta_returns_token_meta_result() {
        use mg_onchain_common::token::{JupiterVerification, TokenMeta};
        use rust_decimal::Decimal;

        let meta = Box::new(TokenMeta {
            chain: Chain::Solana,
            mint: addr("So11111111111111111111111111111111111111112"),
            symbol: Some("SOL".into()),
            name: Some("Solana".into()),
            decimals: 9,
            token_program: None,
            total_supply_raw: 1_000_000_000,
            circulating_supply_raw: None,
            mint_authority: None,
            freeze_authority: None,
            creator: None,
            creator_balance_raw: 0,
            transfer_fee: None,
            permanent_delegate: None,
            transfer_hook_program: None,
            non_transferable: false,
            confidential_transfer: false,
            top_holders: vec![],
            total_holders: 0,
            markets: vec![],
            total_market_liquidity_usd: Decimal::ZERO,
            lockers: vec![],
            graph_insiders_detected: false,
            insider_networks: vec![],
            launchpad: None,
            deploy_platform: None,
            detected_at: None,
            rugged: false,
            verification: JupiterVerification {
                jup_verified: false,
                jup_strict: false,
            },
            rugcheck_score: None,
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: Utc::now(),
        });

        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::TokenMeta(meta), &mut batcher);
        assert!(matches!(result, RouteResult::TokenMeta(_)));
        // Nothing should be in any batcher buffer
        assert_eq!(batcher.pending_count(), 0);
    }

    // -------------------------------------------------------------------------
    // ReorgMarker → RouteResult::Reorg
    // -------------------------------------------------------------------------

    #[test]
    fn reorg_marker_returns_reorg_result() {
        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::ReorgMarker { slot: 300_000_999 }, &mut batcher);
        match result {
            RouteResult::Reorg { slot } => assert_eq!(slot, 300_000_999),
            other => panic!("expected Reorg, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // SlotFinalized → RouteResult::SlotFinalized
    // -------------------------------------------------------------------------

    #[test]
    fn slot_finalized_returns_correct_result() {
        let mut batcher = EventBatcher::new(500, 60_000);
        let result = route_event(Event::SlotFinalized { slot: 300_000_100 }, &mut batcher);
        match result {
            RouteResult::SlotFinalized { slot } => assert_eq!(slot, 300_000_100),
            other => panic!("expected SlotFinalized, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Mixed sequence routes each event to correct buffer
    // -------------------------------------------------------------------------

    #[test]
    fn mixed_sequence_routes_correctly() {
        let mut batcher = EventBatcher::new(500, 60_000);

        route_event(Event::Transfer(make_transfer(0)), &mut batcher);
        route_event(Event::Transfer(make_transfer(1)), &mut batcher);
        route_event(Event::Swap(make_swap()), &mut batcher);
        route_event(Event::PoolEvent(make_pool_event()), &mut batcher);

        assert_eq!(batcher.transfers.len(), 2);
        assert_eq!(batcher.swaps.len(), 1);
        assert_eq!(batcher.pool_events.len(), 1);
        assert_eq!(batcher.holder_snapshots.len(), 0);
    }
}
