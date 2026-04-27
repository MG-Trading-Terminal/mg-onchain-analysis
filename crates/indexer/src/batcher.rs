//! Per-table event batchers.
//!
//! `EventBatcher` holds one `Vec<T>` per table variant. The batcher accumulates
//! events and exposes a `should_flush` predicate. The caller is responsible for
//! calling `flush` and writing the returned slice to storage — this keeps I/O out
//! of the batcher.
//!
//! # Flush triggers
//!
//! - **Size:** `len >= batch_size` — prevents unbounded memory growth.
//! - **Timeout:** time since the first event in the current batch exceeds
//!   `batch_timeout_ms` — bounds write latency.
//!
//! Both predicates are checked in `should_flush`. A single async task drives
//! the subscribe loop, so plain mutable state suffices — no `Mutex` needed.
//!
//! # Ordering guarantee
//!
//! Events are flushed in the order they were buffered (Vec is FIFO when drained
//! from front). Within a slot, the order matches the adapter's emission order.
//! Across slots, monotonicity follows from the stream (adapter emits in slot order).

use std::time::Instant;

use mg_onchain_common::event::{PoolEvent, Swap, Transfer};
use mg_onchain_common::token::HolderSnapshot;

// ---------------------------------------------------------------------------
// PerTableBuffer — one buffer for one event type
// ---------------------------------------------------------------------------

/// A typed ring-buffer for a single event table.
///
/// Not generic-over-T to avoid making `EventBatcher` generic in turn (which
/// would complicate the borrow checker in the router). Each variant is concrete.
pub(crate) struct PerTableBuffer<T> {
    events: Vec<T>,
    /// Time of the first event in the current buffer. `None` when empty.
    pub(crate) first_at: Option<Instant>,
}

impl<T> PerTableBuffer<T> {
    fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            first_at: None,
        }
    }

    fn push(&mut self, event: T) {
        if self.first_at.is_none() {
            self.first_at = Some(Instant::now());
        }
        self.events.push(event);
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns `true` if either the size or the timeout flush trigger fires.
    pub(crate) fn should_flush(&self, batch_size: usize, timeout_ms: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.len() >= batch_size {
            return true;
        }
        if let Some(first) = self.first_at
            && first.elapsed().as_millis() >= timeout_ms as u128
        {
            return true;
        }
        false
    }

    /// Drain the buffer, returning ownership of all events. Resets `first_at`.
    pub(crate) fn drain(&mut self) -> Vec<T> {
        self.first_at = None;
        std::mem::take(&mut self.events)
    }
}

// ---------------------------------------------------------------------------
// EventBatcher — the public API
// ---------------------------------------------------------------------------

/// Batches events by table type.
///
/// Owned by the async loop in `lib.rs`. No concurrent access — a single task
/// drives the subscribe loop and calls `push_*` / `flush_*` without locking.
///
/// Fields are `pub(crate)` so `FlushBatch` tests can inspect them.
pub struct EventBatcher {
    pub(crate) transfers: PerTableBuffer<Transfer>,
    pub(crate) swaps: PerTableBuffer<Swap>,
    pub(crate) pool_events: PerTableBuffer<PoolEvent>,
    pub(crate) holder_snapshots: PerTableBuffer<HolderSnapshot>,
    /// Number of events dispatched to an unknown/unhandled variant.
    /// Used as a metric / log counter, not an error.
    pub(crate) unknown_event_count: u64,
    batch_size: usize,
    timeout_ms: u64,
}

impl EventBatcher {
    /// Create a new `EventBatcher` with the given size and timeout.
    pub fn new(batch_size: usize, timeout_ms: u64) -> Self {
        Self {
            transfers: PerTableBuffer::new(batch_size),
            swaps: PerTableBuffer::new(batch_size),
            pool_events: PerTableBuffer::new(batch_size),
            holder_snapshots: PerTableBuffer::new(batch_size),
            unknown_event_count: 0,
            batch_size,
            timeout_ms,
        }
    }

    /// Push a transfer event into the transfers buffer.
    pub fn push_transfer(&mut self, t: Transfer) {
        self.transfers.push(t);
    }

    /// Push a swap event into the swaps buffer.
    pub fn push_swap(&mut self, s: Swap) {
        self.swaps.push(s);
    }

    /// Push a pool event into the pool_events buffer.
    pub fn push_pool_event(&mut self, e: PoolEvent) {
        self.pool_events.push(e);
    }

    /// Push a holder snapshot into the holder_snapshots buffer.
    pub fn push_holder_snapshot(&mut self, h: HolderSnapshot) {
        self.holder_snapshots.push(h);
    }

    /// Increment the unknown event counter and log at WARN level.
    pub fn count_unknown(&mut self) {
        self.unknown_event_count += 1;
        tracing::warn!(
            count = self.unknown_event_count,
            "unknown/unhandled event variant encountered"
        );
    }

    /// True if any buffer has hit its size or timeout flush trigger.
    pub fn any_should_flush(&self) -> bool {
        self.transfers
            .should_flush(self.batch_size, self.timeout_ms)
            || self.swaps.should_flush(self.batch_size, self.timeout_ms)
            || self
                .pool_events
                .should_flush(self.batch_size, self.timeout_ms)
            || self
                .holder_snapshots
                .should_flush(self.batch_size, self.timeout_ms)
    }

    /// True if a specific buffer is at or over its size limit (ignores timeout).
    /// Used to detect which buffer to flush first when `any_should_flush` is true.
    pub fn transfers_should_flush(&self) -> bool {
        self.transfers
            .should_flush(self.batch_size, self.timeout_ms)
    }

    pub fn swaps_should_flush(&self) -> bool {
        self.swaps.should_flush(self.batch_size, self.timeout_ms)
    }

    pub fn pool_events_should_flush(&self) -> bool {
        self.pool_events
            .should_flush(self.batch_size, self.timeout_ms)
    }

    pub fn holder_snapshots_should_flush(&self) -> bool {
        self.holder_snapshots
            .should_flush(self.batch_size, self.timeout_ms)
    }

    /// Drain the transfers buffer. Returns the drained events (may be empty).
    pub fn drain_transfers(&mut self) -> Vec<Transfer> {
        self.transfers.drain()
    }

    /// Drain the swaps buffer.
    pub fn drain_swaps(&mut self) -> Vec<Swap> {
        self.swaps.drain()
    }

    /// Drain the pool_events buffer.
    pub fn drain_pool_events(&mut self) -> Vec<PoolEvent> {
        self.pool_events.drain()
    }

    /// Drain the holder_snapshots buffer.
    pub fn drain_holder_snapshots(&mut self) -> Vec<HolderSnapshot> {
        self.holder_snapshots.drain()
    }

    /// Drain ALL buffers regardless of flush trigger state.
    /// Called on graceful shutdown and on reorg to ensure in-flight events are
    /// committed before the reorg DELETE is issued.
    pub fn drain_all(&mut self) -> DrainedBatch {
        DrainedBatch {
            transfers: self.transfers.drain(),
            swaps: self.swaps.drain(),
            pool_events: self.pool_events.drain(),
            holder_snapshots: self.holder_snapshots.drain(),
        }
    }

    /// Total pending events across all buffers (for metrics / logging).
    pub fn pending_count(&self) -> usize {
        self.transfers.len()
            + self.swaps.len()
            + self.pool_events.len()
            + self.holder_snapshots.len()
    }

    /// Number of pending transfers in the buffer.
    pub fn transfers_len(&self) -> usize {
        self.transfers.len()
    }
    /// Number of pending swaps in the buffer.
    pub fn swaps_len(&self) -> usize {
        self.swaps.len()
    }
    /// Number of pending pool events in the buffer.
    pub fn pool_events_len(&self) -> usize {
        self.pool_events.len()
    }
    /// Number of pending holder snapshots in the buffer.
    pub fn holder_snapshots_len(&self) -> usize {
        self.holder_snapshots.len()
    }
}

/// All four buffers drained at once (reorg / shutdown flush).
pub struct DrainedBatch {
    pub transfers: Vec<Transfer>,
    pub swaps: Vec<Swap>,
    pub pool_events: Vec<PoolEvent>,
    pub holder_snapshots: Vec<HolderSnapshot>,
}

impl DrainedBatch {
    /// True if every drain is empty.
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
            && self.swaps.is_empty()
            && self.pool_events.is_empty()
            && self.holder_snapshots.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dummy_transfer() -> Transfer {
        use bs58;
        use chrono::Utc;
        use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
        let chain = Chain::Solana;
        let zero = Address::parse(chain, "11111111111111111111111111111111").unwrap();
        let token = Address::parse(chain, "So11111111111111111111111111111111111111112").unwrap();
        let tx = TxHash::solana_from_base58(&bs58::encode([1u8; 64]).into_string()).unwrap();
        Transfer {
            chain,
            tx_hash: tx,
            block: BlockRef::new(chain, 300_000_000),
            block_time: Utc::now(),
            token,
            from: zero.clone(),
            to: zero,
            amount_raw: 1_000_000,
            decimals: 9,
            log_index: 0,
        }
    }

    // -------------------------------------------------------------------------
    // Size trigger
    // -------------------------------------------------------------------------

    #[test]
    fn size_trigger_fires_at_batch_size() {
        let mut batcher = EventBatcher::new(3, 60_000); // 60s timeout — won't fire in test
        assert!(!batcher.any_should_flush());

        batcher.push_transfer(dummy_transfer());
        batcher.push_transfer(dummy_transfer());
        assert!(!batcher.transfers_should_flush()); // 2 < 3

        batcher.push_transfer(dummy_transfer());
        assert!(batcher.transfers_should_flush()); // 3 >= 3
        assert!(batcher.any_should_flush());
    }

    #[test]
    fn size_trigger_does_not_fire_below_batch_size() {
        let mut batcher = EventBatcher::new(500, 60_000);
        for _ in 0..499 {
            batcher.push_transfer(dummy_transfer());
        }
        assert!(!batcher.transfers_should_flush());
    }

    // -------------------------------------------------------------------------
    // Timeout trigger
    // -------------------------------------------------------------------------

    #[test]
    fn timeout_trigger_fires_after_elapsed_time() {
        // Use timeout_ms = 0 so any elapsed time exceeds it
        let mut batcher = EventBatcher::new(500, 0);
        batcher.push_transfer(dummy_transfer());
        // With timeout_ms=0, elapsed (>= 0ms by definition after Instant::now()) should fire.
        // Sleep briefly to guarantee Instant::elapsed > 0.
        std::thread::sleep(Duration::from_millis(1));
        assert!(batcher.transfers_should_flush());
    }

    // -------------------------------------------------------------------------
    // Ordering preserved on drain
    // -------------------------------------------------------------------------

    #[test]
    fn drain_preserves_insertion_order() {
        let mut batcher = EventBatcher::new(10, 60_000);
        for i in 0u128..5 {
            let mut t = dummy_transfer();
            t.amount_raw = i;
            batcher.push_transfer(t);
        }
        let drained = batcher.drain_transfers();
        assert_eq!(drained.len(), 5);
        for (i, t) in drained.iter().enumerate() {
            assert_eq!(t.amount_raw, i as u128, "ordering must be preserved");
        }
    }

    // -------------------------------------------------------------------------
    // Partial batch stays until flush
    // -------------------------------------------------------------------------

    #[test]
    fn partial_batch_stays_in_buffer() {
        let mut batcher = EventBatcher::new(500, 60_000);
        batcher.push_transfer(dummy_transfer());
        batcher.push_swap({
            use bs58;
            use chrono::Utc;
            use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
            use mg_onchain_common::event::{DexKind, Swap};
            let chain = Chain::Solana;
            let addr =
                Address::parse(chain, "So11111111111111111111111111111111111111112").unwrap();
            let tx = TxHash::solana_from_base58(&bs58::encode([2u8; 64]).into_string()).unwrap();
            Swap {
                chain,
                tx_hash: tx,
                block: BlockRef::new(chain, 300_000_001),
                block_time: Utc::now(),
                pool: addr.clone(),
                dex: DexKind::RaydiumV4,
                sender: addr.clone(),
                token_in: addr.clone(),
                token_out: addr,
                amount_in_raw: 1_000,
                decimals_in: 9,
                amount_out_raw: 2_000,
                decimals_out: 6,
                usd_value: None,
                log_index: 0,
            }
        });
        // Neither buffer has hit size=500 or timeout=60s
        assert!(!batcher.any_should_flush());
        // Both events are still in the buffers
        assert_eq!(batcher.transfers.len(), 1);
        assert_eq!(batcher.swaps.len(), 1);
    }

    // -------------------------------------------------------------------------
    // drain_all empties all buffers
    // -------------------------------------------------------------------------

    #[test]
    fn drain_all_empties_every_buffer() {
        let mut batcher = EventBatcher::new(500, 60_000);
        batcher.push_transfer(dummy_transfer());
        batcher.push_transfer(dummy_transfer());
        let batch = batcher.drain_all();
        assert_eq!(batch.transfers.len(), 2);
        assert!(batch.swaps.is_empty());
        assert!(batch.pool_events.is_empty());
        assert!(batch.holder_snapshots.is_empty());
        // Buffer must be cleared after drain
        assert_eq!(batcher.transfers.len(), 0);
        assert!(batcher.transfers.first_at.is_none());
    }

    // -------------------------------------------------------------------------
    // Unknown event counter
    // -------------------------------------------------------------------------

    #[test]
    fn unknown_event_counter_increments() {
        let mut batcher = EventBatcher::new(500, 60_000);
        assert_eq!(batcher.unknown_event_count, 0);
        batcher.count_unknown();
        batcher.count_unknown();
        assert_eq!(batcher.unknown_event_count, 2);
    }
}
