//! Binary entry point for `onchain-service`.
//!
//! # Design reference
//!
//! `docs/designs/0020-server-binary-production-entry.md`
//!
//! # Initialization sequence (design 0020 §4)
//!
//! 1. Parse CLI args (clap)
//! 2. Load ServiceConfig from TOML
//! 3. Initialize tracing subscriber
//! 4. Connect Postgres + run migrations (D-A)
//! 5. Construct stores
//! 6. Load gateway config
//! 7. Construct chain adapters (D-E: Solana on, Ethereum off by default)
//! 8. Build coordinator + indexer hooks
//! 9. Build AppState + gateway
//! 10. Build detector set (all 12 — D01-D12)
//! 11. Spawn streaming subsystem
//! 12. Spawn coordinator + bridge task
//! 13. Run gateway (blocks until SIGTERM/SIGINT)
//! 14. Graceful shutdown drain (D-D: 30s default)
//!
//! # Gotcha #49 closure
//!
//! This file replaces the `fn main() {}` placeholder that has been open
//! since Sprint 12 (7 sprints). The stub had this comment:
//!   "Phase 2: wire AppState, load StreamingConfig, construct StreamingMetrics,
//!    call spawn_streaming_subsystem()."
//! This Sprint 19 S19-2 implementation fulfills all of that.
//!
//! # Gotcha #22: No Utc::now()
//!
//! No wall-clock timestamps in detector logic paths. All timing for
//! `DetectorContext.observed_at` comes from block headers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use tracing::{info, warn};

use mg_onchain_gateway::{GatewayConfig, run_gateway};
use mg_onchain_gateway::auth::jwt::JwtKeys;
use mg_onchain_gateway::metrics::GatewayMetrics;
use mg_onchain_gateway::state::AppState;
use mg_onchain_indexer::shutdown::ShutdownSignal;
use mg_onchain_scoring::ScoringEngine;
use mg_onchain_storage::PgStore;
use mg_onchain_token_registry::{RegistryConfig, TokenRegistry};

use mg_onchain_server::config::ServiceConfig;
use mg_onchain_server::init;
use mg_onchain_server::streaming_metrics::StreamingMetrics;
use mg_onchain_server::spawn_streaming_subsystem;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `onchain-service` — mg-onchain-analysis production binary.
///
/// Starts the multi-chain indexer, streaming detector scheduler, and REST/WS
/// gateway in a single process (ADR 0001 §D8, ADR 0003 single-deployable-unit).
#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    /// Path to `config/service.toml`.
    ///
    /// All runtime-configurable knobs live here. See docs/designs/0020 §7.
    #[arg(long, default_value = "config/service.toml")]
    config: PathBuf,

    /// Skip automatic migration on startup (D-A opt-out).
    ///
    /// Use in read-only replica deployments or CD pipelines that run migrations
    /// as a separate step. When not set (default), migrations are auto-applied.
    #[arg(long)]
    no_migrate: bool,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // -----------------------------------------------------------------------
    // Step 2: Load ServiceConfig
    //
    // Done BEFORE tracing init so config parse errors go to stderr as plain text.
    // This is intentional: if the config file is missing or malformed, the operator
    // needs to see the error without a tracing subscriber in the way.
    // -----------------------------------------------------------------------
    let service_config = ServiceConfig::load(&cli.config).with_context(|| {
        format!("failed to load service config from {}", cli.config.display())
    })?;

    // -----------------------------------------------------------------------
    // Step 3: Initialize tracing
    //
    // Must be first after config load so all subsequent steps emit structured logs.
    // -----------------------------------------------------------------------
    init::init_tracing(&service_config.observability)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        effective_config = %service_config.redacted_display(),
        "onchain-service starting"
    );

    // -----------------------------------------------------------------------
    // Step 4: Postgres connect + migrate (D-A)
    // -----------------------------------------------------------------------
    let pool = init::connect_postgres(&service_config.postgres).await?;

    if !cli.no_migrate {
        info!("running migrations (D-A: auto-migrate; use --no-migrate to skip)");
        init::run_migrations(&pool).await?;
    } else {
        info!("--no-migrate flag set; skipping automatic migration (D-A opt-out)");
    }

    // -----------------------------------------------------------------------
    // Step 5: Construct stores
    // -----------------------------------------------------------------------
    let pg_store = PgStore::new(pool.clone());
    let pg_pool_arc = Arc::new(pool.clone());

    // BocpdStateStore (V00013) — used by D09 streaming re-eval + hook
    let bocpd_state_store: Arc<dyn mg_onchain_detectors::BocpdStateStore> =
        Arc::new(mg_onchain_detectors::PgBocpdStateStore::new(pg_pool_arc.clone()));

    // AnomalyEventSink — D09/D10 indexer hooks write to anomaly_events
    let anomaly_sink_d09: Arc<dyn mg_onchain_detectors::AnomalyEventSink> =
        Arc::new(mg_onchain_detectors::PgAnomalyEventSink::new(pg_store.clone()));

    // D10 has its own AnomalyEventSink trait (same semantics, different trait def per gotcha #48)
    let anomaly_sink_d10: Arc<dyn mg_onchain_detectors::D10AnomalyEventSink> =
        Arc::new(PgD10AnomalyEventSink { store: pg_store.clone() });

    // -----------------------------------------------------------------------
    // Step 6: Load gateway config
    // -----------------------------------------------------------------------
    let gateway_config = GatewayConfig::from_file(&service_config.gateway.gateway_toml)
        .with_context(|| {
            format!(
                "failed to load gateway config from {}",
                service_config.gateway.gateway_toml
            )
        })?;

    // -----------------------------------------------------------------------
    // Step 7: Construct chain adapters (D-E)
    // -----------------------------------------------------------------------
    let solana_adapter = if service_config.chains.solana.enabled {
        info!("chains.solana.enabled = true — building Solana adapter (D-E)");
        Some(init::build_solana_adapter(&service_config.chains.solana)?)
    } else {
        info!("chains.solana.enabled = false — Solana adapter skipped (D-E)");
        None
    };

    let ethereum_adapter = if service_config.chains.ethereum.enabled {
        info!("chains.ethereum.enabled = true — building Ethereum adapter (D-E)");
        Some(init::build_ethereum_adapter(&service_config.chains.ethereum).await?)
    } else {
        info!("chains.ethereum.enabled = false — Ethereum adapter skipped (D-E)");
        None
    };

    // -----------------------------------------------------------------------
    // Step 8a: Build ShutdownSignal (shared by coordinator + scheduler)
    // -----------------------------------------------------------------------
    let shutdown = ShutdownSignal::from_os_signals();

    // -----------------------------------------------------------------------
    // Step 8b: Build coordinator
    // -----------------------------------------------------------------------
    let coordinator = init::build_coordinator(solana_adapter, ethereum_adapter, shutdown.clone());

    // -----------------------------------------------------------------------
    // Step 8c: Build indexer hooks (D09 + D10 composite per gotcha #48)
    //
    // SPEC-NOTE: D10TokenRegistry adapter — the D10IndexerHook requires a
    // `Arc<dyn D10TokenRegistry>` for fetching TokenMeta at pool init time.
    // We use a thin shim that delegates to the PgStore token lookup.
    // Full TokenRegistry enrichment (RPC calls) is deferred to Phase 5.
    // For Sprint 19, the D10 hook uses a NoopD10Registry that returns a
    // default TokenMeta (signal B: lp_locked_pct = 0, signal A: no price data).
    // -----------------------------------------------------------------------
    let d10_registry: Arc<dyn mg_onchain_detectors::d10_launch_audit::TokenRegistry> =
        Arc::new(NoopD10Registry);

    let edge_store = Arc::new(mg_onchain_graph::typed_edges::PgTypedEdgeStore::new(pool.clone()));
    let label_store = Arc::new(mg_onchain_graph::labels::PgGraphLabelStore::new(pool.clone()));

    // SPEC-NOTE: `pool_initialize_hook` is built here and held alive for the
    // process lifetime. The current `MultiChainCoordinator` (Pattern B, ADR 0005)
    // drives adapters directly without going through `Indexer`, so the hook is
    // not yet wired into the coordinator event loop. The coordinator's per-chain
    // tasks will call this hook when `Indexer::pool_initialize_hook` integration
    // lands in Sprint 20 (design 0020 §3 Step 8c follow-up).
    // The `Arc` keeps the hook stores alive so no reconstruction is needed then.
    let _pool_initialize_hook = init::build_pool_initialize_hook(
        bocpd_state_store.clone(),
        anomaly_sink_d09,
        anomaly_sink_d10,
        d10_registry,
        pg_pool_arc.clone(),
        edge_store,
        label_store,
    )?;

    // -----------------------------------------------------------------------
    // Step 9: Build AppState and gateway components
    // -----------------------------------------------------------------------
    let registry_config = RegistryConfig::default();
    let registry = TokenRegistry::with_http_rpc(registry_config, pg_store.clone());

    let scoring_config = mg_onchain_detectors::config::load_detector_config("config/detectors.toml")
        .context("failed to load detector config")?;
    let detector_config = scoring_config;

    let scoring = ScoringEngine::new(
        mg_onchain_scoring::config::ScoringConfig::default_calibrated(),
    );

    let jwt_keys = JwtKeys::from_pem_file(&gateway_config.gateway.auth.jwt_signing_key_path)
        .context("failed to load JWT keys from pem file")?;

    let gateway_metrics = GatewayMetrics::new()
        .context("failed to register gateway Prometheus metrics")?;

    // Build the coordinator for the pull-based query engine (ADR 0007 / design 0028 §4.5).
    // At this point we reconstruct a fresh coordinator reference for AppState.
    // The coordinator used for streaming (step 12) and this one share the same
    // ShutdownSignal so both coordinate on the same shutdown boundary.
    //
    // T26-6: populate detector_ids from build_all_detectors() so that the
    // cache-hit probe in trigger_evaluate iterates ALL registered detectors.
    // The detector list is built later in step 11 (spawn_streaming_subsystem);
    // for Sprint 26 we compute the ids from the static detector list here.
    //
    // Detector ids (alphabetical, matching build_all_detectors order):
    // bridge_drain_v1, deployer_changepoint, holder_concentration, honeypot_sim,
    // mint_burn_anomaly, permit2_drainer_v1, pump_dump, rug_pull_lp_drain,
    // sandwich_mev_v1, sybil_detection, synchronized_activity_v1, wash_trading_h1,
    // withdraw_withheld_drain.
    let all_detector_ids: Vec<String> = vec![
        "bridge_drain_v1".to_owned(),
        "deployer_changepoint".to_owned(),
        "holder_concentration".to_owned(),
        "honeypot_sim".to_owned(),
        "mint_burn_anomaly".to_owned(),
        "permit2_drainer_v1".to_owned(),
        "pump_dump".to_owned(),
        "rug_pull_lp_drain".to_owned(),
        "sandwich_mev_v1".to_owned(),
        "sybil_detection".to_owned(),
        "synchronized_activity_v1".to_owned(),
        "wash_trading_h1".to_owned(),
        "withdraw_withheld_drain".to_owned(),
    ];

    // Verdict cache store for the pull-based coordinator.
    let verdict_cache_store: Arc<dyn mg_onchain_storage::verdict_cache::VerdictCacheStore> =
        Arc::new(mg_onchain_storage::verdict_cache::PgVerdictCacheStore::new(pool.clone()));

    // Build the AppState coordinator (separate Arc from the streaming coordinator
    // so AppState owns a stable reference independent of coordinator.join() consuming it).
    let app_coordinator = Arc::new(
        mg_onchain_indexer::coordinator::MultiChainCoordinator::new(
            // No adapter slots here — the coordinator in AppState is used for trigger_evaluate,
            // not for streaming. Adapter slots are in the streaming coordinator (step 12).
            // T26-8: wire real adapter slots once T26-2 Solana rewrite is complete.
            vec![],
            shutdown.clone(),
        )
        .with_verdict_cache(verdict_cache_store, mg_onchain_indexer::trigger::VerdictCacheConfig::default())
        .with_detector_ids(all_detector_ids.clone())
        .with_max_concurrent(8),
    );

    let app_state = AppState::new(
        gateway_config,
        pg_store.clone(),
        registry,
        scoring,
        detector_config,
        jwt_keys,
        gateway_metrics,
        app_coordinator.clone(),
    );

    // -----------------------------------------------------------------------
    // Step 10: Build streaming metrics
    // -----------------------------------------------------------------------
    let streaming_metrics = Arc::new(
        StreamingMetrics::new()
            .context("failed to register streaming Prometheus metrics")?,
    );

    // -----------------------------------------------------------------------
    // Step 11: Spawn streaming subsystem (D01-D12 all wired)
    // -----------------------------------------------------------------------
    spawn_streaming_subsystem(
        app_state.clone(),
        service_config.streaming.clone(),
        streaming_metrics,
    )
    .await;

    // -----------------------------------------------------------------------
    // Step 11a: Spawn periodic scan workers (T26-6, ADR 0007 §6.4 / design 0028 §4.6)
    //
    // Two workers mirror the smart-money labeller pattern:
    // - watchlist_rescore_worker: every N minutes, re-scores all watchlisted tokens.
    // - launch_discovery_worker: every N minutes, discovers newly launched pools.
    //
    // Both use the same PeriodicScanConfig from service.toml [periodic_scan].
    // Cadence default: 5 minutes (ADR 0007 §9.4).
    // -----------------------------------------------------------------------
    let periodic_scan_cfg = service_config
        .periodic_scan
        .clone()
        .unwrap_or_default();

    let rescore_handle = init::spawn_watchlist_rescore_worker(
        mg_onchain_common::chain::Chain::Solana,
        app_coordinator.clone(),
        periodic_scan_cfg.clone(),
        shutdown.clone(),
    );

    let discovery_handle = init::spawn_launch_discovery_worker(
        mg_onchain_common::chain::Chain::Solana,
        app_coordinator.clone(),
        periodic_scan_cfg,
        shutdown.clone(),
    );

    // -----------------------------------------------------------------------
    // Step 11b: Spawn smart-money labeller (Sprint 22, design 0022 §6.1 Option B)
    //
    // The labeller is a periodic background task — NOT a streaming detector.
    // It runs every `batch_interval_seconds` (default 6h) on Solana, labelling
    // wallets with `LabelType::SmartMoney` based on realized PnL + timing alpha.
    //
    // Wired here (after streaming, before coordinator) so the label store
    // (`pg_pool_arc`) and price provider are fully initialised.
    //
    // Gotcha #22 approved exception: `Utc::now()` inside the spawn loop ticker
    // is documented in `init/smart_money.rs` — it is a batch job (not per-event).
    //
    // The returned `JoinHandle` is added to the shutdown drain below.
    // -----------------------------------------------------------------------
    let sm_label_store: Arc<dyn mg_onchain_graph::labels::GraphLabelStore> =
        Arc::new(mg_onchain_graph::labels::PgGraphLabelStore::new(pool.clone()));

    let sm_price_provider: Arc<dyn mg_onchain_storage::price_provider::TokenPriceProvider> =
        Arc::new(mg_onchain_storage::PgTokenPriceProvider::new(pg_pool_arc.clone()));

    let sm_config_result = init::build_smart_money_config(&app_state.detector_config.smart_money_v1);
    let sm_join_handle = match sm_config_result {
        Ok(sm_cfg) => {
            let sm_interval = sm_cfg.batch_interval_seconds;
            let sm_enabled = sm_cfg.enabled;
            let sm_labeller = init::build_smart_money_labeller(
                mg_onchain_common::chain::Chain::Solana,
                pg_pool_arc.clone(),
                sm_label_store,
                sm_price_provider,
                sm_cfg,
            );
            if sm_enabled {
                info!(
                    chain = "solana",
                    interval_seconds = sm_interval,
                    "smart_money labeller enabled — spawning background task"
                );
            } else {
                info!("smart_money.enabled = false — labeller task will exit immediately");
            }
            init::spawn_smart_money_labeller(
                mg_onchain_common::chain::Chain::Solana,
                shutdown.clone(),
                sm_labeller,
                sm_interval,
            )
        }
        Err(e) => {
            // Config parse failure is non-fatal: log + spawn a no-op task.
            // This keeps the boot sequence identical whether smart_money is enabled or not.
            warn!(
                error = %e,
                "smart_money config parse failed — labeller NOT started; \
                 check config/detectors.toml [smart_money_v1] section"
            );
            tokio::spawn(async {})
        }
    };

    // -----------------------------------------------------------------------
    // Step 12: Start coordinator + spawn bridge task
    // -----------------------------------------------------------------------
    let (coordinator_tx, coordinator_rx) =
        tokio::sync::mpsc::channel(256 * service_config.chains.active_count().max(1));

    // Start coordinator (spawns per-chain indexer tasks).
    // NoAdapters is returned when both chains are disabled — not fatal; log it.
    if let Err(e) = coordinator.start(coordinator_tx).await {
        warn!(error = %e, "coordinator start returned error — continuing without chain indexer");
    }

    // Bridge coordinator events → invalidation broadcast.
    let bridge_tx = app_state.invalidation_tx.clone();
    let bridge_shutdown = shutdown.clone();
    tokio::spawn(async move {
        init::coordinator_to_invalidation_bridge(coordinator_rx, bridge_tx, bridge_shutdown).await;
    });

    // -----------------------------------------------------------------------
    // Step 13: Run gateway (blocks until SIGTERM/SIGINT)
    // -----------------------------------------------------------------------
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "onchain-service ready"
    );
    run_gateway(app_state.clone()).await?;

    // -----------------------------------------------------------------------
    // Step 14: Graceful shutdown drain (D-D: 30s default)
    // -----------------------------------------------------------------------
    let drain_timeout_secs = service_config.shutdown.drain_timeout_seconds;
    info!(
        timeout_s = drain_timeout_secs,
        "shutdown requested; draining in-flight work (D-D)"
    );

    // Signal all coordinator tasks and background tasks to stop.
    shutdown.cancel();

    let drain_result = tokio::time::timeout(
        Duration::from_secs(drain_timeout_secs),
        async {
            coordinator.join().await;
            // Drain the smart-money labeller (it selects on shutdown signal).
            let _ = sm_join_handle.await;
            // Drain periodic scan workers (T26-6).
            let _ = rescore_handle.await;
            let _ = discovery_handle.await;
        },
    )
    .await;

    match drain_result {
        Ok(()) => {
            info!("drain complete — all coordinator and background tasks finished");
        }
        Err(_elapsed) => {
            warn!(
                timeout_s = drain_timeout_secs,
                "drain timed out — forcing exit (tasks abandoned)"
            );
        }
    }

    info!("onchain-service stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper adapters — thin shims for trait boundary mismatches
// ---------------------------------------------------------------------------

/// Thin shim adapting `PgStore::insert_anomaly_events` to the D10 `AnomalyEventSink` trait.
///
/// D09 and D10 each define their own `AnomalyEventSink` trait to avoid a crate
/// dependency cycle (gotcha #48). Both traits have the same method signature;
/// this shim bridges D10's trait to the `PgStore` implementation.
struct PgD10AnomalyEventSink {
    store: PgStore,
}

#[async_trait::async_trait]
impl mg_onchain_detectors::d10_launch_audit::AnomalyEventSink for PgD10AnomalyEventSink {
    async fn insert_anomaly_events(
        &self,
        events: &[mg_onchain_common::anomaly::AnomalyEvent],
        source: &str,
    ) -> anyhow::Result<()> {
        self.store
            .insert_anomaly_events(events, source)
            .await
            .map_err(|e| anyhow::anyhow!("PgD10AnomalyEventSink: {e}"))
    }
}

/// No-op D10 token registry shim for Sprint 19.
///
/// D10 uses `TokenRegistry::enrich` to fetch `TokenMeta` (lp_locked_pct, sol_price_usd)
/// at pool-init time. Full enrichment requires RPC calls; that is Phase 5 work.
/// For Sprint 19, the hook fires with a default `TokenMeta` — D10 Signal A (under-collat)
/// is skipped when `sol_price_usd = None`, and Signal B fires only when `lp_locked_pct = 0`
/// (the default). This is correct behavior per D10 design: "when price unavailable, A is skipped".
///
/// TODO(sprint-20): Replace with a real TokenRegistry that calls the RPC enrich path.
struct NoopD10Registry;

#[async_trait::async_trait]
impl mg_onchain_detectors::d10_launch_audit::TokenRegistry for NoopD10Registry {
    async fn enrich(
        &self,
        token: &str,
        chain: mg_onchain_common::chain::Chain,
    ) -> anyhow::Result<mg_onchain_common::token::TokenMeta> {
        use mg_onchain_common::chain::Address;
        use mg_onchain_common::token::{JupiterVerification, TokenMeta};
        use rust_decimal::Decimal;

        // Construct a minimal TokenMeta for the given token address.
        // Signal A (under-collat) is skipped when sol_price_usd is None —
        // D10 infers this from total_market_liquidity_usd = 0.
        // Signal B (lp_locked_pct < floor) fires because lockers = [] → 0% locked.
        // Per D10 design: "when price unavailable, Signal A is skipped".
        // TODO(sprint-20): Replace with real TokenRegistry RPC enrichment.
        let mint = Address::parse(chain, token)
            .map_err(|e| anyhow::anyhow!("NoopD10Registry: invalid token address {token}: {e}"))?;

        Ok(TokenMeta {
            mint,
            chain,
            symbol: None,
            name: None,
            decimals: 6,
            token_program: None,
            total_supply_raw: 0,
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
            verification: JupiterVerification::default(),
            rugcheck_score: None,
            buy_tax: None,
            sell_tax: None,
            transfer_tax: None,
            honeypot_flags: vec![],
            updated_at: chrono::Utc::now(),
        })
    }
}

// ---------------------------------------------------------------------------
// Extension trait for ChainsConfig
// ---------------------------------------------------------------------------

/// Extension on `ChainsConfig` for count helpers.
trait ChainsConfigExt {
    fn active_count(&self) -> usize;
}

impl ChainsConfigExt for mg_onchain_server::config::ChainsConfig {
    fn active_count(&self) -> usize {
        usize::from(self.solana.enabled) + usize::from(self.ethereum.enabled)
    }
}
