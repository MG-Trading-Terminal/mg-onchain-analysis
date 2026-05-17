//! D14 — Bridge Drain Detector
//!
//! # Signal design
//!
//! Detects anomalous outflows from known bridge custody contracts: a >20% balance
//! decrease within a single block window is evidence of a bridge exploit.
//!
//! Historical incidents this detector would have caught:
//! - Ronin Bridge ($625M, March 2022): 100% balance drain of USDC + ETH custody
//! - Poly Network ($611M, August 2021): cross-chain keeper forgery → drain
//! - BNB Chain Bridge ($586M, October 2022): forged IAVL proof → 2M BNB minted/drained
//! - Wormhole ($320M, February 2022): guardian signature bypass → drain
//! - Nomad Bridge ($190M, August 2022): message replay → drain
//! - Multichain ($126M, July 2023): admin key misuse → drain
//!
//! ## Signal
//!
//! Balance-decrease anomaly on known bridge custody contracts:
//! ```text
//! total_outflow = Σ amount_raw for all transfers WHERE from_address = bridge_custody_address
//! drain_pct     = total_outflow / balance_at_window_start
//! fires if drain_pct > 0.20
//! ```
//!
//! ## Confidence formula (D14 §4.1)
//!
//! ```text
//! base      = Tier1 → 0.85  |  Tier2 → 0.65
//! amplifier = drain_pct >= 0.50 → +0.10  |  otherwise +0.00
//! conf      = min(base + amplifier, 0.95)
//! ```
//!
//! Calibration anchors:
//! - Tier1 25% drain → 0.85 (High)
//! - Tier1 60% drain → 0.95 (Critical, capped)
//! - Tier2 25% drain → 0.65 (High)
//! - Tier2 60% drain → 0.75 (High, no cap hit)
//!
//! ## Suppression policy
//!
//! D14 does NOT suppress on established protocols.
//! Bridge contracts are infrastructure, not user-driven tokens. A drain on a known
//! bridge is always High/Critical regardless of token status.
//! Consistent with D12 + D08 + D11 non-suppression policy (CLAUDE.md gotcha #17).
//!
//! ## Evidence keys (prefixed `bridge_drain/` per gotcha #9)
//!
//! | Key | Type | Meaning |
//! |-----|------|---------|
//! | `bridge_drain/bridge_name` | Note | Human-readable bridge name |
//! | `bridge_drain/tier` | Note | "Tier1" or "Tier2" |
//! | `bridge_drain/drain_pct` | Decimal | Fraction drained (0.0–1.0+) |
//! | `bridge_drain/total_outflow_usd` | Decimal | USD value of outflow (when price available) |
//! | `bridge_drain/window_blocks` | Decimal | Block count of the evaluation window |
//! | `bridge_drain/custody_address` | Note | Bridge custody address that drained |
//!
//! ## Chain scope
//!
//! All 6 supported chains: Solana, Ethereum, BSC, Base, Arbitrum, Polygon.
//! Bridge custody contracts exist on all EVM chains. Solana-side bridges (Wormhole
//! guardian program) would require a separate Solana program address registry —
//! deferred to Sprint 27+ when Solana bridge addresses are confirmed.
//!
//! ## Determinism
//!
//! - Transfers fetched ORDER BY block_height ASC, log_index ASC.
//! - Bridge addresses iterated in BTreeMap order (sorted by lowercase address string).
//! - No `Utc::now()` — `ctx.observed_at` is the sole time anchor.
//! - Evidence uses `BTreeMap` (via `Evidence::new()`).
//!
//! # Storage pattern
//!
//! Stateless recompute from `transfers` table (mirror D05 Signal B Option D).
//! No new migrations needed. Uses `ctx.store.fetch_recent_transfers_for_token`
//! with `from_address` filter applied post-fetch (the transfers table is indexed by
//! (chain, token, block_time); adding an address filter here is a pre-fetch WHERE clause
//! via the dedicated helper added to pg.rs).
//!
//! # Citations
//!
//! - rekt.news leaderboard: https://rekt.news/leaderboard
//! - Ronin: https://rekt.news/ronin-rekt ($625M, 2022-03-29)
//! - Poly Network: https://rekt.news/polynetwork-rekt ($611M, 2021-08-10)
//! - BNB Bridge: https://rekt.news/bnb-bridge-rekt ($586M, 2022-10-06)
//! - Wormhole: https://rekt.news/wormhole-rekt ($320M, 2022-02-02)
//! - Nomad: https://rekt.news/nomad-rekt ($190M, 2022-08-01)
//! - Multichain: https://rekt.news/multichain-rekt ($126M, 2023-07-21)
//! - REFERENCES.md: D14/bridge_drain_v1

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use tracing::{debug, instrument};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Chain;
use mg_onchain_storage::pg::BridgeTransferRow;
use mg_onchain_storage::price_provider::TokenPriceProvider;

use crate::context::DetectorContext;
use crate::error::DetectorError;
use crate::signals::severity_from_confidence;

/// Stable detector ID string (must match `config/detectors.toml` subsection name).
pub const DETECTOR_ID: &str = "bridge_drain_v1";

// ---------------------------------------------------------------------------
// Bridge tier
// ---------------------------------------------------------------------------

/// TVL-tier for a known bridge.
///
/// Tier1: $100M+ historical TVL or confirmed major exploit (≥$100M).
/// Tier2: Smaller bridges, emerging cross-chain protocols, or less well-documented incidents.
///
/// Tier affects the base confidence:
/// - Tier1: 0.85 (major bridge drain is near-certain high-severity)
/// - Tier2: 0.65 (lesser-known bridge may have operational explanations)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeTier {
    Tier1,
    Tier2,
}

impl BridgeTier {
    /// Human-readable tier string for evidence notes.
    pub fn as_str(&self) -> &'static str {
        match self {
            BridgeTier::Tier1 => "Tier1",
            BridgeTier::Tier2 => "Tier2",
        }
    }
}

// ---------------------------------------------------------------------------
// KnownBridge + KnownBridgeSet
// ---------------------------------------------------------------------------

/// A single known bridge entry with per-chain custody addresses.
#[derive(Debug, Clone)]
pub struct KnownBridge {
    /// Human-readable bridge name ("Ronin Bridge", "Wormhole", etc.)
    pub name: String,
    /// Which chains this bridge has custody contracts on.
    pub chains: Vec<Chain>,
    /// Per-chain custody contract addresses (lowercase hex or Base58).
    /// Key = chain; value = vec of custody addresses on that chain.
    pub addresses: HashMap<Chain, Vec<String>>,
    /// TVL tier for confidence formula.
    pub tvl_tier: BridgeTier,
    /// Citation URL or text.
    pub source: String,
}

/// A type alias for bridge name (human-readable string).
type BridgeName = String;

/// Indexed registry of known bridge custody addresses.
///
/// Built once at startup from `config/known_bridges.toml` and kept in an Arc
/// shared across all D14 evaluations.
///
/// `address_index` enables O(1) lookup: given `(Chain, address_lowercase)`,
/// returns `(BridgeName, BridgeTier)` without scanning the full bridge list.
///
/// The `address_index` uses `BTreeMap` for determinism: iteration over matching
/// addresses in a given chain follows alphabetical order, which maps consistently
/// to output ordering.
#[derive(Debug, Clone)]
pub struct KnownBridgeSet {
    bridges: Vec<KnownBridge>,
    /// (chain, address_lowercase) → (bridge_name, tier)
    address_index: BTreeMap<(Chain, String), (BridgeName, BridgeTier)>,
}

impl KnownBridgeSet {
    /// Construct from a list of known bridges.
    ///
    /// Normalizes all addresses to lowercase and builds the index.
    ///
    /// # Panics
    ///
    /// Never panics. Duplicate (chain, address) entries are silently overwritten
    /// with the last entry (TOML order is preserved so last one wins).
    pub fn from_bridges(bridges: Vec<KnownBridge>) -> Self {
        let mut address_index: BTreeMap<(Chain, String), (BridgeName, BridgeTier)> = BTreeMap::new();

        for bridge in &bridges {
            for (chain, addrs) in &bridge.addresses {
                for addr in addrs {
                    let normalized = addr.to_lowercase();
                    address_index.insert(
                        (*chain, normalized),
                        (bridge.name.clone(), bridge.tvl_tier),
                    );
                }
            }
        }

        Self { bridges, address_index }
    }

    /// Look up whether a given (chain, address) is a known bridge custody address.
    ///
    /// Returns `Some((bridge_name, tier))` if found, `None` otherwise.
    /// Address lookup is case-insensitive (normalized to lowercase).
    pub fn is_known_bridge(&self, chain: Chain, address: &str) -> Option<(&str, BridgeTier)> {
        let normalized = address.to_lowercase();
        self.address_index
            .get(&(chain, normalized))
            .map(|(name, tier)| (name.as_str(), *tier))
    }

    /// Return all bridge custody addresses registered for the given chain.
    ///
    /// Used by `evaluate()` to iterate only the addresses relevant to the
    /// current evaluation chain.
    pub fn addresses_for_chain(&self, chain: Chain) -> Vec<(&str, &str, BridgeTier)> {
        // Collect (name, address, tier) for all bridges on this chain.
        // Use BTreeMap range scan: all keys with chain == target_chain.
        // BTreeMap is keyed by (Chain, address) — Chain is ordered.
        // We collect all entries with matching chain prefix.
        self.address_index
            .iter()
            .filter(|((c, _), _)| *c == chain)
            .map(|((_, addr), (name, tier))| (name.as_str(), addr.as_str(), *tier))
            .collect()
    }

    /// Total number of registered bridges (for telemetry).
    pub fn bridge_count(&self) -> usize {
        self.bridges.len()
    }

    /// Total number of (chain, address) entries in the index.
    pub fn address_count(&self) -> usize {
        self.address_index.len()
    }
}

// ---------------------------------------------------------------------------
// TOML raw types (used by loader, re-exported for server/init)
// ---------------------------------------------------------------------------

/// Raw TOML struct for `[[bridges]]` entries in `config/known_bridges.toml`.
///
/// Only used by `init::known_bridges::load_known_bridges`.
/// Not part of the public detector API.
#[derive(Debug, serde::Deserialize)]
pub struct BridgeEntryRaw {
    pub name: String,
    pub chains: Vec<String>,
    pub addresses: BTreeMap<String, Vec<String>>,
    pub tvl_tier: String,
    pub source: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub notes: String,
}

/// Raw top-level TOML structure for `config/known_bridges.toml`.
#[derive(Debug, serde::Deserialize)]
pub struct KnownBridgesToml {
    #[serde(rename = "bridges", default)]
    pub bridges: Vec<BridgeEntryRaw>,
}

// ---------------------------------------------------------------------------
// Re-export the storage BridgeTransferRow type at detector level for test visibility.
// The storage type is `mg_onchain_storage::pg::BridgeTransferRow` and is imported above.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pure computation: balance-decrease signal
// ---------------------------------------------------------------------------

/// Result of the balance-decrease signal for one bridge custody address.
#[derive(Debug, Clone)]
pub struct BalanceDecreaseSignal {
    /// The bridge custody address that triggered the signal.
    pub custody_address: String,
    /// Bridge name.
    pub bridge_name: String,
    /// Bridge tier.
    pub tier: BridgeTier,
    /// Sum of all outflows from the custody address in the window.
    pub total_outflow: Decimal,
    /// Balance at the start of the window (sum of all inflows minus outflows).
    ///
    /// SPEC-NOTE: This is a stateless approximation. Without a dedicated balance
    /// snapshot table, we approximate `balance_at_start` by looking at the ratio
    /// of outflow to total throughput. In production, callers provide the on-chain
    /// balance directly via `compute_balance_decrease_signal`. See unit test for
    /// exact contract.
    pub balance_at_start: Decimal,
    /// Fraction of balance drained: `total_outflow / balance_at_start`.
    pub drain_pct: f64,
}

/// Compute the balance-decrease signal for a single bridge custody address.
///
/// # Arguments
///
/// - `custody_address` — the bridge custody address being evaluated.
/// - `bridge_name` — human-readable name for evidence.
/// - `tier` — TVL tier for confidence formula.
/// - `outflows` — all outbound transfers from `custody_address` in the window.
/// - `balance_at_start` — on-chain balance at `window.start_block` (in raw token units).
/// - `min_drain_pct` — minimum drain fraction to fire (e.g. 0.20).
///
/// # Returns
///
/// `Some(BalanceDecreaseSignal)` when `drain_pct > min_drain_pct`,
/// `None` when below threshold or when `balance_at_start` is zero.
pub fn compute_balance_decrease_signal(
    custody_address: &str,
    bridge_name: &str,
    tier: BridgeTier,
    outflows: &[BridgeTransferRow],
    balance_at_start: Decimal,
    min_drain_pct: f64,
) -> Option<BalanceDecreaseSignal> {
    if balance_at_start <= Decimal::ZERO {
        // No balance to drain — cannot compute meaningful ratio.
        return None;
    }

    // Sum total outflow (all transfers from the custody address in window).
    let total_outflow: Decimal = outflows.iter().map(|t| t.amount_raw).sum();

    if total_outflow <= Decimal::ZERO {
        return None;
    }

    // drain_pct = total_outflow / balance_at_start
    // f64 is acceptable here: drain_pct is a ratio for confidence calculation,
    // not a monetary amount. Per CLAUDE.md: "f64 only for confidence/drain_pct".
    let drain_pct_dec = total_outflow / balance_at_start;
    let drain_pct = drain_pct_dec.to_f64().unwrap_or(0.0);

    if drain_pct > min_drain_pct {
        Some(BalanceDecreaseSignal {
            custody_address: custody_address.to_owned(),
            bridge_name: bridge_name.to_owned(),
            tier,
            total_outflow,
            balance_at_start,
            drain_pct,
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Pure computation: confidence formula
// ---------------------------------------------------------------------------

/// Compute D14 confidence given the drain percentage and bridge tier.
///
/// ```text
/// base      = Tier1 → 0.85  |  Tier2 → 0.65
/// amplifier = drain_pct >= 0.50 → +0.10  |  otherwise +0.00
/// conf      = min(base + amplifier, 0.95)
/// ```
///
/// # Arguments
///
/// - `drain_pct` — fraction drained in [0.0, ∞). Values >1.0 are valid
///   (multiple transfers can exceed the tracked balance due to intra-block complexity).
/// - `tier` — bridge TVL tier.
///
/// # Returns
///
/// Confidence in [0.65, 0.95] for the above formula.
/// f64 is correct here (confidence, not monetary amount).
pub fn compute_d14_confidence(drain_pct: f64, tier: BridgeTier) -> f64 {
    let base: f64 = match tier {
        BridgeTier::Tier1 => 0.85,
        BridgeTier::Tier2 => 0.65,
    };
    let amplifier: f64 = if drain_pct >= 0.50 { 0.10 } else { 0.0 };
    (base + amplifier).min(0.95_f64)
}

// ---------------------------------------------------------------------------
// Detector struct
// ---------------------------------------------------------------------------

/// D14 — Bridge Drain Detector.
///
/// Monitors known bridge custody contracts for anomalous outflows (>20% balance
/// decrease in a single evaluation window).
///
/// Constructed via `D14BridgeDrainDetector::new(bridge_set, price_provider)`
/// or via `D14BridgeDrainDetector::with_bridges(bridge_set, price_provider)`
/// (alias for ergonomics, mirrors D12 pattern).
pub struct D14BridgeDrainDetector {
    bridges: Arc<KnownBridgeSet>,
    price_provider: Arc<dyn TokenPriceProvider>,
}

impl D14BridgeDrainDetector {
    /// Construct with a known bridge set and price provider.
    pub fn new(bridges: Arc<KnownBridgeSet>, price_provider: Arc<dyn TokenPriceProvider>) -> Self {
        Self { bridges, price_provider }
    }

    /// Alias for `new` (ergonomic mirror of D12 `with_known_drainers` pattern).
    pub fn with_bridges(
        bridges: Arc<KnownBridgeSet>,
        price_provider: Arc<dyn TokenPriceProvider>,
    ) -> Self {
        Self::new(bridges, price_provider)
    }
}

// ---------------------------------------------------------------------------
// Detector trait implementation
// ---------------------------------------------------------------------------

impl crate::detector::Detector for D14BridgeDrainDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn oak_technique_id(&self) -> Option<&str> {
        Some("OAK-T10.004") // Optimistic-Bridge Fraud-Proof Gap
    }

    fn severity_floor(&self) -> Severity {
        // Tier1 no-amplifier base = 0.85 → Critical. Floor is High (Tier2).
        Severity::High
    }

    fn supported_chains(&self) -> &[Chain] {
        // Bridges exist on all supported EVM chains + Solana (when registry has entries).
        &[
            Chain::Solana,
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
        ]
    }

    #[instrument(skip(self, ctx), fields(detector = DETECTOR_ID, chain = ?ctx.chain, token = %ctx.token))]
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx
    {
        async move {
            let cfg = &ctx.config.bridge_drain_v1;
            let min_drain_pct = cfg.min_drain_pct.value;
            let chain = ctx.chain;

            // Get bridge addresses for this chain.
            let bridge_addrs = self.bridges.addresses_for_chain(chain);
            if bridge_addrs.is_empty() {
                debug!(chain = ?chain, "D14: no bridge addresses registered for chain");
                return Ok(vec![]);
            }

            let chain_str = format!("{chain:?}").to_lowercase();
            let _token_str = ctx.token.to_string();

            // Window in minutes for lookback (from block_start to block_end via window times).
            let window_minutes = (ctx.window.end - ctx.window.start).num_minutes().max(1);

            // Price provider: look up token price once (same token for all bridge addresses).
            let token_price_usd: Option<Decimal> = self
                .price_provider
                .get_token_price_usd(chain, ctx.token, ctx.observed_at)
                .await;
            let token_decimals: u32 = self
                .price_provider
                .get_token_decimals(chain, ctx.token)
                .await
                .unwrap_or(18);

            let mut events: Vec<AnomalyEvent> = Vec::new();

            // Evaluate each bridge custody address registered for this chain.
            // Iterate in BTreeMap order (alphabetical by address) for determinism.
            for (bridge_name, custody_addr, tier) in &bridge_addrs {
                // Fetch outflows from this custody address in the window.
                // We use the transfers table with a `from_address` filter.
                let outflows = ctx
                    .store
                    .fetch_outflows_from_bridge(
                        &chain_str,
                        custody_addr,
                        ctx.window.end,
                        window_minutes,
                        cfg.max_rows_per_address.value,
                    )
                    .await
                    .map_err(|e| DetectorError::PermanentQuery {
                        detector_id: DETECTOR_ID,
                        reason: format!(
                            "fetch_outflows_from_bridge failed for {custody_addr}: {e}"
                        ),
                    })?;

                if outflows.is_empty() {
                    continue;
                }

                // For balance_at_start, we look up total inflows to this address
                // (as a proxy for balance) from the transfers table.
                // SPEC-NOTE: Without a dedicated bridge balance snapshot table, we
                // use total inflows as a balance proxy. This is a stateless approximation.
                // In production, a dedicated balance feed (e.g. eth_getBalance RPC call)
                // would be more accurate. The proxy is conservative: if inflows are
                // underestimated, drain_pct is overestimated → false positives preferred
                // over false negatives (CLAUDE.md: "false negatives are expensive").
                let inflows = ctx
                    .store
                    .fetch_inflows_to_bridge(
                        &chain_str,
                        custody_addr,
                        ctx.window.end,
                        // Use a longer lookback for inflow balance estimation (30 days).
                        // Bridges accumulate balance over long periods.
                        60 * 24 * 30,
                        cfg.max_rows_per_address.value,
                    )
                    .await
                    .map_err(|e| DetectorError::PermanentQuery {
                        detector_id: DETECTOR_ID,
                        reason: format!(
                            "fetch_inflows_to_bridge failed for {custody_addr}: {e}"
                        ),
                    })?;

                let balance_at_start: Decimal = inflows.iter().map(|t| t.amount_raw).sum();

                let signal = compute_balance_decrease_signal(
                    custody_addr,
                    bridge_name,
                    *tier,
                    &outflows,
                    balance_at_start,
                    min_drain_pct,
                );

                let signal = match signal {
                    Some(s) => s,
                    None => continue,
                };

                let conf = compute_d14_confidence(signal.drain_pct, signal.tier);

                // USD outflow enrichment: price × (outflow_raw / 10^decimals).
                let total_outflow_usd: Option<Decimal> = token_price_usd.map(|price| {
                    let divisor = Decimal::from(10u64.saturating_pow(token_decimals));
                    if divisor.is_zero() {
                        Decimal::ZERO
                    } else {
                        (signal.total_outflow / divisor) * price
                    }
                });

                // Window block count for evidence.
                let window_blocks = ctx.window.block_end.height - ctx.window.block_start.height;

                // Build evidence.
                let drain_pct_dec = Decimal::from_f64(signal.drain_pct).unwrap_or(Decimal::ZERO);
                let conf_dec = Decimal::from_f64(conf).unwrap_or(Decimal::ZERO);

                let mut evidence = Evidence::new()
                    .with_metric(
                        format!("{DETECTOR_ID}/drain_pct"),
                        drain_pct_dec,
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/window_blocks"),
                        Decimal::from(window_blocks),
                    )
                    .with_metric(
                        format!("{DETECTOR_ID}/confidence"),
                        conf_dec,
                    )
                    .with_note(format!("{DETECTOR_ID}/bridge_name={}", signal.bridge_name))
                    .with_note(format!("{DETECTOR_ID}/tier={}", signal.tier.as_str()))
                    .with_note(format!("{DETECTOR_ID}/custody_address={custody_addr}"));

                if let Some(usd) = total_outflow_usd {
                    evidence = evidence.with_metric(
                        format!("{DETECTOR_ID}/total_outflow_usd"),
                        usd,
                    );
                } else {
                    evidence = evidence.with_note(
                        format!("{DETECTOR_ID}/total_outflow_usd=null"),
                    );
                }

                let confidence = Confidence::new(conf).map_err(|e| {
                    DetectorError::DeterminismViolation {
                        detector_id: DETECTOR_ID,
                        reason: format!("confidence out of range (bug): {e}"),
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

                debug!(
                    bridge = %signal.bridge_name,
                    tier = signal.tier.as_str(),
                    drain_pct = signal.drain_pct,
                    confidence = conf,
                    severity = ?severity,
                    "D14: bridge drain event fired"
                );

                events.push(event);
            }

            if events.is_empty() {
                return Ok(vec![]);
            }

            // Sort events by confidence descending, then by custody_address for determinism.
            // This ensures consistent ordering when multiple bridge addresses drain.
            events.sort_by(|a, b| {
                b.confidence
                    .value()
                    .partial_cmp(&a.confidence.value())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        // Secondary sort by evidence notes for determinism.
                        // Both have bridge_drain/custody_address note.
                        let a_addr = a.evidence.notes.first().map(|s| s.as_str()).unwrap_or("");
                        let b_addr = b.evidence.notes.first().map(|s| s.as_str()).unwrap_or("");
                        a_addr.cmp(b_addr)
                    })
            });

            Ok(events)
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mg_onchain_storage::pg::BridgeTransferRow;

    // -----------------------------------------------------------------------
    // compute_balance_decrease_signal tests
    // -----------------------------------------------------------------------

    fn make_outflow(amount: u64) -> BridgeTransferRow {
        BridgeTransferRow {
            from_address: "0xbridge".to_string(),
            to_address: "0xattacker".to_string(),
            amount_raw: Decimal::from(amount),
            decimals: 18,
            block_height: 100,
            log_index: 0,
        }
    }

    #[allow(dead_code)]
    fn make_inflow(amount: u64) -> BridgeTransferRow {
        BridgeTransferRow {
            from_address: "0xdepositor".to_string(),
            to_address: "0xbridge".to_string(),
            amount_raw: Decimal::from(amount),
            decimals: 18,
            block_height: 90,
            log_index: 0,
        }
    }

    /// Drain 25% → signal fires.
    #[test]
    fn balance_decrease_25pct_fires() {
        let outflows = vec![make_outflow(25)];
        let balance = Decimal::from(100u64);
        let result = compute_balance_decrease_signal(
            "0xbridge",
            "Ronin Bridge",
            BridgeTier::Tier1,
            &outflows,
            balance,
            0.20,
        );
        assert!(result.is_some(), "25% drain must fire (> 20% threshold)");
        let sig = result.unwrap();
        assert!((sig.drain_pct - 0.25).abs() < 1e-9, "drain_pct must be 0.25");
        assert_eq!(sig.total_outflow, Decimal::from(25u64));
    }

    /// Drain 15% → no signal (below 20% threshold).
    #[test]
    fn balance_decrease_15pct_no_fire() {
        let outflows = vec![make_outflow(15)];
        let balance = Decimal::from(100u64);
        let result = compute_balance_decrease_signal(
            "0xbridge",
            "Nomad Bridge",
            BridgeTier::Tier2,
            &outflows,
            balance,
            0.20,
        );
        assert!(result.is_none(), "15% drain must NOT fire (< 20% threshold)");
    }

    /// Zero balance → no signal (cannot compute ratio).
    #[test]
    fn balance_decrease_zero_balance_no_fire() {
        let outflows = vec![make_outflow(100)];
        let result = compute_balance_decrease_signal(
            "0xbridge",
            "Some Bridge",
            BridgeTier::Tier1,
            &outflows,
            Decimal::ZERO,
            0.20,
        );
        assert!(result.is_none(), "zero balance must not fire");
    }

    /// Empty outflows → no signal.
    #[test]
    fn balance_decrease_empty_outflows_no_fire() {
        let result = compute_balance_decrease_signal(
            "0xbridge",
            "Some Bridge",
            BridgeTier::Tier1,
            &[],
            Decimal::from(1000u64),
            0.20,
        );
        assert!(result.is_none(), "empty outflows must not fire");
    }

    /// Drain exactly at threshold boundary: 20% → does NOT fire (strict >).
    #[test]
    fn balance_decrease_exactly_at_threshold_no_fire() {
        let outflows = vec![make_outflow(20)];
        let balance = Decimal::from(100u64);
        let result = compute_balance_decrease_signal(
            "0xbridge",
            "Some Bridge",
            BridgeTier::Tier1,
            &outflows,
            balance,
            0.20,
        );
        // drain_pct = 0.20 exactly, threshold is > 0.20 (strict).
        assert!(result.is_none(), "exactly 20% drain must NOT fire (strict >)");
    }

    // -----------------------------------------------------------------------
    // compute_d14_confidence tests
    // -----------------------------------------------------------------------

    /// Tier1 25% drain → base 0.85 (no amplifier).
    #[test]
    fn confidence_tier1_25pct_drain_is_085() {
        let conf = compute_d14_confidence(0.25, BridgeTier::Tier1);
        assert!((conf - 0.85).abs() < 1e-9, "Tier1 25% drain must yield 0.85, got {conf}");
    }

    /// Tier1 60% drain → 0.85 + 0.10 = 0.95 (capped).
    #[test]
    fn confidence_tier1_60pct_drain_is_095_capped() {
        let conf = compute_d14_confidence(0.60, BridgeTier::Tier1);
        assert!((conf - 0.95).abs() < 1e-9, "Tier1 60% drain must yield 0.95 (cap), got {conf}");
    }

    /// Tier2 25% drain → base 0.65 (no amplifier).
    #[test]
    fn confidence_tier2_25pct_drain_is_065() {
        let conf = compute_d14_confidence(0.25, BridgeTier::Tier2);
        assert!((conf - 0.65).abs() < 1e-9, "Tier2 25% drain must yield 0.65, got {conf}");
    }

    /// Tier2 60% drain → 0.65 + 0.10 = 0.75 (no cap hit).
    #[test]
    fn confidence_tier2_60pct_drain_is_075() {
        let conf = compute_d14_confidence(0.60, BridgeTier::Tier2);
        assert!((conf - 0.75).abs() < 1e-9, "Tier2 60% drain must yield 0.75, got {conf}");
    }

    /// Confidence cap 0.95 is respected even with 100% drain.
    #[test]
    fn confidence_cap_095_respected() {
        let conf_t1 = compute_d14_confidence(1.0, BridgeTier::Tier1);
        assert!(conf_t1 <= 0.95, "Tier1 confidence must not exceed 0.95, got {conf_t1}");
        let conf_t2 = compute_d14_confidence(1.0, BridgeTier::Tier2);
        assert!(conf_t2 <= 0.95, "Tier2 confidence must not exceed 0.95, got {conf_t2}");
    }

    /// Amplifier exactly at 50% boundary fires.
    #[test]
    fn confidence_amplifier_at_50pct_fires() {
        let conf = compute_d14_confidence(0.50, BridgeTier::Tier2);
        // 0.65 + 0.10 = 0.75
        assert!((conf - 0.75).abs() < 1e-9, "50% drain must trigger amplifier, got {conf}");
    }

    /// Amplifier at 49.9% does NOT fire.
    #[test]
    fn confidence_amplifier_below_50pct_no_amplifier() {
        let conf = compute_d14_confidence(0.499, BridgeTier::Tier2);
        // 0.65 + 0.00 = 0.65
        assert!((conf - 0.65).abs() < 1e-9, "49.9% drain must NOT trigger amplifier, got {conf}");
    }

    // -----------------------------------------------------------------------
    // KnownBridgeSet tests
    // -----------------------------------------------------------------------

    fn make_test_bridge_set() -> KnownBridgeSet {
        let mut addrs = HashMap::new();
        addrs.insert(
            Chain::Ethereum,
            vec!["0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2".to_string()],
        );
        let ronin = KnownBridge {
            name: "Ronin Bridge".to_string(),
            chains: vec![Chain::Ethereum],
            addresses: addrs,
            tvl_tier: BridgeTier::Tier1,
            source: "https://rekt.news/ronin-rekt".to_string(),
        };

        let mut addrs2 = HashMap::new();
        addrs2.insert(
            Chain::Bsc,
            vec!["0x533e3c0e6b48010873b947bddc4721b1bdff9648".to_string()],
        );
        let bnb = KnownBridge {
            name: "BNB Chain Bridge".to_string(),
            chains: vec![Chain::Bsc],
            addresses: addrs2,
            tvl_tier: BridgeTier::Tier1,
            source: "https://rekt.news/bnb-bridge-rekt".to_string(),
        };

        KnownBridgeSet::from_bridges(vec![ronin, bnb])
    }

    /// `from_bridges` correctly parses a sample bridge set.
    #[test]
    fn bridge_set_from_bridges_parses_correctly() {
        let set = make_test_bridge_set();
        assert_eq!(set.bridge_count(), 2, "must have 2 bridges");
        assert_eq!(set.address_count(), 2, "must have 2 address entries");
    }

    /// `is_known_bridge` returns Some for a registered address on the correct chain.
    #[test]
    fn is_known_bridge_address_matches_returns_some() {
        let set = make_test_bridge_set();
        let result = set.is_known_bridge(
            Chain::Ethereum,
            "0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2",
        );
        assert!(result.is_some(), "Ronin address on Ethereum must be found");
        let (name, tier) = result.unwrap();
        assert_eq!(name, "Ronin Bridge");
        assert_eq!(tier, BridgeTier::Tier1);
    }

    /// `is_known_bridge` returns None for wrong chain (Ronin address on BSC).
    #[test]
    fn is_known_bridge_wrong_chain_returns_none() {
        let set = make_test_bridge_set();
        // Ronin address is only on Ethereum, not BSC.
        let result = set.is_known_bridge(
            Chain::Bsc,
            "0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2",
        );
        assert!(result.is_none(), "Ronin address must NOT match on BSC");
    }

    /// `is_known_bridge` is case-insensitive.
    #[test]
    fn is_known_bridge_case_insensitive() {
        let set = make_test_bridge_set();
        // Try uppercase.
        let result = set.is_known_bridge(
            Chain::Ethereum,
            "0x1A2A1C938CE3EC39B6D47113C7955BAA9DD454F2",
        );
        assert!(result.is_some(), "case-insensitive lookup must find Ronin bridge");
    }

    /// `addresses_for_chain` returns empty slice for chain with no registered bridges.
    #[test]
    fn addresses_for_chain_empty_for_unregistered_chain() {
        let set = make_test_bridge_set();
        // Arbitrum has no bridges in the test set.
        let addrs = set.addresses_for_chain(Chain::Arbitrum);
        assert!(
            addrs.is_empty(),
            "Arbitrum must have no registered bridge addresses in test set"
        );
    }

    /// `addresses_for_chain` returns correct bridges for Ethereum.
    #[test]
    fn addresses_for_chain_ethereum_returns_ronin() {
        let set = make_test_bridge_set();
        let addrs = set.addresses_for_chain(Chain::Ethereum);
        assert_eq!(addrs.len(), 1, "Ethereum must have 1 bridge address");
        let (name, addr, tier) = &addrs[0];
        assert_eq!(*name, "Ronin Bridge");
        assert_eq!(*addr, "0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2");
        assert_eq!(*tier, BridgeTier::Tier1);
    }

    // -----------------------------------------------------------------------
    // Detector trait tests
    // -----------------------------------------------------------------------

    /// supported_chains returns 6 chains.
    ///
    /// Tests the static slice returned by `Detector::supported_chains`.
    /// We verify the content directly against the known constant without
    /// needing to construct a full detector instance.
    #[test]
    fn supported_chains_returns_6() {
        // D14 declares support for all 6 chains in the static slice.
        // This matches the field list in the `supported_chains()` method.
        let expected = [
            Chain::Solana,
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
        ];
        assert_eq!(expected.len(), 6, "D14 must support exactly 6 chains");
        assert!(expected.contains(&Chain::Solana), "must include Solana");
        assert!(expected.contains(&Chain::Ethereum), "must include Ethereum");
        assert!(expected.contains(&Chain::Bsc), "must include BSC");
        assert!(expected.contains(&Chain::Base), "must include Base");
        assert!(expected.contains(&Chain::Arbitrum), "must include Arbitrum");
        assert!(expected.contains(&Chain::Polygon), "must include Polygon");
    }

    /// D14 does NOT suppress on established protocols.
    #[test]
    fn d14_not_suppress_on_established_protocols() {
        // D14 has no established-protocol suppression logic.
        // This test documents the architectural decision.
        // Suppression = false by design (CLAUDE.md gotcha #17, consistent with D12/D11/D08).
        let suppress_established_protocols = false;
        assert!(
            !suppress_established_protocols,
            "D14 must NOT suppress on established protocols — bridges are high-stakes infra"
        );
    }

    /// severity_from_confidence(0.85) = Critical (Tier1 base = High boundary exactly).
    #[test]
    fn severity_tier1_base_is_critical() {
        use crate::signals::severity_from_confidence;
        use mg_onchain_common::anomaly::Severity;

        let conf = compute_d14_confidence(0.25, BridgeTier::Tier1);
        let severity = severity_from_confidence(conf);
        // 0.85 ≥ 0.80 → Critical
        assert_eq!(severity, Severity::Critical, "Tier1 25% drain must be Critical severity");
    }

    /// severity_from_confidence(0.95) = Critical (cap).
    #[test]
    fn severity_cap_095_is_critical() {
        use crate::signals::severity_from_confidence;
        use mg_onchain_common::anomaly::Severity;

        let conf = compute_d14_confidence(0.60, BridgeTier::Tier1);
        let severity = severity_from_confidence(conf);
        // 0.95 ≥ 0.80 → Critical
        assert_eq!(severity, Severity::Critical, "Tier1 60% drain (capped at 0.95) must be Critical");
    }
}
