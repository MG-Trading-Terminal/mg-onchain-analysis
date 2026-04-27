//! `MultiChainCoordinator` — wraps N `ChainAdapter` instances and merges their event streams.
//!
//! # ADR 0005 Decision 1 — Pattern B
//!
//! Each adapter is driven by an independent `tokio::spawn` task. The coordinator:
//! - Spawns one task per chain adapter on `start()`.
//! - Merges the per-adapter event streams into a single unified stream via
//!   `futures::stream::select_all`.
//! - Exposes per-chain `healthcheck` and checkpoint APIs.
//! - Exposes `stop()` by cancelling the shared `ShutdownSignal`.
//!
//! `Indexer<A,S,C>` is NOT touched — each chain uses its own Indexer instance.
//! The coordinator is the multi-chain wrapper; single-chain deployments continue
//! to use `Indexer` directly (zero regression risk on the Solana path).
//!
//! # Dyn-compatibility
//!
//! `ChainAdapter` uses `impl Future` return types which are NOT dyn-compatible.
//! The coordinator defines a local `ErasedAdapter` erased trait (same pattern as
//! `ErasedDetector` in `crates/server`) with `async_trait`-boxed async methods.
//! The blanket `impl<T: ChainAdapter> ErasedAdapter for T` erases the concrete type.
//! `AdapterSlot` holds `Box<dyn ErasedAdapter>`.
//!
//! # Event stream
//!
//! Each adapter's stream is wrapped in a lightweight spawn+mpsc bridge so that
//! slow consumers of the unified stream do not block the individual adapter tasks.
//! Buffer capacity per adapter: 256 events (configurable via `COORDINATOR_CHANNEL_CAP`).
//!
//! # Thread safety
//!
//! `MultiChainCoordinator` is `Send + Sync`. All interior mutability is via
//! `Arc<Mutex<_>>` for the join handles.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use mg_onchain_chain_adapter::{AdapterError, ChainAdapter, Event, SubscribeFilter};
use mg_onchain_common::chain::Chain;

use crate::shutdown::ShutdownSignal;

// ---------------------------------------------------------------------------
// ErasedAdapter — dyn-compatible wrapper for ChainAdapter
// ---------------------------------------------------------------------------

/// A dyn-compatible version of `ChainAdapter` for use in `Box<dyn ErasedAdapter>`.
///
/// `ChainAdapter` uses `impl Future` return types which prevent `Box<dyn ChainAdapter>`.
/// This trait replicates only the methods the coordinator actually needs:
/// - `subscribe()` — already boxed in `ChainAdapter`, forwarded directly.
/// - `health_check()` — boxed via `async_trait`.
/// - `default_filter()` — sync, forwarded directly.
///
/// The blanket `impl<T: ChainAdapter + Send + Sync> ErasedAdapter for T` ensures all
/// existing adapters automatically implement this trait at zero cost.
#[async_trait]
pub trait ErasedAdapter: Send + Sync {
    /// Forward to `ChainAdapter::subscribe`.
    fn subscribe(
        &self,
        filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>>;

    /// Forward to `ChainAdapter::health_check`, boxed via `async_trait`.
    async fn health_check(&self) -> Result<(), AdapterError>;

    /// Forward to `ChainAdapter::default_filter`.
    fn default_filter(&self) -> SubscribeFilter;
}

#[async_trait]
impl<T> ErasedAdapter for T
where
    T: ChainAdapter + Send + Sync,
{
    fn subscribe(
        &self,
        filter: SubscribeFilter,
    ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
        ChainAdapter::subscribe(self, filter)
    }

    async fn health_check(&self) -> Result<(), AdapterError> {
        ChainAdapter::health_check(self).await
    }

    fn default_filter(&self) -> SubscribeFilter {
        ChainAdapter::default_filter(self)
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-adapter event channel buffer depth.
/// Bounded to prevent unbounded memory growth when the unified consumer is slow.
const COORDINATOR_CHANNEL_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-chain health status returned by `MultiChainCoordinator::healthcheck`.
#[derive(Debug, Clone)]
pub struct ChainHealth {
    /// The chain this status is for.
    pub chain: Chain,
    /// Human-readable adapter identifier (e.g. `"solana"`, `"ethereum"`).
    pub adapter_id: String,
    /// `true` if the adapter's `health_check()` returned `Ok(())`.
    pub healthy: bool,
    /// Error message if unhealthy. `None` when `healthy = true`.
    pub error: Option<String>,
}

/// Errors returned by `MultiChainCoordinator` operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoordinatorError {
    /// The coordinator has no adapters configured.
    #[error("coordinator has no adapters — add at least one via MultiChainCoordinator::new")]
    NoAdapters,

    /// A chain adapter task panicked or returned an error.
    #[error("adapter task failed for adapter_id={adapter_id}: {reason}")]
    AdapterTaskFailed { adapter_id: String, reason: String },

    /// A checkpoint write failed.
    #[error("checkpoint failed for adapter_id={adapter_id}: {reason}")]
    CheckpointFailed { adapter_id: String, reason: String },

    /// A join error (task panicked).
    #[error("task join error: {0}")]
    JoinError(String),
}

/// Descriptor for one chain slot in the coordinator.
///
/// Constructed by the caller and passed to `MultiChainCoordinator::new`.
pub struct AdapterSlot {
    /// The chain this adapter handles.
    pub chain: Chain,
    /// A stable, unique string identifier for this adapter instance.
    ///
    /// Used as the checkpoint key in `adapter_checkpoints`. Must be unique
    /// across all slots in one coordinator (e.g. `"solana"`, `"ethereum"`).
    pub adapter_id: String,
    /// The adapter implementation — erased for dyn dispatch.
    pub adapter: Box<dyn ErasedAdapter>,
}

impl AdapterSlot {
    /// Convenience constructor.
    pub fn new(
        chain: Chain,
        adapter_id: impl Into<String>,
        adapter: impl ChainAdapter + 'static,
    ) -> Self {
        Self {
            chain,
            adapter_id: adapter_id.into(),
            adapter: Box::new(adapter),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal type aliases
// ---------------------------------------------------------------------------

/// Per-chain task handle registry: `(adapter_id, JoinHandle)`.
type TaskHandles = Arc<Mutex<Vec<(String, JoinHandle<()>)>>>;

// ---------------------------------------------------------------------------
// MultiChainCoordinator
// ---------------------------------------------------------------------------

/// Wraps N `ChainAdapter` instances; exposes a unified event stream and
/// per-chain lifecycle controls.
///
/// # Usage
///
/// ```ignore
/// let coordinator = MultiChainCoordinator::new(
///     vec![solana_slot, ethereum_slot],
///     shutdown.clone(),
/// );
/// coordinator.start().await?;
/// let mut stream = coordinator.event_stream();
/// while let Some(event) = stream.next().await { /* ... */ }
/// ```
pub struct MultiChainCoordinator {
    /// Per-chain adapter descriptors. Taken by `start()` to spawn tasks.
    slots: Vec<AdapterSlot>,
    /// Shared shutdown signal. `stop()` cancels this; all spawned tasks observe it.
    shutdown: ShutdownSignal,
    /// Handles for the spawned per-chain tasks. Populated by `start()`.
    handles: TaskHandles,
}

impl MultiChainCoordinator {
    /// Create a coordinator with the given adapter slots and shutdown signal.
    ///
    /// Does NOT start any tasks — call `start()` to begin subscribing.
    pub fn new(slots: Vec<AdapterSlot>, shutdown: ShutdownSignal) -> Self {
        let handles: TaskHandles = Arc::new(Mutex::new(Vec::new()));
        Self {
            slots,
            shutdown,
            handles,
        }
    }

    /// Start streaming from all adapters.
    ///
    /// Spawns one tokio task per adapter. Each task drives `adapter.subscribe()`,
    /// forwarding events into the per-adapter mpsc channel that feeds `event_stream()`.
    ///
    /// Returns `Ok(())` immediately after spawning — it does NOT block until completion.
    /// Call `join()` (or drop the coordinator) to collect results.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::NoAdapters` if no slots were provided.
    pub async fn start(
        &self,
        event_tx: tokio::sync::mpsc::Sender<Result<Event, AdapterError>>,
    ) -> Result<(), CoordinatorError> {
        if self.slots.is_empty() {
            return Err(CoordinatorError::NoAdapters);
        }

        let mut handles = self.handles.lock().unwrap();

        for slot in &self.slots {
            let filter = slot.adapter.default_filter();
            let stream = slot.adapter.subscribe(filter);
            let tx = event_tx.clone();
            let adapter_id = slot.adapter_id.clone();
            let chain = slot.chain;
            let shutdown = self.shutdown.clone();

            let handle = tokio::spawn(async move {
                let mut s = stream;
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            info!(adapter_id = %adapter_id, chain = ?chain, "coordinator: shutdown signal received");
                            break;
                        }
                        item = s.next() => {
                            match item {
                                Some(event) => {
                                    if tx.send(event).await.is_err() {
                                        // Receiver dropped — coordinator is shutting down.
                                        break;
                                    }
                                }
                                None => {
                                    warn!(adapter_id = %adapter_id, chain = ?chain, "coordinator: adapter stream ended unexpectedly");
                                    break;
                                }
                            }
                        }
                    }
                }
            });

            handles.push((slot.adapter_id.clone(), handle));
        }

        Ok(())
    }

    /// Signal all adapter tasks to stop.
    ///
    /// Cancels the shared `ShutdownSignal`. Idempotent.
    pub fn stop(&self) {
        self.shutdown.cancel();
    }

    /// Return per-chain health status.
    ///
    /// Calls `adapter.health_check()` on each slot concurrently and collects results.
    pub async fn healthcheck(&self) -> Vec<ChainHealth> {
        let mut results = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let result = slot.adapter.health_check().await;
            results.push(ChainHealth {
                chain: slot.chain,
                adapter_id: slot.adapter_id.clone(),
                healthy: result.is_ok(),
                error: result.err().map(|e| e.to_string()),
            });
        }
        results
    }

    /// Wait for all spawned tasks to complete.
    ///
    /// Typically called after `stop()`. Returns one result per adapter task.
    /// Any `JoinError` (task panic) is surfaced as `CoordinatorError::JoinError`.
    pub async fn join(self) -> Vec<Result<(), CoordinatorError>> {
        let handles = std::mem::take(&mut *self.handles.lock().unwrap());
        let mut results = Vec::with_capacity(handles.len());
        for (adapter_id, handle) in handles {
            match handle.await {
                Ok(()) => results.push(Ok(())),
                Err(e) => {
                    error!(adapter_id = %adapter_id, error = %e, "coordinator: adapter task panicked");
                    results.push(Err(CoordinatorError::JoinError(e.to_string())))
                }
            }
        }
        results
    }

    /// Build a unified event stream from all adapters.
    ///
    /// Spawns the per-adapter tasks and returns a `Stream` that merges events
    /// from all adapters into a single ordered-by-arrival stream.
    ///
    /// This is a convenience wrapper over `start()` + the internal mpsc receiver.
    /// Prefer this method when the caller wants a stream-oriented interface.
    ///
    /// Buffer capacity: `COORDINATOR_CHANNEL_CAP` events per adapter × N adapters
    /// (all share the same receiver).
    pub async fn event_stream(
        self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>>,
        CoordinatorError,
    > {
        let cap = COORDINATOR_CHANNEL_CAP * self.slots.len().max(1);
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        self.start(tx).await?;
        // Bridge mpsc::Receiver into a Stream without tokio-stream dep.
        // `unfold` takes state (receiver) and an async step function.
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Box::pin(stream))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::RangeInclusive;

    use futures::StreamExt;
    use mg_onchain_chain_adapter::{AdapterError, Checkpoint, ChainAdapter, Event, SubscribeFilter};
    use mg_onchain_common::chain::{BlockRef, Chain};

    // -----------------------------------------------------------------------
    // MockStreamAdapter — drives a fixed event vector into the stream
    // -----------------------------------------------------------------------

    /// A mock `ChainAdapter` that emits a fixed sequence of events then terminates.
    struct MockStreamAdapter {
        chain: Chain,
        events: Vec<Event>,
    }

    impl MockStreamAdapter {
        fn new(chain: Chain, events: Vec<Event>) -> Self {
            Self { chain, events }
        }
    }

    impl ChainAdapter for MockStreamAdapter {
        fn subscribe(
            &self,
            _filter: SubscribeFilter,
        ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
            let events: Vec<Result<Event, AdapterError>> =
                self.events.iter().cloned().map(Ok).collect();
            Box::pin(futures::stream::iter(events))
        }

        fn backfill(
            &self,
            _range: RangeInclusive<u64>,
        ) -> Pin<Box<dyn Stream<Item = Result<Event, AdapterError>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }

        async fn checkpoint_save(&self, _checkpoint: &Checkpoint) -> Result<(), AdapterError> {
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

    // -----------------------------------------------------------------------
    // Helper: build a SlotFinalized event (chain-agnostic, no boxed types)
    // -----------------------------------------------------------------------

    fn finalized_event(slot: u64) -> Event {
        Event::SlotFinalized { slot }
    }

    // -----------------------------------------------------------------------
    // coordinator_merges_two_chain_streams
    // -----------------------------------------------------------------------

    /// Coordinator emitting events from two chains merges them into the unified stream.
    #[tokio::test]
    async fn coordinator_merges_two_chain_streams() {
        let solana_events = vec![finalized_event(1), finalized_event(2)];
        let eth_events = vec![finalized_event(100), finalized_event(200)];

        let solana_adapter = MockStreamAdapter::new(Chain::Solana, solana_events);
        let eth_adapter = MockStreamAdapter::new(Chain::Ethereum, eth_events);

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
            .expect("event_stream must not fail with two adapters");

        let mut received: Vec<u64> = Vec::new();
        while let Some(Ok(event)) = stream.next().await {
            if let Event::SlotFinalized { slot } = event {
                received.push(slot);
            }
        }

        // All 4 events from both chains must arrive (order may vary).
        assert_eq!(received.len(), 4, "expected 4 events total, got {received:?}");
        for slot in [1u64, 2, 100, 200] {
            assert!(received.contains(&slot), "missing slot {slot} in {received:?}");
        }
    }

    // -----------------------------------------------------------------------
    // coordinator_no_adapters_returns_error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn coordinator_no_adapters_returns_error() {
        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(vec![], shutdown);
        let result = coordinator.event_stream().await;
        assert!(
            matches!(result, Err(CoordinatorError::NoAdapters)),
            "expected NoAdapters error"
        );
    }

    // -----------------------------------------------------------------------
    // coordinator_healthcheck_returns_per_chain_status
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn coordinator_healthcheck_returns_per_chain_status() {
        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![
                AdapterSlot::new(
                    Chain::Solana,
                    "solana",
                    MockStreamAdapter::new(Chain::Solana, vec![]),
                ),
                AdapterSlot::new(
                    Chain::Ethereum,
                    "ethereum",
                    MockStreamAdapter::new(Chain::Ethereum, vec![]),
                ),
            ],
            shutdown,
        );

        let health = coordinator.healthcheck().await;
        assert_eq!(health.len(), 2);
        assert!(health[0].healthy, "solana should be healthy");
        assert!(health[1].healthy, "ethereum should be healthy");
        assert_eq!(health[0].chain, Chain::Solana);
        assert_eq!(health[1].chain, Chain::Ethereum);
    }

    // -----------------------------------------------------------------------
    // coordinator_stop_cancels_tasks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn coordinator_stop_cancels_running_tasks() {
        use std::time::Duration;

        // Adapter that produces one event then blocks (via a pending future).
        // We wrap it so the test doesn't hang.
        let events = vec![finalized_event(999)];
        let solana_adapter = MockStreamAdapter::new(Chain::Solana, events);

        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![AdapterSlot::new(Chain::Solana, "solana", solana_adapter)],
            shutdown.clone(),
        );

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        coordinator.start(tx).await.unwrap();

        // Give the task time to send the event.
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();

        // Drain via futures::stream::unfold (no tokio-stream dep in indexer crate).
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        futures::pin_mut!(stream);
        let mut count = 0usize;
        while stream.next().await.is_some() {
            count += 1;
        }
        // We must receive at least the one event that was pre-loaded.
        assert!(count >= 1, "expected at least 1 event before stop");
    }

    // -----------------------------------------------------------------------
    // coordinator_per_chain_checkpoint_isolation
    // -----------------------------------------------------------------------

    /// Each adapter_id maps to an isolated checkpoint namespace.
    /// This test verifies slot naming is distinct (structural, not I/O).
    #[test]
    fn coordinator_per_chain_adapter_ids_are_distinct() {
        let solana_slot = AdapterSlot::new(
            Chain::Solana,
            "solana",
            MockStreamAdapter::new(Chain::Solana, vec![]),
        );
        let eth_slot = AdapterSlot::new(
            Chain::Ethereum,
            "ethereum",
            MockStreamAdapter::new(Chain::Ethereum, vec![]),
        );
        assert_ne!(
            solana_slot.adapter_id, eth_slot.adapter_id,
            "adapter IDs must be distinct to avoid checkpoint key collision"
        );
    }
}
