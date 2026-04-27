//! `StreamingRegistry` — tracks which `(Chain, Mint)` pairs are actively streamed.
//!
//! # Lifecycle
//!
//! A token *enters* the registry automatically when an `InvalidationEvent`
//! carries its mint.  A token *exits* when BOTH conditions hold:
//!
//! - No `InvalidationEvent` received for it in `idle_timeout`.
//! - Zero active WS subscribers covering that token.
//!
//! # Cap + LRU eviction
//!
//! When `len() >= max_tokens`, the token with the oldest `last_event_at` is
//! evicted before the new one is inserted.
//!
//! # Determinism
//!
//! `BTreeMap` ensures deterministic iteration order in GC and LRU sweeps.
//! `event_time` values come from block_time (the `InvalidationEvent` field),
//! never from wall-clock.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use mg_onchain_common::chain::Chain;

/// Canonical mint address string (chain-normalised at the adapter boundary).
pub type Mint = String;

/// Per-token streaming state.
#[derive(Debug, Clone)]
pub struct StreamingState {
    /// Time of the most recent `InvalidationEvent` for this token (block_time,
    /// not wall-clock).
    pub last_event_at: DateTime<Utc>,
    /// Number of WS consumers whose subscription filter covers this token.
    pub subscriber_count: u32,
}

/// Registry of actively-streamed `(Chain, Mint)` pairs.
///
/// Wrapped in `Arc<tokio::sync::RwLock<StreamingRegistry>>` at the call site.
pub struct StreamingRegistry {
    /// `BTreeMap` for deterministic iteration (GC, LRU sweep).
    tokens: BTreeMap<(Chain, Mint), StreamingState>,
    /// Hard cap on registered tokens.
    max_tokens: usize,
    /// Duration after which a token with no subscribers is eligible for GC.
    idle_timeout: Duration,
}

impl StreamingRegistry {
    /// Create a new registry.
    ///
    /// - `max_tokens`: hard cap; when reached, LRU token is evicted.
    /// - `idle_timeout`: how long a token with zero subscribers can stay idle.
    pub fn new(max_tokens: usize, idle_timeout: Duration) -> Self {
        Self {
            tokens: BTreeMap::new(),
            max_tokens,
            idle_timeout,
        }
    }

    /// Record a new event for `(chain, mint)`.
    ///
    /// `event_time` MUST come from block_time — never from wall-clock.
    ///
    /// If the token is not yet registered, it is inserted (with LRU eviction if
    /// at cap).  If already registered, `last_event_at` is updated to the
    /// maximum of current and incoming values (monotonically increasing).
    pub fn on_event(&mut self, chain: Chain, mint: Mint, event_time: DateTime<Utc>) {
        if let Some(state) = self.tokens.get_mut(&(chain, mint.clone())) {
            // Monotonically advance; guards against out-of-order events.
            if event_time > state.last_event_at {
                state.last_event_at = event_time;
            }
        } else {
            // Need to insert — enforce cap first.
            if self.tokens.len() >= self.max_tokens {
                self.evict_lru();
            }
            self.tokens.insert(
                (chain, mint),
                StreamingState {
                    last_event_at: event_time,
                    subscriber_count: 0,
                },
            );
        }
    }

    /// Increment the subscriber count for `(chain, mint)`.
    ///
    /// If the token is not currently registered it is inserted (with its
    /// `last_event_at` set to `DateTime::<Utc>::MIN_UTC` as a sentinel — the
    /// token will be GC-eligible if no event arrives within `idle_timeout`).
    pub fn on_subscribe(&mut self, chain: Chain, mint: Mint) {
        let state = self
            .tokens
            .entry((chain, mint))
            .or_insert_with(|| StreamingState {
                last_event_at: DateTime::<Utc>::MIN_UTC,
                subscriber_count: 0,
            });
        state.subscriber_count = state.subscriber_count.saturating_add(1);
    }

    /// Decrement the subscriber count for `(chain, mint)`.
    ///
    /// Saturating sub prevents underflow from stale decrements.
    pub fn on_unsubscribe(&mut self, chain: Chain, mint: Mint) {
        if let Some(state) = self.tokens.get_mut(&(chain, mint)) {
            state.subscriber_count = state.subscriber_count.saturating_sub(1);
        }
    }

    /// Run a garbage-collection pass.
    ///
    /// Evicts tokens where BOTH:
    /// - `subscriber_count == 0`
    /// - `now - last_event_at >= idle_timeout`
    ///
    /// Returns the list of evicted `(Chain, Mint)` pairs.
    ///
    /// `now` MUST be derived from block_time or an explicitly controlled time
    /// source in tests — never from `Utc::now()`.
    pub fn gc(&mut self, now: DateTime<Utc>) -> Vec<(Chain, Mint)> {
        let idle_timeout = self.idle_timeout;
        let mut evicted = Vec::new();
        self.tokens.retain(|(chain, mint), state| {
            let idle = now - state.last_event_at >= idle_timeout;
            let no_subs = state.subscriber_count == 0;
            if idle && no_subs {
                evicted.push((*chain, mint.clone()));
                false
            } else {
                true
            }
        });
        evicted
    }

    /// Returns `true` if the token is currently registered.
    pub fn is_active(&self, chain: &Chain, mint: &Mint) -> bool {
        self.tokens.contains_key(&(*chain, mint.clone()))
    }

    /// Current number of registered tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Returns `true` when the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Evict the token with the oldest `last_event_at` (LRU policy).
    ///
    /// Called only when `len() >= max_tokens`.  No-op on empty map.
    fn evict_lru(&mut self) {
        if self.tokens.is_empty() {
            return;
        }
        // BTreeMap is ordered by (Chain, Mint) — not by last_event_at.
        // We must scan to find the minimum.
        let lru_key = self
            .tokens
            .iter()
            .min_by_key(|(_, state)| state.last_event_at)
            .map(|(k, _)| k.clone());
        if let Some(key) = lru_key {
            self.tokens.remove(&key);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_time(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
    }

    fn mint(s: &str) -> String {
        s.to_string()
    }

    // -----------------------------------------------------------------------
    // LRU eviction: cap=3, 5 tokens — 2 evictions in LRU order
    // -----------------------------------------------------------------------

    #[test]
    fn lru_eviction_at_cap() {
        let mut reg = StreamingRegistry::new(3, Duration::hours(1));

        // Insert 3 tokens with ascending timestamps.
        reg.on_event(Chain::Solana, mint("A"), make_time(1));
        reg.on_event(Chain::Solana, mint("B"), make_time(2));
        reg.on_event(Chain::Solana, mint("C"), make_time(3));
        assert_eq!(reg.len(), 3);

        // Insert 4th — evicts "A" (oldest, last_event_at = hour 1).
        reg.on_event(Chain::Solana, mint("D"), make_time(4));
        assert_eq!(reg.len(), 3);
        assert!(
            !reg.is_active(&Chain::Solana, &mint("A")),
            "A must be evicted (oldest)"
        );
        assert!(
            reg.is_active(&Chain::Solana, &mint("D")),
            "D must be inserted"
        );

        // Insert 5th — evicts "B" (now oldest, last_event_at = hour 2).
        reg.on_event(Chain::Solana, mint("E"), make_time(5));
        assert_eq!(reg.len(), 3);
        assert!(
            !reg.is_active(&Chain::Solana, &mint("B")),
            "B must be evicted"
        );
        assert!(
            reg.is_active(&Chain::Solana, &mint("E")),
            "E must be inserted"
        );

        // C, D, E remain.
        assert!(reg.is_active(&Chain::Solana, &mint("C")));
        assert!(reg.is_active(&Chain::Solana, &mint("D")));
    }

    // -----------------------------------------------------------------------
    // LRU preserves correct order across 5 inserts
    // -----------------------------------------------------------------------

    #[test]
    fn lru_order_preserved_after_five_events() {
        let mut reg = StreamingRegistry::new(3, Duration::hours(1));

        for (label, hour) in [("A", 1u32), ("B", 2), ("C", 3), ("D", 4), ("E", 5)] {
            reg.on_event(Chain::Solana, mint(label), make_time(hour));
        }

        // After 5 inserts with cap=3: C, D, E survive; A and B evicted.
        assert_eq!(reg.len(), 3);
        assert!(!reg.is_active(&Chain::Solana, &mint("A")));
        assert!(!reg.is_active(&Chain::Solana, &mint("B")));
        assert!(reg.is_active(&Chain::Solana, &mint("C")));
        assert!(reg.is_active(&Chain::Solana, &mint("D")));
        assert!(reg.is_active(&Chain::Solana, &mint("E")));
    }

    // -----------------------------------------------------------------------
    // GC evicts entries older than idle_timeout
    // -----------------------------------------------------------------------

    #[test]
    fn gc_evicts_idle_entries() {
        let idle = Duration::minutes(60);
        let mut reg = StreamingRegistry::new(100, idle);

        reg.on_event(Chain::Solana, mint("old"), make_time(0)); // hour 0
        reg.on_event(Chain::Solana, mint("recent"), make_time(1)); // hour 1

        // GC at hour 2 — "old" has been idle for 2h > 1h, "recent" for 1h = 1h.
        // GC condition: now - last_event_at >= idle_timeout.
        // 2h - 0h = 2h >= 1h → evict "old"
        // 2h - 1h = 1h >= 1h → evict "recent" too
        let evicted = reg.gc(make_time(2));
        assert!(evicted.contains(&(Chain::Solana, mint("old"))));
        assert!(evicted.contains(&(Chain::Solana, mint("recent"))));
        assert!(reg.is_empty());
    }

    #[test]
    fn gc_does_not_evict_active_subscribers() {
        let idle = Duration::minutes(30);
        let mut reg = StreamingRegistry::new(100, idle);

        reg.on_event(Chain::Solana, mint("watched"), make_time(0));
        reg.on_subscribe(Chain::Solana, mint("watched"));

        // GC at hour 2: idle > 30min but subscriber_count == 1 — must NOT evict.
        let evicted = reg.gc(make_time(2));
        assert!(
            evicted.is_empty(),
            "token with active subscriber must not be evicted"
        );
        assert!(reg.is_active(&Chain::Solana, &mint("watched")));
    }

    #[test]
    fn gc_evicts_after_unsubscribe() {
        let idle = Duration::minutes(30);
        let mut reg = StreamingRegistry::new(100, idle);

        reg.on_event(Chain::Solana, mint("x"), make_time(0));
        reg.on_subscribe(Chain::Solana, mint("x"));
        reg.on_unsubscribe(Chain::Solana, mint("x"));

        // Now subscriber_count == 0 and idle > 30min.
        let evicted = reg.gc(make_time(2));
        assert!(evicted.contains(&(Chain::Solana, mint("x"))));
    }

    // -----------------------------------------------------------------------
    // Monotonic last_event_at update
    // -----------------------------------------------------------------------

    #[test]
    fn on_event_is_monotonic() {
        let mut reg = StreamingRegistry::new(100, Duration::hours(1));
        reg.on_event(Chain::Solana, mint("t"), make_time(5));
        // Older event does NOT roll back last_event_at.
        reg.on_event(Chain::Solana, mint("t"), make_time(3));
        // GC at make_time(6): 6-5 = 1h >= 1h → evicts.
        let evicted = reg.gc(make_time(6));
        assert!(evicted.contains(&(Chain::Solana, mint("t"))));
    }
}
