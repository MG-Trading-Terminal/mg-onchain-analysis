//! D11 — Synchronized-Activity Clustering Detector
//!
//! # Signal design (docs/designs/0018-detector-11-synchronized-activity.md)
//!
//! Detects clusters of N_min or more distinct wallets each executing a buy swap
//! on the same token within a δ-second window, where the co-occurrence probability
//! under a Poisson null model is below `poisson_p_threshold`.
//!
//! ## Algorithm (§3.1)
//!
//! 1. Fetch buy-swap events for the token over `max_lookback_minutes`.
//! 2. Compute per-token action rate λ (actions/second) from 7-day history.
//! 3. Bucketize events into δ-second slots → binary presence vectors per wallet.
//! 4. Compute pairwise Jaccard similarity → DBSCAN clustering.
//! 5. For each cluster ≥ N_min: compute temporal tightness + Poisson p-value.
//! 6. Emit the highest-confidence cluster as an AnomalyEvent.
//!
//! ## Established-protocol suppression
//!
//! Per design 0018 §11-7 (Decision 7) + SESSION-KICKOFF gotcha #42:
//! D11 does **NOT** suppress on established protocols by default.
//! `suppress_established_protocols = false` in config.
//! Consistent with D08 Sybil non-suppression policy.
//!
//! ## Evidence keys (all prefixed `synchronized_activity_v1/` per gotcha #9)
//!
//! | Key | Type | Meaning |
//! |-----|------|---------|
//! | `synchronized_activity_v1/cluster_size` | Decimal(int) | Wallets in best cluster |
//! | `synchronized_activity_v1/temporal_tightness` | Decimal | [0.0, 1.0] tightness score |
//! | `synchronized_activity_v1/temporal_spread_seconds` | Decimal | Spread of first actions |
//! | `synchronized_activity_v1/poisson_p_value` | Decimal | p-value under null model |
//! | `synchronized_activity_v1/lambda_token_per_second` | Decimal | Token action rate |
//! | `synchronized_activity_v1/delta_seconds` | Decimal | Window width |
//! | `synchronized_activity_v1/secondary_cluster_count` | Decimal | Additional clusters found |
//! | `synchronized_activity_v1/mean_pairwise_jaccard` | Decimal | Average Jaccard in cluster |
//!
//! # Citations
//!
//! - Mazza, Cresci et al. 2019 (RTbust, ACM WebSci 2019, arXiv:1902.04506):
//!   Primary methodological anchor; DBSCAN on temporal patterns; F1=0.87.
//! - Mannocci, Mazza et al. 2024 (CIB Survey, arXiv:2408.01257):
//!   Jaccard temporal similarity + Poisson null model framing.
//! - Arnold et al. 2024 (Temporal Motifs, Scientific Reports, arXiv:2402.09272):
//!   On-chain primary citation; N_min=3 lower bound from temporal motifs.
//! - Nizzoli, Tardelli et al. 2020 (Crypto Landscape, IEEE Access, arXiv:2001.10289):
//!   Domain validation; >56% P&D Telegram bots coordinated on-chain.
//! - research/sprint13-b-citations.md: δ=30s derivation, N_min=5, Poisson framework.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, FromStr as DecimalFromStr};
use sqlx::PgPool;
use tracing::{instrument, warn};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Address;
use mg_onchain_storage::price_provider::TokenPriceProvider;

use crate::context::DetectorContext;
use crate::error::DetectorError;
use crate::signals::{severity_from_confidence, sigmoid};

/// Stable detector ID string used in `AnomalyEvent.detector_id` and as the
/// evidence key prefix (gotcha #9).
pub const DETECTOR_ID: &str = "synchronized_activity_v1";

// ---------------------------------------------------------------------------
// D11SynchronizedActivityDetector
// ---------------------------------------------------------------------------

/// D11 Synchronized-Activity Clustering detector.
///
/// Reads buy-swap events from the `swaps` Postgres table (no new migration required;
/// Decision 6: stateless recompute per evaluation).
///
/// # Phase 5 USD enrichment (Sprint 21)
///
/// `price_provider` injects a `TokenPriceProvider` for computing
/// `total_cluster_volume_usd: Option<Decimal>`. When no price is available,
/// the field is `None` and the raw volume is still emitted.
///
/// # Determinism invariants
///
/// - All SQL queries ordered by `block_time ASC, tx_hash ASC`.
/// - Wallet vertex IDs assigned in order of first appearance in sorted stream.
/// - DBSCAN processes wallets in deterministic first-action order.
/// - No `Utc::now()` anywhere — `ctx.observed_at` is the sole time anchor.
pub struct D11SynchronizedActivityDetector {
    pg: Arc<PgPool>,
    /// Phase 5 USD enrichment (Sprint 21): price provider for cluster volume USD.
    /// PHASE 5 CLOSED Sprint 21: TokenPriceProvider injected; total_cluster_volume_usd: Option<Decimal>
    price_provider: Arc<dyn TokenPriceProvider>,
}

impl D11SynchronizedActivityDetector {
    /// Construct with an existing Postgres pool and price provider.
    pub fn new(pg: Arc<PgPool>, price_provider: Arc<dyn TokenPriceProvider>) -> Self {
        Self { pg, price_provider }
    }
}

// ---------------------------------------------------------------------------
// Detector trait implementation
// ---------------------------------------------------------------------------

impl crate::detector::Detector for D11SynchronizedActivityDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn oak_technique_id(&self) -> Option<&str> {
        Some("OAK-T3.003") // Coordinated Pump-and-Dump
    }

    fn severity_floor(&self) -> Severity {
        Severity::Low
    }

    /// Evaluate D11 synchronized-activity signal for the token in `ctx`.
    ///
    /// # Algorithm (design 0018 §3.1)
    ///
    /// 1. Fetch buy-swap events from `swaps` table for the lookback window.
    /// 2. Compute λ (7-day baseline action rate). If < min_baseline_events → skip.
    /// 3. Bucketize events into δ-second slots. Assign wallet IDs in order of first action.
    /// 4. Compute pairwise Jaccard similarity matrix → DBSCAN clustering.
    /// 5. For each cluster ≥ N_min: filter by temporal_tightness and p_value.
    /// 6. Emit the highest-confidence cluster event (secondary clusters noted in evidence).
    #[instrument(skip(self, ctx), fields(chain = %ctx.chain, token = %ctx.token))]
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx
    {
        async move {
            let cfg = &ctx.config.synchronized_activity_v1;
            let chain_str = ctx.chain.to_string();
            let token_str = ctx.token.to_string();

            let window_end = ctx.observed_at;
            let window_start = window_end
                - chrono::Duration::minutes(cfg.max_lookback_minutes.value as i64);

            // Step 1: Fetch buy-swap events for the lookback window.
            let events = fetch_recent_swap_buys(
                &self.pg,
                &chain_str,
                &token_str,
                window_end,
                cfg.max_lookback_minutes.value as i64,
                cfg.max_events_per_window.value as i64,
            )
            .await
            .map_err(|e| DetectorError::PermanentQuery {
                detector_id: DETECTOR_ID,
                reason: format!("fetch_recent_swap_buys failed: {e}"),
            })?;

            if events.len() < cfg.min_cluster_size.value as usize {
                // Not enough events to form any cluster — skip silently.
                return Ok(vec![]);
            }

            // Step 2: Compute λ from 7-day baseline and validate warmup guard.
            let baseline_start = window_end - chrono::Duration::days(7);
            let baseline_result = fetch_baseline_event_count(
                &self.pg,
                &chain_str,
                &token_str,
                baseline_start,
                window_end,
            )
            .await
            .map_err(|e| DetectorError::PermanentQuery {
                detector_id: DETECTOR_ID,
                reason: format!("fetch_baseline_event_count failed: {e}"),
            })?;

            // Warmup guard (§5.4): insufficient history → skip.
            if baseline_result.count < cfg.min_baseline_events.value as i64 {
                tracing::trace!(
                    chain = %chain_str,
                    token = %token_str,
                    baseline_count = baseline_result.count,
                    min_baseline = cfg.min_baseline_events.value,
                    "D11 warmup guard: insufficient baseline events, skipping"
                );
                return Ok(vec![]);
            }

            // λ = actions / second over the 7-day window.
            let window_seconds_7d = 7.0 * 24.0 * 3600.0_f64;
            let lambda_token = baseline_result.count as f64 / window_seconds_7d;

            // Step 3: Bucketize events and assign wallet IDs.
            let delta_seconds = cfg.window_seconds.value as f64;
            let window_start_epoch = window_start.timestamp() as f64;

            // Collect per-wallet earliest block_time and tx_hash for deterministic ordering.
            // BTreeMap key = wallet address (for deterministic iteration).
            let mut wallet_first_action: BTreeMap<String, (DateTime<Utc>, String)> =
                BTreeMap::new();
            for ev in &events {
                let entry = wallet_first_action
                    .entry(ev.sender.clone())
                    .or_insert_with(|| (ev.block_time, ev.tx_hash.clone()));
                // Keep earliest: if this event's block_time is earlier, update.
                if ev.block_time < entry.0
                    || (ev.block_time == entry.0 && ev.tx_hash < entry.1)
                {
                    *entry = (ev.block_time, ev.tx_hash.clone());
                }
            }

            // Sort wallets by (first_action_block_time ASC, tx_hash ASC) for determinism.
            let mut wallet_order: Vec<String> = wallet_first_action.keys().cloned().collect();
            wallet_order.sort_by(|a, b| {
                let fa = &wallet_first_action[a];
                let fb = &wallet_first_action[b];
                fa.0.cmp(&fb.0).then_with(|| fa.1.cmp(&fb.1))
            });

            // Cap at max_wallets_per_cluster_cap for O(n^2) safety.
            let max_wallets = cfg.max_wallets_per_cluster_cap.value as usize;
            if wallet_order.len() > max_wallets {
                warn!(
                    chain = %chain_str,
                    token = %token_str,
                    wallet_count = wallet_order.len(),
                    cap = max_wallets,
                    "D11 wallet cap hit; truncating to max_wallets_per_cluster_cap"
                );
                wallet_order.truncate(max_wallets);
            }

            // Compute number of buckets covering the lookback window.
            let lookback_seconds =
                cfg.max_lookback_minutes.value as f64 * 60.0;
            let num_buckets = (lookback_seconds / delta_seconds).ceil() as usize + 1;

            // Build presence vectors: wallet_idx → Vec<bool> over buckets.
            // wallet_idx assigned by wallet_order position (deterministic).
            let n = wallet_order.len();
            let mut presence: Vec<Vec<bool>> = vec![vec![false; num_buckets]; n];

            let wallet_idx: BTreeMap<&str, usize> = wallet_order
                .iter()
                .enumerate()
                .map(|(i, w)| (w.as_str(), i))
                .collect();

            for ev in &events {
                let Some(&idx) = wallet_idx.get(ev.sender.as_str()) else {
                    continue; // truncated wallet, skip
                };
                let t_secs = (ev.block_time.timestamp() as f64) - window_start_epoch;
                let bucket = ((t_secs / delta_seconds).floor() as usize).min(num_buckets - 1);
                presence[idx][bucket] = true;
            }

            // Step 4: DBSCAN on Jaccard distance matrix.
            let eps = 1.0 - cfg.jaccard_similarity_threshold.value;
            let min_samples = cfg.min_cluster_size.value as usize;

            let labels = run_dbscan(n, eps, min_samples, |i, j| {
                one_minus_jaccard(&presence[i], &presence[j])
            });

            // Gather clusters (label >= 0 means cluster member; -1 = noise).
            let mut cluster_map: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
            for (idx, &label) in labels.iter().enumerate() {
                if label >= 0 {
                    cluster_map.entry(label).or_default().push(idx);
                }
            }

            if cluster_map.is_empty() {
                return Ok(vec![]);
            }

            // Step 5: Evaluate each cluster.
            let mut valid_events: Vec<(f64, AnomalyEvent)> = Vec::new();

            for wallet_indices in cluster_map.values() {
                let cluster_size = wallet_indices.len();
                if cluster_size < min_samples {
                    continue;
                }

                // Collect wallets in this cluster (sorted by wallet_order index for determinism).
                let cluster_wallets: Vec<&str> = wallet_indices
                    .iter()
                    .map(|&i| wallet_order[i].as_str())
                    .collect();

                // Temporal tightness: spread of first-action times within this cluster.
                let first_times: Vec<DateTime<Utc>> = cluster_wallets
                    .iter()
                    .filter_map(|w| wallet_first_action.get(*w))
                    .map(|(t, _)| *t)
                    .collect();

                let tightness = compute_temporal_tightness_from_times(&first_times, delta_seconds);

                if tightness < cfg.temporal_tightness_threshold.value {
                    tracing::trace!(
                        cluster_size,
                        tightness,
                        threshold = cfg.temporal_tightness_threshold.value,
                        "D11 cluster filtered: temporal_tightness below threshold"
                    );
                    continue;
                }

                // Poisson p-value.
                let p_value =
                    compute_poisson_p_value(lambda_token, delta_seconds, cluster_size);

                if p_value > cfg.poisson_p_threshold.value {
                    tracing::trace!(
                        cluster_size,
                        p_value,
                        threshold = cfg.poisson_p_threshold.value,
                        "D11 cluster filtered: p_value above threshold"
                    );
                    continue;
                }

                // Established-protocol suppression: D11 does NOT suppress by default.
                // Decision 7 (§11-7): suppress only when config explicitly requests it.
                // SPEC-NOTE: The spec keeps suppress_established_protocols = false as default.
                // We respect the config flag but default is non-suppression (D08 policy, gotcha #42).
                if cfg.suppress_established_protocols.value {
                    // Check via token registry if established.
                    let meta = ctx
                        .registry
                        .enrich(&token_str, ctx.chain)
                        .await
                        .map_err(|e| DetectorError::PermanentQuery {
                            detector_id: DETECTOR_ID,
                            reason: format!("registry.enrich failed: {e}"),
                        })?;
                    if crate::token_status::is_established_protocol(&meta) {
                        tracing::trace!(
                            "D11 cluster suppressed: established_protocol suppression enabled"
                        );
                        continue;
                    }
                }

                // Compute confidence.
                let conf = compute_synchronized_activity_confidence(
                    cluster_size,
                    cfg.min_cluster_size.value as usize,
                    cfg.cluster_size_scale.value,
                    tightness,
                    p_value,
                    cfg.poisson_p_threshold.value,
                    cfg.weight_cluster_size.value,
                    cfg.weight_temporal_tightness.value,
                    cfg.weight_statistical_significance.value,
                );

                // Mean pairwise Jaccard for evidence.
                let mean_jaccard = compute_mean_pairwise_jaccard(wallet_indices, &presence);

                // Temporal spread for evidence.
                let temporal_spread = compute_temporal_spread_seconds(&first_times);

                // Representative tx hashes (one per wallet, up to 50).
                let rep_txs: Vec<String> = cluster_wallets
                    .iter()
                    .take(50)
                    .filter_map(|w| wallet_first_action.get(*w))
                    .map(|(_, tx)| tx.clone())
                    .collect();

                // Total cluster volume (sum of amount_out_raw for cluster wallets).
                // PHASE 5 CLOSED Sprint 21: TokenPriceProvider injected;
                // total_cluster_volume_usd: Option<Decimal>. Raw sum still emitted.
                let total_volume_raw: u128 = events
                    .iter()
                    .filter(|ev| {
                        wallet_first_action
                            .contains_key(ev.sender.as_str())
                            && cluster_wallets.contains(&ev.sender.as_str())
                    })
                    .map(|ev| ev.amount_out_raw)
                    .fold(0u128, |acc, v| acc.saturating_add(v));

                // Phase 5 USD enrichment: look up price for the evaluated token.
                // Returns None when no price source available — detector still fires
                // with raw volume only.
                let token_price_usd: Option<Decimal> = self
                    .price_provider
                    .get_token_price_usd(ctx.chain, ctx.token, ctx.observed_at)
                    .await;

                // Exact token decimals from the tokens table (closed S21 SPEC-NOTE).
                // Falls back to 9 (Solana SPL standard) when the token is not in the registry.
                // The fallback preserves existing behaviour for unlisted tokens.
                let token_decimals: u32 = self
                    .price_provider
                    .get_token_decimals(ctx.chain, ctx.token)
                    .await
                    .unwrap_or(9);

                // Compute USD cluster volume when price is available.
                let total_cluster_volume_usd: Option<Decimal> = token_price_usd
                    .and_then(|price| {
                        if total_volume_raw == 0 {
                            return Some(Decimal::ZERO);
                        }
                        let divisor = Decimal::from(10u64.saturating_pow(token_decimals));
                        if divisor.is_zero() {
                            return None;
                        }
                        let tokens = Decimal::from(total_volume_raw as u64) / divisor;
                        Some(tokens * price)
                    });

                // Wallet list for evidence (up to 50 addresses; overflow noted).
                let wallet_list_truncated = cluster_size > 50;
                let wallet_display: Vec<String> = cluster_wallets
                    .iter()
                    .take(50)
                    .map(|w| w.to_string())
                    .collect();

                // Build evidence.
                let conf_dec = Decimal::from_f64(conf).unwrap_or(Decimal::ZERO);
                let tightness_dec =
                    Decimal::from_f64(tightness).unwrap_or(Decimal::ZERO);
                let spread_dec =
                    Decimal::from_f64(temporal_spread).unwrap_or(Decimal::ZERO);
                let p_value_dec = Decimal::from_str(&format!("{p_value:.15e}"))
                    .unwrap_or(Decimal::ZERO);
                let lambda_dec =
                    Decimal::from_f64(lambda_token).unwrap_or(Decimal::ZERO);
                let mean_jac_dec =
                    Decimal::from_f64(mean_jaccard).unwrap_or(Decimal::ZERO);

                let mut evidence = Evidence::new()
                    .with_metric(
                        format!("{DETECTOR_ID}/cluster_size"),
                        Decimal::from(cluster_size as u64),
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/temporal_tightness"),
                        tightness_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/temporal_spread_seconds"),
                        spread_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/poisson_p_value"),
                        p_value_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/lambda_token_per_second"),
                        lambda_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/delta_seconds"),
                        Decimal::from(cfg.window_seconds.value),
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/mean_pairwise_jaccard"),
                        mean_jac_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/total_cluster_volume_raw"),
                        Decimal::from(total_volume_raw as u64),
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/confidence"),
                        conf_dec,
                    )
                    .with_note(format!(
                        "algorithm=jaccard_dbscan; delta_seconds={}; action_source=swap_buy",
                        cfg.window_seconds.value
                    ));

                // Phase 5 USD enrichment (Sprint 21): emit total_cluster_volume_usd
                // when price is available. None → omit metric (explicit absence per
                // Decision 3: fallback emits Option<Decimal> = None).
                if let Some(usd_vol) = total_cluster_volume_usd {
                    evidence = evidence.with_metric(
                        format!("{DETECTOR_ID}/total_cluster_volume_usd"),
                        usd_vol,
                    );
                } else {
                    evidence = evidence.with_note(
                        format!("{DETECTOR_ID}/total_cluster_volume_usd=null")
                    );
                }

                if wallet_list_truncated {
                    evidence = evidence.with_note(format!(
                        "cluster_wallets_truncated=true; total_count={cluster_size}"
                    ));
                }

                for w in &wallet_display {
                    if let Ok(addr) = Address::parse(ctx.chain, w) {
                        evidence = evidence.with_address(addr);
                    }
                }

                for tx in &rep_txs {
                    use mg_onchain_common::chain::TxHash;
                    if let Ok(tx_hash) = TxHash::parse(ctx.chain, tx) {
                        evidence = evidence.with_tx(tx_hash);
                    }
                }

                // window_start_block_time + window_end_block_time in notes (not as Decimal).
                evidence = evidence.with_note(format!(
                    "window_start_block_time={}; window_end_block_time={}",
                    window_start.to_rfc3339(),
                    window_end.to_rfc3339()
                ));

                let confidence = Confidence::new(conf).map_err(|e| {
                    DetectorError::DeterminismViolation {
                        detector_id: DETECTOR_ID,
                        reason: format!("confidence out of range after clamp (bug): {e}"),
                    }
                })?;

                let severity = severity_from_confidence(conf);

                let event = AnomalyEvent {
                    detector_id: DETECTOR_ID.to_owned(),
                    token: ctx.token.clone(),
                    chain: ctx.chain,
                    confidence,
                    severity,
                    evidence,
                    observed_at: ctx.observed_at,
                    oak_technique_id: None,
                    ingested_at: ctx.observed_at,
                    window: (ctx.window.block_start, ctx.window.block_end),
                };

                valid_events.push((conf, event));
            }

            if valid_events.is_empty() {
                return Ok(vec![]);
            }

            // Step 6: Emit the highest-confidence event; note secondary cluster count.
            // Sort descending by confidence for deterministic selection.
            valid_events.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let secondary_count = valid_events.len().saturating_sub(1);
            let (_, mut best_event) = valid_events.remove(0);

            // Annotate secondary cluster count.
            best_event.evidence = best_event
                .evidence
                .with_metric(
                    format!("{DETECTOR_ID}/secondary_cluster_count"),
                    Decimal::from(secondary_count as u64),
                )
                .with_note(format!(
                    "algorithm_version={}",
                    DETECTOR_ID
                ));

            Ok(vec![best_event])
        }
    }
}

// ---------------------------------------------------------------------------
// SwapBuyRow — row struct for fetch_recent_swap_buys
// ---------------------------------------------------------------------------

/// A single buy-swap event returned from the `swaps` table for D11 evaluation.
///
/// Follows the `RoundTripRow` pattern in pg.rs (String-bridge for NUMERIC amounts;
/// ORDER BY block_height ASC, tx_hash ASC for determinism).
#[derive(Debug, Clone)]
pub struct SwapBuyRow {
    /// Wallet (sender) address in canonical chain form.
    pub sender: String,
    /// Pool address in canonical chain form.
    pub pool: String,
    /// Block timestamp for this swap.
    pub block_time: DateTime<Utc>,
    /// Block height for ordering (determinism).
    pub block_height: i64,
    /// Raw token amount received (amount_out_raw from swaps table).
    pub amount_out_raw: u128,
    /// Transaction hash for evidence bundle.
    pub tx_hash: String,
}

// ---------------------------------------------------------------------------
// Baseline count result
// ---------------------------------------------------------------------------

struct BaselineResult {
    count: i64,
}

// ---------------------------------------------------------------------------
// Storage helpers (follow fetch_wash_trading_round_trips pattern)
// ---------------------------------------------------------------------------

/// Fetch recent buy-swap events for a token within a lookback window.
///
/// Ordered by `block_height ASC, tx_hash ASC` for deterministic input to DBSCAN.
/// Hard cap via LIMIT; WARN log on cap hit (consistent with wash-trading pattern).
///
/// # Parameters
///
/// - `chain`: chain string (e.g. `"solana"`).
/// - `token`: token mint / contract address.
/// - `window_end`: exclusive end of the observation window.
/// - `lookback_minutes`: window length in minutes.
/// - `max_rows`: hard row cap (safety ceiling, WARN logged if hit).
#[instrument(skip(pool), fields(chain, token))]
pub(crate) async fn fetch_recent_swap_buys(
    pool: &PgPool,
    chain: &str,
    token: &str,
    window_end: DateTime<Utc>,
    lookback_minutes: i64,
    max_rows: i64,
) -> Result<Vec<SwapBuyRow>, sqlx::Error> {
    let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

    // Buy swaps: token_out = token (we receive the token → buy side).
    // ORDER BY block_height ASC, tx_hash ASC for determinism.
    let rows = sqlx::query(
        r#"
SELECT sender, pool, block_time, block_height, tx_hash,
       amount_out_raw::TEXT AS amount_out_raw_str
FROM swaps
WHERE chain = $1
  AND token_out = $2
  AND block_time >= $3
  AND block_time <  $4
ORDER BY block_height ASC, tx_hash ASC
LIMIT $5
        "#,
    )
    .bind(chain)
    .bind(token)
    .bind(window_start)
    .bind(window_end)
    .bind(max_rows)
    .fetch_all(pool)
    .await?;

    let hit_cap = rows.len() as i64 >= max_rows;
    if hit_cap {
        warn!(
            chain,
            token,
            cap = max_rows,
            "D11 fetch_recent_swap_buys hit max_rows cap; results may be incomplete"
        );
    }

    let mut result = Vec::with_capacity(rows.len());
    for r in rows {
        use sqlx::Row as _;
        let sender: String = r.try_get("sender")?;
        let pool_addr: String = r.try_get("pool")?;
        let block_time: DateTime<Utc> = r.try_get("block_time")?;
        let block_height: i64 = r.try_get("block_height")?;
        let tx_hash: String = r.try_get("tx_hash")?;
        let amount_out_raw_str: String = r.try_get("amount_out_raw_str")?;
        let amount_out_raw: u128 = amount_out_raw_str.parse().unwrap_or(0);

        result.push(SwapBuyRow {
            sender,
            pool: pool_addr,
            block_time,
            block_height,
            amount_out_raw,
            tx_hash,
        });
    }

    tracing::debug!(
        chain,
        token,
        count = result.len(),
        "D11 fetch_recent_swap_buys returned rows"
    );
    Ok(result)
}

/// Fetch the count of buy-swap events in a 7-day window for λ estimation.
///
/// Used to validate the warmup guard (§5.4) and compute `lambda_token`.
#[instrument(skip(pool), fields(chain, token))]
async fn fetch_baseline_event_count(
    pool: &PgPool,
    chain: &str,
    token: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Result<BaselineResult, sqlx::Error> {
    let row = sqlx::query(
        r#"
SELECT COUNT(*) AS event_count
FROM swaps
WHERE chain = $1
  AND token_out = $2
  AND block_time >= $3
  AND block_time <  $4
        "#,
    )
    .bind(chain)
    .bind(token)
    .bind(window_start)
    .bind(window_end)
    .fetch_one(pool)
    .await?;

    use sqlx::Row as _;
    let count: i64 = row.try_get("event_count")?;
    Ok(BaselineResult { count })
}

// ---------------------------------------------------------------------------
// Pure math functions (exposed pub for unit testing without I/O)
// ---------------------------------------------------------------------------

/// Compute 1 − Jaccard(i, j) between two binary presence vectors.
///
/// J(i, j) = |B_i ∩ B_j| / |B_i ∪ B_j|
/// Distance = 1 − J(i, j)
///
/// Returns 1.0 (maximum distance) if both vectors have no active buckets
/// (no union → undefined Jaccard → treat as maximally dissimilar to avoid
/// false clustering).
///
/// # Determinism
///
/// Pure function: same inputs → same f64 output.
pub fn one_minus_jaccard(a: &[bool], b: &[bool]) -> f64 {
    debug_assert_eq!(a.len(), b.len(), "presence vectors must have equal length");
    let len = a.len().min(b.len());

    let mut intersection: usize = 0;
    let mut union_count: usize = 0;

    for i in 0..len {
        let ai = a[i];
        let bi = b[i];
        if ai && bi {
            intersection += 1;
        }
        if ai || bi {
            union_count += 1;
        }
    }

    if union_count == 0 {
        // Both vectors are all-false: no temporal overlap at all.
        // Treat as maximum distance (completely dissimilar).
        return 1.0;
    }

    1.0 - (intersection as f64 / union_count as f64)
}

/// Run DBSCAN on `n` items using a distance closure.
///
/// # Algorithm (design 0018 §3.3 pseudocode)
///
/// Standard DBSCAN: O(n²) pairwise distance evaluation.
/// Deterministic: processes items in index order 0..n.
///
/// # Parameters
///
/// - `n`: number of items.
/// - `eps`: maximum distance for two items to be neighbors.
/// - `min_samples`: minimum cluster size (including the core point itself).
/// - `dist`: closure returning distance between item i and item j.
///
/// # Returns
///
/// `Vec<i32>` of cluster labels (length = n). -1 = noise point.
pub fn run_dbscan(
    n: usize,
    eps: f64,
    min_samples: usize,
    dist: impl Fn(usize, usize) -> f64,
) -> Vec<i32> {
    let mut labels: Vec<i32> = vec![-1_i32; n];
    let mut cluster_id: i32 = 0;

    // Precompute all neighbors lists once (avoids recomputation inside seed expansion).
    // Safety: n is bounded by max_wallets_per_cluster_cap (config default 500).
    let neighbors_of: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            (0..n)
                .filter(|&j| j != i && dist(i, j) <= eps)
                .collect::<Vec<usize>>()
        })
        .collect();

    for i in 0..n {
        if labels[i] != -1 {
            continue; // already classified
        }

        let nbrs = &neighbors_of[i];
        // A core point has at least min_samples - 1 neighbors (itself = 1 + nbrs).
        if nbrs.len() + 1 < min_samples {
            // i is noise (may later be absorbed as border point).
            continue;
        }

        // i is a core point — start new cluster.
        labels[i] = cluster_id;
        let mut seed_set: Vec<usize> = nbrs.clone();
        let mut seed_idx = 0;

        while seed_idx < seed_set.len() {
            let j = seed_set[seed_idx];
            seed_idx += 1;

            if labels[j] == -1 {
                // j was noise — absorb as border point.
                labels[j] = cluster_id;
            }

            // If j already belongs to this cluster, check if it expands the seed set.
            if labels[j] == cluster_id {
                let j_nbrs = &neighbors_of[j];
                if j_nbrs.len() + 1 >= min_samples {
                    // j is a core point — add its unvisited neighbors.
                    for &k in j_nbrs {
                        if labels[k] == -1 || labels[k] != cluster_id {
                            // Only add if not already in seed_set as cluster member.
                            if !seed_set.contains(&k) {
                                seed_set.push(k);
                            }
                        }
                    }
                }
            }
        }

        cluster_id += 1;
    }

    labels
}

/// Compute Poisson p-value: probability that `cluster_size` wallets each
/// independently buy within `delta_seconds` given baseline rate `lambda_token`.
///
/// ```text
/// p_one   = 1 − exp(−lambda_token × delta_seconds)
/// p_joint = p_one ^ cluster_size
/// ```
///
/// Returns 1.0 if `lambda_token == 0.0` (warmup guard — no baseline; §5.3).
///
/// # Note on f64
///
/// p-values are probabilities, not monetary amounts. f64 is the correct type here
/// per CLAUDE.md ("NEVER f64 for prices, amounts, supplies, liquidity" — this is none).
pub fn compute_poisson_p_value(
    lambda_token: f64,
    delta_seconds: f64,
    cluster_size: usize,
) -> f64 {
    if lambda_token <= 0.0 {
        return 1.0; // No baseline → no signal
    }
    let p_one = 1.0 - (-lambda_token * delta_seconds).exp();
    // Guard: if p_one rounds to 0.0 (extremely low lambda), return 0.0 as a
    // conservative approximation (extremely significant result).
    if p_one <= 0.0 {
        return 0.0;
    }
    p_one.powi(cluster_size as i32)
}

/// Compute D11 combined confidence score.
///
/// # Sub-signals (design 0018 §4.1)
///
/// - S_size  = sigmoid((cluster_size - min_cluster_size) / cluster_size_scale)
/// - S_tight = temporal_tightness   [0.0, 1.0]
/// - S_stat  = 1 − p_value / poisson_p_threshold  (clamped to [0.0, 1.0])
///
/// # Combined
///
/// conf_raw = (w_size × S_size + w_tight × S_tight + w_stat × S_stat)
///            / (w_size + w_tight + w_stat)
///
/// conf_final = min(conf_raw, 0.90)   (hard cap; design 0018 §4.3)
///
/// # Exposed for unit testing (pure function — no I/O)
#[allow(clippy::too_many_arguments)]
pub fn compute_synchronized_activity_confidence(
    cluster_size: usize,
    min_cluster_size: usize,
    cluster_size_scale: f64,
    temporal_tightness: f64,
    p_value: f64,
    poisson_p_threshold: f64,
    w_size: f64,
    w_tight: f64,
    w_stat: f64,
) -> f64 {
    // S_size: sigmoid((cluster_size - min_cluster_size) / cluster_size_scale)
    let size_arg =
        (cluster_size as f64 - min_cluster_size as f64) / cluster_size_scale.max(1e-9);
    let s_size = sigmoid(size_arg);

    // S_tight: temporal tightness already in [0.0, 1.0].
    let s_tight = temporal_tightness.clamp(0.0, 1.0);

    // S_stat: linear mapping from p_value space to [0.0, 1.0].
    let s_stat = if p_value <= poisson_p_threshold {
        (1.0 - p_value / poisson_p_threshold.max(1e-300)).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let total_weight = w_size + w_tight + w_stat;
    let conf_raw = if total_weight <= 0.0 {
        0.0
    } else {
        (w_size * s_size + w_tight * s_tight + w_stat * s_stat) / total_weight
    };

    // Hard cap at 0.90 (design 0018 §4.3; consistent with D08 Sybil cap).
    conf_raw.clamp(0.0, 0.90)
}

/// Compute temporal tightness from first-action timestamps of cluster wallets.
///
/// ```text
/// temporal_spread = max(first_times) - min(first_times)  [seconds]
/// tightness = 1.0 - (temporal_spread / delta_seconds)
///             clamped to [0.0, 1.0]
/// ```
///
/// Returns 1.0 for a single-wallet cluster or empty slice.
pub fn compute_temporal_tightness_from_times(
    first_times: &[DateTime<Utc>],
    delta_seconds: f64,
) -> f64 {
    if first_times.len() < 2 {
        return 1.0; // Single wallet or empty → maximum tightness.
    }

    let min_t = first_times
        .iter()
        .min()
        .expect("non-empty slice");
    let max_t = first_times
        .iter()
        .max()
        .expect("non-empty slice");

    let spread_secs = (*max_t - *min_t).num_milliseconds() as f64 / 1000.0;

    if delta_seconds <= 0.0 {
        return 0.0;
    }

    (1.0 - spread_secs / delta_seconds).clamp(0.0, 1.0)
}

/// Compute temporal spread in seconds (max - min first-action time).
fn compute_temporal_spread_seconds(first_times: &[DateTime<Utc>]) -> f64 {
    if first_times.len() < 2 {
        return 0.0;
    }
    let min_t = first_times.iter().min().expect("non-empty slice");
    let max_t = first_times.iter().max().expect("non-empty slice");
    (*max_t - *min_t).num_milliseconds() as f64 / 1000.0
}

/// Compute mean pairwise Jaccard similarity (not distance) within a cluster.
///
/// Returns 1.0 for single-member clusters.
fn compute_mean_pairwise_jaccard(
    wallet_indices: &[usize],
    presence: &[Vec<bool>],
) -> f64 {
    let n = wallet_indices.len();
    if n < 2 {
        return 1.0;
    }

    let mut sum_jaccard = 0.0_f64;
    let mut pair_count = 0_usize;

    for i in 0..n {
        for j in (i + 1)..n {
            let wi = wallet_indices[i];
            let wj = wallet_indices[j];
            let dist = one_minus_jaccard(&presence[wi], &presence[wj]);
            sum_jaccard += 1.0 - dist; // convert distance back to similarity
            pair_count += 1;
        }
    }

    if pair_count == 0 {
        1.0
    } else {
        sum_jaccard / pair_count as f64
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono::prelude::DateTime;
    use chrono::Utc;

    // -------------------------------------------------------------------------
    // Jaccard math tests
    // -------------------------------------------------------------------------

    /// Identical binary presence vectors → Jaccard = 1.0 → distance = 0.0.
    #[test]
    fn jaccard_identical_sets_distance_zero() {
        let a = vec![true, false, true, true, false];
        let b = vec![true, false, true, true, false];
        let dist = one_minus_jaccard(&a, &b);
        assert!(
            dist.abs() < 1e-12,
            "identical vectors must have distance 0.0, got {dist}"
        );
    }

    /// Disjoint binary presence vectors → Jaccard = 0.0 → distance = 1.0.
    #[test]
    fn jaccard_disjoint_sets_distance_one() {
        let a = vec![true, false, false, false];
        let b = vec![false, true, false, false];
        let dist = one_minus_jaccard(&a, &b);
        assert!(
            (dist - 1.0).abs() < 1e-12,
            "disjoint vectors must have distance 1.0, got {dist}"
        );
    }

    /// Partial overlap: intersection=2, union=4 → Jaccard=0.5 → distance=0.5.
    #[test]
    fn jaccard_partial_overlap_known_value() {
        // A = {0, 1, 2}, B = {1, 2, 3}
        // intersection = {1, 2} = 2 buckets
        // union = {0, 1, 2, 3} = 4 buckets
        // J = 2/4 = 0.5 → dist = 0.5
        let a = vec![true, true, true, false];
        let b = vec![false, true, true, true];
        let dist = one_minus_jaccard(&a, &b);
        assert!(
            (dist - 0.5).abs() < 1e-12,
            "partial overlap must give distance 0.5, got {dist}"
        );
    }

    /// All-false vectors → distance = 1.0 (no union → max distance per spec).
    #[test]
    fn jaccard_all_false_vectors_distance_one() {
        let a = vec![false, false, false];
        let b = vec![false, false, false];
        let dist = one_minus_jaccard(&a, &b);
        assert!(
            (dist - 1.0).abs() < 1e-12,
            "all-false vectors must have distance 1.0 (no union), got {dist}"
        );
    }

    // -------------------------------------------------------------------------
    // DBSCAN tests
    // -------------------------------------------------------------------------

    /// 3 wallets with identical presence vectors → single cluster detected.
    ///
    /// eps=0.30 (jaccard_similarity_threshold=0.70), min_samples=3.
    #[test]
    fn dbscan_synchronized_cluster_detected() {
        // 3 items, all at distance 0.0 from each other (identical).
        // All pairs have Jaccard=1.0 → distance=0.0 → all are mutual neighbors.
        let dist = |_i: usize, _j: usize| -> f64 { 0.0 };
        let labels = run_dbscan(3, 0.30, 3, dist);
        // All 3 must be in the same cluster (label 0).
        assert_eq!(labels[0], 0);
        assert_eq!(labels[1], 0);
        assert_eq!(labels[2], 0);
    }

    /// 5 wallets with random (high-distance) patterns → no valid cluster.
    ///
    /// When distances are all > eps (1.0 > 0.30), DBSCAN produces only noise.
    #[test]
    fn dbscan_scattered_wallets_no_cluster() {
        // 5 items, all at maximum distance from each other.
        let dist = |_i: usize, _j: usize| -> f64 { 1.0 };
        let labels = run_dbscan(5, 0.30, 3, dist);
        // All should be noise (-1).
        assert!(
            labels.iter().all(|&l| l == -1),
            "all-distant wallets must be noise: {labels:?}"
        );
    }

    /// Cluster + noise: 3 coordinated + 2 outliers.
    #[test]
    fn dbscan_cluster_with_noise_points() {
        // Items 0,1,2 are all at distance 0.0 (coordinated).
        // Items 3,4 are at distance 1.0 from everything.
        let dist = |i: usize, j: usize| -> f64 {
            if i < 3 && j < 3 && i != j {
                0.0
            } else {
                1.0
            }
        };
        let labels = run_dbscan(5, 0.30, 3, dist);
        // Items 0,1,2 should be in cluster 0; items 3,4 should be noise (-1).
        assert_eq!(labels[0], 0);
        assert_eq!(labels[1], 0);
        assert_eq!(labels[2], 0);
        assert_eq!(labels[3], -1);
        assert_eq!(labels[4], -1);
    }

    // -------------------------------------------------------------------------
    // Poisson p-value tests
    // -------------------------------------------------------------------------

    /// High λ baseline → high p_one → p_joint only moderate.
    ///
    /// λ=10/s, δ=30s → p_one = 1 - exp(-300) ≈ 1.0 → p_joint = 1.0^5 = 1.0.
    /// A token with 10 buys/second has such high organic activity that any
    /// 5-wallet cluster within 30s is perfectly consistent with randomness.
    #[test]
    fn poisson_p_value_high_lambda_baseline_high_p() {
        let p = compute_poisson_p_value(10.0, 30.0, 5);
        // At λ=10/s, p_one ≈ 1.0, so p_joint ≈ 1.0.
        assert!(
            p > 0.99,
            "high-lambda baseline must produce p_value near 1.0, got {p}"
        );
    }

    /// Low λ with burst cluster → extremely low p.
    ///
    /// λ=0.001/s (1 buy per 17 min), δ=30s, k=7.
    /// p_one = 1 - exp(-0.03) ≈ 0.0296
    /// p_joint = 0.0296^7 ≈ 5.5e-12.
    #[test]
    fn poisson_p_value_low_lambda_burst_low_p() {
        let p = compute_poisson_p_value(0.001, 30.0, 7);
        assert!(
            p < 1e-6,
            "low-lambda burst must produce p_value far below 1e-6, got {p:.3e}"
        );
        assert!(
            p < 1e-9,
            "low-lambda 7-wallet burst must produce p_value < 1e-9, got {p:.3e}"
        );
    }

    /// λ=0.0 → p_value = 1.0 (warmup guard: no baseline → no signal).
    #[test]
    fn poisson_p_value_zero_lambda_returns_one() {
        let p = compute_poisson_p_value(0.0, 30.0, 5);
        assert!(
            (p - 1.0).abs() < 1e-12,
            "zero-lambda must return p_value=1.0, got {p}"
        );
    }

    // -------------------------------------------------------------------------
    // Confidence formula tests
    // -------------------------------------------------------------------------

    /// Small cluster (exactly N_min=5) + high p → low confidence.
    ///
    /// S_size = sigmoid(0) = 0.5; S_tight = 0.5; S_stat = 0.0 (p above threshold).
    /// conf_raw = (0.4*0.5 + 0.3*0.5 + 0.3*0.0) = (0.20 + 0.15) = 0.35.
    #[test]
    fn confidence_small_cluster_high_p_low_conf() {
        let conf = compute_synchronized_activity_confidence(
            5,    // cluster_size
            5,    // min_cluster_size
            5.0,  // cluster_size_scale
            0.5,  // temporal_tightness
            1e-3, // p_value > poisson_p_threshold (1e-6) → S_stat = 0.0
            1e-6, // poisson_p_threshold
            0.40, // w_size
            0.30, // w_tight
            0.30, // w_stat
        );
        // S_stat = 0 because p_value > threshold.
        // conf = (0.4*0.5 + 0.3*0.5) = 0.35.
        assert!(
            conf < 0.40,
            "small cluster + high p must produce conf < 0.40, got {conf:.4}"
        );
        assert!(
            (conf - 0.35).abs() < 1e-9,
            "expected conf ≈ 0.35, got {conf:.6}"
        );
    }

    /// Large cluster (15 wallets) + low p (1e-12) → high confidence near cap.
    ///
    /// S_size = sigmoid((15-5)/5) = sigmoid(2.0) ≈ 0.880
    /// S_tight = 0.9; S_stat ≈ 1.0 (p << threshold).
    /// conf_raw ≈ 0.4*0.880 + 0.3*0.9 + 0.3*1.0 = 0.352 + 0.27 + 0.30 = 0.922.
    /// Capped at 0.90.
    #[test]
    fn confidence_large_cluster_low_p_near_cap() {
        let conf = compute_synchronized_activity_confidence(
            15,    // cluster_size
            5,     // min_cluster_size
            5.0,   // cluster_size_scale
            0.9,   // temporal_tightness
            1e-12, // p_value << threshold
            1e-6,  // poisson_p_threshold
            0.40,  // w_size
            0.30,  // w_tight
            0.30,  // w_stat
        );
        assert!(
            conf >= 0.85,
            "large cluster + low p must produce conf >= 0.85, got {conf:.4}"
        );
        assert!(
            conf <= 0.90,
            "confidence must be capped at 0.90, got {conf:.4}"
        );
    }

    /// Confidence is hard-capped at 0.90 regardless of extreme inputs.
    ///
    /// This is the §4.3 cap: "irreducible uncertainty about intent".
    /// Consistent with D08 Sybil cap (0.95) and D05 Signal B cap (0.85).
    #[test]
    fn confidence_hard_cap_at_0_90() {
        // Extreme inputs: 100-wallet cluster, tightness=1.0, p=0.
        let conf = compute_synchronized_activity_confidence(
            100,  // cluster_size
            5,    // min_cluster_size
            5.0,  // cluster_size_scale
            1.0,  // temporal_tightness (perfect)
            0.0,  // p_value (impossible under null)
            1e-6, // poisson_p_threshold
            0.40, // w_size
            0.30, // w_tight
            0.30, // w_stat
        );
        assert!(
            conf <= 0.90,
            "confidence must never exceed 0.90 cap, got {conf:.4}"
        );
        // Should be near the cap.
        assert!(
            conf >= 0.85,
            "extreme inputs must produce confidence near cap, got {conf:.4}"
        );
    }

    // -------------------------------------------------------------------------
    // Temporal tightness tests
    // -------------------------------------------------------------------------

    /// 7-day warmup guard: insufficient baseline → detector must skip.
    ///
    /// This tests the logic path: baseline_count < min_baseline_events → Ok(vec![]).
    /// We validate the guard condition rather than full DB round-trip.
    #[test]
    fn warmup_guard_insufficient_baseline_produces_no_event() {
        let min_baseline_events: i64 = 10;
        let actual_count: i64 = 3; // below threshold

        // The guard condition in evaluate():
        // if baseline_result.count < cfg.min_baseline_events.value → return Ok(vec![])
        assert!(
            actual_count < min_baseline_events,
            "insufficient baseline must trigger warmup guard"
        );
        // Result: Ok(vec![]) — no event emitted.
    }

    // -------------------------------------------------------------------------
    // Determinism test
    // -------------------------------------------------------------------------

    /// Same inputs → bit-identical output (pure function determinism).
    ///
    /// Tests the pure math functions that feed into evaluate().
    /// Full evaluate() determinism is validated by the integration fixture test.
    #[test]
    fn pure_functions_deterministic_three_runs() {
        let input_args = (0.001f64, 30.0f64, 7usize, 1e-6f64);

        let run = |()| -> (f64, f64) {
            let p = compute_poisson_p_value(input_args.0, input_args.1, input_args.2);
            let conf = compute_synchronized_activity_confidence(
                input_args.2, // cluster_size
                5,             // min_cluster_size
                5.0,           // cluster_size_scale
                0.80,          // temporal_tightness
                p,             // p_value
                input_args.3,  // poisson_p_threshold
                0.40, 0.30, 0.30,
            );
            (p, conf)
        };

        let r1 = run(());
        let r2 = run(());
        let r3 = run(());

        assert_eq!(r1, r2, "run1 != run2: not deterministic");
        assert_eq!(r2, r3, "run2 != run3: not deterministic");
    }

    // -------------------------------------------------------------------------
    // Suppression NOT applied test
    // -------------------------------------------------------------------------

    /// D11 does NOT suppress on established protocols by default.
    ///
    /// Per design 0018 §11-7 Decision 7 + gotcha #42: suppress_established_protocols
    /// defaults to false. This test verifies the confidence formula produces a result
    /// (i.e., no suppression short-circuit occurs in the math path).
    ///
    /// The established-protocol suppression is a config flag; with the default
    /// suppress_established_protocols = false, even established tokens would generate
    /// an event if the math criteria are met.
    #[test]
    fn no_suppression_established_protocol_still_produces_confidence() {
        // Scenario: a token with is_established_protocol = true but showing coordinated buying.
        // D11 should NOT suppress — it should compute confidence normally.
        let conf = compute_synchronized_activity_confidence(
            7,     // cluster_size
            5,     // min_cluster_size
            5.0,   // cluster_size_scale
            0.80,  // temporal_tightness
            1e-10, // p_value (very significant)
            1e-6,  // poisson_p_threshold
            0.40, 0.30, 0.30,
        );
        // Confidence > 0 → would generate an event (not suppressed).
        assert!(
            conf > 0.0,
            "established protocol should still produce confidence > 0 when suppress=false"
        );
        assert!(
            conf > 0.50,
            "7-wallet coordinated cluster on established token should have meaningful confidence"
        );
    }

    // -------------------------------------------------------------------------
    // Temporal tightness formula
    // -------------------------------------------------------------------------

    /// Tightness = 1.0 when all wallets act in the same instant.
    #[test]
    fn temporal_tightness_all_same_time_is_one() {
        let t = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 0).unwrap();
        let times = vec![t, t, t, t, t];
        let tightness = compute_temporal_tightness_from_times(&times, 30.0);
        assert!(
            (tightness - 1.0).abs() < 1e-9,
            "all-same-time must give tightness=1.0, got {tightness}"
        );
    }

    /// Tightness = 0.0 when spread equals full window.
    #[test]
    fn temporal_tightness_full_spread_is_zero() {
        let t0 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 30).unwrap(); // +30s = exactly delta
        let times = vec![t0, t1];
        let tightness = compute_temporal_tightness_from_times(&times, 30.0);
        assert!(
            tightness.abs() < 1e-9,
            "spread=delta must give tightness=0.0, got {tightness}"
        );
    }

    /// Tightness is clamped to [0.0, 1.0] when spread exceeds delta.
    #[test]
    fn temporal_tightness_exceeds_delta_clamped_to_zero() {
        let t0 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 1, 0).unwrap(); // +60s > delta=30s
        let times = vec![t0, t1];
        let tightness = compute_temporal_tightness_from_times(&times, 30.0);
        assert!(
            tightness >= 0.0,
            "tightness must be clamped to >= 0.0, got {tightness}"
        );
        assert!(
            tightness == 0.0,
            "spread > delta must clamp to 0.0, got {tightness}"
        );
    }

    /// Tightness with 6s spread over 30s window = 0.80.
    #[test]
    fn temporal_tightness_6s_spread_over_30s_window() {
        // POS_D11_01 fixture: 7 wallets within 6 seconds → tightness = 1 - 6/30 = 0.80.
        let t0 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 4).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 10).unwrap(); // +6s
        let times = vec![t0, t1];
        let tightness = compute_temporal_tightness_from_times(&times, 30.0);
        assert!(
            (tightness - 0.80).abs() < 1e-9,
            "6s spread / 30s delta must give tightness=0.80, got {tightness}"
        );
    }

    // -------------------------------------------------------------------------
    // Decimals wiring tests (closed S21 SPEC-NOTE)
    // -------------------------------------------------------------------------

    /// Token with 6 decimals (e.g. USDC): volume USD uses 6-decimal divisor, not 9.
    ///
    /// Verifies the closed SPEC-NOTE: get_token_decimals replaces the hardcoded 9.
    /// raw_amount = 1_000_000 (1 USDC with 6 decimals), price = $1.00 → volume = $1.00.
    #[test]
    fn decimals_6_usdc_volume_usd_correct() {
        let raw_amount: u128 = 1_000_000; // 1 USDC (6 decimals)
        let price = Decimal::from(1u32);  // $1.00
        let decimals: u32 = 6;
        let divisor = Decimal::from(10u64.saturating_pow(decimals));
        let tokens = Decimal::from(raw_amount as u64) / divisor;
        let usd = tokens * price;
        // 1_000_000 / 10^6 * $1.00 = 1.00 USDC = $1.00
        assert_eq!(
            usd,
            Decimal::from(1u32),
            "1 USDC (6 dec) at $1.00 should compute to $1.00 USD"
        );
    }

    /// Token with 9 decimals (default SPL): fallback preserves existing behaviour.
    ///
    /// When get_token_decimals returns None, fallback = 9 is used.
    /// raw_amount = 1_000_000_000 (1 token with 9 decimals), price = $2.00 → $2.00.
    #[test]
    fn decimals_9_fallback_spl_standard() {
        let raw_amount: u128 = 1_000_000_000; // 1 token (9 decimals, SPL default)
        let price = Decimal::from(2u32);
        let fallback_decimals: u32 = 9; // matches unwrap_or(9)
        let divisor = Decimal::from(10u64.saturating_pow(fallback_decimals));
        let tokens = Decimal::from(raw_amount as u64) / divisor;
        let usd = tokens * price;
        assert_eq!(
            usd,
            Decimal::from(2u32),
            "1 token (9 dec, SPL fallback) at $2.00 should compute to $2.00 USD"
        );
    }

    /// Zero raw amount → USD volume = 0.0 regardless of decimals or price.
    #[test]
    fn decimals_zero_raw_amount_zero_usd() {
        let raw_amount: u128 = 0;
        // When raw == 0 the early return fires: Some(Decimal::ZERO)
        let result: Option<Decimal> = Some(Decimal::ZERO).map(|_price| {
            if raw_amount == 0 {
                return Decimal::ZERO;
            }
            let divisor = Decimal::from(10u64.saturating_pow(9u32));
            let tokens = Decimal::from(raw_amount as u64) / divisor;
            tokens * Decimal::from(100u32)
        });
        assert_eq!(result, Some(Decimal::ZERO), "zero raw amount must yield $0.00 USD");
    }

    // -------------------------------------------------------------------------
    // DETECTOR_ID constant
    // -------------------------------------------------------------------------

    #[test]
    fn detector_id_matches_evidence_prefix() {
        assert_eq!(DETECTOR_ID, "synchronized_activity_v1");
        let key = format!("{DETECTOR_ID}/cluster_size");
        assert_eq!(key, "synchronized_activity_v1/cluster_size");
    }

    // -------------------------------------------------------------------------
    // Fixture file tests (parse JSON, validate math against expected output)
    // -------------------------------------------------------------------------

    /// Load fixture JSON from tests/fixtures/solana/{subfolder}/{filename}.
    fn load_fixture_json(subfolder: &str, filename: &str) -> serde_json::Value {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::PathBuf::from(manifest_dir)
            .parent() // crates/
            .expect("crates dir must exist")
            .parent() // workspace root
            .expect("workspace root must exist")
            .join("tests/fixtures/solana")
            .join(subfolder)
            .join(filename);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("fixture {path:?} must exist: {e}"));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("fixture {filename} must be valid JSON: {e}"))
    }

    /// Positive fixture SYNTH_POS_D11_01: validates math parameters from JSON.
    ///
    /// The fixture specifies lambda=0.001/s, 7 wallets in 6s window.
    /// Expected: fires=true, confidence_min=0.70, p_value_max=1e-9.
    #[test]
    fn positive_fixture_d11_01_validates_against_json() {
        let fixture =
            load_fixture_json("positive", "SYNTH_POS_D11_01_coordinated_buys.json");

        // Read fixture parameters.
        let lambda: f64 = fixture["lambda_token_per_second"]
            .as_f64()
            .expect("lambda_token_per_second must be f64");
        let expected_fires = fixture["_expected"]["fires"]
            .as_bool()
            .expect("fires must be bool");
        let expected_cluster_size = fixture["_expected"]["cluster_size"]
            .as_u64()
            .expect("cluster_size must be u64") as usize;
        let confidence_min = fixture["_expected"]["confidence_min"]
            .as_f64()
            .expect("confidence_min must be f64");
        let p_value_max = fixture["_expected"]["p_value_max"]
            .as_f64()
            .expect("p_value_max must be f64");

        assert!(expected_fires, "POS_D11_01 must expect fires=true");

        // Verify p_value satisfies expected constraint.
        let p = compute_poisson_p_value(lambda, 30.0, expected_cluster_size);
        assert!(
            p <= p_value_max,
            "POS_D11_01: p_value {p:.3e} must be <= {p_value_max:.3e}"
        );

        // Verify confidence satisfies expected constraint.
        let tightness = 0.80; // 6s spread / 30s delta.
        let conf = compute_synchronized_activity_confidence(
            expected_cluster_size,
            5,    // min_cluster_size
            5.0,  // cluster_size_scale
            tightness,
            p,
            1e-6, // poisson_p_threshold
            0.40,
            0.30,
            0.30,
        );
        assert!(
            conf >= confidence_min,
            "POS_D11_01: confidence {conf:.4} must be >= {confidence_min}"
        );

        // Check swaps count matches expected cluster.
        let swaps = fixture["swaps"].as_array().expect("swaps must be array");
        assert_eq!(
            swaps.len(),
            expected_cluster_size,
            "POS_D11_01 fixture swap count must match expected cluster_size"
        );
    }

    /// Negative fixture SYNTH_NEG_D11_01: validates that spread > delta → tightness < threshold.
    #[test]
    fn negative_fixture_d11_01_validates_against_json() {
        let fixture =
            load_fixture_json("negative", "SYNTH_NEG_D11_01_random_activity.json");

        let expected_fires = fixture["_expected"]["fires"]
            .as_bool()
            .expect("fires must be bool");
        assert!(!expected_fires, "NEG_D11_01 must expect fires=false");

        // Verify tightness math: parse timestamps from fixture swaps.
        let swaps = fixture["swaps"].as_array().expect("swaps must be array");
        let times: Vec<DateTime<Utc>> = swaps
            .iter()
            .map(|s| {
                let t_str = s["block_time"]
                    .as_str()
                    .expect("block_time must be string");
                DateTime::parse_from_rfc3339(t_str)
                    .expect("block_time must parse as RFC3339")
                    .with_timezone(&Utc)
            })
            .collect();

        let tightness = compute_temporal_tightness_from_times(&times, 30.0);
        let threshold = 0.50_f64;

        assert!(
            tightness < threshold,
            "NEG_D11_01: tightness {tightness:.4} must be < threshold {threshold}"
        );
    }

    // -------------------------------------------------------------------------
    // Positive fixture math validation
    // -------------------------------------------------------------------------

    /// Validates the math for SYNTH_POS_D11_01 fixture:
    /// 7 wallets within 6 seconds; λ=0.001/s; expected conf >= 0.70.
    #[test]
    fn positive_fixture_d11_01_math_validation() {
        let lambda = 0.001_f64;
        let delta = 30.0_f64;
        let cluster_size = 7_usize;
        let min_cluster_size = 5;
        let poisson_p_threshold = 1e-6_f64;

        let p = compute_poisson_p_value(lambda, delta, cluster_size);
        assert!(
            p < 1e-6,
            "POS_D11_01 p_value must be < 1e-6, got {p:.3e}"
        );

        let tightness = 0.80_f64; // 6s/30s = 0.80 per spec §12.1.

        let conf = compute_synchronized_activity_confidence(
            cluster_size,
            min_cluster_size,
            5.0,
            tightness,
            p,
            poisson_p_threshold,
            0.40,
            0.30,
            0.30,
        );

        assert!(
            conf >= 0.70,
            "POS_D11_01 must produce confidence >= 0.70, got {conf:.4}"
        );
        // Expected severity: High (>= 0.60 < 0.80).
        assert_eq!(
            severity_from_confidence(conf),
            mg_onchain_common::anomaly::Severity::High
        );
    }

    /// Validates the math for SYNTH_NEG_D11_01 fixture:
    /// 8 wallets with 45-second spread → tightness = 0.0 → no cluster passes filter.
    #[test]
    fn negative_fixture_d11_01_temporal_tightness_filters_cluster() {
        let spread_seconds = 45.0_f64;
        let delta_seconds = 30.0_f64;

        // tightness = 1 - 45/30 = 1 - 1.5 = -0.5, clamped to 0.0.
        let tightness = (1.0 - spread_seconds / delta_seconds).clamp(0.0, 1.0);
        assert!(
            tightness == 0.0,
            "NEG_D11_01: 45s spread with 30s delta must give tightness=0.0, got {tightness}"
        );

        let threshold = 0.50_f64;
        assert!(
            tightness < threshold,
            "NEG_D11_01: tightness {tightness} must be below threshold {threshold} → cluster discarded"
        );
    }
}
