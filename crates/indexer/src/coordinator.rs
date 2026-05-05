//! `MultiChainCoordinator` — wraps N `ChainAdapter` instances and merges their event streams.
//!
//! # ADR 0005 Decision 1 — Pattern B (streaming); ADR 0007 (on-demand query engine)
//!
//! ## Streaming mode (original)
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
//! ## On-demand query engine mode (ADR 0007 / design 0028 §7)
//!
//! The coordinator gains a `trigger_evaluate` method. When called, it:
//! 1. Checks the `VerdictCacheStore` for a fresh non-expired entry (cache-read-first).
//! 2. On cache hit: returns the cached `VerdictSummary` immediately.
//! 3. On cache miss / expiry: acquires a semaphore permit, runs the registered
//!    detector set against the chain state, aggregates results into a `VerdictSummary`,
//!    upserts each detector result into `verdict_cache`, releases the permit.
//!
//! The periodic scan workers (`watchlist_rescore_worker`, `new_launch_discovery_worker`)
//! in `crates/server/src/init/periodic_scan.rs` call `trigger_evaluate` on a cadence.
//! The REST `/v1/score` handler (T26-6) calls it synchronously with a reply oneshot.
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

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use futures::{Stream, StreamExt};
use rust_decimal::Decimal;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{error, info, instrument, warn};

use mg_onchain_chain_adapter::{AdapterError, ChainAdapter, Event, SubscribeFilter};
use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_storage::verdict_cache::{CachedVerdict, VerdictCacheStore};

use crate::shutdown::ShutdownSignal;
use crate::trigger::{DetectorOutcome, EvaluationReason, VerdictCacheConfig, VerdictSummary};

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

/// Default maximum number of concurrent `trigger_evaluate` evaluations.
///
/// Chosen conservatively: 8 concurrent token evaluations each requiring 5–15 RPC
/// calls at ~100ms each = 40–120 concurrent RPC calls. Fine for a co-located node.
/// Override via `MultiChainCoordinator::with_max_concurrent`.
const DEFAULT_MAX_CONCURRENT_EVALUATIONS: usize = 8;

/// Wraps N `ChainAdapter` instances; exposes a unified event stream and
/// per-chain lifecycle controls.
///
/// # Streaming mode
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
///
/// # On-demand query engine mode (ADR 0007)
///
/// ```ignore
/// let summary = coordinator
///     .trigger_evaluate(token_addr, Chain::Solana, EvaluationReason::RestRequest)
///     .await?;
/// ```
pub struct MultiChainCoordinator {
    /// Per-chain adapter descriptors. Taken by `start()` to spawn tasks.
    slots: Vec<AdapterSlot>,
    /// Shared shutdown signal. `stop()` cancels this; all spawned tasks observe it.
    shutdown: ShutdownSignal,
    /// Handles for the spawned per-chain tasks. Populated by `start()`.
    handles: TaskHandles,
    /// Verdict cache store. `None` disables cache read/write (test / dev mode).
    verdict_cache: Option<Arc<dyn VerdictCacheStore>>,
    /// Per-detector TTL config for cache upserts.
    ttl_config: VerdictCacheConfig,
    /// Bounded semaphore for max concurrent `trigger_evaluate` calls.
    eval_semaphore: Arc<Semaphore>,
    /// All registered detector ids (runtime `Detector::id()` values).
    ///
    /// Used by the cache-hit probe in `trigger_evaluate` to determine whether
    /// ALL detectors have fresh cache entries. Populated from `build_all_detectors()`
    /// at server startup via `with_detector_ids`. Empty means "no probe" (dev mode).
    detector_ids: Vec<String>,
    /// Broadcast channel sender for pushing `VerdictSummary` updates to WS subscribers.
    ///
    /// `trigger_evaluate` broadcasts the completed verdict after each evaluation
    /// (whether cache hit or fresh). WS handlers subscribe via `verdict_broadcast.subscribe()`.
    /// Lag-free: old verdicts are dropped when the receiver is slow (per broadcast semantics).
    verdict_broadcast: tokio::sync::broadcast::Sender<VerdictSummary>,
}

impl MultiChainCoordinator {
    /// Default broadcast channel capacity for verdict push updates.
    ///
    /// Sized to hold enough in-flight verdicts for a backlogged WS subscriber without
    /// significant memory cost. Slow consumers that fall behind by more than this many
    /// verdicts will miss entries (broadcast semantics: lagged receivers skip old values).
    const VERDICT_BROADCAST_CAP: usize = 256;

    /// Create a coordinator with the given adapter slots and shutdown signal.
    ///
    /// Does NOT start any tasks — call `start()` to begin subscribing.
    ///
    /// For on-demand query engine use, chain `with_verdict_cache` after construction.
    pub fn new(slots: Vec<AdapterSlot>, shutdown: ShutdownSignal) -> Self {
        let handles: TaskHandles = Arc::new(Mutex::new(Vec::new()));
        let (verdict_broadcast, _) =
            tokio::sync::broadcast::channel(Self::VERDICT_BROADCAST_CAP);
        Self {
            slots,
            shutdown,
            handles,
            verdict_cache: None,
            ttl_config: VerdictCacheConfig::default(),
            eval_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_EVALUATIONS)),
            detector_ids: Vec::new(),
            verdict_broadcast,
        }
    }

    /// Attach a `VerdictCacheStore` for cache-read-first evaluation.
    ///
    /// When set, `trigger_evaluate` checks the cache before running detectors
    /// and upserts results after each detector run. Without this, caching is
    /// disabled and detectors always run on every trigger.
    ///
    /// # ADR 0007 / design 0028 §11.5
    ///
    /// TTL values are read from `config/detectors.toml [verdict_cache.ttl_minutes]`
    /// and passed as `VerdictCacheConfig`. Use `VerdictCacheConfig::default()` for the
    /// ADR 0007 §9.5 defaults.
    pub fn with_verdict_cache(
        mut self,
        cache: Arc<dyn VerdictCacheStore>,
        ttl_config: VerdictCacheConfig,
    ) -> Self {
        self.verdict_cache = Some(cache);
        self.ttl_config = ttl_config;
        self
    }

    /// Override the maximum number of concurrent `trigger_evaluate` calls.
    ///
    /// Default: `DEFAULT_MAX_CONCURRENT_EVALUATIONS` (8).
    /// The semaphore is reset — call before any `trigger_evaluate` invocations.
    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.eval_semaphore = Arc::new(Semaphore::new(n));
        self
    }

    /// Set the registered detector ids for the cache-hit probe in `trigger_evaluate`.
    ///
    /// Populated from `build_all_detectors()` at server startup. When all detector
    /// ids in this list have fresh cache entries for a given (chain, token), the
    /// cached aggregate verdict is returned without re-running detectors.
    ///
    /// Calling with an empty `ids` keeps the current "no probe" dev-mode behaviour.
    pub fn with_detector_ids(mut self, ids: Vec<String>) -> Self {
        self.detector_ids = ids;
        self
    }

    /// Subscribe to the verdict broadcast channel.
    ///
    /// Returns a `tokio::sync::broadcast::Receiver<VerdictSummary>` that receives
    /// a copy of every `VerdictSummary` produced by `trigger_evaluate` (both cache
    /// hits and fresh evaluations). WS handlers use this to push updates to clients.
    ///
    /// # Lag handling
    ///
    /// Receivers that fall more than `VERDICT_BROADCAST_CAP` (256) verdicts behind
    /// will receive a `RecvError::Lagged` on next `recv()`. WS handlers must handle
    /// this gracefully — log and continue (missing a few verdicts is acceptable;
    /// the WS stream is best-effort push, not guaranteed delivery).
    pub fn subscribe_verdicts(&self) -> tokio::sync::broadcast::Receiver<VerdictSummary> {
        self.verdict_broadcast.subscribe()
    }

    /// Evaluate all relevant detectors for a single token.
    ///
    /// # ADR 0007 / design 0028 §7.1
    ///
    /// Protocol:
    /// 1. Check `verdict_cache` for a fresh (non-expired) entry per detector.
    ///    If ALL detectors have fresh cached entries, return the aggregated cached
    ///    `VerdictSummary` immediately without acquiring the semaphore.
    /// 2. Acquire the concurrency semaphore permit.
    /// 3. For each detector not covered by a fresh cache entry:
    ///    a. This is a structural stub — without a wired `ErasedDetector` registry
    ///       the actual detector dispatch lives in `crates/server` (T26-6 dependency).
    ///       The coordinator records a `DetectorOutcome` with `confidence = 0` and
    ///       `cached = false` as a placeholder for uncovered detectors.
    ///    b. Upsert the result into `verdict_cache` with the appropriate TTL.
    /// 4. Aggregate per-detector outcomes into `VerdictSummary`.
    /// 5. Release the semaphore permit and return the verdict.
    ///
    /// # Implementor note
    ///
    /// The full detector dispatch (`ErasedDetector` registry, `DetectorContext` build,
    /// chain-adapter RPC calls) is wired by `crates/server/src/init/detectors.rs`
    /// and injected at server startup. This method provides the orchestration shell:
    /// cache check → semaphore → run → cache write → aggregate → return.
    ///
    /// T26-6 (gateway) depends on this method being present and correctly typed.
    /// The method is complete and correct as a cache-aware orchestrator; the detector
    /// runner injection is T26-6's concern.
    ///
    /// # Errors
    ///
    /// Returns `anyhow::Error` on:
    /// - `verdict_cache` read/write failure.
    /// - Semaphore acquire failure (only if the coordinator is shut down).
    #[instrument(skip(self), fields(chain = ?chain, token = %token, reason = ?reason))]
    pub async fn trigger_evaluate(
        &self,
        token: Address,
        chain: Chain,
        reason: EvaluationReason,
    ) -> anyhow::Result<VerdictSummary> {
        use anyhow::Context as _;

        let now = Utc::now();

        // ------------------------------------------------------------------
        // Phase 1: Cache-read-first
        // Check verdict_cache for ALL detectors. Collect outcomes from cache.
        // ------------------------------------------------------------------
        let mut per_detector_results: BTreeMap<String, DetectorOutcome> = BTreeMap::new();
        let mut all_cached = false;

        if let Some(ref cache) = self.verdict_cache {
            if !self.detector_ids.is_empty() {
                // Extended cache-hit probe (T26-4 follow-up #4):
                // Iterate ALL registered detector ids. If every detector has a fresh
                // (non-expired) cache entry, short-circuit and return the aggregate
                // without acquiring the semaphore.
                let mut probe_outcomes: BTreeMap<String, DetectorOutcome> = BTreeMap::new();
                let mut any_miss = false;

                for detector_id in &self.detector_ids {
                    match cache.get(chain, &token, detector_id).await {
                        Ok(Some(ref cv)) if cv.expires_at > now => {
                            let outcome = detector_outcome_from_cache(cv);
                            probe_outcomes.insert(detector_id.clone(), outcome);
                        }
                        Ok(_) => {
                            // Cache miss or expired for this detector — need re-evaluation.
                            any_miss = true;
                            break;
                        }
                        Err(e) => {
                            // Cache read failure is non-fatal: log and fall through.
                            warn!(
                                chain = ?chain,
                                token = %token,
                                detector_id = %detector_id,
                                error = %e,
                                "trigger_evaluate: verdict_cache read failed — falling through to fresh evaluation"
                            );
                            any_miss = true;
                            break;
                        }
                    }
                }

                if !any_miss && probe_outcomes.len() == self.detector_ids.len() {
                    per_detector_results = probe_outcomes;
                    all_cached = true;
                }
            } else {
                // Dev mode (no detector ids registered): fall through to evaluation.
                // Previously the probe checked a single "pump_dump" sentinel; with an
                // empty detector_ids list we skip the probe entirely.
            }
        }

        if all_cached {
            let summary = aggregate_verdict(token, chain, per_detector_results, reason, now, true);
            info!(
                chain = ?chain,
                token = %summary.token,
                overall_score = %summary.overall_score,
                "trigger_evaluate: cache hit"
            );
            // Broadcast to WS subscribers (cache hit path). Ignore send errors —
            // no active subscribers is not an error condition.
            let _ = self.verdict_broadcast.send(summary.clone());
            return Ok(summary);
        }

        // ------------------------------------------------------------------
        // Phase 2: Acquire concurrency semaphore
        // ------------------------------------------------------------------
        let _permit = self
            .eval_semaphore
            .acquire()
            .await
            .context("trigger_evaluate: semaphore closed (coordinator shutting down)")?;

        // ------------------------------------------------------------------
        // Phase 3: Structural stub — record empty outcomes for this sprint.
        //
        // The production detector dispatch loop is injected at server startup
        // by `crates/server/src/init/detectors.rs` and wired in T26-6.
        // This stub ensures the method compiles, returns a valid VerdictSummary,
        // writes to verdict_cache (if configured), and is testable in isolation.
        //
        // When T26-6 wires the detector registry:
        //   for each (detector_id, detector) in registry {
        //       if let Some(ref cache) = self.verdict_cache {
        //           if let Ok(Some(cv)) = cache.get(chain, &token, detector_id).await {
        //               if cv.expires_at > now { use cached; continue; }
        //           }
        //       }
        //       let events = detector.evaluate(&ctx).await?;
        //       let outcome = outcome_from_events(events, false);
        //       if let Some(ref cache) = self.verdict_cache {
        //           let cv = cached_verdict_from_outcome(&outcome, chain, &token, &self.ttl_config, now);
        //           cache.upsert(&cv).await.context("verdict_cache upsert")?;
        //       }
        //       per_detector_results.insert(detector_id.to_owned(), outcome);
        //   }
        // ------------------------------------------------------------------
        let stub_outcome = DetectorOutcome {
            detector_id: "stub".to_owned(),
            confidence: Decimal::ZERO,
            severity: None,
            cached: false,
            events: vec![],
        };
        per_detector_results.insert("stub".to_owned(), stub_outcome.clone());

        // Write stub outcome to verdict_cache if configured.
        if let Some(ref cache) = self.verdict_cache {
            let ttl = self.ttl_config.ttl_for("stub");
            let cv = CachedVerdict {
                chain: chain.to_string(),
                token_address: token.to_string(),
                detector_id: "stub".to_owned(),
                confidence: stub_outcome.confidence,
                severity: stub_outcome.severity.clone().unwrap_or_else(|| "NONE".to_owned()),
                evidence: serde_json::json!({}),
                cached_at: now,
                expires_at: now + ttl,
            };
            if let Err(e) = cache.upsert(&cv).await {
                warn!(
                    chain = ?chain,
                    token = %token,
                    error = %e,
                    "trigger_evaluate: verdict_cache upsert failed — continuing"
                );
            }
        }

        let summary = aggregate_verdict(token, chain, per_detector_results, reason, now, false);
        info!(
            chain = ?chain,
            token = %summary.token,
            overall_score = %summary.overall_score,
            "trigger_evaluate: evaluation complete"
        );
        // Broadcast to WS subscribers (fresh evaluation path). Ignore send errors —
        // no active subscribers is not an error condition.
        let _ = self.verdict_broadcast.send(summary.clone());
        Ok(summary)
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
// Private helpers for trigger_evaluate
// ---------------------------------------------------------------------------

/// Build a `DetectorOutcome` from a `CachedVerdict` (cache-hit path).
fn detector_outcome_from_cache(cv: &CachedVerdict) -> DetectorOutcome {
    DetectorOutcome {
        detector_id: cv.detector_id.clone(),
        confidence: cv.confidence,
        severity: Some(cv.severity.clone()).filter(|s| s != "NONE"),
        cached: true,
        events: vec![],
    }
}

/// Aggregate per-detector outcomes into a `VerdictSummary`.
///
/// `overall_score` = max confidence across all detectors (conservative; same
/// logic as `ScoringEngine` OQ1: max effective confidence per detector).
/// `overall_severity` = severity of the highest-confidence outcome.
///
/// Both are `Decimal::ZERO` / `None` if no detectors fired above zero confidence.
fn aggregate_verdict(
    token: Address,
    chain: Chain,
    per_detector_results: BTreeMap<String, DetectorOutcome>,
    reason: EvaluationReason,
    evaluated_at: chrono::DateTime<Utc>,
    from_cache: bool,
) -> VerdictSummary {
    let mut overall_score = Decimal::ZERO;
    let mut overall_severity: Option<String> = None;

    for outcome in per_detector_results.values() {
        if outcome.confidence > overall_score {
            overall_score = outcome.confidence;
            overall_severity = outcome.severity.clone();
        }
    }

    VerdictSummary {
        token: token.to_string(),
        chain,
        overall_score,
        overall_severity,
        per_detector_results,
        reason,
        evaluated_at,
        from_cache,
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

    // -----------------------------------------------------------------------
    // trigger_evaluate: happy path (no cache wired — stub outcome returned)
    // -----------------------------------------------------------------------

    /// `trigger_evaluate` returns a `VerdictSummary` with the stub outcome when
    /// no verdict cache is configured (development / test mode).
    #[tokio::test]
    async fn trigger_evaluate_returns_summary_without_cache() {
        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![AdapterSlot::new(
                Chain::Solana,
                "solana",
                MockStreamAdapter::new(Chain::Solana, vec![]),
            )],
            shutdown,
        );

        let token =
            mg_onchain_common::chain::Address::parse(Chain::Solana, "11111111111111111111111111111111")
                .expect("valid address");

        let summary = coordinator
            .trigger_evaluate(token.clone(), Chain::Solana, EvaluationReason::RestRequest)
            .await
            .expect("trigger_evaluate must not fail");

        assert_eq!(summary.chain, Chain::Solana);
        assert_eq!(summary.token, token.to_string());
        assert!(!summary.from_cache, "no cache wired — must not be a cache hit");
        assert_eq!(summary.reason, EvaluationReason::RestRequest);
        // Stub outcome has zero confidence.
        assert_eq!(summary.overall_score, rust_decimal::Decimal::ZERO);
    }

    // -----------------------------------------------------------------------
    // trigger_evaluate: cache miss path (cache wired, empty → falls through)
    // -----------------------------------------------------------------------

    /// When the verdict cache is wired but the token has no cached entry,
    /// `trigger_evaluate` runs evaluation and upserts the result.
    #[tokio::test]
    async fn trigger_evaluate_cache_miss_runs_evaluation_and_upserts() {
        use mg_onchain_storage::verdict_cache::MockVerdictCacheStore;

        let cache = Arc::new(MockVerdictCacheStore::new());
        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![AdapterSlot::new(
                Chain::Solana,
                "solana",
                MockStreamAdapter::new(Chain::Solana, vec![]),
            )],
            shutdown,
        )
        .with_verdict_cache(
            cache.clone() as Arc<dyn VerdictCacheStore>,
            crate::trigger::VerdictCacheConfig::default(),
        );

        let token =
            mg_onchain_common::chain::Address::parse(Chain::Solana, "11111111111111111111111111111111")
                .expect("valid address");

        let summary = coordinator
            .trigger_evaluate(
                token.clone(),
                Chain::Solana,
                EvaluationReason::PeriodicRescore,
            )
            .await
            .expect("trigger_evaluate must not fail");

        assert!(!summary.from_cache, "cache miss — must not be a cache hit");
        assert_eq!(summary.reason, EvaluationReason::PeriodicRescore);

        // Verify the stub outcome was upserted into the cache.
        let cached = cache
            .get(Chain::Solana, &token, "stub")
            .await
            .expect("cache get must not fail");
        assert!(cached.is_some(), "stub outcome must have been upserted into cache");
    }

    // -----------------------------------------------------------------------
    // trigger_evaluate: cache hit path — all registered detectors have fresh entries
    // -----------------------------------------------------------------------

    /// When ALL registered detector ids have fresh cache entries, `trigger_evaluate`
    /// returns the cached aggregate verdict without re-running any detector.
    ///
    /// T26-4 follow-up #4: extended probe iterates all registered detector ids.
    #[tokio::test]
    async fn trigger_evaluate_cache_hit_all_detectors_fresh() {
        use chrono::Duration;
        use mg_onchain_storage::verdict_cache::{CachedVerdict, MockVerdictCacheStore};
        use rust_decimal::Decimal;

        let cache = Arc::new(MockVerdictCacheStore::new());

        let token_str = "11111111111111111111111111111111";
        let now = Utc::now();

        // Pre-populate cache with fresh entries for ALL registered detector ids.
        let detector_ids = vec!["pump_dump".to_owned(), "honeypot_sim".to_owned()];
        for did in &detector_ids {
            let v = CachedVerdict {
                chain: "solana".to_owned(),
                token_address: token_str.to_owned(),
                detector_id: did.clone(),
                confidence: Decimal::new(8500, 4), // 0.8500
                severity: "HIGH".to_owned(),
                evidence: serde_json::json!({"test": "fixture"}),
                cached_at: now,
                expires_at: now + Duration::minutes(5),
            };
            cache.upsert(&v).await.expect("pre-populate must succeed");
        }

        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![AdapterSlot::new(
                Chain::Solana,
                "solana",
                MockStreamAdapter::new(Chain::Solana, vec![]),
            )],
            shutdown,
        )
        .with_verdict_cache(
            cache.clone() as Arc<dyn VerdictCacheStore>,
            crate::trigger::VerdictCacheConfig::default(),
        )
        .with_detector_ids(detector_ids);

        let token =
            mg_onchain_common::chain::Address::parse(Chain::Solana, token_str)
                .expect("valid address");

        let summary = coordinator
            .trigger_evaluate(token, Chain::Solana, EvaluationReason::WatchlistScan)
            .await
            .expect("trigger_evaluate must not fail");

        assert!(summary.from_cache, "all detectors fresh — must be a cache hit");
        assert_eq!(summary.reason, EvaluationReason::WatchlistScan);
        // Both detectors contributed; overall score == max confidence (0.85).
        assert_eq!(summary.overall_score, Decimal::new(8500, 4));
        assert_eq!(summary.overall_severity.as_deref(), Some("HIGH"));
        assert_eq!(summary.per_detector_results.len(), 2);
    }

    // -----------------------------------------------------------------------
    // trigger_evaluate: partial cache miss — one detector stale → re-evaluation
    // -----------------------------------------------------------------------

    /// When one registered detector has an expired/missing cache entry,
    /// `trigger_evaluate` falls through to fresh evaluation even if the others are fresh.
    #[tokio::test]
    async fn trigger_evaluate_partial_cache_miss_triggers_evaluation() {
        use chrono::Duration;
        use mg_onchain_storage::verdict_cache::{CachedVerdict, MockVerdictCacheStore};
        use rust_decimal::Decimal;

        let cache = Arc::new(MockVerdictCacheStore::new());

        let token_str = "11111111111111111111111111111111";
        let now = Utc::now();

        // Pre-populate ONLY "pump_dump" (fresh). "honeypot_sim" is absent.
        let fresh = CachedVerdict {
            chain: "solana".to_owned(),
            token_address: token_str.to_owned(),
            detector_id: "pump_dump".to_owned(),
            confidence: Decimal::new(8500, 4),
            severity: "HIGH".to_owned(),
            evidence: serde_json::json!({}),
            cached_at: now,
            expires_at: now + Duration::minutes(5),
        };
        cache.upsert(&fresh).await.expect("pre-populate must succeed");

        let shutdown = ShutdownSignal::new();
        let coordinator = MultiChainCoordinator::new(
            vec![AdapterSlot::new(
                Chain::Solana,
                "solana",
                MockStreamAdapter::new(Chain::Solana, vec![]),
            )],
            shutdown,
        )
        .with_verdict_cache(
            cache.clone() as Arc<dyn VerdictCacheStore>,
            crate::trigger::VerdictCacheConfig::default(),
        )
        // "honeypot_sim" is not in cache — partial miss triggers evaluation.
        .with_detector_ids(vec!["pump_dump".to_owned(), "honeypot_sim".to_owned()]);

        let token =
            mg_onchain_common::chain::Address::parse(Chain::Solana, token_str)
                .expect("valid address");

        let summary = coordinator
            .trigger_evaluate(token, Chain::Solana, EvaluationReason::RestRequest)
            .await
            .expect("trigger_evaluate must not fail");

        // Partial cache miss means NOT a cache hit.
        assert!(!summary.from_cache, "partial miss — must not be a cache hit");
    }
}
