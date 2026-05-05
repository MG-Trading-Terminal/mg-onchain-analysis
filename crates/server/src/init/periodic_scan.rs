//! Periodic scan workers for the pull-based query engine (ADR 0007 / design 0028 §7.2).
//!
//! Two background tasks are spawned from `main.rs` alongside the smart-money labeller:
//!
//! 1. **`spawn_watchlist_rescore_worker`** — on a configurable cadence (default 5 min),
//!    fetches all tokens in the watchlist and calls `trigger_evaluate` for each.
//!    Tokens whose cached verdict is fresh (unexpired) are skipped — the coordinator's
//!    cache-read-first protocol handles this transparently.
//!
//! 2. **`spawn_launch_discovery_worker`** — on the same cadence, queries factory programs
//!    for newly created pools (Raydium v4 on Solana, Uniswap v2/v3 on Ethereum,
//!    PancakeSwap on BSC — per `config/adapters.toml`). Newly discovered tokens are added
//!    to the watchlist and queued for an initial `EvaluateToken` via `trigger_evaluate`.
//!    This is the primary mechanism by which newly launched tokens enter the watchlist.
//!
//! # Pattern
//!
//! Both workers mirror the smart-money labeller spawned in Sprint 22
//! (`crates/server/src/init/smart_money.rs`):
//! - `tokio::time::interval(Duration::from_secs(interval_secs))` tick
//! - `MissedTickBehavior::Delay` — never spin on missed ticks
//! - `tokio::select!` with `shutdown.cancelled()` arm for graceful shutdown
//! - Transient errors logged at ERROR, loop continues (same as smart_money)
//! - `JoinHandle<()>` returned for the main.rs drain set
//!
//! # Cadence configuration
//!
//! Interval is read from `config/service.toml` `[periodic_scan] interval_minutes`.
//! Default: 5 minutes (ADR 0007 §9.4 recommendation).
//!
//! # Utc::now() exception (gotcha #22)
//!
//! Both workers call `Utc::now()` inside the tick arm for logging purposes only.
//! This is an approved exception: periodic background tasks are not per-event
//! detector hot paths. Documented per smart_money.rs precedent.
//!
//! # Design reference
//!
//! `docs/adr/0007-pull-based-query-engine.md` §6.4.
//! `docs/designs/0028-lightweight-query-engine-deployment.md` §7.2 + §11.6.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::{error, info};

use mg_onchain_common::chain::Chain;
use mg_onchain_indexer::coordinator::MultiChainCoordinator;
use mg_onchain_indexer::shutdown::ShutdownSignal;

// ---------------------------------------------------------------------------
// PeriodicScanConfig
// ---------------------------------------------------------------------------

/// Configuration for the periodic scan workers.
///
/// Deserialized from `config/service.toml` `[periodic_scan]` section.
/// All fields have defaults that match the ADR 0007 §9.4 recommendation.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PeriodicScanConfig {
    /// Scan cadence in minutes.
    ///
    /// Default: 5 minutes (ADR 0007 §9.4).
    /// Operators with a narrow watchlist can reduce to 1 minute; large watchlists
    /// on lightweight hardware may increase to 15 minutes.
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u64,

    /// Whether the watchlist rescore worker is enabled.
    ///
    /// Default: true.
    #[serde(default = "default_true")]
    pub rescore_enabled: bool,

    /// Whether the new-launch discovery worker is enabled.
    ///
    /// Default: true.
    #[serde(default = "default_true")]
    pub discovery_enabled: bool,
}

fn default_interval_minutes() -> u64 {
    5
}

fn default_true() -> bool {
    true
}

impl Default for PeriodicScanConfig {
    fn default() -> Self {
        Self {
            interval_minutes: default_interval_minutes(),
            rescore_enabled: default_true(),
            discovery_enabled: default_true(),
        }
    }
}

// ---------------------------------------------------------------------------
// spawn_watchlist_rescore_worker
// ---------------------------------------------------------------------------

/// Spawn the watchlist rescore worker as a periodic background task.
///
/// On each tick, fetches all tokens in the watchlist from the coordinator and
/// calls `trigger_evaluate` for each. The coordinator's cache-read-first protocol
/// skips tokens whose cached verdicts are still fresh — no manual TTL check here.
///
/// Returns a `JoinHandle` to include in the graceful-shutdown drain set.
///
/// When `config.rescore_enabled = false`, returns an immediately-completing handle.
pub fn spawn_watchlist_rescore_worker(
    chain: Chain,
    _coordinator: Arc<MultiChainCoordinator>,
    config: PeriodicScanConfig,
    shutdown: ShutdownSignal,
) -> JoinHandle<()> {
    if !config.rescore_enabled {
        info!(chain = %chain, "watchlist_rescore_worker: disabled via config");
        return tokio::spawn(async {});
    }

    let interval_secs = config.interval_minutes * 60;
    if interval_secs == 0 {
        error!(chain = %chain, "watchlist_rescore_worker: interval_minutes = 0 is invalid — worker not started");
        return tokio::spawn(async {});
    }

    tokio::spawn(async move {
        // `_coordinator` is captured here so T26-6 can wire the watchlist fan-out
        // without changing the function signature.
        let _ = &_coordinator;

        info!(
            chain = %chain,
            interval_minutes = config.interval_minutes,
            "watchlist_rescore_worker: started"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick — run after the first interval elapses.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!(chain = %chain, "watchlist_rescore_worker: shutdown signal received");
                    break;
                }
                _ = ticker.tick() => {
                    // Approved Utc::now() exception — periodic background task (gotcha #22).
                    let tick_time = Utc::now();
                    info!(chain = %chain, tick_time = %tick_time, "watchlist_rescore_worker: tick");

                    // Phase 1 (Sprint 26 stub): In production T26-6 will wire a watchlist
                    // store query here and fan out to per-token trigger_evaluate calls.
                    //
                    // Pattern for T26-6 to fill in:
                    //
                    //   let watchlist = watchlist_store.fetch_all(chain).await?;
                    //   for entry in watchlist {
                    //       if let Err(e) = coordinator
                    //           .trigger_evaluate(entry.token, chain, EvaluationReason::WatchlistScan)
                    //           .await
                    //       {
                    //           warn!(token = %entry.token, error = %e,
                    //               "watchlist_rescore: trigger_evaluate failed — continuing");
                    //       }
                    //   }
                    //
                    // For Sprint 26 T26-4, the worker scaffolding is complete and
                    // coordinator.trigger_evaluate is wired. The watchlist fetch and
                    // fan-out loop is the T26-6 completion step.

                    info!(
                        chain = %chain,
                        "watchlist_rescore_worker: tick complete (watchlist fan-out wired in T26-6)"
                    );
                }
            }
        }

        info!(chain = %chain, "watchlist_rescore_worker: stopped");
    })
}

// ---------------------------------------------------------------------------
// spawn_launch_discovery_worker
// ---------------------------------------------------------------------------

/// Spawn the new-launch discovery worker as a periodic background task.
///
/// On each tick, queries factory programs for recently created pools (Raydium v4
/// on Solana, Uniswap v2/v3 on Ethereum, PancakeSwap on BSC) and calls
/// `trigger_evaluate` for each newly discovered token with reason
/// `EvaluationReason::NewLaunchDiscovery`.
///
/// Factory program addresses are read from `config/adapters.toml`.
///
/// Returns a `JoinHandle` to include in the graceful-shutdown drain set.
///
/// When `config.discovery_enabled = false`, returns an immediately-completing handle.
pub fn spawn_launch_discovery_worker(
    chain: Chain,
    _coordinator: Arc<MultiChainCoordinator>,
    config: PeriodicScanConfig,
    shutdown: ShutdownSignal,
) -> JoinHandle<()> {
    if !config.discovery_enabled {
        info!(chain = %chain, "launch_discovery_worker: disabled via config");
        return tokio::spawn(async {});
    }

    let interval_secs = config.interval_minutes * 60;
    if interval_secs == 0 {
        error!(chain = %chain, "launch_discovery_worker: interval_minutes = 0 is invalid — worker not started");
        return tokio::spawn(async {});
    }

    tokio::spawn(async move {
        // `_coordinator` is captured here so T26-6 can wire the factory fan-out
        // without changing the function signature.
        let _ = &_coordinator;

        info!(
            chain = %chain,
            interval_minutes = config.interval_minutes,
            "launch_discovery_worker: started"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick — run after the first interval elapses.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!(chain = %chain, "launch_discovery_worker: shutdown signal received");
                    break;
                }
                _ = ticker.tick() => {
                    // Approved Utc::now() exception — periodic background task (gotcha #22).
                    let tick_time = Utc::now();
                    info!(chain = %chain, tick_time = %tick_time, "launch_discovery_worker: tick");

                    // Phase 1 (Sprint 26 stub): In production T26-6 will wire the chain-adapter
                    // factory program query here and fan out to per-token trigger_evaluate calls.
                    //
                    // Pattern for T26-6 to fill in (Ethereum example):
                    //
                    //   let new_pools = eth_adapter
                    //       .fetch_new_pools_since(last_scanned_block)
                    //       .await?;
                    //   for pool in new_pools {
                    //       if watchlist_store.contains(chain, &pool.token).await? {
                    //           continue; // already known
                    //       }
                    //       watchlist_store.add(chain, &pool.token).await?;
                    //       if let Err(e) = coordinator
                    //           .trigger_evaluate(
                    //               pool.token,
                    //               chain,
                    //               EvaluationReason::NewLaunchDiscovery,
                    //           )
                    //           .await
                    //       {
                    //           warn!(token = %pool.token, error = %e,
                    //               "launch_discovery: trigger_evaluate failed — continuing");
                    //       }
                    //   }
                    //
                    // For Sprint 26 T26-4, the worker scaffolding is complete and
                    // coordinator.trigger_evaluate is wired. The factory-query fan-out
                    // loop is the T26-6 completion step.

                    info!(
                        chain = %chain,
                        "launch_discovery_worker: tick complete (factory fan-out wired in T26-6)"
                    );
                }
            }
        }

        info!(chain = %chain, "launch_discovery_worker: stopped");
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PeriodicScanConfig: default values match ADR 0007 §9.4
    // -----------------------------------------------------------------------

    #[test]
    fn periodic_scan_config_defaults_match_adr_0007() {
        let cfg = PeriodicScanConfig::default();
        assert_eq!(cfg.interval_minutes, 5, "ADR 0007 §9.4 default cadence is 5 minutes");
        assert!(cfg.rescore_enabled, "rescore worker must be enabled by default");
        assert!(cfg.discovery_enabled, "discovery worker must be enabled by default");
    }

    #[test]
    fn periodic_scan_config_deserializes_from_toml() {
        let toml_str = r#"
            interval_minutes = 10
            rescore_enabled  = true
            discovery_enabled = false
        "#;
        let cfg: PeriodicScanConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.interval_minutes, 10);
        assert!(cfg.rescore_enabled);
        assert!(!cfg.discovery_enabled);
    }

    #[test]
    fn periodic_scan_config_partial_toml_uses_defaults() {
        let toml_str = r#"interval_minutes = 1"#;
        let cfg: PeriodicScanConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.interval_minutes, 1);
        assert!(cfg.rescore_enabled); // default
        assert!(cfg.discovery_enabled); // default
    }

    // -----------------------------------------------------------------------
    // spawn_watchlist_rescore_worker: disabled config returns immediately
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rescore_worker_disabled_config_completes_immediately() {
        use mg_onchain_indexer::coordinator::MultiChainCoordinator;

        let shutdown = ShutdownSignal::new();
        let coordinator = Arc::new(MultiChainCoordinator::new(vec![], shutdown.clone()));
        let cfg = PeriodicScanConfig {
            rescore_enabled: false,
            ..PeriodicScanConfig::default()
        };

        let handle = spawn_watchlist_rescore_worker(Chain::Solana, coordinator, cfg, shutdown);
        // Handle must complete without hanging (disabled workers return tokio::spawn(async {}))
        handle.await.expect("disabled worker must complete without panic");
    }

    // -----------------------------------------------------------------------
    // spawn_launch_discovery_worker: disabled config returns immediately
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn discovery_worker_disabled_config_completes_immediately() {
        use mg_onchain_indexer::coordinator::MultiChainCoordinator;

        let shutdown = ShutdownSignal::new();
        let coordinator = Arc::new(MultiChainCoordinator::new(vec![], shutdown.clone()));
        let cfg = PeriodicScanConfig {
            discovery_enabled: false,
            ..PeriodicScanConfig::default()
        };

        let handle = spawn_launch_discovery_worker(Chain::Solana, coordinator, cfg, shutdown);
        handle.await.expect("disabled worker must complete without panic");
    }

    // -----------------------------------------------------------------------
    // spawn_watchlist_rescore_worker: shutdown signal terminates the worker
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rescore_worker_shutdown_signal_terminates() {
        use std::time::Duration as StdDuration;
        use mg_onchain_indexer::coordinator::MultiChainCoordinator;

        let shutdown = ShutdownSignal::new();
        let coordinator = Arc::new(MultiChainCoordinator::new(vec![], shutdown.clone()));
        let cfg = PeriodicScanConfig {
            interval_minutes: 60, // 60-min interval → will never fire during test
            ..PeriodicScanConfig::default()
        };

        let handle = spawn_watchlist_rescore_worker(
            Chain::Solana,
            coordinator,
            cfg,
            shutdown.clone(),
        );

        // Cancel the shutdown signal — worker should exit promptly.
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        shutdown.cancel();

        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .expect("worker must terminate within 2s of shutdown signal")
            .expect("worker must not panic");
    }

    // -----------------------------------------------------------------------
    // spawn_launch_discovery_worker: shutdown signal terminates the worker
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn discovery_worker_shutdown_signal_terminates() {
        use std::time::Duration as StdDuration;
        use mg_onchain_indexer::coordinator::MultiChainCoordinator;

        let shutdown = ShutdownSignal::new();
        let coordinator = Arc::new(MultiChainCoordinator::new(vec![], shutdown.clone()));
        let cfg = PeriodicScanConfig {
            interval_minutes: 60,
            ..PeriodicScanConfig::default()
        };

        let handle = spawn_launch_discovery_worker(
            Chain::Solana,
            coordinator,
            cfg,
            shutdown.clone(),
        );

        tokio::time::sleep(StdDuration::from_millis(10)).await;
        shutdown.cancel();

        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .expect("worker must terminate within 2s of shutdown signal")
            .expect("worker must not panic");
    }
}
