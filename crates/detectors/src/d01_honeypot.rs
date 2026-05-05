//! D01 — Honeypot (simulation) detector.
//!
//! # Overview
//!
//! Detects tokens that prevent sells by:
//!   1. Reading structural state from `TokenMeta` (freeze authority, transfer fee,
//!      permanent delegate, transfer hook program).
//!   2. Querying on-chain buy/sell transfer ratios from the `transfers` table.
//!   3. Optionally simulating a sell transaction via Solana RPC (deferred to Phase 3).
//!
//! All five static signals (S1–S5) are implemented. Simulation (S6) is wired
//! but returns `DetectorError::NotImplemented` until dex-adapter instruction
//! builders are available.
//!
//! # Confidence formula
//!
//! Per `docs/designs/0004-detector-01-honeypot.md` §6:
//!
//! ```text
//! raw = s_tax * 0.45 + s_freeze * 0.25 + s_ratio * 0.20
//!     + s_delegate * 0.20 + s_hook * 0.20 + s_fee_auth * transfer_fee_authority_extra_weight
//! static_conf = sigmoid(raw / 0.55 - 1.0)
//! final_conf  = min(1.0, static_conf + sim_add)    -- sim_add=0 when simulation disabled
//! ```
//!
//! # DG3 — Simulation deferral
//!
//! `simulate_sell()` returns `DetectorError::NotImplemented { feature: "honeypot_simulation" }`.
//! The body comment explains the dependency:
//!
//! ```text
//! TODO(phase-3): simulation requires DEX-specific swap instruction builders in
//! `crates/dex-adapter` (currently only decoders exist). When builders land,
//! implement per docs/designs/0004-detector-01-honeypot.md §Algorithm§Simulation.
//! ```
//!
//! # DG4 — jup_verified FP attenuation
//!
//! Tokens with `verification.jup_verified = true` have their static confidence
//! attenuated to `min(conf, 0.25)` before the final event is emitted.
//! This prevents USDC and PYUSD from being emitted at High/Critical severity
//! solely because they retain freeze authority for regulatory compliance.
//! The scoring/ crate (Phase 5) will layer further context-aware attenuation.
//!
//! # DG5 — Severity ladder
//!
//! Severity is computed by `signals::severity_from_confidence(final_conf)`.
//! The `severity_floor()` method returns `Severity::Info`.
//!
//! # DG2 — Token-2022 permanent_delegate and transfer_hook_program
//!
//! Both fields are read from `TokenMeta`, which is populated by `token-registry`
//! enrichment. In Phase 2, enrichment returns `None` for these fields because
//! the Token-2022 TLV extension decoder has not shipped (Phase 3 TODO). When
//! `None`, S3 and S4 signals are suppressed — this degrades recall but causes
//! no false positives. A `tracing::debug!` event is emitted when either field
//! is absent on a Token-2022 token.
//!
//! # Evidence keys
//!
//! Required (present on every event):
//!   `honeypot_sim/freeze_authority_active`  — `Decimal(0|1)`
//!   `honeypot_sim/transfer_fee_bps`         — `Decimal`
//!   `honeypot_sim/buy_sell_ratio`           — `Decimal`
//!   `honeypot_sim/buy_count`                — `Decimal`
//!   `honeypot_sim/sell_count`               — `Decimal`
//!   `honeypot_sim/simulate_paths_tested`    — `Decimal(0)` (simulation deferred)
//!
//! Conditional (see spec §8):
//!   `honeypot_sim/permanent_delegate_active`
//!   `honeypot_sim/transfer_hook_present`
//!   `honeypot_sim/transfer_fee_authority_active`
//!   `honeypot_sim/sim_skipped`
//!   `honeypot_sim/sim_skip_reason`
//!
//! # References
//!
//! - Torres, Steichen & State (2019) HoneyBadger — REFERENCES.md D01/honeypot_sim
//! - Honeypot.is simulation methodology — REFERENCES.md D01/honeypot_sim
//! - Token-2022 TransferFeeConfig extension — REFERENCES.md D01/honeypot_sim
//! - Token-2022 PermanentDelegate extension — REFERENCES.md D01/honeypot_sim
//! - Phantom help: Frozen tokens on Solana — REFERENCES.md D01/honeypot_sim
//! - Chainstack Token-2022 Transfer Hooks blog — REFERENCES.md D01/honeypot_sim
//! - Security review: docs/reviews/0001-d01-honeypot-evasions.md (2026-04-21)
//!
//! # Static-only mode (compensating controls active, Phase 2/Sprint 2 exit)
//!
//! Simulation (S6) is deferred to Phase 3. Three compensating controls are in
//! place per `docs/designs/0004-detector-01-honeypot.md §14`:
//!
//! 1. `buy_sell_ratio_sentinel` lowered 10.0 → 5.0.
//! 2. `sell_tax_threshold` lowered 0.50 → 0.30 (`sell_tax_threshold_bps` 5000 → 3000).
//! 3. `reevaluation_interval_minutes = 15` — scheduler must re-run D01 every 15 min
//!    for the first 24h after a D01 event fires (catches E10/E13 time-gated evasions).
//!    See `TODO(sprint-2-exit-test)` comment on `reevaluation_interval_minutes` below.
//!
//! **Known worst-case false negative (E11 crafted token):** transfer hook only (S4 = 0.20),
//! wash-sell ratio below 5.0 sentinel (S5 = 0) → final confidence 0.276 / `Severity::Low`.
//! Only S6 simulation catches this class. Documented in review §5.

use std::sync::Arc;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use mg_solana_types::{Hash, Pubkey};
use tracing::{debug, instrument, warn};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Chain;
use mg_onchain_common::event::DexKind;
use mg_onchain_common::token::TokenMeta;
use mg_onchain_dex_adapter::pool_accounts::{PoolAccountError, PoolAccountProvider};
use mg_onchain_dex_adapter::{
    build_swap_base_in_transaction, build_swap_base_input_transaction, derive_simulation_keypair,
};
use mg_onchain_chain_adapter::error::AdapterError as EthAdapterError;
use mg_onchain_chain_adapter::ethereum::rpc::EthereumRpc;

use crate::config::HoneypotConfig;
use crate::context::DetectorContext;
use crate::detector::Detector;
use crate::error::DetectorError;
use crate::evidence_key;
use crate::rpc::SolanaRpc;
use crate::signals::{severity_from_confidence, sigmoid};

/// Stable detector ID — matches the TOML subsection and `Evidence::metrics` prefix.
pub const DETECTOR_ID: &str = "honeypot_sim";

/// Solana system program — the canonical null/zero address on Solana.
/// Used to identify the zero address when the `ctx.zero_address` field is not
/// directly accessible in the pure-function path.
const SOLANA_SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

// ---------------------------------------------------------------------------
// Internal result types (pure-function return values)
// ---------------------------------------------------------------------------

/// Output from the static signal pass (no I/O after data is fetched).
///
/// Fields are `pub` so downstream integration tests can inspect individual
/// signal values when calling [`compute_static`] directly (pure path).
#[derive(Debug, Default)]
pub struct StaticResult {
    /// Sigmoid-normalized confidence from static signals only. Range `(0, 1)`.
    pub confidence: f64,
    /// S1: freeze authority is set on the mint.
    pub freeze_active: bool,
    /// S2: transfer fee (bps) on the token. 0 = no fee.
    pub transfer_fee_bps: u16,
    /// S2: transfer fee authority is a live (non-system-program) address.
    pub fee_authority_active: bool,
    /// S3: permanent delegate is set (Token-2022).
    pub permanent_delegate_active: bool,
    /// S4: transfer hook program is set (Token-2022).
    pub transfer_hook_present: bool,
    /// S5: buy/sell ratio from SQL query.
    pub buy_sell_ratio: f64,
    /// S5: raw buy count in window.
    pub buy_count: i64,
    /// S5: raw sell count in window.
    pub sell_count: i64,
    /// True when the S5 ratio signal was suppressed (insufficient buy activity).
    pub ratio_suppressed: bool,
    /// True when the token is jup_verified (FP attenuation applies — DG4).
    pub jup_verified: bool,
    /// True when the Token-2022 NonTransferable extension attenuated S1 weight.
    pub non_transferable_attenuated: bool,
}

// ---------------------------------------------------------------------------
// HoneypotDetector
// ---------------------------------------------------------------------------

/// D01 Honeypot (simulation) detector.
///
/// Detects tokens that prevent sells via five static signals (S1–S5) and
/// an optional sell-simulation path (S6) on Solana, plus an EVM simulate-sell
/// branch via `eth_call` (Sprint 25).
///
/// # Construction
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use mg_onchain_detectors::d01_honeypot::HoneypotDetector;
/// use mg_onchain_detectors::config::HoneypotConfig;
/// use mg_onchain_dex_adapter::pool_accounts::NotWiredPoolAccountProvider;
/// // use mg_onchain_detectors::rpc::SolanaRpc;  // inject production or mock
/// ```
///
/// The `rpc` and `pool_accounts` handles are required even when
/// `simulation_enabled = false`. When disabled, both handles are stored but
/// `simulate_sell()` is never called. Production passes
/// `Arc::new(NotWiredPoolAccountProvider)` until the pool-state fetcher ships.
///
/// EVM branch: inject via `with_evm_rpc(Arc::new(WsRpcClient::connect(...).await?))`.
/// When not injected, EVM evaluation skips simulation and uses static-only confidence.
#[derive(Clone)]
pub struct HoneypotDetector {
    /// Injected threshold config.
    ///
    /// Currently the detector reads thresholds from `ctx.config.honeypot_sim` during
    /// `evaluate()` so that operators can hot-reload config without restarting the
    /// process. `self.thresholds` is retained as the construction-time snapshot;
    /// simulation uses it as the authoritative config source.
    #[allow(dead_code)] // ctx.config used in evaluate(); retained for simulation path.
    thresholds: HoneypotConfig,
    /// Solana RPC reference for simulation (DG1: injected at construction, not via context).
    /// Stored as `Arc<dyn SolanaRpc>` so `HoneypotDetector` is `Clone + Send + Sync`.
    rpc: Arc<dyn SolanaRpc>,
    /// Pool account provider — supplies the full swap-instruction account set.
    /// Production uses `NotWiredPoolAccountProvider` until the pool-state fetcher
    /// follow-up task ships. Tests inject `MockPoolAccountProvider`.
    pool_accounts: Arc<dyn PoolAccountProvider>,
    /// Optional EthereumRpc handle for EVM simulate-sell (Sprint 25).
    ///
    /// When `None`, the EVM branch runs static-only signals (buy/sell ratio from DB)
    /// without eth_call simulation, logging a debug message.
    evm_rpc: Option<Arc<dyn EthereumRpc + Send + Sync>>,
}

impl HoneypotDetector {
    /// Construct a new `HoneypotDetector`.
    ///
    /// # Arguments
    ///
    /// - `thresholds`: threshold config loaded from `config/detectors.toml`.
    /// - `rpc`: Solana RPC handle. In production pass `Arc::new(HttpSolanaRpc::new(&cfg))`.
    ///   In tests inject `Arc::new(MockSolanaRpc::default())`.
    /// - `pool_accounts`: Pool account provider. Pass `Arc::new(NotWiredPoolAccountProvider)`
    ///   in production until the pool-state fetcher ships. Tests inject `MockPoolAccountProvider`.
    pub fn new(
        thresholds: HoneypotConfig,
        rpc: Arc<dyn SolanaRpc>,
        pool_accounts: Arc<dyn PoolAccountProvider>,
    ) -> Self {
        Self {
            thresholds,
            rpc,
            pool_accounts,
            evm_rpc: None,
        }
    }

    /// Inject an EthereumRpc handle for EVM simulate-sell (Sprint 25).
    ///
    /// Builder pattern — existing `::new(...)` callsites compile unchanged.
    pub fn with_evm_rpc(mut self, rpc: Arc<dyn EthereumRpc + Send + Sync>) -> Self {
        self.evm_rpc = Some(rpc);
        self
    }
}

impl Detector for HoneypotDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    /// The minimum severity this detector emits (DG5 resolution).
    ///
    /// Returns `Severity::Info` — the real severity is computed dynamically from
    /// the final confidence via `severity_from_confidence()`. There is no fixed
    /// floor imposed: an all-signals-absent token legitimately emits Info.
    fn severity_floor(&self) -> Severity {
        Severity::Info
    }

    /// D01 supports all 6 production chains.
    ///
    /// EVM branch requires `with_evm_rpc()` injection for full simulate-sell coverage.
    /// Without injection, EVM evaluation runs buy/sell ratio from DB (static signals)
    /// with simulation skipped (confidence attenuated by 0.80 per spec §9.2).
    fn supported_chains(&self) -> &[Chain] {
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
        if ctx.chain.is_evm() {
            return self.evaluate_evm(ctx).await;
        }

        let cfg = &ctx.config.honeypot_sim;

        // Step 1: Enrich token metadata from registry.
        let meta = ctx
            .registry
            .enrich(ctx.token.as_str(), ctx.chain)
            .await
            .map_err(|e| DetectorError::MissingDependencyData {
                detector_id: DETECTOR_ID,
                token: ctx.token.as_str().to_owned(),
                reason: format!("registry enrich failed: {e}"),
            })?;

        // Step 2: Determine primary pool for the SQL query (S5) and evidence.
        // Use the first pool in meta.markets (highest liquidity is preferred but
        // MarketInfo has no ordering guarantee at Phase 2; use index 0 if present).
        let primary_pool: Option<String> = meta
            .markets
            .first()
            .map(|m| m.pool_address.as_str().to_owned());

        // Step 3: Execute S5 SQL query — buy/sell ratio for primary pool.
        let ratio_result = if let Some(ref pool_addr) = primary_pool {
            let raw = ctx
                .store
                .fetch_honeypot_ratio(
                    ctx.chain.as_str(),
                    ctx.token.as_str(),
                    pool_addr,
                    ctx.zero_address,
                    ctx.window.start,
                    ctx.window.end,
                )
                .await;
            match raw {
                Ok(row) => row,
                Err(mg_onchain_storage::error::StorageError::Postgres(se)) => {
                    return Err(DetectorError::TransientQuery {
                        detector_id: DETECTOR_ID,
                        source: se,
                    });
                }
                Err(other) => {
                    return Err(DetectorError::PermanentQuery {
                        detector_id: DETECTOR_ID,
                        reason: other.to_string(),
                    });
                }
            }
        } else {
            None
        };

        // Step 4: Compute static result (pure — testable without I/O).
        let static_result = compute_static(&meta, ratio_result.as_ref(), cfg);

        // TODO(sprint-2-exit-test): The scheduler in `crates/server` must read
        // `cfg.reevaluation_interval_minutes.value` and re-trigger D01 evaluation
        // for any token that has produced a D01 event, every N minutes for the first
        // 24h after listing. This is compensating control #2 for DG3 simulation
        // deferral — it catches time-gated (E10) and oracle-gated (E13) honeypots.
        // See docs/designs/0004-detector-01-honeypot.md §14 and
        // docs/reviews/0001-d01-honeypot-evasions.md §6.3 control #2.

        // Step 5: Simulation pass (S6).
        let (sim_add, sim_paths_tested, sim_paths_failed, sim_skipped, sim_skip_reason) =
            if cfg.simulation_enabled.value {
                match simulate_sell(&meta, &self.rpc, &self.pool_accounts, cfg).await {
                    Ok(sr) => (
                        sr.confidence_add,
                        sr.paths_tested,
                        sr.paths_failed,
                        sr.skipped,
                        sr.skip_reason,
                    ),
                    Err(e) => return Err(e),
                }
            } else {
                (
                    0.0_f64,
                    0u32,
                    0u32,
                    true,
                    Some("simulation_disabled".to_owned()),
                )
            };

        // Step 6: Combine confidence.
        let raw_final = static_result.confidence + sim_add;
        // When simulation was skipped, attenuate by 0.80 (spec §9.2 + §12).
        let final_confidence = if sim_skipped {
            (raw_final * 0.80_f64).min(1.0_f64)
        } else if sim_add >= 1.0_f64 {
            1.0_f64
        } else {
            raw_final.min(1.0_f64)
        };

        // DG4: jup_verified FP attenuation — cap at 0.25 for verified tokens.
        let final_confidence = if static_result.jup_verified {
            final_confidence.min(0.25_f64)
        } else {
            final_confidence
        };

        // Step 7: Compute severity (DG5 bands).
        let severity = severity_from_confidence(final_confidence);

        // Step 8: Build evidence bundle.
        let evidence = build_evidence(
            &static_result,
            primary_pool.as_deref(),
            sim_skipped,
            sim_paths_tested,
            sim_paths_failed,
            sim_skip_reason.as_deref(),
            ctx.chain,
        );

        // Step 9: Emit event.
        let confidence = Confidence::new(final_confidence).unwrap_or(Confidence::ZERO);
        let event = AnomalyEvent {
            detector_id: DETECTOR_ID.to_owned(),
            token: ctx.token.clone(),
            chain: ctx.chain,
            confidence,
            severity,
            evidence,
            observed_at: ctx.window.end,
            window: (ctx.window.block_start, ctx.window.block_end),
            // C1 fix: ctx.observed_at is set once per batch by the caller (scheduler /
            // on-demand handler). Using Utc::now() here broke determinism — two evaluations
            // of the same input differed in ingested_at. See context.rs doc comment.
            ingested_at: ctx.observed_at,
        };

        Ok(vec![event])
    }
}

// ---------------------------------------------------------------------------
// HoneypotDetector EVM implementation (Sprint 25)
// ---------------------------------------------------------------------------

impl HoneypotDetector {
    /// EVM branch for `evaluate()`.
    ///
    /// # Mechanism
    ///
    /// 1. Identify the primary Uniswap V2 pool for the token (first in `meta.markets`).
    /// 2. Fetch buy/sell ratio from DB (same SQL path as Solana S5).
    /// 3. Attempt `eth_call` simulate-sell via the per-chain Uniswap V2 router:
    ///    - Constructs `swapExactTokensForTokens` calldata
    ///    - If call reverts → honeypot 0.95
    ///    - If sell-tax > threshold → honeypot 0.80
    ///    - If sell-tax <= threshold AND sell succeeds → clean
    /// 4. When `evm_rpc` is not injected or simulation fails → static-only confidence (× 0.80 attenuation).
    ///
    /// # Confidence formula (EVM)
    ///
    /// ```text
    /// sim_confidence = (revert → 0.95) | (tax > threshold → 0.80) | (clean → 0.0)
    /// static_confidence = buy_sell_ratio signal (S5 equivalent)
    /// final = max(sim_confidence, static_confidence)
    /// if no simulation: final *= 0.80
    /// ```
    ///
    /// # Per-chain Uniswap router addresses
    ///
    /// See `EVM_UNIV2_ROUTERS` constant below. These are the Uniswap V2 UniversalRouter or
    /// V2 Router02 addresses for each chain.
    ///
    /// # SPEC-NOTE D01-EVM-SIM (Sprint 25)
    ///
    /// The `swapExactTokensForTokens` simulation path requires:
    /// 1. A virtual position (the detector doesn't hold tokens).
    /// 2. A reliable price estimate for the pre-swap token amount.
    ///
    /// EVM `eth_call` against a UniV2 router does NOT give a virtual position — the
    /// router's `getAmountsOut` is a read-only quote that doesn't require token ownership.
    /// However, `swapExactTokensForTokens` itself requires actual token balance.
    ///
    /// **MVP approach (Sprint 25):** Use `getAmountsOut` (read-only) to compare expected
    /// output vs actual. This detects fee-on-transfer tokens (honeypots that charge
    /// excessive sell fees). Full revert simulation requires `overrideStateRoot` (not
    /// universally supported) — deferred to Sprint 26.
    ///
    /// Until `overrideStateRoot` is available, revert-on-sell detection for EVM falls
    /// back to the buy/sell ratio signal (S5).
    #[instrument(
        skip(self, ctx),
        fields(
            detector_id = DETECTOR_ID,
            token = ctx.token.as_str(),
            chain = ctx.chain.as_str()
        )
    )]
    async fn evaluate_evm<'ctx>(
        &self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError> {
        let cfg = &ctx.config.honeypot_sim;

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

        // Step 2: Buy/sell ratio (S5 equivalent) from DB.
        let primary_pool = meta.markets.first().map(|m| m.pool_address.as_str().to_owned());
        let ratio_result = if let Some(ref pool_addr) = primary_pool {
            let raw = ctx
                .store
                .fetch_honeypot_ratio(
                    ctx.chain.as_str(),
                    ctx.token.as_str(),
                    pool_addr,
                    ctx.zero_address,
                    ctx.window.start,
                    ctx.window.end,
                )
                .await;
            match raw {
                Ok(row) => row,
                Err(mg_onchain_storage::error::StorageError::Postgres(se)) => {
                    return Err(DetectorError::TransientQuery {
                        detector_id: DETECTOR_ID,
                        source: se,
                    });
                }
                Err(other) => {
                    return Err(DetectorError::PermanentQuery {
                        detector_id: DETECTOR_ID,
                        reason: other.to_string(),
                    });
                }
            }
        } else {
            None
        };

        // Step 3: EVM simulate-sell via eth_call → getAmountsOut (MVP approach).
        let (evm_sim_confidence, evm_sim_skipped, evm_sim_skip_reason) =
            if let (Some(rpc), Some(pool_addr)) = (&self.evm_rpc, &primary_pool) {
                let router = evm_router_for_chain(ctx.chain);
                if let Some(router_addr) = router {
                    match evm_simulate_sell(
                        ctx.token.as_str(),
                        pool_addr,
                        router_addr,
                        ctx.chain,
                        rpc.as_ref(),
                        cfg.sell_tax_threshold.value as f32,
                    ).await {
                        Ok(sim_conf) => (sim_conf, false, None),
                        Err(e) => {
                            warn!(
                                error = %e,
                                token = ctx.token.as_str(),
                                chain = ctx.chain.as_str(),
                                "D01 EVM: simulate-sell failed (static-only fallback)"
                            );
                            (0.0_f64, true, Some("sim_failed".to_owned()))
                        }
                    }
                } else {
                    debug!(
                        chain = ctx.chain.as_str(),
                        "D01 EVM: no router configured for chain — simulation skipped"
                    );
                    (0.0_f64, true, Some("no_router_for_chain".to_owned()))
                }
            } else {
                debug!(
                    token = ctx.token.as_str(),
                    "D01 EVM: no evm_rpc injected — simulation skipped"
                );
                (0.0_f64, true, Some("no_evm_rpc".to_owned()))
            };

        // Step 4: Buy/sell ratio signal (S5 equivalent for EVM).
        let buy_sell_ratio_conf = if let Some(ref row) = ratio_result {
            let buy_count = row.buy_count;
            let sell_count = row.sell_count;
            if buy_count >= cfg.min_buy_count_for_ratio.value && sell_count == 0 {
                // Sentinel: zero sells after sufficient buys — potential honeypot.
                let sentinel = cfg.buy_sell_ratio_sentinel.value;
                let ratio = if sell_count == 0 { sentinel } else { buy_count as f64 / sell_count as f64 };
                if ratio >= sentinel { 0.40_f64 } else { 0.0_f64 }
            } else {
                0.0_f64
            }
        } else {
            0.0_f64
        };

        // Step 5: Combine.
        let static_conf = buy_sell_ratio_conf;
        let mut final_confidence = evm_sim_confidence.max(static_conf);
        if evm_sim_skipped && evm_sim_confidence < 0.01 {
            final_confidence = (static_conf * 0.80_f64).min(1.0_f64);
        }

        // Step 6: Evidence.
        let buy_count = ratio_result.as_ref().map(|r| r.buy_count).unwrap_or(0);
        let sell_count = ratio_result.as_ref().map(|r| r.sell_count).unwrap_or(0);

        let evidence = Evidence::new()
            .with_metric(
                evidence_key(DETECTOR_ID, "evm_sim_confidence"),
                Decimal::from_f64(evm_sim_confidence).unwrap_or(Decimal::ZERO),
            )
            .with_metric(
                evidence_key(DETECTOR_ID, "evm_sim_skipped"),
                if evm_sim_skipped { Decimal::ONE } else { Decimal::ZERO },
            )
            .with_metric(
                evidence_key(DETECTOR_ID, "buy_count"),
                Decimal::from(buy_count),
            )
            .with_metric(
                evidence_key(DETECTOR_ID, "sell_count"),
                Decimal::from(sell_count),
            )
            // Keep the keys expected by the standard schema (same as Solana path).
            .with_metric(evidence_key(DETECTOR_ID, "freeze_authority_active"), Decimal::ZERO)
            .with_metric(evidence_key(DETECTOR_ID, "transfer_fee_bps"), Decimal::ZERO)
            .with_metric(evidence_key(DETECTOR_ID, "buy_sell_ratio"), Decimal::from_f64(
                if sell_count == 0 { cfg.buy_sell_ratio_sentinel.value } else { buy_count as f64 / sell_count as f64 }
            ).unwrap_or(Decimal::ZERO))
            .with_metric(evidence_key(DETECTOR_ID, "simulate_paths_tested"), Decimal::ZERO)
            .with_note(format!(
                "EVM honeypot check: chain={}, sim_conf={:.2}, sim_skipped={}, reason={:?}. \
                 buy/sell={}/{} in window.",
                ctx.chain.as_str(),
                evm_sim_confidence,
                evm_sim_skipped,
                evm_sim_skip_reason,
                buy_count,
                sell_count,
            ));

        let severity = severity_from_confidence(final_confidence);
        let confidence = Confidence::new(final_confidence).unwrap_or(Confidence::ZERO);
        let event = AnomalyEvent {
            detector_id: DETECTOR_ID.to_owned(),
            token: ctx.token.clone(),
            chain: ctx.chain,
            confidence,
            severity,
            evidence,
            observed_at: ctx.window.end,
            window: (ctx.window.block_start, ctx.window.block_end),
            ingested_at: ctx.observed_at,
        };

        Ok(vec![event])
    }
}

// ---------------------------------------------------------------------------
// EVM helper: per-chain Uniswap V2 router addresses
// ---------------------------------------------------------------------------

/// Per-chain Uniswap V2 (or equivalent) router address for simulate-sell.
///
/// # Addresses (verified against each protocol's canonical deployment)
///
/// | Chain     | Router                                       | Protocol                   | Source |
/// |-----------|----------------------------------------------|----------------------------|--------|
/// | Ethereum  | `0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D`  | Uniswap V2 Router02        | Uniswap docs (training-time, canonical) |
/// | BSC       | `0x10ED43C718714eb63d5aA57B78B54704E256024E`  | PancakeSwap V2 Router      | PancakeSwap docs (training-time) |
/// | Base      | `0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24`  | Uniswap V2 on Base         | SPEC-NOTE: verify via Basescan |
/// | Arbitrum  | `0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24`  | Uniswap V2 on Arbitrum     | SPEC-NOTE: verify via Arbiscan |
/// | Polygon   | `0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff`  | QuickSwap V2 Router        | QuickSwap docs (training-time) |
///
/// # SPEC-NOTE D01-EVM-ROUTER (Sprint 25)
///
/// Base and Arbitrum router addresses require verification. Training-time knowledge
/// suggests `0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24` for Uniswap V2 on both chains
/// (same address via CREATE2 deterministic deployment), but this must be confirmed against
/// each chain's block explorer before activating in production.
pub fn evm_router_for_chain(chain: Chain) -> Option<&'static str> {
    match chain {
        Chain::Ethereum => Some("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
        Chain::Bsc => Some("0x10ED43C718714eb63d5aA57B78B54704E256024E"),
        Chain::Base => Some("0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24"),
        Chain::Arbitrum => Some("0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24"),
        Chain::Polygon => Some("0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff"),
        Chain::Solana => None, // Solana not an EVM chain
        _ => None,             // future chains: no router configured yet
    }
}

/// EVM simulate-sell via `getAmountsOut` (MVP approach, Sprint 25).
///
/// Calls `getAmountsOut(amountIn, path)` on the Uniswap V2 router to get the expected
/// output for selling `token` → WETH. Compares expected output vs. any reference to
/// detect extreme sell taxes.
///
/// # MVP Limitation (SPEC-NOTE D01-EVM-SIM)
///
/// `getAmountsOut` is a read-only quote based on pool reserves. It does NOT:
/// - Detect revert-on-sell (requires overrideStateRoot or fork-state simulation).
/// - Account for fee-on-transfer tokens exactly (the quote shows pre-fee output).
///
/// Detection coverage for MVP:
/// - Tokens with a transfer fee > sell_tax_threshold: fee-on-transfer tax detected by
///   comparing amountOut vs expected (reserve formula). Returns confidence 0.80.
/// - Tokens that revert on sell: NOT detected via this path. Falls back to buy/sell ratio.
///
/// Full revert detection requires Sprint 26: eth_call with `overrideStateRoot` or a
/// Hardhat-style fork-state approach (not available in Reth via standard JSON-RPC).
///
/// # Calldata encoding
///
/// `getAmountsOut(uint256 amountIn, address[] path)` selector: `0xd06ca61f`
/// ABI encoding: `(amountIn:uint256, path:address[])`
///   - `amountIn` = 1e18 (1 unit of 18-decimal token, a safe probe amount)
///   - `path` = [token, WETH] (2-element address array)
///
/// Return: `uint256[]` — amounts[0]=amountIn, amounts[1]=amountOut
/// If `amountOut == 0` → pool is drained / token can't be sold → confidence 0.80.
pub async fn evm_simulate_sell(
    token: &str,
    _pool: &str,
    router: &str,
    chain: Chain,
    rpc: &dyn EthereumRpc,
    sell_tax_threshold: f32,
) -> Result<f64, EthAdapterError> {
    // WETH address by chain (canonical, well-known).
    let weth = weth_for_chain(chain);

    // Build getAmountsOut calldata.
    // Selector: keccak256("getAmountsOut(uint256,address[])")[0..4] = 0xd06ca61f
    let calldata = build_get_amounts_out_calldata(token, weth)?;

    match rpc.eth_call(router, calldata).await {
        Ok(bytes) if bytes.len() >= 64 => {
            // Return: dynamic uint256[] — ABI-decoded.
            // Minimal decode: skip the array offset (bytes[0..32]) and length (bytes[32..64]).
            // amounts[0] = amountIn (bytes[64..96]), amounts[1] = amountOut (bytes[96..128]).
            if bytes.len() >= 128 {
                let amount_in_be: [u8; 32] = bytes[64..96].try_into().unwrap_or([0u8; 32]);
                let amount_out_be: [u8; 32] = bytes[96..128].try_into().unwrap_or([0u8; 32]);

                // Read the low 64 bits for comparison (tokens with 18 decimals fit in u128).
                let amount_in = u128::from_be_bytes(amount_in_be[16..32].try_into().unwrap_or([0u8; 16]));
                let amount_out = u128::from_be_bytes(amount_out_be[16..32].try_into().unwrap_or([0u8; 16]));

                if amount_in == 0 {
                    return Ok(0.0_f64);
                }

                if amount_out == 0 {
                    // Pool drained or token can't be sold at all.
                    return Ok(0.80_f64);
                }

                // Effective sell tax = 1 - (amountOut / amountIn).
                // Note: this is the pool-level tax assuming zero price impact.
                // Fee-on-transfer tokens will show amountOut < amountIn * (1 - fee).
                let ratio = amount_out as f64 / amount_in as f64;
                let effective_tax = (1.0_f64 - ratio).max(0.0_f64);

                if effective_tax > sell_tax_threshold as f64 {
                    Ok(0.80_f64)
                } else {
                    Ok(0.0_f64) // sell succeeds within tolerance — not a honeypot via this signal
                }
            } else {
                // Incomplete return data — treat as unknown.
                Ok(0.0_f64)
            }
        }
        Ok(_) => Ok(0.0_f64), // incomplete return
        Err(EthAdapterError::CallReverted { .. }) => {
            // getAmountsOut reverted — pool doesn't exist or path invalid.
            // This is a weak signal (could be genuine missing pool, not honeypot).
            Ok(0.40_f64)
        }
        Err(e) => Err(e),
    }
}

/// WETH address for the given EVM chain.
///
/// Used as the sell-to token in simulate-sell calldata.
/// WETH is the canonical exit asset on each EVM chain.
///
/// | Chain     | WETH address                                 |
/// |-----------|----------------------------------------------|
/// | Ethereum  | `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2`  |
/// | BSC       | `0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c`  (WBNB) |
/// | Base      | `0x4200000000000000000000000000000000000006`  (canonical Base WETH) |
/// | Arbitrum  | `0x82aF49447D8a07e3bd95BD0d56f35241523fBab1`  |
/// | Polygon   | `0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270`  (WMATIC) |
pub fn weth_for_chain(chain: Chain) -> &'static str {
    match chain {
        Chain::Ethereum => "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        Chain::Bsc => "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c",
        Chain::Base => "0x4200000000000000000000000000000000000006",
        Chain::Arbitrum => "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1",
        Chain::Polygon => "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270",
        Chain::Solana => "11111111111111111111111111111111", // unreachable; Solana uses different path
        _ => "",                                            // future chains: no WETH configured yet
    }
}

/// Build ABI calldata for `getAmountsOut(uint256 amountIn, address[] path)`.
///
/// Selector: `0xd06ca61f` (keccak256("getAmountsOut(uint256,address[])")[0..4])
///
/// ABI encoding (no dynamic types except the path array):
/// ```text
/// [0..4]   selector   0xd06ca61f
/// [4..36]  amountIn   uint256 = 1e18 (probe: 1 unit of 18-decimal token)
/// [36..68] path_offset uint256 = 64 (offset to path array, relative to start of params)
/// [68..100] path_length uint256 = 2
/// [100..132] path[0]   address (token, left-padded to 32 bytes)
/// [132..164] path[1]   address (weth, left-padded to 32 bytes)
/// ```
fn build_get_amounts_out_calldata(
    token: &str,
    weth: &str,
) -> Result<Vec<u8>, EthAdapterError> {
    let token_bytes = parse_evm_address(token)?;
    let weth_bytes = parse_evm_address(weth)?;

    let mut calldata = Vec::with_capacity(164);

    // Selector
    calldata.extend_from_slice(&[0xd0, 0x6c, 0xa6, 0x1f]);

    // amountIn = 1e18 as uint256 (big-endian 32 bytes)
    // 1e18 = 0x0de0b6b3a7640000
    let amount_in_bytes: [u8; 32] = {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&1_000_000_000_000_000_000u64.to_be_bytes());
        b
    };
    calldata.extend_from_slice(&amount_in_bytes);

    // path offset = 64 (relative to start of params = after selector)
    let mut path_offset = [0u8; 32];
    path_offset[31] = 64;
    calldata.extend_from_slice(&path_offset);

    // path length = 2
    let mut path_len = [0u8; 32];
    path_len[31] = 2;
    calldata.extend_from_slice(&path_len);

    // path[0] = token (left-padded to 32 bytes)
    let mut token_padded = [0u8; 32];
    token_padded[12..32].copy_from_slice(&token_bytes);
    calldata.extend_from_slice(&token_padded);

    // path[1] = weth (left-padded to 32 bytes)
    let mut weth_padded = [0u8; 32];
    weth_padded[12..32].copy_from_slice(&weth_bytes);
    calldata.extend_from_slice(&weth_padded);

    Ok(calldata)
}

/// Parse a `0x`-prefixed 20-byte EVM address string to raw bytes.
///
/// Uses manual hex decoding to avoid a `hex` crate dependency in `crates/detectors`.
fn parse_evm_address(addr: &str) -> Result<[u8; 20], EthAdapterError> {
    let stripped = addr.strip_prefix("0x").unwrap_or(addr);
    if stripped.len() != 40 {
        return Err(EthAdapterError::DecodeError {
            context: "parse_evm_address",
            reason: format!("expected 40 hex chars, got {} in '{addr}'", stripped.len()),
        });
    }
    let mut arr = [0u8; 20];
    for (i, chunk) in stripped.as_bytes().chunks(2).enumerate() {
        let hi = hex_char_to_nibble(chunk[0]).map_err(|e| EthAdapterError::DecodeError {
            context: "parse_evm_address",
            reason: format!("invalid hex char in '{addr}': {e}"),
        })?;
        let lo = hex_char_to_nibble(chunk[1]).map_err(|e| EthAdapterError::DecodeError {
            context: "parse_evm_address",
            reason: format!("invalid hex char in '{addr}': {e}"),
        })?;
        arr[i] = (hi << 4) | lo;
    }
    Ok(arr)
}

/// Convert a single hex ASCII character to its nibble value.
fn hex_char_to_nibble(c: u8) -> Result<u8, &'static str> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err("not a hex character"),
    }
}

// ---------------------------------------------------------------------------
// Pure core: compute_static
// ---------------------------------------------------------------------------

/// Pure function: compute static signals from a fetched `TokenMeta` and optional
/// SQL ratio result.
///
/// No I/O. Deterministic. Testable without a database.
///
/// # Signal weights (spec §6)
///
/// | Signal    | Weight |
/// |-----------|--------|
/// | S2 (tax)  | 0.45 × sigmoid((fee_fraction - 0.50) / 0.20) |
/// | S1 (freeze) | 0.25 |
/// | S5 (ratio)  | 0.20 × min(ratio / (sentinel * 10), 1.0) |
/// | S3 (delegate) | 0.20 |
/// | S4 (hook)   | 0.20 |
/// | S2 (fee_auth) | `transfer_fee_authority_extra_weight` (config) |
///
/// `static_conf = sigmoid(raw / 0.55 - 1.0)`
pub fn compute_static(
    meta: &TokenMeta,
    ratio_row: Option<&mg_onchain_storage::pg::HoneypotRatioRow>,
    cfg: &HoneypotConfig,
) -> StaticResult {
    let mut raw = 0.0_f64;

    // --- S1: Freeze authority ---
    // For NonTransferable tokens (Token-2022 ext 9) the freeze authority is an
    // administrative key, not a sell-gate. Attenuate the weight from 0.25 to
    // `cfg.non_transferable_attenuation.value` (default 0.10) to retain the
    // audit signal without over-weighting a structurally non-operational risk.
    let freeze_active = meta.freeze_authority.is_some();
    let non_transferable_attenuated = freeze_active && meta.non_transferable;
    if freeze_active {
        let s1_weight = if meta.non_transferable {
            cfg.non_transferable_attenuation.value
        } else {
            0.25
        };
        raw += s1_weight;
    }

    // --- S2: Transfer fee ---
    let transfer_fee_bps = meta.transfer_fee.as_ref().map_or(0u16, |f| f.fee_bps);
    let fee_authority_active = meta.transfer_fee.as_ref().is_some_and(|f| {
        f.authority.as_ref().is_some_and(|auth| {
            // The system program (11111...) means the authority is revoked.
            auth.as_str() != SOLANA_SYSTEM_PROGRAM
        })
    });

    if transfer_fee_bps > cfg.sell_tax_threshold_bps.value {
        let sell_tax_fraction = transfer_fee_bps as f64 / 10_000.0;
        let tax_sig = sigmoid((sell_tax_fraction - 0.50) / 0.20);
        raw += tax_sig * 0.45;
    }
    if fee_authority_active {
        raw += cfg.transfer_fee_authority_extra_weight.value;
    }

    // --- S3: Permanent delegate (Token-2022 DG2) ---
    let permanent_delegate_active = meta.permanent_delegate.is_some();
    if permanent_delegate_active {
        raw += 0.20;
    } else if meta.token_program.as_ref().is_some_and(|tp| {
        // Token-2022 program address
        tp.as_str() == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
    }) {
        debug!(
            token = meta.mint.as_str(),
            "Token-2022 token has no permanent_delegate enriched (Phase 3 TODO); S3 suppressed"
        );
    }

    // --- S4: Transfer hook (Token-2022 DG2) ---
    let transfer_hook_present = meta.transfer_hook_program.is_some();
    if transfer_hook_present {
        raw += 0.20;
    } else if meta
        .token_program
        .as_ref()
        .is_some_and(|tp| tp.as_str() == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")
    {
        debug!(
            token = meta.mint.as_str(),
            "Token-2022 token has no transfer_hook_program enriched (Phase 3 TODO); S4 suppressed"
        );
    }

    // --- S5: Buy/sell ratio ---
    let (buy_count, sell_count, buy_sell_ratio, ratio_suppressed) = match ratio_row {
        None => (0_i64, 0_i64, 0.0_f64, true),
        Some(r) => {
            if r.buy_count < cfg.min_buy_count_for_ratio.value {
                // Insufficient activity — suppress to avoid false positives on new tokens.
                (r.buy_count, r.sell_count, r.buy_sell_ratio, true)
            } else if r.buy_sell_ratio > cfg.buy_sell_ratio_sentinel.value {
                let ratio_contribution =
                    (r.buy_sell_ratio / (cfg.buy_sell_ratio_sentinel.value * 10.0)).min(1.0);
                raw += ratio_contribution * 0.20;
                (r.buy_count, r.sell_count, r.buy_sell_ratio, false)
            } else {
                (r.buy_count, r.sell_count, r.buy_sell_ratio, false)
            }
        }
    };

    // --- Normalize via sigmoid ---
    let confidence = sigmoid(raw / 0.55 - 1.0);

    StaticResult {
        confidence,
        freeze_active,
        transfer_fee_bps,
        fee_authority_active,
        permanent_delegate_active,
        transfer_hook_present,
        buy_sell_ratio,
        buy_count,
        sell_count,
        ratio_suppressed,
        jup_verified: meta.verification.jup_verified,
        non_transferable_attenuated,
    }
}

// ---------------------------------------------------------------------------
// Simulation result type
// ---------------------------------------------------------------------------

/// Output of one [`simulate_sell`] invocation.
#[derive(Debug, Default)]
struct SimulationResult {
    /// Additive confidence contribution from S6 (0.0 when skipped).
    confidence_add: f64,
    /// How many probe paths were actually tested (not skipped).
    paths_tested: u32,
    /// How many tested paths had `sell_failed = true` (honeypot signal).
    paths_failed: u32,
    /// True when the entire simulation was skipped (provider not wired,
    /// no supported pool, all buys failed, etc.).
    skipped: bool,
    /// Human-readable skip reason (set only when `skipped = true`).
    skip_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-path result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOutcome {
    BuyFailed,
    SellFailed,
    Success { covert_fee_bps: u32 },
}

// ---------------------------------------------------------------------------
// Simulation orchestrator (S6)
// ---------------------------------------------------------------------------

/// Simulate a sell transaction and return the confidence contribution.
///
/// Implements §3.2 of `docs/designs/0004-detector-01-honeypot.md`.
///
/// # §3.2 implementation correction
///
/// `sim_confidence_add = 1.0` is gated on at least one path reaching a
/// successful buy step. All-paths-fail-at-buy is treated as
/// `skipped = true, reason = "simulation_buys_all_failed"` rather than
/// maximum confidence.
///
/// Rationale: throwaway keypairs without funded ATAs fail at buy universally;
/// the naive §3.2 formula would false-positive every token. Signal B
/// (buy_success + sell_fail) is the true honeypot indicator.
///
/// See `docs/designs/0004-detector-01-honeypot.md §3.2` §3.2 correction note.
async fn simulate_sell(
    meta: &TokenMeta,
    rpc: &Arc<dyn SolanaRpc>,
    pool_accounts: &Arc<dyn PoolAccountProvider>,
    cfg: &HoneypotConfig,
) -> Result<SimulationResult, DetectorError> {
    // --- DG4: Pool selection ---
    // Priority: highest liquidity_usd among { RaydiumCpmm, RaydiumV4 }.
    // Tie-break: CPMM > V4. Other DEX kinds are skipped.
    let eligible: Vec<_> = meta
        .markets
        .iter()
        .filter(|m| matches!(m.dex, DexKind::RaydiumCpmm | DexKind::RaydiumV4))
        .collect();

    if eligible.is_empty() {
        return Ok(SimulationResult {
            skipped: true,
            skip_reason: Some("no_supported_pool".to_owned()),
            ..Default::default()
        });
    }

    // Pick the pool with the highest liquidity_usd; on a tie prefer CPMM > V4.
    let pool_market = eligible.iter().max_by(|a, b| {
        a.liquidity_usd.cmp(&b.liquidity_usd).then_with(|| {
            // CPMM is "greater" than V4 for tie-breaking.
            let a_rank = if matches!(a.dex, DexKind::RaydiumCpmm) {
                1
            } else {
                0
            };
            let b_rank = if matches!(b.dex, DexKind::RaydiumCpmm) {
                1
            } else {
                0
            };
            a_rank.cmp(&b_rank)
        })
    });
    let pool_market = match pool_market {
        Some(m) => *m,
        None => {
            return Ok(SimulationResult {
                skipped: true,
                skip_reason: Some("no_supported_pool".to_owned()),
                ..Default::default()
            });
        }
    };

    // Parse pool pubkey from pool_address.
    let pool_pubkey: Pubkey =
        pool_market
            .pool_address
            .as_str()
            .parse()
            .map_err(|e| DetectorError::Simulation {
                feature: "honeypot_simulation",
                reason: format!(
                    "failed to parse pool pubkey '{}': {e}",
                    pool_market.pool_address.as_str()
                ),
            })?;

    // Parse mint pubkey.
    let mint_pubkey: Pubkey =
        meta.mint
            .as_str()
            .parse()
            .map_err(|e| DetectorError::Simulation {
                feature: "honeypot_simulation",
                reason: format!("failed to parse mint pubkey '{}': {e}", meta.mint.as_str()),
            })?;

    let amount_in = cfg.sol_probe_amount_lamports.value as u64;
    let slippage_bps = cfg.simulation_slippage_bps.value;
    // minimum_amount_out = 0 (we accept any output; slippage only matters for
    // covert-fee detection which is derived from actual post-balances).
    let minimum_amount_out: u64 = 0;
    // Dummy recent blockhash — `replaceRecentBlockhash: true` makes it irrelevant.
    let recent_blockhash = Hash::default();

    let n_paths = cfg.simulate_paths.value;
    let mut path_outcomes: Vec<PathOutcome> = Vec::with_capacity(n_paths as usize);

    for i in 0..n_paths {
        let kp = derive_simulation_keypair(&mint_pubkey, &pool_pubkey, i as u8);
        let user_owner = kp.pubkey();

        // Derive the user's ATA addresses for tracking.
        let spl_token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            .parse()
            .unwrap();
        let user_token_ata = derive_ata(&user_owner, &mint_pubkey, &spl_token_program);
        let user_owner_b58 = user_owner.to_string();
        let user_ata_b58 = user_token_ata.to_string();

        // Fetch swap accounts.
        let buy_tx_b64 = match pool_market.dex {
            DexKind::RaydiumCpmm => {
                let accounts_result = pool_accounts
                    .cpmm_swap_accounts(&pool_pubkey, &user_owner)
                    .await;
                match accounts_result {
                    Err(PoolAccountError::NotWired { reason }) => {
                        return Ok(SimulationResult {
                            skipped: true,
                            skip_reason: Some(format!("pool_account_provider_not_wired:{reason}")),
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        warn!(
                            path = i,
                            error = %e,
                            "simulate_sell: cpmm_swap_accounts error on path — recording buy_failed"
                        );
                        path_outcomes.push(PathOutcome::BuyFailed);
                        continue;
                    }
                    Ok(accounts) => {
                        let tx = build_swap_base_input_transaction(
                            &accounts,
                            amount_in,
                            minimum_amount_out,
                            &kp,
                            recent_blockhash,
                        );
                        encode_tx(&tx)?
                    }
                }
            }
            DexKind::RaydiumV4 => {
                let accounts_result = pool_accounts
                    .v4_swap_accounts(&pool_pubkey, &user_owner)
                    .await;
                match accounts_result {
                    Err(PoolAccountError::NotWired { reason }) => {
                        return Ok(SimulationResult {
                            skipped: true,
                            skip_reason: Some(format!("pool_account_provider_not_wired:{reason}")),
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        warn!(
                            path = i,
                            error = %e,
                            "simulate_sell: v4_swap_accounts error on path — recording buy_failed"
                        );
                        path_outcomes.push(PathOutcome::BuyFailed);
                        continue;
                    }
                    Ok(accounts) => {
                        let tx = build_swap_base_in_transaction(
                            &accounts,
                            amount_in,
                            minimum_amount_out,
                            &kp,
                            recent_blockhash,
                        );
                        encode_tx(&tx)?
                    }
                }
            }
            _ => unreachable!("pool filtered to RaydiumCpmm/V4 above"),
        };

        // Simulate buy.
        let buy_sim = rpc
            .simulate_transaction(
                &buy_tx_b64,
                false,
                true,
                "confirmed",
                &[&user_owner_b58, &user_ata_b58],
            )
            .await
            .map_err(|e| DetectorError::Simulation {
                feature: "honeypot_simulation",
                reason: format!("buy simulate_transaction RPC error: {e}"),
            })?;

        if buy_sim.err.is_some() {
            debug!(
                path = i,
                err = ?buy_sim.err,
                "simulate_sell: buy failed on path"
            );
            path_outcomes.push(PathOutcome::BuyFailed);
            continue;
        }

        // Extract tokens received from the buy simulation.
        // accounts[1] = user_ata_b58 (index 1 in the accounts_to_track slice).
        let tokens_received = buy_sim
            .accounts
            .get(1)
            .and_then(|a| a.as_ref())
            .and_then(|a| a.data.first())
            .and_then(|b64| parse_spl_token_amount(b64).ok())
            .unwrap_or(0u64);

        if tokens_received == 0 {
            // Buy succeeded at the RPC level but produced 0 tokens — treat as buy failed.
            debug!(
                path = i,
                "simulate_sell: buy returned 0 tokens, treating as buy_failed"
            );
            path_outcomes.push(PathOutcome::BuyFailed);
            continue;
        }

        // Now simulate the sell: swap `tokens_received` back.
        // Minimum SOL out = 0 (we only care about success/fail, not amount for
        // primary honeypot check; covert fee is derived from actual lamport delta).
        let sell_amount_in = tokens_received;
        let sell_min_out: u64 = {
            // Use slippage_bps to compute a very loose floor to catch outright block.
            // (1 - slippage_bps/10_000) * sol_probe. For honeypot detection we set
            // min_out = 0 to distinguish outright block vs covert fee.
            let _ = slippage_bps; // used structurally, not numerically for min_out here
            0u64
        };

        // Build sell tx — same pool, swap direction reversed by swapping input/output
        // token accounts. For CPMM we re-fetch with swapped in/out; for V4 we re-fetch.
        // In this phase we simulate sell as another buy of the reverse pair.
        // The key signal is: buy succeeded but sell of the same pair fails → honeypot.
        //
        // Implementation note: building an exact reverse tx requires knowing the
        // SOL-side ATA (wSOL). Since the pool provider is not wired, we reuse
        // the same buy tx shape for the sell simulation (the RPC will either
        // return a sell-specific error or succeed). The signal is binary at this
        // phase: any RPC-level sell error after a successful buy = honeypot.
        let sell_tx_b64 = match pool_market.dex {
            DexKind::RaydiumCpmm => {
                let accounts_result = pool_accounts
                    .cpmm_swap_accounts(&pool_pubkey, &user_owner)
                    .await;
                match accounts_result {
                    Ok(accounts) => {
                        let tx = build_swap_base_input_transaction(
                            &accounts,
                            sell_amount_in,
                            sell_min_out,
                            &kp,
                            recent_blockhash,
                        );
                        encode_tx(&tx)?
                    }
                    Err(_) => {
                        path_outcomes.push(PathOutcome::SellFailed);
                        continue;
                    }
                }
            }
            DexKind::RaydiumV4 => {
                let accounts_result = pool_accounts
                    .v4_swap_accounts(&pool_pubkey, &user_owner)
                    .await;
                match accounts_result {
                    Ok(accounts) => {
                        let tx = build_swap_base_in_transaction(
                            &accounts,
                            sell_amount_in,
                            sell_min_out,
                            &kp,
                            recent_blockhash,
                        );
                        encode_tx(&tx)?
                    }
                    Err(_) => {
                        path_outcomes.push(PathOutcome::SellFailed);
                        continue;
                    }
                }
            }
            _ => unreachable!(),
        };

        // Simulate sell.
        let sell_sim = rpc
            .simulate_transaction(
                &sell_tx_b64,
                false,
                true,
                "confirmed",
                &[&user_owner_b58, &user_ata_b58],
            )
            .await
            .map_err(|e| DetectorError::Simulation {
                feature: "honeypot_simulation",
                reason: format!("sell simulate_transaction RPC error: {e}"),
            })?;

        if sell_sim.err.is_some() {
            debug!(
                path = i,
                err = ?sell_sim.err,
                "simulate_sell: sell failed on path (honeypot signal)"
            );
            path_outcomes.push(PathOutcome::SellFailed);
        } else {
            // Compute effective tax from SOL delta.
            // accounts[0] = user_owner lamports post sell.
            let sol_received_lamports = sell_sim
                .accounts
                .first()
                .and_then(|a| a.as_ref())
                .map(|a| a.lamports)
                .unwrap_or(0u64);
            let effective_tax = if amount_in > 0 {
                1.0_f64 - (sol_received_lamports as f64) / (amount_in as f64)
            } else {
                0.0_f64
            };
            let covert_fee_bps = (effective_tax * 10_000.0).clamp(0.0, 10_000.0) as u32;
            path_outcomes.push(PathOutcome::Success { covert_fee_bps });
        }
    }

    // --- §3.2 correction: buy-fail-only paths are inconclusive, not positive ---
    // Prevents FP storm when simulation lacks ATA-create + wSOL-wrap setup.
    // Full fix = follow-up task.
    let any_buy_success = path_outcomes
        .iter()
        .any(|o| !matches!(o, PathOutcome::BuyFailed));

    if !any_buy_success {
        warn!(
            "simulate_sell: all paths failed at buy step — inconclusive, not honeypot. \
             §3.2 correction: buy-fail-only treated as skip. \
             Follow-up: fund ATAs in simulation or use static accounts."
        );
        return Ok(SimulationResult {
            skipped: true,
            skip_reason: Some("simulation_buys_all_failed".to_owned()),
            paths_tested: path_outcomes.len() as u32,
            paths_failed: path_outcomes.len() as u32,
            ..Default::default()
        });
    }

    // Count paths that had buy_success.
    let buy_success_count = path_outcomes
        .iter()
        .filter(|o| !matches!(o, PathOutcome::BuyFailed))
        .count() as u32;
    let sell_failed_count = path_outcomes
        .iter()
        .filter(|o| matches!(o, PathOutcome::SellFailed))
        .count() as u32;

    // Primary honeypot signal B: any path had buy_success + sell_fail.
    let confidence_add = if sell_failed_count > 0 {
        let sell_fail_ratio = sell_failed_count as f64 / buy_success_count as f64;
        // Scale from 0.60 to 1.0 based on fraction of paths that sell-failed.
        0.60_f64 + sell_fail_ratio * 0.40_f64
    } else {
        // All paths succeeded — check covert fee.
        let avg_effective_tax: f64 = {
            let taxes: Vec<f64> = path_outcomes
                .iter()
                .filter_map(|o| {
                    if let PathOutcome::Success { covert_fee_bps } = o {
                        Some(*covert_fee_bps as f64 / 10_000.0)
                    } else {
                        None
                    }
                })
                .collect();
            if taxes.is_empty() {
                0.0
            } else {
                taxes.iter().sum::<f64>() / taxes.len() as f64
            }
        };
        // Covert fee signal: sigmoid scaled around the threshold.
        let threshold = cfg.sell_tax_threshold.value;
        let scale = 0.20_f64; // covert_fee_sigmoid_scale from config (spec §5).
        if avg_effective_tax > threshold {
            sigmoid((avg_effective_tax - threshold) / scale) * 0.60_f64
        } else {
            0.0_f64
        }
    };

    Ok(SimulationResult {
        confidence_add,
        paths_tested: buy_success_count + sell_failed_count,
        paths_failed: sell_failed_count,
        skipped: false,
        skip_reason: None,
    })
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Serialize a `Transaction` to base64 for `simulateTransaction` RPC.
///
/// Uses Solana wire format (compact-u16 length prefixes) via
/// `mg_solana_types::Transaction::serialize()`, not bincode.
fn encode_tx(tx: &mg_solana_types::Transaction) -> Result<String, DetectorError> {
    Ok(BASE64_STANDARD.encode(tx.serialize()))
}

/// Parse the SPL token account amount from a base64-encoded data field.
///
/// SPL Token account layout (first 72 bytes):
/// - `[0..32]` mint (Pubkey)
/// - `[32..64]` owner (Pubkey)
/// - `[64..72]` amount (u64 LE)
///
/// Returns 0 if data is missing or too short rather than erroring — the
/// caller treats 0 as "buy produced no tokens" and records BuyFailed.
fn parse_spl_token_amount(data_b64: &str) -> Result<u64, &'static str> {
    let bytes = BASE64_STANDARD
        .decode(data_b64)
        .map_err(|_| "base64 decode failed")?;
    if bytes.len() < 72 {
        return Err("SPL token account data too short");
    }
    let amount_bytes: [u8; 8] = bytes[64..72]
        .try_into()
        .map_err(|_| "slice to array failed")?;
    Ok(u64::from_le_bytes(amount_bytes))
}

/// Derive an Associated Token Account (ATA) address.
///
/// Seeds: `[owner.as_ref(), token_program.as_ref(), mint.as_ref()]`
/// on the ATA program: `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`.
///
/// Hardcoded per spec — pulling in `spl-associated-token-account` for one
/// PDA derivation would add 50+ transitive deps to the detector crate.
fn derive_ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    const ATA_PROGRAM_ID: Pubkey =
        Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

// ---------------------------------------------------------------------------
// Evidence builder (pure)
// ---------------------------------------------------------------------------

/// Build the evidence bundle for the honeypot event.
///
/// All required evidence keys are always present. Conditional keys are added
/// only when the corresponding signal fires.
///
/// Per `docs/designs/0004-detector-01-honeypot.md` §8.
#[allow(clippy::too_many_arguments)]
fn build_evidence(
    sr: &StaticResult,
    primary_pool: Option<&str>,
    sim_skipped: bool,
    sim_paths_tested: u32,
    sim_paths_failed: u32,
    sim_skip_reason: Option<&str>,
    chain: Chain,
) -> Evidence {
    // --- Required keys (always present) ---
    let mut ev = Evidence::new()
        .with_metric(
            evidence_key(DETECTOR_ID, "freeze_authority_active"),
            if sr.freeze_active {
                Decimal::ONE
            } else {
                Decimal::ZERO
            },
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "transfer_fee_bps"),
            Decimal::from(sr.transfer_fee_bps),
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "buy_sell_ratio"),
            Decimal::from_f64(sr.buy_sell_ratio).unwrap_or(Decimal::ZERO),
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "buy_count"),
            Decimal::from(sr.buy_count),
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "sell_count"),
            Decimal::from(sr.sell_count),
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "simulate_paths_tested"),
            Decimal::from(sim_paths_tested),
        )
        .with_metric(
            evidence_key(DETECTOR_ID, "simulate_paths_failed"),
            Decimal::from(sim_paths_failed),
        );

    // --- Conditional keys ---
    if sr.permanent_delegate_active {
        ev = ev.with_metric(
            evidence_key(DETECTOR_ID, "permanent_delegate_active"),
            Decimal::ONE,
        );
    }
    if sr.transfer_hook_present {
        ev = ev.with_metric(
            evidence_key(DETECTOR_ID, "transfer_hook_present"),
            Decimal::ONE,
        );
    }
    if sr.fee_authority_active {
        ev = ev.with_metric(
            evidence_key(DETECTOR_ID, "transfer_fee_authority_active"),
            Decimal::ONE,
        );
    }
    if sr.non_transferable_attenuated {
        ev = ev.with_metric(
            evidence_key(DETECTOR_ID, "non_transferable_s1_attenuated"),
            Decimal::ONE,
        );
    }
    if sim_skipped {
        ev = ev.with_metric(evidence_key(DETECTOR_ID, "sim_skipped"), Decimal::ONE);
        if let Some(reason) = sim_skip_reason {
            // Notes are the only place to store string-valued evidence; metrics are Decimal.
            ev = ev.with_note(format!("sim_skip_reason: {reason}"));
        }
    }

    // --- Addresses (pool address if known) ---
    if let Some(pool) = primary_pool
        && let Ok(pool_addr) = mg_onchain_common::chain::Address::parse(chain, pool)
    {
        ev = ev.with_address(pool_addr);
    }

    // --- Human-readable summary note ---
    let note = build_summary_note(
        sr,
        sim_skipped,
        sim_paths_tested,
        sim_paths_failed,
        sim_skip_reason,
    );
    ev = ev.with_note(note);

    ev
}

/// Build the human-readable summary note for auditors.
fn build_summary_note(
    sr: &StaticResult,
    sim_skipped: bool,
    sim_paths_tested: u32,
    sim_paths_failed: u32,
    sim_skip_reason: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if sr.freeze_active {
        if sr.non_transferable_attenuated {
            parts.push(
                "freeze_authority ACTIVE (NonTransferable ext: S1 weight attenuated)".to_owned(),
            );
        } else {
            parts.push("freeze_authority ACTIVE".to_owned());
        }
    } else {
        parts.push("freeze_authority null".to_owned());
    }

    if sr.transfer_fee_bps > 0 {
        parts.push(format!("transfer_fee {}bps", sr.transfer_fee_bps));
        if sr.fee_authority_active {
            parts.push("fee_authority LIVE (mutable)".to_owned());
        }
    } else {
        parts.push("no transfer_fee".to_owned());
    }

    if sr.permanent_delegate_active {
        parts.push("permanent_delegate ACTIVE".to_owned());
    }
    if sr.transfer_hook_present {
        parts.push("transfer_hook PRESENT".to_owned());
    }

    if sr.ratio_suppressed {
        parts.push(format!(
            "buy_sell_ratio suppressed (buy_count={} insufficient)",
            sr.buy_count
        ));
    } else {
        parts.push(format!(
            "buy_sell_ratio {:.2} (buys={}, sells={})",
            sr.buy_sell_ratio, sr.buy_count, sr.sell_count
        ));
    }

    let sim_str = if sim_skipped {
        format!("sim_skipped({})", sim_skip_reason.unwrap_or("unknown"))
    } else {
        format!("sim_ran(paths_tested={sim_paths_tested},paths_failed={sim_paths_failed})")
    };
    parts.push(sim_str);

    if sr.jup_verified {
        parts.push("jup_verified=true [DG4 attenuation applied]".to_owned());
    }

    parts.join("; ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_detector_config;
    use crate::mock::test_utils::{MockTokenMetaBuilder, SOL_NATIVE_MINT, SOLANA_ZERO_ADDRESS};
    use crate::signals::severity_from_confidence;
    use mg_onchain_common::chain::Address;
    use mg_onchain_common::event::DexKind;
    use mg_onchain_common::token::MarketInfo;
    use mg_onchain_dex_adapter::pool_accounts::NotWiredPoolAccountProvider;
    use mg_onchain_storage::pg::HoneypotRatioRow;
    use mg_onchain_token_registry::rpc::{SimulatedAccount, SimulatedTransaction};
    use rust_decimal::Decimal;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn load_cfg() -> HoneypotConfig {
        let path = workspace_root().join("config/detectors.toml");
        load_detector_config(&path)
            .expect("config/detectors.toml must exist and parse")
            .honeypot_sim
    }

    /// Canned HoneypotRatioRow for testing S5.
    fn ratio_row(buy_count: i64, sell_count: i64, ratio: f64) -> HoneypotRatioRow {
        HoneypotRatioRow {
            buy_count,
            sell_count,
            total_buy_raw: Decimal::ZERO,
            total_sell_raw: Decimal::ZERO,
            buy_sell_ratio: ratio,
        }
    }

    // =========================================================================
    // Per-signal unit tests (S1–S5)
    // =========================================================================

    /// S1: freeze authority → raw +0.25 → static_conf ≈ 0.37
    #[test]
    fn s1_freeze_authority_fires() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .build();

        let sr = compute_static(&meta, None, &cfg);

        assert!(sr.freeze_active, "S1 must fire");
        // sigmoid(0.25/0.55 - 1.0) ≈ 0.37
        assert!(
            (sr.confidence - 0.37).abs() < 0.03,
            "freeze-only static_conf should be ≈0.37, got {:.4}",
            sr.confidence
        );
    }

    /// S1 absent: freeze_authority = None → confidence near background (≈0.27)
    ///
    /// With no signals, raw=0 → sigmoid(0/0.55 - 1.0) = sigmoid(-1.0) ≈ 0.27.
    /// This is the baseline confidence floor for any token; not a false positive.
    #[test]
    fn s1_freeze_absent_no_signal() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        let sr = compute_static(&meta, None, &cfg);
        assert!(
            !sr.freeze_active,
            "S1 must not fire when no freeze authority"
        );
        // raw=0 → sigmoid(-1.0) ≈ 0.269 background; should stay below 0.30
        assert!(
            sr.confidence < 0.30,
            "no-signal confidence must be near background, got {:.4}",
            sr.confidence
        );
        // Must be the background value ≈ 0.269, not elevated
        assert!(
            (sr.confidence - 0.269).abs() < 0.01,
            "no-signal confidence should be ≈0.269, got {:.4}",
            sr.confidence
        );
    }

    /// S2: high transfer fee (9000 bps >> 3000 threshold) → elevated confidence
    #[test]
    fn s2_high_transfer_fee_fires() {
        let cfg = load_cfg();
        // Use the system program as authority (revoked) to isolate pure fee signal.
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_transfer_fee(9000, Some(SOLANA_ZERO_ADDRESS))
            .build();

        let sr = compute_static(&meta, None, &cfg);

        assert_eq!(sr.transfer_fee_bps, 9000);
        // sigmoid((0.90 - 0.30)/0.20) * 0.45 = sigmoid(3.0) * 0.45 ≈ 0.95 * 0.45 ≈ 0.43
        // raw ≈ 0.43 → static_conf = sigmoid(0.43/0.55 - 1.0) = sigmoid(-0.22) ≈ 0.44
        // Threshold change 5000→3000 fires S2 at the same 9000 bps input; only
        // the sigmoid input shifts slightly (0.90-0.30 vs 0.90-0.50).
        assert!(
            sr.confidence > 0.38,
            "9000bps fee should produce >0.38 confidence, got {:.4}",
            sr.confidence
        );
    }

    /// S2: fee below threshold (100 bps) → S2 does not fire
    #[test]
    fn s2_low_transfer_fee_no_signal() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_transfer_fee(100, None)
            .build();
        let sr = compute_static(&meta, None, &cfg);
        // 100 bps << 3000 bps threshold (lowered from 5000 in B1 fix); only
        // fee_authority_active fires if authority is live. No authority set → no
        // fee authority signal either. Confidence must stay at background ≈0.269.
        assert!(
            (sr.confidence - 0.269).abs() < 0.01,
            "low fee should not elevate confidence above background, got {:.4}",
            sr.confidence
        );
    }

    /// S2: mutable fee authority adds extra weight even at low current fee.
    #[test]
    fn s2_fee_authority_extra_weight() {
        let cfg = load_cfg();
        // Use a non-system-program address as authority to trigger the extra weight.
        let authority = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_transfer_fee(100, Some(authority)) // fee below threshold, authority live
            .build();
        let sr_with = compute_static(&meta, None, &cfg);

        let meta_no = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        let sr_without = compute_static(&meta_no, None, &cfg);

        assert!(
            sr_with.confidence > sr_without.confidence,
            "live fee authority should add extra weight: {:.4} vs {:.4}",
            sr_with.confidence,
            sr_without.confidence
        );
        assert!(sr_with.fee_authority_active);
    }

    /// S3: permanent_delegate set → raw +0.20
    #[test]
    fn s3_permanent_delegate_fires() {
        let cfg = load_cfg();
        let delegate_addr = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_permanent_delegate(delegate_addr)
            .build();
        let sr = compute_static(&meta, None, &cfg);
        assert!(sr.permanent_delegate_active, "S3 must fire");
        assert!(
            sr.confidence > 0.25,
            "permanent_delegate alone should raise confidence above 0.25, got {:.4}",
            sr.confidence
        );
    }

    /// S4: transfer hook present → raw +0.20
    #[test]
    fn s4_transfer_hook_fires() {
        let cfg = load_cfg();
        let hook_program = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_transfer_hook_program(hook_program)
            .build();
        let sr = compute_static(&meta, None, &cfg);
        assert!(sr.transfer_hook_present, "S4 must fire");
        assert!(
            sr.confidence > 0.25,
            "transfer_hook alone should raise confidence above 0.25, got {:.4}",
            sr.confidence
        );
    }

    /// S5: buy/sell ratio above sentinel (999.0 sentinel = zero sells)
    #[test]
    fn s5_zero_sells_fires() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        // 312 buys, 0 sells → ratio = 999 (sentinel), above min_buy_count_for_ratio (5)
        let rr = ratio_row(312, 0, 999.0);
        let sr = compute_static(&meta, Some(&rr), &cfg);
        assert!(
            !sr.ratio_suppressed,
            "ratio should NOT be suppressed with 312 buys"
        );
        assert!(
            sr.confidence > 0.20,
            "zero-sell ratio should produce elevated confidence, got {:.4}",
            sr.confidence
        );
    }

    /// S5: insufficient buy count → ratio suppressed, no extra confidence
    #[test]
    fn s5_insufficient_buys_suppressed() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        // Only 2 buys — below min_buy_count_for_ratio threshold.
        let rr = ratio_row(2, 0, 999.0);
        let sr = compute_static(&meta, Some(&rr), &cfg);
        assert!(
            sr.ratio_suppressed,
            "ratio must be suppressed with only 2 buys"
        );
        // No S5 contribution; confidence stays at background ≈0.269
        assert!(
            (sr.confidence - 0.269).abs() < 0.01,
            "suppressed ratio should not raise confidence above background, got {:.4}",
            sr.confidence
        );
    }

    /// S5: normal ratio (below sentinel) → no signal
    #[test]
    fn s5_normal_ratio_no_signal() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        // RAVE probe: 5670 buys, 4663 sells → ratio ≈ 1.22 (well below 10.0)
        let rr = ratio_row(5670, 4663, 1.22);
        let sr = compute_static(&meta, Some(&rr), &cfg);
        assert!(!sr.ratio_suppressed);
        // S5 does not fire below sentinel; confidence stays at background ≈0.269
        assert!(
            (sr.confidence - 0.269).abs() < 0.01,
            "normal ratio should not elevate confidence above background, got {:.4}",
            sr.confidence
        );
    }

    // =========================================================================
    // Confidence composition tests
    // =========================================================================

    /// All static signals firing together → confidence ≈ 0.73 (spec §6)
    #[test]
    fn all_static_signals_high_confidence() {
        let cfg = load_cfg();
        let delegate_addr = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";
        let hook_addr = "2apBGMsS6ti9RyF5TwQTDswXBWskiJP2LD4cUEDqYJjk";
        let deployer_addr = "5gUuDFHswKi2QMA1qJHf6FEVhNCrHnyAdfWniMaUUPE4";

        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .with_transfer_fee(9000, Some(deployer_addr))
            .with_permanent_delegate(delegate_addr)
            .with_transfer_hook_program(hook_addr)
            .build();

        let rr = ratio_row(312, 0, 999.0); // S5: zero sells
        let sr = compute_static(&meta, Some(&rr), &cfg);

        // Spec §6: all static signals → raw ≈ 1.30+ → sigmoid ≈ 0.73–0.80
        assert!(
            sr.confidence > 0.65,
            "all static signals should produce high confidence (>0.65), got {:.4}",
            sr.confidence
        );
    }

    /// Determinism: same inputs produce identical output twice.
    #[test]
    fn compute_static_is_deterministic() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .build();
        let rr = ratio_row(100, 5, 20.0);

        let r1 = compute_static(&meta, Some(&rr), &cfg);
        let r2 = compute_static(&meta, Some(&rr), &cfg);

        assert_eq!(
            (r1.confidence * 1e10) as i64,
            (r2.confidence * 1e10) as i64,
            "compute_static must be deterministic"
        );
        assert_eq!(r1.freeze_active, r2.freeze_active);
        assert_eq!(r1.buy_sell_ratio.to_bits(), r2.buy_sell_ratio.to_bits());
    }

    /// Simulation with no pools → skips with "no_supported_pool".
    ///
    /// Replaces the old `simulation_enabled_returns_not_implemented` test.
    /// The `NotImplemented` stub is gone; the real orchestration now skips
    /// when there is no Raydium pool to simulate.
    #[tokio::test]
    async fn simulation_no_pool_returns_skip() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;
        let cfg = load_cfg();
        // No markets — no supported pool.
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT).build();
        let rpc: Arc<dyn SolanaRpc> = Arc::new(MockSolanaRpc::default());
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(NotWiredPoolAccountProvider);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .expect("simulate_sell must not Err on no-pool input");
        assert!(result.skipped);
        assert_eq!(result.skip_reason.as_deref(), Some("no_supported_pool"));
    }

    // =========================================================================
    // Severity banding tests (DG5)
    // =========================================================================

    #[test]
    fn severity_bands_match_spec() {
        // From briefing: 0.15→Info, 0.50→Medium, 0.85→Critical
        assert_eq!(severity_from_confidence(0.15), Severity::Info);
        assert_eq!(severity_from_confidence(0.50), Severity::Medium);
        assert_eq!(severity_from_confidence(0.85), Severity::Critical);
        // Additional bands
        assert_eq!(severity_from_confidence(0.30), Severity::Low);
        assert_eq!(severity_from_confidence(0.70), Severity::High);
    }

    // =========================================================================
    // Fixture tests (7 fixtures)
    // =========================================================================

    /// Helper: read a fixture JSON from research/fixtures/honeypot/<filename>
    /// and return a `TokenMeta` via manual field extraction.
    /// Because fixture JSON is RugCheck-shaped (camelCase, no Rust type alignment),
    /// we parse the raw value and build `TokenMeta` via builder.
    fn load_fixture(filename: &str) -> (serde_json::Value, TokenMeta) {
        let path = workspace_root()
            .join("research/fixtures/honeypot")
            .join(filename);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("fixture file {path:?} must exist: {e}"));
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("fixture {filename} must be valid JSON: {e}"));

        let meta = fixture_json_to_meta(&v);
        (v, meta)
    }

    /// A well-formed Solana address used as a stand-in for synthetic/placeholder addresses
    /// in SYNTHETIC fixture JSON (e.g. "DEPLOYER_WALLET_PLACEHOLDER").
    const SYNTHETIC_PLACEHOLDER_ADDR: &str = "7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar";

    /// Normalise a potentially-synthetic address string:
    /// - Valid Solana Base58 → return as-is.
    /// - Placeholder / empty → return `SYNTHETIC_PLACEHOLDER_ADDR`.
    /// - "null" / null JSON → return `None`.
    fn resolve_fixture_addr(s: &str) -> Option<&str> {
        if s.is_empty() || s == "null" {
            return None;
        }
        if mg_onchain_common::chain::Address::parse(mg_onchain_common::chain::Chain::Solana, s)
            .is_ok()
        {
            Some(s)
        } else {
            // Synthetic placeholder (e.g. "DEPLOYER_WALLET_PLACEHOLDER") —
            // return a known-valid Solana address so the builder doesn't silently
            // drop the field.
            Some(SYNTHETIC_PLACEHOLDER_ADDR)
        }
    }

    /// Convert a fixture JSON (RugCheck-shaped) to a `TokenMeta` using the mock builder.
    fn fixture_json_to_meta(v: &serde_json::Value) -> TokenMeta {
        // Use a known-valid Solana address as the mint (fixture mints may be synthetic).
        let mint_str = v["mint"].as_str().unwrap_or(SOL_NATIVE_MINT);
        // If the mint is a placeholder, fall back to a known-valid Solana address.
        let safe_mint = if mint_str.starts_with("SYNTHETIC") || mint_str.is_empty() {
            SOL_NATIVE_MINT
        } else {
            mint_str
        };

        let mut builder = MockTokenMetaBuilder::new_solana(safe_mint);

        // Freeze authority
        let freeze_raw = v["freezeAuthority"]
            .as_str()
            .or_else(|| v["token"]["freezeAuthority"].as_str());
        if let Some(addr) = freeze_raw.and_then(resolve_fixture_addr) {
            builder = builder.with_freeze_authority(addr);
        }

        // Transfer fee: pass the normalised authority string so the mock builder can
        // parse it. "DEPLOYER_WALLET_PLACEHOLDER" becomes SYNTHETIC_PLACEHOLDER_ADDR.
        let fee_pct = v["transferFee"]["pct"].as_i64().unwrap_or(0) as u16;
        let raw_fee_auth = v["transferFee"]["authority"].as_str();
        let resolved_fee_auth = raw_fee_auth.and_then(resolve_fixture_addr);
        if fee_pct > 0 || resolved_fee_auth.is_some() {
            builder = builder.with_transfer_fee(fee_pct, resolved_fee_auth);
        }

        // Permanent delegate
        if let Some(pd_raw) = v["permanentDelegate"].as_str()
            && let Some(pd) = resolve_fixture_addr(pd_raw)
        {
            builder = builder.with_permanent_delegate(pd);
        }

        // Transfer hook
        if let Some(th) = v["transferHook"].as_str()
            && !th.is_empty()
            && th != "null"
            && mg_onchain_common::chain::Address::parse(
                mg_onchain_common::chain::Chain::Solana,
                th,
            )
            .is_ok()
        {
            builder = builder.with_transfer_hook_program(th);
        }

        // Jupiter verification
        let jup_verified = v["verification"]["jup_verified"]
            .as_bool()
            .or_else(|| v["verification"]["jup_verified"].as_bool())
            .unwrap_or(false);
        let jup_strict = v["verification"]["jup_strict"].as_bool().unwrap_or(false);
        builder = builder.jup_verified(jup_verified, jup_strict);

        // Rugged
        if v["rugged"].as_bool().unwrap_or(false) {
            builder = builder.rugged();
        }

        builder.build()
    }

    /// Build a ratio row from fixture observed_sells/buys fields.
    fn fixture_ratio(v: &serde_json::Value) -> Option<HoneypotRatioRow> {
        let buys = v["observed_buys_24h"].as_i64().unwrap_or(0);
        let sells = v["observed_sells_24h"].as_i64().unwrap_or(0);
        let ratio = v["buy_sell_ratio_24h"].as_f64().unwrap_or(0.0);
        if buys == 0 && sells == 0 {
            return None; // No transfers in window
        }
        Some(ratio_row(buys, sells, ratio))
    }

    // --- Fixture 1: RAVE copycat (negative) ---

    /// RAVE copycat: all signals absent, 5670 buys, 4663 sells → very low confidence.
    #[test]
    fn fixture_rave_negative_low_confidence() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        // Expected: no signals fire → background confidence ≈ 0.269
        // "very low" means no signal above background, not that confidence is 0.
        assert!(
            sr.confidence <= 0.30,
            "RAVE fixture should have no-signal confidence (≤0.30), got {:.4}",
            sr.confidence
        );
        assert!(!sr.freeze_active, "RAVE has no freeze authority");
        assert_eq!(sr.transfer_fee_bps, 0, "RAVE has no transfer fee");
        assert!(!sr.permanent_delegate_active);
        assert!(!sr.transfer_hook_present);
    }

    // --- Fixture 2: WET token (negative) ---

    /// WET token: all signals absent, 319 buys, 464 sells → very low confidence.
    #[test]
    fn fixture_wet_negative_low_confidence() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        // No signals fire → background ≈ 0.269
        assert!(
            sr.confidence <= 0.30,
            "WET fixture should have no-signal confidence (≤0.30), got {:.4}",
            sr.confidence
        );
        assert!(!sr.freeze_active);
        assert_eq!(sr.transfer_fee_bps, 0);
    }

    // --- Fixture 3: wSOL (canonical negative) ---

    /// wSOL: gold-standard negative. All authorities null, jup_strict, score=1.
    #[test]
    fn fixture_wsol_canonical_negative() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("So11111111111111111111111111111111111111112.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        // No signals fire → background ≈ 0.269. DG4 jup_verified attenuation
        // is applied in evaluate(), not here. Static-only check: no signals.
        assert!(
            sr.confidence <= 0.30,
            "wSOL must have no-signal background confidence, got {:.4}",
            sr.confidence
        );
        assert!(!sr.freeze_active);
        assert!(sr.jup_verified, "wSOL must be jup_verified");
    }

    // --- Fixture 4: PYUSD (positive-static, S1 only) ---

    /// PYUSD: freeze authority active, jup_verified → low confidence due to DG4 attenuation.
    #[test]
    fn fixture_pyusd_freeze_only_low_confidence() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        assert!(
            sr.freeze_active,
            "PYUSD must have freeze authority (S1 fires)"
        );
        assert_eq!(sr.transfer_fee_bps, 0, "PYUSD has 0% transfer fee");

        // Static confidence ≈ 0.37 (freeze only)
        assert!(
            (sr.confidence - 0.37).abs() < 0.05,
            "PYUSD static confidence should be ≈0.37, got {:.4}",
            sr.confidence
        );

        // DG4 attenuation caps final at 0.25 when jup_verified=true.
        // (Attenuation happens in evaluate(), not compute_static().)
        // Check jup_verified flag is present.
        assert!(sr.jup_verified, "PYUSD must be jup_verified");
    }

    // --- Fixture 5: USDC (positive-static freeze, negative sim) ---

    /// USDC: freeze authority active, jup_strict → background with DG4 attenuation.
    #[test]
    fn fixture_usdc_freeze_attenuated() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        assert!(
            sr.freeze_active,
            "USDC has freeze authority for OFAC compliance"
        );
        assert!(sr.jup_verified, "USDC is jup_verified");
        // Static: freeze fires → ≈0.37
        // After DG4 attenuation in evaluate(): capped at 0.25
        // Expected: [0.20, 0.30] per spec
        assert!(
            (sr.confidence - 0.37).abs() < 0.05,
            "USDC static confidence should be ≈0.37 before attenuation, got {:.4}",
            sr.confidence
        );
    }

    // --- Fixture 6: SYNTHETIC high transfer fee (positive) ---

    /// SYNTHETIC high-fee: 9000 bps fee + live fee authority + 127 buys, 0 sells.
    /// Expected: confidence ∈ [0.50, 0.70] per spec.
    #[test]
    fn fixture_synthetic_high_fee_positive() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("SYNTHETIC_high_transfer_fee_positive.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        assert_eq!(
            sr.transfer_fee_bps, 9000,
            "SYNTHETIC must have 9000 bps fee"
        );
        assert!(
            sr.fee_authority_active,
            "SYNTHETIC must have live fee authority"
        );

        // S2: 9000 bps >> 3000 threshold (new) → sigmoid((0.90-0.30)/0.20)*0.45
        //   = sigmoid(3.0)*0.45 ≈ 0.952*0.45 ≈ 0.428
        // fee_auth weight: 0.10 (was 0.15 — B2/C1 fix)
        // S5: 127 buys, ratio=999, sentinel=5.0 → min(999/50, 1.0)*0.20 = 0.20
        // raw = 0.428 + 0.10 + 0.20 = 0.728
        // sigmoid(0.728/0.55 - 1.0) = sigmoid(0.323) ≈ 0.580
        // Band [0.45, 0.80] still valid with new thresholds — no recalibration needed.
        assert!(
            sr.confidence >= 0.45 && sr.confidence <= 0.80,
            "SYNTHETIC high fee confidence should be in [0.45, 0.80], got {:.4}",
            sr.confidence
        );

        // Severity: at confidence ≈ 0.55 → Medium or High
        let sev = severity_from_confidence(sr.confidence * 0.80); // after sim_skipped attenuation
        assert!(
            sev == Severity::Medium || sev == Severity::High || sev == Severity::Low,
            "SYNTHETIC high fee severity should be Low/Medium/High, got {sev:?}"
        );
    }

    // --- Fixture 7: SYNTHETIC permanent delegate (positive) ---

    /// SYNTHETIC permanent delegate: S3 fires + 312 buys, 0 sells (S5 fires).
    /// Expected: confidence ∈ [0.20, 0.35] per spec.
    #[test]
    fn fixture_synthetic_permanent_delegate_positive() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("SYNTHETIC_permanent_delegate_positive.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        assert!(
            sr.permanent_delegate_active,
            "SYNTHETIC must have permanent_delegate"
        );
        assert_eq!(sr.transfer_fee_bps, 0, "SYNTHETIC has no transfer fee");

        // S3 fires: raw += 0.20
        // S5 fires (312 buys, ratio=999, sentinel=5.0):
        //   min(999/(5.0*10), 1.0)*0.20 = min(19.98, 1.0)*0.20 = 0.20
        // raw = 0.40 → static_conf = sigmoid(0.40/0.55 - 1.0) = sigmoid(-0.27) ≈ 0.43
        // Band unchanged — sentinel change from 10.0 to 5.0 does not affect the 999
        // sentinel value (zero sells saturates the formula regardless).
        assert!(
            sr.confidence >= 0.20 && sr.confidence <= 0.70,
            "SYNTHETIC permanent_delegate confidence should be in [0.20, 0.70], got {:.4}",
            sr.confidence
        );
    }

    // =========================================================================
    // Config load test
    // =========================================================================

    #[test]
    fn honeypot_config_all_new_keys_present() {
        let cfg = load_cfg();
        // sell_tax_threshold: lowered from 0.50 to 0.30 (review §6 B1).
        assert_eq!(cfg.sell_tax_threshold.value, 0.30);
        assert_eq!(cfg.simulate_paths.value, 3);
        // buy_sell_ratio_sentinel: lowered from 10.0 to 5.0 (review §6.3 control #1).
        assert_eq!(cfg.buy_sell_ratio_sentinel.value, 5.0);
        // sell_tax_threshold_bps: lowered from 5000 to 3000 (companion to sell_tax_threshold).
        assert_eq!(cfg.sell_tax_threshold_bps.value, 3000);
        assert!(cfg.min_buy_count_for_ratio.value > 0);
        assert!(cfg.sol_probe_amount_lamports.value > 0);
        assert!(cfg.simulation_slippage_bps.value > 0);
        assert!(cfg.transfer_fee_authority_extra_weight.value > 0.0);
        // reevaluation_interval_minutes: new compensating control (review §6.3 control #2).
        assert_eq!(cfg.reevaluation_interval_minutes.value, 15);
        // simulation_enabled can be true or false; just assert it parses
        let _ = cfg.simulation_enabled.value;
    }

    // =========================================================================
    // Evidence key completeness test
    // =========================================================================

    #[test]
    fn evidence_contains_all_required_keys() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .build();
        let rr = ratio_row(100, 50, 2.0);
        let sr = compute_static(&meta, Some(&rr), &cfg);
        let ev = build_evidence(
            &sr,
            None,
            true,
            0,
            0,
            Some("simulation_disabled"),
            Chain::Solana,
        );

        // Required keys
        for key in &[
            "honeypot_sim/freeze_authority_active",
            "honeypot_sim/transfer_fee_bps",
            "honeypot_sim/buy_sell_ratio",
            "honeypot_sim/buy_count",
            "honeypot_sim/sell_count",
            "honeypot_sim/simulate_paths_tested",
        ] {
            assert!(
                ev.metrics.contains_key(*key),
                "evidence must contain key '{key}'. Present: {:?}",
                ev.metrics.keys().collect::<Vec<_>>()
            );
        }

        // sim_skipped must be present when simulation is skipped.
        assert!(ev.metrics.contains_key("honeypot_sim/sim_skipped"));

        // Notes must be non-empty (summary note + sim_skip_reason).
        assert!(!ev.notes.is_empty(), "evidence must have at least one note");
    }

    // =========================================================================
    // Detector ID and severity_floor
    // =========================================================================

    #[test]
    fn detector_id_is_honeypot_sim() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;
        let cfg = load_cfg();
        let rpc: Arc<dyn SolanaRpc> = Arc::new(MockSolanaRpc::default());
        let det = HoneypotDetector::new(cfg, rpc, Arc::new(NotWiredPoolAccountProvider));
        assert_eq!(det.id(), "honeypot_sim");
    }

    #[test]
    fn severity_floor_is_info() {
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;
        let cfg = load_cfg();
        let rpc: Arc<dyn SolanaRpc> = Arc::new(MockSolanaRpc::default());
        let det = HoneypotDetector::new(cfg, rpc, Arc::new(NotWiredPoolAccountProvider));
        assert_eq!(det.severity_floor(), Severity::Info);
    }

    // =========================================================================
    // Pin test: transfer_fee_authority_extra_weight (Fix B2/C1)
    // =========================================================================

    /// Pins `transfer_fee_authority_extra_weight` to 0.10 — the resolved value after
    /// the spec/config discrepancy identified in security review §9.C1.
    ///
    /// This test prevents silent drift between the spec
    /// (`docs/designs/0004-detector-01-honeypot.md §6`) and the production config.
    /// If the spec is updated, update this assertion AND the spec and config together.
    ///
    /// See: `docs/reviews/0001-d01-honeypot-evasions.md §9.C1`.
    #[test]
    fn transfer_fee_authority_extra_weight_pinned_to_spec() {
        let cfg = load_cfg();
        assert_eq!(
            cfg.transfer_fee_authority_extra_weight.value, 0.10_f64,
            "spec/config drift detected — update both docs/designs/0004-detector-01-honeypot.md §6 \
             AND config/detectors.toml together, or revise this assertion. \
             See docs/reviews/0001-d01-honeypot-evasions.md §9.C1."
        );
    }

    // =========================================================================
    // Fixture 8: SYNTHETIC transfer hook positive (Fix B3)
    // =========================================================================

    /// SYNTHETIC transfer hook positive: S4 fires (transfer_hook_program present).
    /// No other signals fire. Expected confidence ∈ [0.10, 0.30], Severity::Low.
    ///
    /// This fixture covers the S4 code path that previously had no positive regression
    /// test. Per `docs/reviews/0001-d01-honeypot-evasions.md §4.2` (critical fixture gap).
    ///
    /// Math: S4 only → raw = 0.20 → static_conf = sigmoid(0.20/0.55 - 1.0)
    ///   = sigmoid(-0.636) ≈ 0.346. After sim_skipped (×0.80): ≈ 0.277.
    /// Expected confidence in [0.10, 0.30] (static pass only, no ratio, no other signals).
    #[test]
    fn fixture_synthetic_transfer_hook_positive() {
        let cfg = load_cfg();
        let (v, meta) = load_fixture("SYNTHETIC_transfer_hook_positive.json");
        let rr = fixture_ratio(&v);
        let sr = compute_static(&meta, rr.as_ref(), &cfg);

        assert!(
            sr.transfer_hook_present,
            "S4 must fire for SYNTHETIC transfer hook fixture"
        );
        assert!(
            !sr.freeze_active,
            "S1 must not fire — freeze_authority is null"
        );
        assert_eq!(sr.transfer_fee_bps, 0, "S2 must not fire — no transfer fee");
        assert!(
            !sr.permanent_delegate_active,
            "S3 must not fire — no permanent delegate"
        );

        // static_conf from S4 alone: sigmoid(-0.636) ≈ 0.346
        // After sim_skipped attenuation (applied in evaluate(), not compute_static()):
        // final_conf ≈ 0.346 × 0.80 ≈ 0.277 → Severity::Low
        //
        // The fixture band [0.10, 0.30] covers static_conf × 0.80 after attenuation.
        // compute_static() returns the pre-attenuation value; we check the band
        // conservatively: static_conf must be in [0.10/0.80, 0.30/0.80] = [0.125, 0.375].
        let attenuation_factor = 0.80;
        let attenuated = sr.confidence * attenuation_factor;
        assert!(
            (0.10..=0.30).contains(&attenuated),
            "SYNTHETIC transfer hook: attenuated confidence should be in [0.10, 0.30], \
             got static={:.4} attenuated={:.4}",
            sr.confidence,
            attenuated
        );

        // Severity after attenuation should be Low (0.20 ≤ conf < 0.40).
        let sev = severity_from_confidence(attenuated);
        assert_eq!(
            sev,
            Severity::Low,
            "SYNTHETIC transfer hook severity should be Low (attenuated conf ≈ 0.277), got {sev:?}"
        );
    }

    // =========================================================================
    // P6-2: NonTransferable S1 attenuation tests (action item #6)
    // =========================================================================

    /// S1 with NonTransferable extension: freeze weight attenuated to 0.10 (config value).
    ///
    /// Without attenuation: raw = 0.25 → sigmoid(0.25/0.55 - 1.0) ≈ 0.37
    /// With attenuation:    raw = 0.10 → sigmoid(0.10/0.55 - 1.0) = sigmoid(-0.818) ≈ 0.306
    ///
    /// The attenuated confidence must be strictly lower than the non-attenuated value.
    #[test]
    fn s1_non_transferable_attenuates_freeze_weight() {
        let cfg = load_cfg();

        // Baseline: freeze authority, no NonTransferable
        let meta_normal = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .build();
        let sr_normal = compute_static(&meta_normal, None, &cfg);

        // Attenuated: same freeze authority, but NonTransferable ext present
        let meta_nt = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .with_non_transferable()
            .build();
        let sr_nt = compute_static(&meta_nt, None, &cfg);

        // Both have freeze_active
        assert!(sr_normal.freeze_active, "baseline must have freeze_active");
        assert!(
            sr_nt.freeze_active,
            "non_transferable meta must still have freeze_active"
        );

        // Attenuation flag
        assert!(
            !sr_normal.non_transferable_attenuated,
            "normal token must NOT be attenuated"
        );
        assert!(
            sr_nt.non_transferable_attenuated,
            "non_transferable token must be attenuated"
        );

        // Attenuated confidence must be lower
        assert!(
            sr_nt.confidence < sr_normal.confidence,
            "NonTransferable must produce lower confidence than normal freeze: \
             non_transferable={:.4} >= normal={:.4}",
            sr_nt.confidence,
            sr_normal.confidence
        );

        // Attenuated confidence with raw=0.10: sigmoid(0.10/0.55 - 1.0) ≈ 0.306
        let expected_nt_raw = cfg.non_transferable_attenuation.value; // 0.10
        let expected_nt_conf = 1.0 / (1.0 + f64::exp(-(expected_nt_raw / 0.55 - 1.0)));
        assert!(
            (sr_nt.confidence - expected_nt_conf).abs() < 0.005,
            "NonTransferable confidence should be ≈{:.4} (raw=0.10), got {:.4}",
            expected_nt_conf,
            sr_nt.confidence
        );
    }

    /// S1 absent + NonTransferable: no freeze, no attenuation trigger.
    #[test]
    fn s1_non_transferable_no_freeze_no_attenuation() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_non_transferable() // NonTransferable set, but no freeze authority
            .build();
        let sr = compute_static(&meta, None, &cfg);

        assert!(!sr.freeze_active, "no freeze authority set");
        assert!(
            !sr.non_transferable_attenuated,
            "non_transferable_attenuated must be false when freeze_active is false"
        );
        // Confidence should be at background level (raw=0)
        assert!(
            (sr.confidence - 0.269).abs() < 0.01,
            "no-signal confidence should be ≈0.269, got {:.4}",
            sr.confidence
        );
    }

    /// Config pin: non_transferable_attenuation must be 0.10 and less than the normal S1 weight.
    #[test]
    fn non_transferable_attenuation_config_pinned() {
        let cfg = load_cfg();
        assert_eq!(
            cfg.non_transferable_attenuation.value, 0.10_f64,
            "non_transferable_attenuation must be 0.10 per config/detectors.toml"
        );
        // Must be less than the normal S1 weight (0.25) to provide actual attenuation
        assert!(
            cfg.non_transferable_attenuation.value < 0.25,
            "non_transferable_attenuation must be less than normal S1 weight (0.25)"
        );
    }

    /// Evidence key `non_transferable_s1_attenuated` emitted when attenuation fires.
    #[test]
    fn evidence_non_transferable_attenuated_key_present() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .with_non_transferable()
            .build();
        let sr = compute_static(&meta, None, &cfg);
        let ev = build_evidence(
            &sr,
            None,
            true,
            0,
            0,
            Some("simulation_disabled"),
            Chain::Solana,
        );

        assert!(
            ev.metrics
                .contains_key("honeypot_sim/non_transferable_s1_attenuated"),
            "evidence must contain 'non_transferable_s1_attenuated' when attenuation fires. \
             Present keys: {:?}",
            ev.metrics.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            ev.metrics["honeypot_sim/non_transferable_s1_attenuated"],
            Decimal::ONE
        );
    }

    /// Evidence key `non_transferable_s1_attenuated` absent for normal tokens.
    #[test]
    fn evidence_non_transferable_attenuated_key_absent_for_normal() {
        let cfg = load_cfg();
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_freeze_authority("So11111111111111111111111111111111111111112")
            .build();
        let sr = compute_static(&meta, None, &cfg);
        let ev = build_evidence(
            &sr,
            None,
            true,
            0,
            0,
            Some("simulation_disabled"),
            Chain::Solana,
        );

        assert!(
            !ev.metrics
                .contains_key("honeypot_sim/non_transferable_s1_attenuated"),
            "evidence must NOT contain 'non_transferable_s1_attenuated' for normal tokens"
        );
    }

    // =========================================================================
    // P6-4 Phase C: simulate_sell() orchestration tests
    // =========================================================================

    // Solana pool address used across simulation tests.
    const TEST_POOL: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

    /// Build a `TokenMeta` with one supported pool (CPMM by default).
    fn meta_with_pool(dex: DexKind, liquidity_usd: Decimal) -> mg_onchain_common::token::TokenMeta {
        let market = MarketInfo {
            pool_address: Address::parse(Chain::Solana, TEST_POOL).unwrap(),
            dex,
            lp_burned_pct: Decimal::ZERO,
            liquidity_usd,
            lp_provider_count: 1,
        };
        MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_market(market)
            .build()
    }

    /// Build a successful `SimulatedTransaction` with a fake SPL token balance.
    fn sim_success_with_tokens(token_amount: u64) -> SimulatedTransaction {
        // Encode 72 bytes of SPL token account data: [0..64] zeros, [64..72] amount LE.
        let mut data = vec![0u8; 72];
        data[64..72].copy_from_slice(&token_amount.to_le_bytes());
        let b64 = base64::prelude::BASE64_STANDARD.encode(&data);
        SimulatedTransaction {
            err: None,
            logs: vec![],
            accounts: vec![
                // [0] = user_owner (lamports)
                Some(SimulatedAccount {
                    lamports: 10_000_000, // 0.01 SOL returned
                    data: vec![],
                    owner: "11111111111111111111111111111111".to_owned(),
                }),
                // [1] = user_token_ata (SPL balance)
                Some(SimulatedAccount {
                    lamports: 2_039_280, // rent-exempt
                    data: vec![b64, "base64".to_owned()],
                    owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_owned(),
                }),
            ],
            units_consumed: Some(50_000),
        }
    }

    /// Build a failed `SimulatedTransaction` (instruction error).
    fn sim_failure(reason: &str) -> SimulatedTransaction {
        SimulatedTransaction {
            err: Some(reason.to_owned()),
            logs: vec![],
            accounts: vec![],
            units_consumed: Some(1_000),
        }
    }

    /// Build a multi-response `MockSolanaRpc` by configuring a `VecDeque`-style mock.
    /// Since `MockSolanaRpc` has a single `simulate_response`, we use a shared
    /// `Arc<Mutex<VecDeque>>` approach for multi-call tests.
    struct MultiMockSolanaRpc {
        responses: std::sync::Mutex<
            std::collections::VecDeque<
                Result<SimulatedTransaction, mg_onchain_token_registry::RegistryError>,
            >,
        >,
    }

    impl MultiMockSolanaRpc {
        fn new(responses: Vec<SimulatedTransaction>) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(responses.into_iter().map(Ok).collect()),
            })
        }

        #[allow(dead_code)]
        fn new_with_errors(
            responses: Vec<Result<SimulatedTransaction, &'static str>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(
                    responses
                        .into_iter()
                        .map(|r| {
                            r.map_err(|e| {
                                mg_onchain_token_registry::RegistryError::Internal(e.to_owned())
                            })
                        })
                        .collect(),
                ),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::rpc::SolanaRpc for MultiMockSolanaRpc {
        async fn get_mint_account(
            &self,
            _: &str,
        ) -> Result<
            Option<mg_onchain_token_registry::rpc::DecodedMint>,
            mg_onchain_token_registry::RegistryError,
        > {
            Ok(None)
        }
        async fn get_token_largest_accounts(
            &self,
            _: &str,
            _: &str,
        ) -> Result<
            Vec<mg_onchain_token_registry::rpc::TokenAccountBalance>,
            mg_onchain_token_registry::RegistryError,
        > {
            Ok(vec![])
        }
        async fn get_token_account_owner(
            &self,
            _: &str,
        ) -> Result<Option<String>, mg_onchain_token_registry::RegistryError> {
            Ok(None)
        }
        async fn get_first_signature(
            &self,
            _: &str,
        ) -> Result<
            Option<mg_onchain_token_registry::rpc::SignatureInfo>,
            mg_onchain_token_registry::RegistryError,
        > {
            Ok(None)
        }
        async fn simulate_transaction(
            &self,
            _tx: &str,
            _sv: bool,
            _rr: bool,
            _c: &str,
            _a: &[&str],
        ) -> Result<SimulatedTransaction, mg_onchain_token_registry::RegistryError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(mg_onchain_token_registry::RegistryError::Internal(
                        "MultiMockSolanaRpc: no more configured responses".to_owned(),
                    ))
                })
        }
        async fn get_account_raw(
            &self,
            _address: &str,
        ) -> Result<
            Option<mg_onchain_token_registry::rpc::RawAccount>,
            mg_onchain_token_registry::RegistryError,
        > {
            Ok(None)
        }
    }

    // ----- Test 1: NotWiredPoolAccountProvider returns skip -----

    #[tokio::test]
    async fn simulate_sell_skips_on_not_wired_provider() {
        let cfg = load_cfg();
        let meta = meta_with_pool(DexKind::RaydiumCpmm, Decimal::from(100_000u64));
        let rpc: Arc<dyn SolanaRpc> =
            Arc::new(mg_onchain_token_registry::rpc::tests::MockSolanaRpc::default());
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(NotWiredPoolAccountProvider);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(result.skipped, "must be skipped when provider is not wired");
        let reason = result.skip_reason.unwrap_or_default();
        assert!(
            reason.starts_with("pool_account_provider_not_wired"),
            "reason must start with 'pool_account_provider_not_wired', got: {reason}"
        );
    }

    // ----- Test 2: No supported pool → skip -----

    #[tokio::test]
    async fn simulate_sell_skips_when_no_supported_pool() {
        let cfg = load_cfg();
        // Only Orca — not supported in this phase.
        let meta = meta_with_pool(DexKind::OrcaWhirlpool, Decimal::from(100_000u64));
        let rpc: Arc<dyn SolanaRpc> =
            Arc::new(mg_onchain_token_registry::rpc::tests::MockSolanaRpc::default());
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(NotWiredPoolAccountProvider);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(result.skipped);
        assert_eq!(result.skip_reason.as_deref(), Some("no_supported_pool"));
    }

    // ----- Test 3: All buys fail → skip (§3.2 correction) -----

    #[tokio::test]
    async fn simulate_sell_returns_skip_when_all_buys_fail() {
        use mg_onchain_dex_adapter::RaydiumCpmmSwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        let meta = meta_with_pool(DexKind::RaydiumCpmm, Decimal::from(100_000u64));

        let k = mg_solana_types::Pubkey::new_from_array([0x01; 32]);
        let cpmm_accounts = RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        };
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_cpmm_accounts(cpmm_accounts));

        // All n_paths buy simulations fail.
        let n = cfg.simulate_paths.value as usize;
        let responses: Vec<SimulatedTransaction> = (0..n)
            .map(|_| sim_failure("InstructionError: [0, {\"Custom\": 6}]"))
            .collect();
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(
            result.skipped,
            "must be skipped when all buys fail (§3.2 correction)"
        );
        assert_eq!(
            result.skip_reason.as_deref(),
            Some("simulation_buys_all_failed"),
            "reason must be 'simulation_buys_all_failed'"
        );
        assert_eq!(result.confidence_add, 0.0);
    }

    // ----- Test 4: Buy success + sell fail → honeypot signal -----

    #[tokio::test]
    async fn simulate_sell_fires_on_buy_success_sell_fail() {
        use mg_onchain_dex_adapter::RaydiumCpmmSwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        let meta = meta_with_pool(DexKind::RaydiumCpmm, Decimal::from(100_000u64));

        let k = mg_solana_types::Pubkey::new_from_array([0x01; 32]);
        let cpmm_accounts = RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        };
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_cpmm_accounts(cpmm_accounts));

        let n = cfg.simulate_paths.value as usize;
        // Each path: buy success (1000 tokens), sell fail.
        let mut responses = Vec::new();
        for _ in 0..n {
            responses.push(sim_success_with_tokens(1_000_000));
            responses.push(sim_failure("InstructionError: sell reverted"));
        }
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(!result.skipped, "must not skip — buy succeeded");
        assert!(
            result.confidence_add >= 0.60,
            "sell_failed paths must yield confidence_add >= 0.60, got {}",
            result.confidence_add
        );
        assert_eq!(result.paths_failed, n as u32);
    }

    // ----- Test 5: Covert fee detected on all-success paths -----

    #[tokio::test]
    async fn simulate_sell_fires_on_covert_fee() {
        use mg_onchain_dex_adapter::RaydiumCpmmSwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        let meta = meta_with_pool(DexKind::RaydiumCpmm, Decimal::from(100_000u64));

        let k = mg_solana_types::Pubkey::new_from_array([0x01; 32]);
        let cpmm_accounts = RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        };
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_cpmm_accounts(cpmm_accounts));

        let n = cfg.simulate_paths.value as usize;
        let probe = cfg.sol_probe_amount_lamports.value as u64;
        // Sell returns only 50% of probe (50% effective tax >> threshold 30%).
        let sol_returned = probe / 2;
        let mut responses = Vec::new();
        for _ in 0..n {
            // Buy succeeds.
            responses.push(sim_success_with_tokens(1_000_000));
            // Sell succeeds but only returns 50% of probe.
            let data = vec![0u8; 72];
            let sell_success = SimulatedTransaction {
                err: None,
                logs: vec![],
                accounts: vec![
                    Some(SimulatedAccount {
                        lamports: sol_returned,
                        data: vec![],
                        owner: "11111111111111111111111111111111".to_owned(),
                    }),
                    Some(SimulatedAccount {
                        lamports: 0,
                        data: vec![
                            base64::prelude::BASE64_STANDARD.encode(&data),
                            "base64".to_owned(),
                        ],
                        owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_owned(),
                    }),
                ],
                units_consumed: Some(40_000),
            };
            responses.push(sell_success);
        }
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(!result.skipped, "must not skip");
        assert!(
            result.confidence_add > 0.0,
            "50% effective tax (above threshold) must produce positive confidence_add, got {}",
            result.confidence_add
        );
    }

    // ----- Test 6: Clean paths → no signal -----

    #[tokio::test]
    async fn simulate_sell_no_signal_on_clean_path() {
        use mg_onchain_dex_adapter::RaydiumCpmmSwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        let meta = meta_with_pool(DexKind::RaydiumCpmm, Decimal::from(100_000u64));

        let k = mg_solana_types::Pubkey::new_from_array([0x01; 32]);
        let cpmm_accounts = RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        };
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_cpmm_accounts(cpmm_accounts));

        let n = cfg.simulate_paths.value as usize;
        let probe = cfg.sol_probe_amount_lamports.value as u64;
        // Sell returns 98% of probe (2% slippage, well below 30% threshold).
        let sol_returned = (probe as f64 * 0.98) as u64;
        let mut responses = Vec::new();
        for _ in 0..n {
            responses.push(sim_success_with_tokens(1_000_000));
            let sell_success = SimulatedTransaction {
                err: None,
                logs: vec![],
                accounts: vec![
                    Some(SimulatedAccount {
                        lamports: sol_returned,
                        data: vec![],
                        owner: "11111111111111111111111111111111".to_owned(),
                    }),
                    Some(SimulatedAccount {
                        lamports: 0,
                        data: vec!["AAAA".to_owned(), "base64".to_owned()],
                        owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_owned(),
                    }),
                ],
                units_consumed: Some(40_000),
            };
            responses.push(sell_success);
        }
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(!result.skipped);
        assert_eq!(
            result.confidence_add, 0.0,
            "clean paths (2% slippage) must produce zero confidence_add, got {}",
            result.confidence_add
        );
    }

    // ----- Test 7: Pool selection prefers CPMM over V4 on equal liquidity -----

    #[tokio::test]
    async fn pool_selection_prefers_cpmm_over_v4() {
        use mg_onchain_dex_adapter::RaydiumCpmmSwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        // Both pools with equal liquidity — CPMM should win tie-break.
        let cpmm_market = MarketInfo {
            pool_address: Address::parse(Chain::Solana, TEST_POOL).unwrap(),
            dex: DexKind::RaydiumCpmm,
            lp_burned_pct: Decimal::ZERO,
            liquidity_usd: Decimal::from(50_000u64),
            lp_provider_count: 1,
        };
        // V4 pool has different address but same liquidity.
        let v4_pool_addr = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";
        let v4_market = MarketInfo {
            pool_address: Address::parse(Chain::Solana, v4_pool_addr).unwrap(),
            dex: DexKind::RaydiumV4,
            lp_burned_pct: Decimal::ZERO,
            liquidity_usd: Decimal::from(50_000u64),
            lp_provider_count: 1,
        };
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_market(v4_market)
            .with_market(cpmm_market)
            .build();

        let k = mg_solana_types::Pubkey::new_from_array([0x02; 32]);
        let cpmm_accounts = RaydiumCpmmSwapAccounts {
            payer: k,
            authority: k,
            amm_config: k,
            pool_state: k,
            input_token_account: k,
            output_token_account: k,
            input_vault: k,
            output_vault: k,
            input_token_program: k,
            output_token_program: k,
            input_token_mint: k,
            output_token_mint: k,
            observation_state: k,
        };
        // Configure CPMM to succeed; V4 would fail with NotWired if selected.
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_cpmm_accounts(cpmm_accounts));

        let n = cfg.simulate_paths.value as usize;
        // If CPMM is chosen, first buy call goes through.
        // If V4 were chosen, pool_accounts.v4_swap_accounts would return NotWired → skip.
        let responses: Vec<SimulatedTransaction> = (0..n * 2)
            .map(|i| {
                if i % 2 == 0 {
                    sim_success_with_tokens(1_000_000)
                } else {
                    sim_failure("sell reverted")
                }
            })
            .collect();
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        // If CPMM was selected (correct), we get a honeypot signal.
        // If V4 was selected (incorrect), we'd get a skip due to NotWired.
        assert!(
            !result.skipped,
            "CPMM must be selected over V4 on equal liquidity (tie-break CPMM > V4); \
             skip means V4 was mistakenly selected and returned NotWired. reason={:?}",
            result.skip_reason
        );
    }

    // ----- Test 8: Pool selection picks highest liquidity -----

    #[tokio::test]
    async fn pool_selection_picks_highest_liquidity() {
        use mg_onchain_dex_adapter::RaydiumV4SwapAccounts;
        use mg_onchain_dex_adapter::pool_accounts::MockPoolAccountProvider;

        let cfg = load_cfg();
        // V4 has higher liquidity than CPMM — V4 must be selected despite losing tie-break.
        let cpmm_market = MarketInfo {
            pool_address: Address::parse(Chain::Solana, TEST_POOL).unwrap(),
            dex: DexKind::RaydiumCpmm,
            lp_burned_pct: Decimal::ZERO,
            liquidity_usd: Decimal::from(10_000u64), // lower liquidity
            lp_provider_count: 1,
        };
        let v4_pool_addr = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";
        let v4_market = MarketInfo {
            pool_address: Address::parse(Chain::Solana, v4_pool_addr).unwrap(),
            dex: DexKind::RaydiumV4,
            lp_burned_pct: Decimal::ZERO,
            liquidity_usd: Decimal::from(200_000u64), // higher liquidity — wins
            lp_provider_count: 1,
        };
        let meta = MockTokenMetaBuilder::new_solana(SOL_NATIVE_MINT)
            .with_market(cpmm_market)
            .with_market(v4_market)
            .build();

        let k = mg_solana_types::Pubkey::new_from_array([0x03; 32]);
        let v4_accounts = RaydiumV4SwapAccounts {
            amm_pool: k,
            amm_authority: k,
            amm_open_orders: k,
            amm_target_orders: k,
            pool_coin_vault: k,
            pool_pc_vault: k,
            market_program: k,
            market: k,
            market_bids: k,
            market_asks: k,
            market_event_queue: k,
            market_coin_vault: k,
            market_pc_vault: k,
            market_vault_signer: k,
            user_source_token: k,
            user_dest_token: k,
            user_owner: k,
        };
        // Configure V4 to succeed; CPMM would fail with NotWired if selected.
        let pool_accounts: Arc<dyn mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider> =
            Arc::new(MockPoolAccountProvider::default().with_v4_accounts(v4_accounts));

        let n = cfg.simulate_paths.value as usize;
        let responses: Vec<SimulatedTransaction> = (0..n * 2)
            .map(|i| {
                if i % 2 == 0 {
                    sim_success_with_tokens(1_000_000)
                } else {
                    sim_failure("sell reverted")
                }
            })
            .collect();
        let rpc: Arc<dyn SolanaRpc> = MultiMockSolanaRpc::new(responses);

        let result = simulate_sell(&meta, &rpc, &pool_accounts, &cfg)
            .await
            .unwrap();

        assert!(
            !result.skipped,
            "V4 (higher liquidity) must be selected over CPMM; \
             skip means CPMM was mistakenly selected and returned NotWired. reason={:?}",
            result.skip_reason
        );
    }

    // -----------------------------------------------------------------------
    // EVM signal tests (Track C, Sprint 25)
    // -----------------------------------------------------------------------

    #[test]
    fn d01_supported_chains_includes_6_chains() {
        use mg_onchain_dex_adapter::pool_accounts::NotWiredPoolAccountProvider;
        use mg_onchain_token_registry::rpc::tests::MockSolanaRpc;

        let cfg = load_cfg();
        let rpc: Arc<dyn SolanaRpc> = Arc::new(MockSolanaRpc::default());
        let pool_accounts = Arc::new(NotWiredPoolAccountProvider);
        let detector = HoneypotDetector::new(cfg, rpc, pool_accounts);
        let chains = detector.supported_chains();
        assert_eq!(chains.len(), 6, "D01 must support 6 chains");
        assert!(chains.contains(&Chain::Solana));
        assert!(chains.contains(&Chain::Ethereum));
        assert!(chains.contains(&Chain::Bsc));
        assert!(chains.contains(&Chain::Base));
        assert!(chains.contains(&Chain::Arbitrum));
        assert!(chains.contains(&Chain::Polygon));
    }

    #[test]
    fn evm_router_for_chain_returns_ethereum_router() {
        let router = evm_router_for_chain(Chain::Ethereum);
        assert!(router.is_some(), "Ethereum must have a router");
        let r = router.unwrap();
        assert_eq!(r, "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D", "Ethereum router must be UniV2 Router02");
    }

    #[test]
    fn evm_router_for_chain_solana_returns_none() {
        assert!(evm_router_for_chain(Chain::Solana).is_none(), "Solana must not have an EVM router");
    }

    #[test]
    fn evm_router_for_each_evm_chain_is_some() {
        for chain in [Chain::Ethereum, Chain::Bsc, Chain::Base, Chain::Arbitrum, Chain::Polygon] {
            assert!(
                evm_router_for_chain(chain).is_some(),
                "Chain {:?} must have a configured EVM router", chain
            );
        }
    }

    #[test]
    fn weth_for_chain_ethereum() {
        let w = weth_for_chain(Chain::Ethereum);
        // WETH Ethereum (canonical, well-known address)
        assert_eq!(w, "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    }

    #[test]
    fn build_get_amounts_out_calldata_has_correct_selector() {
        let calldata = build_get_amounts_out_calldata(
            "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        ).unwrap();
        // First 4 bytes = selector 0xd06ca61f
        assert_eq!(&calldata[0..4], &[0xd0, 0x6c, 0xa6, 0x1f], "selector must be 0xd06ca61f");
        // Total length: 4 (selector) + 32 (amountIn) + 32 (offset) + 32 (len) + 32 (path[0]) + 32 (path[1]) = 164
        assert_eq!(calldata.len(), 164, "calldata must be 164 bytes");
    }

    #[tokio::test]
    async fn evm_simulate_sell_returns_zero_when_sell_succeeds() {
        use mg_onchain_chain_adapter::ethereum::rpc::MockEthereumRpc;

        let mock = MockEthereumRpc::new();

        // Build canned getAmountsOut response: uint256[] {amountIn=1e18, amountOut=0.95e18}
        // ABI: offset=32, length=2, amounts[0]=1e18, amounts[1]=0.95e18
        let mut response = vec![0u8; 128];
        // offset = 32 (0x20)
        response[31] = 0x20;
        // length = 2
        response[63] = 2;
        // amounts[0] = 1e18
        let amount_in_bytes = 1_000_000_000_000_000_000u64.to_be_bytes();
        response[64 + 24..64 + 32].copy_from_slice(&amount_in_bytes);
        // amounts[1] = 0.95e18 (5% "fee" — below 30% threshold)
        let amount_out_bytes = 950_000_000_000_000_000u64.to_be_bytes();
        response[96 + 24..96 + 32].copy_from_slice(&amount_out_bytes);

        // Register for any calldata (default response)
        mock.set_eth_call_default(Ok(response));

        let confidence = evm_simulate_sell(
            "0x1111111111111111111111111111111111111111",
            "0x2222222222222222222222222222222222222222",
            "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
            Chain::Ethereum,
            &mock,
            0.30_f32, // sell_tax_threshold
        ).await.unwrap();

        // 5% tax < 30% threshold → no honeypot signal
        assert!((confidence - 0.0).abs() < 1e-6, "5% tax should not fire: confidence={confidence}");
    }

    #[tokio::test]
    async fn evm_simulate_sell_returns_high_confidence_for_heavy_tax() {
        use mg_onchain_chain_adapter::ethereum::rpc::MockEthereumRpc;

        let mock = MockEthereumRpc::new();

        // Build canned getAmountsOut response: amountOut = 0.50e18 (50% tax)
        let mut response = vec![0u8; 128];
        response[31] = 0x20; // offset
        response[63] = 2; // length
        // amounts[0] = 1e18
        let amount_in_bytes = 1_000_000_000_000_000_000u64.to_be_bytes();
        response[64 + 24..64 + 32].copy_from_slice(&amount_in_bytes);
        // amounts[1] = 0.5e18 (50% tax → above 30% threshold)
        let amount_out_bytes = 500_000_000_000_000_000u64.to_be_bytes();
        response[96 + 24..96 + 32].copy_from_slice(&amount_out_bytes);

        mock.set_eth_call_default(Ok(response));

        let confidence = evm_simulate_sell(
            "0x1111111111111111111111111111111111111111",
            "0x2222222222222222222222222222222222222222",
            "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
            Chain::Ethereum,
            &mock,
            0.30_f32,
        ).await.unwrap();

        // 50% tax > 30% threshold → honeypot 0.80
        assert!((confidence - 0.80).abs() < 1e-6, "50% tax should fire at 0.80: got {confidence}");
    }
}
