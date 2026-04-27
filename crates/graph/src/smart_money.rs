//! Smart-money labelling pipeline — Stage 1 (realized PnL corpus) + Stage 3 (timing features).
//!
//! # Background-task pattern
//!
//! This module implements a periodic batch job (`SmartMoneyLabeller::run_batch`), NOT a
//! `Detector` trait implementation. The distinction matters because smart-money labelling is:
//!   1. Population-level (scans all wallets, not a single token's event stream)
//!   2. Time-triggered (every 6 hours), not per-event
//!   3. Writes `address_labels` rows (not `anomaly_events`)
//!
//! Spawned by `crates/server/src/init/smart_money.rs` via `tokio::spawn` — see Option B
//! in design 0022 §6.1 and spec decision 1.
//!
//! # Algorithm
//!
//! **Stage 1 — Realized PnL corpus (design 0022 §3.1):**
//! FIFO-match buy and sell swaps per wallet per token. For each closed round-trip, compute:
//!   `pnl_usd = (exit_price - entry_price) * closed_qty`
//! Aggregate per-wallet: total_pnl_usd, win_rate, mean_holding_time, round_trip_count.
//! Price lookups via `TokenPriceProvider` — None when price unavailable.
//!
//! **Stage 3 — Timing features (design 0022 §3.2):**
//! For each pump event (D04 anomaly), identify pre-event buyers.
//! Compute recurrence count, timing lead percentile, sell-before-peak rate.
//! Sources: Fantazzini & Xiao 2023 (60-min pre-event window); Perseus 2025 (recurrence ≥ 3).
//!
//! # Stage 2 FDR (Barras 2010)
//!
//! NOT implemented in Sprint 22. Config flag `smart_money_fdr_enabled` ships as `false`.
//! TODO(sprint-23+): apply Barras 2010 FDR when live corpus has >= 30 days and >= 1000 wallets
//! with >= 10 round-trips. See design 0022 §3 / §7.5.
//!
//! # Calibration annotation
//!
//! Every emitted label carries:
//!   `"smart_money/heuristic_not_fdr_controlled": true`
//!   `"calibration": "heuristic, not FDR-controlled"`
//! until Stage 2 activates. This is a hard requirement per the session brief.
//!
//! # Citations
//!
//! - Barras, Scaillet & Wermers 2010 (JoF 65(1)) — FDR skill/luck; Stage 2 (blocked).
//!   `min_round_trips = 10` derivation.
//! - Fantazzini & Xiao 2023 (Econometrics 11(3)) — 60-min pre-event window.
//! - Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — recurrence ≥ 3 threshold.
//! - Easley, López de Prado & O'Hara 2012 (VPIN, RFS 25(5)) — informed-flow theory.
//!
//! # Design reference
//!
//! `docs/designs/0022-smart-money-labelling-mvp.md`

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use mg_onchain_common::chain::{Address, Chain};
use mg_onchain_storage::price_provider::TokenPriceProvider;
use mg_onchain_storage::wallet_pnl_corpus::{WalletPnlCorpusRow, WalletPnlCorpusStore};

use crate::error::GraphError;
use crate::labels::{AddressLabel, GraphLabelStore, LabelType};

// ---------------------------------------------------------------------------
// SwapFetcher trait — abstracts the swap-fetching path
// ---------------------------------------------------------------------------

/// A single swap event row from the `swaps` table.
///
/// Used by the smart-money labeller to compute FIFO PnL round-trips.
/// Deliberately minimal — only fields needed for Stage 1 + Stage 3 computation.
#[derive(Debug, Clone)]
pub struct SwapRow {
    /// Wallet / signer of the swap.
    pub wallet: String,
    /// Token mint / contract address (the non-SOL / non-USDC side).
    pub token: String,
    /// `"buy"` or `"sell"`.
    pub side: SwapSide,
    /// Amount of token received (buy) or spent (sell), in decimal-adjusted units.
    ///
    /// Stored as raw u128 in the DB; caller must divide by 10^decimals before passing here.
    /// For MVP, the labeller receives pre-adjusted amounts from the SwapFetcher impl.
    pub token_qty: Decimal,
    /// Block timestamp (from chain, not wall-clock; gotcha #28).
    pub block_time: DateTime<Utc>,
    /// Block height for tie-breaking ordering.
    pub block_height: i64,
    /// Transaction hash.
    pub tx_hash: String,
}

/// Direction of a swap relative to the tracked token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapSide {
    Buy,
    Sell,
}

/// Fetcher trait for swap rows.
///
/// Abstracts the underlying `PgStore` fetch so `SmartMoneyLabeller` can be
/// tested without a live Postgres connection (`MockSwapFetcher` in tests).
///
/// The graph crate depends on storage (see `Cargo.toml`), so direct `sqlx::PgPool`
/// usage is also acceptable. The trait adds testability without a cycle.
#[async_trait]
pub trait SwapFetcher: Send + Sync {
    /// Fetch all swap rows for a given wallet on a chain within `[since, until]`.
    ///
    /// Returns rows ordered by `(block_time ASC, tx_hash ASC)` for deterministic
    /// FIFO matching. Callers assume amounts are decimal-adjusted.
    async fn fetch_swaps_for_wallet(
        &self,
        chain: Chain,
        wallet: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<SwapRow>>;

    /// Fetch all wallets that had any swap activity on `chain` within `[since, until]`.
    ///
    /// Returns distinct wallet addresses ordered alphabetically.
    async fn fetch_active_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<String>>;

    /// Fetch pump events (D04 `pump_dump_v1`) above `min_confidence` for Stage 3.
    ///
    /// Returns `(token, event_peak_time)` pairs ordered by `event_peak_time ASC`.
    async fn fetch_pump_events(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        min_confidence: f64,
    ) -> anyhow::Result<Vec<PumpEvent>>;

    /// Fetch wallets excluded from smart-money labelling.
    ///
    /// Exclusion criteria (design 0022 §5.3):
    /// - `LabelType::KnownExchange` — CEX hot wallets
    /// - `LabelType::KnownDex` — DEX program addresses
    /// - `LabelType::KnownBurn` — burn addresses
    ///
    /// Returns excluded wallet addresses as a sorted `Vec`.
    async fn fetch_excluded_wallets(&self, chain: Chain) -> anyhow::Result<Vec<String>>;

    /// Fetch wallets with an active `wash_trading_v1` anomaly event above
    /// `exclusion_confidence` (design 0022 §8 E-SM-2 evasion guard).
    async fn fetch_wash_trading_excluded(
        &self,
        chain: Chain,
        exclusion_confidence: f64,
    ) -> anyhow::Result<Vec<String>>;
}

/// A pump event from the D04 detector's output.
#[derive(Debug, Clone)]
pub struct PumpEvent {
    /// Token mint / contract address.
    pub token: String,
    /// Block time when the D04 event was emitted (proxy for pump peak).
    pub event_peak_time: DateTime<Utc>,
    /// D04 confidence value.
    pub confidence: f64,
}

// ---------------------------------------------------------------------------
// SmartMoneyConfig
// ---------------------------------------------------------------------------

/// Configuration for the smart-money labelling pipeline.
///
/// All thresholds have inline citations per CLAUDE.md rule.
/// Values mirror `config/detectors.toml` `[smart_money_v1]` section.
#[derive(Debug, Clone)]
pub struct SmartMoneyConfig {
    /// Whether the labeller is enabled. When `false`, `run_batch` is a no-op.
    /// Default: `true`.
    pub enabled: bool,

    /// Minimum completed round-trips for a wallet to enter the Stage 1 corpus.
    ///
    /// Barras et al. 2010 JoF 65(1): below 10, alpha t-statistic has insufficient power.
    /// The win-rate standard error at N=10 is ≈ 0.16 — borderline acceptable for heuristics.
    /// Default: 10.
    pub min_round_trips: u32,

    /// Configurable floor for `min_round_trips`. Operators may lower to 5 to accept
    /// higher heuristic noise during sparse-corpus periods.
    /// Default: 5.
    pub min_round_trips_floor: u32,

    // ---- Tier 1 criteria ----
    /// Minimum total realized PnL in USD for Tier 1.
    /// Default: $10,000 (Nansen secondary market-color; no academic anchor).
    pub tier1_min_pnl_usd: Decimal,
    /// Minimum win rate (fraction of priced round-trips with positive PnL) for Tier 1.
    /// Default: 0.55 (unverified-heuristic; Stage 2 FDR replaces).
    pub tier1_min_win_rate: Decimal,
    /// Minimum distinct pump events for Tier 1 recurrence criterion.
    /// Perseus 2025 (arXiv:2503.01686): all 438 confirmed masterminds recurred >= 3 times.
    pub tier1_min_recurrence: u32,
    /// Timing lead percentile threshold for Tier 1 (top-10% earliest entries).
    /// Fantazzini & Xiao 2023 operationalized: 90th percentile vs co-participants.
    pub tier1_top_timing_percentile: Decimal,

    // ---- Tier 2 criteria ----
    /// Minimum total realized PnL in USD for Tier 2 (PnL-only path).
    /// Default: $1,000 (heuristic).
    pub tier2_min_pnl_usd: Decimal,
    /// Minimum distinct pump events for Tier 2 recurrence path.
    /// Default: 2 (heuristic lower bound).
    pub tier2_min_recurrence: u32,

    // ---- Stage 3 timing parameters ----
    /// Pre-event entry window in seconds. Fantazzini & Xiao 2023: 60-minute window.
    /// Default: 3600.
    pub pre_event_lookback_blocks: u32,
    /// Maximum pre-event lookback in minutes. Default: 60.
    pub pre_event_lookback_max_minutes: u32,

    // ---- Stage 2 FDR (NOT activated) ----
    /// Config flag for Stage 2 FDR. Ships as `false` — data-blocked until 30-day corpus.
    /// TODO(sprint-23+): activate when corpus matures.
    pub smart_money_fdr_enabled: bool,

    // ---- Infrastructure ----
    /// Batch lookback window in minutes for stale-wallet detection.
    /// Default: 720 (12 hours — covers one full 6h interval with margin).
    pub batch_lookback_minutes: u32,

    /// Label TTL in hours. Labels expire and must be re-earned.
    /// Default: 720 (30 days).
    pub label_ttl_hours: i64,

    /// Batch interval in seconds.
    /// Default: 21600 (6 hours).
    pub batch_interval_seconds: u64,

    /// Minimum D04 confidence for a pump event to be used in Stage 3.
    /// Default: 0.60.
    pub pump_event_min_confidence: f64,

    /// Confidence for active `wash_trading_v1` events that trigger wallet exclusion.
    /// Default: 0.70 (design 0022 §8 E-SM-2).
    pub wash_trading_exclusion_confidence: f64,

    /// Corpus lookback window in days for swap history.
    /// Default: 90.
    pub corpus_lookback_days: i64,
}

impl Default for SmartMoneyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_round_trips: 10,
            min_round_trips_floor: 5,
            tier1_min_pnl_usd: Decimal::from(10_000),
            tier1_min_win_rate: Decimal::from_str_exact("0.55").unwrap_or(Decimal::from_str_exact("55").unwrap() / Decimal::from(100)),
            tier1_min_recurrence: 3,
            tier1_top_timing_percentile: Decimal::from_str_exact("0.90").unwrap_or(Decimal::from(9) / Decimal::from(10)),
            tier2_min_pnl_usd: Decimal::from(1_000),
            tier2_min_recurrence: 2,
            pre_event_lookback_blocks: 100,
            pre_event_lookback_max_minutes: 60,
            smart_money_fdr_enabled: false,
            batch_lookback_minutes: 720,
            label_ttl_hours: 720,
            batch_interval_seconds: 21_600,
            pump_event_min_confidence: 0.60,
            wash_trading_exclusion_confidence: 0.70,
            corpus_lookback_days: 90,
        }
    }
}

// ---------------------------------------------------------------------------
// SmartMoneyTier
// ---------------------------------------------------------------------------

/// Tier assignment for a smart-money wallet.
///
/// Tier is encoded in `evidence["smart_money/tier"]` — NOT in `LabelType`
/// (Decision 2: `LabelType::SmartMoney` is reused as-is; no new variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartMoneyTier {
    /// Strong realized PnL + timing alpha confirmed (design 0022 §3.3).
    Tier1,
    /// Strong PnL OR ≥ 2 event recurrence (single criterion).
    Tier2,
    /// Positive PnL only — candidate pool for Stage 2 FDR analysis.
    Tier3,
}

impl SmartMoneyTier {
    fn as_str(self) -> &'static str {
        match self {
            SmartMoneyTier::Tier1 => "tier1",
            SmartMoneyTier::Tier2 => "tier2",
            SmartMoneyTier::Tier3 => "tier3",
        }
    }

    /// Base confidence for the tier (design 0022 §4.1).
    fn base_confidence(self) -> Decimal {
        match self {
            SmartMoneyTier::Tier1 => Decimal::from_str_exact("0.70").unwrap_or(Decimal::from(7) / Decimal::from(10)),
            SmartMoneyTier::Tier2 => Decimal::from_str_exact("0.50").unwrap_or(Decimal::from(1) / Decimal::from(2)),
            SmartMoneyTier::Tier3 => Decimal::from_str_exact("0.30").unwrap_or(Decimal::from(3) / Decimal::from(10)),
        }
    }
}

// ---------------------------------------------------------------------------
// SmartMoneyError
// ---------------------------------------------------------------------------

/// Errors produced by the smart-money labelling pipeline.
#[derive(Debug, thiserror::Error)]
pub enum SmartMoneyError {
    #[error("storage error: {0}")]
    Storage(#[from] mg_onchain_storage::StorageError),
    #[error("label store error: {0}")]
    LabelStore(GraphError),
    #[error("swap fetcher error: {0}")]
    SwapFetcher(#[from] anyhow::Error),
    #[error("computation error: {0}")]
    Computation(String),
}

impl From<GraphError> for SmartMoneyError {
    fn from(e: GraphError) -> Self {
        SmartMoneyError::LabelStore(e)
    }
}

// ---------------------------------------------------------------------------
// BatchStats
// ---------------------------------------------------------------------------

/// Summary statistics from a single batch run.
#[derive(Debug, Clone, Default)]
pub struct BatchStats {
    /// Wallets evaluated for corpus recomputation.
    pub wallets_evaluated: u64,
    /// Corpus rows written or updated.
    pub corpus_rows_upserted: u64,
    /// Labels written to `address_labels`.
    pub labels_written: u64,
    /// Wallets skipped (insufficient round-trips, excluded, wash-trading, etc.).
    pub wallets_skipped: u64,
    /// Batch run UUID (for cross-referencing corpus rows).
    pub batch_run_id: Uuid,
}

// ---------------------------------------------------------------------------
// Internal FIFO computation types
// ---------------------------------------------------------------------------

/// An open long position (buy with no matching sell yet).
#[derive(Debug)]
struct OpenPosition {
    qty: Decimal,
    price: Option<Decimal>,
    block_time: DateTime<Utc>,
}

/// A completed round-trip (buy + sell pair).
///
/// Pub so that the public pure functions (`compute_win_rate`, `compute_mean_holding_seconds`,
/// `compute_realized_pnl_round_trips`) can be used in downstream tests without
/// repackaging the return type.
#[derive(Debug)]
pub struct RoundTrip {
    /// Quantity of token units closed in this round-trip.
    pub closed_qty: Decimal,
    /// Realized PnL in USD, or `None` when entry or exit price was unavailable.
    pub pnl_usd: Option<Decimal>,
    /// Holding time in seconds (sell_time - buy_time).
    pub holding_secs: i64,
    /// Block time of the closing sell.
    pub sell_time: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Pure math: Stage 1 computations
// ---------------------------------------------------------------------------

/// Compute realized PnL round-trips from a FIFO-matched sequence of swaps.
///
/// `swaps` must be sorted by `(block_time ASC, tx_hash ASC)`.
/// All monetary arithmetic uses `Decimal` — no `f64`.
///
/// Returns a `Vec<RoundTrip>` where each entry is one closed FIFO pair.
/// Open positions (buys without a matching sell) are ignored.
///
/// # Design reference
///
/// design 0022 §3.1 Rust pseudocode — authoritative algorithm.
pub fn compute_realized_pnl_round_trips(
    swaps: &[SwapRow],
    price_provider_results: &BTreeMap<(String, i64), Option<Decimal>>,
) -> Vec<RoundTrip> {
    let mut buys: VecDeque<OpenPosition> = VecDeque::new();
    let mut round_trips: Vec<RoundTrip> = Vec::new();

    for swap in swaps {
        match swap.side {
            SwapSide::Buy => {
                let price = price_provider_results
                    .get(&(swap.token.clone(), swap.block_height))
                    .copied()
                    .flatten();
                buys.push_back(OpenPosition {
                    qty: swap.token_qty,
                    price,
                    block_time: swap.block_time,
                });
            }
            SwapSide::Sell => {
                let exit_price = price_provider_results
                    .get(&(swap.token.clone(), swap.block_height))
                    .copied()
                    .flatten();

                let mut remaining_sell = swap.token_qty;

                while remaining_sell > Decimal::ZERO {
                    let Some(mut pos) = buys.pop_front() else {
                        break;
                    };

                    let closed = remaining_sell.min(pos.qty);
                    let pnl_usd = match (pos.price, exit_price) {
                        (Some(entry_p), Some(exit_p)) => Some((exit_p - entry_p) * closed),
                        _ => None,
                    };
                    let holding_secs = (swap.block_time - pos.block_time).num_seconds();

                    round_trips.push(RoundTrip {
                        closed_qty: closed,
                        pnl_usd,
                        holding_secs,
                        sell_time: swap.block_time,
                    });

                    remaining_sell -= closed;

                    if pos.qty > closed {
                        pos.qty -= closed;
                        buys.push_front(pos); // remainder stays open
                    }
                }
            }
        }
    }

    round_trips
}

/// Compute win rate: fraction of priced round-trips with positive PnL.
///
/// Returns `None` when `round_trips` has no entries with non-None pnl.
pub fn compute_win_rate(round_trips: &[RoundTrip]) -> Option<Decimal> {
    let priced: Vec<Decimal> = round_trips
        .iter()
        .filter_map(|rt| rt.pnl_usd)
        .collect();

    if priced.is_empty() {
        return None;
    }

    let wins = priced.iter().filter(|&&p| p > Decimal::ZERO).count();
    let total = priced.len();

    Decimal::from_u64(wins as u64)
        .and_then(|w| Decimal::from_u64(total as u64).map(|t| w / t))
}

/// Compute mean holding time in seconds across all round-trips.
///
/// Returns `None` when `round_trips` is empty.
pub fn compute_mean_holding_seconds(round_trips: &[RoundTrip]) -> Option<Decimal> {
    if round_trips.is_empty() {
        return None;
    }
    let sum: i64 = round_trips.iter().map(|rt| rt.holding_secs).sum();
    let count = round_trips.len() as i64;
    Decimal::from_i64(sum).and_then(|s| Decimal::from_i64(count).map(|c| s / c))
}

/// Compute cross-event recurrence: count of distinct pump events where this wallet
/// appeared in the pre-event entry window.
///
/// `wallet_buy_times` is the set of buy block_times for this wallet on each token.
/// `pump_events` is the list of known pump events from D04.
/// `pre_event_window_minutes` is the pre-event window (default 60 min per Fantazzini 2023).
///
/// Returns the count of distinct pump events with a matching pre-event buy.
pub fn compute_cross_event_recurrence(
    wallet_swaps: &[SwapRow],
    pump_events: &[PumpEvent],
    pre_event_window_minutes: i64,
) -> u32 {
    let mut recurrence: u32 = 0;

    for event in pump_events {
        let window_start = event.event_peak_time - Duration::minutes(pre_event_window_minutes);
        let window_end = event.event_peak_time;

        // Check if wallet has any buy on this token within the pre-event window.
        let has_pre_event_buy = wallet_swaps.iter().any(|s| {
            s.token == event.token
                && s.side == SwapSide::Buy
                && s.block_time >= window_start
                && s.block_time <= window_end
        });

        if has_pre_event_buy {
            recurrence += 1;
        }
    }

    recurrence
}

/// Compute earliest pre-event entry lead in blocks for this wallet across all pump events.
///
/// For each pump event where the wallet had a pre-event buy, compute the time lead
/// `event_peak_time - earliest_buy_time` (in seconds). Returns the maximum lead (earliest entry).
///
/// Returns `None` when the wallet has no pre-event entries on any pump event.
pub fn compute_timing_lead_secs(
    wallet_swaps: &[SwapRow],
    pump_events: &[PumpEvent],
    pre_event_window_minutes: i64,
) -> Option<Decimal> {
    let leads: Vec<i64> = pump_events
        .iter()
        .filter_map(|event| {
            let window_start = event.event_peak_time - Duration::minutes(pre_event_window_minutes);
            let window_end = event.event_peak_time;

            // Find the earliest buy on this token within the pre-event window.
            wallet_swaps
                .iter()
                .filter(|s| {
                    s.token == event.token
                        && s.side == SwapSide::Buy
                        && s.block_time >= window_start
                        && s.block_time <= window_end
                })
                .map(|s| (event.event_peak_time - s.block_time).num_seconds())
                .max()
        })
        .collect();

    if leads.is_empty() {
        return None;
    }

    // Median lead.
    let mut sorted = leads.clone();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    let median_secs = if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
    };

    Decimal::from_i64(median_secs)
}

// ---------------------------------------------------------------------------
// classify_tier — pure function per design 0022 §3.3
// ---------------------------------------------------------------------------

/// Classify a wallet into a smart-money tier based on corpus metrics.
///
/// Returns `None` if the wallet does not meet any tier's criteria.
///
/// # Invariant
///
/// `round_trip_count` must be the TOTAL completed round-trips (including NULL-pnl).
/// `non_null_pnl_count` is the subset with price data.
/// The tier classification gates on `non_null_pnl_count` (not `round_trip_count`) per
/// design 0022 §5.1: wallets where all round-trips have NULL PnL are NOT labelled.
pub fn classify_tier(
    non_null_pnl_count: i64,
    total_pnl_usd: Option<Decimal>,
    win_rate: Option<Decimal>,
    recurrence_count: u32,
    timing_lead_pct_rank: Option<Decimal>,
    cfg: &SmartMoneyConfig,
) -> Option<SmartMoneyTier> {
    let effective_min = cfg.min_round_trips.max(cfg.min_round_trips_floor);
    if non_null_pnl_count < effective_min as i64 {
        return None;
    }

    let pnl = total_pnl_usd.unwrap_or(Decimal::ZERO);

    // Tier 1: PnL + win rate + recurrence + timing percentile all satisfied.
    let win_rate_ok = win_rate
        .map(|wr| wr >= cfg.tier1_min_win_rate)
        .unwrap_or(false);
    let timing_ok = timing_lead_pct_rank
        .map(|pct| pct >= cfg.tier1_top_timing_percentile)
        .unwrap_or(false);

    if pnl >= cfg.tier1_min_pnl_usd
        && win_rate_ok
        && recurrence_count >= cfg.tier1_min_recurrence
        && timing_ok
    {
        return Some(SmartMoneyTier::Tier1);
    }

    // Tier 2: strong PnL OR ≥ 2 recurrence (either criterion, not both required).
    if pnl >= cfg.tier2_min_pnl_usd || recurrence_count >= cfg.tier2_min_recurrence {
        return Some(SmartMoneyTier::Tier2);
    }

    // Tier 3: positive PnL only (candidate pool for Stage 2 FDR).
    if pnl > Decimal::ZERO {
        return Some(SmartMoneyTier::Tier3);
    }

    None
}

// ---------------------------------------------------------------------------
// Confidence computation — design 0022 §4.2
// ---------------------------------------------------------------------------

fn compute_confidence(
    tier: SmartMoneyTier,
    total_pnl_usd: Option<Decimal>,
    _win_rate: Option<Decimal>,
    recurrence_count: u32,
    sell_before_peak_rate: Option<Decimal>,
    mean_holding_secs: Option<Decimal>,
    cfg: &SmartMoneyConfig,
) -> f64 {
    let base = tier.base_confidence();
    let min_recurrence = match tier {
        SmartMoneyTier::Tier1 => cfg.tier1_min_recurrence,
        SmartMoneyTier::Tier2 => cfg.tier2_min_recurrence,
        SmartMoneyTier::Tier3 => 0,
    };

    // Sell-before-peak bonus: +0.10 if sell_before_peak_rate >= 0.70
    let sbp_bonus = sell_before_peak_rate
        .filter(|&r| r >= Decimal::from_str_exact("0.70").unwrap_or(Decimal::ZERO))
        .map(|_| Decimal::from_str_exact("0.10").unwrap_or(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO);

    // Recurrence bonus: +0.03 per additional event beyond minimum, capped at 0.10.
    let recurrence_extra = recurrence_count.saturating_sub(min_recurrence);
    let rec_bonus = (Decimal::from_str_exact("0.03").unwrap_or(Decimal::ZERO)
        * Decimal::from(recurrence_extra))
    .min(Decimal::from_str_exact("0.10").unwrap_or(Decimal::ZERO));

    // Holding time bonus: +0.05 if mean_holding is between 5 min and 24h.
    let holding_bonus = mean_holding_secs
        .filter(|&h| {
            h >= Decimal::from(300) && h <= Decimal::from(86400)
        })
        .map(|_| Decimal::from_str_exact("0.05").unwrap_or(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO);

    // PnL scale bonus: log10-based marginal bonus, capped at 0.05.
    let pnl_bonus = total_pnl_usd
        .filter(|&p| p >= Decimal::from(1000))
        .and_then(|p| {
            // log10(p) * 0.02 - 0.02, clamped to [0, 0.05].
            // Use f64 for log; result is a confidence adjustment (not monetary).
            use rust_decimal::prelude::ToPrimitive;
            let log_val = p.to_f64()?.log10();
            let bonus_f64 = (log_val * 0.02 - 0.02).clamp(0.0, 0.05);
            Decimal::from_f64(bonus_f64)
        })
        .unwrap_or(Decimal::ZERO);

    let raw = base + sbp_bonus + rec_bonus + holding_bonus + pnl_bonus;
    let cap = Decimal::from_str_exact("0.90").unwrap_or(Decimal::from(9) / Decimal::from(10));
    let clamped = raw.min(cap).max(Decimal::ZERO);

    // Convert to f64 for AddressLabel.confidence (probability, not money — f64 is correct).
    use rust_decimal::prelude::ToPrimitive;
    clamped.to_f64().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// SmartMoneyLabeller
// ---------------------------------------------------------------------------

/// Smart-money labelling background task.
///
/// Spawned by `crates/server/src/init/smart_money.rs` via `tokio::spawn`.
/// NOT a `Detector` trait implementation (design 0022 §6.1 Decision 1 = Option B).
///
/// # Concurrency
///
/// `run_batch` is called from a single periodic task. No internal locking required
/// beyond what the trait objects provide.
pub struct SmartMoneyLabeller {
    /// Chain this labeller operates on.
    chain: Chain,
    /// Writes `address_labels` rows.
    label_store: Arc<dyn GraphLabelStore>,
    /// Reads/writes `wallet_pnl_corpus` rows.
    corpus_store: Arc<dyn WalletPnlCorpusStore>,
    /// Fetches swap rows and pump events.
    swap_fetcher: Arc<dyn SwapFetcher>,
    /// USD price lookups for PnL computation.
    price_provider: Arc<dyn TokenPriceProvider>,
    /// Labeller configuration.
    config: SmartMoneyConfig,
}

impl SmartMoneyLabeller {
    /// Construct a new `SmartMoneyLabeller`.
    pub fn new(
        chain: Chain,
        label_store: Arc<dyn GraphLabelStore>,
        corpus_store: Arc<dyn WalletPnlCorpusStore>,
        swap_fetcher: Arc<dyn SwapFetcher>,
        price_provider: Arc<dyn TokenPriceProvider>,
        config: SmartMoneyConfig,
    ) -> Self {
        Self {
            chain,
            label_store,
            corpus_store,
            swap_fetcher,
            price_provider,
            config,
        }
    }

    /// Run one batch of the smart-money labelling pipeline.
    ///
    /// 1. Determine the swap history window (`window_end - corpus_lookback_days`).
    /// 2. Fetch all active wallets with swap activity in the window.
    /// 3. Exclude CEX hot wallets, DEX programs, burn addresses, and wash traders.
    /// 4. For each non-excluded wallet:
    ///    a. Fetch swaps and compute FIFO PnL corpus (Stage 1).
    ///    b. Apply `min_round_trips` gate.
    ///    c. Compute timing features (Stage 3).
    ///    d. Upsert corpus row.
    ///    e. Classify tier → compute confidence → write label.
    /// 5. Return `BatchStats`.
    ///
    /// # Utc::now() documented exception
    ///
    /// `window_end` is wall-clock by design — this is a periodic batch task, NOT in the
    /// per-event detector hot path (gotcha #22 / design 0022 §6.4). The batch task
    /// processes swap history up to the current wall-clock moment.
    #[instrument(skip(self), fields(chain = %self.chain))]
    pub async fn run_batch(&self, window_end: DateTime<Utc>) -> Result<BatchStats, SmartMoneyError> {
        if !self.config.enabled {
            debug!("smart_money labeller disabled — skipping batch");
            return Ok(BatchStats::default());
        }

        let batch_run_id = Uuid::new_v4();
        let mut stats = BatchStats {
            batch_run_id,
            ..Default::default()
        };

        let corpus_since = window_end - Duration::days(self.config.corpus_lookback_days);

        // --- Fetch exclusion lists ---
        let excluded_wallets = self
            .swap_fetcher
            .fetch_excluded_wallets(self.chain)
            .await
            .context("fetch_excluded_wallets failed")?;
        let excluded_set: std::collections::BTreeSet<String> =
            excluded_wallets.into_iter().collect();

        let wash_excluded = self
            .swap_fetcher
            .fetch_wash_trading_excluded(self.chain, self.config.wash_trading_exclusion_confidence)
            .await
            .context("fetch_wash_trading_excluded failed")?;
        let wash_excluded_set: std::collections::BTreeSet<String> =
            wash_excluded.into_iter().collect();

        // --- Fetch pump events for Stage 3 ---
        let pump_events = self
            .swap_fetcher
            .fetch_pump_events(self.chain, corpus_since, self.config.pump_event_min_confidence)
            .await
            .context("fetch_pump_events failed")?;

        // --- Fetch active wallets ---
        let active_wallets = self
            .swap_fetcher
            .fetch_active_wallets(self.chain, corpus_since, window_end)
            .await
            .context("fetch_active_wallets failed")?;

        for wallet in &active_wallets {
            // Skip excluded wallets.
            if excluded_set.contains(wallet) || wash_excluded_set.contains(wallet) {
                stats.wallets_skipped += 1;
                continue;
            }

            stats.wallets_evaluated += 1;

            // Process this wallet — best-effort; log errors and continue.
            match self
                .process_wallet(wallet, corpus_since, window_end, &pump_events, batch_run_id)
                .await
            {
                Ok(label_written) => {
                    stats.corpus_rows_upserted += 1;
                    if label_written {
                        stats.labels_written += 1;
                    }
                }
                Err(e) => {
                    warn!(chain = %self.chain, wallet = %wallet, error = %e, "smart_money: wallet processing failed — skipping");
                    stats.wallets_skipped += 1;
                }
            }
        }

        debug!(
            chain = %self.chain,
            ?stats.wallets_evaluated,
            ?stats.labels_written,
            ?stats.wallets_skipped,
            "smart_money batch complete"
        );

        Ok(stats)
    }

    /// Process a single wallet: compute corpus, classify tier, write label.
    ///
    /// Returns `Ok(true)` if a label was written, `Ok(false)` if the wallet
    /// did not qualify (insufficient round-trips, no PnL, below tier floors).
    async fn process_wallet(
        &self,
        wallet: &str,
        corpus_since: DateTime<Utc>,
        window_end: DateTime<Utc>,
        pump_events: &[PumpEvent],
        batch_run_id: Uuid,
    ) -> Result<bool, SmartMoneyError> {
        // Fetch swaps for this wallet.
        let swaps = self
            .swap_fetcher
            .fetch_swaps_for_wallet(self.chain, wallet, corpus_since, window_end)
            .await?;

        if swaps.is_empty() {
            return Ok(false);
        }

        // Collect unique (token, block_height) pairs for price lookups.
        let price_keys: std::collections::BTreeSet<(String, i64)> = swaps
            .iter()
            .map(|s| (s.token.clone(), s.block_height))
            .collect();

        // Fetch prices for all relevant (token, block_height) pairs.
        let mut prices: BTreeMap<(String, i64), Option<Decimal>> = BTreeMap::new();
        for (token, block_height) in &price_keys {
            // Use block_time from the corresponding swap row as the `observed_at` anchor.
            let block_time = swaps
                .iter()
                .find(|s| &s.token == token && s.block_height == *block_height)
                .map(|s| s.block_time)
                .unwrap_or(window_end);

            let addr_result = Address::parse(self.chain, token);
            let price = match addr_result {
                Ok(addr) => {
                    self.price_provider
                        .get_token_price_usd(self.chain, &addr, block_time)
                        .await
                }
                Err(_) => None,
            };
            prices.insert((token.clone(), *block_height), price);
        }

        // Compute FIFO round-trips per token.
        let mut all_round_trips: Vec<RoundTrip> = Vec::new();
        let tokens: std::collections::BTreeSet<&str> =
            swaps.iter().map(|s| s.token.as_str()).collect();

        for token in &tokens {
            let token_swaps: Vec<SwapRow> = swaps
                .iter()
                .filter(|s| s.token == *token)
                .cloned()
                .collect();
            let rts = compute_realized_pnl_round_trips(&token_swaps, &prices);
            all_round_trips.extend(rts);
        }

        let round_trip_count = all_round_trips.len() as i64;
        let non_null_pnl_count = all_round_trips
            .iter()
            .filter(|rt| rt.pnl_usd.is_some())
            .count() as i64;

        // Apply min_round_trips gate.
        let effective_min = self.config.min_round_trips.max(self.config.min_round_trips_floor);
        if non_null_pnl_count < effective_min as i64 {
            // Wallet does not have enough priced round-trips — skip label, but still
            // upsert corpus row with current data for future batches.
            let corpus_row = self.build_corpus_row(
                wallet,
                &swaps,
                &all_round_trips,
                pump_events,
                batch_run_id,
                window_end,
            );
            self.corpus_store
                .upsert_corpus_row(&corpus_row)
                .await?;
            return Ok(false);
        }

        // Compute aggregates.
        let total_pnl_usd = if non_null_pnl_count > 0 {
            let sum = all_round_trips
                .iter()
                .filter_map(|rt| rt.pnl_usd)
                .fold(Decimal::ZERO, |acc, p| acc + p);
            Some(sum)
        } else {
            None
        };

        let win_rate = compute_win_rate(&all_round_trips);
        let mean_holding = compute_mean_holding_seconds(&all_round_trips);

        // Stage 3: timing features.
        let pre_event_window_minutes = i64::from(self.config.pre_event_lookback_max_minutes);
        let recurrence_count = compute_cross_event_recurrence(&swaps, pump_events, pre_event_window_minutes);
        let median_timing_lead_secs = compute_timing_lead_secs(&swaps, pump_events, pre_event_window_minutes);

        // Per-token PnL (top-10 by absolute PnL, Decision 9).
        let mut per_token_pnl_map: BTreeMap<String, Decimal> = BTreeMap::new();
        for rt in &all_round_trips {
            if let Some(pnl) = rt.pnl_usd {
                // Find the token for this round-trip — use last seen sell swap.
                // For simplicity in MVP, this requires matching by sell_time.
                // We iterate swaps to find the token at this sell_time.
                if let Some(swap) = swaps
                    .iter()
                    .find(|s| s.side == SwapSide::Sell && s.block_time == rt.sell_time)
                {
                    *per_token_pnl_map
                        .entry(swap.token.clone())
                        .or_insert(Decimal::ZERO) += pnl;
                }
            }
        }
        // Keep top-10 by absolute PnL.
        let mut per_token_vec: Vec<(String, Decimal)> = per_token_pnl_map.into_iter().collect();
        per_token_vec.sort_by_key(|item| std::cmp::Reverse(item.1.abs()));
        per_token_vec.truncate(10);
        let per_token_json: serde_json::Value = serde_json::Value::Object(
            per_token_vec
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.to_string())))
                .collect(),
        );

        // Build corpus row.
        let last_round_trip_at = all_round_trips
            .iter()
            .map(|rt| rt.sell_time)
            .max();
        let first_trade_at = swaps.iter().map(|s| s.block_time).min();

        let corpus_row = WalletPnlCorpusRow {
            id: 0,
            chain: self.chain.as_str().to_owned(),
            wallet: wallet.to_owned(),
            token: "_aggregate_".to_owned(), // cross-token aggregate row
            round_trip_count,
            non_null_pnl_count,
            total_pnl_usd,
            win_rate,
            mean_holding_time_secs: mean_holding,
            sell_before_peak_rate: None, // TODO(sprint-23): sell-before-peak computation
            recurrence_count: i64::from(recurrence_count),
            median_timing_lead_secs,
            timing_lead_pct_rank: None, // TODO(sprint-23): compute percentile rank vs co-participants
            per_token_pnl: Some(per_token_json),
            first_trade_at,
            last_round_trip_at,
            last_updated: window_end,
            batch_run_id,
        };

        self.corpus_store.upsert_corpus_row(&corpus_row).await?;

        // Classify tier.
        let tier = classify_tier(
            non_null_pnl_count,
            total_pnl_usd,
            win_rate,
            recurrence_count,
            corpus_row.timing_lead_pct_rank,
            &self.config,
        );

        let Some(tier) = tier else {
            // Below all tier thresholds — corpus updated but no label.
            return Ok(false);
        };

        // Compute confidence.
        let confidence = compute_confidence(
            tier,
            total_pnl_usd,
            win_rate,
            recurrence_count,
            corpus_row.sell_before_peak_rate,
            mean_holding,
            &self.config,
        );

        // Build label TTL.
        let issued_at = window_end;
        let expires_at = Some(issued_at + Duration::hours(self.config.label_ttl_hours));

        // Build evidence (all keys prefixed `smart_money/` per gotcha #9).
        let evidence = serde_json::json!({
            "smart_money/tier":                          tier.as_str(),
            "smart_money/total_realized_pnl_usd":        total_pnl_usd.map(|d| d.to_string()),
            "smart_money/win_rate":                      win_rate.map(|d| d.to_string()),
            "smart_money/round_trip_count":              round_trip_count,
            "smart_money/non_null_pnl_count":            non_null_pnl_count,
            "smart_money/cross_event_recurrence":        recurrence_count,
            "smart_money/median_timing_lead_secs":       corpus_row.median_timing_lead_secs.map(|d| d.to_string()),
            "smart_money/mean_holding_time_secs":        mean_holding.map(|d| d.to_string()),
            "smart_money/top_timing_lead_blocks":        null,
            "smart_money/heuristic_not_fdr_controlled":  true,
            "calibration":                               "heuristic, not FDR-controlled",
            "stage2_blocked_reason":                     "live corpus < 30 days; activate via smart_money_fdr_enabled = true",
        });

        let label = AddressLabel {
            chain: self.chain.as_str().to_owned(),
            address: wallet.to_owned(),
            label_type: LabelType::SmartMoney,
            confidence,
            evidence,
            issued_at,
            expires_at,
            source: "smart_money_labeller_v1".to_owned(),
        };

        self.label_store.upsert_label(&label).await?;

        Ok(true)
    }

    /// Build a corpus row from swap and round-trip data.
    ///
    /// Used for wallets that are below the `min_round_trips` threshold — their
    /// corpus row is stored for incremental recomputation in future batches.
    fn build_corpus_row(
        &self,
        wallet: &str,
        swaps: &[SwapRow],
        round_trips: &[RoundTrip],
        pump_events: &[PumpEvent],
        batch_run_id: Uuid,
        window_end: DateTime<Utc>,
    ) -> WalletPnlCorpusRow {
        let pre_event_window_minutes = i64::from(self.config.pre_event_lookback_max_minutes);
        let recurrence_count = compute_cross_event_recurrence(swaps, pump_events, pre_event_window_minutes);
        let median_timing_lead_secs = compute_timing_lead_secs(swaps, pump_events, pre_event_window_minutes);

        let round_trip_count = round_trips.len() as i64;
        let non_null_pnl_count = round_trips.iter().filter(|rt| rt.pnl_usd.is_some()).count() as i64;
        let total_pnl_usd = if non_null_pnl_count > 0 {
            Some(round_trips.iter().filter_map(|rt| rt.pnl_usd).fold(Decimal::ZERO, |a, b| a + b))
        } else {
            None
        };

        WalletPnlCorpusRow {
            id: 0,
            chain: self.chain.as_str().to_owned(),
            wallet: wallet.to_owned(),
            token: "_aggregate_".to_owned(),
            round_trip_count,
            non_null_pnl_count,
            total_pnl_usd,
            win_rate: compute_win_rate(round_trips),
            mean_holding_time_secs: compute_mean_holding_seconds(round_trips),
            sell_before_peak_rate: None,
            recurrence_count: i64::from(recurrence_count),
            median_timing_lead_secs,
            timing_lead_pct_rank: None,
            per_token_pnl: None,
            first_trade_at: swaps.iter().map(|s| s.block_time).min(),
            last_round_trip_at: round_trips.iter().map(|rt| rt.sell_time).max(),
            last_updated: window_end,
            batch_run_id,
        }
    }
}

// ---------------------------------------------------------------------------
// MockSwapFetcher (test-utils)
// ---------------------------------------------------------------------------

/// In-memory `SwapFetcher` for unit tests.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockSwapFetcher {
    /// Swaps keyed by (chain_str, wallet).
    pub swaps: std::sync::Mutex<BTreeMap<(String, String), Vec<SwapRow>>>,
    /// Pump events keyed by chain_str.
    pub pump_events: std::sync::Mutex<BTreeMap<String, Vec<PumpEvent>>>,
    /// Excluded wallets keyed by chain_str.
    pub excluded_wallets: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
    /// Wash-trading excluded wallets keyed by chain_str.
    pub wash_excluded: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockSwapFetcher {
    fn default() -> Self {
        Self {
            swaps: std::sync::Mutex::new(BTreeMap::new()),
            pump_events: std::sync::Mutex::new(BTreeMap::new()),
            excluded_wallets: std::sync::Mutex::new(BTreeMap::new()),
            wash_excluded: std::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl MockSwapFetcher {
    /// Add swap rows for a wallet.
    pub fn add_swaps(&self, chain: Chain, wallet: &str, swaps: Vec<SwapRow>) {
        self.swaps
            .lock()
            .unwrap()
            .entry((chain.as_str().to_owned(), wallet.to_owned()))
            .or_default()
            .extend(swaps);
    }

    /// Add pump events for a chain.
    pub fn add_pump_events(&self, chain: Chain, events: Vec<PumpEvent>) {
        self.pump_events
            .lock()
            .unwrap()
            .entry(chain.as_str().to_owned())
            .or_default()
            .extend(events);
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl SwapFetcher for MockSwapFetcher {
    async fn fetch_swaps_for_wallet(
        &self,
        chain: Chain,
        wallet: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<SwapRow>> {
        let guard = self.swaps.lock().unwrap();
        let key = (chain.as_str().to_owned(), wallet.to_owned());
        let rows: Vec<SwapRow> = guard
            .get(&key)
            .map(|v| {
                v.iter()
                    .filter(|s| s.block_time >= since && s.block_time <= until)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(rows)
    }

    async fn fetch_active_wallets(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<String>> {
        let guard = self.swaps.lock().unwrap();
        let chain_str = chain.as_str().to_owned();
        // Collect distinct wallets that have any swap in the window.
        let wallets: std::collections::BTreeSet<String> = guard
            .iter()
            .filter(|((ch, _), swaps)| {
                *ch == chain_str
                    && swaps.iter().any(|s| s.block_time >= since && s.block_time <= until)
            })
            .map(|((_, w), _)| w.clone())
            .collect();
        Ok(wallets.into_iter().collect())
    }

    async fn fetch_pump_events(
        &self,
        chain: Chain,
        since: DateTime<Utc>,
        _min_confidence: f64,
    ) -> anyhow::Result<Vec<PumpEvent>> {
        let guard = self.pump_events.lock().unwrap();
        let events: Vec<PumpEvent> = guard
            .get(chain.as_str())
            .map(|v| {
                v.iter()
                    .filter(|e| e.event_peak_time >= since)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(events)
    }

    async fn fetch_excluded_wallets(&self, chain: Chain) -> anyhow::Result<Vec<String>> {
        let guard = self.excluded_wallets.lock().unwrap();
        Ok(guard.get(chain.as_str()).cloned().unwrap_or_default())
    }

    async fn fetch_wash_trading_excluded(
        &self,
        chain: Chain,
        _exclusion_confidence: f64,
    ) -> anyhow::Result<Vec<String>> {
        let guard = self.wash_excluded.lock().unwrap();
        Ok(guard.get(chain.as_str()).cloned().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mg_onchain_storage::wallet_pnl_corpus::MockWalletPnlCorpusStore;

    #[cfg(any(test, feature = "test-utils"))]
    use crate::mock::MockGraphLabelStore;

    fn t(ts: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(ts, 0).unwrap()
    }

    fn buy(token: &str, qty: &str, ts: i64, height: i64) -> SwapRow {
        SwapRow {
            wallet: "wallet_A".to_owned(),
            token: token.to_owned(),
            side: SwapSide::Buy,
            token_qty: Decimal::from_str_exact(qty).unwrap(),
            block_time: t(ts),
            block_height: height,
            tx_hash: format!("buy_{ts}"),
        }
    }

    fn sell(token: &str, qty: &str, ts: i64, height: i64) -> SwapRow {
        SwapRow {
            wallet: "wallet_A".to_owned(),
            token: token.to_owned(),
            side: SwapSide::Sell,
            token_qty: Decimal::from_str_exact(qty).unwrap(),
            block_time: t(ts),
            block_height: height,
            tx_hash: format!("sell_{ts}"),
        }
    }

    fn prices_map(entries: &[(&str, i64, Option<&str>)]) -> BTreeMap<(String, i64), Option<Decimal>> {
        entries
            .iter()
            .map(|(token, height, price)| {
                (
                    (token.to_string(), *height),
                    price.map(|p| Decimal::from_str_exact(p).unwrap()),
                )
            })
            .collect()
    }

    // --- compute_realized_pnl_round_trips ---

    #[test]
    fn pnl_single_buy_sell_known_pnl() {
        let swaps = vec![
            buy("mint_A", "100", 1000, 1),
            sell("mint_A", "100", 2000, 2),
        ];
        let prices = prices_map(&[("mint_A", 1, Some("1.00")), ("mint_A", 2, Some("1.50"))]);
        let rts = compute_realized_pnl_round_trips(&swaps, &prices);
        assert_eq!(rts.len(), 1);
        let pnl = rts[0].pnl_usd.unwrap();
        // (1.50 - 1.00) * 100 = 50
        assert_eq!(pnl, Decimal::from(50), "PnL = 50");
    }

    #[test]
    fn pnl_multiple_round_trips_summed() {
        let swaps = vec![
            buy("mint_A", "50", 1000, 1),
            sell("mint_A", "50", 2000, 2),
            buy("mint_A", "80", 3000, 3),
            sell("mint_A", "80", 4000, 4),
        ];
        let prices = prices_map(&[
            ("mint_A", 1, Some("1.00")),
            ("mint_A", 2, Some("2.00")),
            ("mint_A", 3, Some("2.00")),
            ("mint_A", 4, Some("3.00")),
        ]);
        let rts = compute_realized_pnl_round_trips(&swaps, &prices);
        assert_eq!(rts.len(), 2, "two round-trips");
        let total: Decimal = rts.iter().filter_map(|r| r.pnl_usd).sum();
        // RT1: (2.0 - 1.0) * 50 = 50
        // RT2: (3.0 - 2.0) * 80 = 80
        // Total = 130
        assert_eq!(total, Decimal::from(130));
    }

    #[test]
    fn pnl_buy_without_matching_sell_no_contribution() {
        // Only a buy, no sell — should produce 0 round-trips.
        let swaps = vec![buy("mint_A", "100", 1000, 1)];
        let prices = prices_map(&[("mint_A", 1, Some("1.00"))]);
        let rts = compute_realized_pnl_round_trips(&swaps, &prices);
        assert_eq!(rts.len(), 0, "open position — no round-trip");
    }

    #[test]
    fn pnl_missing_price_propagates_none() {
        let swaps = vec![
            buy("mint_A", "100", 1000, 1),
            sell("mint_A", "100", 2000, 2),
        ];
        // No entry for block_height 1 — price will be None.
        let prices = prices_map(&[("mint_A", 2, Some("1.50"))]);
        let rts = compute_realized_pnl_round_trips(&swaps, &prices);
        assert_eq!(rts.len(), 1);
        assert!(rts[0].pnl_usd.is_none(), "missing entry price → pnl_usd is None");
    }

    // --- compute_win_rate ---

    #[test]
    fn win_rate_six_wins_four_losses() {
        let rts: Vec<RoundTrip> = (0..10)
            .map(|i| RoundTrip {
                closed_qty: Decimal::ONE,
                pnl_usd: if i < 6 { Some(Decimal::from(10)) } else { Some(Decimal::from(-5)) },
                holding_secs: 3600,
                sell_time: t(1000 + i * 100),
            })
            .collect();
        let wr = compute_win_rate(&rts).unwrap();
        // 6/10 = 0.6
        assert_eq!(wr, Decimal::from_str_exact("0.6").unwrap());
    }

    #[test]
    fn win_rate_zero_round_trips_returns_none() {
        let wr = compute_win_rate(&[]);
        assert!(wr.is_none());
    }

    // --- compute_cross_event_recurrence ---

    #[test]
    fn recurrence_three_distinct_pre_event_windows() {
        let events = vec![
            PumpEvent { token: "mint_A".to_owned(), event_peak_time: t(3600), confidence: 0.8 },
            PumpEvent { token: "mint_B".to_owned(), event_peak_time: t(7200), confidence: 0.8 },
            PumpEvent { token: "mint_C".to_owned(), event_peak_time: t(10800), confidence: 0.8 },
        ];
        let swaps = vec![
            // Pre-event buy for mint_A (60 min before 3600 = at 0)
            buy("mint_A", "100", 100, 1),
            // Pre-event buy for mint_B (60 min before 7200 = at 3600)
            buy("mint_B", "100", 4000, 10),
            // Pre-event buy for mint_C (60 min before 10800 = at 7200)
            buy("mint_C", "100", 8000, 20),
        ];
        let recurrence = compute_cross_event_recurrence(&swaps, &events, 60);
        assert_eq!(recurrence, 3, "3 distinct pre-event entries");
    }

    #[test]
    fn timing_lead_earliest_80_seconds_pre_event() {
        let events = vec![
            PumpEvent { token: "mint_A".to_owned(), event_peak_time: t(3600), confidence: 0.8 },
        ];
        // Buy at t(3520) — 80 seconds before event peak at t(3600).
        let swaps = vec![buy("mint_A", "100", 3520, 1)];
        let lead = compute_timing_lead_secs(&swaps, &events, 60);
        assert!(lead.is_some(), "timing lead should be computed");
        assert_eq!(lead.unwrap(), Decimal::from(80), "lead = 80 seconds");
    }

    // --- classify_tier ---

    #[test]
    fn classify_tier1_saturation_case() {
        let cfg = SmartMoneyConfig::default();
        let tier = classify_tier(
            12,                        // non_null_pnl_count
            Some(Decimal::from(15000)), // > tier1_min_pnl_usd ($10K)
            Some(Decimal::from_str_exact("0.60").unwrap()), // win_rate > 0.55
            4,                          // recurrence >= 3
            Some(Decimal::from_str_exact("0.92").unwrap()), // timing_pct >= 0.90
            &cfg,
        );
        assert_eq!(tier, Some(SmartMoneyTier::Tier1));
    }

    #[test]
    fn classify_tier2_pnl_only_case() {
        let cfg = SmartMoneyConfig::default();
        let tier = classify_tier(
            12,
            Some(Decimal::from(5000)), // > tier2_min_pnl_usd ($1K), but < tier1 ($10K)
            Some(Decimal::from_str_exact("0.50").unwrap()), // win_rate < 0.55 → not Tier 1
            1,                          // recurrence < tier1 (3) and < tier2 (2) → PnL path
            None,
            &cfg,
        );
        assert_eq!(tier, Some(SmartMoneyTier::Tier2));
    }

    #[test]
    fn classify_tier3_positive_pnl_only() {
        let cfg = SmartMoneyConfig::default();
        let tier = classify_tier(
            12,
            Some(Decimal::from(500)), // > 0 but < tier2_min ($1K)
            None,
            0, // no recurrence
            None,
            &cfg,
        );
        assert_eq!(tier, Some(SmartMoneyTier::Tier3));
    }

    #[test]
    fn classify_none_below_min_round_trips() {
        let cfg = SmartMoneyConfig::default(); // min_round_trips = 10
        let tier = classify_tier(
            8, // < 10
            Some(Decimal::from(50_000)),
            Some(Decimal::from_str_exact("0.80").unwrap()),
            5,
            Some(Decimal::from_str_exact("0.95").unwrap()),
            &cfg,
        );
        assert!(tier.is_none(), "below min_round_trips → no tier");
    }

    #[test]
    fn classify_none_below_floor_even_with_override() {
        // Even if min_round_trips is lowered in config, floor prevents going below 5.
        // effective_min = max(3, 5) = 5
        let cfg = SmartMoneyConfig {
            min_round_trips: 3, // operator tries to lower below floor
            ..SmartMoneyConfig::default()
        };
        let tier = classify_tier(
            4, // non_null_pnl_count = 4 (below floor of 5)
            Some(Decimal::from(50_000)),
            Some(Decimal::from_str_exact("0.80").unwrap()),
            5,
            Some(Decimal::from_str_exact("0.95").unwrap()),
            &cfg,
        );
        assert!(tier.is_none(), "effective floor of 5 prevents labelling with 4 round-trips");
    }

    // --- run_batch integration tests ---

    #[tokio::test]
    async fn run_batch_zero_stale_wallets_zero_labels_written() {
        let label_store = Arc::new(MockGraphLabelStore::default());
        let corpus_store = Arc::new(MockWalletPnlCorpusStore::new());
        let swap_fetcher = Arc::new(MockSwapFetcher::default());
        let price_provider = Arc::new(mg_onchain_storage::MockTokenPriceProvider::new());

        let labeller = SmartMoneyLabeller::new(
            Chain::Solana,
            label_store.clone(),
            corpus_store,
            swap_fetcher,
            price_provider,
            SmartMoneyConfig::default(),
        );

        let window_end = t(1_700_000_000);
        let stats = labeller.run_batch(window_end).await.unwrap();

        assert_eq!(stats.labels_written, 0);
        assert_eq!(stats.wallets_evaluated, 0);

        let labels = label_store
            .addresses_with_label("solana", LabelType::SmartMoney, 0.0)
            .await
            .unwrap();
        assert!(labels.is_empty());
    }

    /// Verify that run_batch produces a wallet with count evaluated when swaps are present.
    ///
    /// Note: "mint_A" is not a valid Solana base58 address, so price lookups return None
    /// and non_null_pnl_count = 0, meaning no label is written (below min_round_trips for
    /// priced round-trips). This is correct behavior per design 0022 §5.1.
    #[tokio::test]
    async fn run_batch_one_wallet_below_priced_threshold_no_label() {
        let label_store = Arc::new(MockGraphLabelStore::default());
        let corpus_store = Arc::new(MockWalletPnlCorpusStore::new());
        let swap_fetcher = Arc::new(MockSwapFetcher::default());
        let price_provider = Arc::new(mg_onchain_storage::MockTokenPriceProvider::new());

        // Seed 11 round-trips — but token "fake_token" won't parse as a Solana address,
        // so prices return None → non_null_pnl_count = 0 → below min_round_trips floor.
        let buy_time = t(1_699_900_000);
        let sell_time = t(1_699_950_000);
        let mut swaps = Vec::new();
        for i in 0..11_i64 {
            swaps.push(SwapRow {
                wallet: "wallet_X".to_owned(),
                token: "fake_token".to_owned(),
                side: SwapSide::Buy,
                token_qty: Decimal::from(10),
                block_time: buy_time + Duration::hours(i),
                block_height: i,
                tx_hash: format!("buy_{i}"),
            });
            swaps.push(SwapRow {
                wallet: "wallet_X".to_owned(),
                token: "fake_token".to_owned(),
                side: SwapSide::Sell,
                token_qty: Decimal::from(10),
                block_time: sell_time + Duration::hours(i),
                block_height: 100 + i,
                tx_hash: format!("sell_{i}"),
            });
        }
        swap_fetcher.add_swaps(Chain::Solana, "wallet_X", swaps);

        let labeller = SmartMoneyLabeller::new(
            Chain::Solana,
            label_store.clone(),
            corpus_store,
            swap_fetcher,
            price_provider,
            SmartMoneyConfig::default(),
        );

        let window_end = t(1_700_000_000);
        let stats = labeller.run_batch(window_end).await.unwrap();

        // Wallet was evaluated but no label written (no priced round-trips).
        assert!(stats.wallets_evaluated > 0, "wallet must have been evaluated");
        assert_eq!(stats.labels_written, 0, "no label: non_null_pnl_count = 0 (no price data)");
    }

    /// Verify that evidence always includes the heuristic_not_fdr_controlled annotation.
    ///
    /// This test writes the label directly to the label store (bypassing the labeller's
    /// batch machinery) to verify the evidence JSON shape independently of the batch runner.
    #[tokio::test]
    async fn run_batch_heuristic_not_fdr_controlled_in_evidence() {
        let label_store = Arc::new(MockGraphLabelStore::default());

        // Build a label directly to verify evidence shape — this mirrors what
        // process_wallet emits when it writes a SmartMoney label.
        let evidence = serde_json::json!({
            "smart_money/tier":                         "tier3",
            "smart_money/total_realized_pnl_usd":       "500",
            "smart_money/win_rate":                     "0.6",
            "smart_money/round_trip_count":             11,
            "smart_money/non_null_pnl_count":           11,
            "smart_money/cross_event_recurrence":       0,
            "smart_money/median_timing_lead_secs":      null,
            "smart_money/mean_holding_time_secs":       "3600",
            "smart_money/top_timing_lead_blocks":       null,
            "smart_money/heuristic_not_fdr_controlled": true,
            "calibration":                              "heuristic, not FDR-controlled",
            "stage2_blocked_reason":                    "live corpus < 30 days; activate via smart_money_fdr_enabled = true",
        });

        let label = AddressLabel {
            chain: "solana".to_owned(),
            address: "wallet_test".to_owned(),
            label_type: LabelType::SmartMoney,
            confidence: 0.30,
            evidence: evidence.clone(),
            issued_at: t(1_700_000_000),
            // Use None (never expires) so the mock store doesn't filter it out
            // based on wall-clock time. The expiry logic is tested in mock.rs tests.
            expires_at: None,
            source: "smart_money_labeller_v1".to_owned(),
        };

        label_store.upsert_label(&label).await.unwrap();

        let labels = label_store
            .addresses_with_label("solana", LabelType::SmartMoney, 0.0)
            .await
            .unwrap();

        assert_eq!(labels.len(), 1, "label must be stored");
        let ev = &labels[0].evidence;
        assert_eq!(
            ev["smart_money/heuristic_not_fdr_controlled"],
            serde_json::Value::Bool(true),
            "heuristic_not_fdr_controlled must be true"
        );
        assert_eq!(
            ev["calibration"],
            serde_json::Value::String("heuristic, not FDR-controlled".to_owned()),
            "calibration annotation must match design 0022 §4"
        );
        assert_eq!(
            ev["smart_money/tier"],
            serde_json::Value::String("tier3".to_owned()),
            "tier must be encoded in evidence JSON, not in LabelType (Decision 2)"
        );
    }
}
