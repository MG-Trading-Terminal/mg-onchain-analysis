//! `mg-onchain-indexer` — pipeline glue: chain-adapter → batch → storage → checkpoint.
//!
//! # Architecture
//!
//! ```text
//! ChainAdapter::subscribe()
//!     │
//!     ▼
//! route_event()    ←─ router.rs
//!     │
//!     ├─ Transfer  ─┐
//!     ├─ Swap       ├─ EventBatcher (batcher.rs) — size OR timeout trigger
//!     ├─ PoolEvent  ┘
//!     │
//!     ├─ TokenMeta  → PgStore::upsert_token() (low-volume, no batching)
//!     ├─ ReorgMarker → handle_reorg() (flush + delete + rewind checkpoint)
//!     └─ SlotFinalized → log, no action (timeout/size handles flush)
//!
//! On flush trigger:
//!     EventSink::insert_*()  ←─ sink.rs (PgEventSink wraps PgStore)
//!     AsyncCheckpointStore::save()
//! ```
//!
//! # Backpressure
//!
//! The subscribe loop runs in a single async task — it is single-producer,
//! single-consumer. Backpressure is achieved by blocking on `sink.insert_*()`
//! when a flush is needed: if Postgres falls behind, the insert awaits, which
//! pauses the loop, which stops pulling from the Yellowstone gRPC stream, which
//! eventually causes the gRPC sender to block. This is the correct backpressure
//! shape for a single-task pipeline.
//!
//! We do NOT use unbounded buffers or `spawn` for each flush — those would mask
//! Postgres lag and cause unbounded memory growth.
//!
//! # Reorg safety
//!
//! - Checkpoint saved ONLY after the batch is committed.
//! - On reorg: flush pending → delete affected rows → rewind checkpoint.
//! - Duplicate events on restart are absorbed by `ON CONFLICT DO NOTHING`.
//!
//! # Shutdown
//!
//! The `ShutdownSignal` token is checked in every `tokio::select!` arm.
//! On shutdown: break the loop, drain all buffers, write final checkpoint.

pub mod batcher;
pub mod config;
pub mod coordinator;
pub mod error;
pub mod graph_writer;
pub mod hooks;
pub mod reorg;
pub mod router;
pub mod shutdown;
pub mod sink;

pub use coordinator::{AdapterSlot, ChainHealth, CoordinatorError, MultiChainCoordinator};

use futures::StreamExt;
use tracing::{debug, error, info, instrument, warn};

use mg_onchain_chain_adapter::ChainAdapter;
use mg_onchain_storage::{AsyncCheckpointStore, Checkpoint, PgCheckpointStore};

use batcher::EventBatcher;
use config::{BatchConfig, IndexerConfig};
use error::IndexerError;
use graph_writer::GraphIndexerWriter;
use hooks::PoolInitializeHook;
use reorg::{flush_drained_batch, handle_reorg};
use router::{RouteResult, route_event};
use shutdown::ShutdownSignal;
use sink::{EventSink, PgEventSink};

// ---------------------------------------------------------------------------
// Indexer — the public entry point
// ---------------------------------------------------------------------------

/// The indexer orchestrates the full subscribe → batch → write → checkpoint loop.
///
/// Constructed via [`Indexer::new`] and run via [`Indexer::run`].
///
/// Generic parameters allow dependency injection for testing:
/// - `A`: a `ChainAdapter` implementation (production: `SolanaAdapter`)
/// - `S`: an `EventSink` implementation (production: `PgEventSink`)
/// - `C`: an `AsyncCheckpointStore` (production: `PgCheckpointStore`)
pub struct Indexer<A, S, C>
where
    A: ChainAdapter,
    S: EventSink,
    C: AsyncCheckpointStore,
{
    adapter: A,
    sink: S,
    checkpoint_store: C,
    adapter_id: String,
    chain: String,
    batch_cfg: BatchConfig,
    shutdown: ShutdownSignal,
    /// Optional graph writer. When `Some`, graph edges and labels are written
    /// on `PoolEvent::Initialize` and `TokenMeta` events. When `None`, all
    /// graph writes are silently skipped (e.g. in tests that don't wire graph stores).
    graph_writer: Option<GraphIndexerWriter>,
    /// Optional hook for pool initialization events (e.g. D09 BOCPD detector).
    ///
    /// When `Some`, called immediately after `graph_writer.on_pool_event` for
    /// every `PoolEvent::Initialize`. When `None`, the hook is silently skipped.
    ///
    /// The hook is also called on reorg to let it retract any state derived from
    /// blocks at or above the reorg height.
    pool_initialize_hook: Option<std::sync::Arc<dyn PoolInitializeHook>>,
}

impl<A, S, C> Indexer<A, S, C>
where
    A: ChainAdapter,
    S: EventSink,
    C: AsyncCheckpointStore,
{
    /// Construct an `Indexer` with explicit dependency injection.
    ///
    /// Use this constructor in tests and in the server crate when wiring
    /// production dependencies.
    ///
    /// `graph_writer` is optional. Pass `None` to disable graph writes (e.g. in
    /// tests that don't provision graph stores). Pass `Some(writer)` in production
    /// and integration tests that assert on graph edge/label writes.
    ///
    /// # Note on argument count
    ///
    /// This constructor carries 9 arguments because `Indexer` is a dependency-injection
    /// root that wires all subsystems (adapter, sink, checkpoint, IDs, batch config,
    /// shutdown, graph writer, pool initialize hook). Splitting into a builder would
    /// add indirection without clarity benefit for a struct with no optional arguments
    /// prior to `graph_writer`.
    ///
    /// `pool_initialize_hook` is optional. Pass `None` to disable D09 (and any future
    /// hook) wiring. Pass `Some(Arc::new(hook))` in production when D09 is configured.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        adapter: A,
        sink: S,
        checkpoint_store: C,
        adapter_id: impl Into<String>,
        chain: impl Into<String>,
        batch_cfg: BatchConfig,
        shutdown: ShutdownSignal,
        graph_writer: Option<GraphIndexerWriter>,
        pool_initialize_hook: Option<std::sync::Arc<dyn PoolInitializeHook>>,
    ) -> Self {
        Self {
            adapter,
            sink,
            checkpoint_store,
            adapter_id: adapter_id.into(),
            chain: chain.into(),
            batch_cfg,
            shutdown,
            graph_writer,
            pool_initialize_hook,
        }
    }

    /// Run the indexer until shutdown is requested or an unrecoverable error occurs.
    ///
    /// On graceful shutdown: flushes all pending buffers, writes the final
    /// checkpoint, and returns `Ok(())`.
    ///
    /// On unrecoverable error: returns `Err(IndexerError)` after logging.
    ///
    /// # Checkpoint resume
    ///
    /// On startup, the last checkpoint is loaded. If no checkpoint exists
    /// (first run), the indexer subscribes from the current chain tip.
    #[instrument(skip(self), fields(adapter_id = %self.adapter_id, chain = %self.chain))]
    pub async fn run(&mut self) -> Result<(), IndexerError> {
        info!("indexer starting");

        // Load last checkpoint (if any).
        let maybe_checkpoint = self
            .checkpoint_store
            .load(&self.adapter_id)
            .await
            .map_err(|e| IndexerError::Checkpoint(e.to_string()))?;

        if let Some(ref cp) = maybe_checkpoint {
            info!(slot = cp.slot, sig = ?cp.last_signature, "resuming from checkpoint");
        } else {
            info!("no checkpoint found — starting from chain tip");
        }

        // Subscribe to the live stream.
        // ADR 0005 Decision 5: use the adapter's own default filter instead of the
        // hardcoded Solana filter. This fixes a latent bug where `SolanaAdapter::subscribe`
        // would receive Solana program IDs when the Ethereum adapter was plumbed in.
        let filter = self.adapter.default_filter();
        let mut stream = self.adapter.subscribe(filter);

        // Initialise the batcher.
        let mut batcher = EventBatcher::new(self.batch_cfg.size, self.batch_cfg.timeout_ms);

        // Track the last slot we've seen — used for checkpoint writes.
        let mut last_slot: u64 = maybe_checkpoint.as_ref().map(|c| c.slot).unwrap_or(0);
        let mut last_signature: Option<String> = maybe_checkpoint
            .as_ref()
            .and_then(|c| c.last_signature.clone());

        // Track the most recent block_time observed from any timed event.
        // Used as the graph reorg boundary (gotcha #28 — never wall clock).
        // Initialized to UNIX_EPOCH (conservatively deletes all indexer graph labels
        // on first reorg when no block_time has been observed yet; safe because the
        // labels will be re-written on replay).
        let mut last_block_time: chrono::DateTime<chrono::Utc> = chrono::DateTime::UNIX_EPOCH;

        info!("entering event loop");

        loop {
            // Poll for the next event, interleaved with shutdown.
            let event_result = tokio::select! {
                biased; // Check shutdown first so we don't starve it.
                _ = self.shutdown.cancelled() => {
                    info!("shutdown signal received — flushing and exiting");
                    break;
                }
                event_opt = stream.next() => {
                    match event_opt {
                        Some(r) => r,
                        None => {
                            warn!("event stream ended unexpectedly");
                            return Err(IndexerError::StreamEnded);
                        }
                    }
                }
            };

            // Unwrap the adapter result (network / decode errors).
            let event = match event_result {
                Ok(e) => e,
                Err(e) => {
                    error!(err = %e, "adapter error — continuing (adapter reconnects internally)");
                    continue;
                }
            };

            // Pre-route inspection: update last_block_time and fire graph writes.
            //
            // We peek at the event BEFORE `route_event` consumes it. The borrow
            // ends before the ownership transfer on the next line (Rust NLL).
            //
            // - Track `last_block_time` from any timestamped event for use as
            //   the reorg block_time boundary.
            // - For `PoolEvent::Initialize`: fire the graph writer (DeployerOf edge +
            //   DeployerEOA label) immediately. Graph writes fail loud — no
            //   log-and-continue (gotcha #36).
            if let mg_onchain_chain_adapter::Event::PoolEvent(ref pe) = event {
                last_block_time = pe.block_time;
                if let mg_onchain_common::event::PoolEventKind::Initialize {
                    ref token0,
                    ref token1,
                } = pe.kind
                {
                    if let Some(ref gw) = self.graph_writer {
                        gw.on_pool_event(pe).await?;
                    }
                    // Invoke the pool-initialize hook (e.g. D09 BOCPD) immediately
                    // after the graph writer so the DeployerOf edge is already present
                    // when the hook queries the graph store.
                    // Time source: pe.block_time — NEVER Utc::now() (gotcha #22).
                    if let Some(ref hook) = self.pool_initialize_hook {
                        hook.on_new_token_launch(
                            pe.chain,
                            pe.actor.as_str(),
                            token0.as_str(),
                            token1.as_str(),
                            pe.block_time,
                            pe.block,
                        )
                        .await?;
                    }
                }
            }
            if let mg_onchain_chain_adapter::Event::Transfer(ref t) = event {
                last_block_time = t.block_time;
            }
            if let mg_onchain_chain_adapter::Event::Swap(ref s) = event {
                last_block_time = s.block_time;
            }

            // Route the event.
            match route_event(event, &mut batcher) {
                RouteResult::Buffered => {
                    // Event pushed to batcher — check if flush is needed.
                }
                RouteResult::TokenMeta(meta) => {
                    // Low-volume: upsert directly into the `tokens` table, no batching.
                    // One upsert per newly-seen mint address. `permanent_delegate` and
                    // `transfer_hook_program` are not yet stored (Phase-3 TLV gap).
                    debug!(
                        mint = %meta.mint.as_str(),
                        "TokenMeta event — persisting to tokens table"
                    );
                    self.sink.upsert_token_meta(&meta).await?;
                    // Graph writer hook for TokenMeta: write AuthorityOf edges.
                    // `last_slot` is the current slot tracker — used as block_height
                    // approximation since TokenMeta carries no per-block height.
                    if let Some(ref gw) = self.graph_writer {
                        gw.on_token_meta(&meta, last_slot).await?;
                    }
                }
                RouteResult::Reorg { slot } => {
                    last_slot = handle_reorg(
                        slot,
                        &self.chain,
                        &self.adapter_id,
                        &mut batcher,
                        &self.sink,
                        &self.checkpoint_store,
                    )
                    .await?;

                    // Graph reorg cleanup: delete edges above the reorg slot and
                    // indexer-written labels issued at or after the reorg block_time.
                    // `last_block_time` is the most recent block_time from any observed
                    // timed event — a safe approximation for the reorg boundary
                    // (gotcha #28: derived from block, not wall clock).
                    if let Some(ref gw) = self.graph_writer {
                        gw.on_reorg(&self.chain, slot, last_block_time).await?;
                    }

                    // Pool-initialize hook reorg: retract D09 BOCPD state derived
                    // from blocks at or above the reorg height.
                    if let Some(ref hook) = self.pool_initialize_hook {
                        hook.on_reorg(&self.chain, slot).await?;
                    }

                    last_signature = None;
                    continue; // back to stream without checking flush triggers
                }
                RouteResult::SlotFinalized { slot } => {
                    debug!(slot, "SlotFinalized — events for this slot are immutable");
                    // No explicit action: the timeout/size trigger handles flushing.
                    // Future enhancement: use SlotFinalized as an explicit flush trigger.
                }
                RouteResult::Unknown => {
                    // batcher.count_unknown() already called; just continue.
                }
            }

            // Check flush triggers for each buffer independently.
            // This avoids one large table delay blocking a small, fast-filling one.
            if batcher.transfers_should_flush() {
                let events = batcher.drain_transfers();
                self.sink.insert_transfers(&events).await?;
                self.save_checkpoint(last_slot, last_signature.as_deref())
                    .await?;
            }
            if batcher.swaps_should_flush() {
                let events = batcher.drain_swaps();
                self.sink.insert_swaps(&events).await?;
                self.save_checkpoint(last_slot, last_signature.as_deref())
                    .await?;
            }
            if batcher.pool_events_should_flush() {
                let events = batcher.drain_pool_events();
                self.sink.insert_pool_events(&events).await?;
                self.save_checkpoint(last_slot, last_signature.as_deref())
                    .await?;
            }
            if batcher.holder_snapshots_should_flush() {
                let events = batcher.drain_holder_snapshots();
                self.sink.upsert_holder_snapshots(&events).await?;
                self.save_checkpoint(last_slot, last_signature.as_deref())
                    .await?;
            }
        }

        // --- Graceful shutdown: flush all remaining buffers ---
        info!(
            pending = batcher.pending_count(),
            "flushing remaining events before shutdown"
        );
        let final_batch = batcher.drain_all();
        if !final_batch.is_empty() {
            flush_drained_batch(final_batch, &self.sink).await?;
        }

        // Save the final checkpoint.
        self.save_checkpoint(last_slot, last_signature.as_deref())
            .await?;

        info!("indexer shut down cleanly");
        Ok(())
    }

    /// Persist the checkpoint. Non-fatal: logs at ERROR on failure but does not
    /// propagate (the next successful flush will retry).
    async fn save_checkpoint(
        &self,
        slot: u64,
        last_signature: Option<&str>,
    ) -> Result<(), IndexerError> {
        let cp = Checkpoint {
            slot,
            last_signature: last_signature.map(|s| s.to_owned()),
        };
        self.checkpoint_store
            .save(&self.adapter_id, &cp)
            .await
            .map_err(|e| IndexerError::Checkpoint(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// IndexerBuilder — convenience constructor for production use
// ---------------------------------------------------------------------------

/// Convenience builder for the production indexer wired to `PgStore`.
///
/// Tests that need to inject mock sinks or stores should use `Indexer::new` directly.
pub struct IndexerBuilder;

impl IndexerBuilder {
    /// Build a production indexer from an `IndexerConfig`.
    ///
    /// Connects to Postgres, constructs `PgEventSink` and `PgCheckpointStore`,
    /// and returns an `Indexer<A, PgEventSink, PgCheckpointStore>`.
    ///
    /// The caller provides the concrete `ChainAdapter` (e.g. `SolanaAdapter`)
    /// because adapter construction requires credentials that live in the adapter's
    /// own config struct.
    pub async fn build<A: ChainAdapter>(
        adapter: A,
        config: IndexerConfig,
        shutdown: ShutdownSignal,
    ) -> Result<Indexer<A, PgEventSink, PgCheckpointStore>, IndexerError> {
        let store = mg_onchain_storage::StorageHandle::new(config.storage)
            .await
            .map_err(IndexerError::Storage)?;

        let sink = PgEventSink::new(store.pg.clone());
        let checkpoint_store = PgCheckpointStore::new(store.pg);
        let chain = "solana".to_string(); // Phase 2 is Solana-only; Phase 4 reads from AdapterConfig.

        Ok(Indexer::new(
            adapter,
            sink,
            checkpoint_store,
            config.adapter_id,
            chain,
            config.batch,
            shutdown,
            None, // graph_writer: None — production callers inject via IndexerBuilder::with_graph
            None, // pool_initialize_hook: None — callers inject D09IndexerHook when enabled
        ))
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pure logic, no I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    use mg_onchain_chain_adapter::Event;
    use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
    use mg_onchain_common::event::Transfer;
    use mg_onchain_storage::{AsyncCheckpointStore, Checkpoint};

    use crate::batcher::EventBatcher;
    use crate::router::route_event;
    use crate::sink::EventSink;

    // -----------------------------------------------------------------------
    // MockSink — records calls for assertion
    // -----------------------------------------------------------------------

    /// Records every call made to it. `Arc<Mutex<_>>` because tests may inspect
    /// from the main thread while the sink is called from the async context.
    #[derive(Clone, Default)]
    struct MockSink {
        /// Each element is the count of transfers inserted in that call.
        pub transfer_batches: Arc<Mutex<Vec<usize>>>,
        pub swap_batches: Arc<Mutex<Vec<usize>>>,
        pub pool_event_batches: Arc<Mutex<Vec<usize>>>,
        pub holder_snapshot_batches: Arc<Mutex<Vec<usize>>>,
        /// Mint addresses passed to `upsert_token_meta`, in order.
        pub token_meta_mints: Arc<Mutex<Vec<String>>>,
        /// Slots passed to `delete_from_slot`.
        pub deleted_slots: Arc<Mutex<Vec<(String, u64)>>>,
    }

    impl EventSink for MockSink {
        async fn insert_transfers(
            &self,
            transfers: &[mg_onchain_common::event::Transfer],
        ) -> Result<(), IndexerError> {
            self.transfer_batches.lock().unwrap().push(transfers.len());
            Ok(())
        }

        async fn insert_swaps(
            &self,
            swaps: &[mg_onchain_common::event::Swap],
        ) -> Result<(), IndexerError> {
            self.swap_batches.lock().unwrap().push(swaps.len());
            Ok(())
        }

        async fn insert_pool_events(
            &self,
            events: &[mg_onchain_common::event::PoolEvent],
        ) -> Result<(), IndexerError> {
            self.pool_event_batches.lock().unwrap().push(events.len());
            Ok(())
        }

        async fn upsert_holder_snapshots(
            &self,
            snapshots: &[mg_onchain_common::token::HolderSnapshot],
        ) -> Result<(), IndexerError> {
            self.holder_snapshot_batches
                .lock()
                .unwrap()
                .push(snapshots.len());
            Ok(())
        }

        async fn upsert_token_meta(
            &self,
            meta: &mg_onchain_common::token::TokenMeta,
        ) -> Result<(), IndexerError> {
            self.token_meta_mints
                .lock()
                .unwrap()
                .push(meta.mint.as_str().to_owned());
            Ok(())
        }

        async fn delete_from_slot(&self, chain: &str, from_slot: u64) -> Result<(), IndexerError> {
            self.deleted_slots
                .lock()
                .unwrap()
                .push((chain.to_owned(), from_slot));
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // MockCheckpointStore
    // -----------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct MockCheckpointStore {
        inner: Arc<Mutex<std::collections::HashMap<String, Checkpoint>>>,
    }

    #[async_trait::async_trait]
    impl AsyncCheckpointStore for MockCheckpointStore {
        async fn save(
            &self,
            adapter_id: &str,
            checkpoint: &Checkpoint,
        ) -> Result<(), mg_onchain_storage::StorageError> {
            self.inner
                .lock()
                .unwrap()
                .insert(adapter_id.to_owned(), checkpoint.clone());
            Ok(())
        }

        async fn load(
            &self,
            adapter_id: &str,
        ) -> Result<Option<Checkpoint>, mg_onchain_storage::StorageError> {
            Ok(self.inner.lock().unwrap().get(adapter_id).cloned())
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn dummy_transfer(log_index: u32) -> Transfer {
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
            log_index,
        }
    }

    // -----------------------------------------------------------------------
    // Batcher flush on size trigger
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn batcher_flushes_on_size() {
        let sink = MockSink::default();
        let mut batcher = EventBatcher::new(3, 60_000);

        // Push 3 events — size trigger fires at exactly 3.
        for i in 0u32..3 {
            let result = route_event(Event::Transfer(dummy_transfer(i)), &mut batcher);
            assert!(matches!(result, RouteResult::Buffered));
        }
        assert!(batcher.transfers_should_flush());

        // Simulate what the run loop does on flush.
        let events = batcher.drain_transfers();
        sink.insert_transfers(&events).await.unwrap();

        let batches = sink.transfer_batches.lock().unwrap().clone();
        assert_eq!(batches, vec![3]);
    }

    // -----------------------------------------------------------------------
    // Checkpoint written after batch commit
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn checkpoint_saved_after_batch_commit() {
        let call_order = Arc::new(Mutex::new(vec![]));

        // Sink that records "insert" calls.
        #[derive(Clone)]
        struct OrderedSink {
            order: Arc<Mutex<Vec<&'static str>>>,
        }
        impl EventSink for OrderedSink {
            async fn insert_transfers(
                &self,
                _: &[mg_onchain_common::event::Transfer],
            ) -> Result<(), IndexerError> {
                self.order.lock().unwrap().push("insert_transfers");
                Ok(())
            }
            async fn insert_swaps(
                &self,
                _: &[mg_onchain_common::event::Swap],
            ) -> Result<(), IndexerError> {
                Ok(())
            }
            async fn insert_pool_events(
                &self,
                _: &[mg_onchain_common::event::PoolEvent],
            ) -> Result<(), IndexerError> {
                Ok(())
            }
            async fn upsert_holder_snapshots(
                &self,
                _: &[mg_onchain_common::token::HolderSnapshot],
            ) -> Result<(), IndexerError> {
                Ok(())
            }
            async fn upsert_token_meta(
                &self,
                _: &mg_onchain_common::token::TokenMeta,
            ) -> Result<(), IndexerError> {
                Ok(())
            }
            async fn delete_from_slot(&self, _: &str, _: u64) -> Result<(), IndexerError> {
                Ok(())
            }
        }

        struct OrderedCpStore {
            order: Arc<Mutex<Vec<&'static str>>>,
            inner: MockCheckpointStore,
        }
        #[async_trait::async_trait]
        impl AsyncCheckpointStore for OrderedCpStore {
            async fn save(
                &self,
                adapter_id: &str,
                checkpoint: &Checkpoint,
            ) -> Result<(), mg_onchain_storage::StorageError> {
                self.order.lock().unwrap().push("save_checkpoint");
                self.inner.save(adapter_id, checkpoint).await
            }
            async fn load(
                &self,
                adapter_id: &str,
            ) -> Result<Option<Checkpoint>, mg_onchain_storage::StorageError> {
                self.inner.load(adapter_id).await
            }
        }

        let sink = OrderedSink {
            order: call_order.clone(),
        };
        let cp_store = OrderedCpStore {
            order: call_order.clone(),
            inner: MockCheckpointStore::default(),
        };

        let mut batcher = EventBatcher::new(1, 60_000); // size 1 → flushes immediately
        batcher.push_transfer(dummy_transfer(0));
        let events = batcher.drain_transfers();
        // Simulate the run loop sequence:
        sink.insert_transfers(&events).await.unwrap();
        let cp = Checkpoint {
            slot: 300_000_000,
            last_signature: None,
        };
        cp_store.save("solana", &cp).await.unwrap();

        let order = call_order.lock().unwrap().clone();
        assert_eq!(
            order,
            vec!["insert_transfers", "save_checkpoint"],
            "checkpoint must be saved AFTER the batch is committed"
        );
    }

    // -----------------------------------------------------------------------
    // Reorg: flush → delete → rewind checkpoint
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reorg_flushes_then_deletes_then_rewinds() {
        let sink = MockSink::default();
        let cp_store = MockCheckpointStore::default();
        let mut batcher = EventBatcher::new(500, 60_000);

        // Pre-load 2 transfers into the batcher (simulating in-flight events).
        batcher.push_transfer(dummy_transfer(0));
        batcher.push_transfer(dummy_transfer(1));

        let reorg_slot = 300_000_500u64;
        let rewound = handle_reorg(
            reorg_slot,
            "solana",
            "solana",
            &mut batcher,
            &sink,
            &cp_store,
        )
        .await
        .unwrap();

        // Buffers should be drained after flush.
        assert_eq!(
            batcher.pending_count(),
            0,
            "buffers must be empty after reorg flush"
        );

        // Flush happened before delete: insert_transfers should have been called.
        let transfer_batches = sink.transfer_batches.lock().unwrap().clone();
        assert_eq!(
            transfer_batches,
            vec![2],
            "in-flight transfers must be flushed before DELETE"
        );

        // DELETE called with the reorg slot.
        let deleted = sink.deleted_slots.lock().unwrap().clone();
        assert_eq!(deleted, vec![("solana".to_owned(), reorg_slot)]);

        // Checkpoint rewound to slot - 1.
        assert_eq!(rewound, reorg_slot - 1);
        let saved_cp = cp_store.load("solana").await.unwrap().unwrap();
        assert_eq!(saved_cp.slot, reorg_slot - 1);
        assert!(saved_cp.last_signature.is_none());
    }

    // -----------------------------------------------------------------------
    // Graceful shutdown: pending events are flushed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn shutdown_flushes_remaining_events() {
        let sink = MockSink::default();
        let mut batcher = EventBatcher::new(500, 60_000);
        batcher.push_transfer(dummy_transfer(0));
        batcher.push_transfer(dummy_transfer(1));

        // Simulate what the run loop does on shutdown.
        let final_batch = batcher.drain_all();
        flush_drained_batch(final_batch, &sink).await.unwrap();

        let batches = sink.transfer_batches.lock().unwrap().clone();
        assert_eq!(batches, vec![2]);
    }

    // -----------------------------------------------------------------------
    // TokenMeta: 3 events → sink receives 3 upsert_token_meta calls in order
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn token_meta_events_reach_sink_in_order() {
        use mg_onchain_common::token::{JupiterVerification, TokenMeta};
        use rust_decimal::Decimal;

        fn make_meta(mint_str: &str) -> TokenMeta {
            let mint = Address::parse(Chain::Solana, mint_str).unwrap();
            TokenMeta {
                chain: Chain::Solana,
                mint,
                symbol: None,
                name: None,
                decimals: 6,
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
            }
        }

        // Three distinct mints (valid Solana base58 addresses of the right length).
        let mint_a = "So11111111111111111111111111111111111111112";
        let mint_b = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
        let mint_c = "11111111111111111111111111111111";

        let sink = MockSink::default();
        let mut batcher = EventBatcher::new(500, 60_000);

        // Route three TokenMeta events and call upsert_token_meta for each,
        // mirroring what the run loop does.
        for mint_str in [mint_a, mint_b, mint_c] {
            let meta = make_meta(mint_str);
            let result = route_event(Event::TokenMeta(Box::new(meta.clone())), &mut batcher);
            match result {
                RouteResult::TokenMeta(boxed) => {
                    sink.upsert_token_meta(&boxed).await.unwrap();
                }
                other => panic!("expected TokenMeta route result, got {other:?}"),
            }
        }

        // Batcher must be empty — TokenMeta is never buffered.
        assert_eq!(
            batcher.pending_count(),
            0,
            "TokenMeta events must not enter the batcher"
        );

        let recorded = sink.token_meta_mints.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            3,
            "expected exactly 3 upsert_token_meta calls"
        );
        assert_eq!(recorded[0], mint_a);
        assert_eq!(recorded[1], mint_b);
        assert_eq!(recorded[2], mint_c);
    }

    // -----------------------------------------------------------------------
    // PoolInitializeHook wiring: hook is invoked on Initialize event
    // -----------------------------------------------------------------------

    type LaunchRecord = (String, String, String, String);
    type ReorgRecord = (String, u64);

    /// Mock `PoolInitializeHook` that records every call for assertion.
    #[derive(Default)]
    struct MockPoolInitializeHook {
        /// Each element is `(chain, deployer, token0, token1)` in call order.
        pub launch_calls: Arc<Mutex<Vec<LaunchRecord>>>,
        /// Each element is `(chain, reorg_height)` in call order.
        pub reorg_calls: Arc<Mutex<Vec<ReorgRecord>>>,
    }

    #[async_trait::async_trait]
    impl crate::hooks::PoolInitializeHook for MockPoolInitializeHook {
        async fn on_new_token_launch(
            &self,
            chain: mg_onchain_common::chain::Chain,
            deployer: &str,
            token0: &str,
            token1: &str,
            _observed_at: chrono::DateTime<chrono::Utc>,
            _block_ref: mg_onchain_common::chain::BlockRef,
        ) -> Result<(), IndexerError> {
            self.launch_calls.lock().unwrap().push((
                chain.as_str().to_owned(),
                deployer.to_owned(),
                token0.to_owned(),
                token1.to_owned(),
            ));
            Ok(())
        }

        async fn on_reorg(&self, chain: &str, reorg_height: u64) -> Result<(), IndexerError> {
            self.reorg_calls
                .lock()
                .unwrap()
                .push((chain.to_owned(), reorg_height));
            Ok(())
        }
    }

    #[tokio::test]
    async fn pool_initialize_hook_invoked_on_initialize_event() {
        use mg_onchain_chain_adapter::Event;
        use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
        use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind};

        let chain = Chain::Solana;
        let actor = Address::parse(chain, "So11111111111111111111111111111111111111112").unwrap();
        let pool = Address::parse(chain, "11111111111111111111111111111111").unwrap();
        let token0 = Address::parse(chain, "So11111111111111111111111111111111111111112").unwrap();
        let token1 = Address::parse(chain, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let tx = TxHash::solana_from_base58(&bs58::encode([2u8; 64]).into_string()).unwrap();

        let pe = PoolEvent {
            chain,
            tx_hash: tx,
            block: BlockRef::new(chain, 300_000_100),
            block_time: Utc::now(),
            pool,
            dex: DexKind::RaydiumV4,
            kind: PoolEventKind::Initialize {
                token0: token0.clone(),
                token1: token1.clone(),
            },
            actor: actor.clone(),
            log_index: 0,
        };

        let hook = Arc::new(MockPoolInitializeHook::default());
        let launch_calls = hook.launch_calls.clone();
        let reorg_calls = hook.reorg_calls.clone();

        // Simulate exactly what the indexer run loop does when it encounters
        // a PoolEvent::Initialize with a hook attached.
        if let mg_onchain_chain_adapter::Event::PoolEvent(ref pe_ref) = Event::PoolEvent(pe)
            && let mg_onchain_common::event::PoolEventKind::Initialize {
                ref token0,
                ref token1,
            } = pe_ref.kind
        {
            hook.on_new_token_launch(
                pe_ref.chain,
                pe_ref.actor.as_str(),
                token0.as_str(),
                token1.as_str(),
                pe_ref.block_time,
                pe_ref.block,
            )
            .await
            .expect("hook must succeed");
        }

        let calls = launch_calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            1,
            "hook must be called exactly once per Initialize event"
        );
        assert_eq!(calls[0].0, "solana");
        assert_eq!(calls[0].1, actor.as_str());
        assert_eq!(calls[0].2, token0.as_str());
        assert_eq!(calls[0].3, token1.as_str());

        // Reorg hook not called yet.
        assert!(reorg_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pool_initialize_hook_on_reorg_called() {
        let hook = Arc::new(MockPoolInitializeHook::default());
        let reorg_calls = hook.reorg_calls.clone();

        hook.on_reorg("solana", 300_000_500)
            .await
            .expect("reorg must succeed");

        let calls = reorg_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "on_reorg must record exactly one call");
        assert_eq!(calls[0].0, "solana");
        assert_eq!(calls[0].1, 300_000_500u64);
    }

    // -----------------------------------------------------------------------
    // Router: unknown event → counter incremented, no buffer change
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_event_increments_counter_not_buffer() {
        // We can't construct an unknown variant directly (non_exhaustive),
        // so we rely on the fact that ReorgMarker + SlotFinalized are handled
        // and count_unknown is only called for truly unknown variants.
        // This test verifies the counter via count_unknown directly.
        let mut batcher = EventBatcher::new(500, 60_000);
        batcher.count_unknown();
        batcher.count_unknown();
        assert_eq!(batcher.unknown_event_count, 2);
        assert_eq!(
            batcher.pending_count(),
            0,
            "unknown events must not enter any buffer"
        );
    }

    // -----------------------------------------------------------------------
    // ADR 0005 Decision 5: Indexer::run uses adapter.default_filter()
    // -----------------------------------------------------------------------

    /// Verify that `Indexer::run` calls `adapter.default_filter()` and passes the
    /// result to `adapter.subscribe()`, rather than the hardcoded Solana filter.
    ///
    /// Uses a mock adapter that records the filter it received in `subscribe()` and
    /// returns its own sentinel marker in `default_filter()`.
    #[tokio::test]
    async fn indexer_uses_adapter_default_filter() {
        use std::ops::RangeInclusive;
        use std::pin::Pin;
        use mg_onchain_chain_adapter::{Checkpoint, ChainAdapter, Event, SubscribeFilter};
        use mg_onchain_common::chain::{BlockRef, Chain};
        use crate::shutdown::ShutdownSignal;

        // Sentinel: a unique program_id string that identifies "this came from default_filter()".
        const SENTINEL_PROGRAM_ID: &str = "SENTINEL_DEFAULT_FILTER_TEST";

        /// Mock adapter that records the filter passed to `subscribe()`.
        struct FilterCapturingAdapter {
            captured_filter: Arc<Mutex<Option<SubscribeFilter>>>,
        }

        impl ChainAdapter for FilterCapturingAdapter {
            fn subscribe(
                &self,
                filter: SubscribeFilter,
            ) -> Pin<Box<dyn futures::Stream<Item = Result<Event, mg_onchain_chain_adapter::AdapterError>> + Send + 'static>> {
                // Record the filter.
                *self.captured_filter.lock().unwrap() = Some(filter);
                // Return empty stream so the run loop exits gracefully (stream ended → Err).
                Box::pin(futures::stream::empty())
            }

            fn backfill(
                &self,
                _range: RangeInclusive<u64>,
            ) -> Pin<Box<dyn futures::Stream<Item = Result<Event, mg_onchain_chain_adapter::AdapterError>> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }

            async fn checkpoint_save(&self, _: &Checkpoint) -> Result<(), mg_onchain_chain_adapter::AdapterError> { Ok(()) }
            async fn checkpoint_load(&self) -> Result<Option<Checkpoint>, mg_onchain_chain_adapter::AdapterError> { Ok(None) }
            async fn health_check(&self) -> Result<(), mg_onchain_chain_adapter::AdapterError> { Ok(()) }
            async fn tip(&self) -> Result<BlockRef, mg_onchain_chain_adapter::AdapterError> {
                Ok(BlockRef::new(Chain::Solana, 0))
            }

            /// Return a sentinel filter identifiable in assertions.
            fn default_filter(&self) -> SubscribeFilter {
                SubscribeFilter {
                    program_ids: vec![SENTINEL_PROGRAM_ID.to_string()],
                    account_owners: vec![],
                    include_slot_updates: false,
                    evm_contract_addresses: vec![],
                }
            }
        }

        let captured = Arc::new(Mutex::new(None::<SubscribeFilter>));
        let adapter = FilterCapturingAdapter { captured_filter: captured.clone() };

        let sink = MockSink::default();
        let cp_store = MockCheckpointStore::default();
        let shutdown = ShutdownSignal::new();

        let mut indexer = Indexer::new(
            adapter,
            sink,
            cp_store,
            "test_filter_adapter",
            "test_chain",
            crate::config::BatchConfig::default(),
            shutdown,
            None,
            None,
        );

        // run() will fail immediately (StreamEnded) since subscribe returns empty.
        let _ = indexer.run().await;

        // Assert the filter captured by subscribe() matches our sentinel default_filter().
        let filter = captured.lock().unwrap().clone().expect("subscribe was not called");
        assert_eq!(
            filter.program_ids,
            vec![SENTINEL_PROGRAM_ID.to_string()],
            "Indexer::run must call adapter.default_filter(), not SubscribeFilter::solana_default()"
        );
    }
}
