//! Threshold configuration types and TOML loader for all detectors.
//!
//! Every threshold is wrapped in [`Threshold<T>`] which carries the value,
//! a human-readable rationale string, and citation references to `REFERENCES.md`.
//!
//! # Loading
//!
//! At startup, [`load_detector_config`] parses `config/detectors.toml` and
//! validates that all required threshold keys are present. Missing keys produce
//! a hard error at startup — detectors never silently fall back to defaults.
//! This is intentional: a missing config key means the system was deployed
//! without the operator consciously setting thresholds.
//!
//! # Adding a new detector
//!
//! 1. Add a new struct `XxxConfig` with [`Threshold<T>`] fields.
//! 2. Add it to [`AllDetectorConfigs`].
//! 3. Add the TOML subsection to `config/detectors.toml` with rationale + refs.
//! 4. Document the threshold in `REFERENCES.md`.
//!
//! No other changes are needed — the loader picks up the new struct automatically.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Threshold wrapper
// ---------------------------------------------------------------------------

/// A typed threshold value with its cited rationale.
///
/// Every threshold in `config/detectors.toml` uses this shape:
/// ```toml
/// [detector_id.threshold_name]
/// value    = 0.65
/// rationale = "Chainalysis 2025: deployer removes >= 65% of pool liquidity..."
/// refs     = ["D02/rug_pull_lp_drain"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Threshold<T> {
    /// The threshold value.
    pub value: T,
    /// Human-readable rationale explaining the chosen value and its source.
    /// Must reference a REFERENCES.md entry ID in `refs`.
    pub rationale: String,
    /// REFERENCES.md entry IDs that justify this threshold.
    /// Format: `"D<NN>/<slug>"` matching the REFERENCES.md Detector column.
    ///
    /// TODO(phase-3): Add a validation pass that checks each entry in `refs`
    /// exists in REFERENCES.md at startup. For Phase 2, refs are validated
    /// manually during code review. Automated validation requires parsing
    /// REFERENCES.md's Markdown table — non-trivial and deferred to keep Phase 2
    /// scope focused on detector correctness rather than tooling.
    pub refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Per-detector config structs
// ---------------------------------------------------------------------------

/// Thresholds for D01 Honeypot (simulation) detector.
///
/// Source: `research/02-detection-methodology.md` §2 + Torres et al. 2019.
///
/// Config keys per `docs/designs/0004-detector-01-honeypot.md` §5 Threshold Table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HoneypotConfig {
    /// Sell tax above this fraction triggers the detector.
    /// Range: (0.0, 1.0]. Default 0.50 (50%).
    pub sell_tax_threshold: Threshold<f64>,

    /// Sell tax threshold in basis points (same as `sell_tax_threshold` but as u16).
    /// Used for direct comparison with `TransferFeeConfig.fee_bps` (Token-2022 S2 signal).
    /// Default: 5000 bps = 50%.
    pub sell_tax_threshold_bps: Threshold<u16>,

    /// Number of distinct probe amounts to simulate for buy+sell.
    /// Catches max-sell-amount honeypots that allow small sells but block large ones.
    pub simulate_paths: Threshold<u32>,

    /// Buy/sell ratio sentinel above which the detector fires (zero-sell honeypot).
    /// 999.0 is the sentinel returned by d01_honeypot.sql when sell_count = 0.
    pub buy_sell_ratio_sentinel: Threshold<f64>,

    /// Minimum number of observed buy transfers before the buy/sell ratio signal (S5)
    /// is evaluated. Below this, the pool has insufficient activity to produce a
    /// meaningful ratio — a token with few buys and 0 sells is not evidence of
    /// sell suppression; it may simply be newly listed.
    pub min_buy_count_for_ratio: Threshold<i64>,

    /// Probe amount for each `simulateTransaction` path, in lamports.
    /// Default: 10_000_000 (0.01 SOL).
    pub sol_probe_amount_lamports: Threshold<i64>,

    /// Slippage tolerance in basis points for simulated swap transactions.
    /// Default: 500 bps (5%).
    pub simulation_slippage_bps: Threshold<u16>,

    /// When `false`, skip the simulation path and run static signals only (S1–S5).
    /// Confidence is attenuated by 0.80 when simulation is skipped.
    /// Default: `true` in production; `false` in CI/unit tests.
    pub simulation_enabled: Threshold<bool>,

    /// Additional confidence weight added when `TransferFeeConfig.authority` is a
    /// live (non-system-program) address, even if the current fee_bps is below
    /// the sell_tax_threshold. Reflects the deployer's ability to raise the fee
    /// to 100% at any time.
    pub transfer_fee_authority_extra_weight: Threshold<f64>,

    /// Re-evaluation cadence in minutes for tokens that have triggered a D01 event.
    ///
    /// Compensating control for DG3 simulation deferral: any token that produces a
    /// D01 event must be re-evaluated at this interval for the first 24 hours.
    /// Catches E10 (delayed freeze) and E13 (oracle-gated honeypot).
    ///
    /// **This value is consumed by the scheduler in `crates/server`, NOT by the
    /// detector itself.** The detector has no scheduling responsibility.
    ///
    /// See `docs/designs/0004-detector-01-honeypot.md §14`.
    pub reevaluation_interval_minutes: Threshold<u32>,

    /// Attenuated S1 (freeze authority) weight applied when the token has the
    /// Token-2022 `NonTransferable` extension (discriminator 9).
    ///
    /// NonTransferable tokens are structurally soulbound — no on-chain transfer
    /// is possible. A freeze authority on such a token is an administrative key,
    /// not a sell-gate. The raw weight is reduced from 0.25 to this value to
    /// retain the audit signal while not over-weighting a non-operational risk.
    ///
    /// Range: [0.0, 0.25). Default: 0.10.
    /// See `config/detectors.toml [honeypot_sim.non_transferable_attenuation]`.
    pub non_transferable_attenuation: Threshold<f64>,
}

/// Thresholds for D02 Rug Pull / LP Drain detector.
///
/// Source: `research/02-detection-methodology.md` §1 + Chainalysis 2025.
/// Full threshold rationale: `docs/designs/0005-detector-02-rug-pull.md` §5.
///
/// # DG-D02-1 resolution
///
/// Old fields `lp_burn_safe_floor` and `lp_lock_safe_floor` have been replaced by
/// a unified `lp_safe_floor_pct`. See design §5 Threshold changes for rationale.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugPullConfig {
    /// Fraction of LP supply removed in a single tx (or cumulatively per actor
    /// in the window) to trigger the event-based drain signal (Signal A).
    /// Default: 0.65. Source: Chainalysis 2025.
    pub lp_removal_threshold: Threshold<f64>,

    /// Minimum pool liquidity in USD. Below this, false positives dominate.
    /// Default: 1000.0. Source: Chainalysis 2025.
    pub min_pool_usd: Threshold<f64>,

    /// Minimum lifetime transaction count for the pool.
    /// Default: 100. Source: Chainalysis 2025.
    pub min_prior_txs: Threshold<i64>,

    /// Unified safe floor for effective LP protection.
    /// `effective_safe_pct = lp_burned_pct + active_locked_pct`.
    /// When `effective_safe_pct < lp_safe_floor_pct`, Signal B fires.
    /// Default: 70.0 (percent). Source: SolRPDS 2025 Table 3.
    ///
    /// Replaces old split `lp_burn_safe_floor` + `lp_lock_safe_floor` fields.
    /// See `docs/designs/0005-detector-02-rug-pull.md` §5 Threshold changes.
    pub lp_safe_floor_pct: Threshold<f64>,

    /// A lock expiring within this many days is treated as effectively unlocked
    /// for Signal B's `active_locked_pct` computation.
    /// Default: 30 days. Unverified heuristic — see spec §5.
    pub minimum_lock_horizon_days: Threshold<u32>,

    /// Additional confidence bonus when `lp_provider_count <= lp_providers_threshold`.
    /// Default: 0.15. Calibrated from RAVE probe anchor.
    /// Source: `research/token-probes/rave-FeqiF7TE.md` §2 D02.
    pub single_provider_bonus: Threshold<f64>,

    /// Observation window over which cumulative LP removal per actor is summed.
    /// Default: 60 minutes. Source: SolRPDS 2025 trickle-drain analysis.
    pub drain_window_minutes: Threshold<u32>,

    /// If LP provider count is <= this value, apply the `single_provider_bonus`.
    /// Default: 1 (only genuine single-provider pools).
    /// Source: RAVE probe §5; SolRPDS 2025.
    pub lp_providers_threshold: Threshold<i64>,

    /// 24-hour companion window for Signal A (E-D02-7 trickle drain mitigation).
    ///
    /// When a single-actor cumulative drain exceeds `lp_removal_threshold` over this
    /// extended window (but NOT in the 60-minute window), Signal A fires at a fixed
    /// confidence floor of 0.75. Evidence records `detection_window_minutes = 1440`.
    ///
    /// Default: 1440 minutes (24 hours).
    /// Source: review 0002-d02-rug-pull-evasions.md §4 recommendation #1; LROO 2026.
    pub drain_window_24h_minutes: Threshold<u32>,

    /// Maximum expiry-proximity confidence bonus for Signal B (E-D02-15 mitigation).
    ///
    /// When a locker's `unlock_at` falls within `2 × minimum_lock_horizon_days` from
    /// now, a bonus is added to Signal B confidence before the 0.75 ceiling is applied:
    ///
    /// ```text
    /// days_to_expiry = (unlock_at - now).num_days()
    /// proximity_ratio = 1.0 - days_to_expiry / (2 * minimum_lock_horizon_days)
    /// bonus = expiry_proximity_bonus_max * proximity_ratio
    /// ```
    ///
    /// This gives a warning window of up to `2 × minimum_lock_horizon_days` (90 days at
    /// default) before the locker drops out of the active window, rather than zero advance
    /// warning when the drain is executed the moment the lock crosses the horizon.
    ///
    /// Default: 0.20. Source: review 0002-d02-rug-pull-evasions.md §4 recommendation #2.
    pub expiry_proximity_bonus_max: Threshold<f64>,
}

/// Thresholds for D03 Holder Concentration detector.
///
/// Source: `research/02-detection-methodology.md` §10 + Brown 2023 + TM-RugPull 2026.
///
/// # DG-D03-5 resolution
///
/// Old fields `top10_pct_elevated`, `top10_pct_high_risk`, and `deployer_balance_max_pct`
/// have been REMOVED per `docs/designs/0006-detector-03-concentration.md` §5:
/// - `top10_pct_elevated (0.50)` — subsumed by `absolute_top10_ceiling`
/// - `top10_pct_high_risk (0.70)` — replaced by `absolute_top10_ceiling = 0.80`
///   (post-sidecar-exclusion 80% is equivalent to pre-exclusion 70%)
/// - `deployer_balance_max_pct (0.15)` — subsumed by liquid top-10 signals
///
/// New fields per spec §5:
/// - `absolute_top10_ceiling` — replaces the two-tier pair
/// - `delta_window_hours` — explicit config key (was implicit)
/// - `min_liquid_holders` — Gini reliability guard
/// - `max_lazy_classifications` — RPC cost cap per evaluation
/// - `prior_snapshot_tolerance_hours` — snapshot pipeline lag tolerance
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConcentrationConfig {
    /// 24h Gini coefficient delta above this value triggers Signal 1.
    ///
    /// Applied only when `liquid_count >= min_liquid_holders`.
    /// Default: 0.05. Source: Brown 2023.
    pub gini_delta_24h: Threshold<f64>,

    /// 24h liquid-only top-10 holder percentage delta above this value triggers Signal 2.
    ///
    /// Applied only when `liquid_count >= min_liquid_holders`.
    /// Default: 0.10. Source: TM-RugPull 2026, RugCheck DANGER tier.
    pub top10_pct_delta_24h: Threshold<f64>,

    /// Liquid-only top-10 holder share at or above this value triggers Signal 3 (absolute ceiling).
    ///
    /// Signal 3 is cold-start capable — it fires on the first snapshot without a prior.
    /// Default: 0.80. Source: TM-RugPull 2026.
    pub absolute_top10_ceiling: Threshold<f64>,

    /// Window in hours for computing the delta between the current and prior snapshot.
    ///
    /// Default: 24 hours. Source: Brown 2023, TM-RugPull 2026.
    pub delta_window_hours: Threshold<u32>,

    /// Tolerance window (±hours) when looking up the prior snapshot in history.
    ///
    /// Accommodates pipeline lag without pulling in snapshots too far from the target time.
    /// Default: 2 hours.
    pub prior_snapshot_tolerance_hours: Threshold<u32>,

    /// Minimum liquid holder count for Signals 1 and 2 to evaluate.
    ///
    /// Gini is statistically unreliable for small populations. Below this, only
    /// Signal 3 (absolute ceiling) is evaluated. Default: 50. Source: Brown 2023 §3.
    pub min_liquid_holders: Threshold<u32>,

    /// Maximum number of `ctx.registry.classify_holder()` calls per `evaluate()` invocation.
    ///
    /// Bounds RPC cost. Top-N unclassified holders (by balance_raw) are classified first.
    /// Default: 10.
    pub max_lazy_classifications: Threshold<u32>,
}

/// Thresholds for D04 Pump & Dump detector.
///
/// Source: `research/02-detection-methodology.md` §3 + Karbalaii 2025 + Bolz 2024.
/// Full threshold rationale: `docs/designs/0007-detector-04-pump-dump.md` §4.
///
/// # Field rename from architect stub
///
/// `burst_concentration_ratio_threshold` renamed to `burst_concentration_threshold`
/// for consistency with other detector naming conventions. Config TOML updated accordingly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PumpDumpConfig {
    /// 1-hour price spike as a fraction of window-open price.
    /// Default: 0.30 (30%). Source: Chainalysis 2025 + Karbalaii 2025.
    pub price_spike_pct: Threshold<f64>,

    /// Ratio of 1h volume to 7-day daily median volume.
    /// Default: 5.0. Source: Karbalaii 2025 + Bolz 2024.
    pub volume_multiplier: Threshold<f64>,

    /// Minimum days of baseline history required to use the volume_multiplier
    /// check. Below this, the detector falls back to burst_concentration_threshold.
    /// Source: RAVE probe §4 Gap 2 — zero-baseline case.
    /// Default: 3.
    pub min_baseline_days: Threshold<u32>,

    /// Fallback signal when baseline is unavailable (WET gap / RAVE gap):
    /// `volume_1h / volume_24h` above this threshold fires the detector at
    /// reduced confidence. Renamed from `burst_concentration_ratio_threshold`.
    /// Default: 0.90. Source: RAVE/WET probe analysis.
    pub burst_concentration_threshold: Threshold<f64>,

    /// Fraction of insider (deployer cluster) holdings sold within
    /// `post_pump_insider_window_hours` of the spike to confirm the dump phase.
    /// Default: 0.40. Source: Chainalysis 2025.
    pub insider_sell_pct: Threshold<f64>,

    /// Additive confidence boost applied to Signal A or B when Signal C confirms
    /// insider sell-off. Capped at 0.95 for Signal A base, 0.85 for Signal B base.
    /// Default: 0.15. Source: confidence-formula design (docs/designs/0007 §5.3).
    pub insider_amplifier: Threshold<f64>,

    /// Hours after the pump spike within which insider sells are monitored.
    /// Default: 24. Source: Karbalaii 2025 + Chainalysis 2025.
    pub post_pump_insider_window_hours: Threshold<u32>,

    /// Market cap (FDV) above which the token is excluded from pump detection.
    /// Default: 60_000_000 (USD). Source: Bolz et al. 2024 §4.2.
    pub market_cap_filter_usd: Threshold<f64>,

    /// Minimum 1h volume (USD) required before Signal B fires.
    /// Below this, the burst is likely noise in a thin-market token.
    /// Default: 5000.0. Source: dust filter (unverified-heuristic).
    pub min_burst_volume_usd: Threshold<f64>,

    /// Minimum fraction of total supply a holder must hold to be included in
    /// the Signal C Priority 2 top_holders_proxy (when deployer_clusters is absent).
    /// Default: 0.01 (1%). Source: docs/designs/0007 §10.
    pub top_holders_insider_floor_pct: Threshold<f64>,

    // ---- Smart-money amplification (Sprint 23, design 0023 §4.1) ----

    /// Pre-pump window in minutes for the smart-money buyer-set computation (Decision 3).
    ///
    /// Wallets that bought the token within this window before the evaluation window start
    /// are eligible for smart-money amplification.
    /// Default: 60. Source: Fantazzini & Xiao 2023 (Econometrics 11(3)) — 60-min pre-event window.
    pub pre_pump_window_minutes: Threshold<u32>,

    /// Confidence delta when >= 1 Tier1 smart-money wallet bought in the pre-pump window.
    ///
    /// unverified-heuristic; Perseus 2025 (arXiv:2503.01686): masterminds buy pre-event in
    /// 100% of confirmed pump events. Design derivation: +0.12 moves threshold confidence
    /// (0.60) to 0.72 (Medium → High boundary). See design 0023 §4.1 Decision 2.
    pub smart_money_tier1_delta: Threshold<f64>,

    /// Confidence delta when >= `smart_money_tier2_min_count` Tier2 wallets bought in
    /// the pre-pump window and no Tier1 wallet was found.
    ///
    /// unverified-heuristic; two independent Tier2 data points reduce single-event luck.
    /// See design 0023 §4.1 Decision 2.
    pub smart_money_tier2_delta: Threshold<f64>,

    /// Minimum Tier2 wallet count in the pre-pump window to unlock Tier2 amplification.
    ///
    /// Noise floor: a single Tier2 buyer is consistent with luck (base-rate argument).
    /// Default: 2. See design 0023 §4.1 Decision 2.
    pub smart_money_tier2_min_count: Threshold<u32>,
}

/// Thresholds for D05 Signal B — Graph Cycle Detection (Tarjan SCC + Johnson).
///
/// Replaces the old cluster flow balance proxy (`min_cluster_size`,
/// `cluster_balance_tolerance_pct`, `min_cluster_volume_usd`, `top_senders_cap`).
///
/// Design: `docs/designs/0017-d05-signal-b-graph-cycles.md` §7.
/// References: Tarjan 1972; Johnson 1975; Victor & Weintraud 2021; Chainalysis 2025.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignalBCyclesConfig {
    /// Maximum elementary cycle length (number of hops = number of wallets in ring).
    /// Default: 5. Source: Victor & Weintraud 2021 §4.2 (median ring 3–4 wallets).
    pub max_cycle_length: Threshold<usize>,

    /// Time window (minutes) within which all edges in a cycle must fall.
    /// Default: 120. Source: user decision 2026-04-24; spec 0017 §7.
    pub max_cycle_window_minutes: Threshold<u64>,

    /// Minimum USD volume of the bottleneck edge of a qualifying cycle.
    /// Default: 1000.0. Source: Victor & Weintraud 2021 §4.2 (median $5K–$20K).
    pub min_cycle_volume_usd: Threshold<f64>,

    /// Maximum cycles enumerated per SCC to bound Johnson's worst-case latency.
    /// Default: 100. Source: spec 0017 §4.4.
    pub max_cycles_per_scc: Threshold<usize>,

    /// Minimum SCC size to pass to Johnson's algorithm (singleton + 2-vertex SCCs
    /// are dropped; 2-vertex SCCs produce only 2-hop cycles covered by Signal A).
    /// Default: 3. Source: Tarjan 1972 §2; Johnson 1975 §1.
    pub min_scc_size: Threshold<usize>,

    /// Safety ceiling on transfer rows fetched per evaluation window.
    /// Default: 10000. Source: spec 0017 §2.3.
    pub max_transfers_per_window: Threshold<u32>,
}

/// Thresholds for D05 Wash Trading (Heuristic 1) detector.
///
/// Source: `research/02-detection-methodology.md` §4 + Chainalysis 2025.
/// Full threshold rationale: `docs/designs/0008-detector-05-wash-trading.md` §5.
///
/// # Sprint 12 T2-2 change
///
/// Signal B cluster-flow fields (`min_cluster_size`, `cluster_balance_tolerance_pct`,
/// `min_cluster_volume_usd`, `top_senders_cap`) replaced by `signal_b_cycles`
/// (a `SignalBCyclesConfig` sub-section). See `docs/designs/0017`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WashTradingConfig {
    /// Solana slot window for buy+sell round-trip matching (Signal A).
    ///
    /// 25 slots ≈ 10 seconds at 400ms/slot (Solana). Chainalysis canonical value.
    /// Named `block_window_slots` to distinguish from EVM block counts.
    pub block_window_slots: Threshold<i64>,

    /// Maximum fractional volume difference between buy and sell token amounts.
    ///
    /// Signal A: `|buy_amount - sell_amount| / max(buy, sell) <= volume_diff_pct`.
    /// Default: 0.01 (1%). Source: Chainalysis 2025.
    pub volume_diff_pct: Threshold<f64>,

    /// Minimum qualifying round-trip pairs per (sender, pool) to fire Signal A.
    ///
    /// Default: 3. Source: Chainalysis 2025.
    pub min_repetitions: Threshold<i64>,

    /// Wash-volume-to-pool-volume ratio threshold for Signal C severity upgrade.
    ///
    /// When `wash_volume / total_pool_volume >= threshold`, severity upgrades one band.
    /// Default: 0.30 (30%). Source: Victor & Weintraud 2021.
    pub severity_amplifier_ratio: Threshold<f64>,

    /// Observation window in hours for all signals.
    ///
    /// Default: 24h. Source: Chainalysis 2025; D04 window consistency.
    pub detection_window_hours: Threshold<i64>,

    /// Minimum pool USD liquidity to evaluate Signal A or B.
    ///
    /// Pools below this value are too thin to produce meaningful wash-trading signal.
    /// Default: 10000.0 (USD). Source: design derivation; unverified-heuristic.
    pub min_pool_usd_for_h1: Threshold<f64>,

    /// Minimum total wash volume USD for Signal A confidence formula denominator.
    ///
    /// Prevents log(0) and stabilises the formula for tiny-volume round trips.
    /// Default: 500.0 (USD). Source: design derivation; unverified-heuristic.
    pub min_wash_volume_usd: Threshold<f64>,

    /// Signal B — Graph Cycle Detection thresholds (T2-2, Sprint 12).
    ///
    /// Replaces old cluster-flow-balance proxy. See design 0017.
    pub signal_b_cycles: SignalBCyclesConfig,
}

/// Thresholds for D06 Mint/Burn Anomaly detector.
///
/// Source: `research/02-detection-methodology.md` §9 + Xia et al. 2021 + Sun et al. 2024.
/// Full threshold rationale: `docs/designs/0009-detector-06-mint-burn.md` §4.
///
/// # Threshold rename
///
/// `supply_change_pct` is retained as the canonical TOML key (backward compatible)
/// and exposed as `supply_change_threshold_pct` in code for clarity.
/// Both names refer to the 5% per-event supply change threshold (Signal B gate).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MintBurnConfig {
    /// Per-event supply change threshold (as fraction of circulating supply)
    /// required to trigger Signal B.
    ///
    /// Default: 0.05 (5%). Source: Xia et al. 2021; research/02-detection-methodology.md §9.
    ///
    /// TOML key: `supply_change_pct` (retained for backward compatibility).
    #[serde(rename = "supply_change_pct")]
    pub supply_change_threshold_pct: Threshold<f64>,

    /// Grace period in days after token deployment during which Signal A does NOT fire.
    ///
    /// Legitimate new projects often deploy with mint authority for genesis minting
    /// (airdrops, LP seeding) and revoke it within the first week.
    /// Default: 7. Source: Sun et al. 2024 §4 hidden mint pattern.
    pub mint_authority_grace_period_days: Threshold<u64>,

    /// Cumulative non-LP supply increase (as fraction of circulating supply) over
    /// `hidden_mint_window_days` required to trigger Signal C.
    ///
    /// Default: 0.20 (20%). Source: Sun et al. 2024 §4 hidden mint category.
    pub hidden_mint_cumulative_pct: Threshold<f64>,

    /// Rolling window in days for Signal C cumulative supply change calculation.
    ///
    /// Default: 30. Source: research/02-detection-methodology.md §Cross-cutting C.
    pub hidden_mint_window_days: Threshold<u64>,

    /// Minimum token age in days before Signal C evaluates.
    ///
    /// Prevents Signal C from firing on genesis-phase minting activity.
    /// Default: 14. Source: design derivation; no prior art.
    pub min_token_age_days_for_hidden_mint: Threshold<u64>,

    /// Confidence multiplier applied to Signal A confidence when
    /// `is_established_protocol(meta) = true`.
    ///
    /// Signal A is dampened (not suppressed) for established protocols to preserve
    /// audit observability. Default: 0.5. Source: token_status.rs P4-0; D02 §14.
    pub established_protocol_confidence_dampening: Threshold<f64>,

    /// Additive confidence weight added to Signal B when the mint recipient is
    /// NOT a known LP contract address.
    ///
    /// Default: 0.30. Source: Sun et al. 2024 hidden mint recipient analysis.
    pub non_lp_recipient_signal_weight: Threshold<f64>,
}

/// Thresholds for D07 Token-2022 Withdraw-Withheld Drain detector.
///
/// Source: docs/designs/0012-detector-07-withdraw-withheld.md §7 Threshold Table.
/// All thresholds are unverified-heuristic pending Sprint 6 corpus calibration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WithdrawWithheldConfig {
    /// Minimum count of `WithdrawWithheld*` instructions in the detection window
    /// for Signal A to fire.
    /// Default: 3. Single events can be legitimate protocol fee collection.
    pub min_extraction_events: Threshold<u32>,

    /// Minimum cumulative USD value extracted in the detection window for Signal A
    /// to fire (when price data is available).
    /// Default: 1000.0. Dust filter; consistent with D02 `min_pool_usd`.
    pub min_cumulative_withdraw_usd: Threshold<f64>,

    /// Lookback window in days for Signal B authority rotation detection.
    /// Default: 30. Consistent with D02/D06 30-day windows.
    pub authority_rotation_window_days: Threshold<u32>,

    /// Authorities holding the role for fewer than this many days before rotation
    /// are classified as disposable keys (rapid rotation). Default: 7.
    pub min_authority_tenure_days: Threshold<u32>,

    /// Minimum USD value accumulated in the mint's withheld balance at the time
    /// of a rotation for the fresh_wallet_bonus to apply. Default: 500.0.
    pub min_withheld_at_rotation_usd: Threshold<f64>,

    /// A wallet that received its first SOL within this many hours before being
    /// set as `withdraw_withheld_authority` is classified as a disposable key.
    /// Default: 48.
    pub fresh_wallet_funding_hours: Threshold<u32>,

    /// Detection window in hours for Signal A extraction event accumulation.
    /// Default: 168 (7 days). Consistent with D04/D05.
    pub detection_window_hours: Threshold<u32>,

    /// Whether to emit the `combined_with_d01_s2` evidence key.
    /// Default: true.
    pub cross_detector_composite_enabled: Threshold<bool>,

    /// Maximum extraction-to-pool-volume ratio allowed before Signal A fires on
    /// established protocols. Default: 0.50 (50%).
    /// Above this ratio, Signal A fires regardless of `is_established_protocol`.
    /// Lowered from 0.90 → 0.50 per review 0004 §4 T2 (E-D07-12 mitigation).
    pub established_protocol_fee_extraction_allowlist_pct: Threshold<f64>,

    /// Single-event USD floor for two-tier Signal A (E-D07-9 mitigation).
    ///
    /// When `event_count == 1` AND `cumulative_usd >= this value`, Signal A fires
    /// at confidence 0.65 (`detection_tier = "single_event"`).
    /// When `event_count == 2` AND `cumulative_usd >= min_cumulative_withdraw_usd`,
    /// Signal A fires at confidence 0.60 (`detection_tier = "two_event"`).
    /// `event_count >= min_extraction_events` uses the primary formula (`detection_tier
    /// = "recurring"`).
    ///
    /// Default: 5000.0. Set to 5× `min_cumulative_withdraw_usd` so a single event
    /// represents meaningful value before firing.
    /// See review 0004 §4 T1.
    pub min_single_event_withdraw_usd: Threshold<f64>,
}

/// Thresholds for D08 Sybil (bundled-launch) detector.
///
/// Source: docs/designs/0015-crates-graph-phase3.md §6.4
/// Prior art: Liu et al. 2025 (arxiv:2505.09313); Chainalysis 2025 (Heuristic 2).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SybilConfig {
    /// Fraction of a common-funder cluster that must appear as token holders
    /// for Signal A to fire.
    ///
    /// `top_holder_overlap_pct = token_holders_in_cluster / sybil_cluster_member_count`
    ///
    /// Default: 0.30 (30%). Source: Liu et al. 2025 (arxiv:2505.09313) — "fraction of
    /// cluster members holding the token" is a top-5 feature in their LightGBM
    /// Sybil classifier. Chainalysis 2025 Heuristic 2 uses ≥5 funded addresses as
    /// the wash-trading analog. Low floor to maximise recall per CLAUDE.md heuristic:
    /// "false negatives are expensive".
    pub sybil_cluster_top_holder_pct_threshold: Threshold<f64>,

    /// Minimum common-funder cluster size for D08 to evaluate.
    ///
    /// Clusters below this size are ignored. Default: 3. Source: Liu et al. 2025;
    /// Chainalysis 2025 Heuristic 2 lower bound. Matches `graph.toml`
    /// `min_cluster_size` to ensure D08 fires on any cluster that ClusterDetector emits.
    pub sybil_cluster_min_size: Threshold<u32>,

    // ---- Smart-money amplification (Sprint 23, design 0023 §4.2) ----

    /// Confidence delta when any Tier1 smart-money wallet is a cluster member.
    ///
    /// Amplification direction is UPWARD: Tier1 in a Sybil cluster = informed coordinated
    /// attacker, NOT a legitimate skilled trader (domain framing, design 0023 §1.2).
    /// unverified-heuristic; Perseus 2025 (arXiv:2503.01686) + Liu et al. 2025
    /// (arXiv:2505.09313) behavioral anchor. Default: 0.10. See design 0023 §4.2 + Decision 4.
    pub smart_money_tier1_delta: Threshold<f64>,

    /// Confidence delta when any Tier2 smart-money wallet is a cluster member and no Tier1
    /// was found.
    ///
    /// unverified-heuristic. Default: 0.05. See design 0023 §4.2.
    pub smart_money_tier2_delta: Threshold<f64>,

    /// Minimum Tier2 wallet count in the cluster to unlock Tier2 amplification.
    ///
    /// Default: 2. See design 0023 §4.2 (user-approved decision: Tier1=+0.10, Tier2=+0.05).
    pub smart_money_tier2_min_count: Threshold<u32>,
}

/// Thresholds for D10 Launch Audit detector.
///
/// Source: research/03-feature-gap-2026-04-24.md §T1-1
/// Prior art: RugWatch (machenxi + rookiester, 2024–2025); Alhaidari et al. 2025 (SolRPDS).
///
/// # Sprint 24 EVM expansion
///
/// `initial_liquidity_floor_sol` (chain-specific SOL threshold) replaced by
/// `initial_liquidity_usd_threshold` (chain-agnostic USD threshold). The SOL field
/// and `sol_price_usd_fallback` are removed — USD is the canonical unit.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LaunchAuditConfig {
    /// Minimum initial liquidity at pool genesis, in USD (chain-agnostic).
    ///
    /// Below this threshold, Signal A fires. Default: 750.0 USD.
    /// USD-equivalent of the RugWatch 5 SOL threshold at $150/SOL.
    /// Chain-agnostic: ~0.25 ETH @ $3000, ~1.25 BNB @ $600, ~1875 MATIC @ $0.40.
    /// Source: RugWatch (machenxi + rookiester, 2024–2025); Sprint 24 EVM expansion.
    pub initial_liquidity_usd_threshold: Threshold<f64>,

    /// LP lock safe floor percentage.
    ///
    /// Used for evidence display only — Signal B gates on `lp_locked_pct == 0.0` exactly.
    /// Default: 0.70 (70%). Source: Alhaidari et al. 2025 (SolRPDS) Table 3.
    pub lp_safe_floor_pct: Threshold<f64>,
}

/// Thresholds for D09 BOCPD Deployer Changepoint detector.
///
/// Source: docs/designs/0016-detector-09-bocpd-deployer-changepoint.md §7
/// Primary references: Adams & MacKay 2007 (arXiv:0710.3742), Murphy 2007,
/// latent-flux (#10) production BOCPD deployment.
///
/// # Normal-Gamma prior (§7 spec defaults)
///
/// The prior is weakly informative: `mu_0 = 0.20` (expected composite score for a
/// typical legitimate deployer), `kappa_0 = 1.0` (equivalent to 1 prior observation),
/// `alpha_0 = 3.0` (well-defined variance from the start), `beta_0 = 1.0`.
/// These are locked decisions from Sprint 12 and MUST NOT be changed without re-reading
/// docs/designs/0016 §3.3 and updating bocpd_deployer_state for all existing deployers.
///
/// # Composite weights (§2.3 spec defaults)
///
/// `w0 + w1 + w2 + w3 + w4 = 1.0` (validated by `CompositeWeights::validate()`).
/// Features: F1=log_gap_seconds, F2=lp_locked_pct, F3=log_initial_liquidity_usd,
/// F4=holder_count_at_1h, F5=prior_rug_rate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployerChangepointConfig {
    // ---- Alert threshold ----
    /// P(r_t = 0 | x_{1:t}) above this threshold fires an alert.
    ///
    /// At the default 0.50, the changepoint posterior must be the mode before
    /// alerting. Lower values increase recall at the cost of false positives.
    /// Source: Adams & MacKay 2007 §4; latent-flux (#10) production deployment.
    pub changepoint_prob_threshold: Threshold<f64>,

    /// Minimum token launches observed from a deployer before D09 fires any alert.
    ///
    /// Below this count, the BOCPD posterior is too diffuse for a meaningful changepoint
    /// signal. Default: 3. Source: spec §6.1 warm-up guard; unverified-heuristic.
    pub min_history_length: Threshold<u32>,

    // ---- BOCPD hazard ----
    /// Constant hazard rate H = 1/hazard_rate_denom for the run-length prior.
    ///
    /// P(changepoint at t) = 1/hazard_rate_denom per token launch.
    /// At 300, the expected run length is 300 token launches between regime shifts —
    /// correct for a legitimate deployer whose pattern is stable across hundreds of tokens.
    /// Source: latent-flux (#10) confirmed H=1/300 for deployer behavior on EVM chains.
    pub hazard_rate_denom: Threshold<u32>,

    // ---- Normal-Gamma hyperparameters ----
    /// Prior mean of the composite score under the null (normal) regime.
    ///
    /// 0.20 corresponds to a deployer with 80% LP locked, moderate liquidity, and
    /// low rug rate — representative of a legitimate meme-coin deployer.
    /// Source: spec §3.3; calibrated from latent-flux deployer corpus.
    pub mu_0: Threshold<f64>,

    /// Prior virtual sample count (strength of the prior mean).
    ///
    /// kappa_0 = 1.0 makes the prior equivalent to 1 prior observation.
    /// Source: Murphy 2007 §2.1 Normal-Gamma hyperparameter guide.
    pub kappa_0: Threshold<f64>,

    /// Shape parameter of the Gamma prior over precision.
    ///
    /// alpha_0 = 3.0 > 1 ensures a well-defined prior variance from the first observation.
    /// Source: Murphy 2007 §2.1; spec §3.3 locked decision.
    pub alpha_0: Threshold<f64>,

    /// Rate parameter of the Gamma prior over precision.
    ///
    /// beta_0 = 1.0. Together with alpha_0, this sets the prior predictive variance to
    /// beta_0 / ((alpha_0 - 0.5) * kappa_0) ≈ 0.40 — consistent with S_t ∈ [0,1] range.
    /// Source: Murphy 2007 §2.1; spec §3.3 locked decision.
    pub beta_0: Threshold<f64>,

    /// Maximum number of run-length slots tracked in the BOCPD posterior.
    ///
    /// At 300, the state vector is bounded: O(300) memory per deployer.
    /// Run-length mass beyond this slot is folded into the last slot (absorbing boundary).
    /// Source: spec §3.5; latent-flux (#10) confirmed state-vector bound.
    pub max_run_length_tracked: Threshold<u32>,

    // ---- Composite score weights (F1..F5) ----
    // INVARIANT: w0 + w1 + w2 + w3 + w4 = 1.0 (enforced by CompositeWeights::validate())
    /// Weight for F1: log_gap_seconds term.
    ///
    /// Higher w0 → deployer launch cadence regime shift dominates the score.
    /// Source: spec §2.3 Table 2.
    pub w0: Threshold<f64>,

    /// Weight for F2: (1 - lp_locked_pct) term.
    ///
    /// Higher w1 → LP protection regime shift dominates the score.
    /// Source: spec §2.3 Table 2; Alhaidari et al. 2025 (SolRPDS) top-3 predictor.
    pub w1: Threshold<f64>,

    /// Weight for F3: (1 - sigmoid(log_initial_liquidity_usd/8)) term.
    ///
    /// Higher w2 → initial liquidity anomaly dominates the score.
    /// Source: spec §2.3 Table 2.
    pub w2: Threshold<f64>,

    /// Weight for F4: (1 - sigmoid(holder_count_at_1h/100)) term.
    ///
    /// Higher w3 → holder count anomaly at launch dominates the score.
    /// Source: spec §2.3 Table 2.
    pub w3: Threshold<f64>,

    /// Weight for F5: prior_rug_rate term.
    ///
    /// Higher w4 → deployer rug history dominates the score.
    /// Source: spec §2.3 Table 2; Chainalysis 2025 "94% of rugged tokens have deployer history".
    pub w4: Threshold<f64>,

    // ---- Scoring output ----
    /// Minimum `confidence` at which D09 event is stored in `anomaly_events`.
    ///
    /// Events below this floor are computed but discarded (no DB write).
    /// Default: 0.30 — matches other detectors' low-confidence suppression floor.
    /// Source: spec §6.2; unverified-heuristic.
    pub rug_confidence_threshold: Threshold<f64>,
}

// ---------------------------------------------------------------------------
// Top-level config container
// ---------------------------------------------------------------------------

/// Thresholds for D11 Synchronized-Activity Clustering detector.
///
/// Source: docs/designs/0018-detector-11-synchronized-activity.md §9.
/// Primary references:
/// - Mazza, Cresci et al. 2019 (RTbust, arXiv:1902.04506) — DBSCAN + cluster size.
/// - Mannocci, Mazza et al. 2024 (CIB Survey, arXiv:2408.01257) — Jaccard + Poisson null.
/// - Arnold et al. 2024 (Temporal Motifs, arXiv:2402.09272) — N_min derivation.
/// - research/sprint13-b-citations.md — δ=30s, N_min=5, Poisson framework formulation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SynchronizedActivityConfig {
    /// Window width in seconds for time-bucket formation (Step 2 of §3.1).
    ///
    /// Default 30s: 2 Solana slot-equivalents at ~400ms; above single-slot MEV arb
    /// range; below human-reaction-time coordination. Phase 4: adjust per chain.
    /// Source: research/sprint13-b-citations.md §"Suggested Signal/Threshold".
    pub window_seconds: Threshold<u32>,

    /// Minimum number of distinct wallets required in a DBSCAN cluster to fire.
    ///
    /// Default 5: midpoint between Arnold et al. 2024 (N_min=3) and RTbust (N>=10).
    /// Source: research/sprint13-b-citations.md §"Suggested Signal/Threshold".
    pub min_cluster_size: Threshold<u32>,

    /// Poisson null model p-value threshold. Clusters with p_value > this are discarded.
    ///
    /// Default 1e-6: lenient relative to the k=5, λ=1/hour, δ=30s derivation
    /// (p≈4e-10) to accommodate higher-λ tokens. Source: research/sprint13-b-citations.md.
    pub poisson_p_threshold: Threshold<f64>,

    /// Minimum temporal tightness score [0.0, 1.0] for a cluster to pass.
    ///
    /// Default 0.50 (unverified-heuristic): midpoint of [0, 1.0]. Calibrate against
    /// Sprint 14 fixture corpus once available.
    pub temporal_tightness_threshold: Threshold<f64>,

    /// Jaccard similarity threshold for DBSCAN neighborhood.
    ///
    /// Wallets with J(i,j) >= this threshold are DBSCAN neighbors.
    /// eps = 1 - jaccard_similarity_threshold.
    /// Default 0.70 (unverified-heuristic): midpoint of [0.5, 0.9] per Mannocci 2024.
    pub jaccard_similarity_threshold: Threshold<f64>,

    /// Maximum lookback window in minutes for event fetching.
    ///
    /// Default 10 minutes. The 7-day baseline window (λ computation) is separate.
    pub max_lookback_minutes: Threshold<u32>,

    /// Maximum number of raw events to fetch per evaluation (safety ceiling).
    ///
    /// Analogous to D05 Signal B max_transfers_per_window. Default 10,000.
    pub max_events_per_window: Threshold<u32>,

    /// Maximum number of distinct wallets to include in DBSCAN (O(n^2) guard).
    ///
    /// Default 500: 250,000 distance evaluations worst case at sub-μs each ≈ 0.25ms.
    pub max_wallets_per_cluster_cap: Threshold<u32>,

    /// Minimum number of historical events in the 7-day window for Poisson warmup guard.
    ///
    /// Default 10. Below this, Poisson baseline is unreliable → skip evaluation.
    /// Analogous to D04 min_baseline_days guard. Source: design 0018 §5.4.
    pub min_baseline_events: Threshold<u32>,

    /// Cadence in seconds between D11 evaluations per tracked token.
    ///
    /// Analogous to D08 sybil_cluster_cadence_seconds. Default 120.
    pub cadence_seconds: Threshold<u32>,

    /// Whether to suppress D11 events for established-protocol tokens.
    ///
    /// Default false (consistent with D08 Sybil non-suppression; gotcha #42).
    /// Decision 7 of design 0018 §11: do NOT suppress by default.
    pub suppress_established_protocols: Threshold<bool>,

    /// Confidence weight for cluster size sub-signal (S_size, §4.1).
    ///
    /// Default 0.40: RTbust 2019 ranks cluster size as strongest predictor.
    pub weight_cluster_size: Threshold<f64>,

    /// Confidence weight for temporal tightness sub-signal (S_tight, §4.1).
    ///
    /// Default 0.30: equal weight with statistical significance (S_stat).
    pub weight_temporal_tightness: Threshold<f64>,

    /// Confidence weight for statistical significance sub-signal (S_stat, §4.1).
    ///
    /// Default 0.30: equal weight with temporal tightness (S_tight).
    pub weight_statistical_significance: Threshold<f64>,

    /// Sigmoid stretch factor for cluster size sub-signal (S_size, §4.1).
    ///
    /// At cluster_size = min_cluster_size + cluster_size_scale: S_size ≈ 0.73.
    /// Default 5.0: a 10-wallet cluster produces S_size ≈ 0.73 (meaningfully elevated).
    pub cluster_size_scale: Threshold<f64>,
}

/// Thresholds for D12 Permit2 Drainer detector.
///
/// Source: docs/designs/0019-detector-12-permit2-drainer.md §9
/// Primary references:
/// - Scam Sniffer 2024 Annual Report §methodology (https://scamsniffer.io/reports/2024-annual/)
/// - ZachXBT Telegram (Pink Drainer scale); Dune beetle/pink-drainer dashboard
/// - Blockaid blog 2024-02-05 (Angel Drainer / Ethena)
/// - Uniswap Permit2 GitHub (https://github.com/Uniswap/permit2)
///
/// # Suppression policy
///
/// Per gotcha #17 + design 0019 §5.3: D12 does NOT suppress on established protocols.
/// USDC, WETH, and wBTC are the most commonly drained tokens — suppressing on established
/// protocols would eliminate the most important signals.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PermitDrainerConfig {
    /// Minimum USD-equivalent value for a drain event to be reported.
    ///
    /// Default: $100. Calibration: Pink Drainer p5 victim loss ~$200; $100 provides
    /// safety margin while capturing >97% of victim volume.
    /// Source: Dune beetle/pink-drainer dashboard (public 2026-04-24).
    /// String Decimal — never float.
    pub min_amount_usd: Threshold<String>,

    /// Lookback window in minutes for fetching transfers and permit events.
    ///
    /// Default: 60 minutes. Drains complete in one block (~12s on mainnet);
    /// 60 minutes is a generous scheduler buffer.
    pub lookback_minutes: Threshold<u32>,

    /// Minimum PermitBatch size to trigger the batch_size_bonus (+0.10 conf).
    ///
    /// Default: 2. PermitBatch with N >= this value is a drainer-template signal.
    /// Legitimate batch swaps via UniversalRouter are allowlisted and never reach this gate.
    pub min_batch_size: Threshold<u32>,

    /// Confidence weight for A1 (known-drainer cluster match).
    ///
    /// Default: 0.70. Known-drainer label is near-ground-truth (post-hoc forensic label).
    /// Residual 0.30 reflects label lag for re-used infrastructure wallets.
    /// Source: design 0019 §4.1; classified unverified-heuristic.
    pub conf_weight_a1: Threshold<String>,

    /// Confidence weight for A2 (structural Permit2 correlation).
    ///
    /// Default: 0.55. Permit + same-tx Transfer is structurally strong but FP risk
    /// from allowlist gaps brings it below definitive. See design 0019 §4.1.
    pub conf_weight_a2: Threshold<String>,

    /// Confidence bonus for PermitBatch with N >= min_batch_size.
    ///
    /// Default: 0.10. Legitimate batch swaps are allowlisted; batch drains are not.
    pub conf_bonus_batch: Threshold<String>,

    /// Confidence bonus for max uint160 approval amount.
    ///
    /// Default: 0.05. Legitimate swaps use exact amounts; max approval is a drainer template.
    pub conf_bonus_max_approval: Threshold<String>,

    /// Maximum confidence cap.
    ///
    /// Default: 0.95. Loss-of-funds severity; 5% residual uncertainty maintained.
    /// Analogous to D02 rug pull cap.
    pub conf_cap: Threshold<String>,

    /// Minimum confidence to emit an AnomalyEvent.
    ///
    /// Default: 0.05 (per CLAUDE.md: "false positives are cheap, false negatives expensive").
    pub min_emit_confidence: Threshold<String>,

    /// Known drainer wallet addresses (lowercase hex with 0x prefix).
    ///
    /// Seed list from public disclosures. Sources:
    /// - Scam Sniffer 2023-12-23 (Inferno Drainer infrastructure wallets)
    /// - ZachXBT May 2024 (Pink Drainer fee wallet)
    /// - Blockaid 2024-02-05 (Angel Drainer)
    ///
    /// Per ADR 0003: no runtime Scam Sniffer API call. This list is static and
    /// updated manually per sprint.
    pub known_drainer_addresses: Threshold<Vec<String>>,

    /// Spender addresses known to be legitimate Permit2 routers (DEX routers).
    ///
    /// Exact 20-byte match after lowercase normalization. No partial matching.
    /// A2 signal is suppressed when `permit.spender` is in this list.
    /// Source: design 0019 §5.2.
    pub known_legitimate_permit2_spenders: Threshold<Vec<String>>,

    /// Suppress on established protocols?
    ///
    /// Default: false. D12 does NOT suppress — USDC/WETH/wBTC are prime drain targets.
    /// Consistent with D08 + D11 non-suppression policy (gotcha #17).
    pub suppress_established_protocols: Threshold<bool>,

    /// Unknown token USD fallback.
    ///
    /// "0" means: do not estimate, conservatively pass the threshold gate.
    /// Prevents oracle-dependency in D12 hot path (ADR 0003).
    pub unknown_token_usd_fallback: Threshold<String>,
}

/// Thresholds for D13 Sandwich/MEV detector.
///
/// Source: docs/designs/0021-detector-13-sandwich-mev.md §9
/// Primary references:
/// - Daian et al. 2019 (Flash Boys 2.0, arXiv:1904.05234) — base 0.55 derivation
/// - Chi, He, Hu & Wang 2024 (arXiv:2405.17944) — profit gate ($10 min), slippage distribution
/// - Flashbots mev-inspect-py (archived) — structural 3-swap F-V-B classifier
///
/// # Suppression policy
///
/// Settlement allowlist addresses are hardcoded in the detector (ADR 0003: no runtime API).
/// `settlement_allowlist_extra` provides an operator extension point.
/// `suppress_established_protocols` defaults false (consistent with D08/D11/D12).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandwichMevConfig {
    /// Minimum victim slippage fraction for an event to fire.
    ///
    /// Default 0.005 (0.5%). Chi et al. 2024 median sandwich-imposed slippage ≈ 0.8%.
    /// Values below 0.5% are indistinguishable from normal AMM price impact.
    /// Source: design 0021 Decision 4; Chi et al. 2024 §3 distribution.
    /// String Decimal — never float.
    pub min_victim_slippage_pct: Threshold<String>,

    /// Minimum attacker profit in USD for the profit bonus to fire.
    ///
    /// Default $10. Chi et al. 2024 median sandwich profit ~$32; $10 provides safety margin.
    /// Gates the +0.15 profit confidence bonus ONLY — structural match at conf 0.55
    /// still fires below this threshold. Source: design 0021 Decision 5.
    /// String Decimal — never float.
    pub min_attacker_profit_usd: Threshold<String>,

    /// Minimum victim swap size in USD for event evaluation.
    ///
    /// Default $1000. Below this, sandwich is economically marginal (gas cost > profit).
    /// Source: Chi et al. 2024 §3.1 — minimum profitable sandwich ≈ $500 on liquid pools.
    /// String Decimal — never float.
    pub min_victim_swap_usd: Threshold<String>,

    /// Base confidence for structural A1 pattern match (F-V-B same pool, same attacker).
    ///
    /// Default 0.55. Above 0.50 (more likely than not); below 0.70 (not definitive).
    /// Source: design 0021 §4.1 derivation; Flashbots mev-inspect-py classifier.
    pub confidence_base: Threshold<String>,

    /// Profit confirmation bonus (added when profit_usd > min_attacker_profit_usd).
    ///
    /// Default 0.15. Chi et al. 2024 profit criterion is strong but secondary to structure.
    /// Source: design 0021 §4.1 Decision 1 rationale.
    pub confidence_profit_bonus: Threshold<String>,

    /// Slippage magnitude bonus (added when victim_slippage_pct >= min_victim_slippage_pct).
    ///
    /// Default 0.15. Slippage above threshold is anomalous but not an independent signal.
    /// Source: design 0021 §4.1 Decision 4 rationale.
    pub confidence_slippage_bonus: Threshold<String>,

    /// Maximum confidence cap for D13.
    ///
    /// Default 0.85. Sandwich = indirect harm (slippage, not direct asset drain like D12).
    /// Source: design 0021 Decision 6 rationale. Compare to D12 cap 0.95.
    pub confidence_cap: Threshold<String>,

    /// Pool kinds to evaluate. `["univ2", "univ3"]` at MVP (Decision 2).
    ///
    /// Sprint 16 decoders cover UniV2 + UniV3. Curve/Balancer/SushiSwap are Sprint 21+.
    pub pool_kinds_enabled: Threshold<Vec<String>>,

    /// Operator-extension allowlist of settlement contract addresses (lowercase hex).
    ///
    /// The hardcoded baseline (CoW Protocol + Flashbots Protect + 1inch Fusion) is
    /// always active. This field adds operator-specific extra entries without requiring
    /// a code change. Default: empty.
    /// Source: design 0021 Decision 7 (hardcoded + operator extension point).
    pub settlement_allowlist_extra: Threshold<Vec<String>>,

    /// Whether to suppress D13 events for established-protocol tokens.
    ///
    /// Default false. Consistent with D08/D11/D12 non-suppression policy (gotcha #17).
    /// Setting true would suppress events on WETH/USDC pools — the primary sandwich targets.
    pub suppress_established_protocols: Threshold<bool>,
}

/// Thresholds for the Smart-Money Labelling pipeline (Sprint 22, Stage 1 + Stage 3).
///
/// Source: docs/designs/0022-smart-money-labelling-mvp.md §9
/// Primary references:
/// - Barras, Scaillet & Wermers 2010 (JoF 65(1)) — FDR skill/luck; `min_round_trips = 10`.
/// - Fantazzini & Xiao 2023 (Econometrics 11(3)) — 60-min pre-event window.
/// - Fu, Feng, Wu & Xu 2025 (Perseus, arXiv:2503.01686) — recurrence ≥ 3 threshold.
///
/// # Stage 2 FDR
///
/// `smart_money_fdr_enabled = false` by default — data-blocked until 30-day corpus.
/// TODO(sprint-23+): activate Barras 2010 FDR when corpus matures.
///
/// # Calibration annotation
///
/// All emitted labels carry `"calibration": "heuristic, not FDR-controlled"` until
/// Stage 2 activates. This is a hard requirement per design 0022 §4.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SmartMoneyConfig {
    /// Whether the smart-money labeller is enabled.
    /// When `false`, the batch task exits immediately without processing wallets.
    pub enabled: Threshold<bool>,

    /// Batch interval in seconds. Default: 21600 (6 hours).
    pub batch_interval_seconds: Threshold<u64>,

    /// Minimum completed round-trips for a wallet to enter the corpus.
    ///
    /// Barras et al. 2010 JoF 65(1): below 10, alpha t-statistic has insufficient power.
    /// CALIBRATION: "heuristic, not FDR-controlled" until smart_money_fdr_enabled = true.
    pub min_round_trips: Threshold<u32>,

    /// Configurable floor: operator-accessible lower bound for min_round_trips.
    /// Must be >= 5. Accepting higher heuristic noise is the operator's explicit choice.
    /// Default: 5.
    pub min_round_trips_floor: Threshold<u32>,

    // ---- Tier 1 criteria ----
    /// Minimum total realized PnL in USD for Tier 1.
    /// Nansen secondary market-color; no academic anchor. String Decimal (never float).
    pub tier1_min_pnl_usd: Threshold<String>,

    /// Minimum win rate (fraction of priced round-trips with positive PnL) for Tier 1.
    /// Default: 0.55 (unverified-heuristic; Stage 2 FDR replaces). String Decimal.
    pub tier1_min_win_rate: Threshold<String>,

    /// Minimum distinct pump events where this wallet appeared in the pre-event window.
    /// Perseus 2025 (arXiv:2503.01686): all 438 confirmed masterminds recurred >= 3 times.
    pub tier1_min_recurrence: Threshold<u32>,

    /// Timing lead percentile threshold for Tier 1 (top-10% earliest entries).
    /// Fantazzini & Xiao 2023 operationalized: 90th percentile vs co-participants.
    pub tier1_top_timing_percentile: Threshold<String>,

    // ---- Tier 2 criteria ----
    /// Minimum total realized PnL in USD for Tier 2 (PnL-only path). String Decimal.
    pub tier2_min_pnl_usd: Threshold<String>,

    /// Minimum distinct pump events for Tier 2 recurrence path. Default: 2.
    pub tier2_min_recurrence: Threshold<u32>,

    // ---- Stage 3 timing parameters ----
    /// Pre-event entry lookback in blocks. Default: 100.
    pub pre_event_lookback_blocks: Threshold<u32>,

    /// Maximum pre-event lookback in minutes.
    /// Fantazzini & Xiao 2023: 60-minute pre-announcement window. Default: 60.
    pub pre_event_lookback_max_minutes: Threshold<u32>,

    // ---- Stage 2 FDR (NOT ACTIVATED in Sprint 22) ----
    /// Stage 2 FDR activation flag. Ships as `false` — data-blocked.
    /// Citation: Barras, Scaillet & Wermers 2010 JoF DOI 10.1111/j.1540-6261.2009.01527.x
    pub smart_money_fdr_enabled: Threshold<bool>,

    // ---- Infrastructure ----
    /// Batch lookback window in minutes for stale-wallet detection. Default: 720 (12h).
    pub batch_lookback_minutes: Threshold<u32>,

    /// Label TTL in hours. Labels expire and must be re-earned. Default: 720 (30 days).
    pub label_ttl_hours: Threshold<i64>,

    /// Corpus lookback window in days for swap history. Default: 90.
    pub corpus_lookback_days: Threshold<i64>,

    /// Minimum D04 confidence for a pump event to be included in Stage 3 event index.
    /// Default: 0.60.
    pub pump_event_min_confidence: Threshold<f64>,

    /// Confidence for active `wash_trading_v1` events that trigger wallet exclusion.
    /// Design 0022 §8 E-SM-2: evasion guard against fake PnL via wash trading.
    /// Default: 0.70.
    pub wash_trading_exclusion_confidence: Threshold<f64>,

    /// Minimum label confidence for an address to be included in the smart-money
    /// lookup map returned to D04/D08/D05 (design 0023 §9).
    /// Labels below this floor are excluded from amplification.
    /// Default: 0.40 — unverified-heuristic; above Tier3 noise floor.
    pub min_label_confidence: Threshold<f64>,
}

/// Thresholds for D14 Bridge Drain detector.
///
/// Source: rekt.news leaderboard (https://rekt.news/leaderboard) — 6 major bridge exploits
/// from 2021–2023 totalling >$2B in losses.
///
/// # Suppression policy
///
/// D14 does NOT suppress on established protocols. Bridges are infrastructure, not
/// user-driven tokens. A drain on a known bridge is always High/Critical regardless
/// of token status. Consistent with D12/D11/D08 non-suppression policy.
///
/// # Confidence formula
///
/// ```text
/// base      = Tier1 → 0.85  |  Tier2 → 0.65
/// amplifier = drain_pct >= 0.50 → +0.10  |  otherwise +0.00
/// conf      = min(base + amplifier, 0.95)
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BridgeDrainConfig {
    /// Minimum outflow fraction to fire the detector.
    ///
    /// Default: 0.20 (20%). Any drain above 20% of the custody balance within
    /// a single evaluation window is anomalous for bridge infrastructure.
    /// Normal bridge operations involve proportional inflows + outflows; a 20%+
    /// one-directional drain in a single block window has no legitimate explanation.
    /// Source: rekt.news leaderboard; all major bridge incidents exceeded 50%+ drain.
    /// The 20% threshold is conservative: provides early-warning signal even for
    /// partial drains (e.g. partial liquidity extraction before bridge is paused).
    pub min_drain_pct: Threshold<f64>,

    /// Base confidence for Tier1 bridges (>$100M TVL or confirmed major exploit).
    ///
    /// Default: 0.85. A Tier1 bridge drain at 20%+ is near-certain high-severity.
    /// Tier1 bridges have multiple security layers; a 20% drain indicates either
    /// a major exploit or intentional insider action.
    /// Source: Ronin ($625M), Wormhole ($320M), Nomad ($190M) — all near-certain exploits.
    pub tier1_base_confidence: Threshold<f64>,

    /// Base confidence for Tier2 bridges (smaller, less well-documented).
    ///
    /// Default: 0.65. A Tier2 bridge drain at 20%+ may have operational explanations
    /// (bridge protocol upgrade, liquidity rebalancing by operators). Tier2 confidence
    /// reflects meaningful but not near-certain risk.
    /// Source: Multichain shutdown — admin key misuse, not a traditional exploit.
    pub tier2_base_confidence: Threshold<f64>,

    /// Additional confidence when drain >= 50% (large drain amplifier).
    ///
    /// Default: 0.10. A 50%+ drain leaves the bridge half-empty in one window —
    /// operationally impossible through normal bridge usage patterns.
    /// Bridges accumulate and disperse liquidity in small fractions; 50%+ is
    /// exclusively consistent with exploit or insider theft.
    /// Source: All 6 rekt.news incidents exceeded 50% drain within the exploit window.
    pub large_drain_amplifier: Threshold<f64>,

    /// Maximum confidence cap.
    ///
    /// Default: 0.95. Preserves 5% epistemic uncertainty — the registry may be
    /// stale, addresses may have been legitimately migrated, or a token may have
    /// unusual mechanics. Never assert 100% confidence from static address lookup.
    /// Consistent with D12 cap (0.95). Source: CLAUDE.md §Detector Rules.
    pub confidence_cap: Threshold<f64>,

    /// Maximum number of transfer rows fetched per bridge address per window.
    ///
    /// Bounds DB query cost. Default: 10,000. At the row cap, drain_pct may be
    /// underestimated (conservative: reduces false positives, not false negatives).
    pub max_rows_per_address: Threshold<u32>,

    /// Path to the bridge registry TOML file.
    ///
    /// Default: "config/known_bridges.toml". Loaded once at startup.
    /// The loader falls back to an empty registry if the file is absent.
    pub bridge_registry_path: Threshold<String>,
}

// ---------------------------------------------------------------------------
// VerdictCacheTtlConfig — Sprint 26 (ADR 0007 §9.5)
// ---------------------------------------------------------------------------

/// Per-detector TTL minutes for `verdict_cache`.
///
/// Deserialized from `config/detectors.toml` `[verdict_cache.ttl_minutes]`.
/// Keys are config-side detector ids (e.g. `d01_honeypot_v1`); values are
/// TTL in minutes. The indexer maps these to runtime `Detector::id()` values
/// at lookup time via `VerdictCacheConfig::from_toml_map`.
///
/// Three TTL classes per ADR 0007 §9.5:
/// - Fast-moving signals (5 min): D04 pump_dump, D05 wash_trading, D11, D13.
/// - Honeypot (15 min): D01.
/// - Slow-moving signals (60 min): everything else.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VerdictCacheTtlConfig {
    /// TTL in minutes, keyed by config-side detector id.
    ///
    /// Example: `{ "d01_honeypot_v1" = 15, "d04_pump_dump_v1" = 5, ... }`
    #[serde(default)]
    pub ttl_minutes: std::collections::HashMap<String, u64>,
}

/// All detector threshold configs, loaded from one TOML file.
///
/// Subsection names MUST match detector ID constants (the `id()` method on each
/// `Detector` implementor). Adding a new detector requires adding its config
/// struct here and a matching TOML subsection in `config/detectors.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AllDetectorConfigs {
    pub honeypot_sim: HoneypotConfig,
    pub rug_pull_lp_drain: RugPullConfig,
    pub holder_concentration: ConcentrationConfig,
    pub pump_dump: PumpDumpConfig,
    pub wash_trading_h1: WashTradingConfig,
    pub mint_burn_anomaly: MintBurnConfig,
    pub withdraw_withheld: WithdrawWithheldConfig,
    pub sybil_detection: SybilConfig,
    pub deployer_changepoint: DeployerChangepointConfig,
    pub launch_audit: LaunchAuditConfig,
    pub synchronized_activity_v1: SynchronizedActivityConfig,
    pub permit2_drainer_v1: PermitDrainerConfig,
    pub sandwich_mev_v1: SandwichMevConfig,
    /// Smart-money labelling pipeline config (Sprint 22, Stage 1 + Stage 3).
    pub smart_money_v1: SmartMoneyConfig,
    /// D14 Bridge Drain detector config (Sprint 26).
    pub bridge_drain_v1: BridgeDrainConfig,
    /// Verdict cache TTL config (Sprint 26 T26-4, ADR 0007 §9.5).
    ///
    /// Per-detector TTL minutes for `verdict_cache`. Read by the indexer's
    /// `VerdictCacheConfig::from_toml_map` to compute `expires_at` on upsert.
    #[serde(default)]
    pub verdict_cache: VerdictCacheTtlConfig,
}

/// Thin alias used in [`DetectorContext`][crate::context::DetectorContext].
///
/// Phase 2 passes the whole `AllDetectorConfigs` to context; each detector
/// accesses only its own subsection by field name.
pub type DetectorConfig = AllDetectorConfigs;

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load and validate `config/detectors.toml`.
///
/// # Errors
///
/// Returns `anyhow::Error` if:
/// - The file does not exist or cannot be read.
/// - The TOML fails to parse.
/// - Any required subsection or threshold key is missing.
///   (Serde's `Deserialize` derive handles missing-key errors automatically —
///   missing keys are reported with the TOML path of the missing field.)
///
/// # Usage
///
/// ```rust,no_run
/// use mg_onchain_detectors::config::load_detector_config;
///
/// let config = load_detector_config("config/detectors.toml").unwrap();
/// println!("sell_tax_threshold = {}", config.honeypot_sim.sell_tax_threshold.value);
/// ```
pub fn load_detector_config(path: impl AsRef<Path>) -> anyhow::Result<AllDetectorConfigs> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read detector config from {}", path.display()))?;
    let config: AllDetectorConfigs = toml::from_str(&content)
        .with_context(|| format!("failed to parse detector config from {}", path.display()))?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Returns the workspace root directory.
    ///
    /// `cargo test` sets CWD to the crate directory. The config files live at
    /// the workspace root two levels up from `crates/detectors/`.
    fn workspace_root() -> PathBuf {
        // CARGO_MANIFEST_DIR is the crate's Cargo.toml directory.
        // Workspace root is two levels up: crates/detectors -> crates -> workspace.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        PathBuf::from(manifest_dir)
            .parent() // crates/
            .expect("crates dir must exist")
            .parent() // workspace root
            .expect("workspace root must exist")
            .to_path_buf()
    }

    /// The example config file ships with honeypot section fully populated and
    /// all other sections as stubs. Parsing it must succeed and return correct values.
    #[test]
    fn load_example_config_succeeds() {
        let path = workspace_root().join("config/detectors.toml.example");
        let config = load_detector_config(&path)
            .expect("config/detectors.toml.example must parse without error");

        // Verify honeypot thresholds are correct.
        // NOTE: sell_tax_threshold = 0.30 (lowered from 0.50 — review §6, B1 fix).
        assert_eq!(
            config.honeypot_sim.sell_tax_threshold.value, 0.30,
            "honeypot sell_tax_threshold must be 0.30 (lowered from 0.50 per review §6 B1)"
        );
        assert_eq!(
            config.honeypot_sim.simulate_paths.value, 3,
            "honeypot simulate_paths must be 3 per the example config"
        );
        // NOTE: buy_sell_ratio_sentinel = 5.0 (lowered from 10.0 — review §6.3 control #1).
        assert_eq!(
            config.honeypot_sim.buy_sell_ratio_sentinel.value, 5.0,
            "honeypot buy_sell_ratio_sentinel must be 5.0 (lowered from 10.0 per review §6.3)"
        );

        // Verify refs are non-empty for honeypot thresholds.
        assert!(
            !config.honeypot_sim.sell_tax_threshold.refs.is_empty(),
            "sell_tax_threshold must have at least one ref"
        );
        assert_eq!(
            config.honeypot_sim.sell_tax_threshold.refs[0], "D01/honeypot_sim",
            "first ref must be D01/honeypot_sim"
        );

        // Verify rationale is non-empty.
        assert!(
            !config.honeypot_sim.sell_tax_threshold.rationale.is_empty(),
            "sell_tax_threshold rationale must not be empty"
        );
    }

    #[test]
    fn load_detectors_toml_production_config() {
        // config/detectors.toml is the production config — must be present and
        // parse correctly with honeypot section populated.
        let path = workspace_root().join("config/detectors.toml");
        let config =
            load_detector_config(&path).expect("config/detectors.toml must parse without error");

        // sell_tax_threshold = 0.30 (lowered from 0.50 — review §6 B1 fix).
        assert_eq!(config.honeypot_sim.sell_tax_threshold.value, 0.30);
    }

    #[test]
    fn missing_file_returns_descriptive_error() {
        let err = load_detector_config("/nonexistent/path/detectors.toml")
            .expect_err("must fail on missing file");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to read detector config"),
            "error message should describe what failed: {msg}"
        );
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "[honeypot_sim\nbad toml").unwrap();
        let err = load_detector_config(tmp.path()).expect_err("must fail on bad TOML");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse detector config"),
            "error message should describe parse failure: {msg}"
        );
    }

    #[test]
    fn rug_pull_thresholds_loaded_correctly() {
        let path = workspace_root().join("config/detectors.toml.example");
        let config = load_detector_config(&path).unwrap();
        assert_eq!(config.rug_pull_lp_drain.lp_removal_threshold.value, 0.65);
        // DG-D02-4: raised from 1000→1500 to cut straddling-band false positives.
        assert_eq!(config.rug_pull_lp_drain.min_pool_usd.value, 1500.0);
        assert_eq!(config.rug_pull_lp_drain.min_prior_txs.value, 100);
        // DG-D02-1: unified safe floor replaces old split fields
        assert_eq!(config.rug_pull_lp_drain.lp_safe_floor_pct.value, 70.0);
        // DG-D02-1: lp_providers_threshold changed from 2→1 per spec §5
        assert_eq!(config.rug_pull_lp_drain.lp_providers_threshold.value, 1);
        assert_eq!(config.rug_pull_lp_drain.single_provider_bonus.value, 0.15);
        // E-D02-15: raised from 30→45 to catch same-block lock-expiry drain.
        assert_eq!(config.rug_pull_lp_drain.minimum_lock_horizon_days.value, 45);
        assert_eq!(config.rug_pull_lp_drain.drain_window_minutes.value, 60);
        // Threshold fix 3: 24h companion window for trickle drains.
        assert_eq!(
            config.rug_pull_lp_drain.drain_window_24h_minutes.value,
            1440
        );
        // Blocker Fix 2: expiry-proximity bonus cap.
        assert_eq!(
            config.rug_pull_lp_drain.expiry_proximity_bonus_max.value,
            0.20
        );
    }

    #[test]
    fn all_detector_configs_have_refs() {
        let path = workspace_root().join("config/detectors.toml.example");
        let config = load_detector_config(&path).unwrap();
        // Every threshold that ships must have at least one REFERENCES.md citation.
        assert!(!config.honeypot_sim.sell_tax_threshold.refs.is_empty());
        assert!(
            !config
                .rug_pull_lp_drain
                .lp_removal_threshold
                .refs
                .is_empty()
        );
        assert!(!config.holder_concentration.gini_delta_24h.refs.is_empty());
        assert!(!config.pump_dump.price_spike_pct.refs.is_empty());
        assert!(!config.wash_trading_h1.block_window_slots.refs.is_empty());
        assert!(
            !config
                .mint_burn_anomaly
                .supply_change_threshold_pct
                .refs
                .is_empty()
        );
    }
}
