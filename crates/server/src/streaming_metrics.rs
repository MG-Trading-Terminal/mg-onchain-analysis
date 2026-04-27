//! Prometheus metrics for the streaming detector scheduler.
//!
//! Registered separately from `GatewayMetrics` because the scheduler lives in
//! the server binary, not the gateway crate.  Owned by `crates/server` and
//! stored behind `Arc<StreamingMetrics>` so workers can share it cheaply.

use prometheus::{Counter, CounterVec, Gauge, HistogramOpts, HistogramVec, Opts, Registry};

/// All Prometheus metrics for the streaming detector scheduler.
///
/// Stored behind `Arc` — cheap to clone across workers.
#[derive(Clone)]
pub struct StreamingMetrics {
    /// Current number of tokens in `StreamingRegistry`.
    pub streaming_tokens_active: Gauge,
    /// Evictions from the registry. `reason ∈ {"idle", "cap"}`.
    pub streaming_tokens_evicted_total: CounterVec,
    /// Current job-queue depth.
    pub streaming_queue_depth: Gauge,
    /// Jobs dropped because the bounded queue was full.
    pub streaming_queue_overflow_total: Counter,
    /// Worker evaluations. `chain` × `outcome ∈ {"ok", "error", "timeout", "panic"}`.
    pub streaming_evaluations_total: CounterVec,
    /// Time per full token evaluation (all detectors + scoring).
    /// Buckets match design §5: [50ms, 100ms, 200ms, 500ms, 1s, 2s, 5s].
    pub streaming_evaluation_duration_seconds: HistogramVec,
    /// `AnomalyEvent` rows written to Postgres from the streaming path.
    pub streaming_anomaly_events_persisted_total: CounterVec,
    /// Events merged into an existing pending job (debounce hit).
    pub streaming_debounce_merge_total: Counter,
    /// Time workers spend waiting for a job (idle).
    pub streaming_worker_idle_seconds: HistogramVec,
    /// Score writes skipped because per-detector delta was below threshold.
    /// `reason ∈ {"below_delta", "first_evaluation", "manual_force"}`.
    pub streaming_score_skipped_total: CounterVec,
    /// D01 (honeypot_sim) evaluations skipped due to cadence rate-limiting.
    /// Incremented each tick that D01 is cadence-gated (not run).
    /// Complements `streaming_detector_runs_total{detector="honeypot_sim"}`.
    pub streaming_d01_skipped_total: Counter,
    /// Per-detector evaluation latency. Labels: `chain` × `detector_id`.
    /// Buckets widened vs full-eval histogram (25ms–10s) to capture D01's
    /// RPC-bound tail (Solana `simulateTransaction` can be hundreds of ms).
    /// Used for P95/P99 analysis to calibrate `streaming_d01_cadence_n`.
    pub streaming_detector_evaluation_duration_seconds: HistogramVec,
    /// Prometheus registry — not for the global default registry; isolated per binary.
    pub registry: Registry,
}

impl StreamingMetrics {
    /// Register all metrics and return the struct.
    ///
    /// Uses an isolated `Registry` (not the global default) so it does not
    /// conflict with `GatewayMetrics` which also uses an isolated registry.
    ///
    /// # Errors
    ///
    /// Returns an error if any metric registration fails (should only happen on
    /// double-registration, i.e. a bug).
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let streaming_tokens_active = Gauge::with_opts(Opts::new(
            "streaming_tokens_active",
            "Current tokens in StreamingRegistry",
        ))?;
        registry.register(Box::new(streaming_tokens_active.clone()))?;

        let streaming_tokens_evicted_total = CounterVec::new(
            Opts::new(
                "streaming_tokens_evicted_total",
                "Evictions from StreamingRegistry",
            ),
            &["reason"],
        )?;
        registry.register(Box::new(streaming_tokens_evicted_total.clone()))?;

        let streaming_queue_depth = Gauge::with_opts(Opts::new(
            "streaming_queue_depth",
            "Current SchedulerQueue depth",
        ))?;
        registry.register(Box::new(streaming_queue_depth.clone()))?;

        let streaming_queue_overflow_total = Counter::with_opts(Opts::new(
            "streaming_queue_overflow_total",
            "Jobs dropped because the bounded queue was full",
        ))?;
        registry.register(Box::new(streaming_queue_overflow_total.clone()))?;

        let streaming_evaluations_total = CounterVec::new(
            Opts::new("streaming_evaluations_total", "Worker evaluations"),
            &["chain", "outcome"],
        )?;
        registry.register(Box::new(streaming_evaluations_total.clone()))?;

        // Buckets per design §5: p99 target < 2s.
        let eval_buckets = vec![0.050, 0.100, 0.200, 0.500, 1.0, 2.0, 5.0];
        let streaming_evaluation_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "streaming_evaluation_duration_seconds",
                "Time per full token evaluation (all detectors + scoring)",
            )
            .buckets(eval_buckets),
            &["chain"],
        )?;
        registry.register(Box::new(streaming_evaluation_duration_seconds.clone()))?;

        let streaming_anomaly_events_persisted_total = CounterVec::new(
            Opts::new(
                "streaming_anomaly_events_persisted_total",
                "AnomalyEvent rows written to Postgres from the streaming path",
            ),
            &["chain", "detector_id"],
        )?;
        registry.register(Box::new(streaming_anomaly_events_persisted_total.clone()))?;

        let streaming_debounce_merge_total = Counter::with_opts(Opts::new(
            "streaming_debounce_merge_total",
            "Events merged into existing pending job (debounce hit)",
        ))?;
        registry.register(Box::new(streaming_debounce_merge_total.clone()))?;

        // Idle-wait histogram: how long workers block waiting for a job.
        let idle_buckets = vec![0.001, 0.005, 0.010, 0.050, 0.100, 0.500, 1.0];
        let streaming_worker_idle_seconds = HistogramVec::new(
            HistogramOpts::new(
                "streaming_worker_idle_seconds",
                "Time workers spend waiting for a job",
            )
            .buckets(idle_buckets),
            &[],
        )?;
        registry.register(Box::new(streaming_worker_idle_seconds.clone()))?;

        let streaming_score_skipped_total = CounterVec::new(
            Opts::new(
                "streaming_score_skipped_total",
                "Score writes skipped because per-detector delta was below threshold",
            ),
            &["reason"],
        )?;
        registry.register(Box::new(streaming_score_skipped_total.clone()))?;

        let streaming_d01_skipped_total = Counter::with_opts(Opts::new(
            "streaming_d01_skipped_total",
            "D01 honeypot_sim evaluations skipped due to cadence rate-limiting",
        ))?;
        registry.register(Box::new(streaming_d01_skipped_total.clone()))?;

        // Per-detector histogram: wider tail than full-eval histogram (D01 RPC can spike).
        let detector_buckets = vec![0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.0, 5.0, 10.0];
        let streaming_detector_evaluation_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "streaming_detector_evaluation_duration_seconds",
                "Per-detector evaluation latency (P95/P99 calibration for cadence tuning)",
            )
            .buckets(detector_buckets),
            &["chain", "detector_id"],
        )?;
        registry.register(Box::new(streaming_detector_evaluation_duration_seconds.clone()))?;

        Ok(Self {
            streaming_tokens_active,
            streaming_tokens_evicted_total,
            streaming_queue_depth,
            streaming_queue_overflow_total,
            streaming_evaluations_total,
            streaming_evaluation_duration_seconds,
            streaming_anomaly_events_persisted_total,
            streaming_debounce_merge_total,
            streaming_worker_idle_seconds,
            streaming_score_skipped_total,
            streaming_d01_skipped_total,
            streaming_detector_evaluation_duration_seconds,
            registry,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_metrics_register_without_error() {
        StreamingMetrics::new().expect("StreamingMetrics registration must succeed");
    }

    #[test]
    fn per_detector_histogram_observes_per_label_pair() {
        let m = StreamingMetrics::new().unwrap();
        let h = m
            .streaming_detector_evaluation_duration_seconds
            .with_label_values(&["solana", "honeypot_sim"]);
        h.observe(0.042);
        h.observe(0.250);
        // Separate label set is independent.
        m.streaming_detector_evaluation_duration_seconds
            .with_label_values(&["solana", "rug_pull_lp_drain"])
            .observe(0.010);
        let mf = m.registry.gather();
        let found = mf.iter().any(|f| {
            f.get_name() == "streaming_detector_evaluation_duration_seconds"
        });
        assert!(found, "per-detector histogram must register under canonical name");
    }

    #[test]
    fn streaming_metrics_double_new_uses_isolated_registry() {
        // Each call creates an independent registry — no conflict.
        let m1 = StreamingMetrics::new().unwrap();
        let m2 = StreamingMetrics::new().unwrap();
        m1.streaming_evaluations_total
            .with_label_values(&["solana", "ok"])
            .inc();
        m2.streaming_evaluations_total
            .with_label_values(&["solana", "ok"])
            .inc();
        // If they shared a registry this would panic. Reaching here = isolated.
    }
}
