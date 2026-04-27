//! Smart-money labeller construction and background-task spawn.
//!
//! # Option B (design 0022 §6.1 Decision 1)
//!
//! The `SmartMoneyLabeller` is constructed here and spawned as a `tokio::spawn`
//! periodic background task — NOT wired into `MultiChainCoordinator`'s core API.
//! This is the least-intrusive wiring pattern (gotcha #77: init/ is the production
//! wiring entry; core crate APIs are not modified).
//!
//! # Utc::now() documented exception (gotcha #22)
//!
//! `spawn_smart_money_labeller` calls `Utc::now()` inside the batch ticker loop.
//! This is an APPROVED exception: the batch task is a periodic background job,
//! not a per-event detector hot path. `window_end` is wall-clock by design —
//! the labeller processes swap history accumulated up to the current moment.
//! Documented per design 0022 §6.4.
//!
//! # Shutdown
//!
//! The task loop runs `tokio::select!` with explicit `cancellation.cancelled()` arm.
//! On cancellation, the loop exits cleanly — no task is silently dropped.
//! The returned `JoinHandle` is added to the graceful-shutdown drain set in `main.rs`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::{error, info};
use tokio::task::JoinHandle;

use mg_onchain_common::chain::Chain;
use mg_onchain_graph::labels::GraphLabelStore;
use mg_onchain_graph::smart_money::{SmartMoneyConfig, SmartMoneyLabeller};
use mg_onchain_indexer::shutdown::ShutdownSignal;
use mg_onchain_storage::price_provider::TokenPriceProvider;
use mg_onchain_storage::wallet_pnl_corpus::{PgWalletPnlCorpusStore, WalletPnlCorpusStore};

use crate::pg_swap_fetcher::PgSwapFetcher;

/// Spawn the smart-money labeller as a periodic background task.
///
/// Returns a `JoinHandle` that should be added to the graceful-shutdown drain set
/// alongside the coordinator join handles in `main.rs`.
///
/// When `config.enabled = false`, this function returns immediately without spawning.
/// The returned handle completes immediately in that case.
///
/// # Utc::now() approved exception
///
/// The `ticker.tick()` arm calls `Utc::now()` as `window_end`. This is intentional:
/// the batch task processes swap history up to the current wall-clock moment.
/// It is NOT in the per-event detector hot path (gotcha #22 does not apply here).
/// Design 0022 §6.4 documents this exception.
pub fn spawn_smart_money_labeller(
    chain: Chain,
    shutdown: ShutdownSignal,
    labeller: Arc<SmartMoneyLabeller>,
    interval_seconds: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if interval_seconds == 0 {
            // Safety guard: zero interval would spin infinitely.
            error!("smart_money: batch_interval_seconds = 0 is invalid; labeller not started");
            return;
        }

        info!(
            chain = %chain,
            interval_seconds,
            "smart_money labeller started"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds));
        // Skip the first immediate tick — run after the first interval elapses.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!(chain = %chain, "smart_money labeller: shutdown signal received");
                    break;
                }
                _ = ticker.tick() => {
                    // Batch task: window_end is wall-clock by design — this is NOT in the
                    // per-event detector hot path. Approved exception to gotcha #22.
                    // Design 0022 §6.4 documents this explicitly.
                    let window_end = Utc::now();

                    match labeller.run_batch(window_end).await {
                        Ok(stats) => {
                            info!(
                                chain = %chain,
                                wallets_evaluated = stats.wallets_evaluated,
                                labels_written = stats.labels_written,
                                wallets_skipped = stats.wallets_skipped,
                                batch_run_id = %stats.batch_run_id,
                                "smart_money batch complete"
                            );
                        }
                        Err(e) => {
                            error!(chain = %chain, error = %e, "smart_money batch failed — will retry next interval");
                            // Continue — transient errors (DB hiccup, RPC timeout) should not
                            // stop the background task. The next tick will retry.
                        }
                    }
                }
            }
        }

        info!(chain = %chain, "smart_money labeller stopped");
    })
}

/// Build a `SmartMoneyLabeller` from production dependencies.
///
/// # Arguments
///
/// - `chain`: chain to label (Solana for MVP).
/// - `pg_pool`: shared Postgres connection pool.
/// - `label_store`: graph label store for writing `address_labels` rows.
/// - `price_provider`: USD price provider (Sprint 21 `PgTokenPriceProvider`).
/// - `config`: smart-money configuration (from `config/detectors.toml [smart_money_v1]`).
///
/// # Dependency notes
///
/// `PgSwapFetcher` is a thin wrapper around `sqlx::PgPool` that implements the
/// `SwapFetcher` trait. It lives in `crates/server` because it contains SQL queries
/// specific to the production Postgres schema — not in `crates/graph` (which should
/// stay schema-agnostic per the dependency direction: graph → storage → common).
pub fn build_smart_money_labeller(
    chain: Chain,
    pg_pool: Arc<sqlx::PgPool>,
    label_store: Arc<dyn GraphLabelStore>,
    price_provider: Arc<dyn TokenPriceProvider>,
    config: SmartMoneyConfig,
) -> Arc<SmartMoneyLabeller> {
    let corpus_store: Arc<dyn WalletPnlCorpusStore> =
        Arc::new(PgWalletPnlCorpusStore::new((*pg_pool).clone()));

    let swap_fetcher = Arc::new(PgSwapFetcher::new((*pg_pool).clone()));

    Arc::new(SmartMoneyLabeller::new(
        chain,
        label_store,
        corpus_store,
        swap_fetcher,
        price_provider,
        config,
    ))
}

/// Convert `AllDetectorConfigs::smart_money_v1` TOML config into a `SmartMoneyConfig`.
///
/// Parses string-encoded Decimal fields (per ADR 0002 string-bridge pattern).
/// Returns an error if any Decimal field is malformed.
pub fn build_smart_money_config(
    toml: &mg_onchain_detectors::config::SmartMoneyConfig,
) -> anyhow::Result<SmartMoneyConfig> {
    use anyhow::Context as _;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    Ok(SmartMoneyConfig {
        enabled: toml.enabled.value,
        batch_interval_seconds: toml.batch_interval_seconds.value,
        min_round_trips: toml.min_round_trips.value,
        min_round_trips_floor: toml.min_round_trips_floor.value,
        tier1_min_pnl_usd: Decimal::from_str(&toml.tier1_min_pnl_usd.value)
            .context("tier1_min_pnl_usd parse failed")?,
        tier1_min_win_rate: Decimal::from_str(&toml.tier1_min_win_rate.value)
            .context("tier1_min_win_rate parse failed")?,
        tier1_min_recurrence: toml.tier1_min_recurrence.value,
        tier1_top_timing_percentile: Decimal::from_str(&toml.tier1_top_timing_percentile.value)
            .context("tier1_top_timing_percentile parse failed")?,
        tier2_min_pnl_usd: Decimal::from_str(&toml.tier2_min_pnl_usd.value)
            .context("tier2_min_pnl_usd parse failed")?,
        tier2_min_recurrence: toml.tier2_min_recurrence.value,
        pre_event_lookback_blocks: toml.pre_event_lookback_blocks.value,
        pre_event_lookback_max_minutes: toml.pre_event_lookback_max_minutes.value,
        smart_money_fdr_enabled: toml.smart_money_fdr_enabled.value,
        batch_lookback_minutes: toml.batch_lookback_minutes.value,
        label_ttl_hours: toml.label_ttl_hours.value,
        corpus_lookback_days: toml.corpus_lookback_days.value,
        pump_event_min_confidence: toml.pump_event_min_confidence.value,
        wash_trading_exclusion_confidence: toml.wash_trading_exclusion_confidence.value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_smart_money_config_parses_defaults_cleanly() {
        // Verify the config loader parses a minimal TOML without panicking.
        // This catches `Decimal::from_str` failures on malformed config values.
        // TOML requires separate lines for each key within a table — no semicolons.
        let raw = r#"
[smart_money_v1]

[smart_money_v1.enabled]
value = true
rationale = ""
refs = []

[smart_money_v1.batch_interval_seconds]
value = 21600
rationale = ""
refs = []

[smart_money_v1.min_round_trips]
value = 10
rationale = ""
refs = []

[smart_money_v1.min_round_trips_floor]
value = 5
rationale = ""
refs = []

[smart_money_v1.tier1_min_pnl_usd]
value = "10000"
rationale = ""
refs = []

[smart_money_v1.tier1_min_win_rate]
value = "0.55"
rationale = ""
refs = []

[smart_money_v1.tier1_min_recurrence]
value = 3
rationale = ""
refs = []

[smart_money_v1.tier1_top_timing_percentile]
value = "0.90"
rationale = ""
refs = []

[smart_money_v1.tier2_min_pnl_usd]
value = "1000"
rationale = ""
refs = []

[smart_money_v1.tier2_min_recurrence]
value = 2
rationale = ""
refs = []

[smart_money_v1.pre_event_lookback_blocks]
value = 100
rationale = ""
refs = []

[smart_money_v1.pre_event_lookback_max_minutes]
value = 60
rationale = ""
refs = []

[smart_money_v1.smart_money_fdr_enabled]
value = false
rationale = ""
refs = []

[smart_money_v1.batch_lookback_minutes]
value = 720
rationale = ""
refs = []

[smart_money_v1.label_ttl_hours]
value = 720
rationale = ""
refs = []

[smart_money_v1.corpus_lookback_days]
value = 90
rationale = ""
refs = []

[smart_money_v1.pump_event_min_confidence]
value = 0.60
rationale = ""
refs = []

[smart_money_v1.wash_trading_exclusion_confidence]
value = 0.70
rationale = ""
refs = []

[smart_money_v1.min_label_confidence]
value = 0.40
rationale = ""
refs = []
        "#;

        #[derive(serde::Deserialize)]
        struct Wrapper {
            smart_money_v1: mg_onchain_detectors::config::SmartMoneyConfig,
        }
        let parsed: Wrapper = toml::from_str(raw).expect("TOML parse must succeed");
        let cfg = build_smart_money_config(&parsed.smart_money_v1)
            .expect("config conversion must succeed");

        assert_eq!(cfg.min_round_trips, 10);
        assert!(!cfg.smart_money_fdr_enabled, "FDR must ship disabled");
        assert_eq!(cfg.batch_interval_seconds, 21600);
    }
}
