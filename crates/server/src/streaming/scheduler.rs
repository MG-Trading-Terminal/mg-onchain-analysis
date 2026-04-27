//! `DetectorScheduler` — top-level streaming orchestrator.
//!
//! Subscribes to `invalidation_tx`, debounces events by `(Chain, Mint)` over
//! `debounce_window_ms`, then drains accumulated jobs into the bounded
//! `async_channel` queue consumed by the `SchedulerWorker` pool.
//!
//! # Debounce model
//!
//! A `BTreeMap<(Chain, Mint), PendingJob>` accumulates events.  On every
//! debounce interval tick, all pending jobs are drained into the queue.
//! If a `(Chain, Mint)` pair already has a pending job, the incoming
//! `slot_hints` are merged (union) and `observed_at` is updated to the max
//! block_time — `streaming_debounce_merge_total` is incremented.
//!
//! # Backpressure
//!
//! When the queue is full, `async_channel::Sender::try_send` returns `Full`.
//! The job is dropped and `streaming_queue_overflow_total` is incremented.
//! This is intentional — dropping a re-evaluation tick is safe; blocking the
//! scheduler loop (and thereby the broadcast channel receiver) is not.
//!
//! # Broadcast lag
//!
//! `broadcast::Receiver` can return `RecvError::Lagged(n)` if the scheduler
//! falls behind.  The scheduler logs the lag and increments
//! `streaming_queue_overflow_total` by `n` (approximation: we don't know
//! which tokens were in the skipped events, so we can't enqueue them).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, broadcast};
use tokio::time;
use tracing::{debug, instrument, warn};

use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_gateway::state::InvalidationEvent;

use crate::streaming::registry::StreamingRegistry;
use crate::streaming_config::StreamingConfig;
use crate::streaming_metrics::StreamingMetrics;

// ---------------------------------------------------------------------------
// SchedulerJob
// ---------------------------------------------------------------------------

/// A single unit of work consumed by `SchedulerWorker`.
#[derive(Debug, Clone)]
pub struct SchedulerJob {
    pub chain: Chain,
    pub mint: Address,
    /// Derived from `MAX(block_time)` over the accumulated `slot_hints`.
    /// Never wall-clock — determinism invariant.
    pub observed_at: DateTime<Utc>,
    pub slot_hints: Vec<u64>,
}

// ---------------------------------------------------------------------------
// DetectorScheduler
// ---------------------------------------------------------------------------

/// Orchestrates streaming detector re-evaluations.
///
/// Spawned as a single `tokio::task` in `crates/server/src/main.rs`.
pub struct DetectorScheduler {
    pub invalidation_rx: broadcast::Receiver<InvalidationEvent>,
    pub queue_tx: async_channel::Sender<SchedulerJob>,
    pub registry: Arc<RwLock<StreamingRegistry>>,
    pub config: StreamingConfig,
    pub metrics: Arc<StreamingMetrics>,
}

impl DetectorScheduler {
    /// Run the scheduler until the broadcast channel closes.
    ///
    /// Consumes `self`; meant to be wrapped in `tokio::spawn`.
    #[instrument(skip(self), name = "detector_scheduler")]
    pub async fn run(mut self) {
        let debounce = Duration::from_millis(self.config.debounce_window_ms);
        let mut debounce_ticker = time::interval(debounce);
        // Map of pending jobs, keyed by (chain, mint).
        // BTreeMap for deterministic drain order.
        let mut pending: BTreeMap<(Chain, String), PendingJob> = BTreeMap::new();

        loop {
            tokio::select! {
                // Debounce tick — drain pending map into queue.
                _ = debounce_ticker.tick() => {
                    let drained = std::mem::take(&mut pending);
                    for ((chain, mint_str), job) in drained {
                        let mint = match Address::parse(chain, &mint_str) {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(chain = %chain, mint = %mint_str, error = %e,
                                      "failed to parse mint address — skipping job");
                                continue;
                            }
                        };
                        let sj = SchedulerJob {
                            chain,
                            mint,
                            observed_at: job.max_block_time,
                            slot_hints: job.slot_hints,
                        };
                        // Update registry (acquire write lock, drop before await).
                        {
                            let mut reg = self.registry.write().await;
                            reg.on_event(chain, mint_str.clone(), job.max_block_time);
                            self.metrics.streaming_tokens_active.set(reg.len() as f64);
                        }
                        // Enqueue; drop on full (backpressure safety).
                        match self.queue_tx.try_send(sj) {
                            Ok(()) => {
                                self.metrics.streaming_queue_depth.set(
                                    self.queue_tx.len() as f64,
                                );
                            }
                            Err(async_channel::TrySendError::Full(_)) => {
                                self.metrics.streaming_queue_overflow_total.inc();
                                debug!(chain = %chain, mint = %mint_str,
                                       "queue full — dropping streaming job");
                            }
                            Err(async_channel::TrySendError::Closed(_)) => {
                                // Workers have all exited; shut down.
                                return;
                            }
                        }
                    }
                }

                // Receive invalidation event from broadcast channel.
                recv_result = self.invalidation_rx.recv() => {
                    match recv_result {
                        Ok(event) => {
                            self.handle_invalidation(event, &mut pending);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                skipped = n,
                                "streaming scheduler lagged on broadcast channel — \
                                 re-evaluation ticks for affected tokens were missed"
                            );
                            self.metrics.streaming_queue_overflow_total.inc_by(n as f64);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Indexer / AppState shut down.
                            debug!("invalidation_tx closed — scheduler exiting");
                            return;
                        }
                    }
                }
            }
        }
    }

    fn handle_invalidation(
        &self,
        event: InvalidationEvent,
        pending: &mut BTreeMap<(Chain, String), PendingJob>,
    ) {
        let key = (event.chain, event.mint.clone());

        // Derive observed_at from block_time (determinism invariant — never Utc::now()).
        // block_time is a Unix timestamp (seconds). If zero or negative, skip.
        let event_time = if event.block_time > 0 {
            match DateTime::<Utc>::from_timestamp(event.block_time, 0) {
                Some(t) => t,
                None => {
                    warn!(
                        block_time = event.block_time,
                        "invalid block_time — skipping event"
                    );
                    return;
                }
            }
        } else {
            // Defensive: block_time not set (e.g. legacy call sites that haven't
            // been updated yet).  Skip rather than use wall-clock.
            debug!(mint = %event.mint, "block_time == 0 — skipping invalidation event");
            return;
        };

        if let Some(existing) = pending.get_mut(&key) {
            // Debounce merge: accumulate slot_hints, advance max_block_time.
            existing.slot_hints.extend(event.slot_hints);
            if event_time > existing.max_block_time {
                existing.max_block_time = event_time;
            }
            self.metrics.streaming_debounce_merge_total.inc();
        } else {
            pending.insert(
                key,
                PendingJob {
                    max_block_time: event_time,
                    slot_hints: event.slot_hints,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// PendingJob — internal accumulator
// ---------------------------------------------------------------------------

struct PendingJob {
    max_block_time: DateTime<Utc>,
    slot_hints: Vec<u64>,
}
