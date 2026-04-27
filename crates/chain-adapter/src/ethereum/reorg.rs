//! Reorg detection buffer for the Ethereum chain adapter.
//!
//! # Reorg model on post-Merge Ethereum (ADR 0004 §Reorg handling)
//!
//! Post-Merge Ethereum uses LMD-GHOST + Casper FFG consensus. Reorgs deeper than
//! 1-2 blocks are extremely rare and reorgs past the finality horizon (64 slots,
//! ~12.8 minutes) are theoretically impossible without >1/3 malicious stake.
//!
//! The practical policy (CLAUDE.md §Ethereum/EVM):
//! - **Depth 12**: block is treated as "safe" for hot-path event emission.
//! - **`finalized` tag**: block is immutable; use for checkpoint saves and durable writes.
//!
//! # Detection algorithm
//!
//! `ReorgBuffer` maintains a sliding window of the last `reorg_buffer_depth` block
//! headers (hash + parent_hash). On each new head:
//! 1. If the new head's `parent_hash` matches the tip of our buffer — linear chain,
//!    push and continue.
//! 2. If the new head's `parent_hash` does NOT match the buffer tip — reorg detected.
//!    Walk back through the buffer to find the fork point. Return the list of evicted
//!    block numbers so callers can emit `Event::ReorgMarker` for each.
//!
//! The buffer depth is `reorg_buffer_depth = 16` (12 for the safe-confirmation window
//! plus 4 margin). Because post-Merge reorgs beyond 2 blocks are extraordinary, this
//! window provides ample coverage.
//!
//! # ExEx path (Sprint 16+)
//!
//! When the Reth ExEx path is implemented, `ChainReverted` notifications carry the
//! exact reverted block list — no hash-tracking needed. The `ReorgBuffer` is then
//! only used by the `eth_subscribe` fallback path. Both paths emit `Event::ReorgMarker`
//! through the same channel.

use std::collections::VecDeque;

use tracing::{debug, warn};

use crate::ethereum::types::BlockHeader;

/// Default buffer depth: safe-confirmation window (12) + margin (4).
///
/// CLAUDE.md specifies 12 confirmations for EVM "finality" on the hot path.
/// The 4-block margin ensures reorgs that land exactly at depth-12 are still caught.
pub const REORG_BUFFER_DEPTH: usize = 16;

// ---------------------------------------------------------------------------
// ReorgBuffer
// ---------------------------------------------------------------------------

/// Sliding-window buffer of recent block headers for reorg detection.
///
/// Maintains the last `capacity` block headers in insertion order (oldest first).
/// Detects reorgs by checking whether each new head's `parent_hash` matches the
/// current tip's `hash`.
///
/// Thread safety: NOT internally synchronized — wrap in `Mutex<ReorgBuffer>` if
/// shared across tasks. `EthereumAdapter` holds it behind `Mutex` in its subscribe loop.
pub struct ReorgBuffer {
    headers: VecDeque<BlockHeader>,
    capacity: usize,
}

/// Outcome of processing a new block header through the buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum ReorgOutcome {
    /// New head extends the canonical chain linearly. No reorg.
    LinearExtension,
    /// A reorg was detected. The returned `Vec<u64>` contains the block numbers
    /// that were evicted from the buffer (they should be marked as reverted).
    ///
    /// Callers emit `Event::ReorgMarker { slot: block_number }` for each entry.
    Reorg { reverted_block_numbers: Vec<u64> },
}

impl ReorgBuffer {
    /// Create a new `ReorgBuffer` with the given capacity.
    ///
    /// Use `REORG_BUFFER_DEPTH` (16) as the production capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            headers: VecDeque::with_capacity(capacity + 1),
            capacity,
        }
    }

    /// Return the current tip (the most recently pushed block header).
    ///
    /// Returns `None` on an empty buffer (first block has not been pushed yet).
    pub fn tip(&self) -> Option<&BlockHeader> {
        self.headers.back()
    }

    /// Return the number of entries currently in the buffer.
    pub fn len(&self) -> usize {
        self.headers.len()
    }

    /// Returns `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Process a new block header and determine whether a reorg occurred.
    ///
    /// # Algorithm
    ///
    /// 1. If buffer is empty, push the header and return `LinearExtension`.
    /// 2. If `new_head.parent_hash == tip.hash`, push and return `LinearExtension`.
    /// 3. Otherwise, walk back from the tip to find the fork point. All headers
    ///    after the fork point are evicted and their block numbers are returned as
    ///    `Reorg { reverted_block_numbers }`. The new head replaces the evicted tip.
    ///
    /// If the fork point is not found within the buffer (reorg deeper than
    /// `capacity` blocks), all buffered headers are evicted and the new head is
    /// pushed as the new genesis of our buffer. A warning is logged.
    pub fn push(&mut self, new_head: BlockHeader) -> ReorgOutcome {
        if self.headers.is_empty() {
            self.headers.push_back(new_head);
            return ReorgOutcome::LinearExtension;
        }

        let tip = self.headers.back().expect("non-empty buffer must have a tip");

        if tip.hash == new_head.parent_hash {
            // Linear extension — no reorg.
            debug!(
                block = new_head.number,
                hash = %new_head.hash,
                "buffer: linear extension"
            );
            self.headers.push_back(new_head);
            // Trim the oldest entry if over capacity.
            if self.headers.len() > self.capacity {
                self.headers.pop_front();
            }
            return ReorgOutcome::LinearExtension;
        }

        // Reorg detected — find the fork point.
        warn!(
            new_block = new_head.number,
            new_parent = %new_head.parent_hash,
            tip_hash = %tip.hash,
            "reorg detected — scanning buffer for fork point"
        );

        // Walk back to find where the new head's parent_hash matches.
        let fork_idx = self.headers.iter().rposition(|h| h.hash == new_head.parent_hash);

        let reverted: Vec<u64> = match fork_idx {
            Some(idx) => {
                // Evict everything after the fork point.
                let evicted: Vec<u64> = self.headers
                    .iter()
                    .skip(idx + 1)
                    .map(|h| h.number)
                    .collect();
                self.headers.truncate(idx + 1);
                evicted
            }
            None => {
                // Fork point not in buffer — reorg deeper than capacity.
                warn!(
                    capacity = self.capacity,
                    "reorg deeper than buffer capacity — evicting all headers"
                );
                let evicted: Vec<u64> = self.headers.iter().map(|h| h.number).collect();
                self.headers.clear();
                evicted
            }
        };

        self.headers.push_back(new_head);

        ReorgOutcome::Reorg { reverted_block_numbers: reverted }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn header(number: u64, hash: &str, parent_hash: &str) -> BlockHeader {
        BlockHeader {
            number,
            hash: hash.to_string(),
            parent_hash: parent_hash.to_string(),
        }
    }

    #[test]
    fn empty_buffer_is_empty() {
        let buf = ReorgBuffer::new(REORG_BUFFER_DEPTH);
        assert!(buf.is_empty());
        assert!(buf.tip().is_none());
    }

    #[test]
    fn linear_chain_no_reorg() {
        let mut buf = ReorgBuffer::new(REORG_BUFFER_DEPTH);
        let h1 = header(1, "0xh1", "0xgenesis");
        let h2 = header(2, "0xh2", "0xh1");
        let h3 = header(3, "0xh3", "0xh2");

        assert_eq!(buf.push(h1), ReorgOutcome::LinearExtension);
        assert_eq!(buf.push(h2), ReorgOutcome::LinearExtension);
        assert_eq!(buf.push(h3), ReorgOutcome::LinearExtension);

        assert_eq!(buf.len(), 3);
        assert_eq!(buf.tip().unwrap().number, 3);
    }

    #[test]
    fn single_block_reorg_detected() {
        let mut buf = ReorgBuffer::new(REORG_BUFFER_DEPTH);
        buf.push(header(1, "0xh1", "0xgenesis"));
        buf.push(header(2, "0xh2", "0xh1"));
        buf.push(header(3, "0xh3", "0xh2"));

        // Block 4 arrives but its parent is h2 (not h3) — 1-block reorg.
        let outcome = buf.push(header(4, "0xh4_reorg", "0xh2"));

        match outcome {
            ReorgOutcome::Reorg { reverted_block_numbers } => {
                assert_eq!(reverted_block_numbers, vec![3]);
            }
            _ => panic!("expected Reorg, got LinearExtension"),
        }

        // Buffer tip should now be block 4 on the new canonical chain.
        assert_eq!(buf.tip().unwrap().number, 4);
    }

    #[test]
    fn three_block_reorg_detected() {
        let mut buf = ReorgBuffer::new(REORG_BUFFER_DEPTH);
        buf.push(header(1, "0xh1", "0xgenesis"));
        buf.push(header(2, "0xh2", "0xh1"));
        buf.push(header(3, "0xh3", "0xh2"));
        buf.push(header(4, "0xh4", "0xh3"));
        buf.push(header(5, "0xh5", "0xh4"));

        // New head at block 5 but parent is h2 — 3-block reorg (reverts 3, 4, 5).
        let outcome = buf.push(header(5, "0xh5_reorg", "0xh2"));

        match outcome {
            ReorgOutcome::Reorg { reverted_block_numbers } => {
                // Blocks 3, 4, 5 from the old chain are reverted.
                assert_eq!(reverted_block_numbers, vec![3, 4, 5]);
            }
            _ => panic!("expected Reorg"),
        }
    }

    #[test]
    fn buffer_trims_at_capacity() {
        let mut buf = ReorgBuffer::new(4);
        for i in 1..=8u64 {
            let parent = if i == 1 {
                "0xgenesis".to_string()
            } else {
                format!("0xh{}", i - 1)
            };
            buf.push(header(i, &format!("0xh{i}"), &parent));
        }
        // Buffer should hold at most 4 headers.
        assert_eq!(buf.len(), 4);
        // Tip should be the most recent.
        assert_eq!(buf.tip().unwrap().number, 8);
    }

    #[test]
    fn reorg_deeper_than_capacity_evicts_all() {
        let mut buf = ReorgBuffer::new(4);
        buf.push(header(1, "0xh1", "0xgenesis"));
        buf.push(header(2, "0xh2", "0xh1"));
        buf.push(header(3, "0xh3", "0xh2"));
        buf.push(header(4, "0xh4", "0xh3"));

        // New head whose parent is NOT in the buffer at all.
        let outcome = buf.push(header(5, "0xh5_alien", "0xunknown_parent"));

        match outcome {
            ReorgOutcome::Reorg { reverted_block_numbers } => {
                // All 4 buffered blocks (1-4) are evicted.
                assert_eq!(reverted_block_numbers.len(), 4);
            }
            _ => panic!("expected Reorg"),
        }
        // Buffer should only have the new head.
        assert_eq!(buf.len(), 1);
    }
}
