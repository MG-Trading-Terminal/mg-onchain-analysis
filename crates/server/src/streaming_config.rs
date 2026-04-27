//! `StreamingConfig` — configuration for the streaming detector scheduler.
//!
//! Loaded from `config/service.toml` under the `[streaming]` section.
//! All thresholds have documented defaults; no magic numbers in code.

use serde::{Deserialize, Serialize};

/// Full configuration for the streaming detector scheduler.
///
/// Loaded from `config/service.toml [streaming]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingConfig {
    /// Whether to spawn the streaming scheduler at all.
    /// `false` → server boots normally; scheduler task is NOT spawned.
    #[serde(default = "StreamingConfig::default_enabled")]
    pub enabled: bool,

    /// Debounce window in milliseconds.
    ///
    /// Events for the same `(chain, mint)` that arrive within this window are
    /// merged into a single `SchedulerJob`.
    /// Default 500ms — balances freshness vs. evaluation cost.
    #[serde(default = "StreamingConfig::default_debounce_window_ms")]
    pub debounce_window_ms: u64,

    /// Bounded job queue capacity.
    ///
    /// When full, new jobs are dropped (backpressure safety).
    /// Default 4096 per design §4.
    #[serde(default = "StreamingConfig::default_queue_capacity")]
    pub queue_capacity: usize,

    /// Number of worker tasks.
    ///
    /// `0` = `tokio::available_parallelism() * 2`.
    #[serde(default = "StreamingConfig::default_worker_count")]
    pub worker_count: usize,

    /// `DetectorContext` window per streaming tick in minutes.
    #[serde(default = "StreamingConfig::default_window_minutes")]
    pub window_minutes: u32,

    /// GC interval in seconds.
    #[serde(default = "StreamingConfig::default_gc_interval_seconds")]
    pub gc_interval_seconds: u64,

    /// Hard cap on registered tokens.  LRU eviction when cap is reached.
    #[serde(default = "StreamingConfig::default_max_streaming_tokens")]
    pub max_streaming_tokens: usize,

    /// Idle timeout for tokens with no active WS subscribers, in minutes.
    ///
    /// # Calibration
    ///
    /// Default derived from pump.fun bonding-curve event cadence.
    /// Proper calibration requires a captured block-stream dataset with
    /// per-token inter-event gap distribution — measure p99 gap between
    /// Swap/Transfer events for a representative set of tracked tokens and
    /// set `idle_timeout > p99 + safety margin` so legitimate quiet periods
    /// don't trigger eviction.
    ///
    /// Method (Sprint 12 T4 observability deferred part):
    /// 1. Capture 24h of `PoolEvent::Swap` rows for 50 actively-traded pump.fun tokens.
    /// 2. Compute per-token inter-event gaps in seconds.
    /// 3. Take p99 across all tokens.
    /// 4. Set default = ceil(p99 / 60) + 5-minute safety margin.
    ///
    /// Without capture, the default stands as a reasonable starting point;
    /// once `streaming_detector_evaluation_duration_seconds` has production
    /// data, re-visit this config. Tracked: Sprint 12 task #4 part B.
    #[serde(default = "StreamingConfig::default_streaming_idle_timeout_minutes")]
    pub streaming_idle_timeout_minutes: u64,

    /// Per-detector evaluation timeout in milliseconds.
    ///
    /// A detector that exceeds this is cancelled; the outcome metric is
    /// labelled "timeout".  Default 3000ms per design §4 rev 1.
    #[serde(default = "StreamingConfig::default_per_detector_timeout_ms")]
    pub per_detector_timeout_ms: u64,

    /// Delta threshold below which per-detector score writes are skipped.
    ///
    /// When `max(|new[i] - prev[i]|) < threshold`, the `score()` +
    /// `risk_cache.insert` + `upsert_token_risk_report` writes are elided.
    /// Default 0.05 per design §4 rev 1.
    #[serde(default = "StreamingConfig::default_scoring_skip_delta_threshold")]
    pub scoring_skip_delta_threshold: f64,

    /// D01 (honeypot_sim) cadence: run D01 every Nth tick.
    ///
    /// D01 simulation is expensive (multiple RPC roundtrips per evaluation).
    /// Running it on every tick would dominate evaluation cost at scale.
    /// A modulo counter gates D01 to every Nth evaluation for a given token.
    ///
    /// Design: Option A from design 0014 §8 Phase 3 note.
    /// Rationale: simple, deterministic, no per-token state about token age.
    ///
    /// Default = 10: D01 fires on 1 in 10 ticks, balancing detection freshness
    /// (still fires within ~30s at typical 3s debounce) vs. RPC cost.
    ///
    /// Reference: `docs/designs/0014-streaming-detector.md` §8 Phase 3.
    #[serde(default = "StreamingConfig::default_d01_cadence_n")]
    pub streaming_d01_cadence_n: u64,

    /// Whether to persist `TokenRiskReport` rows to the `token_risk_reports`
    /// table (V00012) after each scoring tick.
    ///
    /// Default `false` — opt-in. When `false`, the server spawns workers without
    /// a `risk_report_store` and no Postgres writes occur for scoring output.
    /// The in-memory `RiskCache` (gateway/cache.rs) remains the sole hot cache
    /// in both modes.
    ///
    /// Set to `true` once V00012 migration has been applied and durable scoring
    /// persistence is desired (e.g. for crash recovery, audit trail, or backfill).
    ///
    /// Config path: `[streaming.persistence] token_risk_reports_enabled` in
    /// `config/service.toml`.
    #[serde(default = "StreamingConfig::default_token_risk_reports_enabled")]
    pub token_risk_reports_enabled: bool,
}

impl StreamingConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_debounce_window_ms() -> u64 {
        500
    }
    fn default_queue_capacity() -> usize {
        4096
    }
    fn default_worker_count() -> usize {
        0
    }
    fn default_window_minutes() -> u32 {
        60
    }
    fn default_gc_interval_seconds() -> u64 {
        120
    }
    fn default_max_streaming_tokens() -> usize {
        5000
    }
    fn default_streaming_idle_timeout_minutes() -> u64 {
        60
    }
    fn default_per_detector_timeout_ms() -> u64 {
        3000
    }
    fn default_scoring_skip_delta_threshold() -> f64 {
        0.05
    }
    fn default_d01_cadence_n() -> u64 {
        10
    }
    fn default_token_risk_reports_enabled() -> bool {
        false
    }
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            debounce_window_ms: Self::default_debounce_window_ms(),
            queue_capacity: Self::default_queue_capacity(),
            worker_count: Self::default_worker_count(),
            window_minutes: Self::default_window_minutes(),
            gc_interval_seconds: Self::default_gc_interval_seconds(),
            max_streaming_tokens: Self::default_max_streaming_tokens(),
            streaming_idle_timeout_minutes: Self::default_streaming_idle_timeout_minutes(),
            per_detector_timeout_ms: Self::default_per_detector_timeout_ms(),
            scoring_skip_delta_threshold: Self::default_scoring_skip_delta_threshold(),
            streaming_d01_cadence_n: Self::default_d01_cadence_n(),
            token_risk_reports_enabled: Self::default_token_risk_reports_enabled(),
        }
    }
}
