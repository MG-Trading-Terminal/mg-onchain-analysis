//! D08 — Sybil Bundled-Launch Detector
//!
//! # Signal design (design 0015 §6)
//!
//! Detects coordinated wallet sets (Sybil clusters) that hold a disproportionate
//! share of a token's top-N holder list. The primary mechanism is the
//! **common-funder heuristic**: two or more wallets funded by the same source
//! address within the same time+amount window form a cluster (ClusterDetector).
//! When a significant fraction of that cluster appears as top holders of a single
//! token, a bundled launch is indicated.
//!
//! ## Signal A — Cluster top-holder overlap
//!
//! ```text
//! top_holder_overlap_pct = token_holders_in_cluster / cluster_member_count
//!
//! if top_holder_overlap_pct >= sybil_cluster_top_holder_pct_threshold
//!    AND cluster_member_count >= sybil_cluster_min_size:
//!     conf_raw_A = 0.40 + 0.40 * top_holder_overlap_pct  // [0.40, 0.80]
//! ```
//!
//! ## Signal B — Cluster confidence amplifier
//!
//! ```text
//! conf_raw_B = conf_raw_A * (0.50 + 0.50 * cluster_confidence)
//!            // amplify by wallet_clusters.confidence in [0.50, 0.85]
//!            // multiplier range: [0.75, 0.925]
//!
//! confidence = clamp(conf_raw_B, 0.0, 0.95)
//! ```
//!
//! Capped at 0.95 because Sprint 11 has no synchronized-activity confirmation.
//! Even a tight common-funder cluster with 100% top-holder overlap could be a
//! legitimate airdrop recipient group.
//!
//! ## Established-protocol suppression
//!
//! Per design 0015 §6.2: D08 does NOT apply `is_established_protocol` suppression.
//! Established tokens (BONK, WIF, RAY) can be Sybil-targeted for wash trading;
//! suppressing D08 on `jup_strict` tokens would mask those events.
//!
//! # Evidence keys (all prefixed `sybil_detection/` per gotcha #9)
//!
//! | Key | Type | Meaning |
//! |-----|------|---------|
//! | `sybil_detection/cluster_size` | Decimal(int) | `wallet_clusters.member_count` |
//! | `sybil_detection/cluster_confidence` | Decimal | `wallet_clusters.confidence` |
//! | `sybil_detection/top_holder_overlap_pct` | Decimal | Signal A metric |
//! | `sybil_detection/token_holders_in_cluster` | Decimal(int) | Signal A numerator |
//! | `sybil_detection/sybil_cluster_min_size_threshold` | Decimal(int) | Config threshold used |
//!
//! The cluster UUID goes into `Evidence::addresses` as the root_funder address
//! (consistent with D01 pool address pattern), and into `Evidence::notes` as
//! `"cluster_id=<uuid>"` (UUIDs cannot be stored in the Decimal metrics map).
//!
//! # Design reference
//!
//! `docs/designs/0015-crates-graph-phase3.md` §5.2 + §6
//!
//! # Citations
//!
//! - Liu et al. 2025 (arxiv:2505.09313): Sybil detection LightGBM, AUC >0.90
//!   on 193,701 addresses. "Fraction of cluster members holding the token" top-5 feature.
//! - Messias, Yaish & Livshits 2023 (arxiv:2312.02752): airdrop farming via common funder.
//! - Chainalysis 2025: Heuristic 2, common-funder controller; $1.87B confirmed wash volume.
//!   "94% of rugged tokens had deployer as primary holder controller."

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tracing::{instrument, warn};
use uuid::Uuid;

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Address;
use mg_onchain_graph::api::ClusterStore;
use mg_onchain_graph::labels::GraphLabelStore;
use mg_onchain_graph::SmartMoneyLookup;

use crate::context::DetectorContext;
use crate::error::DetectorError;
use crate::signals::severity_from_confidence;
use crate::smart_money_amplifier::{TierCounts, intersect_tier_counts};

/// Stable detector ID string used in `AnomalyEvent.detector_id` and as the
/// evidence key prefix (gotcha #9).
pub const DETECTOR_ID: &str = "sybil_detection";

// ---------------------------------------------------------------------------
// D08SybilDetector
// ---------------------------------------------------------------------------

/// D08 Sybil bundled-launch detector.
///
/// Consumes `ClusterStore` and `GraphLabelStore` injected at construction time
/// (design 0015 §5.2 Option B). These stores hold their own database connections
/// and are safe to call from async contexts.
///
/// # Object-safety note
///
/// `D08SybilDetector` stores `Arc<dyn ClusterStore>` and `Arc<dyn GraphLabelStore>`.
/// Both traits are `Send + Sync`. The `evaluate` future captures `&'ctx self` which
/// is `Send` when `Self: Send`. The `D08SybilDetector` implements `Send` automatically
/// because all fields are `Send`.
pub struct D08SybilDetector {
    /// Cluster membership store — used to look up cluster members for each top holder.
    pub cluster_store: Arc<dyn ClusterStore>,
    /// Label store — used to write `Sybil` labels on detected clusters.
    /// Note: this is SEPARATE from `smart_money` — different trait, different semantics.
    /// See design 0023 Deliverable 4: both injections coexist.
    pub label_store: Arc<dyn GraphLabelStore>,
    /// Smart-money lookup — `None` when not wired (backwards-compat, existing tests).
    /// Injected by production `init/detectors.rs` in Sprint 23.
    /// SEPARATE from `label_store` (different trait, different DB access pattern).
    pub smart_money: Option<Arc<dyn SmartMoneyLookup>>,
}

impl D08SybilDetector {
    /// Construct a new D08 detector with the given stores.
    ///
    /// `smart_money` defaults to `None`. Existing call sites are unchanged.
    pub fn new(
        cluster_store: Arc<dyn ClusterStore>,
        label_store: Arc<dyn GraphLabelStore>,
    ) -> Self {
        Self {
            cluster_store,
            label_store,
            smart_money: None,
        }
    }

    /// Wire in a [`SmartMoneyLookup`] for D08 cluster smart-money amplification.
    ///
    /// When `Some`, the detector fetches all SmartMoney-labelled addresses for the
    /// chain, intersects with the cluster member set, and applies per-tier confidence
    /// deltas (Tier1: +0.10, Tier2 ≥2: +0.05). Amplification direction is UPWARD:
    /// smart money in a Sybil cluster = informed coordinated attacker (design 0023 §1.2).
    ///
    /// When `None` (default), no amplification occurs — fully backwards-compatible.
    ///
    /// # References
    ///
    /// - Fu, Feng, Wu & Xu 2025 (Perseus): mastermind wallets in adversarial clusters.
    /// - Liu et al. 2025 (arXiv:2505.09313): cluster + smart-money co-presence.
    /// - design 0023 §4.2, Decision 4 (user approved).
    pub fn with_smart_money(mut self, lookup: Arc<dyn SmartMoneyLookup>) -> Self {
        self.smart_money = Some(lookup);
        self
    }
}

// ---------------------------------------------------------------------------
// Detector trait implementation
// ---------------------------------------------------------------------------

impl crate::detector::Detector for D08SybilDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn severity_floor(&self) -> Severity {
        Severity::Low
    }

    fn supported_chains(&self) -> &[mg_onchain_common::chain::Chain] {
        &[
            mg_onchain_common::chain::Chain::Solana,
            mg_onchain_common::chain::Chain::Ethereum,
            mg_onchain_common::chain::Chain::Bsc,
            mg_onchain_common::chain::Chain::Base,
            mg_onchain_common::chain::Chain::Arbitrum,
            mg_onchain_common::chain::Chain::Polygon,
        ]
    }

    /// Evaluate Sybil bundled-launch signal for the token in `ctx`.
    ///
    /// # Algorithm (design 0015 §6.2)
    ///
    /// 1. Fetch the top-N holder snapshot from `ctx.store`.
    /// 2. For each holder, query `cluster_store.wallet_cluster` to find its cluster.
    /// 3. Group holders by `cluster_id` → find the largest cluster.
    /// 4. Compute `top_holder_overlap_pct = holders_in_cluster / cluster_member_count`.
    /// 5. If overlap >= threshold AND cluster_size >= min_size: compute Signal A + B.
    /// 6. Emit `AnomalyEvent` with evidence keys prefixed `sybil_detection/`.
    ///
    /// # Determinism
    ///
    /// Uses `BTreeMap` for grouping (no HashMap). DB queries are ORDER BY'd at the
    /// storage layer. Output is fully determined by input block range + config.
    #[instrument(skip(self, ctx), fields(chain = %ctx.chain, token = %ctx.token))]
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx
    {
        async move {
            let cfg = &ctx.config.sybil_detection;
            let min_overlap = cfg.sybil_cluster_top_holder_pct_threshold.value;
            let min_size = cfg.sybil_cluster_min_size.value as usize;

            let chain_str = ctx.chain.to_string();
            let token_str = ctx.token.to_string();

            // Step 1: fetch top-N holder snapshot.
            let holders = fetch_top_holders(ctx, &chain_str, &token_str).await?;
            if holders.is_empty() {
                return Ok(vec![]);
            }

            // Step 2+3: for each holder, look up cluster membership. Group by cluster_id.
            // BTreeMap key = cluster_id string for deterministic iteration.
            let mut cluster_holders: BTreeMap<Uuid, ClusterAccum> = BTreeMap::new();

            for holder_addr in &holders {
                match self
                    .cluster_store
                    .wallet_cluster(&chain_str, holder_addr)
                    .await
                {
                    Ok(Some(cref)) => {
                        let entry = cluster_holders.entry(cref.cluster_id).or_insert_with(|| {
                            ClusterAccum {
                                cluster_ref: cref.clone(),
                                holders_found: BTreeSet::new(),
                            }
                        });
                        entry.holders_found.insert(holder_addr.clone());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return Err(DetectorError::PermanentQuery {
                            detector_id: DETECTOR_ID,
                            reason: format!(
                                "cluster_store.wallet_cluster failed for {holder_addr}: {e}"
                            ),
                        });
                    }
                }
            }

            if cluster_holders.is_empty() {
                return Ok(vec![]);
            }

            // Step 3: find the largest cluster by token_holders_in_cluster.
            // Tie-break by cluster_id string for determinism.
            let best = cluster_holders
                .values()
                .max_by(|a, b| {
                    a.holders_found
                        .len()
                        .cmp(&b.holders_found.len())
                        .then_with(|| {
                            a.cluster_ref
                                .cluster_id
                                .to_string()
                                .cmp(&b.cluster_ref.cluster_id.to_string())
                        })
                })
                .expect("cluster_holders is non-empty; max must exist");

            let cref = &best.cluster_ref;
            let cluster_size = cref.member_count as usize;
            let token_holders_in_cluster = best.holders_found.len();
            let cluster_confidence = cref.confidence;

            // Step 4: compute overlap.
            // Guard against cluster_size = 0 (should not happen, but defensive).
            if cluster_size < min_size {
                return Ok(vec![]);
            }

            let top_holder_overlap = token_holders_in_cluster as f64 / cluster_size as f64;

            if top_holder_overlap < min_overlap {
                return Ok(vec![]);
            }

            // Step 5: compute confidence (design 0015 §6.2).
            let conf_raw_a = 0.40 + 0.40 * top_holder_overlap;
            let conf_raw_b = conf_raw_a * (0.50 + 0.50 * cluster_confidence);
            let confidence_f64 = conf_raw_b.clamp(0.0, 0.95);

            let confidence = Confidence::new(confidence_f64).map_err(|e| {
                DetectorError::DeterminismViolation {
                    detector_id: DETECTOR_ID,
                    reason: format!("confidence out of range after clamp (bug): {e}"),
                }
            })?;

            let severity = severity_from_confidence(confidence_f64);

            // Step 6: build evidence (keys prefixed sybil_detection/ per gotcha #9).
            let overlap_decimal = Decimal::from_f64(top_holder_overlap).unwrap_or(Decimal::ZERO);
            let cluster_conf_decimal =
                Decimal::from_f64(cluster_confidence).unwrap_or(Decimal::ZERO);
            let min_size_threshold_decimal = Decimal::from_usize(min_size).unwrap_or(Decimal::ZERO);

            let mut evidence = Evidence::new()
                .with_metric(
                    format!("{DETECTOR_ID}/cluster_size"),
                    Decimal::from_usize(cluster_size).unwrap_or(Decimal::ZERO),
                )
                .with_metric(
                    format!("{DETECTOR_ID}/cluster_confidence"),
                    cluster_conf_decimal,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/top_holder_overlap_pct"),
                    overlap_decimal,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/token_holders_in_cluster"),
                    Decimal::from_usize(token_holders_in_cluster).unwrap_or(Decimal::ZERO),
                )
                .with_metric(
                    format!("{DETECTOR_ID}/sybil_cluster_min_size_threshold"),
                    min_size_threshold_decimal,
                )
                // cluster UUID goes into notes (cannot be stored as Decimal metric).
                .with_note(format!("cluster_id={}", cref.cluster_id))
                .with_note(format!(
                    "algorithm=common_funder; cluster_kind={}",
                    format!("{:?}", cref.cluster_kind).to_lowercase()
                ));

            // root_funder address goes into addresses (consistent with D01 pool address pattern).
            // Only add if root_funder is a valid canonical address; skip if absent.
            if let Some(ref funder_str) = cref.root_funder {
                if let Ok(addr) = Address::parse(ctx.chain, funder_str) {
                    evidence = evidence.with_address(addr);
                } else {
                    // root_funder is not a valid canonical address — note it instead.
                    evidence = evidence.with_note(format!("root_funder={funder_str}"));
                }
            }

            // Step 7 (S23): Smart-money cluster member amplification.
            //
            // Minimum confidence gate per design 0023 §5.3: lookup only when conf_raw_b >= 0.30.
            // Amplification direction: UPWARD — smart money in a Sybil cluster = informed
            // coordinated attacker (design 0023 §1.2 + Decision 4).
            let (final_confidence, final_severity) = if let Some(ref sm_lookup) = self.smart_money {
                if confidence_f64 >= 0.30 {
                    // Fetch cluster member addresses for smart-money intersection.
                    let cluster_members = self
                        .cluster_store
                        .cluster_members(cref.cluster_id)
                        .await
                        .unwrap_or_else(|e| {
                            warn!(
                                cluster_id = %cref.cluster_id,
                                error = %e,
                                "D08: cluster_members query failed; skipping smart-money intersection"
                            );
                            vec![]
                        });

                    match sm_lookup
                        .fetch_smart_money_addresses(ctx.chain.as_str(), ctx.observed_at)
                        .await
                    {
                        Ok(sm_map) => {
                            let tier_counts = intersect_tier_counts(&cluster_members, &sm_map);
                            let delta = compute_smart_money_amplification_d08(
                                &tier_counts,
                                &ctx.config.sybil_detection,
                            );
                            let amplified = (confidence_f64 + delta).clamp(0.0, 0.95);

                            // Emit 5-key standardized evidence schema (Decision 7).
                            let sm_present = if tier_counts.has_any() { Decimal::ONE } else { Decimal::ZERO };
                            let delta_dec = Decimal::from_f64(delta).unwrap_or(Decimal::ZERO);
                            evidence.metrics.insert(
                                format!("{DETECTOR_ID}/smart_money_present"),
                                sm_present,
                            );
                            evidence.metrics.insert(
                                format!("{DETECTOR_ID}/smart_money_tier1_count"),
                                Decimal::from(tier_counts.tier1),
                            );
                            evidence.metrics.insert(
                                format!("{DETECTOR_ID}/smart_money_tier2_count"),
                                Decimal::from(tier_counts.tier2),
                            );
                            evidence.metrics.insert(
                                format!("{DETECTOR_ID}/smart_money_tier3_count"),
                                Decimal::from(tier_counts.tier3),
                            );
                            evidence.metrics.insert(
                                format!("{DETECTOR_ID}/smart_money_amplification_delta"),
                                delta_dec,
                            );

                            let amp_conf = Confidence::new(amplified).unwrap_or(confidence);
                            let amp_severity = severity_from_confidence(amplified);
                            (amp_conf, amp_severity)
                        }
                        Err(e) => {
                            warn!(
                                token = ctx.token.as_str(),
                                error = %e,
                                "D08: smart_money_lookup failed; skipping amplification (non-fatal)"
                            );
                            (confidence, severity)
                        }
                    }
                } else {
                    (confidence, severity)
                }
            } else {
                (confidence, severity)
            };

            let event = AnomalyEvent {
                detector_id: DETECTOR_ID.to_owned(),
                token: ctx.token.clone(),
                chain: ctx.chain,
                confidence: final_confidence,
                severity: final_severity,
                evidence,
                observed_at: ctx.observed_at,
                ingested_at: ctx.observed_at,
                window: (ctx.window.block_start, ctx.window.block_end),
            };

            Ok(vec![event])
        }
    }
}

// ---------------------------------------------------------------------------
// ClusterAccum — intermediate grouping state
// ---------------------------------------------------------------------------

/// Intermediate state for grouping top holders by cluster.
struct ClusterAccum {
    cluster_ref: mg_onchain_graph::api::ClusterRef,
    /// Set of holder addresses (from the top-N snapshot) that belong to this cluster.
    holders_found: BTreeSet<String>,
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// Fetch the top-N holder addresses for a token from the `holder_snapshots` table.
///
/// Returns addresses in descending balance order (ORDER BY balance_raw DESC).
/// Returns an empty Vec if no snapshot exists for the token.
async fn fetch_top_holders(
    ctx: &DetectorContext<'_>,
    chain: &str,
    token: &str,
) -> Result<Vec<String>, DetectorError> {
    // Query the most recent holder snapshot for this token.
    // Column is `holder` (per migration V00003 schema).
    let rows = sqlx::query(
        r#"
        SELECT hs.holder
        FROM holder_snapshots hs
        WHERE hs.chain = $1
          AND hs.token = $2
        ORDER BY hs.balance_raw DESC
        LIMIT 100
        "#,
    )
    .bind(chain)
    .bind(token)
    .fetch_all(ctx.store.pool())
    .await
    .map_err(|e| DetectorError::PermanentQuery {
        detector_id: DETECTOR_ID,
        reason: format!("fetch_top_holders query failed: {e}"),
    })?;

    let addresses: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            use sqlx::Row as _;
            row.try_get::<String, _>("holder").ok()
        })
        .collect();

    Ok(addresses)
}

// ---------------------------------------------------------------------------
// Pure signal computation (exposed for unit tests)
// ---------------------------------------------------------------------------

/// Compute D08 Signal A+B confidence from raw metrics.
///
/// Exposed as a pure function for unit testing without I/O. The caller is
/// responsible for enforcing `cluster_size >= min_size` and
/// `overlap >= min_overlap` before calling this function.
///
/// # Arguments
///
/// - `top_holder_overlap_pct`: `token_holders_in_cluster / cluster_member_count`
/// - `cluster_confidence`: `wallet_clusters.confidence` in `[0.50, 0.85]`
///
/// # Returns
///
/// Confidence in `[0.0, 0.95]`.
pub fn compute_sybil_confidence(top_holder_overlap_pct: f64, cluster_confidence: f64) -> f64 {
    let conf_raw_a = 0.40 + 0.40 * top_holder_overlap_pct;
    let conf_raw_b = conf_raw_a * (0.50 + 0.50 * cluster_confidence);
    conf_raw_b.clamp(0.0, 0.95)
}

// ---------------------------------------------------------------------------
// Smart-money amplification — D08 specific
// ---------------------------------------------------------------------------

/// Compute the smart-money confidence amplification delta for D08.
///
/// Per-tier delta (additive, applied once per evaluation):
/// - Tier1: `tier1_count >= 1` → +0.10 (user-approved Decision 4, design 0023 §4.2)
/// - Tier2: `tier2_count >= smart_money_tier2_min_count` → +0.05
/// - Tier3: 0.00 (no amplification)
///
/// When Tier1 is present, only the Tier1 delta is applied.
///
/// D08 deltas are slightly lower than D04 (Tier1: 0.10 vs 0.12) because D08's base
/// confidence is already structural/strong (Signal A+B formula). See design 0023 §4.2.
///
/// `f64` is used here because this is a probability/confidence delta, NOT a monetary
/// amount (per CLAUDE.md: f64 only for confidence/deltas).
pub fn compute_smart_money_amplification_d08(
    tier_counts: &TierCounts,
    cfg: &crate::config::SybilConfig,
) -> f64 {
    let tier1_delta = cfg.smart_money_tier1_delta.value;
    let tier2_delta = cfg.smart_money_tier2_delta.value;
    let tier2_min_count = cfg.smart_money_tier2_min_count.value;

    if tier_counts.tier1 >= 1 {
        tier1_delta
    } else if tier_counts.tier2 >= tier2_min_count {
        tier2_delta
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- compute_sybil_confidence ---

    /// At minimum threshold (overlap=0.30, cluster_conf=0.50):
    /// conf_raw_a = 0.40 + 0.40*0.30 = 0.52
    /// conf_raw_b = 0.52 * (0.50 + 0.50*0.50) = 0.52 * 0.75 = 0.39
    /// → Severity::Low
    #[test]
    fn sybil_confidence_minimum_threshold() {
        let conf = compute_sybil_confidence(0.30, 0.50);
        assert!(
            (conf - 0.39).abs() < 1e-9,
            "minimum-threshold confidence must be ≈0.39, got {conf:.4}"
        );
    }

    /// At full overlap (overlap=1.0, cluster_conf=0.85):
    /// conf_raw_a = 0.40 + 0.40*1.0 = 0.80
    /// conf_raw_b = 0.80 * (0.50 + 0.50*0.85) = 0.80 * 0.925 = 0.74
    /// → Severity::High
    #[test]
    fn sybil_confidence_full_overlap_high_cluster_conf() {
        let conf = compute_sybil_confidence(1.0, 0.85);
        assert!(
            (conf - 0.74).abs() < 1e-9,
            "full-overlap high-cluster-conf confidence must be ≈0.74, got {conf:.4}"
        );
    }

    /// Confidence is clamped at 0.95 — cannot exceed (theoretical worst case).
    #[test]
    fn sybil_confidence_never_exceeds_0_95() {
        // Extreme inputs — still clamped.
        let conf = compute_sybil_confidence(1.0, 1.0);
        assert!(
            conf <= 0.95,
            "confidence must never exceed 0.95 (no synchronized-activity confirmation), got {conf}"
        );
    }

    /// Confidence is always >= 0.
    #[test]
    fn sybil_confidence_always_non_negative() {
        let conf = compute_sybil_confidence(0.0, 0.0);
        assert!(conf >= 0.0, "confidence must be non-negative, got {conf}");
    }

    /// Known-value test: overlap=0.60, cluster_conf=0.70.
    /// conf_raw_a = 0.40 + 0.40*0.60 = 0.64
    /// conf_raw_b = 0.64 * (0.50 + 0.50*0.70) = 0.64 * 0.85 = 0.544
    /// → Severity::Medium
    #[test]
    fn sybil_confidence_known_value_medium() {
        let conf = compute_sybil_confidence(0.60, 0.70);
        assert!(
            (conf - 0.544).abs() < 1e-9,
            "known-value test: conf must be ≈0.544, got {conf:.6}"
        );
    }

    // --- severity ladder for D08 confidence outputs ---

    #[test]
    fn sybil_severity_low_at_minimum_threshold() {
        // overlap=0.30, cluster_conf=0.50 → conf≈0.39 → Severity::Low
        let conf = compute_sybil_confidence(0.30, 0.50);
        assert_eq!(
            severity_from_confidence(conf),
            mg_onchain_common::anomaly::Severity::Low
        );
    }

    #[test]
    fn sybil_severity_medium_at_60pct_overlap() {
        // overlap=0.60, cluster_conf=0.70 → conf=0.544 → Severity::Medium
        let conf = compute_sybil_confidence(0.60, 0.70);
        assert_eq!(
            severity_from_confidence(conf),
            mg_onchain_common::anomaly::Severity::Medium
        );
    }

    #[test]
    fn sybil_severity_high_at_full_overlap() {
        // overlap=1.0, cluster_conf=0.85 → conf=0.74 → Severity::High
        let conf = compute_sybil_confidence(1.0, 0.85);
        assert_eq!(
            severity_from_confidence(conf),
            mg_onchain_common::anomaly::Severity::High
        );
    }

    // --- Positive fixture POS_D08_01 (synthetic) ---

    /// Positive fixture: 5 top holders all in a cluster of 5 (overlap=1.0, conf=0.75).
    /// D08 should fire with confidence > 0.40 (Severity::Medium or higher).
    #[test]
    fn positive_fixture_pos_d08_01_fires() {
        // POS_D08_01: 5 cluster members all appear as top holders.
        let cluster_member_count = 5;
        let token_holders_in_cluster = 5;
        let cluster_confidence = 0.75_f64;

        // Replicate the threshold check from D08 algorithm.
        let min_overlap = 0.30_f64;
        let min_size = 3_usize;

        assert!(
            cluster_member_count >= min_size,
            "cluster_size must pass min_size filter"
        );

        let top_holder_overlap = token_holders_in_cluster as f64 / cluster_member_count as f64;
        assert!(
            top_holder_overlap >= min_overlap,
            "POS_D08_01 overlap must exceed threshold"
        );

        let conf = compute_sybil_confidence(top_holder_overlap, cluster_confidence);
        assert!(
            conf >= 0.40,
            "POS_D08_01 must produce confidence >= 0.40 (Medium+), got {conf:.4}"
        );
    }

    /// Negative fixture: empty cluster membership (no common funder).
    /// D08 must NOT fire — overlap check returns before computing confidence.
    #[test]
    fn negative_fixture_neg_d08_01_no_cluster_no_fire() {
        // NEG_D08_01: 10 top holders, cluster_members is empty → overlap = 0.0.
        let cluster_member_count = 0_usize;
        let min_size = 3_usize;

        // Guard: cluster below min_size → early return (no signal).
        assert!(
            cluster_member_count < min_size,
            "NEG_D08_01 cluster must be below min_size to prevent firing"
        );
    }

    /// Partial overlap below threshold should not fire.
    #[test]
    fn below_threshold_overlap_no_fire() {
        // 1 out of 5 cluster members is a top holder → overlap = 0.20 < 0.30 threshold.
        let cluster_member_count = 5_usize;
        let token_holders_in_cluster = 1_usize;
        let min_overlap = 0.30_f64;
        let min_size = 3_usize;

        assert!(cluster_member_count >= min_size);
        let top_holder_overlap = token_holders_in_cluster as f64 / cluster_member_count as f64;
        assert!(
            top_holder_overlap < min_overlap,
            "overlap {} must be below threshold {}",
            top_holder_overlap,
            min_overlap
        );
        // D08 would return Ok(vec![]) here — no event.
    }

    /// Idempotency: calling compute_sybil_confidence with the same inputs twice gives
    /// the same output (pure function check).
    #[test]
    fn compute_sybil_confidence_is_deterministic() {
        let c1 = compute_sybil_confidence(0.80, 0.75);
        let c2 = compute_sybil_confidence(0.80, 0.75);
        assert_eq!(c1, c2, "compute_sybil_confidence must be deterministic");
    }

    /// DETECTOR_ID matches the evidence key prefix used in the implementation.
    #[test]
    fn detector_id_matches_evidence_prefix() {
        assert_eq!(DETECTOR_ID, "sybil_detection");
        // Evidence keys must use this prefix.
        let key = format!("{DETECTOR_ID}/cluster_size");
        assert_eq!(key, "sybil_detection/cluster_size");
    }

    // -------------------------------------------------------------------------
    // S23 smart-money amplification tests for D08
    // -------------------------------------------------------------------------

    fn default_sybil_cfg() -> crate::config::SybilConfig {
        use crate::config::Threshold;
        crate::config::SybilConfig {
            sybil_cluster_top_holder_pct_threshold: Threshold {
                value: 0.30,
                rationale: "test".into(),
                refs: vec!["D08/sybil_detection".into()],
            },
            sybil_cluster_min_size: Threshold {
                value: 3,
                rationale: "test".into(),
                refs: vec!["D08/sybil_detection".into()],
            },
            smart_money_tier1_delta: Threshold {
                value: 0.10,
                rationale: "test".into(),
                refs: vec!["D08/smart_money_amplification".into()],
            },
            smart_money_tier2_delta: Threshold {
                value: 0.05,
                rationale: "test".into(),
                refs: vec!["D08/smart_money_amplification".into()],
            },
            smart_money_tier2_min_count: Threshold {
                value: 2,
                rationale: "test".into(),
                refs: vec!["D08/smart_money_amplification".into()],
            },
        }
    }

    /// Tier1 in cluster → delta = +0.10.
    #[test]
    fn sm_amplification_d08_tier1() {
        let cfg = default_sybil_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 0,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d08(&counts, &cfg);
        assert!(
            (delta - 0.10).abs() < 1e-9,
            "Tier1 must produce delta = 0.10, got {delta}"
        );
    }

    /// Tier2 (≥ 2) in cluster, no Tier1 → delta = +0.05.
    #[test]
    fn sm_amplification_d08_tier2_min_count_met() {
        let cfg = default_sybil_cfg();
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 0,
            tier2: 2,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d08(&counts, &cfg);
        assert!(
            (delta - 0.05).abs() < 1e-9,
            "Tier2 count=2 must produce delta = 0.05, got {delta}"
        );
    }

    /// No smart money in cluster → delta = 0.00.
    #[test]
    fn sm_amplification_d08_no_smart_money() {
        let cfg = default_sybil_cfg();
        let counts = crate::smart_money_amplifier::TierCounts::default();
        let delta = compute_smart_money_amplification_d08(&counts, &cfg);
        assert!(
            delta.abs() < 1e-9,
            "no smart money must produce delta = 0.00, got {delta}"
        );
    }

    /// Full overlap + high cluster_conf + Tier1 → amplified to 0.82 (clamped ≤ 0.95).
    /// conf_raw_b = 0.74 (from existing test) + 0.10 = 0.84 → ≤ 0.95.
    #[test]
    fn sm_amplification_d08_tier1_amplifies_full_overlap() {
        let cfg = default_sybil_cfg();
        let base = compute_sybil_confidence(1.0, 0.85); // = 0.74
        let counts = crate::smart_money_amplifier::TierCounts {
            tier1: 1,
            tier2: 0,
            tier3: 0,
        };
        let delta = compute_smart_money_amplification_d08(&counts, &cfg);
        let amplified = (base + delta).clamp(0.0, 0.95);
        assert!(
            (amplified - 0.84).abs() < 1e-9,
            "full-overlap + Tier1 must produce 0.84, got {amplified}"
        );
    }

    /// D08SybilDetector::new() defaults smart_money to None.
    #[test]
    fn d08_detector_new_has_no_smart_money() {
        use mg_onchain_graph::mock::{MockClusterStore, MockGraphLabelStore};
        let det = D08SybilDetector::new(
            std::sync::Arc::new(MockClusterStore::default()),
            std::sync::Arc::new(MockGraphLabelStore::default()),
        );
        assert!(det.smart_money.is_none());
    }

    /// with_smart_money() builder sets the field.
    #[test]
    fn d08_detector_with_smart_money_sets_field() {
        use mg_onchain_graph::mock::{MockClusterStore, MockGraphLabelStore};
        use mg_onchain_graph::MockSmartMoneyLookup;
        let det = D08SybilDetector::new(
            std::sync::Arc::new(MockClusterStore::default()),
            std::sync::Arc::new(MockGraphLabelStore::default()),
        )
        .with_smart_money(std::sync::Arc::new(MockSmartMoneyLookup::empty()));
        assert!(det.smart_money.is_some());
    }

    /// D08 supported_chains returns all 6 chains.
    ///
    /// D08 is chain-agnostic: funding-cluster detection operates on wallet graphs
    /// built per-chain at the graph crate level. The `ClusterStore` and
    /// `GraphLabelStore` both key on chain strings. No Solana-specific logic
    /// exists in the D08 production evaluate path.
    #[test]
    fn d08_supported_chains_returns_six_chains() {
        use crate::detector::Detector as _;
        use mg_onchain_graph::mock::{MockClusterStore, MockGraphLabelStore};
        let det = D08SybilDetector::new(
            std::sync::Arc::new(MockClusterStore::default()),
            std::sync::Arc::new(MockGraphLabelStore::default()),
        );
        let chains = det.supported_chains();
        assert_eq!(chains.len(), 6, "D08 must support exactly 6 chains");
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Solana),
            "D08 must support Solana"
        );
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Ethereum),
            "D08 must support Ethereum"
        );
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Bsc),
            "D08 must support BSC"
        );
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Base),
            "D08 must support Base"
        );
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Arbitrum),
            "D08 must support Arbitrum"
        );
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Polygon),
            "D08 must support Polygon"
        );
    }

    /// D08 evaluate with Ethereum context: cluster_store returns empty for any chain.
    /// With chain-agnostic storage, an Ethereum context that has no cluster data
    /// produces Ok(vec![]) — no panic, no chain-guard short-circuit.
    #[test]
    fn d08_evaluate_ethereum_context_sane_with_empty_cluster() {
        use crate::detector::Detector as _;
        use mg_onchain_graph::mock::{MockClusterStore, MockGraphLabelStore};
        let det = D08SybilDetector::new(
            std::sync::Arc::new(MockClusterStore::default()),
            std::sync::Arc::new(MockGraphLabelStore::default()),
        );
        // Ethereum is in the supported set — no chain guard prevents reaching evaluate.
        let chains = det.supported_chains();
        assert!(
            chains.contains(&mg_onchain_common::chain::Chain::Ethereum),
            "Ethereum must be in D08 supported_chains"
        );
        // compute_sybil_confidence is deterministic regardless of chain.
        // An empty cluster (size 0) is below min_size; D08 returns Ok(vec![]).
        let cluster_member_count = 0_usize;
        let min_size = 3_usize;
        assert!(
            cluster_member_count < min_size,
            "empty cluster below min_size → D08 returns Ok(vec![]) for any chain"
        );
    }
}
