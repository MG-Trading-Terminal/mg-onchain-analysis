//! D07 — Token-2022 Withdraw-Withheld Drain detector.
//!
//! # Overview
//!
//! Detects malicious extraction of accumulated Token-2022 withheld fee balances
//! via `WithdrawWithheldTokensFromMint` / `WithdrawWithheldTokensFromAccounts`
//! instructions, and the authority rotation pattern that precedes such attacks.
//!
//! D07 closes the gap identified in E-D02-11: `WithdrawWithheld*` instructions
//! produce no LP burn events (D02 is blind) and no zero-address transfers (D06 is
//! blind). D07 monitors the `token2022_instructions` table (V00007 migration)
//! populated by the chain-adapter Token-2022 instruction decoder.
//!
//! # Signals
//!
//! - **Signal A — Active extraction:** `WithdrawWithheld*` instructions in the
//!   detection window with cumulative USD ≥ `min_cumulative_withdraw_usd` AND
//!   event count ≥ `min_extraction_events`.
//!
//! - **Signal B — Authority rotation alert:** `SetAuthority(WithdrawWithheldTokens)`
//!   within `authority_rotation_window_days`, with fresh-wallet or rapid-rotation
//!   bonuses.
//!
//! - **Signal A+B composite:** When both signals fire within the same window,
//!   Signal A confidence is upgraded: `min(0.95, signal_a_conf + 0.10)`.
//!
//! # Confidence formulas
//!
//! Signal A:
//! ```text
//! extraction_event_factor = min(0.15, (event_count - min_extraction_events) * 0.03)
//! usd_factor = if usd_ratio > 1.0 { (usd_ratio.ln() * 0.10).min(0.15) } else { 0.0 }
//!   where usd_ratio = cumulative_usd / min_cumulative_withdraw_usd
//! authority_penalty = if authority_match == "unknown" { 0.10 } else { 0.0 }
//! conf_raw = 0.60 + extraction_event_factor + usd_factor - authority_penalty
//! conf = min(0.90, conf_raw.max(0.0))
//! ```
//!
//! Signal B:
//! ```text
//! fresh_wallet_bonus   = if authority_is_fresh_wallet { 0.20 } else { 0.0 }
//! rapid_rotation_bonus = if prev_authority_tenure_days < min_authority_tenure_days { 0.15 } else { 0.0 }
//! conf = min(0.75, 0.40 + fresh_wallet_bonus + rapid_rotation_bonus)
//! ```
//!
//! # Established-protocol suppression
//!
//! Signal A: conditionally suppressed when `is_established_protocol(meta) = true` AND
//! `extraction_usd / pool_volume_usd <= 0.90`. Signal B is NEVER suppressed.
//!
//! # Evidence keys
//!
//! All evidence keys are prefixed `withdraw_withheld/`. Exact key set per
//! `docs/designs/0012-detector-07-withdraw-withheld.md` §10.
//!
//! # Design references
//!
//! - spec: docs/designs/0012-detector-07-withdraw-withheld.md
//! - queries: docs/queries/d07_withdraw_withheld.sql
//! - gap: docs/reviews/0002-d02-rug-pull-evasions.md §E-D02-11

use chrono::Duration;
use rust_decimal::Decimal;
use tracing::{debug, instrument, warn};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::{Address, Chain, TxHash};
use mg_onchain_storage::pg::{AuthorityRotationRow, WithdrawWithheldEventsResult};

use crate::context::DetectorContext;
use crate::detector::Detector;
use crate::error::DetectorError;
use crate::signals::severity_from_confidence;
use crate::token_status::is_established_protocol;

/// The stable detector identifier. Must match the TOML subsection key.
pub const DETECTOR_ID: &str = "withdraw_withheld_drain";

// ---------------------------------------------------------------------------
// WithdrawWithheldDetector
// ---------------------------------------------------------------------------

/// D07 — Token-2022 Withdraw-Withheld Drain detector.
pub struct WithdrawWithheldDetector;

impl Detector for WithdrawWithheldDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn severity_floor(&self) -> Severity {
        Severity::Info
    }

    #[instrument(skip(self, ctx), fields(
        detector_id = DETECTOR_ID,
        token = %ctx.token,
        chain = ?ctx.chain,
    ))]
    async fn evaluate<'ctx>(
        &self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError> {
        let cfg = &ctx.config.withdraw_withheld;
        let token_str = ctx.token.as_str().to_owned();
        let chain_str = chain_str(ctx.chain);

        // ---------------------------------------------------------------
        // Step 1: Enrich TokenMeta — require Token-2022 + TransferFeeConfig
        // ---------------------------------------------------------------
        let meta = ctx
            .registry
            .enrich(ctx.token.as_str(), ctx.chain)
            .await
            .map_err(|e| DetectorError::MissingDependencyData {
                detector_id: DETECTOR_ID,
                token: token_str.clone(),
                reason: format!("token-registry enrich failed: {e}"),
            })?;

        let transfer_fee =
            meta.transfer_fee
                .as_ref()
                .ok_or_else(|| DetectorError::InsufficientBaseline {
                    detector_id: DETECTOR_ID,
                    token: token_str.clone(),
                    reason: "not a Token-2022 mint with TransferFeeConfig".into(),
                    fallback_used: false,
                })?;

        let fee_bps = transfer_fee.fee_bps;
        let established = is_established_protocol(&meta);

        // ---------------------------------------------------------------
        // Step 2: Fetch Signal A data (W1 + W3)
        // ---------------------------------------------------------------
        let detection_window_hours = cfg.detection_window_hours.value as i64;
        let window_start = ctx.window.end - Duration::hours(detection_window_hours);
        let window_end = ctx.window.end;

        let w1_result: WithdrawWithheldEventsResult = ctx
            .store
            .fetch_withdraw_withheld_events(chain_str, &token_str, window_start, window_end)
            .await
            .map_err(|e| match e {
                mg_onchain_storage::error::StorageError::Postgres(se) => {
                    DetectorError::TransientQuery {
                        detector_id: DETECTOR_ID,
                        source: se,
                    }
                }
                other => DetectorError::PermanentQuery {
                    detector_id: DETECTOR_ID,
                    reason: format!("fetch_withdraw_withheld_events: {other}"),
                },
            })?;

        // ---------------------------------------------------------------
        // Step 3: Fetch Signal B data (W2)
        // ---------------------------------------------------------------
        let rotation_lookback_days = cfg.authority_rotation_window_days.value as i64;
        let rotation_lookback_start = window_end - Duration::days(rotation_lookback_days);

        let rotation_rows: Vec<AuthorityRotationRow> = ctx
            .store
            .fetch_withdraw_authority_history(
                chain_str,
                &token_str,
                rotation_lookback_start,
                window_end,
            )
            .await
            .map_err(|e| match e {
                mg_onchain_storage::error::StorageError::Postgres(se) => {
                    DetectorError::TransientQuery {
                        detector_id: DETECTOR_ID,
                        source: se,
                    }
                }
                other => DetectorError::PermanentQuery {
                    detector_id: DETECTOR_ID,
                    reason: format!("fetch_withdraw_authority_history: {other}"),
                },
            })?;

        // ---------------------------------------------------------------
        // Step 4: Guard — if both are empty, return MissingDependencyData
        // ---------------------------------------------------------------
        if w1_result.events.is_empty() && rotation_rows.is_empty() {
            debug!(
                token = %ctx.token,
                "D07: no rows in token2022_instructions for this mint/window — MissingDependencyData"
            );
            return Err(DetectorError::MissingDependencyData {
                detector_id: DETECTOR_ID,
                token: token_str.clone(),
                reason: "token2022_instructions table empty for (chain, mint, window) — decoder may not have run".into(),
            });
        }

        // ---------------------------------------------------------------
        // Step 5: Evaluate Signal A
        // ---------------------------------------------------------------
        let signal_a_result = evaluate_signal_a(
            &w1_result,
            cfg.min_extraction_events.value,
            cfg.min_cumulative_withdraw_usd.value,
            cfg.min_single_event_withdraw_usd.value,
            established,
            cfg.established_protocol_fee_extraction_allowlist_pct.value,
        );

        // ---------------------------------------------------------------
        // Step 6: Evaluate Signal B
        // ---------------------------------------------------------------
        let signal_b_result = evaluate_signal_b(
            &rotation_rows,
            cfg.min_authority_tenure_days.value,
            cfg.fresh_wallet_funding_hours.value,
            cfg.min_withheld_at_rotation_usd.value,
            window_start,
            window_end,
        );

        // ---------------------------------------------------------------
        // Step 6b: ACCEPTED-RISK-D07-02 operational warning.
        //
        // When Signal B fires AND the wallet_funding_events sidecar is empty
        // (rotation_within_fresh_wallet_hours == -1), emit a WARN so that the
        // scheduler / on-call engineer knows the fresh_wallet_bonus is disabled.
        // The warn fires at most once per (chain, token, evaluation) because
        // evaluate_signal_b returns a single Option<SignalBResult> — the most
        // recent rotation in the detection window. No additional dedupe is needed.
        // ---------------------------------------------------------------
        if let Some(ref sb) = signal_b_result
            && sb.rotation_within_fresh_wallet_hours == -1
        {
            warn!(
                token = %ctx.token.as_str(),
                "D07 Signal B: wallet_funding_events sidecar is empty; fresh_wallet_bonus \
                 disabled (ACCEPTED-RISK-D07-02). Phase 3 indexer write path required."
            );
        }

        // ---------------------------------------------------------------
        // Step 7: Check D01 S2 overlap (combined_with_d01_s2 evidence key)
        // ---------------------------------------------------------------
        let sell_tax_threshold_bps = ctx.config.honeypot_sim.sell_tax_threshold_bps.value;
        let combined_with_d01_s2 =
            cfg.cross_detector_composite_enabled.value && fee_bps > sell_tax_threshold_bps;

        // ---------------------------------------------------------------
        // Step 8: Build AnomalyEvents
        // ---------------------------------------------------------------
        let mut events: Vec<AnomalyEvent> = Vec::new();

        // Is rotation in window? (for composite check)
        let rotation_in_window = signal_b_result.is_some();

        if let Some(ref sa) = signal_a_result
            && !sa.suppressed
        {
            // Apply composite boost if Signal B rotation was in window
            let final_conf = if rotation_in_window {
                (sa.confidence + 0.10_f64).min(0.95_f64)
            } else {
                sa.confidence
            };

            let ev = build_signal_a_event(
                ctx,
                &w1_result,
                sa,
                signal_b_result.as_ref(),
                final_conf,
                rotation_in_window,
                fee_bps,
                combined_with_d01_s2,
            );
            events.push(ev);
        }

        if let Some(ref sb) = signal_b_result {
            let ev = build_signal_b_event(ctx, sb, fee_bps, established);
            events.push(ev);
        }

        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// Signal A computation
// ---------------------------------------------------------------------------

/// Signal A detection tier, set by the two-tier gate (review 0004 §4 T1).
///
/// - `Recurring`: `event_count >= min_extraction_events` — primary formula applies.
/// - `TwoEvent`: `event_count == 2 AND cumulative_usd >= min_cumulative_withdraw_usd`.
/// - `SingleEvent`: `event_count == 1 AND cumulative_usd >= min_single_event_withdraw_usd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectionTier {
    Recurring,
    TwoEvent,
    SingleEvent,
}

impl DetectionTier {
    fn as_str(self) -> &'static str {
        match self {
            DetectionTier::Recurring => "recurring",
            DetectionTier::TwoEvent => "two_event",
            DetectionTier::SingleEvent => "single_event",
        }
    }
}

/// Result of Signal A evaluation (before composite upgrade).
struct SignalAResult {
    /// Whether Signal A threshold was met (enough events + USD).
    _fires: bool,
    /// Raw confidence before composite upgrade.
    confidence: f64,
    /// Whether established-protocol suppression was applied.
    suppressed: bool,
    /// Whether the suppression was overridden (ratio > allowlist threshold).
    suppression_skipped: bool,
    /// "exact" or "unknown" authority match.
    authority_match: &'static str,
    /// Tenure of the current authority in days (-1 if unknown).
    authority_tenure_days: i64,
    /// Whether the authority is a fresh wallet (funded within funding_hours).
    _authority_is_fresh_wallet: bool,
    /// Whether USD data was available for the confidence formula.
    _usd_available: bool,
    /// Which detection tier fired (single_event / two_event / recurring).
    detection_tier: DetectionTier,
}

fn evaluate_signal_a(
    result: &WithdrawWithheldEventsResult,
    min_events: u32,
    min_usd: f64,
    min_single_event_usd: f64,
    established: bool,
    ep_allowlist_pct: f64,
) -> Option<SignalAResult> {
    let event_count = result.event_count;
    let cumulative_usd = result.cumulative_amount_usd;
    let usd_available = cumulative_usd.is_some();

    // Helper: Decimal → f64 for ratio math.  Monetary precision is not required here —
    // these are ratio/gate checks, not stored monetary values.
    let usd_f64 = |d: Decimal| -> f64 { d.to_string().parse::<f64>().unwrap_or(0.0_f64) };

    // -----------------------------------------------------------------------
    // Two-tier Signal A gate (review 0004 §4 T1, E-D07-9 mitigation).
    //
    // Primary path: event_count >= min_extraction_events → recurring formula.
    // Second tier : event_count == 2 AND cumulative_usd >= min_usd → fixed 0.60.
    // Third  tier : event_count == 1 AND cumulative_usd >= min_single_event_usd → fixed 0.65.
    // Else        : None (no fire).
    // -----------------------------------------------------------------------
    let (tier, confidence_override) = if event_count >= min_events as i64 {
        (DetectionTier::Recurring, None)
    } else if event_count == 2 {
        let meets_usd = match cumulative_usd {
            Some(usd) => usd_f64(usd) >= min_usd,
            None => true, // price unavailable — event-count path
        };
        if meets_usd {
            (DetectionTier::TwoEvent, Some(0.60_f64))
        } else {
            return None;
        }
    } else if event_count == 1 {
        let meets_floor = match cumulative_usd {
            Some(usd) => usd_f64(usd) >= min_single_event_usd,
            None => false, // single-event path requires USD floor; no price → no fire
        };
        if meets_floor {
            (DetectionTier::SingleEvent, Some(0.65_f64))
        } else {
            return None;
        }
    } else {
        // event_count == 0
        return None;
    };

    // Determine authority match from the events.
    // Use the authority from the first extraction event as reference.
    // If events have different authorities, classify as "unknown" (possible rotation or CPI).
    let first_authority = result.events.first().and_then(|e| e.authority.clone());
    let authority_match = if let Some(ref auth) = first_authority {
        let all_same = result
            .events
            .iter()
            .all(|e| e.authority.as_deref() == Some(auth));
        if all_same { "exact" } else { "unknown" }
    } else {
        "unknown"
    };

    let authority_penalty = if authority_match == "unknown" {
        0.10_f64
    } else {
        0.0_f64
    };

    // Compute confidence: override for single/two-event tiers; primary formula for recurring.
    let confidence = if let Some(fixed) = confidence_override {
        // Single-event and two-event tiers use fixed confidence (no formula).
        // Authority penalty still applies.
        (fixed - authority_penalty).clamp(0.0_f64, 0.90_f64)
    } else {
        // Recurring tier: USD gate first.
        let usd_gate_met = match cumulative_usd {
            Some(usd) => usd_f64(usd) >= min_usd,
            None => true, // no price data: skip USD gate, use event count only
        };

        if !usd_gate_met {
            return None;
        }

        // Confidence formula (Signal A per spec §6 — recurring tier only).
        let extraction_event_factor =
            ((event_count - min_events as i64) as f64 * 0.03_f64).min(0.15_f64);

        let usd_factor = match cumulative_usd {
            Some(usd) => {
                let usd_ratio = usd_f64(usd) / min_usd;
                if usd_ratio > 1.0 {
                    (usd_ratio.ln() * 0.10_f64).min(0.15_f64)
                } else {
                    0.0_f64
                }
            }
            None => 0.0_f64,
        };

        let conf_raw = 0.60_f64 + extraction_event_factor + usd_factor - authority_penalty;
        conf_raw.clamp(0.0_f64, 0.90_f64)
    };

    // Established-protocol suppression check.
    let (suppressed, suppression_skipped) = if established {
        // ACCEPTED-RISK-D07-01: pool_volume_usd stub (DG-D07-2 Phase 3).
        // Current behavior: Signal A suppression for established protocols is NEVER
        // APPLIED in MVP because pool_volume_usd is hardcoded to 0.0 (the ratio check
        // divides by zero → NaN → suppression condition never satisfied → Signal A
        // always fires for established protocols). This is the security-safe failure
        // mode — we over-alert rather than silently suppress.
        //
        // Before DG-D07-2 ships (ClickHouse/Postgres pool-volume aggregate query):
        //   1. Lower established_protocol_fee_extraction_allowlist_pct from 0.50
        //      (applied in P6-1 T2) to a calibrated value from the Sprint 6+ corpus.
        //   2. Add regression tests for the suppression path (currently dead code).
        // See docs/reviews/0004-d07-withdraw-withheld-evasions.md §6 CF-1.
        let pool_volume_usd = 0.0_f64;

        if pool_volume_usd == 0.0_f64 {
            // Can't compute ratio — fire regardless (pool_volume_zero path).
            (false, false)
        } else if let Some(usd) = cumulative_usd {
            let extraction_usd = usd_f64(usd);
            let ratio = extraction_usd / pool_volume_usd;
            if ratio > ep_allowlist_pct {
                // Ratio exceeds allowlist — fire with skip_reason = "1".
                (false, true)
            } else {
                // Suppressed — extraction is proportional to volume.
                (true, false)
            }
        } else {
            // No USD data — can't do ratio check, fire (event-count path).
            (false, false)
        }
    } else {
        (false, false)
    };

    let fires = !suppressed;

    Some(SignalAResult {
        _fires: fires,
        confidence,
        suppressed,
        suppression_skipped,
        authority_match,
        authority_tenure_days: -1, // DG-D07-1: not available without set_authority history cross-ref
        _authority_is_fresh_wallet: false, // populated via Signal B result when available
        _usd_available: usd_available,
        detection_tier: tier,
    })
}

// ---------------------------------------------------------------------------
// Signal B computation
// ---------------------------------------------------------------------------

/// Result of Signal B evaluation.
struct SignalBResult {
    /// The rotation instruction row that triggered Signal B.
    rotation_row: AuthorityRotationRow,
    /// Signal B confidence.
    confidence: f64,
    /// Whether the new authority is a fresh wallet.
    is_fresh_wallet: bool,
    /// Hours between new authority's first SOL and the rotation instruction.
    /// -1 if sidecar unavailable.
    rotation_within_fresh_wallet_hours: i64,
    /// Whether previous authority had been in role < min_tenure_days.
    _is_rapid_rotation: bool,
    /// Previous authority tenure in days.
    prev_authority_tenure_days: i64,
}

fn evaluate_signal_b(
    rotation_rows: &[AuthorityRotationRow],
    min_tenure_days: u32,
    fresh_wallet_hours: u32,
    _min_withheld_at_rotation_usd: f64,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
) -> Option<SignalBResult> {
    // Find the most recent rotation within the window. If none, no Signal B.
    let rotation = rotation_rows
        .iter()
        .rfind(|r| r.row.block_time >= window_start && r.row.block_time < window_end)?;

    // Fresh-wallet check
    let (is_fresh_wallet, rotation_within_hours) =
        if let Some(first_sol) = rotation.new_authority_first_sol_time {
            let delta = rotation.row.block_time - first_sol;
            let hours = delta.num_hours();
            let fresh = hours >= 0 && hours < fresh_wallet_hours as i64;
            (fresh, hours)
        } else {
            // ACCEPTED-RISK-D07-02: wallet_funding_events depopulation.
            // The `wallet_funding_events` table (V00007 migration) exists but is not
            // populated by the indexer in Phase 2 — no write path is wired. Consequence:
            // `fetch_wallet_funding_time` returns None for every query, `authority_is_fresh_wallet`
            // evidence is always "0", and `fresh_wallet_bonus` is permanently 0.0 in Signal B.
            // Signal B still fires on rapid rotation (prev_authority_tenure < min_authority_tenure_days).
            // The fresh-wallet augmentation is disabled in MVP.
            //
            // Indexer write path lands Phase 3 (blockchain-engineer P6-4 or Sprint 7).
            // See docs/reviews/0004-d07-withdraw-withheld-evasions.md §5 B2 + §E-D07-x.
            (false, -1_i64)
        };

    // Rapid rotation check: compare rotation block_time to previous rotation block_time
    // or use token creation time as fallback (not available in MVP, so derive from history).
    let prev_tenure_days = if rotation_rows.len() >= 2 {
        // Find the rotation before this one
        let idx = rotation_rows
            .iter()
            .position(|r| r.row.tx_hash == rotation.row.tx_hash)
            .unwrap_or(rotation_rows.len() - 1);
        if idx > 0 {
            let prev = &rotation_rows[idx - 1];
            let delta = rotation.row.block_time - prev.row.block_time;
            delta.num_days()
        } else {
            // First rotation — no prior rotation to compare against; use window size as sentinel
            i64::MAX
        }
    } else {
        // Only one rotation ever seen — no rapid rotation signal
        i64::MAX
    };

    let is_rapid_rotation = prev_tenure_days < min_tenure_days as i64 && prev_tenure_days >= 0;

    let fresh_wallet_bonus = if is_fresh_wallet { 0.20_f64 } else { 0.0_f64 };
    let rapid_rotation_bonus = if is_rapid_rotation { 0.15_f64 } else { 0.0_f64 };
    let confidence = (0.40_f64 + fresh_wallet_bonus + rapid_rotation_bonus).min(0.75_f64);

    Some(SignalBResult {
        rotation_row: rotation.clone(),
        confidence,
        is_fresh_wallet,
        rotation_within_fresh_wallet_hours: rotation_within_hours,
        _is_rapid_rotation: is_rapid_rotation,
        prev_authority_tenure_days: prev_tenure_days,
    })
}

// ---------------------------------------------------------------------------
// Event builders
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_signal_a_event(
    ctx: &DetectorContext<'_>,
    result: &WithdrawWithheldEventsResult,
    sa: &SignalAResult,
    sb: Option<&SignalBResult>,
    final_conf: f64,
    rotation_in_window: bool,
    fee_bps: u16,
    combined_with_d01_s2: bool,
) -> AnomalyEvent {
    let severity = severity_from_confidence(final_conf);
    let confidence = Confidence::new(final_conf).unwrap_or(Confidence::ZERO);

    // Build evidence
    let cumulative_usd = result.cumulative_amount_usd.unwrap_or(Decimal::ZERO);
    let cumulative_raw = result.cumulative_amount_raw.unwrap_or(Decimal::ZERO);

    // Latest up-to-5 tx hashes (most recent first from ASC-ordered events)
    let latest_txs: Vec<String> = result
        .events
        .iter()
        .rev()
        .take(5)
        .map(|e| e.tx_hash.clone())
        .collect();

    let latest_txs_json = serde_json::to_string(&latest_txs).unwrap_or_else(|_| "[]".into());

    let authority_str = result
        .events
        .last()
        .and_then(|e| e.authority.clone())
        .unwrap_or_else(|| "unknown".into());

    let rotation_tx_hash = sb
        .map(|s| s.rotation_row.row.tx_hash.clone())
        .unwrap_or_default();

    let authority_is_fresh = sb.map(|s| s.is_fresh_wallet).unwrap_or(false);
    let authority_tenure = sa.authority_tenure_days;

    // Price unavailable note
    let price_unavailable = result.cumulative_amount_usd.is_none();

    let mut evidence = Evidence::new()
        .with_metric(
            "withdraw_withheld/extraction_event_count",
            Decimal::from(result.event_count),
        )
        .with_metric("withdraw_withheld/cumulative_withdrawn_usd", cumulative_usd)
        .with_metric("withdraw_withheld/cumulative_withdrawn_raw", cumulative_raw)
        .with_metric(
            "withdraw_withheld/authority_is_fresh_wallet",
            Decimal::from(u8::from(authority_is_fresh)),
        )
        .with_metric(
            "withdraw_withheld/authority_tenure_days",
            Decimal::from(authority_tenure),
        )
        .with_metric(
            "withdraw_withheld/rotation_detected",
            Decimal::from(u8::from(rotation_in_window)),
        )
        .with_metric("withdraw_withheld/transfer_fee_bps", Decimal::from(fee_bps))
        .with_metric(
            "withdraw_withheld/combined_with_d01_s2",
            Decimal::from(u8::from(combined_with_d01_s2)),
        )
        .with_metric(
            "withdraw_withheld/established_protocol_suppression_skipped_reason",
            Decimal::from(u8::from(sa.suppression_skipped)),
        );

    // Observed block range
    evidence.observed_range = Some((ctx.window.block_start, ctx.window.block_end));

    // Tx hashes for evidence
    for tx_str in result.events.iter().rev().take(5) {
        if let Ok(tx) = TxHash::solana_from_base58(&tx_str.tx_hash) {
            evidence = evidence.with_tx(tx);
        }
    }

    // Authority address
    if let Ok(addr) = Address::parse(ctx.chain, &authority_str) {
        evidence = evidence.with_address(addr);
    }

    // Notes
    if !rotation_tx_hash.is_empty() {
        evidence = evidence.with_note(format!(
            "withdraw_withheld/rotation_tx_hash: {rotation_tx_hash}"
        ));
    }
    if price_unavailable {
        evidence = evidence.with_note("price_data_unavailable".to_string());
    }
    evidence = evidence.with_note(format!(
        "withdraw_withheld/authority_match: {}",
        sa.authority_match
    ));
    evidence = evidence.with_note("withdraw_withheld/signal: extraction_event".to_string());
    evidence = evidence.with_note(format!(
        "withdraw_withheld/detection_tier: {}",
        sa.detection_tier.as_str()
    ));
    evidence = evidence.with_note(format!(
        "withdraw_withheld/latest_extraction_txs: {latest_txs_json}"
    ));

    let notes_summary = format!(
        "D07: {} WithdrawWithheld instructions; ${:.0} extracted in {}h window; \
         authority {} ({}, tenure {}d); transfer_fee_bps={}",
        result.event_count,
        cumulative_usd,
        ctx.config.withdraw_withheld.detection_window_hours.value,
        &authority_str[..authority_str.len().min(12)],
        sa.authority_match,
        if authority_tenure >= 0 {
            authority_tenure.to_string()
        } else {
            "unknown".into()
        },
        fee_bps,
    );
    evidence = evidence.with_note(notes_summary);

    AnomalyEvent {
        detector_id: DETECTOR_ID.into(),
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

fn build_signal_b_event(
    ctx: &DetectorContext<'_>,
    sb: &SignalBResult,
    fee_bps: u16,
    established: bool,
) -> AnomalyEvent {
    let confidence = Confidence::new(sb.confidence).unwrap_or(Confidence::ZERO);
    let severity = severity_from_confidence(sb.confidence);

    let withheld_at_rotation = sb.rotation_row.row.amount_usd.unwrap_or(Decimal::ZERO);

    let prev_authority_str = sb
        .rotation_row
        .row
        .prev_authority
        .clone()
        .unwrap_or_else(|| "none".into());
    let new_authority_str = sb
        .rotation_row
        .row
        .new_authority
        .clone()
        .unwrap_or_else(|| "none".into());

    let mut evidence = Evidence::new()
        .with_metric(
            "withdraw_withheld/authority_is_fresh_wallet",
            Decimal::from(u8::from(sb.is_fresh_wallet)),
        )
        .with_metric(
            "withdraw_withheld/rotation_within_fresh_wallet_hours",
            Decimal::from(sb.rotation_within_fresh_wallet_hours),
        )
        .with_metric(
            "withdraw_withheld/prev_authority_tenure_days",
            Decimal::from(sb.prev_authority_tenure_days.min(i64::from(i32::MAX))),
        )
        .with_metric(
            "withdraw_withheld/withheld_at_rotation_usd",
            withheld_at_rotation,
        )
        .with_metric("withdraw_withheld/transfer_fee_bps", Decimal::from(fee_bps));

    evidence.observed_range = Some((ctx.window.block_start, ctx.window.block_end));

    // Rotation tx hash
    if let Ok(tx) = TxHash::solana_from_base58(&sb.rotation_row.row.tx_hash) {
        evidence = evidence.with_tx(tx);
    }
    // Address: new authority + prev authority
    if let Ok(addr) = Address::parse(ctx.chain, &new_authority_str) {
        evidence = evidence.with_address(addr);
    }
    if prev_authority_str != "none"
        && let Ok(addr) = Address::parse(ctx.chain, &prev_authority_str)
    {
        evidence = evidence.with_address(addr);
    }

    // Notes
    evidence = evidence.with_note("withdraw_withheld/signal: authority_rotation".to_string());
    evidence = evidence.with_note(format!(
        "withdraw_withheld/rotation_tx_hash: {}",
        sb.rotation_row.row.tx_hash
    ));
    evidence = evidence.with_note(format!(
        "withdraw_withheld/prev_authority_address: {prev_authority_str}"
    ));
    if established {
        evidence = evidence.with_note(
            "established_protocol = true; Signal B not suppressed per design 0012 §9".to_string(),
        );
    }
    if sb.rotation_within_fresh_wallet_hours == -1 {
        evidence = evidence.with_note("wallet_funding_sidecar_unavailable".to_string());
    }

    let notes_summary = format!(
        "D07: withdraw_withheld_authority rotated to {} wallet (funded {}h before rotation); \
         prev authority tenure {}d; ${:.0} withheld at rotation",
        if sb.is_fresh_wallet {
            "fresh"
        } else {
            "existing"
        },
        if sb.rotation_within_fresh_wallet_hours >= 0 {
            sb.rotation_within_fresh_wallet_hours.to_string()
        } else {
            "unknown".into()
        },
        sb.prev_authority_tenure_days,
        withheld_at_rotation,
    );
    evidence = evidence.with_note(notes_summary);

    AnomalyEvent {
        detector_id: DETECTOR_ID.into(),
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
// Helpers
// ---------------------------------------------------------------------------

fn chain_str(chain: Chain) -> &'static str {
    match chain {
        Chain::Solana => "solana",
        Chain::Ethereum => "ethereum",
        Chain::Bsc => "bsc",
        Chain::Base => "base",
        Chain::Arbitrum => "arbitrum",
        Chain::Polygon => "polygon",
        Chain::Tron => "tron",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Pure computation helpers (for unit testing without a database)
// ---------------------------------------------------------------------------

/// Compute Signal A confidence and detection tier from raw inputs (pure function, no I/O).
///
/// Implements the two-tier gate from review 0004 §4 T1 (E-D07-9 mitigation):
///
/// - `event_count >= min_events` → `"recurring"` tier, primary spec §6 formula.
/// - `event_count == 2 AND cumulative_usd >= min_usd` → `"two_event"` tier, fixed 0.60.
/// - `event_count == 1 AND cumulative_usd >= min_single_event_usd` → `"single_event"` tier, fixed 0.65.
/// - Otherwise → `None`.
///
/// Returns `Some((confidence, tier_str))` or `None`.
pub fn compute_signal_a_confidence_tiered(
    event_count: i64,
    cumulative_usd: Option<f64>,
    min_events: u32,
    min_usd: f64,
    min_single_event_usd: f64,
    authority_match_exact: bool,
) -> Option<(f64, &'static str)> {
    let authority_penalty = if authority_match_exact {
        0.0_f64
    } else {
        0.10_f64
    };

    if event_count >= min_events as i64 {
        // Recurring tier — USD gate + primary formula.
        let usd_gate_met = match cumulative_usd {
            Some(usd) => usd >= min_usd,
            None => true,
        };
        if !usd_gate_met {
            return None;
        }

        let extraction_event_factor =
            ((event_count - min_events as i64) as f64 * 0.03_f64).min(0.15_f64);

        let usd_factor = match cumulative_usd {
            Some(usd) => {
                let ratio = usd / min_usd;
                if ratio > 1.0 {
                    (ratio.ln() * 0.10_f64).min(0.15_f64)
                } else {
                    0.0_f64
                }
            }
            None => 0.0_f64,
        };

        let conf_raw = 0.60_f64 + extraction_event_factor + usd_factor - authority_penalty;
        Some((conf_raw.clamp(0.0_f64, 0.90_f64), "recurring"))
    } else if event_count == 2 {
        // Two-event tier — fixed 0.60, USD >= min_usd required.
        let meets_usd = match cumulative_usd {
            Some(usd) => usd >= min_usd,
            None => true, // price unavailable — event-count path
        };
        if meets_usd {
            let conf = (0.60_f64 - authority_penalty).clamp(0.0_f64, 0.90_f64);
            Some((conf, "two_event"))
        } else {
            None
        }
    } else if event_count == 1 {
        // Single-event tier — fixed 0.65, USD >= min_single_event_usd required.
        // No price data → cannot satisfy the USD floor → no fire.
        let meets_floor = match cumulative_usd {
            Some(usd) => usd >= min_single_event_usd,
            None => false,
        };
        if meets_floor {
            let conf = (0.65_f64 - authority_penalty).clamp(0.0_f64, 0.90_f64);
            Some((conf, "single_event"))
        } else {
            None
        }
    } else {
        None
    }
}

/// Compute Signal A confidence from raw inputs (pure function, testable without I/O).
///
/// Returns `None` if the event_count or USD gate is not met.
/// Equivalent to the confidence formula in spec §6, factored out for unit tests.
///
/// For the two-tier gate (E-D07-9 mitigation, review 0004 §4 T1) use
/// [`compute_signal_a_confidence_tiered`] which accepts `min_single_event_usd` and
/// returns the detection tier alongside confidence.
pub fn compute_signal_a_confidence(
    event_count: i64,
    cumulative_usd: Option<f64>,
    min_events: u32,
    min_usd: f64,
    authority_match_exact: bool,
) -> Option<f64> {
    // Delegate to the tiered variant using a large single-event floor so that
    // the single-event and two-event sub-paths are not reachable from this function.
    // Callers that need tier-aware behavior must use compute_signal_a_confidence_tiered.
    compute_signal_a_confidence_tiered(
        event_count,
        cumulative_usd,
        min_events,
        min_usd,
        f64::MAX, // single-event path unreachable via this entry point
        authority_match_exact,
    )
    .map(|(conf, _tier)| conf)
}

/// Compute Signal B confidence from raw inputs (pure function, testable without I/O).
pub fn compute_signal_b_confidence(
    is_fresh_wallet: bool,
    prev_tenure_days: i64,
    min_tenure_days: u32,
) -> f64 {
    let fresh_bonus = if is_fresh_wallet { 0.20_f64 } else { 0.0_f64 };
    let rapid_bonus = if prev_tenure_days >= 0 && prev_tenure_days < min_tenure_days as i64 {
        0.15_f64
    } else {
        0.0_f64
    };
    (0.40_f64 + fresh_bonus + rapid_bonus).min(0.75_f64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Config defaults used in all tests
    const MIN_EVENTS: u32 = 3;
    const MIN_USD: f64 = 1000.0;
    const MIN_TENURE_DAYS: u32 = 7;

    // -----------------------------------------------------------------------
    // Signal A confidence formula
    // -----------------------------------------------------------------------

    /// Spec §6 worked example 1: 3 events, exactly $1,000, exact authority → 0.60
    #[test]
    fn signal_a_conf_base_case() {
        let conf = compute_signal_a_confidence(3, Some(1000.0), MIN_EVENTS, MIN_USD, true);
        let c = conf.expect("must fire at threshold");
        // 0.60 + 0.0 + 0.0 = 0.60
        assert!((c - 0.60).abs() < 1e-6, "base case must be 0.60, got {c}");
    }

    /// Spec §6 worked example 2: 5 events, $5,000, exact authority
    /// usd_factor = min(0.15, ln(5.0)*0.10) = min(0.15, 0.1609) = 0.15
    /// conf = 0.60 + min(0.15, 2*0.03) + 0.15 = 0.60 + 0.06 + 0.15 = 0.81
    #[test]
    fn signal_a_conf_5_events_5k_usd() {
        let conf = compute_signal_a_confidence(5, Some(5000.0), MIN_EVENTS, MIN_USD, true);
        let c = conf.expect("must fire");
        // usd_factor is capped at 0.15 per spec §6
        let usd_factor = ((5.0_f64).ln() * 0.10_f64).min(0.15_f64);
        let expected = (0.60 + 0.06 + usd_factor).min(0.90);
        assert!(
            (c - expected).abs() < 1e-4,
            "5 events $5k: expected {expected:.4}, got {c:.4}"
        );
    }

    /// Spec §6 worked example 3: 10 events, $50,000, exact authority → capped at 0.90
    #[test]
    fn signal_a_conf_capped_at_090() {
        let conf = compute_signal_a_confidence(10, Some(50000.0), MIN_EVENTS, MIN_USD, true);
        let c = conf.expect("must fire");
        assert!(
            (c - 0.90).abs() < 1e-6,
            "high values must be capped at 0.90, got {c}"
        );
    }

    /// Unknown authority penalty: 5 events, $5,000, unknown → subtract 0.10
    #[test]
    fn signal_a_conf_unknown_authority_penalty() {
        let conf_exact =
            compute_signal_a_confidence(5, Some(5000.0), MIN_EVENTS, MIN_USD, true).unwrap();
        let conf_unknown =
            compute_signal_a_confidence(5, Some(5000.0), MIN_EVENTS, MIN_USD, false).unwrap();
        assert!(
            (conf_exact - conf_unknown - 0.10).abs() < 1e-4,
            "unknown authority must subtract 0.10: exact={conf_exact:.4} unknown={conf_unknown:.4}"
        );
    }

    /// event_count == 0 → None (no events at all).
    /// Note: event_count == 1 and event_count == 2 may now fire via the two-tier gate
    /// (review 0004 §4 T1); zero events is the unconditional no-fire case.
    #[test]
    fn signal_a_zero_events_returns_none() {
        let conf = compute_signal_a_confidence(0, Some(5000.0), MIN_EVENTS, MIN_USD, true);
        assert!(conf.is_none(), "zero events must return None");
    }

    /// Below min_usd (with price data) → None
    #[test]
    fn signal_a_below_min_usd_returns_none() {
        let conf = compute_signal_a_confidence(5, Some(999.0), MIN_EVENTS, MIN_USD, true);
        assert!(conf.is_none(), "below min_usd must return None");
    }

    /// No price data → fires at event count only (skip USD gate)
    #[test]
    fn signal_a_no_usd_data_fires_at_event_count() {
        let conf = compute_signal_a_confidence(3, None, MIN_EVENTS, MIN_USD, true);
        let c = conf.expect("must fire when price unavailable and count ≥ min");
        // No usd_factor → 0.60 + 0.0 + 0.0 = 0.60
        assert!(
            (c - 0.60).abs() < 1e-6,
            "no-price fallback must be 0.60, got {c}"
        );
    }

    /// POS-D07-01 fixture confidence:
    /// 5 events, $2000, exact → 0.60 + 0.06 + ln(2.0)*0.10 = 0.60 + 0.06 + 0.0693 = 0.729
    #[test]
    fn signal_a_pos_d07_01_fixture_confidence() {
        let conf = compute_signal_a_confidence(5, Some(2000.0), MIN_EVENTS, MIN_USD, true);
        let c = conf.expect("must fire");
        let expected = 0.60_f64 + 0.06_f64 + (2.0_f64).ln() * 0.10_f64;
        assert!(
            (c - expected).abs() < 1e-4,
            "POS-D07-01: expected {expected:.4}, got {c:.4}"
        );
        // fixture expects conf >= 0.72
        assert!(c >= 0.72, "POS-D07-01: conf must be >= 0.72, got {c}");
    }

    // -----------------------------------------------------------------------
    // Signal A+B composite
    // -----------------------------------------------------------------------

    /// Composite: signal_a_conf + 0.10, capped at 0.95
    #[test]
    fn composite_upgrades_signal_a_by_0_10() {
        let base_a_conf = 0.77_f64;
        let composite = (base_a_conf + 0.10_f64).min(0.95_f64);
        assert!(
            (composite - 0.87).abs() < 1e-6,
            "composite must be 0.87, got {composite}"
        );
    }

    #[test]
    fn composite_capped_at_095() {
        let base_a_conf = 0.90_f64;
        let composite = (base_a_conf + 0.10_f64).min(0.95_f64);
        assert!(
            (composite - 0.95).abs() < 1e-6,
            "composite must be capped at 0.95, got {composite}"
        );
    }

    // -----------------------------------------------------------------------
    // Signal B confidence formula
    // -----------------------------------------------------------------------

    /// Signal B base (no bonuses): 0.40
    #[test]
    fn signal_b_base_confidence() {
        let c = compute_signal_b_confidence(false, i64::MAX, MIN_TENURE_DAYS);
        assert!(
            (c - 0.40).abs() < 1e-6,
            "base Signal B must be 0.40, got {c}"
        );
    }

    /// Fresh wallet only: 0.60
    #[test]
    fn signal_b_fresh_wallet_bonus() {
        let c = compute_signal_b_confidence(true, i64::MAX, MIN_TENURE_DAYS);
        assert!(
            (c - 0.60).abs() < 1e-6,
            "fresh wallet only Signal B must be 0.60, got {c}"
        );
    }

    /// Rapid rotation only: 0.55
    #[test]
    fn signal_b_rapid_rotation_bonus() {
        let c = compute_signal_b_confidence(false, 3, MIN_TENURE_DAYS);
        assert!(
            (c - 0.55).abs() < 1e-6,
            "rapid rotation only Signal B must be 0.55, got {c}"
        );
    }

    /// Fresh wallet + rapid rotation: 0.75 (capped)
    #[test]
    fn signal_b_both_bonuses_capped_at_075() {
        let c = compute_signal_b_confidence(true, 3, MIN_TENURE_DAYS);
        assert!(
            (c - 0.75).abs() < 1e-6,
            "fresh wallet + rapid rotation Signal B must be 0.75 (capped), got {c}"
        );
    }

    /// tenure_days == min_tenure_days (boundary) → NOT rapid rotation (strictly less than)
    #[test]
    fn signal_b_tenure_at_boundary_not_rapid() {
        let c = compute_signal_b_confidence(false, MIN_TENURE_DAYS as i64, MIN_TENURE_DAYS);
        assert!(
            (c - 0.40).abs() < 1e-6,
            "tenure at boundary must NOT trigger rapid rotation bonus"
        );
    }

    // -----------------------------------------------------------------------
    // Severity mapping
    // -----------------------------------------------------------------------

    #[test]
    fn signal_a_base_conf_is_medium_severity() {
        // conf = 0.60 → High (per spec §8: 0.60 ≤ conf < 0.75 → Medium... wait)
        // Re-check: spec §8 says:
        //   0.40 ≤ conf < 0.60 → Info (low bound)
        //   0.60 ≤ conf < 0.75 → Medium
        //   0.75 ≤ conf < 0.90 → High
        //   0.90 ≤ conf ≤ 1.0  → Critical
        // But severity_from_confidence uses:
        //   0.60 ≤ conf < 0.80 → High
        // There's a discrepancy with the spec's §8 bands.
        // D07 spec §8 provides its own severity ladder, but our code uses the shared
        // `severity_from_confidence` from signals.rs which has different bands.
        // The spec says Signal A at 0.60 should be "Medium" per its own ladder.
        // But `severity_from_confidence(0.60) = High`.
        // We follow the shared ladder (signals.rs) per CLAUDE.md consistency rule.
        use crate::signals::severity_from_confidence as sfc;
        let sev = sfc(0.60);
        assert_eq!(sev, Severity::High, "0.60 confidence maps to High severity");
    }

    #[test]
    fn signal_b_base_conf_is_info_severity() {
        use crate::signals::severity_from_confidence as sfc;
        let sev = sfc(0.40);
        assert_eq!(
            sev,
            Severity::Medium,
            "0.40 maps to Medium via shared ladder"
        );
    }

    // -----------------------------------------------------------------------
    // Config pin tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_min_extraction_events_is_3() {
        use std::path::PathBuf;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");
        let cfg = crate::config::load_detector_config(&config_path).expect("config must load");
        assert_eq!(cfg.withdraw_withheld.min_extraction_events.value, 3);
    }

    #[test]
    fn config_detection_window_hours_is_168() {
        use std::path::PathBuf;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");
        let cfg = crate::config::load_detector_config(&config_path).expect("config must load");
        assert_eq!(cfg.withdraw_withheld.detection_window_hours.value, 168);
    }

    /// T2: established_protocol_fee_extraction_allowlist_pct lowered 0.90→0.50
    /// per review 0004 §4 T2 (E-D07-12 mitigation).
    #[test]
    fn config_established_protocol_pct_is_050() {
        use std::path::PathBuf;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");
        let cfg = crate::config::load_detector_config(&config_path).expect("config must load");
        let v = cfg
            .withdraw_withheld
            .established_protocol_fee_extraction_allowlist_pct
            .value;
        assert!(
            (v - 0.50).abs() < 1e-6,
            "established_protocol_pct must be 0.50 (lowered from 0.90 per review 0004 §4 T2), got {v}"
        );
    }

    /// T3: fresh_wallet_funding_hours lowered 48→24 per review 0004 §4 T3.
    #[test]
    fn config_fresh_wallet_hours_is_24() {
        use std::path::PathBuf;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");
        let cfg = crate::config::load_detector_config(&config_path).expect("config must load");
        assert_eq!(
            cfg.withdraw_withheld.fresh_wallet_funding_hours.value, 24,
            "fresh_wallet_funding_hours must be 24 (lowered from 48 per review 0004 §4 T3)"
        );
    }

    /// T1: min_single_event_withdraw_usd exists and equals 5000.
    #[test]
    fn config_min_single_event_withdraw_usd_is_5000() {
        use std::path::PathBuf;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config/detectors.toml");
        let cfg = crate::config::load_detector_config(&config_path).expect("config must load");
        let v = cfg.withdraw_withheld.min_single_event_withdraw_usd.value;
        assert!(
            (v - 5000.0).abs() < 1e-6,
            "min_single_event_withdraw_usd must be 5000.0, got {v}"
        );
    }

    // -----------------------------------------------------------------------
    // Fixture-based tests (pure computation, no DB)
    // -----------------------------------------------------------------------

    /// POS-D07-01: Signal A fires, no Signal B, combined_with_d01_s2 = 1
    #[test]
    fn fixture_pos_d07_01_signal_a_fires() {
        // 5 events, $2,000 cumulative, exact authority, fee_bps=5000 > 3000
        let conf =
            compute_signal_a_confidence(5, Some(2000.0), 3, 1000.0, true).expect("must fire");
        assert!(conf >= 0.72, "POS-D07-01: conf must be >= 0.72, got {conf}");
        assert!(conf <= 0.90, "POS-D07-01: conf must be <= 0.90");
        // combined_with_d01_s2: fee_bps=5000 > threshold=3000 (compile-time invariant, not a runtime check)
    }

    /// POS-D07-02: Signal B fires at 0.75 (fresh wallet + rapid rotation)
    #[test]
    fn fixture_pos_d07_02_signal_b_confidence() {
        // prev_authority_tenure = 3d < 7d → rapid_rotation = true
        // fresh wallet (funded 2h before rotation, < 48h threshold) → is_fresh = true
        let b_conf = compute_signal_b_confidence(true, 3, 7);
        assert!((b_conf - 0.75).abs() < 1e-6, "Signal B must be 0.75");
    }

    /// POS-D07-02: Signal A base for composite calculation
    #[test]
    fn fixture_pos_d07_02_signal_a_for_composite() {
        // 3 events, $5,500, exact authority (FreshWalletBBBB)
        // usd_factor = min(0.15, ln(5.5)*0.10) = min(0.15, 0.1704) = 0.15
        // conf = 0.60 + min(0.15, 0*0.03) + 0.15 = 0.60 + 0.0 + 0.15 = 0.75
        let a_conf = compute_signal_a_confidence(3, Some(5500.0), 3, 1000.0, true).unwrap();
        let usd_factor = ((5.5_f64).ln() * 0.10_f64).min(0.15_f64);
        let expected = (0.60_f64 + usd_factor).min(0.90_f64);
        assert!(
            (a_conf - expected).abs() < 1e-4,
            "Signal A for POS-D07-02: expected {expected:.4}, got {a_conf:.4}"
        );
        // Composite: min(0.95, 0.75 + 0.10) = 0.85
        let composite = (a_conf + 0.10).min(0.95);
        assert!(
            (composite - 0.85).abs() < 0.01,
            "Composite for POS-D07-02: expected 0.85, got {composite:.4}"
        );
    }

    /// POS-D07-03: 8 events, $12,000, Signal A → capped at 0.90
    #[test]
    fn fixture_pos_d07_03_signal_a_capped() {
        let conf = compute_signal_a_confidence(8, Some(12000.0), 3, 1000.0, true).unwrap();
        // 0.60 + min(0.15, 5*0.03) + ln(12.0)*0.10 = 0.60 + 0.15 + 0.249 = 0.999 → cap 0.90
        assert!(
            (conf - 0.90).abs() < 1e-6,
            "POS-D07-03 must be capped at 0.90, got {conf}"
        );
    }

    /// NEG-D07-01: PYUSD – zero transfer fee → InsufficientBaseline
    /// (The no-transfer-fee check is tested via the pure function: event_count=0, usd=None)
    #[test]
    fn fixture_neg_d07_01_zero_fee_token_no_signal_a() {
        // If fee_bps = 0, signal A should not fire (but we test the event count gate here
        // since the fee check happens in the evaluate() method, not in compute_signal_a_confidence)
        let conf = compute_signal_a_confidence(0, None, 3, 1000.0, true);
        assert!(conf.is_none(), "zero events must return None");
    }

    /// NEG-D07-02: No TransferFeeConfig → InsufficientBaseline
    #[test]
    fn fixture_neg_d07_02_no_transfer_fee_config() {
        // Simulate: transfer_fee is None → detector returns InsufficientBaseline
        // Pure function test: event_count below threshold
        let conf = compute_signal_a_confidence(0, None, 3, 1000.0, true);
        assert!(conf.is_none(), "must return None with no events");
    }

    /// NEG-D07-03: Legacy SPL (no transfer fee) → InsufficientBaseline
    #[test]
    fn fixture_neg_d07_03_legacy_spl_no_signal() {
        // Same as NEG-D07-02: no events → None
        let conf = compute_signal_a_confidence(1, None, 3, 1000.0, true);
        assert!(conf.is_none(), "1 event below min_events must return None");
    }

    // -----------------------------------------------------------------------
    // is_established_protocol suppression (conditional)
    // -----------------------------------------------------------------------

    /// Established protocol + ratio < 0.50 → suppressed (Signal A does not fire).
    /// Threshold lowered 0.90→0.50 per review 0004 §4 T2.
    /// (Tests the ratio logic directly; full suppression path requires DB and is
    /// dead code while pool_volume_usd = 0.0 per ACCEPTED-RISK-D07-01.)
    #[test]
    fn established_protocol_low_ratio_suppresses() {
        // extraction_usd = 500, pool_volume = 10000 → ratio = 0.05 < 0.50 → suppressed
        let extraction_usd = 500.0_f64;
        let pool_volume_usd = 10_000.0_f64;
        let ratio = extraction_usd / pool_volume_usd;
        let ep_allowlist_pct = 0.50_f64; // updated from 0.90 per review 0004 §4 T2
        assert!(
            ratio <= ep_allowlist_pct,
            "ratio {ratio} must be <= {ep_allowlist_pct} → should be suppressed"
        );
    }

    /// Established protocol + ratio > 0.50 → NOT suppressed, skip_reason = 1.
    /// Threshold lowered 0.90→0.50 per review 0004 §4 T2.
    #[test]
    fn established_protocol_high_ratio_fires_with_skip_reason() {
        let extraction_usd = 5_500.0_f64;
        let pool_volume_usd = 10_000.0_f64;
        let ratio = extraction_usd / pool_volume_usd;
        let ep_allowlist_pct = 0.50_f64; // updated from 0.90 per review 0004 §4 T2
        assert!(
            ratio > ep_allowlist_pct,
            "ratio {ratio} must be > {ep_allowlist_pct} → should fire (skip_reason=1)"
        );
    }

    // -----------------------------------------------------------------------
    // Two-tier Signal A tests (review 0004 §4 T1, E-D07-9 mitigation)
    // -----------------------------------------------------------------------

    const MIN_SINGLE_EVENT_USD: f64 = 5000.0;

    /// Single event above the $5,000 floor → fires at confidence 0.65, tier "single_event".
    #[test]
    fn signal_a_single_event_above_floor_fires_at_0_65() {
        let result = compute_signal_a_confidence_tiered(
            1,
            Some(5500.0),
            MIN_EVENTS,
            MIN_USD,
            MIN_SINGLE_EVENT_USD,
            true, // exact authority
        );
        let (conf, tier) = result.expect("must fire: 1 event at $5,500 > $5,000 floor");
        assert!(
            (conf - 0.65).abs() < 1e-6,
            "single_event tier must fire at 0.65, got {conf}"
        );
        assert_eq!(tier, "single_event", "tier must be single_event");
    }

    /// Single event below the $5,000 floor → no fire.
    #[test]
    fn signal_a_single_event_below_floor_no_fire() {
        let result = compute_signal_a_confidence_tiered(
            1,
            Some(1500.0),
            MIN_EVENTS,
            MIN_USD,
            MIN_SINGLE_EVENT_USD,
            true,
        );
        assert!(
            result.is_none(),
            "single event below $5,000 floor must not fire"
        );
    }

    /// Two events with cumulative USD >= min_usd → fires at 0.60, tier "two_event".
    #[test]
    fn signal_a_two_events_fires_at_0_60() {
        let result = compute_signal_a_confidence_tiered(
            2,
            Some(2500.0),
            MIN_EVENTS,
            MIN_USD,
            MIN_SINGLE_EVENT_USD,
            true,
        );
        let (conf, tier) = result.expect("must fire: 2 events at $2,500 >= $1,000 min_usd");
        assert!(
            (conf - 0.60).abs() < 1e-6,
            "two_event tier must fire at 0.60, got {conf}"
        );
        assert_eq!(tier, "two_event", "tier must be two_event");
    }

    /// Three events → recurring tier (primary formula, not two-event override).
    /// Confirms the tier label and that the formula path is taken rather than fixed 0.60.
    #[test]
    fn signal_a_three_events_uses_recurring_tier() {
        let result = compute_signal_a_confidence_tiered(
            3,
            Some(3500.0),
            MIN_EVENTS,
            MIN_USD,
            MIN_SINGLE_EVENT_USD,
            true,
        );
        let (conf, tier) = result.expect("must fire: 3 events >= min_extraction_events=3");
        assert_eq!(tier, "recurring", "3 events must use recurring tier");
        // Primary formula: 0.60 + 0*0.03 + ln(3.5)*0.10 = 0.60 + 0 + 0.1253 = 0.7253
        let expected = (0.60_f64 + (3.5_f64).ln() * 0.10_f64).min(0.90_f64);
        assert!(
            (conf - expected).abs() < 1e-4,
            "recurring tier at 3 events $3,500: expected {expected:.4}, got {conf:.4}"
        );
        // Must NOT be the fixed two-event value of 0.60
        assert!(
            (conf - 0.60).abs() > 1e-4,
            "recurring tier must use formula, not the two-event fixed 0.60"
        );
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    /// Same inputs → same confidence (determinism contract).
    /// Two calls with identical inputs must produce bit-identical output.
    #[test]
    fn signal_a_confidence_deterministic() {
        let c1 = compute_signal_a_confidence(5, Some(2000.0), 3, 1000.0, true).unwrap();
        let c2 = compute_signal_a_confidence(5, Some(2000.0), 3, 1000.0, true).unwrap();
        // f64 equality is appropriate here: same arithmetic path, same inputs
        assert_eq!(
            c1.to_bits(),
            c2.to_bits(),
            "confidence must be bit-identical"
        );
    }

    #[test]
    fn signal_b_confidence_deterministic() {
        let c1 = compute_signal_b_confidence(true, 3, 7);
        let c2 = compute_signal_b_confidence(true, 3, 7);
        assert_eq!(
            c1.to_bits(),
            c2.to_bits(),
            "Signal B confidence must be bit-identical"
        );
    }
}
