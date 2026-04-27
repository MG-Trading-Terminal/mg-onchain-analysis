//! Reorg handler.
//!
//! When `Event::ReorgMarker { slot }` arrives the indexer must:
//!
//! 1. **Flush all in-flight buffers** — commit whatever is pending before the
//!    DELETE so we don't leave events from the reorg'd slot orphaned in the buffers.
//! 2. **Issue deletes** — `DELETE FROM <table> WHERE chain = $1 AND block_height >= $2`
//!    for `transfers`, `swaps`, `pool_events`, and `holder_snapshots_history`.
//!    `holder_snapshots` is NOT deleted (idempotent UPSERT guard handles it).
//!    `anomaly_events` is NOT deleted (detectors re-run on post-reorg state).
//! 3. **Rewind checkpoint** — save `slot - 1` so on restart the adapter replays
//!    from the correct position.
//!
//! # Ordering guarantee
//!
//! The DELETE must happen AFTER the pending batch is committed. If the process
//! dies between flush and delete, the next restart will re-emit the reorg'd events;
//! the `ON CONFLICT DO NOTHING` constraint absorbs the duplicates and the next
//! reorg marker will trigger another delete. This is safe because at-least-once
//! delivery + idempotent storage is the design invariant.
//!
//! # Single-task model
//!
//! The reorg handler is called inline from the same async task that drives the
//! subscribe loop. No concurrent writes happen during the handler (the loop is
//! paused). This removes any need for locking.

use tracing::{error, info, warn};

use mg_onchain_storage::{AsyncCheckpointStore, Checkpoint};

use crate::batcher::{DrainedBatch, EventBatcher};
use crate::error::IndexerError;
use crate::sink::EventSink;

// ---------------------------------------------------------------------------
// handle_reorg
// ---------------------------------------------------------------------------

/// Execute the full reorg protocol for `slot`.
///
/// Steps:
/// 1. Drain and flush all buffers via `sink`.
/// 2. Delete events at `block_height >= slot` from mutable tables.
/// 3. Rewind the checkpoint to `slot - 1`.
///
/// Returns `Ok(rewound_slot)` on success. The caller should update its
/// `last_slot` tracker to `rewound_slot`.
///
/// `adapter_id` is the checkpoint key (e.g. `"solana"`).
/// `chain` is the chain string used as the Postgres `chain` column value.
pub async fn handle_reorg<S, C>(
    reorg_slot: u64,
    chain: &str,
    adapter_id: &str,
    batcher: &mut EventBatcher,
    sink: &S,
    checkpoint_store: &C,
) -> Result<u64, IndexerError>
where
    S: EventSink,
    C: AsyncCheckpointStore,
{
    warn!(
        reorg_slot,
        chain, "ReorgMarker received — flushing buffers and issuing DELETEs"
    );

    // Step 1: flush all in-flight buffers.
    let pending = batcher.pending_count();
    let batch = batcher.drain_all();
    if !batch.is_empty() {
        info!(
            pending,
            reorg_slot, "flushing in-flight events before reorg delete"
        );
        flush_drained_batch(batch, sink).await?;
    }

    // Step 2: delete events at block_height >= reorg_slot.
    info!(
        reorg_slot,
        chain, "issuing reorg DELETEs for slot and above"
    );
    sink.delete_from_slot(chain, reorg_slot).await?;

    // Step 3: rewind checkpoint to slot - 1.
    // If slot == 0 (extremely unusual), rewind to 0 rather than underflow.
    let rewound_slot = reorg_slot.saturating_sub(1);
    let rewound = Checkpoint {
        slot: rewound_slot,
        last_signature: None, // start of rewound slot
    };
    checkpoint_store
        .save(adapter_id, &rewound)
        .await
        .map_err(|e| IndexerError::Checkpoint(e.to_string()))?;

    info!(
        reorg_slot,
        rewound_slot, chain, "reorg handling complete — checkpoint rewound"
    );
    Ok(rewound_slot)
}

// ---------------------------------------------------------------------------
// flush_drained_batch — shared by reorg and shutdown
// ---------------------------------------------------------------------------

/// Write a `DrainedBatch` to the sink.
///
/// Skips empty slices. Each table is written independently — a failure in one
/// table does NOT prevent the others from being written. If partial failure
/// matters, the caller can inspect the error and retry.
pub async fn flush_drained_batch<S: EventSink>(
    batch: DrainedBatch,
    sink: &S,
) -> Result<(), IndexerError> {
    let mut last_err: Option<IndexerError> = None;

    if !batch.transfers.is_empty()
        && let Err(e) = sink.insert_transfers(&batch.transfers).await
    {
        error!(count = batch.transfers.len(), err = %e, "failed to insert transfers");
        last_err = Some(e);
    }
    if !batch.swaps.is_empty()
        && let Err(e) = sink.insert_swaps(&batch.swaps).await
    {
        error!(count = batch.swaps.len(), err = %e, "failed to insert swaps");
        last_err = Some(e);
    }
    if !batch.pool_events.is_empty()
        && let Err(e) = sink.insert_pool_events(&batch.pool_events).await
    {
        error!(count = batch.pool_events.len(), err = %e, "failed to insert pool_events");
        last_err = Some(e);
    }
    if !batch.holder_snapshots.is_empty()
        && let Err(e) = sink.upsert_holder_snapshots(&batch.holder_snapshots).await
    {
        error!(
            count = batch.holder_snapshots.len(),
            err = %e,
            "failed to upsert holder_snapshots"
        );
        last_err = Some(e);
    }

    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
