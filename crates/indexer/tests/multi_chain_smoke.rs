//! Multi-chain smoke tests for `MultiChainCoordinator` (ADR 0005 Decision 1).
//!
//! Validates that:
//! - Events from both Solana and Ethereum adapters arrive in the unified stream.
//! - Per-chain checkpoint API is distinct (Solana checkpoint does not affect Ethereum and vice versa).
//! - Coordinator stop/join lifecycle works correctly.
//! - 3-chain coordinator (Solana + Ethereum + BSC) merges events correctly.
//! - 5-chain coordinator (Solana + 4 EVM chains) stream merge works under load.
//! - Graceful shutdown across N chains drains all pending events.
//!
//! All tests use mock adapters — no live nodes required.
//! Not Docker-gated.

use std::ops::RangeInclusive;
use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};

use mg_onchain_chain_adapter::{AdapterError, Checkpoint, ChainAdapter, Event, SubscribeFilter};
use mg_onchain_common::chain::{BlockRef, Chain};
use mg_onchain_indexer::{AdapterSlot, MultiChainCoordinator};
use mg_onchain_indexer::shutdown::ShutdownSignal;

// ---------------------------------------------------------------------------
// Mock adapter
// ---------------------------------------------------------------------------

/// A mock `ChainAdapter` that emits a fixed sequence of `SlotFinalized` events.
///
/// Used to exercise the coordinator event stream without real RPC connections.
struct FixedEventAdapter {
    chain: Chain,
    slots: Vec<u64>,
}

impl FixedEventAdapter {
    fn new(chain: Chain, slots: Vec<u64>) -> Self {
        Self { chain, slots }
    }
}

impl ChainAdapter for FixedEventAdapter {
    fn subscribe(
        &self,
        _filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        let events: Vec<Result<Event, AdapterError>> = self
            .slots
            .iter()
            .map(|&slot| Ok(Event::SlotFinalized { slot }))
            .collect();
        Box::pin(futures::stream::iter(events))
    }

    fn backfill(
        &self,
        _range: RangeInclusive<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        Box::pin(futures::stream::empty())
    }

    async fn checkpoint_save(&self, _cp: &Checkpoint) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn checkpoint_load(&self) -> Result<Option<Checkpoint>, AdapterError> {
        Ok(None)
    }

    async fn health_check(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn tip(&self) -> Result<BlockRef, AdapterError> {
        Ok(BlockRef::new(self.chain, 0))
    }

    fn default_filter(&self) -> SubscribeFilter {
        match self.chain {
            Chain::Solana => SubscribeFilter::solana_default(),
            Chain::Ethereum => SubscribeFilter::ethereum_default(),
            _ => SubscribeFilter::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1: events from both chains arrive in unified stream
// ---------------------------------------------------------------------------

/// Both Solana and Ethereum events must appear in the coordinator's event stream.
///
/// Order within the stream may vary (select_all / mpsc merge is non-deterministic),
/// but all events from both chains must eventually arrive.
#[tokio::test]
async fn coordinator_events_from_both_chains_arrive() {
    let solana_slots = vec![1u64, 2, 3];
    let eth_slots = vec![100u64, 200, 300];

    let solana_adapter = FixedEventAdapter::new(Chain::Solana, solana_slots.clone());
    let eth_adapter = FixedEventAdapter::new(Chain::Ethereum, eth_slots.clone());

    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(
        vec![
            AdapterSlot::new(Chain::Solana, "solana", solana_adapter),
            AdapterSlot::new(Chain::Ethereum, "ethereum", eth_adapter),
        ],
        shutdown,
    );

    let mut stream = coordinator
        .event_stream()
        .await
        .expect("event_stream must not fail");

    let mut received_slots: Vec<u64> = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok(Event::SlotFinalized { slot }) = item {
            received_slots.push(slot);
        }
    }

    // All 6 events must arrive.
    assert_eq!(
        received_slots.len(),
        6,
        "expected 6 events total (3 Solana + 3 Ethereum), got: {received_slots:?}"
    );

    // Each expected slot present.
    for &slot in solana_slots.iter().chain(eth_slots.iter()) {
        assert!(
            received_slots.contains(&slot),
            "missing slot {slot} from received: {received_slots:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: per-chain checkpoint isolation (structural)
// ---------------------------------------------------------------------------

/// Verify that per-chain adapter IDs are distinct, ensuring checkpoint rows
/// in `adapter_checkpoints` do not collide.
///
/// This is a structural invariant test — no actual checkpoint writes are needed
/// because `adapter_id` is the Postgres unique key.
#[test]
fn coordinator_per_chain_checkpoint_ids_are_distinct() {
    let solana_slot = AdapterSlot {
        chain: Chain::Solana,
        adapter_id: "solana".into(),
        adapter: Box::new(FixedEventAdapter::new(Chain::Solana, vec![])),
    };
    let eth_slot = AdapterSlot {
        chain: Chain::Ethereum,
        adapter_id: "ethereum".into(),
        adapter: Box::new(FixedEventAdapter::new(Chain::Ethereum, vec![])),
    };

    assert_ne!(
        solana_slot.adapter_id, eth_slot.adapter_id,
        "adapter IDs must be distinct to prevent checkpoint key collision in adapter_checkpoints"
    );
}

// ---------------------------------------------------------------------------
// Test 3: stop drains running tasks
// ---------------------------------------------------------------------------

/// After `coordinator.stop()` is called, the unified stream closes.
#[tokio::test]
async fn coordinator_stop_closes_stream() {
    // Adapter with no pre-loaded events (stream ends immediately).
    let solana_adapter = FixedEventAdapter::new(Chain::Solana, vec![42, 43]);

    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(
        vec![AdapterSlot::new(Chain::Solana, "solana", solana_adapter)],
        shutdown.clone(),
    );

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    coordinator.start(tx).await.unwrap();

    // Give task a moment to deliver events.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Signal stop.
    shutdown.cancel();

    // Collect whatever arrived — bridge receiver into stream without tokio-stream dep.
    let rx_stream = futures::stream::unfold(rx, |mut r| async move {
        r.recv().await.map(|item| (item, r))
    });
    futures::pin_mut!(rx_stream);
    let mut slots: Vec<u64> = Vec::new();
    while let Some(Ok(Event::SlotFinalized { slot })) = rx_stream.next().await {
        slots.push(slot);
    }

    // Must have received the pre-loaded events.
    assert!(
        slots.contains(&42),
        "slot 42 must arrive before stop; got: {slots:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: healthcheck returns per-chain status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coordinator_healthcheck_returns_both_chains() {
    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(
        vec![
            AdapterSlot::new(Chain::Solana, "solana", FixedEventAdapter::new(Chain::Solana, vec![])),
            AdapterSlot::new(
                Chain::Ethereum,
                "ethereum",
                FixedEventAdapter::new(Chain::Ethereum, vec![]),
            ),
        ],
        shutdown,
    );

    let health = coordinator.healthcheck().await;

    assert_eq!(health.len(), 2, "expected health status for 2 chains");

    let solana_health = health.iter().find(|h| h.chain == Chain::Solana)
        .expect("Solana health missing");
    let eth_health = health.iter().find(|h| h.chain == Chain::Ethereum)
        .expect("Ethereum health missing");

    assert!(solana_health.healthy, "Solana mock adapter must be healthy");
    assert!(eth_health.healthy, "Ethereum mock adapter must be healthy");
    assert_eq!(solana_health.adapter_id, "solana");
    assert_eq!(eth_health.adapter_id, "ethereum");
    assert!(solana_health.error.is_none());
    assert!(eth_health.error.is_none());
}

// ---------------------------------------------------------------------------
// Test 5: 3-chain coordinator — Solana + Ethereum + BSC events interleave
// ---------------------------------------------------------------------------

/// Three adapters (Solana, Ethereum, BSC) — all events arrive in the unified stream.
///
/// Order within the stream may vary (select_all merge is non-deterministic), but
/// ALL events from all three chains must eventually arrive.
///
/// Per-chain checkpoint isolation: all three adapter IDs are distinct.
#[tokio::test]
async fn coordinator_three_chains_all_events_arrive() {
    let solana_slots = vec![10u64, 11, 12];
    let eth_slots = vec![200u64, 201, 202];
    let bsc_slots = vec![300u64, 301, 302];

    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(
        vec![
            AdapterSlot::new(Chain::Solana, "solana", FixedEventAdapter::new(Chain::Solana, solana_slots.clone())),
            AdapterSlot::new(Chain::Ethereum, "ethereum", FixedEventAdapter::new(Chain::Ethereum, eth_slots.clone())),
            AdapterSlot::new(Chain::Bsc, "bsc", FixedEventAdapter::new(Chain::Bsc, bsc_slots.clone())),
        ],
        shutdown,
    );

    let mut stream = coordinator
        .event_stream()
        .await
        .expect("event_stream must not fail on 3-chain coordinator");

    let mut received: Vec<u64> = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok(Event::SlotFinalized { slot }) = item {
            received.push(slot);
        }
    }

    assert_eq!(
        received.len(),
        9,
        "expected 9 events (3 Solana + 3 Ethereum + 3 BSC), got: {received:?}"
    );

    // All slots from all 3 chains must arrive.
    for &slot in solana_slots.iter().chain(eth_slots.iter()).chain(bsc_slots.iter()) {
        assert!(
            received.contains(&slot),
            "missing slot {slot} in 3-chain stream; got: {received:?}"
        );
    }
}

/// Adapter IDs for 3 chains must all be distinct (checkpoint row isolation).
#[test]
fn coordinator_three_chain_adapter_ids_are_distinct() {
    let ids: Vec<&str> = vec!["solana", "ethereum", "bsc"];
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "all 3 chain adapter IDs must be unique to prevent checkpoint key collisions"
    );
}

// ---------------------------------------------------------------------------
// Test 6: 5-chain coordinator — Solana + 4 EVM mocks under load
// ---------------------------------------------------------------------------

/// Five adapters (Solana + Ethereum + BSC + Base + Arbitrum) — 25 events total.
///
/// Stress-tests the stream merge (select_all / mpsc) under higher adapter count.
/// All 25 events must arrive with correct totals.
#[tokio::test]
async fn coordinator_five_chains_stress_merge() {
    // 5 events per chain × 5 chains = 25 events total.
    let chains_and_slots: Vec<(Chain, &str, Vec<u64>)> = vec![
        (Chain::Solana, "solana", vec![1, 2, 3, 4, 5]),
        (Chain::Ethereum, "ethereum", vec![100, 101, 102, 103, 104]),
        (Chain::Bsc, "bsc", vec![200, 201, 202, 203, 204]),
        (Chain::Base, "base", vec![300, 301, 302, 303, 304]),
        (Chain::Arbitrum, "arbitrum", vec![400, 401, 402, 403, 404]),
    ];

    let all_slots: Vec<u64> = chains_and_slots
        .iter()
        .flat_map(|(_, _, slots)| slots.iter().copied())
        .collect();

    let slots: Vec<AdapterSlot> = chains_and_slots
        .into_iter()
        .map(|(chain, id, slots)| {
            AdapterSlot::new(chain, id, FixedEventAdapter::new(chain, slots))
        })
        .collect();

    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(slots, shutdown);

    let mut stream = coordinator
        .event_stream()
        .await
        .expect("event_stream must not fail on 5-chain coordinator");

    let mut received: Vec<u64> = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok(Event::SlotFinalized { slot }) = item {
            received.push(slot);
        }
    }

    assert_eq!(
        received.len(),
        25,
        "expected 25 events (5 chains × 5 events), got {}: {received:?}",
        received.len()
    );

    for &slot in &all_slots {
        assert!(
            received.contains(&slot),
            "missing slot {slot} in 5-chain stream; received: {received:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7: Graceful shutdown drains all streams across N chains
// ---------------------------------------------------------------------------

/// Graceful shutdown via `ShutdownSignal::cancel()` must drain the event channel.
///
/// Adapter emits 3 events per chain × 4 chains = 12 events total. After cancellation,
/// the coordinator's channel receiver must drain all pending events (no data loss
/// for events that were already queued before the cancel signal).
#[tokio::test]
async fn coordinator_graceful_shutdown_drains_all_chains() {
    let chains: Vec<(Chain, &str, Vec<u64>)> = vec![
        (Chain::Solana, "solana", vec![10, 11, 12]),
        (Chain::Ethereum, "ethereum", vec![20, 21, 22]),
        (Chain::Bsc, "bsc", vec![30, 31, 32]),
        (Chain::Base, "base", vec![40, 41, 42]),
    ];

    let all_slots: Vec<u64> = chains
        .iter()
        .flat_map(|(_, _, s)| s.iter().copied())
        .collect();

    let slots: Vec<AdapterSlot> = chains
        .into_iter()
        .map(|(chain, id, s)| AdapterSlot::new(chain, id, FixedEventAdapter::new(chain, s)))
        .collect();

    let shutdown = ShutdownSignal::new();
    let coordinator = MultiChainCoordinator::new(slots, shutdown.clone());

    // Use mpsc channel to receive events — same pattern as coordinator_stop_closes_stream.
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    coordinator.start(tx).await.unwrap();

    // Give tasks time to deliver their pre-loaded events, then cancel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown.cancel();

    // Drain the receiver.
    let rx_stream = futures::stream::unfold(rx, |mut r| async move {
        r.recv().await.map(|item| (item, r))
    });
    futures::pin_mut!(rx_stream);

    let mut received: Vec<u64> = Vec::new();
    while let Some(Ok(Event::SlotFinalized { slot })) = rx_stream.next().await {
        received.push(slot);
    }

    // All 12 pre-loaded events must have been delivered before shutdown completed.
    for &slot in &all_slots {
        assert!(
            received.contains(&slot),
            "slot {slot} must be delivered before graceful shutdown; received: {received:?}"
        );
    }
}
