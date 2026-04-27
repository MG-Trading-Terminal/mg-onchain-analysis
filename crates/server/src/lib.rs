//! `mg-onchain-server` — binary orchestration library.
//!
//! Exposes `streaming_config`, `streaming_metrics`, and the `streaming`
//! module for use in `main.rs` and in integration tests.

pub mod config;
pub mod erased_detector;
pub mod init;
pub mod pg_swap_fetcher;
pub mod risk_report_store;
pub mod streaming;
pub mod streaming_config;
pub mod streaming_metrics;

use std::sync::Arc;

use chrono::Duration;
use tokio::sync::RwLock;
use tracing::info;

use mg_onchain_gateway::state::AppState;
use mg_onchain_scoring::ScoringEngine;
use risk_report_store::{PgTokenRiskReportStore, TokenRiskReportStore};

use erased_detector::ArcErasedDetector;
use streaming::registry::StreamingRegistry;
use streaming::scheduler::DetectorScheduler;
use streaming::worker::SchedulerWorker;
use streaming_config::StreamingConfig;
use streaming_metrics::StreamingMetrics;

/// Spawn the streaming detector scheduler and worker pool.
///
/// Called from `main.rs` after `AppState` is built.
/// Gated by `config.enabled`; when `false`, returns immediately without
/// spawning anything — the server boots normally without the streaming
/// subsystem.
///
/// # Worker count
///
/// When `config.worker_count == 0`, defaults to
/// `tokio::available_parallelism() * 2` (capped at 2 on error).
///
/// # Sprint 19 — 11 streaming detectors wired
///
/// Sprint 19 wires D01-D09, D11, D12 as streaming detectors.
/// D10 (`launch_audit`) is hook-triggered only and is NOT in the streaming set.
/// Workers hold `PgStore`, `TokenRegistry`, `DetectorConfig`, and
/// `ScoringEngine` borrowed from `AppState`.
pub async fn spawn_streaming_subsystem(
    state: Arc<AppState>,
    config: StreamingConfig,
    metrics: Arc<StreamingMetrics>,
) {
    if !config.enabled {
        info!("streaming.enabled = false — scheduler not spawned");
        return;
    }

    let worker_count = if config.worker_count == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(2)
    } else {
        config.worker_count
    };

    info!(
        worker_count,
        queue_capacity = config.queue_capacity,
        debounce_window_ms = config.debounce_window_ms,
        "spawning streaming detector scheduler (Sprint 19: D01-D09/D11/D12 wired)"
    );

    // Build the MPMC async-channel queue.
    let (queue_tx, queue_rx) =
        async_channel::bounded::<streaming::scheduler::SchedulerJob>(config.queue_capacity);

    // Build the shared streaming registry.
    let idle_timeout = Duration::minutes(config.streaming_idle_timeout_minutes as i64);
    let registry = Arc::new(RwLock::new(StreamingRegistry::new(
        config.max_streaming_tokens,
        idle_timeout,
    )));

    // Spawn the scheduler.
    let scheduler = DetectorScheduler {
        invalidation_rx: state.invalidation_tx.subscribe(),
        queue_tx,
        registry: registry.clone(),
        config: config.clone(),
        metrics: metrics.clone(),
    };
    tokio::spawn(scheduler.run());

    // Build the shared scoring engine wrapped in Arc (ScoringEngine is Clone
    // but Arc avoids cloning the ScoringConfig for each worker).
    let scoring = Arc::new(ScoringEngine::new(state.scoring.config.clone()));

    // Build the streaming detector set (D01-D09, D11, D12 — 11 detectors).
    //
    // D10 (launch_audit) is hook-triggered only and does NOT implement Detector;
    // it is excluded from this list. The `SchedulerWorker` chain-filter ensures:
    //   - D01-D09, D11 skip on Ethereum contexts (supported_chains = &[Chain::Solana])
    //   - D12 skips on Solana contexts (supported_chains = &[Chain::Ethereum])
    //
    // D09 is dual-path: event-driven via PoolInitializeHook AND streaming re-eval.
    // D09 requires a BocpdStateStore; here we use PgBocpdStateStore from the pool.
    // D12 uses the pool directly for permit2_events queries.
    //
    // When `state.store` is not connected to a live DB (CI / unit tests),
    // the `build_all_detectors` call will succeed but D09/D11/D12 will
    // return errors from their DB queries at evaluation time — those errors
    // are logged as warnings per SchedulerWorker's best-effort semantics.
    let detector_config = Arc::new(state.detector_config.clone());
    let rpc = state.registry.rpc();
    let pg_pool = Arc::new(state.store.pool().clone());
    let bocpd_state_store: Arc<dyn mg_onchain_detectors::BocpdStateStore> =
        Arc::new(mg_onchain_detectors::PgBocpdStateStore::new(pg_pool.clone()));
    // Phase 5 USD enrichment (Sprint 21): construct PgTokenPriceProvider once, shared by D11/D12/D13.
    let price_provider: Arc<dyn mg_onchain_storage::price_provider::TokenPriceProvider> =
        Arc::new(mg_onchain_storage::PgTokenPriceProvider::new(pg_pool.clone()));
    let detectors: Vec<ArcErasedDetector> =
        init::detectors::build_all_detectors(&state.detector_config, pg_pool, bocpd_state_store, rpc, price_provider)
            .unwrap_or_else(|e| {
                // D09 weight validation failure is a config bug — log and fall back to 5-detector set.
                // This path should never occur with valid config/detectors.toml.
                tracing::error!(
                    error = %e,
                    "build_all_detectors failed — falling back to 5-detector set. \
                     Check config/detectors.toml deployer_changepoint weights."
                );
                vec![]
            });

    info!(
        detector_count = detectors.len(),
        "streaming workers: 11 detectors registered (D01-D09/D11/D12; D10 is hook-only)"
    );

    // Build optional durable risk report store (V00012).
    //
    // `token_risk_reports_enabled = false` (default) → None; workers log + continue
    // without persisting. Set to true in config after V00012 migration is applied.
    let risk_report_store: Option<Arc<dyn TokenRiskReportStore>> = if config
        .token_risk_reports_enabled
    {
        info!("token_risk_reports persistence ENABLED (V00012)");
        Some(Arc::new(PgTokenRiskReportStore::from_pg_store(
            &state.store,
        )))
    } else {
        info!(
            "token_risk_reports persistence disabled (set streaming.token_risk_reports_enabled=true to enable)"
        );
        None
    };

    // Spawn workers.
    for _ in 0..worker_count {
        let worker = SchedulerWorker {
            queue_rx: queue_rx.clone(),
            config: config.clone(),
            metrics: metrics.clone(),
            detectors: detectors.clone(),
            store: Some(state.store.clone()),
            registry: Some(state.registry.clone()),
            scoring: Some(scoring.clone()),
            detector_config: Some(detector_config.clone()),
            risk_report_store: risk_report_store.clone(),
        };
        tokio::spawn(worker.run());
    }
}
