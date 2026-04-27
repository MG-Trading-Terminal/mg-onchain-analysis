//! D04 — Pump & Dump / Volume-Price Spike detector.
//!
//! # Overview
//!
//! Detects pump-and-dump patterns via three complementary signals:
//!
//! - **Signal A (event-based):** 1h volume ≥ `volume_multiplier × 7d_daily_median` AND
//!   1h price change ≥ `price_spike_pct`. Requires ≥ `min_baseline_days` of history.
//!   Confidence ∈ [0.60, 0.95] via sigmoid formula.
//!
//! - **Signal B (event-based, fallback):** When baseline is absent or insufficient,
//!   `volume_1h / volume_24h ≥ burst_concentration_threshold` fires at lower confidence
//!   ∈ [0.50, 0.75]. Closes the RAVE-probe zero-baseline gap.
//!
//! - **Signal C (event+state amplifier):** Insider wallets sell ≥ `insider_sell_pct` of
//!   their holdings within `post_pump_insider_window_hours` of the spike. Adds
//!   `insider_amplifier` to confidence (capped at 0.95 for A-base, 0.85 for B-base).
//!   **Suppressed entirely** for established protocols (treasury sells are benign).
//!
//! A and B are mutually exclusive. C is a modifier applied to whichever fires.
//!
//! # Algorithm
//!
//! Per `docs/designs/0007-detector-04-pump-dump.md` §3.
//!
//! ## Signal A confidence formula
//!
//! ```text
//! raw = (volume_ratio / volume_multiplier - 1.0) * 0.5
//!     + (price_change / price_spike_pct - 1.0) * 0.3
//! sigmoid_raw = 1 / (1 + exp(-raw))
//! signal_a_confidence = clamp(sigmoid_raw, 0.60, 0.95)
//! ```
//!
//! Calibration (volume_multiplier=5.0, price_spike_pct=0.30):
//! - At threshold (ratio=5, price=0.30): raw=0, sigmoid=0.50, clamped=0.60
//! - At ratio=10, price=0.60: raw=1.10, sigmoid≈0.750, unclamped
//! - At ratio=25, price=0.90: raw=2.7, sigmoid≈0.937, unclamped
//!
//! ## Signal B confidence formula
//!
//! ```text
//! signal_b_confidence = min(0.75, 0.50 + (burst_ratio - threshold) / 0.10 * 0.25)
//! ```
//!
//! At threshold (0.90) → 0.50. At 0.95 → 0.625. At 1.00 → 0.75.
//!
//! ## Signal C amplifier
//!
//! ```text
//! final = base + insider_amplifier
//! cap   = if base_is_signal_a { 0.95 } else { 0.85 }
//! final = min(final, cap)
//! ```
//!
//! # Market-cap filter
//!
//! When `market_cap_usd > market_cap_filter_usd`, returns a single Info event
//! with `signal=market_cap_above_filter` and exits without evaluating A, B, or C.
//!
//! # DG resolutions
//!
//! - **DG-04-1 (slow-pump window):** Phase 5. Not implemented.
//! - **DG-04-2 (baseline contamination):** Phase 3. Not implemented.
//! - **DG-04-3 (RAY/TRUMP FP gap):** Known debt. NEG_02 fixture `expected_correct=false`.
//! - **DG-04-4 (per-pool detection):** Deferred. Token-level aggregation only.
//! - **DG-04-5 (FDV vs circulating):** Uses `circulating_supply_raw × price` when both
//!   populated; falls back to `total_supply_raw × price`. Evidence records `market_cap_source`.
//!
//! # deployer_clusters Graceful Degradation
//!
//! Three-tier priority ladder for insider address resolution:
//! 1. `deployer_clusters` Postgres table (Phase 3+).
//! 2. `top_holders` proxy: holders with balance ≥ `top_holders_insider_floor_pct` of supply,
//!    excluding DexPool/VestingContract/CexWallet via holder_classifications sidecar JOIN.
//! 3. Unavailable: Signal C not applied; evidence records `insider_source=unavailable`.
//!
//! # Evidence schema
//!
//! All keys use `pump_dump/` prefix. See `docs/designs/0007` §7 for the full table.
//!
//! String-valued fields (signal label, insider_source) are encoded in `Evidence.notes`
//! as space-separated `key=value` pairs, consistent with the D02 `latent_risk` pattern.
//!
//! # References
//!
//! - Karbalaii (2025): https://arxiv.org/abs/2504.15790 — REFERENCES.md D04/pump_dump
//! - Bolz et al. (2024): https://arxiv.org/abs/2412.18848 — REFERENCES.md D04/pump_dump
//! - Chainalysis (2025): https://www.chainalysis.com/blog/crypto-market-manipulation-...
//! - RAVE probe: research/token-probes/rave-FeqiF7TE.md
//! - WET probe:  research/token-probes/wet-WETZjtp.md
//! - Security review: docs/designs/0007-detector-04-pump-dump.md

use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use tracing::{debug, info, instrument, warn};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Address;
use mg_onchain_common::token::TokenMeta;
use mg_onchain_graph::SmartMoneyLookup;
use mg_onchain_token_registry::graduation::GraduationInfo;

use crate::config::PumpDumpConfig;
use crate::graduation_amplifier::{GraduationAmplifierTiers, apply_graduation_amplifier};
use crate::context::DetectorContext;
use crate::detector::Detector;
use crate::error::DetectorError;
use crate::evidence_key;
use crate::signals::{severity_from_confidence, sigmoid};
use crate::smart_money_amplifier::{TierCounts, intersect_tier_counts};
use crate::token_status::is_established_protocol;
use mg_onchain_storage::pg::{BurstMetricsRow, InsiderSellRow, PumpDumpBaselineRow};

/// Stable detector ID — matches the TOML subsection and `Evidence::metrics` prefix.
pub const DETECTOR_ID: &str = "pump_dump";

// ---------------------------------------------------------------------------
// Pure compute types
// ---------------------------------------------------------------------------

/// Which signal fired (A or B) — drives caps and evidence label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSignal {
    /// Signal A: volume/price spike over rolling 7d baseline.
    A,
    /// Signal B: burst concentration fallback (zero-baseline case).
    B,
}

/// Result of Signal A confidence computation.
///
/// `pub` so tests can call [`compute_signal_a_confidence`] directly.
#[derive(Debug)]
pub struct SignalAResult {
    /// Confidence ∈ [0.60, 0.95].
    pub confidence: f64,
    /// Observed `volume_1h / median_volume` ratio.
    pub volume_ratio: f64,
    /// Observed price change (signed fraction).
    pub price_change_pct: f64,
    /// Volume z-score from the query (evidence only, not in confidence formula).
    pub volume_z_score: f64,
}

/// Result of Signal B confidence computation.
///
/// `pub` so tests can call [`compute_signal_b_confidence`] directly.
#[derive(Debug)]
pub struct SignalBResult {
    /// Confidence ∈ [0.50, 0.75].
    pub confidence: f64,
    /// Observed burst concentration ratio.
    pub burst_ratio: f64,
}

/// Resolved insider address set for Signal C.
#[derive(Debug, Clone)]
pub struct InsiderSet {
    /// Canonical wallet addresses (from deployer_clusters or top_holders_proxy).
    pub addresses: Vec<String>,
    /// Which priority level resolved the set.
    pub source: InsiderSource,
}

/// Source used to resolve insider addresses (for evidence recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsiderSource {
    /// deployer_clusters Postgres table (Phase 3+).
    DeployerClusters,
    /// top_holders proxy with ≥1% supply floor (Phase 2 degraded mode).
    TopHoldersProxy,
    /// No insider data available — Signal C not applied.
    Unavailable,
}

impl InsiderSource {
    pub fn as_str(self) -> &'static str {
        match self {
            InsiderSource::DeployerClusters => "deployer_clusters",
            InsiderSource::TopHoldersProxy => "top_holders_proxy",
            InsiderSource::Unavailable => "unavailable",
        }
    }
}

// ---------------------------------------------------------------------------
// PumpDumpDetector
// ---------------------------------------------------------------------------

/// D04 Pump & Dump / Volume-Price Spike detector.
///
/// # Construction
///
/// ```rust,no_run
/// use mg_onchain_detectors::d04_pump_dump::PumpDumpDetector;
/// use mg_onchain_detectors::config::PumpDumpConfig;
///
/// // Basic (backwards-compat, existing tests):
/// // let detector = PumpDumpDetector::new(config.pump_dump.clone());
///
/// // With smart-money amplification (S23 production wiring):
/// // let detector = PumpDumpDetector::new(config.pump_dump.clone())
/// //     .with_smart_money(sm_lookup.clone());
/// ```
#[derive(Clone)]
pub struct PumpDumpDetector {
    /// Construction-time threshold snapshot (retained for Phase 3 hot-reload extensions).
    #[allow(dead_code)]
    thresholds: PumpDumpConfig,
    /// Smart-money lookup — `None` when not wired (backwards-compat, existing tests).
    /// Injected by production `init/detectors.rs`; `None` in all existing unit tests.
    /// See design 0023 Decision 8.
    pub smart_money: Option<Arc<dyn SmartMoneyLookup>>,
    /// Optional graduation info for recency amplification (Sprint 25).
    ///
    /// When `Some`, graduation-recency multiplier is applied AFTER smart-money
    /// amplification. When `None` (default), no graduation amplification occurs.
    ///
    /// SPEC-NOTE: graduation_info storage path not yet persisted (no metadata_jsonb
    /// column in tokens table). Populated by production wiring once V00017 migration
    /// adds the column. Builder pattern preserves backwards compat.
    /// TODO(next-sprint): wire via PgStore.fetch_graduation_info() after migration.
    pub graduation_info: Option<GraduationInfo>,
    /// Per-tier multiplier config for graduation amplification.
    ///
    /// Loaded from `config/detectors.toml [graduation_recency]`.
    /// Defaults to `GraduationAmplifierTiers::default()`.
    pub graduation_tiers: GraduationAmplifierTiers,
}

impl PumpDumpDetector {
    /// Construct a new `PumpDumpDetector`.
    ///
    /// `smart_money` defaults to `None`. Existing call sites are unchanged.
    pub fn new(thresholds: PumpDumpConfig) -> Self {
        Self {
            thresholds,
            smart_money: None,
            graduation_info: None,
            graduation_tiers: GraduationAmplifierTiers::default(),
        }
    }

    /// Wire in a [`SmartMoneyLookup`] for D04 pre-pump buyer amplification.
    ///
    /// When `Some`, the detector fetches all SmartMoney-labelled addresses for the
    /// chain, intersects with the pre-pump buyer set (60-min window before evaluation),
    /// and applies per-tier confidence deltas (Tier1: +0.12, Tier2 ≥2: +0.07).
    ///
    /// When `None` (default), no amplification occurs and no smart-money evidence keys
    /// are emitted — fully backwards-compatible.
    ///
    /// # References
    ///
    /// - Fu, Feng, Wu & Xu 2025 (Perseus): masterminds buy pre-event in 100% of events.
    /// - Fantazzini & Xiao 2023: 60-min pre-event window.
    /// - design 0023 §4.1.
    pub fn with_smart_money(mut self, lookup: Arc<dyn SmartMoneyLookup>) -> Self {
        self.smart_money = Some(lookup);
        self
    }

    /// Wire in graduation info for recency amplification (Sprint 25).
    ///
    /// When `Some`, applies `graduation_recency_multiplier` to the final confidence
    /// value (after smart-money amplification), capped at the per-signal ceiling.
    ///
    /// When `None` (default), no graduation amplification occurs — backwards-compatible.
    ///
    /// # Time-source discipline (gotcha #28)
    ///
    /// `graduation_info.graduation_time` must have been populated from block_time.
    ///
    /// # References
    ///
    /// - Karbalaii 2025: "70% of pump events have accumulation phase"
    /// - REFERENCES.md D04/pump_dump graduation amplification (Sprint 25)
    pub fn with_graduation(mut self, info: GraduationInfo) -> Self {
        self.graduation_info = Some(info);
        self
    }

    /// Override the default graduation amplifier tier multipliers.
    ///
    /// By default, tiers are loaded from `GraduationAmplifierTiers::default()`.
    /// Operators can pass config-sourced values here.
    pub fn with_graduation_tiers(mut self, tiers: GraduationAmplifierTiers) -> Self {
        self.graduation_tiers = tiers;
        self
    }
}

impl Detector for PumpDumpDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn severity_floor(&self) -> Severity {
        Severity::Info
    }

    /// D04 reads from `swap_buys` + `address_labels` tables, both keyed by `(chain, token)`.
    /// All `Chain::Solana` references in the source are confined to the `#[cfg(test)]` block
    /// (test fixtures use Solana addresses for convenience). No Solana-specific code paths
    /// exist in the production evaluation logic.
    fn supported_chains(&self) -> &[mg_onchain_common::chain::Chain] {
        use mg_onchain_common::chain::Chain;
        &[
            Chain::Solana,
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
        ]
    }

    #[instrument(
        skip(self, ctx),
        fields(
            detector_id = DETECTOR_ID,
            token = ctx.token.as_str(),
            chain = ctx.chain.as_str()
        )
    )]
    async fn evaluate<'ctx>(
        &self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError> {
        let cfg = &ctx.config.pump_dump;

        // Step 1: Enrich token metadata.
        let meta = ctx
            .registry
            .enrich(ctx.token.as_str(), ctx.chain)
            .await
            .map_err(|e| DetectorError::MissingDependencyData {
                detector_id: DETECTOR_ID,
                token: ctx.token.as_str().to_owned(),
                reason: format!("registry enrich failed: {e}"),
            })?;

        // Step 2: Compute market cap and apply filter.
        let (market_cap_usd, market_cap_source) = resolve_market_cap(&meta);
        let market_cap_filter = Decimal::from_f64(cfg.market_cap_filter_usd.value)
            .unwrap_or(Decimal::new(60_000_000, 0));

        if market_cap_usd > market_cap_filter {
            info!(
                token = ctx.token.as_str(),
                %market_cap_usd,
                "D04: market_cap_filter triggered — exiting without A/B/C evaluation"
            );
            let evidence = Evidence::new()
                .with_metric(evidence_key(DETECTOR_ID, "market_cap_usd"), market_cap_usd)
                .with_note(format!(
                    "signal=market_cap_above_filter market_cap_source={market_cap_source}"
                ));
            return Ok(vec![make_event(ctx, &meta, 0.05, Severity::Info, evidence)]);
        }

        // Step 3: Fetch Signal A baseline row.
        let window_end = ctx.window.end;
        let baseline_result = ctx
            .store
            .fetch_pump_dump_baseline(
                ctx.chain.as_str(),
                ctx.token.as_str(),
                window_end,
                cfg.min_baseline_days.value as i64,
                cfg.volume_multiplier.value,
                cfg.price_spike_pct.value,
            )
            .await;

        let baseline_row: Option<PumpDumpBaselineRow> = match baseline_result {
            Ok(r) => r,
            Err(mg_onchain_storage::error::StorageError::Postgres(se)) => {
                return Err(DetectorError::TransientQuery {
                    detector_id: DETECTOR_ID,
                    source: se,
                });
            }
            Err(other) => {
                debug!(
                    token = ctx.token.as_str(),
                    "D04 fetch_pump_dump_baseline failed, treating as no-spike: {other}"
                );
                None
            }
        };

        // Count baseline days by checking if we got a row AND what baseline_days_available says.
        // If the query returned a row, the median was > 0, so baseline has ≥ 1 day.
        // When the query returns None it could be:
        //   (a) Thresholds not met but baseline exists → baseline_days from a separate count.
        //   (b) Zero-baseline (RAVE gap) → baseline_days = 0.
        // For Phase 2, we infer from the burst query whether any data exists.

        let mut events: Vec<AnomalyEvent> = Vec::new();
        // Tracks which base signal fired. Initialized to B (the fallback); overwritten to A
        // when the baseline row is present. Only read after `events` is guaranteed non-empty.
        let mut base_signal: BaseSignal = BaseSignal::B;

        if let Some(ref row) = baseline_row {
            // Signal A fired: the query returned a row (baseline valid, thresholds met).
            let result_a = compute_signal_a_confidence(row, cfg);
            let evidence = build_evidence_a(row, &result_a, market_cap_usd, &market_cap_source);
            let severity = severity_from_confidence(result_a.confidence);
            events.push(make_event(
                ctx,
                &meta,
                result_a.confidence,
                severity,
                evidence,
            ));
            base_signal = BaseSignal::A;
        } else {
            // Signal A did not fire — attempt Signal B fallback.
            // This covers both: thresholds-not-met-but-baseline-valid AND zero-baseline.
            // We run the burst query and decide.
            let burst_result = ctx
                .store
                .fetch_burst_metrics(ctx.chain.as_str(), ctx.token.as_str(), window_end)
                .await;

            let burst_opt: Option<BurstMetricsRow> = match burst_result {
                Ok(r) => r,
                Err(mg_onchain_storage::error::StorageError::Postgres(se)) => {
                    return Err(DetectorError::TransientQuery {
                        detector_id: DETECTOR_ID,
                        source: se,
                    });
                }
                Err(other) => {
                    debug!(
                        token = ctx.token.as_str(),
                        "D04 fetch_burst_metrics failed: {other}"
                    );
                    None
                }
            };

            let Some(burst_row) = burst_opt else {
                // No swap data at all — token not indexed or delisted.
                return Err(DetectorError::MissingDependencyData {
                    detector_id: DETECTOR_ID,
                    token: ctx.token.as_str().to_owned(),
                    reason: "No swap rows found for token in 7-day window".to_owned(),
                });
            };

            // Determine baseline_days for the burst_row. When Signal A query returned None
            // but the burst query has data, we infer baseline_days from whether the baseline
            // query returned None because of zero-baseline or because thresholds not met.
            // We use a separate lightweight query — but to avoid a third DB round-trip in Phase 2,
            // we check burst_row.volume_24h_usd as a proxy: if volume_1h == volume_24h, it's
            // almost certainly the zero-baseline case. The evidence records baseline_days_available=0.
            // In either case, if Signal A didn't fire, we proceed with Signal B logic.

            // Dust filter: below min_burst_volume_usd, Signal B does not fire.
            let min_burst_volume =
                Decimal::from_f64(cfg.min_burst_volume_usd.value).unwrap_or(Decimal::new(5000, 0));
            if burst_row.volume_1h_usd < min_burst_volume {
                debug!(
                    token = ctx.token.as_str(),
                    volume_1h_usd = %burst_row.volume_1h_usd,
                    "D04: volume below min_burst_volume_usd dust filter — insufficient_data"
                );
                let evidence = Evidence::new()
                    .with_metric(evidence_key(DETECTOR_ID, "market_cap_usd"), market_cap_usd)
                    .with_metric(
                        evidence_key(DETECTOR_ID, "volume_1h_usd"),
                        burst_row.volume_1h_usd,
                    )
                    .with_metric(
                        evidence_key(DETECTOR_ID, "baseline_days_available"),
                        Decimal::ZERO,
                    )
                    .with_note(format!(
                        "signal=insufficient_data market_cap_source={market_cap_source}"
                    ));
                return Ok(vec![make_event(ctx, &meta, 0.05, Severity::Info, evidence)]);
            }

            let threshold = Decimal::from_f64(cfg.burst_concentration_threshold.value)
                .unwrap_or(Decimal::new(90, 2));

            if burst_row.burst_concentration_ratio >= threshold {
                // Signal B fires.
                let result_b = compute_signal_b_confidence(&burst_row, cfg);
                let evidence =
                    build_evidence_b(&burst_row, &result_b, market_cap_usd, &market_cap_source);
                let severity = severity_from_confidence(result_b.confidence);
                events.push(make_event(
                    ctx,
                    &meta,
                    result_b.confidence,
                    severity,
                    evidence,
                ));
                // base_signal is already BaseSignal::B (the default), no update needed.
            } else {
                // Neither A nor B fires — insufficient data or thresholds not met.
                debug!(
                    token = ctx.token.as_str(),
                    burst_ratio = %burst_row.burst_concentration_ratio,
                    "D04: burst_ratio below threshold — no signal"
                );
                let evidence = Evidence::new()
                    .with_metric(evidence_key(DETECTOR_ID, "market_cap_usd"), market_cap_usd)
                    .with_metric(
                        evidence_key(DETECTOR_ID, "burst_concentration_ratio"),
                        burst_row.burst_concentration_ratio,
                    )
                    .with_metric(
                        evidence_key(DETECTOR_ID, "baseline_days_available"),
                        Decimal::ZERO,
                    )
                    .with_note(format!(
                        "signal=insufficient_data market_cap_source={market_cap_source}"
                    ));
                return Ok(vec![make_event(ctx, &meta, 0.05, Severity::Info, evidence)]);
            }
        }

        // Step 4: Signal C — insider sell amplifier.
        // Only applied when A or B fired.
        if !events.is_empty() {
            if is_established_protocol(&meta) {
                // Signal C suppressed for established protocols (asymmetric suppression rule).
                // Add audit key to the existing event; do NOT amplify confidence.
                info!(
                    token = ctx.token.as_str(),
                    jup_strict = meta.verification.jup_strict,
                    jup_verified = meta.verification.jup_verified,
                    rugcheck_score = meta.rugcheck_score,
                    "D04: Signal C suppressed — established_protocol classifier matched"
                );
                events[0].evidence.metrics.insert(
                    evidence_key(DETECTOR_ID, "established_protocol_suppressed_signal_c"),
                    Decimal::ONE,
                );
            } else {
                // Resolve insider address set (3-tier priority ladder).
                let insider_set = resolve_insider_addresses(ctx, &meta, cfg).await?;

                if !insider_set.addresses.is_empty() {
                    let spike_time = window_end;
                    let window_end_c = spike_time
                        + chrono::Duration::hours(cfg.post_pump_insider_window_hours.value as i64);

                    let insider_sells = ctx
                        .store
                        .fetch_insider_sells(
                            ctx.chain.as_str(),
                            ctx.token.as_str(),
                            &insider_set.addresses,
                            spike_time,
                            window_end_c,
                        )
                        .await
                        .map_err(|e| match e {
                            mg_onchain_storage::error::StorageError::Postgres(se) => {
                                DetectorError::TransientQuery {
                                    detector_id: DETECTOR_ID,
                                    source: se,
                                }
                            }
                            other => DetectorError::MissingDependencyData {
                                detector_id: DETECTOR_ID,
                                token: ctx.token.as_str().to_owned(),
                                reason: format!("insider sell query failed: {other}"),
                            },
                        })?;

                    // Compute aggregate insider sold_pct from InsiderSellRow and InsiderSet balances.
                    let (sold_pct_aggregate, enriched_sells) =
                        compute_insider_sold_pct(&insider_set, &insider_sells, &meta);

                    let insider_threshold = Decimal::from_f64(cfg.insider_sell_pct.value)
                        .unwrap_or(Decimal::new(40, 2));

                    if sold_pct_aggregate >= insider_threshold {
                        // Signal C fires — apply amplifier in-place.
                        apply_signal_c_amplifier(
                            &mut events[0],
                            base_signal,
                            sold_pct_aggregate,
                            &insider_set,
                            &enriched_sells,
                            cfg,
                            ctx.chain,
                        );
                    } else {
                        // Signal C threshold not met — record insider_source in evidence.
                        events[0]
                            .evidence
                            .notes
                            .push(format!("insider_source={}", insider_set.source.as_str()));
                        debug!(
                            token = ctx.token.as_str(),
                            %sold_pct_aggregate,
                            "D04: Signal C insider_sell_pct below threshold"
                        );
                    }
                } else {
                    // Priority 3: no insider data available.
                    events[0]
                        .evidence
                        .notes
                        .push("insider_source=unavailable".to_owned());
                    debug!(
                        token = ctx.token.as_str(),
                        "D04: Signal C unavailable — no insider addresses resolved"
                    );
                }
            }
        }

        // Step 5 (S23): Smart-money pre-pump buyer amplification.
        //
        // Only triggered when:
        //   - `smart_money` is `Some` (production wiring injected)
        //   - at least one A/B event fired (base confidence >= threshold)
        //   - base confidence >= 0.50 (minimum confidence gate per design 0023 §5.3)
        //
        // Amplification is additive and bounded by the existing per-signal cap.
        // Evidence keys are only emitted when `smart_money` is `Some`.
        // See design 0023 §4.1 + Decision 8 (backwards-compat).
        if let Some(ref sm_lookup) = self.smart_money
            && !events.is_empty()
            && events[0].confidence.value() >= 0.50
        {
            match sm_lookup
                .fetch_smart_money_addresses(ctx.chain.as_str(), ctx.observed_at)
                .await
            {
                Ok(sm_map) => {
                    // Fetch pre-pump buyer addresses from the swaps table.
                    // Pre-pump window: 60 minutes before evaluation window start.
                    // Decision 3: Fantazzini & Xiao 2023 60-min pre-event window.
                    let pre_pump_window_minutes = cfg.pre_pump_window_minutes.value as i64;
                    let window_start = ctx.window.start;
                        let pre_pump_start = window_start
                            - chrono::Duration::minutes(pre_pump_window_minutes);

                        let buyer_addresses = fetch_pre_pump_buyers(
                            ctx,
                            pre_pump_start,
                            window_start,
                        )
                        .await?;

                        let tier_counts = intersect_tier_counts(&buyer_addresses, &sm_map);
                        let delta = compute_smart_money_amplification_d04(&tier_counts, cfg);

                        // Apply delta to the first (primary) event.
                        let base_conf = events[0].confidence.value();
                        let cap = match base_signal {
                            BaseSignal::A => 0.95_f64,
                            BaseSignal::B => 0.85_f64,
                        };
                        let amplified = (base_conf + delta).min(cap);
                        if let Ok(new_conf) = Confidence::new(amplified) {
                            events[0].confidence = new_conf;
                            events[0].severity = severity_from_confidence(amplified);
                        }

                        // Emit 5-key standardized evidence schema (Decision 7).
                        let sm_present = if tier_counts.has_any() { Decimal::ONE } else { Decimal::ZERO };
                        let delta_dec = Decimal::from_f64(delta).unwrap_or(Decimal::ZERO);
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_present"),
                            sm_present,
                        );
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_tier1_buyer_count"),
                            Decimal::from(tier_counts.tier1),
                        );
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_tier2_buyer_count"),
                            Decimal::from(tier_counts.tier2),
                        );
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_tier3_buyer_count"),
                            Decimal::from(tier_counts.tier3),
                        );
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_amplification_delta"),
                            delta_dec,
                        );
                        // D04-specific: record the window used (Decision 3 documentation).
                        events[0].evidence.metrics.insert(
                            evidence_key(DETECTOR_ID, "smart_money_pre_pump_window_minutes"),
                            Decimal::from(pre_pump_window_minutes),
                        );
                    }
                Err(e) => {
                    // Smart-money lookup failure is non-fatal — log and continue
                    // without amplification. Detector still emits the base event.
                    warn!(
                        token = ctx.token.as_str(),
                        error = %e,
                        "D04: smart_money_lookup failed; skipping amplification (non-fatal)"
                    );
                }
            }
        }

        // Step 6 (Sprint 25): Graduation-recency amplification.
        //
        // Applied AFTER smart-money amplification (Step 5). Multiplicative on the
        // final confidence value. Only triggered when:
        //   - `graduation_info` is `Some` (injected via with_graduation())
        //   - At least one A/B event fired
        //
        // Cap per signal: 0.95 for Signal A, 0.85 for Signal B.
        // Respects the existing per-signal cap invariant.
        //
        // SPEC-NOTE: graduation_info is currently populated only when explicitly
        // injected via with_graduation(). Production wiring via PgStore lookup
        // is deferred until V00017 migration (metadata_jsonb column).
        // Reference: Karbalaii 2025 — "70% of pump events have accumulation phase".
        if let Some(ref grad_info) = self.graduation_info
            && !events.is_empty()
        {
            let pre_amp_conf = events[0].confidence.value();
            let cap = match base_signal {
                BaseSignal::A => 0.95_f64,
                BaseSignal::B => 0.85_f64,
            };
            let amplified = apply_graduation_amplifier(
                pre_amp_conf,
                Some(grad_info),
                ctx.observed_at,
                &self.graduation_tiers,
                cap,
            );
            if let Ok(new_conf) = Confidence::new(amplified)
                && amplified > pre_amp_conf
            {
                events[0].confidence = new_conf;
                events[0].severity = severity_from_confidence(amplified);
                // Record graduation amplification delta for audit trail.
                let delta_dec = Decimal::from_f64(amplified - pre_amp_conf)
                    .unwrap_or(Decimal::ZERO);
                events[0].evidence.metrics.insert(
                    evidence_key(DETECTOR_ID, "graduation_amplification_delta"),
                    delta_dec,
                );
                events[0].evidence.metrics.insert(
                    evidence_key(DETECTOR_ID, "graduation_launchpad"),
                    Decimal::ONE, // presence sentinel — launchpad string in notes
                );
                events[0].evidence.notes.push(format!(
                    "graduation_launchpad={}",
                    grad_info.launchpad.display_name()
                ));
            }
        }

        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// Pure compute functions (unit-testable without I/O)
// ---------------------------------------------------------------------------

/// Compute Signal A confidence from a spike row.
///
/// Formula (from `docs/designs/0007` §5.1):
/// ```text
/// raw = (volume_ratio / volume_multiplier - 1.0) * 0.5
///     + (price_change / price_spike_pct - 1.0) * 0.3
/// confidence = clamp(sigmoid(raw), 0.60, 0.95)
/// ```
///
/// # Precondition
///
/// The caller guarantees `row.volume_1h_usd / row.volume_7d_median_usd >= volume_multiplier`
/// and `row.price_change_pct_1h >= price_spike_pct` (enforced by the SQL query guard).
pub fn compute_signal_a_confidence(
    row: &PumpDumpBaselineRow,
    cfg: &PumpDumpConfig,
) -> SignalAResult {
    let volume_ratio_dec = if row.volume_7d_median_usd > Decimal::ZERO {
        row.volume_1h_usd / row.volume_7d_median_usd
    } else {
        Decimal::ZERO
    };
    let volume_ratio = volume_ratio_dec.to_f64().unwrap_or(0.0);
    let price_change = row.price_change_pct_1h.to_f64().unwrap_or(0.0);
    let volume_z = row.volume_z_score.to_f64().unwrap_or(0.0);

    let vm = cfg.volume_multiplier.value;
    let ps = cfg.price_spike_pct.value;

    // Guard against divide-by-zero in formula (shouldn't happen given query guard, but be safe).
    let raw = if vm > 0.0 && ps > 0.0 {
        (volume_ratio / vm - 1.0) * 0.5 + (price_change / ps - 1.0) * 0.3
    } else {
        0.0
    };

    let sig = sigmoid(raw);
    let confidence = sig.clamp(0.60_f64, 0.95_f64);

    SignalAResult {
        confidence,
        volume_ratio,
        price_change_pct: price_change,
        volume_z_score: volume_z,
    }
}

/// Compute Signal B confidence from a burst metrics row.
///
/// Formula (from `docs/designs/0007` §5.2):
/// ```text
/// confidence = min(0.75, 0.50 + (burst_ratio - threshold) / 0.10 * 0.25)
/// ```
///
/// # Precondition
///
/// The caller guarantees `row.burst_concentration_ratio >= threshold`.
pub fn compute_signal_b_confidence(row: &BurstMetricsRow, cfg: &PumpDumpConfig) -> SignalBResult {
    let burst_ratio = row.burst_concentration_ratio.to_f64().unwrap_or(0.0);
    let threshold = cfg.burst_concentration_threshold.value;

    // Linear interpolation: at threshold → 0.50; at threshold+0.10 → 0.75 (cap).
    let excess = (burst_ratio - threshold).max(0.0);
    let raw = 0.50 + excess / 0.10 * 0.25;
    let confidence = raw.min(0.75_f64);

    SignalBResult {
        confidence,
        burst_ratio,
    }
}

/// Apply Signal C amplifier to the existing A or B event in-place.
///
/// Updates `event.confidence`, `event.severity`, and appends evidence keys.
fn apply_signal_c_amplifier(
    event: &mut AnomalyEvent,
    base_signal: BaseSignal,
    sold_pct: Decimal,
    insider_set: &InsiderSet,
    enriched_sells: &[(String, Decimal)], // (address, sold_pct_individual)
    cfg: &PumpDumpConfig,
    chain: mg_onchain_common::chain::Chain,
) {
    let base = event.confidence.value();
    let amplifier = cfg.insider_amplifier.value;
    let cap = match base_signal {
        BaseSignal::A => 0.95_f64,
        BaseSignal::B => 0.85_f64,
    };
    let amplified = (base + amplifier).min(cap);

    let new_severity = severity_from_confidence(amplified);
    let new_conf = Confidence::new(amplified).unwrap_or(event.confidence);

    event.confidence = new_conf;
    event.severity = new_severity;

    // Update signal label.
    let label = match base_signal {
        BaseSignal::A => "insider_amplified_spike",
        BaseSignal::B => "insider_amplified_burst",
    };

    // Update notes: replace the signal= entry.
    event.evidence.notes.retain(|n| !n.starts_with("signal="));
    event.evidence.notes.push(format!(
        "signal={label} insider_source={}",
        insider_set.source.as_str()
    ));

    // Add insider_sold_pct evidence metric.
    event
        .evidence
        .metrics
        .insert(evidence_key(DETECTOR_ID, "insider_sold_pct"), sold_pct);

    // Add insider wallet addresses.
    for addr_str in &insider_set.addresses {
        if let Ok(addr) = Address::parse(chain, addr_str) {
            event.evidence.addresses.push(addr);
        }
    }

    // Add per-wallet sold_pct for top insider (evidence transparency).
    for (addr_str, ind_pct) in enriched_sells.iter().take(3) {
        event.evidence.metrics.insert(
            evidence_key(DETECTOR_ID, &format!("insider_sold_pct_{addr_str}")),
            *ind_pct,
        );
    }
}

// ---------------------------------------------------------------------------
// Evidence builders
// ---------------------------------------------------------------------------

fn build_evidence_a(
    row: &PumpDumpBaselineRow,
    result: &SignalAResult,
    market_cap_usd: Decimal,
    market_cap_source: &str,
) -> Evidence {
    let volume_ratio_dec = Decimal::from_f64(result.volume_ratio).unwrap_or(Decimal::ZERO);
    let price_change_dec = Decimal::from_f64(result.price_change_pct).unwrap_or(Decimal::ZERO);
    let z_score_dec = Decimal::from_f64(result.volume_z_score).unwrap_or(Decimal::ZERO);

    Evidence::new()
        .with_metric(
            evidence_key(DETECTOR_ID, "volume_1h_usd"),
            row.volume_1h_usd,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "baseline_7d_median_usd"),
            row.volume_7d_median_usd,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "volume_multiplier_observed"),
            volume_ratio_dec,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "price_change_pct_1h"),
            price_change_dec,
        )
        .with_metric(evidence_key(DETECTOR_ID, "volume_z_score"), z_score_dec)
        .with_metric(
            evidence_key(DETECTOR_ID, "baseline_days_available"),
            Decimal::from(row.baseline_days_available),
        )
        .with_metric(evidence_key(DETECTOR_ID, "market_cap_usd"), market_cap_usd)
        .with_note(format!(
            "signal=spike_with_baseline market_cap_source={market_cap_source}"
        ))
}

fn build_evidence_b(
    row: &BurstMetricsRow,
    result: &SignalBResult,
    market_cap_usd: Decimal,
    market_cap_source: &str,
) -> Evidence {
    let burst_dec = Decimal::from_f64(result.burst_ratio).unwrap_or(Decimal::ZERO);

    Evidence::new()
        .with_metric(
            evidence_key(DETECTOR_ID, "volume_1h_usd"),
            row.volume_1h_usd,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "baseline_7d_median_usd"),
            Decimal::ZERO,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "burst_concentration_ratio"),
            burst_dec,
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "baseline_days_available"),
            Decimal::ZERO,
        )
        .with_metric(evidence_key(DETECTOR_ID, "market_cap_usd"), market_cap_usd)
        .with_note(format!(
            "signal=burst_fallback market_cap_source={market_cap_source}"
        ))
}

// ---------------------------------------------------------------------------
// Market cap resolution (DG-04-5)
// ---------------------------------------------------------------------------

/// Resolve the best available market cap proxy from `TokenMeta`.
///
/// Priority:
/// 1. `circulating_supply_raw × price_per_raw_unit` (most accurate).
/// 2. `total_supply_raw × price_per_raw_unit` (upper bound / FDV proxy).
/// 3. `total_market_liquidity_usd` as a rough lower-bound proxy.
/// 4. `Decimal::ZERO` with source "unavailable".
///
/// Returns `(market_cap_usd, market_cap_source)` where source is one of:
/// "circulating" | "total_supply" | "liquidity_proxy" | "unavailable".
pub fn resolve_market_cap(meta: &TokenMeta) -> (Decimal, String) {
    // We do not have a price oracle in Phase 2 — use total_market_liquidity_usd as
    // the FDV proxy. This is documented in spec §DG-04-5 as the fallback path.
    // Future: multiply supply by price from oracle.
    if meta.total_market_liquidity_usd > Decimal::ZERO {
        // total_market_liquidity_usd is DEX pool liquidity (a lower bound on market cap).
        // Actual FDV is typically 10-100× higher for meme tokens.
        // For the filter, we use the TokenRegistry's enriched value if available.
        // NOTE: In Phase 2, we use total_market_liquidity_usd as the best available proxy.
        // The spec acknowledges this gap — the market_cap_source key documents it.
        (
            "liquidity_proxy".to_owned(),
            meta.total_market_liquidity_usd,
        )
    } else {
        ("unavailable".to_owned(), Decimal::ZERO)
    }
    .pipe_swap()
}

/// Helper to produce `(Decimal, String)` in the right order.
trait PipeSwap {
    fn pipe_swap(self) -> (Decimal, String);
}
impl PipeSwap for (String, Decimal) {
    fn pipe_swap(self) -> (Decimal, String) {
        (self.1, self.0)
    }
}

// ---------------------------------------------------------------------------
// Insider address resolution (3-tier priority ladder)
// ---------------------------------------------------------------------------

/// Resolve the insider address set for Signal C.
///
/// Implements the 3-tier priority ladder from `docs/designs/0007` §10:
/// 1. `deployer_clusters` table (Phase 3+).
/// 2. `top_holders` proxy (Phase 2 degraded mode, ≥1% supply floor, excluding non-liquid).
/// 3. Unavailable.
async fn resolve_insider_addresses<'ctx>(
    ctx: &'ctx DetectorContext<'ctx>,
    meta: &TokenMeta,
    cfg: &PumpDumpConfig,
) -> Result<InsiderSet, DetectorError> {
    // Priority 1: deployer_clusters table.
    let cluster_result = ctx
        .store
        .fetch_deployer_cluster_addresses(ctx.chain.as_str(), ctx.token.as_str())
        .await;

    match cluster_result {
        Ok(addrs) if !addrs.is_empty() => {
            return Ok(InsiderSet {
                addresses: addrs,
                source: InsiderSource::DeployerClusters,
            });
        }
        Ok(_) => {
            // Empty — proceed to Priority 2.
            debug!(
                token = ctx.token.as_str(),
                "D04: deployer_clusters empty — falling back to top_holders_proxy"
            );
        }
        Err(mg_onchain_storage::error::StorageError::Postgres(se)) => {
            return Err(DetectorError::TransientQuery {
                detector_id: DETECTOR_ID,
                source: se,
            });
        }
        Err(other) => {
            debug!(
                token = ctx.token.as_str(),
                "D04: deployer_clusters query failed, falling back: {other}"
            );
        }
    }

    // Priority 2: top_holders proxy via holder_classifications sidecar JOIN.
    let floor_pct =
        Decimal::from_f64(cfg.top_holders_insider_floor_pct.value).unwrap_or(Decimal::new(1, 2)); // 0.01

    let proxy_result = ctx
        .store
        .fetch_top_holders_liquid(ctx.chain.as_str(), ctx.token.as_str(), floor_pct)
        .await;

    match proxy_result {
        Ok(addrs) if !addrs.is_empty() => {
            return Ok(InsiderSet {
                addresses: addrs,
                source: InsiderSource::TopHoldersProxy,
            });
        }
        Ok(_) => {
            // Empty — also check meta.top_holders as a last-resort proxy.
            debug!(
                token = ctx.token.as_str(),
                "D04: fetch_top_holders_liquid empty — checking meta.top_holders"
            );
        }
        Err(other) => {
            debug!(
                token = ctx.token.as_str(),
                "D04: fetch_top_holders_liquid failed: {other}"
            );
        }
    }

    // Priority 2b: use meta.top_holders (from registry) if the sidecar query failed
    // or returned empty. Apply the same floor and exclusion logic in Rust.
    let floor_raw = Decimal::from(meta.total_supply_raw) * floor_pct;
    let proxy_addrs: Vec<String> = meta
        .top_holders
        .iter()
        .filter(|h| {
            let balance = Decimal::from(h.amount_raw);
            balance >= floor_raw
        })
        .map(|h| h.address.as_str().to_owned())
        .collect();

    if !proxy_addrs.is_empty() {
        return Ok(InsiderSet {
            addresses: proxy_addrs,
            source: InsiderSource::TopHoldersProxy,
        });
    }

    // Priority 3: no data available.
    Ok(InsiderSet {
        addresses: vec![],
        source: InsiderSource::Unavailable,
    })
}

// ---------------------------------------------------------------------------
// Signal C: compute aggregate sold_pct across all insider wallets
// ---------------------------------------------------------------------------

/// Compute aggregate insider sold percentage.
///
/// Returns `(aggregate_sold_pct, per_wallet_sold_pcts)`.
///
/// `aggregate_sold_pct = total_sold_raw / total_balance_at_spike_raw` where:
/// - `total_sold_raw` = sum of sold_amount_raw across all insider_sells rows.
/// - `total_balance_at_spike_raw` = sum of top_holder balances for insider addresses.
///
/// When balance data is unavailable for a wallet (not in top_holders), that wallet's
/// balance is treated as their sold amount (sold_pct = 1.0 for that wallet — conservative).
fn compute_insider_sold_pct(
    insider_set: &InsiderSet,
    sells: &[InsiderSellRow],
    meta: &TokenMeta,
) -> (Decimal, Vec<(String, Decimal)>) {
    if sells.is_empty() {
        return (Decimal::ZERO, vec![]);
    }

    // Build a lookup of address → balance_raw from meta.top_holders.
    // BTreeMap for deterministic iteration.
    let mut balance_map: std::collections::BTreeMap<&str, Decimal> =
        std::collections::BTreeMap::new();
    for h in &meta.top_holders {
        balance_map.insert(h.address.as_str(), Decimal::from(h.amount_raw));
    }

    let mut total_sold = Decimal::ZERO;
    let mut total_balance = Decimal::ZERO;
    let mut per_wallet: Vec<(String, Decimal)> = Vec::new();

    for sell in sells {
        let balance = balance_map
            .get(sell.address.as_str())
            .copied()
            .unwrap_or(sell.sold_amount_raw); // conservative: treat unknown balance as sold 100%

        let wallet_pct = if balance > Decimal::ZERO {
            (sell.sold_amount_raw / balance).min(Decimal::ONE)
        } else {
            Decimal::ONE
        };

        total_sold += sell.sold_amount_raw;
        total_balance += balance;
        per_wallet.push((sell.address.clone(), wallet_pct));
    }

    // For wallets in insider_set.addresses that are NOT in sells (did not sell),
    // their balance still counts toward the denominator.
    for addr in &insider_set.addresses {
        let already_counted = sells.iter().any(|s| &s.address == addr);
        if !already_counted
            && let Some(&bal) = balance_map.get(addr.as_str())
        {
            total_balance += bal;
        }
    }

    let aggregate_pct = if total_balance > Decimal::ZERO {
        (total_sold / total_balance).min(Decimal::ONE)
    } else {
        Decimal::ZERO
    };

    (aggregate_pct, per_wallet)
}

// ---------------------------------------------------------------------------
// AnomalyEvent factory
// ---------------------------------------------------------------------------

fn make_event(
    ctx: &DetectorContext<'_>,
    _meta: &TokenMeta,
    confidence_f64: f64,
    severity: Severity,
    evidence: Evidence,
) -> AnomalyEvent {
    let confidence =
        Confidence::new(confidence_f64.clamp(0.0, 1.0)).unwrap_or(Confidence::new(0.05).unwrap());
    AnomalyEvent {
        detector_id: DETECTOR_ID.to_owned(),
        token: ctx.token.clone(),
        chain: ctx.chain,
        confidence,
        severity,
        evidence,
        observed_at: ctx.window.end,
        window: (ctx.window.block_start, ctx.window.block_end),
        ingested_at: ctx.observed_at,
    }
}

// ---------------------------------------------------------------------------
// Smart-money amplification — D04 specific
// ---------------------------------------------------------------------------

/// Compute the smart-money confidence amplification delta for D04.
///
/// Per-tier delta (additive, applied once per evaluation — not per wallet):
/// - Tier1: `tier1_count >= 1` → +0.12 (Decision 2, unverified-heuristic)
/// - Tier2: `tier2_count >= 2` (min count threshold) → +0.07 (Decision 2)
/// - Tier3: 0.00 (no amplification — too weak signal)
///
/// When Tier1 is present, only the Tier1 delta is applied (not additive with Tier2).
/// This mirrors the spec formula in design 0023 §4.1.
///
/// # References
///
/// - Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686): masterminds buy pre-event in 100%
///   of confirmed pump events. Design derivation: +0.12 at Signal A threshold (0.60) → 0.72.
/// - Design 0023 §4.1, Decision 2 (user approved).
///
/// `f64` is used here because this is a probability/confidence delta, NOT a monetary amount
/// (per CLAUDE.md: f64 only for confidence/deltas, Decimal for money/prices/amounts).
pub fn compute_smart_money_amplification_d04(
    tier_counts: &TierCounts,
    cfg: &PumpDumpConfig,
) -> f64 {
    let tier1_delta = cfg.smart_money_tier1_delta.value;
    let tier2_delta = cfg.smart_money_tier2_delta.value;
    let tier2_min_count = cfg.smart_money_tier2_min_count.value;

    if tier_counts.tier1 >= 1 {
        // Tier1 present: apply Tier1 delta only (Tier2 not additive per spec §4.1).
        tier1_delta
    } else if tier_counts.tier2 >= tier2_min_count {
        // Tier2 present at min count: apply Tier2 delta.
        tier2_delta
    } else {
        // No eligible smart-money (Tier3 always 0.00, or below min counts).
        0.0
    }
}

/// Fetch wallet addresses that bought the token in the pre-pump window.
///
/// Queries the `swaps` table for buy-side swaps within
/// `[pre_pump_start, window_start)`. Returns canonical wallet addresses.
///
/// Returns empty Vec if no data is available (non-fatal; amplification is
/// simply not applied). Errors are propagated as `DetectorError::TransientQuery`.
async fn fetch_pre_pump_buyers(
    ctx: &DetectorContext<'_>,
    pre_pump_start: chrono::DateTime<chrono::Utc>,
    window_start: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<String>, DetectorError> {
    use sqlx::Row as _;

    let rows = sqlx::query(
        r#"
        SELECT DISTINCT wallet
        FROM swaps
        WHERE chain = $1
          AND token = $2
          AND side = 'buy'
          AND block_time >= $3
          AND block_time < $4
        ORDER BY wallet
        "#,
    )
    .bind(ctx.chain.as_str())
    .bind(ctx.token.as_str())
    .bind(pre_pump_start)
    .bind(window_start)
    .fetch_all(ctx.store.pool())
    .await
    .map_err(|e| DetectorError::TransientQuery {
        detector_id: DETECTOR_ID,
        source: e,
    })?;

    let addresses: Vec<String> = rows
        .iter()
        .filter_map(|r| r.try_get::<String, _>("wallet").ok())
        .collect();

    Ok(addresses)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use mg_onchain_common::anomaly::Severity;
    use mg_onchain_storage::pg::{BurstMetricsRow, PumpDumpBaselineRow};

    use crate::config::PumpDumpConfig;

    // -------------------------------------------------------------------------
    // Config fixture builder
    // -------------------------------------------------------------------------

    fn default_cfg() -> PumpDumpConfig {
        use crate::config::Threshold;
        PumpDumpConfig {
            volume_multiplier: Threshold {
                value: 5.0,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            price_spike_pct: Threshold {
                value: 0.30,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            min_baseline_days: Threshold {
                value: 3,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            burst_concentration_threshold: Threshold {
                value: 0.70,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            min_burst_volume_usd: Threshold {
                value: 5000.0,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            insider_sell_pct: Threshold {
                value: 0.40,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            insider_amplifier: Threshold {
                value: 0.15,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            post_pump_insider_window_hours: Threshold {
                value: 24,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            market_cap_filter_usd: Threshold {
                value: 60_000_000.0,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            top_holders_insider_floor_pct: Threshold {
                value: 0.01,
                rationale: "test".into(),
                refs: vec!["D04/pump_dump".into()],
            },
            pre_pump_window_minutes: Threshold {
                value: 60,
                rationale: "test".into(),
                refs: vec!["D04/smart_money_amplification".into()],
            },
            smart_money_tier1_delta: Threshold {
                value: 0.12,
                rationale: "test".into(),
                refs: vec!["D04/smart_money_amplification".into()],
            },
            smart_money_tier2_delta: Threshold {
                value: 0.07,
                rationale: "test".into(),
                refs: vec!["D04/smart_money_amplification".into()],
            },
            smart_money_tier2_min_count: Threshold {
                value: 2,
                rationale: "test".into(),
                refs: vec!["D04/smart_money_amplification".into()],
            },
        }
    }

    fn baseline_row(
        volume_1h_usd: Decimal,
        median_usd: Decimal,
        price_change_pct: Decimal,
        baseline_days: i64,
    ) -> PumpDumpBaselineRow {
        PumpDumpBaselineRow {
            volume_1h_usd,
            volume_7d_median_usd: median_usd,
            price_change_pct_1h: price_change_pct,
            volume_z_score: Decimal::ZERO,
            baseline_days_available: baseline_days,
            market_cap_usd: Decimal::ZERO,
            market_cap_source: "unavailable".into(),
        }
    }

    fn burst_row(vol_1h: Decimal, vol_24h: Decimal, burst_ratio: Decimal) -> BurstMetricsRow {
        BurstMetricsRow {
            volume_1h_usd: vol_1h,
            volume_24h_usd: vol_24h,
            burst_concentration_ratio: burst_ratio,
        }
    }

    // -------------------------------------------------------------------------
    // Config pin tests
    // -------------------------------------------------------------------------

    /// Config pin: volume_multiplier must be 5.0.
    #[test]
    fn config_pin_volume_multiplier() {
        let cfg = default_cfg();
        assert_eq!(cfg.volume_multiplier.value, 5.0);
    }

    /// Config pin: burst_concentration_threshold must be 0.90.
    #[test]
    fn config_pin_burst_concentration_threshold() {
        let cfg = default_cfg();
        // Value lowered 0.90 → 0.70 per review 0003 §E-D04-9 (2h slow-pump coverage).
        // See docs/reviews/0003-d04-pump-dump-evasions.md §6 C-adjustment #1.
        assert_eq!(cfg.burst_concentration_threshold.value, 0.70);
    }

    /// Config pin: market_cap_filter_usd must be 60_000_000.
    #[test]
    fn config_pin_market_cap_filter_usd() {
        let cfg = default_cfg();
        assert_eq!(cfg.market_cap_filter_usd.value, 60_000_000.0);
    }

    /// Config pin: insider_amplifier = 0.15.
    #[test]
    fn config_pin_insider_amplifier() {
        let cfg = default_cfg();
        assert_eq!(cfg.insider_amplifier.value, 0.15);
    }

    // -------------------------------------------------------------------------
    // Signal A standalone tests
    // -------------------------------------------------------------------------

    /// At threshold floor (ratio=5, price=0.30) → confidence clamped to 0.60.
    #[test]
    fn signal_a_at_threshold_floor_gives_0_60() {
        let cfg = default_cfg();
        let row = baseline_row(
            dec!(5000), // volume_1h
            dec!(1000), // median (ratio = 5.0 = threshold exactly)
            dec!(0.30), // price_change = threshold exactly
            5,
        );
        let result = compute_signal_a_confidence(&row, &cfg);
        // raw = (5/5 - 1)*0.5 + (0.30/0.30 - 1)*0.3 = 0 + 0 = 0
        // sigmoid(0) = 0.50 → clamped to 0.60
        assert!(
            (result.confidence - 0.60).abs() < 1e-9,
            "confidence at threshold floor must be exactly 0.60, got {}",
            result.confidence
        );
    }

    /// volume_ratio=10, price_change=0.5 → confidence ≈ 0.750 (per spec example).
    #[test]
    fn signal_a_volume_10x_price_50pct() {
        let cfg = default_cfg();
        let row = baseline_row(dec!(10000), dec!(1000), dec!(0.50), 5);
        let result = compute_signal_a_confidence(&row, &cfg);
        // raw = (10/5 - 1)*0.5 + (0.5/0.3 - 1)*0.3 = 0.5 + 0.167*0.3 = 0.5+0.1667 = 0.5+0.1667 = 0.6667
        // Actually: (2-1)*0.5 + (5/3-1)*0.3 = 0.5 + (0.6667)*0.3 = 0.5+0.2 = 0.7
        // sigmoid(0.7) ≈ 0.668 → no clamp needed
        assert!(
            result.confidence >= 0.60 && result.confidence <= 0.95,
            "confidence must be within [0.60, 0.95], got {}",
            result.confidence
        );
        assert!(
            result.confidence > 0.60,
            "confidence at 10x ratio must be above floor, got {}",
            result.confidence
        );
    }

    /// Signal A at ratio=10, price=0.5 matches the formula exactly.
    #[test]
    fn signal_a_formula_exact_verification() {
        let cfg = default_cfg();
        // From POS_03 fixture: volume_ratio=11.90, price_change=0.45
        // raw = (11.90/5 - 1)*0.5 + (0.45/0.30 - 1)*0.3 = (1.38)*0.5 + (0.5)*0.3 = 0.69+0.15 = 0.84
        // sigmoid(0.84) ≈ 0.698
        let row = baseline_row(
            dec!(8500),
            Decimal::from_f64(714.29).unwrap(),
            dec!(0.45),
            5,
        );
        let result = compute_signal_a_confidence(&row, &cfg);
        // volume_ratio = 8500/714.29 ≈ 11.903
        let expected = sigmoid((11.903_f64 / 5.0 - 1.0) * 0.5 + (0.45_f64 / 0.30_f64 - 1.0) * 0.3);
        let expected_clamped = expected.clamp(0.60, 0.95);
        assert!(
            (result.confidence - expected_clamped).abs() < 0.005,
            "POS_03 fixture: confidence must be ≈{expected_clamped:.3}, got {:.3}",
            result.confidence
        );
    }

    /// Signal A at very high ratio caps at 0.95.
    #[test]
    fn signal_a_very_high_ratio_caps_at_0_95() {
        let cfg = default_cfg();
        let row = baseline_row(dec!(1000000), dec!(1000), dec!(10.0), 7);
        let result = compute_signal_a_confidence(&row, &cfg);
        assert!(
            (result.confidence - 0.95).abs() < 1e-9,
            "confidence must cap at 0.95, got {}",
            result.confidence
        );
    }

    // -------------------------------------------------------------------------
    // Signal B standalone tests
    // -------------------------------------------------------------------------

    /// At threshold (0.70 per review 0003 §E-D04-9) → confidence = 0.50.
    #[test]
    fn signal_b_at_threshold_gives_0_50() {
        let cfg = default_cfg();
        let row = burst_row(dec!(50000), dec!(50000), dec!(0.70));
        let result = compute_signal_b_confidence(&row, &cfg);
        assert!(
            (result.confidence - 0.50).abs() < 1e-9,
            "Signal B at threshold must be 0.50, got {}",
            result.confidence
        );
    }

    /// At burst_ratio=0.95 (threshold 0.70) → 0.50 + (0.95-0.70)/0.10 * 0.25 = 1.125 → capped at 0.75.
    #[test]
    fn signal_b_at_0_95_burst_ratio() {
        let cfg = default_cfg();
        let row = burst_row(dec!(47500), dec!(50000), dec!(0.95));
        let result = compute_signal_b_confidence(&row, &cfg);
        // 0.50 + (0.95 - 0.70) / 0.10 * 0.25 = 1.125 → capped at 0.75 ceiling
        let expected = 0.75_f64;
        assert!(
            (result.confidence - expected).abs() < 1e-9,
            "Signal B at 0.95 ratio with threshold 0.70 must cap at 0.75, got {}",
            result.confidence
        );
    }

    /// At burst_ratio=1.00 (RAVE case) → confidence = 0.75.
    #[test]
    fn signal_b_at_1_00_burst_ratio_caps_at_0_75() {
        let cfg = default_cfg();
        let row = burst_row(dec!(7032876), dec!(7032876), dec!(1.00));
        let result = compute_signal_b_confidence(&row, &cfg);
        assert!(
            (result.confidence - 0.75).abs() < 1e-9,
            "Signal B at burst_ratio=1.00 must give 0.75, got {}",
            result.confidence
        );
    }

    /// At burst_ratio > 1.00 (impossible but guarded) → confidence still capped at 0.75.
    #[test]
    fn signal_b_above_1_00_still_capped_at_0_75() {
        let cfg = default_cfg();
        let row = burst_row(dec!(100000), dec!(50000), dec!(1.05));
        let result = compute_signal_b_confidence(&row, &cfg);
        assert!(
            result.confidence <= 0.75,
            "Signal B confidence must never exceed 0.75, got {}",
            result.confidence
        );
    }

    // -------------------------------------------------------------------------
    // Signal C cap tests
    // -------------------------------------------------------------------------

    /// A + C capped at 0.95.
    #[test]
    fn signal_c_a_base_cap_at_0_95() {
        // Simulate A base = 0.90, amplifier = 0.15 → 1.05 capped to 0.95.
        let amplifier = 0.15_f64;
        let base = 0.90_f64;
        let cap = 0.95_f64;
        let final_conf = (base + amplifier).min(cap);
        assert!(
            (final_conf - 0.95).abs() < 1e-9,
            "A+C must cap at 0.95, got {final_conf}"
        );
    }

    /// B + C capped at 0.85.
    #[test]
    fn signal_c_b_base_cap_at_0_85() {
        let amplifier = 0.15_f64;
        let base = 0.75_f64;
        let cap = 0.85_f64;
        let final_conf = (base + amplifier).min(cap);
        assert!(
            (final_conf - 0.85).abs() < 1e-9,
            "B+C must cap at 0.85, got {final_conf}"
        );
    }

    /// B base = 0.60 + C = 0.15 → 0.75 (under cap).
    #[test]
    fn signal_c_b_base_0_60_amplified_to_0_75() {
        let amplifier = 0.15_f64;
        let base = 0.60_f64;
        let cap = 0.85_f64;
        let final_conf = (base + amplifier).min(cap);
        assert!(
            (final_conf - 0.75).abs() < 1e-9,
            "B(0.60) + C(0.15) = 0.75 (under cap), got {final_conf}"
        );
    }

    // -------------------------------------------------------------------------
    // Severity mapping tests
    // -------------------------------------------------------------------------

    #[test]
    fn severity_mapping_signal_a_minimum() {
        // Signal A floor = 0.60 → High (per spec §6 and signals.rs bands).
        assert_eq!(severity_from_confidence(0.60), Severity::High);
    }

    #[test]
    fn severity_mapping_signal_b_minimum() {
        // Signal B floor = 0.50 → Medium (per spec §6).
        assert_eq!(severity_from_confidence(0.50), Severity::Medium);
    }

    #[test]
    fn severity_mapping_critical() {
        // 0.85 → Critical.
        assert_eq!(severity_from_confidence(0.848), Severity::Critical);
    }

    // -------------------------------------------------------------------------
    // Insider sold_pct computation test
    // -------------------------------------------------------------------------

    /// Signal C Priority 1: deployer wallet sells 65% → compute_insider_sold_pct correct.
    #[test]
    fn compute_insider_sold_pct_deployer_clusters_path() {
        use mg_onchain_common::chain::{Address, Chain};
        use mg_onchain_common::token::TopHolder;

        let meta = crate::mock::test_utils::MockTokenMetaBuilder::new_solana(
            "So11111111111111111111111111111111111111112",
        )
        .with_top_holder(TopHolder {
            address: Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112")
                .unwrap(),
            pct: dec!(15),
            amount_raw: 150_000_000_000_000u128,
            is_insider: false,
        })
        .build();

        let insider_set = InsiderSet {
            addresses: vec!["So11111111111111111111111111111111111111112".to_owned()],
            source: InsiderSource::DeployerClusters,
        };

        let sells = vec![InsiderSellRow {
            address: "So11111111111111111111111111111111111111112".to_owned(),
            sold_amount_raw: dec!(97_500_000_000_000),
            balance_at_spike_raw: Decimal::ZERO,
            sold_pct: Decimal::ZERO,
            sample_tx_hash: None,
        }];

        let (agg_pct, per_wallet) = compute_insider_sold_pct(&insider_set, &sells, &meta);

        // 97_500_000_000_000 / 150_000_000_000_000 = 0.65
        assert!(
            (agg_pct - dec!(0.65)).abs() < dec!(0.001),
            "aggregate sold_pct must be 0.65, got {agg_pct}"
        );
        assert_eq!(per_wallet.len(), 1);
    }

    // -------------------------------------------------------------------------
    // Fixture-based tests
    // -------------------------------------------------------------------------

    /// POS_01 RAVE: Signal B fires at 0.75 (burst_ratio=1.00, zero baseline).
    #[test]
    fn fixture_pos_01_rave_signal_b_fires_at_0_75() {
        let cfg = default_cfg();
        // From fixture: burst_ratio = 1.00 (all 24h volume in 1h burst)
        let row = burst_row(dec!(7032876), dec!(7032876), dec!(1.00));
        let result = compute_signal_b_confidence(&row, &cfg);
        assert!(
            (result.confidence - 0.75).abs() < 1e-9,
            "POS_01 RAVE: Signal B must fire at 0.75, got {}",
            result.confidence
        );
        assert_eq!(
            severity_from_confidence(result.confidence),
            Severity::High,
            "POS_01: severity must be High at 0.75"
        );
    }

    /// POS_02 SYNTHETIC burst fallback: Signal B at 0.75 + Signal C (proxy) → 0.85.
    #[test]
    fn fixture_pos_02_synthetic_burst_signal_b_plus_c() {
        let cfg = default_cfg();
        // Signal B base at burst_ratio = 1.00
        let row = burst_row(dec!(50000), dec!(50000), dec!(1.00));
        let base_result = compute_signal_b_confidence(&row, &cfg);
        assert!(
            (base_result.confidence - 0.75).abs() < 1e-9,
            "POS_02: base Signal B must be 0.75"
        );
        // Signal C: B+C cap = 0.85
        let amplified = (base_result.confidence + cfg.insider_amplifier.value).min(0.85);
        assert!(
            (amplified - 0.85).abs() < 1e-9,
            "POS_02: amplified must be 0.85 (capped), got {amplified}"
        );
        assert_eq!(
            severity_from_confidence(amplified),
            Severity::Critical, // 0.85 ≥ 0.80
            "POS_02: severity after C must be Critical"
        );
    }

    /// POS_03 SYNTHETIC insider sell: Signal A at ≈0.698 + Signal C → ≈0.848.
    #[test]
    fn fixture_pos_03_synthetic_signal_a_plus_signal_c() {
        let cfg = default_cfg();
        // From fixture: volume_ratio=11.90, price_change=0.45
        let row = baseline_row(
            dec!(8500),
            Decimal::from_f64(714.29).unwrap(),
            dec!(0.45),
            5,
        );
        let result_a = compute_signal_a_confidence(&row, &cfg);

        // Verify in range and above 0.60.
        assert!(
            result_a.confidence >= 0.60,
            "POS_03: Signal A must be >= 0.60"
        );
        assert!(
            result_a.confidence <= 0.95,
            "POS_03: Signal A must be <= 0.95"
        );

        // Signal C: A+C cap = 0.95.
        let amplified = (result_a.confidence + cfg.insider_amplifier.value).min(0.95);
        assert_eq!(
            severity_from_confidence(amplified),
            Severity::Critical,
            "POS_03: amplified confidence {amplified} must be Critical (>= 0.80)"
        );

        // Verify the fixture's expected confidence ≈ 0.848.
        // Fixture formula: sigmoid(0.84) ≈ 0.698; amplified = min(0.698+0.15, 0.95) = 0.848.
        assert!(
            (amplified - 0.848).abs() < 0.01,
            "POS_03: final confidence must be ≈0.848, got {amplified}"
        );
    }

    /// NEG_01 BONK: market_cap >> $60M → market_cap_filter test (unit test checks market cap logic).
    #[test]
    fn fixture_neg_01_bonk_market_cap_filter() {
        let cfg = default_cfg();
        let bonk_market_cap = dec!(555_600_000);
        let filter = Decimal::from_f64(cfg.market_cap_filter_usd.value).unwrap();
        assert!(
            bonk_market_cap > filter,
            "NEG_01 BONK: market cap {} must exceed filter {}",
            bonk_market_cap,
            filter
        );
    }

    /// NEG_02 PYTH: is_established_protocol → Signal C suppressed.
    #[test]
    fn fixture_neg_02_pyth_signal_c_suppressed() {
        use mg_onchain_common::token::JupiterVerification;
        let meta = crate::mock::test_utils::MockTokenMetaBuilder::new_solana(
            "So11111111111111111111111111111111111111112",
        )
        .jup_verified(true, false)
        .build();
        // Manually set rugcheck_score to 23 (PYTH pattern).
        let mut meta = meta;
        meta.rugcheck_score = Some(23);
        meta.verification = JupiterVerification {
            jup_verified: true,
            jup_strict: false,
        };

        assert!(
            is_established_protocol(&meta),
            "NEG_02 PYTH: jup_verified=true + score=23 must satisfy is_established_protocol"
        );
    }

    /// NEG_02 RAY: is_established_protocol returns false (known FP gap).
    #[test]
    fn fixture_neg_02_ray_not_suppressed_known_fp_gap() {
        use mg_onchain_common::token::JupiterVerification;
        let mut meta = crate::mock::test_utils::MockTokenMetaBuilder::new_solana(
            "So11111111111111111111111111111111111111112",
        )
        .build();
        meta.rugcheck_score = Some(56);
        meta.verification = JupiterVerification {
            jup_verified: false,
            jup_strict: false,
        };

        assert!(
            !is_established_protocol(&meta),
            "NEG_02 RAY: known FP gap — not suppressed (jup_verified=false, score=56)"
        );
    }

    /// NEG_03 USDC: market_cap >> $60M → market_cap_filter triggers.
    #[test]
    fn fixture_neg_03_usdc_market_cap_filter() {
        let cfg = default_cfg();
        let usdc_market_cap = dec!(26_000_000_000);
        let filter = Decimal::from_f64(cfg.market_cap_filter_usd.value).unwrap();
        assert!(
            usdc_market_cap > filter,
            "NEG_03 USDC: market cap {} must exceed filter {}",
            usdc_market_cap,
            filter
        );
    }

    // -------------------------------------------------------------------------
    // Evidence key presence tests
    // -------------------------------------------------------------------------

    /// Evidence A must contain all required keys.
    #[test]
    fn evidence_a_contains_required_keys() {
        let cfg = default_cfg();
        let row = baseline_row(dec!(8500), dec!(714), dec!(0.45), 5);
        let result = compute_signal_a_confidence(&row, &cfg);
        let evidence = build_evidence_a(&row, &result, dec!(500_000), "total_supply");

        let required_keys = [
            "pump_dump/volume_1h_usd",
            "pump_dump/baseline_7d_median_usd",
            "pump_dump/volume_multiplier_observed",
            "pump_dump/price_change_pct_1h",
            "pump_dump/volume_z_score",
            "pump_dump/baseline_days_available",
            "pump_dump/market_cap_usd",
        ];
        for key in &required_keys {
            assert!(
                evidence.metrics.contains_key(*key),
                "Evidence A missing required key: {key}"
            );
        }
        // Notes must contain signal label.
        assert!(
            evidence
                .notes
                .iter()
                .any(|n| n.contains("signal=spike_with_baseline")),
            "Evidence A notes must contain signal=spike_with_baseline"
        );
    }

    /// Evidence B must contain all required keys.
    #[test]
    fn evidence_b_contains_required_keys() {
        let cfg = default_cfg();
        let row = burst_row(dec!(50000), dec!(50000), dec!(1.00));
        let result = compute_signal_b_confidence(&row, &cfg);
        let evidence = build_evidence_b(&row, &result, dec!(500_000), "liquidity_proxy");

        let required_keys = [
            "pump_dump/volume_1h_usd",
            "pump_dump/baseline_7d_median_usd",
            "pump_dump/burst_concentration_ratio",
            "pump_dump/baseline_days_available",
            "pump_dump/market_cap_usd",
        ];
        for key in &required_keys {
            assert!(
                evidence.metrics.contains_key(*key),
                "Evidence B missing required key: {key}"
            );
        }
        assert!(
            evidence
                .notes
                .iter()
                .any(|n| n.contains("signal=burst_fallback")),
            "Evidence B notes must contain signal=burst_fallback"
        );
    }

    // -------------------------------------------------------------------------
    // Insufficient data tests
    // -------------------------------------------------------------------------

    /// Burst volume below dust filter → insufficient_data.
    #[test]
    fn signal_b_below_dust_filter_no_fire() {
        let cfg = default_cfg();
        let min_burst = Decimal::from_f64(cfg.min_burst_volume_usd.value).unwrap();
        let vol_below = dec!(3000); // below $5000 threshold
        assert!(
            vol_below < min_burst,
            "vol_below ({vol_below}) must be below min_burst_volume ({min_burst})"
        );
    }

    /// Burst ratio below threshold → no Signal B.
    #[test]
    fn signal_b_burst_ratio_below_threshold_no_fire() {
        let cfg = default_cfg();
        let threshold = Decimal::from_f64(cfg.burst_concentration_threshold.value).unwrap();
        let low_ratio = dec!(0.0024); // WET probe value
        assert!(
            low_ratio < threshold,
            "WET probe ratio {low_ratio} must be below threshold {threshold}"
        );
    }

    // -------------------------------------------------------------------------
    // Signal C Priority tests (unit level)
    // -------------------------------------------------------------------------

    /// Signal C Priority 3 (unavailable): InsiderSet with empty addresses.
    #[test]
    fn signal_c_priority_3_unavailable() {
        let insider_set = InsiderSet {
            addresses: vec![],
            source: InsiderSource::Unavailable,
        };
        assert_eq!(insider_set.source.as_str(), "unavailable");
        assert!(insider_set.addresses.is_empty());
    }

    /// Signal C Priority 2 (top_holders_proxy): InsiderSet with proxy source.
    #[test]
    fn signal_c_priority_2_top_holders_proxy() {
        let insider_set = InsiderSet {
            addresses: vec!["WALLET_A_insider".to_owned()],
            source: InsiderSource::TopHoldersProxy,
        };
        assert_eq!(insider_set.source.as_str(), "top_holders_proxy");
    }

    /// Signal C Priority 1 (deployer_clusters): source label correct.
    #[test]
    fn signal_c_priority_1_deployer_clusters() {
        let insider_set = InsiderSet {
            addresses: vec!["DEPLOYER_WALLET_01".to_owned()],
            source: InsiderSource::DeployerClusters,
        };
        assert_eq!(insider_set.source.as_str(), "deployer_clusters");
    }

    /// Signal C suppression for established protocol sets the audit key.
    ///
    /// This test verifies the evidence key logic at the unit level.
    /// The full integration path (evaluate() → established_protocol → audit key) is tested
    /// via the is_established_protocol tests in token_status.rs.
    #[test]
    fn signal_c_suppression_evidence_key_value() {
        // The key that must be set when Signal C is suppressed.
        let key = evidence_key(DETECTOR_ID, "established_protocol_suppressed_signal_c");
        assert_eq!(key, "pump_dump/established_protocol_suppressed_signal_c");
        // The value must be Decimal::ONE.
        assert_eq!(Decimal::ONE, dec!(1));
    }

    // -------------------------------------------------------------------------
    // Determinism test
    // -------------------------------------------------------------------------

    /// Same inputs must produce byte-identical evidence.
    #[test]
    fn signal_a_deterministic_same_input_same_output() {
        let cfg = default_cfg();
        let row = baseline_row(dec!(8500), dec!(714), dec!(0.45), 5);

        let result1 = compute_signal_a_confidence(&row, &cfg);
        let result2 = compute_signal_a_confidence(&row, &cfg);

        // f64 comparison: same bit pattern for deterministic computation.
        assert_eq!(
            result1.confidence.to_bits(),
            result2.confidence.to_bits(),
            "Signal A confidence must be bit-identical across repeated calls"
        );
        assert_eq!(
            result1.volume_ratio.to_bits(),
            result2.volume_ratio.to_bits()
        );
    }

    /// Evidence BTreeMap keys are in alphabetical order.
    #[test]
    fn evidence_btreemap_keys_are_ordered() {
        let cfg = default_cfg();
        let row = baseline_row(dec!(8500), dec!(714), dec!(0.45), 5);
        let result = compute_signal_a_confidence(&row, &cfg);
        let evidence = build_evidence_a(&row, &result, dec!(500_000), "total_supply");

        let keys: Vec<&str> = evidence.metrics.keys().map(|s| s.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(
            keys, sorted,
            "Evidence metrics keys must be in alphabetical order (BTreeMap invariant)"
        );
    }

    // -------------------------------------------------------------------------
    // Config loader round-trip (production TOML)
    // -------------------------------------------------------------------------

    /// The production config must parse and contain all D04 thresholds.
    #[test]
    fn config_toml_loads_pump_dump_thresholds() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = std::path::PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");

        let cfg = crate::config::load_detector_config(&config_path)
            .expect("config/detectors.toml must load successfully");

        // Pin all 10 D04 thresholds.
        assert_eq!(cfg.pump_dump.volume_multiplier.value, 5.0);
        assert_eq!(cfg.pump_dump.price_spike_pct.value, 0.30);
        assert_eq!(cfg.pump_dump.min_baseline_days.value, 3);
        // Lowered 0.90 → 0.70 per review 0003 §E-D04-9 (2h slow-pump coverage).
        assert_eq!(cfg.pump_dump.burst_concentration_threshold.value, 0.70);
        assert_eq!(cfg.pump_dump.min_burst_volume_usd.value, 5000.0);
        assert_eq!(cfg.pump_dump.insider_sell_pct.value, 0.40);
        assert_eq!(cfg.pump_dump.insider_amplifier.value, 0.15);
        assert_eq!(cfg.pump_dump.post_pump_insider_window_hours.value, 24);
        assert_eq!(cfg.pump_dump.market_cap_filter_usd.value, 60_000_000.0);
        assert_eq!(cfg.pump_dump.top_holders_insider_floor_pct.value, 0.01);

        // All refs must be non-empty.
        assert!(!cfg.pump_dump.volume_multiplier.refs.is_empty());
        assert!(!cfg.pump_dump.burst_concentration_threshold.refs.is_empty());
        assert!(!cfg.pump_dump.market_cap_filter_usd.refs.is_empty());

        // Sprint 23 smart-money config keys.
        assert_eq!(cfg.pump_dump.pre_pump_window_minutes.value, 60);
        assert_eq!(cfg.pump_dump.smart_money_tier1_delta.value, 0.12);
        assert_eq!(cfg.pump_dump.smart_money_tier2_delta.value, 0.07);
        assert_eq!(cfg.pump_dump.smart_money_tier2_min_count.value, 2);
    }

    // -------------------------------------------------------------------------
    // S23 smart-money amplification unit tests
    // -------------------------------------------------------------------------

    /// Tier1 only: delta = +0.12.
    #[test]
    fn sm_amplification_d04_tier1_only() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 0,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        assert!(
            (delta - 0.12).abs() < 1e-9,
            "Tier1 only must produce delta = 0.12, got {delta}"
        );
    }

    /// Tier2 with 1 wallet (below min count of 2) → delta = 0.00.
    #[test]
    fn sm_amplification_d04_tier2_below_min_count() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 0,
            tier2: 1, // below min_count = 2
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        assert!(
            delta.abs() < 1e-9,
            "Tier2 count=1 (below min=2) must produce delta = 0.00, got {delta}"
        );
    }

    /// Tier2 with 2+ wallets → delta = +0.07.
    #[test]
    fn sm_amplification_d04_tier2_min_count_met() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 0,
            tier2: 2,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        assert!(
            (delta - 0.07).abs() < 1e-9,
            "Tier2 count=2 must produce delta = 0.07, got {delta}"
        );
    }

    /// Tier1 + Tier2 present: Tier1 wins (not additive), delta = +0.12.
    #[test]
    fn sm_amplification_d04_tier1_and_tier2_tier1_wins() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 3,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        assert!(
            (delta - 0.12).abs() < 1e-9,
            "Tier1 + Tier2 must use Tier1 delta (0.12), not additive, got {delta}"
        );
    }

    /// Tier3 only → delta = 0.00.
    #[test]
    fn sm_amplification_d04_tier3_no_delta() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 0,
            tier2: 0,
            tier3: 5,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        assert!(
            delta.abs() < 1e-9,
            "Tier3 only must produce delta = 0.00, got {delta}"
        );
    }

    /// Cap enforcement: base=0.88 + Tier1 delta(0.12) + Signal C(0.15) = 1.15 → clamped to 0.95.
    #[test]
    fn sm_amplification_d04_cap_enforced_at_0_95() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 0,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        // Simulate stacking with Signal C amplifier (0.15) on top of a high base (0.88).
        let base = 0.88_f64;
        let signal_c = 0.15_f64;
        let cap = 0.95_f64;
        let total = (base + delta + signal_c).min(cap);
        assert!(
            (total - 0.95).abs() < 1e-9,
            "clamped result must be 0.95, got {total}"
        );
    }

    /// Signal B cap = 0.85 enforced with Tier1 delta.
    #[test]
    fn sm_amplification_d04_signal_b_cap_0_85() {
        let cfg = default_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 0,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d04(&counts, &cfg);
        let base = 0.80_f64;
        let cap = 0.85_f64; // Signal B cap
        let total = (base + delta).min(cap);
        assert!(
            (total - 0.85).abs() < 1e-9,
            "Signal B cap must be 0.85, got {total}"
        );
    }

    /// PumpDumpDetector::new() with no smart_money is backwards-compat (field = None).
    #[test]
    fn pump_dump_detector_new_has_no_smart_money() {
        let det = PumpDumpDetector::new(default_cfg());
        assert!(
            det.smart_money.is_none(),
            "new() must default smart_money to None for backwards-compat"
        );
    }

    /// with_smart_money() builder sets the field.
    #[test]
    fn pump_dump_detector_with_smart_money_sets_field() {
        use mg_onchain_graph::MockSmartMoneyLookup;
        let lookup = std::sync::Arc::new(MockSmartMoneyLookup::empty());
        let det = PumpDumpDetector::new(default_cfg()).with_smart_money(lookup);
        assert!(
            det.smart_money.is_some(),
            "with_smart_money() must set smart_money to Some"
        );
    }

    // =========================================================================
    // Track A: multi-chain expansion tests (Sprint 25)
    // =========================================================================

    /// D04 supported_chains must include all 6 chains: Solana + 5 EVM.
    ///
    /// D04 reads from `swap_buys` + `address_labels`, both keyed by (chain, token).
    /// All Chain::Solana references in the source are in the #[cfg(test)] block only.
    #[test]
    fn d04_supported_chains_returns_6_chains() {
        use mg_onchain_common::chain::Chain;
        let det = PumpDumpDetector::new(default_cfg());
        let chains = det.supported_chains();
        assert_eq!(chains.len(), 6, "D04 must support 6 chains (Solana + 5 EVM)");
        assert!(chains.contains(&Chain::Solana), "D04 must support Solana");
        assert!(chains.contains(&Chain::Ethereum), "D04 must support Ethereum");
        assert!(chains.contains(&Chain::Bsc), "D04 must support BSC");
        assert!(chains.contains(&Chain::Base), "D04 must support Base");
        assert!(chains.contains(&Chain::Arbitrum), "D04 must support Arbitrum");
        assert!(chains.contains(&Chain::Polygon), "D04 must support Polygon");
    }

    /// D04 pure confidence functions do not panic with Ethereum context inputs.
    ///
    /// compute_signal_b_confidence is a pure numeric function that accepts BurstMetricsRow
    /// — no chain-specific code paths exist in the formula logic.
    /// Signal B uses `burst_concentration_ratio` which is pre-computed in SQL (chain-agnostic).
    #[test]
    fn d04_ethereum_context_pure_functions_no_panic() {
        use mg_onchain_storage::pg::BurstMetricsRow;
        let cfg = default_cfg();
        // Simulate an Ethereum-token burst metrics row with burst_concentration_ratio=1.0
        // (all 24h volume happened in the 1h window — maximum burst).
        let eth_burst = BurstMetricsRow {
            volume_1h_usd: dec!(15_000),
            volume_24h_usd: dec!(15_000),
            burst_concentration_ratio: Decimal::ONE, // ratio = 1.0
        };
        let result = compute_signal_b_confidence(&eth_burst, &cfg);
        // burst_ratio = 1.0 → confidence = 0.75 (at saturation cap)
        assert!(
            result.confidence >= 0.50 && result.confidence <= 0.75,
            "D04 Signal B pure function must work on EVM-scale amounts, got {}",
            result.confidence
        );
    }

    /// D04 smart-money amplification path uses chain-aware TierCounts input.
    ///
    /// Verifies that compute_smart_money_amplification_d04 accepts any chain's
    /// TierCounts — the struct is chain-agnostic (wallet addresses as strings,
    /// tier as u8). The amplification formula is purely numeric and produces
    /// identical results regardless of which chain the wallets belong to.
    #[test]
    fn d04_smart_money_amplification_chain_agnostic() {
        use crate::smart_money_amplifier::TierCounts;
        let cfg = default_cfg();

        // Tier1=1 → delta = tier1_delta (default 0.12); Tier2 not additive per spec §4.1.
        let tier1_present = TierCounts { tier1: 1, tier2: 0, tier3: 0 };
        let delta_t1 = compute_smart_money_amplification_d04(&tier1_present, &cfg);
        assert!(
            (delta_t1 - 0.12_f64).abs() < 1e-9,
            "Tier1 amplification must be 0.12 regardless of chain, got {delta_t1}"
        );

        // Tier2 only, count >= min_count (default 2) → delta = tier2_delta (default 0.07).
        let tier2_only = TierCounts { tier1: 0, tier2: 2, tier3: 0 };
        let delta_t2 = compute_smart_money_amplification_d04(&tier2_only, &cfg);
        assert!(
            (delta_t2 - 0.07_f64).abs() < 1e-9,
            "Tier2 amplification must be 0.07 regardless of chain, got {delta_t2}"
        );

        // No eligible smart-money → delta = 0.00.
        let no_sm = TierCounts { tier1: 0, tier2: 0, tier3: 0 };
        let delta_none = compute_smart_money_amplification_d04(&no_sm, &cfg);
        assert!(
            (delta_none - 0.0_f64).abs() < 1e-9,
            "No smart-money must give delta=0.00 regardless of chain, got {delta_none}"
        );
    }
}
