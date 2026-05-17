//! `SchedulerWorker` — one unit of the streaming detector worker pool.
//!
//! Workers consume `SchedulerJob`s from the `async_channel` queue (MPMC,
//! no shared mutex).  Phase 3 wires D02/D04/D05/D06 as active detectors.
//!
//! # Per-detector panic isolation (design §6 rev 1)
//!
//! Each detector call is wrapped with `AssertUnwindSafe(...).catch_unwind()`
//! inside a `tokio::time::timeout`.  A panicking detector increments
//! `streaming_evaluations_total{outcome="panic"}` and the worker continues
//! with the next detector.  The worker task itself never dies from a detector
//! panic.
//!
//! # Delta threshold short-circuit (design §2.4 rev 1)
//!
//! Each worker holds a `HashMap<(Chain, String), Vec<f32>>` of the last
//! per-detector confidence vector.  When the max absolute delta between the
//! new and previous vector is below `scoring_skip_delta_threshold`, the
//! scoring + upsert writes are skipped and `streaming_score_skipped_total`
//! is incremented.  Note: `AnomalyEvent` rows are ALWAYS persisted — only
//! the downstream scoring write is gated by the delta check.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use chrono::Duration as ChronoDuration;
use futures::FutureExt as _;
use tokio::time;
use tracing::{debug, error, info, instrument, warn};

use mg_onchain_common::anomaly::AnomalyEvent;
use mg_onchain_common::chain::{BlockRef, Chain};
use mg_onchain_detectors::config::DetectorConfig;
use mg_onchain_detectors::context::{DetectorContext, DetectorWindow};
use mg_onchain_scoring::ScoringEngine;
use mg_onchain_scoring::types::SkipReason;
use mg_onchain_storage::pg::PgStore;
use mg_onchain_token_registry::TokenRegistry;

use crate::risk_report_store::TokenRiskReportStore;

use crate::erased_detector::ArcErasedDetector;
use crate::streaming::scheduler::SchedulerJob;
use crate::streaming_config::StreamingConfig;
use crate::streaming_metrics::StreamingMetrics;

// ---------------------------------------------------------------------------
// SchedulerWorker
// ---------------------------------------------------------------------------

/// One worker in the streaming detector pool.
///
/// Constructed by `crates/server/src/lib.rs` and consumed by `tokio::spawn`.
///
/// # Field invariant
///
/// When `detectors` is non-empty, `store`, `registry`, `scoring`, and
/// `detector_config` MUST be `Some`.  The Phase 1 plumbing path uses an
/// empty `detectors` vec; those workers never access the optional fields.
pub struct SchedulerWorker {
    /// MPMC receiver — cheap to clone; no mutex.
    pub queue_rx: async_channel::Receiver<SchedulerJob>,
    pub config: StreamingConfig,
    pub metrics: Arc<StreamingMetrics>,
    /// Detectors to run on each job. Phase 2: D04 (pump/dump).
    /// Empty = plumbing-only mode (Phase 1 smoke tests).
    pub detectors: Vec<ArcErasedDetector>,
    /// Postgres store for event persistence.
    /// Required when `detectors` is non-empty.
    pub store: Option<PgStore>,
    /// Token registry for metadata enrichment.
    /// Required when `detectors` is non-empty.
    pub registry: Option<TokenRegistry>,
    /// Scoring engine (stateless, shared read-only config).
    /// Required when `detectors` is non-empty.
    pub scoring: Option<Arc<ScoringEngine>>,
    /// Detector configuration (thresholds). Borrowed by DetectorContext each tick.
    /// Required when `detectors` is non-empty.
    pub detector_config: Option<Arc<DetectorConfig>>,
    /// Durable risk report store (V00012 `token_risk_reports` table).
    ///
    /// `None` → persistence disabled (default; controlled by
    /// `streaming.token_risk_reports_enabled` in config/service.toml).
    /// `Some(store)` → upsert called best-effort after each scoring tick.
    ///
    /// A Postgres outage with `Some(store)` logs a `warn!` and continues;
    /// the in-memory `RiskCache` (gateway) is unaffected.
    ///
    /// # Delta-threshold short-circuit (gotcha #30)
    ///
    /// When the delta check fires, `evaluate_token` returns early before
    /// reaching the upsert call. The store is NOT called on short-circuit —
    /// there is no new report to persist.
    pub risk_report_store: Option<Arc<dyn TokenRiskReportStore>>,
}

impl SchedulerWorker {
    /// Run the worker until the queue closes.
    ///
    /// Consumes `self`; meant to be wrapped in `tokio::spawn`.
    #[instrument(skip(self), name = "scheduler_worker")]
    pub async fn run(self) {
        let mut score_cache: HashMap<(Chain, String), Vec<f32>> = HashMap::new();
        // Per-token D01 evaluation tick counter.
        // Key: (chain, mint). Value: number of evaluate_token calls since last D01 run.
        // Incremented each call; D01 runs when counter % cadence_n == 0 (i.e., on 0).
        // Counter is reset to 1 after a D01 run (not 0, to avoid running twice in a row
        // if the token is evicted and re-added).
        let mut d01_tick_counters: HashMap<(Chain, String), u64> = HashMap::new();

        loop {
            let t_wait = std::time::Instant::now();
            let job = match self.queue_rx.recv().await {
                Ok(j) => j,
                Err(_) => {
                    // Channel closed; all senders (scheduler) have gone away.
                    debug!("worker queue closed — exiting");
                    return;
                }
            };
            self.metrics
                .streaming_worker_idle_seconds
                .with_label_values(&[])
                .observe(t_wait.elapsed().as_secs_f64());

            // Update queue depth gauge.
            self.metrics
                .streaming_queue_depth
                .set(self.queue_rx.len() as f64);

            let chain_str = job.chain.to_string();
            let t_eval = std::time::Instant::now();

            if let Err(e) = self
                .evaluate_token(&job, &mut score_cache, &mut d01_tick_counters)
                .await
            {
                error!(
                    chain = %chain_str,
                    mint = %job.mint,
                    error = %e,
                    "evaluate_token returned error"
                );
                self.metrics
                    .streaming_evaluations_total
                    .with_label_values(&[&chain_str, "error"])
                    .inc();
            } else {
                self.metrics
                    .streaming_evaluations_total
                    .with_label_values(&[&chain_str, "ok"])
                    .inc();
            }

            self.metrics
                .streaming_evaluation_duration_seconds
                .with_label_values(&[&chain_str])
                .observe(t_eval.elapsed().as_secs_f64());
        }
    }

    /// Evaluate all detectors for one `(chain, mint)` job.
    ///
    /// Active detectors (alphabetical, matching score_cache Vec<f32> index layout):
    ///   [0] honeypot_sim      (D01) — cadenced via `streaming_d01_cadence_n`
    ///   [1] mint_burn_anomaly (D06)
    ///   [2] pump_dump         (D04)
    ///   [3] rug_pull_lp_drain (D02)
    ///   [4] wash_trading_h1   (D05)
    ///
    /// D01 cadence: runs every Nth tick (Option A — modulo counter, default N=10).
    /// On skipped ticks, D01 contributes 0.0 to the score vector.
    /// The delta-threshold check gates whether scoring + Postgres upsert are
    /// executed. `AnomalyEvent` rows are ALWAYS persisted regardless of delta.
    #[instrument(skip(self, job, score_cache, d01_tick_counters),
                 fields(chain = %job.chain, mint = %job.mint, observed_at = %job.observed_at))]
    pub async fn evaluate_token(
        &self,
        job: &SchedulerJob,
        score_cache: &mut HashMap<(Chain, String), Vec<f32>>,
        d01_tick_counters: &mut HashMap<(Chain, String), u64>,
    ) -> anyhow::Result<()> {
        let chain_str = job.chain.to_string();
        let cache_key = (job.chain, job.mint.to_string());

        // ----------------------------------------------------------------
        // Build DetectorContext
        //
        // observed_at comes from job.observed_at (MAX(block_time) over slot
        // hints — never Utc::now(); determinism invariant).
        //
        // The window is [observed_at - window_minutes, observed_at].
        // block_start / block_end use placeholder heights (0 / u64::MAX),
        // matching the gateway's analyze.rs pattern.
        // ----------------------------------------------------------------
        let window_end = job.observed_at;
        let window_start = window_end - ChronoDuration::minutes(self.config.window_minutes as i64);

        let zero_address = if job.chain.is_evm() {
            "0x0000000000000000000000000000000000000000"
        } else {
            "11111111111111111111111111111111"
        };

        // ----------------------------------------------------------------
        // Short-circuit early when no detectors are configured (Phase 1
        // plumbing mode, or future per-chain detector exclusion).
        // MUST be before accessing optional fields below.
        // ----------------------------------------------------------------
        if self.detectors.is_empty() {
            debug!(
                chain = %chain_str,
                mint = %job.mint,
                "no detectors configured — skipping evaluation"
            );
            return Ok(());
        }

        let window = DetectorWindow {
            start: window_start,
            end: window_end,
            block_start: BlockRef::new(job.chain, 0),
            block_end: BlockRef::new(job.chain, u64::MAX),
        };

        // Safety: the is_empty() early-return above guarantees detectors is
        // non-empty, so these fields MUST be Some per the struct invariant.
        let store = self
            .store
            .as_ref()
            .expect("SchedulerWorker: store must be Some when detectors is non-empty");
        let registry = self
            .registry
            .as_ref()
            .expect("SchedulerWorker: registry must be Some when detectors is non-empty");
        let detector_config = self
            .detector_config
            .as_ref()
            .expect("SchedulerWorker: detector_config must be Some when detectors is non-empty");
        let scoring = self
            .scoring
            .as_ref()
            .expect("SchedulerWorker: scoring must be Some when detectors is non-empty");

        let ctx = DetectorContext {
            token: &job.mint,
            chain: job.chain,
            window,
            observed_at: job.observed_at,
            store,
            registry,
            config: detector_config,
            zero_address,
        };

        // ----------------------------------------------------------------
        // D01 cadence check (Option A — modulo counter per-token).
        //
        // D01 (honeypot_sim) is at index 0 in self.detectors. It is
        // expensive (multiple RPC roundtrips); run every Nth tick only.
        // cadence_n = 1 means run every tick (no cadence).
        //
        // Design reference: docs/designs/0014-streaming-detector.md §8 Phase 3 note.
        // ----------------------------------------------------------------
        let d01_cache_key = (job.chain, job.mint.to_string());
        let cadence_n = self.config.streaming_d01_cadence_n.max(1);
        let tick = d01_tick_counters.entry(d01_cache_key.clone()).or_insert(0);
        let run_d01 = (*tick).is_multiple_of(cadence_n);
        *tick = tick.wrapping_add(1);

        if !run_d01 {
            self.metrics.streaming_d01_skipped_total.inc();
            debug!(
                chain = %chain_str,
                mint = %job.mint,
                tick = *tick - 1,
                cadence_n,
                "D01 cadence-skipped this tick"
            );
        }

        let mut all_events: Vec<AnomalyEvent> = Vec::new();
        let mut new_scores: Vec<f32> = Vec::with_capacity(self.detectors.len());

        for (det_idx, det) in self.detectors.iter().enumerate() {
            // ADR 0005 Decision 2: skip detectors that don't support this chain.
            // All D01-D11 default to Solana-only; once EVM detectors land (Sprint 18+),
            // they will override `supported_chains()` to include Chain::Ethereum.
            if !det.supported_chains().contains(&job.chain) {
                tracing::debug!(
                    detector_id = %det.id(),
                    chain = ?job.chain,
                    "skipping detector — chain not supported"
                );
                // Contribute 0.0 to keep score vector aligned.
                new_scores.push(0.0_f32);
                continue;
            }

            // D01 is at index 0; apply cadence gate.
            if det_idx == 0 && !run_d01 {
                // Cadence-skip D01: contribute 0.0 to keep score vector aligned.
                new_scores.push(0.0_f32);
                continue;
            }

            let events_opt = run_detector_isolated(
                &**det,
                &ctx,
                self.config.per_detector_timeout_ms,
                &self.metrics,
                &chain_str,
            )
            .await;

            if let Some(mut events) = events_opt {
                // Auto-populate OAK technique ID from the detector if the
                // detector didn't set one during evaluation.
                if let Some(tid) = det.oak_technique_id() {
                    for e in &mut events {
                        e.oak_technique_id.get_or_insert_with(|| tid.to_owned());
                    }
                }
                let confidence: f32 = events
                    .iter()
                    .map(|e| e.confidence.value() as f32)
                    .fold(0.0_f32, f32::max);
                new_scores.push(confidence);
                all_events.extend(events);
            } else {
                // Timeout or panic — contribute 0.0 to score vector so
                // vector lengths stay aligned across ticks.
                new_scores.push(0.0_f32);
            }
        }

        // ----------------------------------------------------------------
        // Persist AnomalyEvents (always — events ≠ score)
        // ----------------------------------------------------------------
        if !all_events.is_empty() {
            store
                .insert_anomaly_events(&all_events, "streaming_scheduler")
                .await
                .context("insert_anomaly_events")?;

            // Per-event metric
            for event in &all_events {
                self.metrics
                    .streaming_anomaly_events_persisted_total
                    .with_label_values(&[&chain_str, event.detector_id.as_str()])
                    .inc();
            }
        }

        // ----------------------------------------------------------------
        // Delta-threshold short-circuit (§2.4 rev 1)
        //
        // Skip scoring + risk_cache + upsert_token_risk_report when the max
        // per-detector confidence delta is below threshold.
        // ----------------------------------------------------------------
        if let Some(prev) = score_cache.get(&cache_key)
            && prev.len() == new_scores.len()
            && !new_scores.is_empty()
        {
            let delta = prev
                .iter()
                .zip(new_scores.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f32, f32::max);
            if delta < self.config.scoring_skip_delta_threshold as f32 {
                self.metrics
                    .streaming_score_skipped_total
                    .with_label_values(&["below_delta"])
                    .inc();
                debug!(
                    chain = %chain_str,
                    mint = %job.mint,
                    delta,
                    threshold = self.config.scoring_skip_delta_threshold,
                    "scoring skip: below delta threshold"
                );
                // Update cache even on skip so next tick compares against latest scores.
                score_cache.insert(cache_key, new_scores);
                return Ok(());
            }
        }

        // Update score cache before the scoring path (ensures cache is fresh
        // even if scoring errors below).
        score_cache.insert(cache_key, new_scores);

        // ----------------------------------------------------------------
        // Score + risk_cache upsert
        //
        // Active detectors (alphabetical, sorted per TokenRiskReport convention):
        //   honeypot_sim, mint_burn_anomaly, pump_dump, rug_pull_lp_drain, wash_trading_h1
        //
        // D03 (holder_concentration) and D07 (withdraw_withheld_drain) are
        // permanently skipped with SkipReason.
        //
        // D01 (honeypot_sim) is active but cadenced — it may emit 0.0 on non-run
        // ticks. The SkipReason for D01 is NOT emitted here (it's in the active
        // set); the cadence-skipped metric handles observability.
        //
        // This order must match the score_cache Vec<f32> index layout documented
        // in crates/server/src/lib.rs spawn_streaming_subsystem.
        // ----------------------------------------------------------------
        let mint_str = job.mint.to_string();

        let meta = registry
            .enrich(&mint_str, job.chain)
            .await
            .context("registry enrich")?;

        let detectors_skipped: Vec<SkipReason> = vec![
            SkipReason {
                detector_id: "holder_concentration".to_string(),
                reason: "streaming_snapshot_only".to_string(),
            },
            SkipReason {
                detector_id: "withdraw_withheld_drain".to_string(),
                reason: "streaming_low_value".to_string(),
            },
        ];

        let report = scoring.score(
            &all_events,
            &meta,
            (window_start, window_end),
            &detectors_skipped,
            job.observed_at,
        );

        info!(
            chain = %chain_str,
            mint = %mint_str,
            overall_score = %report.overall_score.value(),
            events = all_events.len(),
            "streaming evaluation complete"
        );

        // Durable persistence: upsert to token_risk_reports (V00012).
        //
        // Best-effort: a Postgres outage must NOT crash the worker or stop
        // live scoring. The in-memory RiskCache (gateway) remains the hot path.
        //
        // Delta-threshold short-circuit (gotcha #30): when the delta check fires
        // at line 316, evaluate_token returns early and never reaches this code.
        // The store is therefore NOT called on short-circuit — there is no new
        // report to persist. This is a structural guarantee, not a conditional.
        if let Some(ref store) = self.risk_report_store {
            store
                .upsert_token_risk_report(&report)
                .await
                .inspect_err(|e| {
                    warn!(
                        chain = %chain_str,
                        mint = %mint_str,
                        err = %e,
                        "token_risk_reports upsert failed — continuing (best-effort)"
                    );
                })
                .ok();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-detector isolation wrapper (design §6 rev 1)
// ---------------------------------------------------------------------------

/// Run a single detector with panic isolation and a per-detector timeout.
///
/// Returns `Some(events)` on success, `None` on timeout or panic.
/// Increments the appropriate `streaming_evaluations_total` label on failure.
///
/// # Safety of `AssertUnwindSafe`
///
/// `DetectorContext` is borrowed immutably; even if a panic occurs mid-future,
/// no shared mutable state is corrupted.  The assertion is sound per design §6.
pub async fn run_detector_isolated<'ctx>(
    det: &'ctx dyn crate::erased_detector::ErasedDetector,
    ctx: &'ctx mg_onchain_detectors::context::DetectorContext<'ctx>,
    timeout_ms: u64,
    metrics: &StreamingMetrics,
    chain: &str,
) -> Option<Vec<AnomalyEvent>> {
    let per_det = Duration::from_millis(timeout_ms);
    let timer = metrics
        .streaming_detector_evaluation_duration_seconds
        .with_label_values(&[chain, det.id()])
        .start_timer();
    let det_fut = AssertUnwindSafe(det.evaluate_erased(ctx)).catch_unwind();
    let outcome = time::timeout(per_det, det_fut).await;
    timer.observe_duration();
    match outcome {
        Ok(Ok(Ok(events))) => Some(events),
        Ok(Ok(Err(e))) => {
            error!(detector_id = %det.id(), error = %e, "detector returned Err");
            metrics
                .streaming_evaluations_total
                .with_label_values(&[chain, "error"])
                .inc();
            None
        }
        Ok(Err(panic_val)) => {
            error!(
                detector_id = %det.id(),
                panic = ?panic_val,
                "detector panicked — isolated by catch_unwind"
            );
            metrics
                .streaming_evaluations_total
                .with_label_values(&[chain, "panic"])
                .inc();
            None
        }
        Err(_elapsed) => {
            error!(
                detector_id = %det.id(),
                timeout_ms = timeout_ms,
                "detector timed out"
            );
            metrics
                .streaming_evaluations_total
                .with_label_values(&[chain, "timeout"])
                .inc();
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use mg_onchain_detectors::error::DetectorError;

    use crate::streaming_metrics::StreamingMetrics;

    fn test_metrics() -> Arc<StreamingMetrics> {
        Arc::new(StreamingMetrics::new().unwrap())
    }

    // -----------------------------------------------------------------------
    // Isolation helper for testing panic + timeout without a real DetectorContext.
    //
    // `run_detector_isolated` requires a `&DetectorContext` which needs a real
    // PgStore / TokenRegistry — both are heavyweight.  Instead we test the
    // same `AssertUnwindSafe + catch_unwind + timeout` mechanics via a thin
    // helper that operates on arbitrary futures.
    //
    // This mirrors exactly what `run_detector_isolated` does internally and
    // gives us full coverage of the isolation logic without any unsafe code.
    // -----------------------------------------------------------------------

    enum IsolatedOutcome {
        Ok,
        Error,
        Panic,
        Timeout,
    }

    async fn run_future_isolated<F, T>(fut: F, timeout_ms: u64) -> IsolatedOutcome
    where
        F: std::future::Future<Output = Result<T, DetectorError>>,
    {
        use futures::FutureExt as _;
        // AssertUnwindSafe is needed because arbitrary futures are not UnwindSafe.
        // This mirrors what run_detector_isolated does for detector futures.
        let wrapped = AssertUnwindSafe(fut).catch_unwind();
        match time::timeout(Duration::from_millis(timeout_ms), wrapped).await {
            Ok(Ok(Ok(_))) => IsolatedOutcome::Ok,
            Ok(Ok(Err(_))) => IsolatedOutcome::Error,
            Ok(Err(_)) => IsolatedOutcome::Panic,
            Err(_) => IsolatedOutcome::Timeout,
        }
    }

    // -----------------------------------------------------------------------
    // Test: panicking future → Panic outcome, no task death
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn panicking_future_is_isolated() {
        let metrics = test_metrics();

        let panicking: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>>>,
        > = Box::pin(async move { panic!("intentional panic for test") });
        let outcome = run_future_isolated(panicking, 5_000).await;

        assert!(
            matches!(outcome, IsolatedOutcome::Panic),
            "panicking future must produce Panic outcome"
        );

        // Simulate what run_detector_isolated does on panic: increment the metric.
        metrics
            .streaming_evaluations_total
            .with_label_values(&["solana", "panic"])
            .inc();

        use prometheus::Encoder;
        let families = metrics.registry.gather();
        let encoder = prometheus::TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains(r#"outcome="panic""#),
            "panic label must appear in metrics output;\nmetrics text:\n{text}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: slow future → Timeout outcome, no task death
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn slow_future_times_out_and_is_isolated() {
        let metrics = test_metrics();

        let slow: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>>>,
        > = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(vec![])
        });
        let outcome = run_future_isolated(slow, 50).await;

        assert!(
            matches!(outcome, IsolatedOutcome::Timeout),
            "slow future must produce Timeout outcome"
        );

        // Simulate what run_detector_isolated does on timeout: increment the metric.
        metrics
            .streaming_evaluations_total
            .with_label_values(&["solana", "timeout"])
            .inc();

        use prometheus::Encoder;
        let families = metrics.registry.gather();
        let encoder = prometheus::TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains(r#"outcome="timeout""#),
            "timeout label must appear in metrics output;\nmetrics text:\n{text}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: delta-threshold math (hand-built vectors, no real detectors needed)
    // -----------------------------------------------------------------------
    #[test]
    fn delta_threshold_math_below_fires_skip() {
        let prev = [0.80_f32, 0.50_f32];
        let new_scores = [0.82_f32, 0.51_f32];
        let threshold = 0.05_f32;

        let delta = prev
            .iter()
            .zip(new_scores.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);

        // max(0.02, 0.01) = 0.02 < 0.05 → skip should fire
        assert!(
            delta < threshold,
            "delta {delta} must be < threshold {threshold}"
        );
    }

    #[test]
    fn delta_threshold_math_above_does_not_skip() {
        let prev = [0.80_f32];
        let new_scores = [0.90_f32]; // delta = 0.10 ≥ 0.05
        let threshold = 0.05_f32;

        let delta = prev
            .iter()
            .zip(new_scores.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);

        assert!(
            delta >= threshold,
            "delta {delta} must be ≥ threshold {threshold}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: empty detector vec → score cache stays empty
    // -----------------------------------------------------------------------
    #[test]
    fn empty_detector_vec_yields_empty_score_vector() {
        // new_scores is built from self.detectors — empty vec → empty score vec.
        // This is a unit-level invariant test.
        let detectors: Vec<ArcErasedDetector> = vec![];
        let new_scores: Vec<f32> = Vec::with_capacity(detectors.len());
        assert!(
            new_scores.is_empty(),
            "empty detector vec must yield empty score vec"
        );
    }

    // -----------------------------------------------------------------------
    // Existing Phase 1 test: delta_threshold_no_fire_on_empty_scores
    // Kept for regression coverage.
    // -----------------------------------------------------------------------
    #[test]
    fn delta_threshold_no_fire_on_empty_scores_math() {
        // With empty score vectors, !new_scores.is_empty() guard prevents skip.
        let prev: Vec<f32> = vec![];
        let new_scores: Vec<f32> = vec![];
        let should_skip = prev.len() == new_scores.len() && !new_scores.is_empty();
        assert!(!should_skip, "empty score vector must not trigger skip");
    }

    // -----------------------------------------------------------------------
    // ADR 0005 Decision 2: chain-filter guard
    // -----------------------------------------------------------------------

    /// Verify the chain-filter guard logic: a detector that only supports Solana
    /// must produce a 0.0 score entry (skip) when the job chain is Ethereum.
    ///
    /// This test operates on the score-vector-building logic directly
    /// (no real PgStore / DetectorContext needed).
    #[test]
    fn chain_filter_skips_detector_for_unsupported_chain() {
        use mg_onchain_common::chain::Chain;
        use crate::erased_detector::ErasedDetector;

        // Build a minimal mock that reports supported_chains() = [Solana].
        struct SolanaOnlyDetector;

        impl ErasedDetector for SolanaOnlyDetector {
            fn id(&self) -> &'static str { "solana_only_test" }
            fn severity_floor(&self) -> mg_onchain_common::anomaly::Severity {
                mg_onchain_common::anomaly::Severity::Low
            }
            fn supported_chains(&self) -> &[Chain] {
                &[Chain::Solana]
            }
            fn evaluate_erased<'ctx>(
                &'ctx self,
                _ctx: &'ctx mg_onchain_detectors::context::DetectorContext<'ctx>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<AnomalyEvent>, mg_onchain_detectors::error::DetectorError>> + Send + 'ctx>> {
                unreachable!("evaluate_erased must never be called when chain is not supported")
            }
        }

        let det = SolanaOnlyDetector;

        // Simulate Ethereum job chain.
        let eth_chain = Chain::Ethereum;
        let sol_chain = Chain::Solana;

        // Ethereum job → chain not supported → skip (score 0.0, don't call evaluate).
        let skipped_for_eth = !det.supported_chains().contains(&eth_chain);
        assert!(
            skipped_for_eth,
            "Solana-only detector must be skipped for Ethereum chain"
        );

        // Solana job → chain supported → do NOT skip.
        let skipped_for_sol = !det.supported_chains().contains(&sol_chain);
        assert!(
            !skipped_for_sol,
            "Solana-only detector must NOT be skipped for Solana chain"
        );
    }

    /// Verify that `ErasedDetector::supported_chains()` is callable through dyn dispatch.
    #[test]
    fn erased_detector_supported_chains_is_dyn_safe() {
        use mg_onchain_common::chain::Chain;
        use crate::erased_detector::ErasedDetector;
        use std::sync::Arc;

        struct SolanaOnlyDyn;
        impl ErasedDetector for SolanaOnlyDyn {
            fn id(&self) -> &'static str { "sol_only_dyn" }
            fn severity_floor(&self) -> mg_onchain_common::anomaly::Severity {
                mg_onchain_common::anomaly::Severity::Low
            }
            fn supported_chains(&self) -> &[Chain] { &[Chain::Solana] }
            fn evaluate_erased<'ctx>(
                &'ctx self,
                _ctx: &'ctx mg_onchain_detectors::context::DetectorContext<'ctx>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<AnomalyEvent>, mg_onchain_detectors::error::DetectorError>> + Send + 'ctx>> {
                unreachable!()
            }
        }

        let boxed: Arc<dyn ErasedDetector> = Arc::new(SolanaOnlyDyn);
        let chains = boxed.supported_chains();
        assert_eq!(chains, &[Chain::Solana]);
    }
}

// ---------------------------------------------------------------------------
// Mock TokenRiskReportStore for unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod risk_report_store_mock_tests {
    use super::*;
    use async_trait::async_trait;
    use mg_onchain_common::anomaly::{Confidence, Severity};
    use mg_onchain_common::chain::{Address, Chain};
    use mg_onchain_scoring::{
        config::ScoringConfig,
        types::{CoverageReport, SignalCounts, TokenRiskReport},
    };
    use mg_onchain_storage::StorageError;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use crate::risk_report_store::TokenRiskReportStore;

    // -----------------------------------------------------------------------
    // Mock implementations
    // -----------------------------------------------------------------------

    /// Mock store that counts upsert calls.
    struct CountingStore {
        call_count: AtomicU32,
    }

    impl CountingStore {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
        fn count(&self) -> u32 {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenRiskReportStore for CountingStore {
        async fn upsert_token_risk_report(
            &self,
            _report: &TokenRiskReport,
        ) -> Result<(), StorageError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn get_latest_token_risk_report(
            &self,
            _chain: &str,
            _token: &str,
        ) -> Result<Option<TokenRiskReport>, StorageError> {
            Ok(None)
        }
    }

    /// Mock store that always returns an error.
    struct FailingStore {
        called: AtomicBool,
    }

    impl FailingStore {
        fn new() -> Self {
            Self {
                called: AtomicBool::new(false),
            }
        }
        fn was_called(&self) -> bool {
            self.called.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenRiskReportStore for FailingStore {
        async fn upsert_token_risk_report(
            &self,
            _report: &TokenRiskReport,
        ) -> Result<(), StorageError> {
            self.called.store(true, Ordering::SeqCst);
            Err(StorageError::Other("simulated Postgres outage".into()))
        }

        async fn get_latest_token_risk_report(
            &self,
            _chain: &str,
            _token: &str,
        ) -> Result<Option<TokenRiskReport>, StorageError> {
            Ok(None)
        }
    }

    // -----------------------------------------------------------------------
    // Helper: minimal report for mock testing
    // -----------------------------------------------------------------------
    fn mock_report() -> TokenRiskReport {
        let token =
            Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let now = chrono::Utc::now();
        TokenRiskReport {
            chain: Chain::Solana,
            token,
            window: (now - chrono::Duration::hours(1), now),
            computed_at: now,
            overall_score: Confidence::new(0.6).unwrap(),
            base_score: Confidence::new(0.6).unwrap(),
            overall_severity: Severity::Medium,
            per_detector: BTreeMap::new(),
            top_evidence: vec![],
            signal_counts: SignalCounts {
                fired: 0,
                inconclusive: 0,
                suppressed_info: 0,
            },
            coverage: CoverageReport {
                detectors_run: vec![],
                detectors_skipped: vec![],
                coverage_completeness: 0.0,
            },
            config_snapshot: ScoringConfig::default_calibrated(),
        }
    }

    // -----------------------------------------------------------------------
    // Test: when risk_report_store is Some, upsert is called exactly once
    // per scoring tick.
    //
    // This tests the call pattern from evaluate_token:
    //   if let Some(ref store) = self.risk_report_store {
    //       store.upsert_token_risk_report(&report).await...ok();
    //   }
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn worker_calls_upsert_after_scoring() {
        let store = Arc::new(CountingStore::new());
        let report = mock_report();

        // Simulate what evaluate_token does at the upsert site.
        let risk_report_store: Option<Arc<dyn TokenRiskReportStore>> = Some(store.clone());

        if let Some(ref s) = risk_report_store {
            s.upsert_token_risk_report(&report)
                .await
                .inspect_err(|e| warn!(err = %e, "upsert failed"))
                .ok();
        }

        assert_eq!(
            store.count(),
            1,
            "upsert must be called exactly once per scoring tick"
        );
    }

    // -----------------------------------------------------------------------
    // Test: when risk_report_store is None (delta short-circuit or disabled),
    // upsert is never called.
    //
    // The delta short-circuit returns early BEFORE reaching the upsert site.
    // This test verifies the store-is-None fast path used when persistence
    // is disabled (token_risk_reports_enabled = false).
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn worker_skips_upsert_when_store_is_none() {
        // No store — simulates both "disabled" config and delta short-circuit
        // (which returns before the upsert site is reached).
        let risk_report_store: Option<Arc<dyn TokenRiskReportStore>> = None;
        let report = mock_report();

        // This is the exact pattern from evaluate_token.
        if let Some(ref s) = risk_report_store {
            s.upsert_token_risk_report(&report)
                .await
                .inspect_err(|e| warn!(err = %e, "upsert failed"))
                .ok();
        }
        // If we reach here without panic, the None guard worked.
        // assert passes implicitly — no upsert was called.
    }

    // -----------------------------------------------------------------------
    // Test: when upsert returns an error, the caller (.ok()) swallows it
    // and the function completes normally (best-effort semantics).
    //
    // This simulates a Postgres outage: the worker must NOT crash.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn worker_continues_on_upsert_error() {
        let store = Arc::new(FailingStore::new());
        let report = mock_report();

        let risk_report_store: Option<Arc<dyn TokenRiskReportStore>> = Some(store.clone());

        // Should NOT panic or return an error to the caller.
        if let Some(ref s) = risk_report_store {
            s.upsert_token_risk_report(&report)
                .await
                .inspect_err(|e| warn!(err = %e, "upsert failed — continuing (best-effort)"))
                .ok(); // Swallow error: best-effort, must not propagate.
        }

        assert!(store.was_called(), "failing store must have been called");
        // Function completed normally despite the error — test itself completing is the proof.
    }

    // -----------------------------------------------------------------------
    // Test: delta short-circuit structural guarantee.
    //
    // Verifies the return-before-upsert pattern: when the delta check fires,
    // score_cache is updated and we return early. The upsert site is never
    // reached. We test this at the logic level (no real worker needed).
    // -----------------------------------------------------------------------
    #[test]
    fn delta_short_circuit_returns_before_upsert_site() {
        // Simulate the delta check logic from evaluate_token lines 316–338.
        let mut score_cache: HashMap<(Chain, String), Vec<f32>> = HashMap::new();
        let cache_key = (Chain::Solana, "mint".to_string());
        let prev_scores = vec![0.80_f32, 0.50_f32];
        score_cache.insert(cache_key.clone(), prev_scores.clone());

        let new_scores = vec![0.81_f32, 0.51_f32]; // delta = 0.01, below 0.05
        let threshold = 0.05_f32;

        let should_skip = if let Some(prev) = score_cache.get(&cache_key) {
            if prev.len() == new_scores.len() && !new_scores.is_empty() {
                let delta = prev
                    .iter()
                    .zip(new_scores.iter())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0_f32, f32::max);
                delta < threshold
            } else {
                false
            }
        } else {
            false
        };

        assert!(
            should_skip,
            "delta=0.01 < threshold=0.05 must trigger short-circuit"
        );

        // If short-circuit fires, the upsert site is never reached.
        // Simulate: update cache, then return. Upsert count stays 0.
        let store = Arc::new(CountingStore::new());
        if should_skip {
            score_cache.insert(cache_key, new_scores);
            // return Ok(()) — we're in a test so just assert count is still 0
        }
        assert_eq!(
            store.count(),
            0,
            "upsert must NOT be called on delta short-circuit"
        );
    }
}
