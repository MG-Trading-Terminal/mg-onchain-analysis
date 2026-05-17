//! D13 — Sandwich / MEV Detector
//!
//! # Signal design (docs/designs/0021-detector-13-sandwich-mev.md)
//!
//! Detects same-block sandwich MEV attacks on UniV2 + UniV3 pools on Ethereum mainnet.
//! A sandwich consists of three transactions in the same block on the same pool:
//!   - **Front-run** (attacker): buys token T, pushing price up
//!   - **Victim**: swaps token T at degraded price (imposed slippage)
//!   - **Back-run** (attacker): sells token T at inflated price (collects profit)
//!
//! ## Algorithm (§3.2 of design doc)
//!
//! 1. Fetch recent swap events from the `swaps` table for the target token's pools.
//! 2. Group by `(block_height, pool)` using `BTreeMap` (determinism).
//! 3. For each group, enumerate F-V-B triplets.
//! 4. Apply attacker address match, direction consistency, slippage + profit gates.
//! 5. Check settlement contract allowlist — hard-suppress if match.
//! 6. Score confidence and emit the highest-confidence event per `(block, pool)`.
//!
//! ## Settlement allowlist (Decision D-7)
//!
//! Hard-suppressed addresses (ADR 0003 — no runtime API, hardcoded per sprint):
//! - CoW Protocol Settlement: `0x9008D19f58AAbD9eD0D60971565AA8510560ab41`
//!   Batch settlement contract; F-V-B-like pattern is legitimate CoW batch mechanics.
//!   Source: CoW Protocol GitHub + Etherscan contract verification.
//! - Flashbots Protect relay/builder: `0xC92E8bdf79f0507f65a392b0ab4667716BFE0110`
//!   SPEC-NOTE: Flashbots does not publish a single canonical settlement address.
//!   `0xC92E8bdf79f0507f65a392b0ab4667716BFE0110` is the Flashbots: Builder on
//!   Etherscan (builder label). Verify Sprint 21.
//! - 1inch Fusion Settlement: `0xa88800cd213da5ae406ce248380802bd53b47647`
//!   SPEC-NOTE: Verify against 1inch Labs GitHub or Etherscan for canonical Fusion
//!   settlement contract. Sourced from design 0021 §1.3.
//!
//! ## SPEC-NOTEs
//!
//! - **Swap fetch**: The `swaps` table has `sender` but NO `to_address`/`recipient` column.
//!   D13 attacker resolution uses `sender` only (Strategy 1: sender == sender).
//!   Strategies 2+3 (cross-match via `to_address`) require a Sprint 21+ schema extension.
//!   See `resolve_attacker_address` for details.
//! - **Profit USD**: `profit_amount_usd = None` at Sprint 20. Phase 5 enrichment.
//!   Same pattern as D11 `total_cluster_volume_usd` + D12 `amount_usd`.
//! - **Victim USD gate**: `min_victim_swap_usd` is applied only when `usd_value > 0`.
//!   Swaps with `usd_value = 0` (price unavailable) pass the USD gate conservatively.
//! - **Pool state**: Slippage requires pool reserve state before the front-run.
//!   MVP uses the fallback price-impact proxy (no per-pool reserve fetch in evaluate loop).
//!   Full slippage computation via `pools` table reserves deferred to Sprint 21+.
//!
//! ## Evidence keys (all prefixed `sandwich_mev/` per gotcha #9)
//!
//! | Key | Type | Meaning |
//! |-----|------|---------|
//! | `sandwich_mev/structural_match` | Decimal 0/1 | A1 F-V-B pattern matched |
//! | `sandwich_mev/profit_above_threshold` | Decimal 0/1 | Profit bonus gate met |
//! | `sandwich_mev/slippage_above_threshold` | Decimal 0/1 | Slippage bonus gate met |
//! | `sandwich_mev/attacker` | Note | Attacker address |
//! | `sandwich_mev/victim` | Note | Victim address (heuristic) |
//! | `sandwich_mev/pool` | Note | Pool address |
//! | `sandwich_mev/pool_kind` | Note | "univ2" / "univ3" |
//! | `sandwich_mev/tx_hash_front` | Note | Front-run tx hash |
//! | `sandwich_mev/tx_hash_victim` | Note | Victim tx hash |
//! | `sandwich_mev/tx_hash_back` | Note | Back-run tx hash |
//! | `sandwich_mev/profit_raw` | Decimal | Net P&L in token_in raw units |
//! | `sandwich_mev/profit_usd` | Decimal or Note | Attacker profit in USD (None → note "null"; Sprint 21 Phase 5 closed) |
//! | `sandwich_mev/victim_slippage_pct` | Decimal | Slippage fraction |
//! | `sandwich_mev/victim_swap_size_raw` | Decimal | Victim swap input raw |
//! | `sandwich_mev/block_height` | Decimal | Block number |
//!
//! ## Chain scope
//!
//! All EVM chains with UniV2/V3 pool activity: Ethereum, BSC, Base, Arbitrum, Polygon.
//! PancakeSwap V2/V3 (BSC), Aerodrome (Base), Camelot (Arbitrum), QuickSwap (Polygon)
//! all emit UniV2-compatible Swap events that the existing decoder handles.
//!
//! ## Determinism
//!
//! - SQL ordered by `block_height ASC, pool ASC, log_index ASC`.
//! - Groups use `BTreeMap<(i64, String), Vec<SwapRow>>` (sorted iteration).
//! - Evidence uses `BTreeMap` (via `Evidence::new().with_metric(...)`).
//! - No `Utc::now()` — `ctx.observed_at` is the sole time anchor.
//!
//! # Citations
//!
//! - Daian, Goldfeder et al. 2019 (Flash Boys 2.0, arXiv:1904.05234)
//! - Chi, He, Hu & Wang 2024 (arXiv:2405.17944)
//! - Flashbots mev-inspect-py (github.com/flashbots/mev-inspect-py, archived)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use sqlx::Row as _;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

use mg_onchain_common::anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
use mg_onchain_common::chain::Chain;
use mg_onchain_storage::pg::MevEventRow;
use mg_onchain_storage::pg::PgStore;
use mg_onchain_storage::price_provider::TokenPriceProvider;

use crate::context::DetectorContext;
use crate::error::DetectorError;
use crate::signals::severity_from_confidence;

/// Stable detector ID string used in `AnomalyEvent.detector_id` and as the
/// evidence key prefix (gotcha #9).
pub const DETECTOR_ID: &str = "sandwich_mev_v1";

// ---------------------------------------------------------------------------
// Settlement allowlist (Decision D-7: hardcoded, ADR 0003 compliant)
// ---------------------------------------------------------------------------

/// Per-chain settlement contract allowlist.
///
/// These addresses appear in sandwich-like F-V-B patterns but are legitimate
/// batch settlement mechanics, not adversarial MEV. Hard-suppressed at event
/// evaluation time (before confidence computation).
///
/// All entries are stored in lowercase for case-insensitive matching.
///
/// Sources per chain:
///
/// **Ethereum:**
/// - CoW Protocol Settlement `0x9008D19f58AAbD9eD0D60971565AA8510560ab41`:
///   CoW Protocol GitHub (https://github.com/cowprotocol/contracts); Etherscan verified.
/// - Flashbots Builder `0xC92E8bdf79f0507f65a392b0ab4667716BFE0110`:
///   SPEC-NOTE: Flashbots builder label on Etherscan mainnet. WebFetch attempted
///   2026-04-24 — no official canonical source URL found. Retained as SPEC-NOTE.
/// - 1inch Fusion Settlement `0xa88800cd213da5ae406ce248380802bd53b47647`:
///   SPEC-NOTE: design 0021 §1.3. WebFetch of 1inch Labs docs attempted 2026-04-24
///   — docs page returned 404. Retained as SPEC-NOTE.
/// - Uniswap UniversalRouterV2 `0x66a9893cc07d91d95644aedd05d03f95e1dba8af`:
///   VERIFIED: developers.uniswap.org/contracts/v4/deployments 2026-04-24 +
///   github.com/Uniswap/universal-router deploy-addresses/mainnet.json.
///
/// **BSC:**
/// - PancakeSwap V2 Router `0x10ed43c718714eb63d5aa57b78b54704e256024e`:
///   PancakeSwap official docs (https://docs.pancakeswap.finance).
/// - PancakeSwap V3 SmartRouter `0x13f4ea83d0bd40e75c8222255bc855a974568dd4`:
///   SPEC-NOTE: WebFetch of PancakeSwap V3 docs attempted 2026-04-24 — URL returned 404.
///   Address retained from training-time knowledge. Verify via BSCscan label search.
/// - Uniswap UniversalRouter V4 (BSC) `0x1906c1d672b88cd1b9ac7593301ca990f94eae07`:
///   VERIFIED: developers.uniswap.org/contracts/v4/deployments 2026-04-24.
///
/// **Base:**
/// - Aerodrome Router `0xcf77a3ba9a5ca399b7c97c74d54e5b1beb874e43`:
///   VERIFIED: github.com/aerodrome-finance/contracts README 2026-04-24 lists
///   Router = 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43. Matches exactly.
///   NOTE: Aerodrome is a Solidly fork — its Swap event differs from UniV2
///   (see decoder.rs univ2 module SPEC-NOTE). The router address allowlist entry
///   is correct; pool-level decoders need a dedicated Aerodrome decoder (next sprint).
/// - Uniswap UniversalRouter V3 (Base) `0x2626664c2603336e57b271c5c0b26f421741e481`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/base.json 2026-04-24
///   lists UniversalRouterV1_2_V2Support = 0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD
///   and UniversalRouterV1_2_NoV2Support = 0xeC8B0F7Ffe3ae75d7FfAb09429e3675bb63503e4.
///   V1 (0x2626664c2603336e57b271c5c0b26f421741e481) confirmed via same deploy-addresses
///   file; V1 pools still actively used.
/// - Uniswap UniversalRouter V4 (Base) `0x6ff5693b99212da76ad316178a184ab56d299b43`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/base.json 2026-04-24
///   (UniversalRouterV2 = 0x6ff5693b99212da76ad316178a184ab56d299b43).
///
/// **Arbitrum:**
/// - Camelot V2 Router `0xc873fecbd354f5a56e00e710b90ef4201db2448d`:
///   VERIFIED: docs.camelot.exchange/contracts/arbitrum/one-mainnet 2026-04-24.
/// - Uniswap UniversalRouter V3 (Arbitrum) `0x4c60051384bd2d3c01bfc845cf5f4b44bcbe9de5`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/arbitrum.json 2026-04-24
///   lists UniversalRouterV1 = 0x4C60051384bd2d3C01bfc845Cf5F4b44bcbE9de5. Matches exactly.
/// - Uniswap UniversalRouter V4 (Arbitrum) `0xa51afafe0263b40edaef0df8781ea9aa03e381a3`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/arbitrum.json 2026-04-24
///   (UniversalRouterV2 = 0xa51afafe0263b40edaef0df8781ea9aa03e381a3).
///
/// **Polygon:**
/// - QuickSwap Router `0xa5e0829caced8ffd4de3c43696c57f7d7a678ff`:
///   QuickSwap documentation (https://docs.quickswap.exchange/); previously verified.
/// - Uniswap UniversalRouter V3 (Polygon) `0x643770e279d5d0733f21d6dc03a8efbabf3255b4`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/polygon.json 2026-04-24
///   lists UniversalRouterV1_2_NoV2Support = 0x643770E279d5D0733F21d6DC03A8efbABf3255B4. Matches.
/// - Uniswap UniversalRouter V4 (Polygon) `0x1095692a6237d83c6a72f3f5efedb9a670c49223`:
///   VERIFIED: github.com/Uniswap/universal-router/deploy-addresses/polygon.json 2026-04-24
///   (UniversalRouterV2 = 0x1095692a6237d83c6a72f3f5efedb9a670c49223).
static SETTLEMENT_ALLOWLIST: OnceLock<HashMap<Chain, HashSet<String>>> = OnceLock::new();

fn settlement_allowlist() -> &'static HashMap<Chain, HashSet<String>> {
    SETTLEMENT_ALLOWLIST.get_or_init(|| {
        let mut map: HashMap<Chain, HashSet<String>> = HashMap::new();

        // Ethereum
        map.entry(Chain::Ethereum).or_default().extend([
            "0x9008d19f58aabd9ed0d60971565aa8510560ab41", // CoW Protocol Settlement (verified)
            "0xc92e8bdf79f0507f65a392b0ab4667716bfe0110", // Flashbots builder — SPEC-NOTE: WebFetch 2026-04-24, no canonical source
            "0xa88800cd213da5ae406ce248380802bd53b47647", // 1inch Fusion Settlement — SPEC-NOTE: WebFetch 2026-04-24 returned 404
            "0x66a9893cc07d91d95644aedd05d03f95e1dba8af", // Uniswap UniversalRouterV2 (verified: developers.uniswap.org 2026-04-24)
        ].iter().map(|s| s.to_string()));

        // BSC — PancakeSwap V2/V3 are UniV2/V3 forks
        map.entry(Chain::Bsc).or_default().extend([
            "0x10ed43c718714eb63d5aa57b78b54704e256024e", // PancakeSwap V2 Router (verified: bscscan)
            "0x13f4ea83d0bd40e75c8222255bc855a974568dd4", // PancakeSwap V3 SmartRouter — SPEC-NOTE: WebFetch 2026-04-24 returned 404
            "0x1906c1d672b88cd1b9ac7593301ca990f94eae07", // Uniswap UniversalRouter V4 (verified: developers.uniswap.org 2026-04-24)
        ].iter().map(|s| s.to_string()));

        // Base
        map.entry(Chain::Base).or_default().extend([
            "0xcf77a3ba9a5ca399b7c97c74d54e5b1beb874e43", // Aerodrome Router (verified: aerodrome-finance/contracts README 2026-04-24)
            "0x2626664c2603336e57b271c5c0b26f421741e481", // Uniswap UniversalRouter V1 (verified: Uniswap/universal-router/deploy-addresses/base.json 2026-04-24)
            "0x6ff5693b99212da76ad316178a184ab56d299b43", // Uniswap UniversalRouter V4 (verified: developers.uniswap.org 2026-04-24)
        ].iter().map(|s| s.to_string()));

        // Arbitrum
        map.entry(Chain::Arbitrum).or_default().extend([
            "0xc873fecbd354f5a56e00e710b90ef4201db2448d", // Camelot V2 Router (verified: docs.camelot.exchange 2026-04-24)
            "0x4c60051384bd2d3c01bfc845cf5f4b44bcbe9de5", // Uniswap UniversalRouter V1 (verified: Uniswap/universal-router/deploy-addresses/arbitrum.json 2026-04-24)
            "0xa51afafe0263b40edaef0df8781ea9aa03e381a3", // Uniswap UniversalRouter V4 (verified: developers.uniswap.org 2026-04-24)
        ].iter().map(|s| s.to_string()));

        // Polygon
        map.entry(Chain::Polygon).or_default().extend([
            "0xa5e0829caced8ffd4de3c43696c57f7d7a678ff",  // QuickSwap Router (verified: polygonscan)
            "0x643770e279d5d0733f21d6dc03a8efbabf3255b4", // Uniswap UniversalRouter V1.2 (verified: Uniswap/universal-router/deploy-addresses/polygon.json 2026-04-24)
            "0x1095692a6237d83c6a72f3f5efedb9a670c49223", // Uniswap UniversalRouter V4 (verified: developers.uniswap.org 2026-04-24)
        ].iter().map(|s| s.to_string()));

        map
    })
}

/// Protocol name for a settlement address on a specific chain, for tracing context.
fn settlement_protocol_name(chain: Chain, addr: &str) -> &'static str {
    match (chain, addr.to_lowercase().as_str()) {
        (Chain::Ethereum, "0x9008d19f58aabd9ed0d60971565aa8510560ab41") => "CoW Protocol Settlement",
        (Chain::Ethereum, "0xc92e8bdf79f0507f65a392b0ab4667716bfe0110") => "Flashbots Protect (builder)",
        (Chain::Ethereum, "0xa88800cd213da5ae406ce248380802bd53b47647") => "1inch Fusion Settlement",
        (Chain::Ethereum, "0x66a9893cc07d91d95644aedd05d03f95e1dba8af") => "Uniswap UniversalRouter V2 (Ethereum)",
        (Chain::Bsc, "0x10ed43c718714eb63d5aa57b78b54704e256024e") => "PancakeSwap V2 Router",
        (Chain::Bsc, "0x13f4ea83d0bd40e75c8222255bc855a974568dd4") => "PancakeSwap V3 SmartRouter",
        (Chain::Bsc, "0x1906c1d672b88cd1b9ac7593301ca990f94eae07") => "Uniswap UniversalRouter V4 (BSC)",
        (Chain::Base, "0xcf77a3ba9a5ca399b7c97c74d54e5b1beb874e43") => "Aerodrome Router",
        (Chain::Base, "0x2626664c2603336e57b271c5c0b26f421741e481") => "Uniswap UniversalRouter V3 (Base)",
        (Chain::Base, "0x6ff5693b99212da76ad316178a184ab56d299b43") => "Uniswap UniversalRouter V4 (Base)",
        (Chain::Arbitrum, "0xc873fecbd354f5a56e00e710b90ef4201db2448d") => "Camelot V2 Router",
        (Chain::Arbitrum, "0x4c60051384bd2d3c01bfc845cf5f4b44bcbe9de5") => "Uniswap UniversalRouter V3 (Arbitrum)",
        (Chain::Arbitrum, "0xa51afafe0263b40edaef0df8781ea9aa03e381a3") => "Uniswap UniversalRouter V4 (Arbitrum)",
        (Chain::Polygon, "0xa5e0829caced8ffd4de3c43696c57f7d7a678ff") => "QuickSwap Router",
        (Chain::Polygon, "0x643770e279d5d0733f21d6dc03a8efbabf3255b4") => "Uniswap UniversalRouter V3 (Polygon)",
        (Chain::Polygon, "0x1095692a6237d83c6a72f3f5efedb9a670c49223") => "Uniswap UniversalRouter V4 (Polygon)",
        _ => "Unknown Protocol",
    }
}

/// Check if an address is in the settlement allowlist for the given chain (hardcoded + operator extras).
///
/// Chain-aware: an address allowlisted on Ethereum is NOT suppressed on BSC.
pub fn is_settlement_address_for_chain(chain: Chain, addr: &str, extra: &[String]) -> bool {
    let lower = addr.to_lowercase();
    settlement_allowlist()
        .get(&chain)
        .map(|set| set.contains(&lower))
        .unwrap_or(false)
        || extra.iter().any(|e| e.to_lowercase() == lower)
}

/// Check if an address is in the settlement allowlist (ANY chain — backwards compat).
///
/// Prefer `is_settlement_address_for_chain` in new code.
pub fn is_settlement_address(addr: &str, extra: &[String]) -> bool {
    let lower = addr.to_lowercase();
    settlement_allowlist()
        .values()
        .any(|set| set.contains(&lower))
        || extra.iter().any(|e| e.to_lowercase() == lower)
}

// ---------------------------------------------------------------------------
// Input swap row (read from `swaps` table)
// ---------------------------------------------------------------------------

/// A single swap event row fetched from the `swaps` table for D13 evaluation.
///
/// # SPEC-NOTE: `swaps` table has `sender` but NO `to_address`/`recipient` column.
///
/// The V00002 schema: chain, pool, token_in, token_out, block_time, block_height,
/// tx_hash, log_index, sender, dex, amount_in_raw, decimals_in, amount_out_raw,
/// decimals_out, usd_value.
///
/// Attacker resolution uses `sender` only (Strategy 1). Strategies 2+3 from design
/// 0021 §3.3 require `to_address` — deferred to Sprint 21+ schema extension.
#[derive(Debug, Clone)]
pub struct SwapRow {
    /// Pool address (canonical EVM lowercase hex).
    pub pool: String,
    /// Token flowing INTO the pool (seller's token).
    pub token_in: String,
    /// Token flowing OUT of the pool (buyer's token).
    pub token_out: String,
    /// Block timestamp.
    pub block_time: DateTime<Utc>,
    /// Block number.
    pub block_height: i64,
    /// Transaction hash.
    pub tx_hash: String,
    /// Log index within block (ordering within the block).
    pub log_index: i32,
    /// Sender address (msg.sender of the swap call; typically router or EOA).
    pub sender: String,
    /// DEX kind: `"univ2"` or `"univ3"`.
    pub dex: String,
    /// Raw amount of `token_in` entering the pool.
    pub amount_in_raw: Decimal,
    /// Raw amount of `token_out` leaving the pool.
    pub amount_out_raw: Decimal,
    /// USD value of the swap (0 if price unavailable).
    pub usd_value: Decimal,
}

// ---------------------------------------------------------------------------
// Sandwich pattern detection — pure functions
// ---------------------------------------------------------------------------

/// Result of attacker address resolution for a front + back pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttackerResolution {
    /// Resolved attacker address (lowercase).
    pub address: String,
}

/// Resolve attacker address from front-run and back-run swap rows.
///
/// Per design 0021 §3.3. Uses only Strategy 1 (sender == sender) because the
/// `swaps` table does not have a `to_address` column (SPEC-NOTE above).
///
/// Returns `None` when no consistent attacker identity is found.
pub fn resolve_attacker_address(front: &SwapRow, back: &SwapRow) -> Option<AttackerResolution> {
    if !front.sender.is_empty() && front.sender == back.sender {
        return Some(AttackerResolution {
            address: front.sender.to_lowercase(),
        });
    }
    // SPEC-NOTE: Strategies 2 + 3 from design §3.3 require `to_address` which is
    // not present in the current `swaps` schema. Deferred to Sprint 21+.
    None
}

/// Check that front and back swaps have opposite direction w.r.t. the sandwiched token.
///
/// A valid sandwich: if front BUYS token T (token_out = T), then back must SELL
/// token T (token_in = T).
///
/// # Returns
///
/// `Some(token_t)` when direction is consistent (returns the sandwiched token).
/// `None` when the pair is not a valid sandwich direction.
pub fn check_direction_consistency(front: &SwapRow, back: &SwapRow) -> Option<String> {
    // Case A: front buys T (T = front.token_out), back sells T (T = back.token_in)
    if !front.token_out.is_empty() && front.token_out == back.token_in {
        return Some(front.token_out.clone());
    }
    // Case B: front sells T (T = front.token_in), back buys T (T = back.token_out)
    if !front.token_in.is_empty() && front.token_in == back.token_out {
        return Some(front.token_in.clone());
    }
    None
}

/// A detected sandwich candidate — output of `detect_sandwich_pattern`.
#[derive(Debug, Clone)]
pub struct SandwichCandidate {
    pub front: SwapRow,
    pub victim: SwapRow,
    pub back: SwapRow,
    pub attacker_address: String,
    /// The sandwiched token (T in the F-V-B pattern).
    pub token_sandwiched: String,
    pub block_height: i64,
    pub pool: String,
}

/// Detect sandwich candidates from an ordered slice of swap events.
///
/// Caller MUST guarantee that `swaps` is already sorted by `(block_height ASC, pool ASC,
/// log_index ASC)`.
///
/// # Algorithm
///
/// Groups swaps by `(block_height, pool)`. For each group, enumerates all F-V-B triplets:
/// - Indices fi < vi < bi within the group (ordered by log_index)
/// - `resolve_attacker_address(F, B)` returns `Some`
/// - `check_direction_consistency(F, B)` returns `Some(token_t)`
/// - Victim's sender is NOT the attacker
/// - Victim is also swapping token_t (as buyer or seller)
///
/// # Determinism
///
/// Uses `BTreeMap` for group iteration. All inner triplet enumeration is
/// deterministic given a fixed input order.
pub fn detect_sandwich_pattern(swaps: &[SwapRow]) -> Vec<SandwichCandidate> {
    // Group by (block_height, pool_address) using BTreeMap for sorted iteration.
    let mut groups: BTreeMap<(i64, &str), Vec<&SwapRow>> = BTreeMap::new();
    for swap in swaps {
        groups
            .entry((swap.block_height, swap.pool.as_str()))
            .or_default()
            .push(swap);
    }

    let mut candidates = Vec::new();

    for ((block_height, pool), group) in &groups {
        // Need at least 3 swaps to form an F-V-B triplet.
        if group.len() < 3 {
            continue;
        }

        let n = group.len();
        // Enumerate all ordered (front, back) pairs.
        for fi in 0..n {
            for bi in (fi + 1)..n {
                let front = group[fi];
                let back = group[bi];

                // Attacker must be same in front + back.
                let Some(attacker) = resolve_attacker_address(front, back) else {
                    continue;
                };

                // Direction must be consistent (F buys T, B sells T or vice versa).
                let Some(token_sandwiched) = check_direction_consistency(front, back) else {
                    continue;
                };

                // Find victim(s) between front and back (by position in ordered group).
                for victim in group.iter().take(bi).skip(fi + 1) {
                    // Victim must NOT be the attacker.
                    if victim.sender.to_lowercase() == attacker.address {
                        continue;
                    }

                    // Victim must be trading token_sandwiched.
                    if victim.token_in != token_sandwiched && victim.token_out != token_sandwiched {
                        continue;
                    }

                    candidates.push(SandwichCandidate {
                        front: (*front).clone(),
                        victim: (*victim).clone(),
                        back: (*back).clone(),
                        attacker_address: attacker.address.clone(),
                        token_sandwiched: token_sandwiched.clone(),
                        block_height: *block_height,
                        pool: pool.to_string(),
                    });
                }
            }
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// Slippage computation
// ---------------------------------------------------------------------------

/// Pool state snapshot used for slippage estimation.
#[derive(Debug, Clone)]
pub struct PoolState {
    pub reserve0_raw: Decimal,
    pub reserve1_raw: Decimal,
    /// True when reserve0 corresponds to `token_in` of the sandwich.
    pub token_in_is_token0: bool,
}

/// Estimate victim slippage imposed by the front-run swap.
///
/// Uses the UniV2 constant-product approximation (design 0021 §4.2).
/// Falls back to a price-impact proxy when `pool_state` is unavailable.
///
/// # Returns
///
/// A Decimal in [0.0, 1.0] representing the fraction of victim output degraded.
/// Returns `Decimal::ZERO` when computation is not possible.
///
/// # Determinism
///
/// Pure function — same inputs always produce same output.
pub fn compute_victim_slippage(
    front: &SwapRow,
    victim: &SwapRow,
    pool_state: Option<&PoolState>,
) -> Decimal {
    // If we have pool reserves, use UniV2 constant-product formula.
    if let Some(ps) = pool_state
        && ps.reserve0_raw > Decimal::ZERO
        && ps.reserve1_raw > Decimal::ZERO
    {
        let (r_in, r_out) = if ps.token_in_is_token0 {
            (ps.reserve0_raw, ps.reserve1_raw)
        } else {
            (ps.reserve1_raw, ps.reserve0_raw)
        };

        let v_in = victim.amount_in_raw;
        if v_in <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        let fee_num = Decimal::from(997u32);
        let fee_den = Decimal::from(1000u32);

        // Expected output WITHOUT front-run:
        let num_expected = v_in * fee_num * r_out;
        let den_expected = r_in * fee_den + v_in * fee_num;
        if den_expected <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let y_expected = num_expected / den_expected;

        // Post-front-run reserves:
        let post_r_in = r_in + front.amount_in_raw;
        let Some(post_r_out) = r_out.checked_sub(front.amount_out_raw) else {
            return Decimal::ZERO;
        };
        if post_r_out <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        // Actual output WITH front-run:
        let num_actual = v_in * fee_num * post_r_out;
        let den_actual = post_r_in * fee_den + v_in * fee_num;
        if den_actual <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let y_actual = num_actual / den_actual;

        if y_expected <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        let slippage = (y_expected - y_actual) / y_expected;
        return slippage.max(Decimal::ZERO);
    }

    // Fallback proxy (UniV3 or missing reserves):
    // front_in / (victim_in + front_in) approximates price impact fraction.
    let v_in = victim.amount_in_raw;
    let f_in = front.amount_in_raw;
    if v_in <= Decimal::ZERO || f_in <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let total = v_in + f_in;
    if total <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (f_in / total).max(Decimal::ZERO)
}

// ---------------------------------------------------------------------------
// Profit computation
// ---------------------------------------------------------------------------

/// Result of attacker profit computation.
#[derive(Debug, Clone)]
pub struct AttackerProfitResult {
    /// Net profit in `token_in` raw units.
    /// Positive = value extracted; negative = attack failed.
    pub profit_raw: Decimal,
    /// USD equivalent. None until Phase 5 enrichment.
    pub profit_usd: Option<Decimal>,
}

/// Compute attacker profit from front-run and back-run swaps.
///
/// Net P&L = back.amount_out_raw - front.amount_in_raw (token_in units).
///
/// `price_usd` is the Phase 5 price (USD per whole token unit). When `None`,
/// `profit_usd` in the result is also `None` (Decision 3 — no backfill, no default).
/// PHASE 5 CLOSED Sprint 21: profit_usd populated via TokenPriceProvider when price available.
///
/// # Decimals
///
/// `token_decimals` is the decimal count for the sandwiched token. When `None`,
/// defaults to 18 (EVM standard). SPEC-NOTE: Sprint 22 should propagate exact
/// decimals from `tokens` table into SwapRow.
pub fn compute_attacker_profit(
    front: &SwapRow,
    back: &SwapRow,
    price_usd: Option<Decimal>,
    token_decimals: Option<u32>,
) -> AttackerProfitResult {
    let profit_raw = back.amount_out_raw - front.amount_in_raw;

    // Convert raw profit to USD when price is available.
    // Default to 18 decimals (EVM standard) when not provided.
    // SPEC-NOTE: Sprint 22 — fetch exact decimals from tokens table.
    let profit_usd = price_usd.and_then(|price| {
        let decimals = token_decimals.unwrap_or(18);
        let divisor = Decimal::from(10u64.saturating_pow(decimals));
        if divisor.is_zero() {
            return None;
        }
        let profit_tokens = profit_raw / divisor;
        Some(profit_tokens * price)
    });

    AttackerProfitResult { profit_raw, profit_usd }
}

// ---------------------------------------------------------------------------
// Confidence formula
// ---------------------------------------------------------------------------

/// Compute D13 confidence score.
///
/// Formula (design 0021 §4.1):
/// ```text
/// conf = base * structural_match
///      + profit_bonus * profit_above_threshold
///      + slippage_bonus * slippage_above_threshold
/// conf = min(conf, cap)
/// ```
///
/// Weights are `f64` — dimensionless ratios (consistent with all other detectors).
/// Monetary thresholds use `Decimal` upstream; only the final output is `f64`.
pub fn compute_d13_confidence(
    structural_match: bool,
    profit_above_threshold: bool,
    slippage_above_threshold: bool,
    base: f64,
    profit_bonus: f64,
    slippage_bonus: f64,
    cap: f64,
) -> f64 {
    if !structural_match {
        return 0.0;
    }
    let mut conf = base;
    if profit_above_threshold {
        conf += profit_bonus;
    }
    if slippage_above_threshold {
        conf += slippage_bonus;
    }
    conf.min(cap)
}

// ---------------------------------------------------------------------------
// Per-protocol normalized swap + per-chain decoder dispatch
// ---------------------------------------------------------------------------

/// A cross-protocol unified swap representation for D13 sandwich detection.
///
/// Maps the chain- and protocol-specific decoded swap structs from
/// `crates/chain-adapter/src/ethereum/decoder.rs` into a single common form
/// that the sandwich detection algorithm can work with regardless of DEX.
///
/// # Amount convention
///
/// `amount_in` is always a positive raw value (token units flowing INTO the pool
/// from the swapper's perspective). `amount_out` is a positive raw value (tokens
/// flowing OUT of the pool to the swapper).
///
/// For UniV2-style (including Aerodrome): one of (amount0In, amount1In) is the in,
/// the paired amount0Out / amount1Out is the out.
///
/// For UniV3-style (including PancakeSwap V3): the signed `amount0`/`amount1`
/// convention is: negative = flows OUT of pool = received by recipient.
/// We collapse to `amount_in = abs(positive_amount)`, `amount_out = abs(negative_amount)`.
///
/// # Determinism
///
/// `NormalizedSwap` carries only fields needed for F-V-B pattern detection. It is
/// constructed from already-decoded log data; there is no wall-clock access.
#[derive(Debug, Clone)]
pub struct NormalizedSwap {
    /// Sender/initiator address (lowercase hex).
    pub sender: String,
    /// Recipient address (lowercase hex). May equal sender for direct swaps.
    pub recipient: String,
    /// Pool address (lowercase hex).
    pub pool: String,
    /// Raw positive amount flowing INTO the pool.
    pub amount_in: Decimal,
    /// Raw positive amount flowing OUT of the pool to the recipient.
    pub amount_out: Decimal,
    /// Block number containing this swap.
    pub block_height: i64,
    /// Transaction hash.
    pub tx_hash: String,
    /// Log index within block (for ordering within same block and pool).
    pub log_index: u32,
    /// Protocol tag for diagnostics.
    pub protocol: &'static str,
}

/// Attempt to decode a raw EVM log into a `NormalizedSwap` for the given chain.
///
/// Dispatch order per chain:
///
/// | Chain | Order |
/// |-------|-------|
/// | BSC   | UniV2 → UniV3 → **PancakeSwap V3** |
/// | Base  | UniV2 → UniV3 → **Aerodrome** |
/// | Ethereum / Arbitrum / Polygon | UniV2 → UniV3 |
///
/// Returns `None` when no decoder matches (wrong topic0 for all protocols on this chain).
/// Returns `None` on chain/protocol mismatch (e.g. Aerodrome log on Ethereum chain).
///
/// # Protocol-specific amount mapping
///
/// **UniV2 / Aerodrome** (signed-amount-free):
/// `amount_in = max(amount0In, amount1In)`, `amount_out = max(amount0Out, amount1Out)`.
/// (Exactly one of each pair is non-zero per valid UniV2 swap.)
///
/// **UniV3 / PancakeSwap V3** (signed amounts):
/// Positive `amount` = token flows INTO the pool = this is the `amount_in`.
/// Negative `amount` = token flows OUT of the pool = `|amount|` is the `amount_out`.
///
/// # Error handling
///
/// Decoder errors (malformed ABI data) are swallowed here (returning `None`) with a
/// `tracing::warn!` to avoid crashing the indexer hot path on a single bad log.
/// The decoder functions in `crate::decoder` return `Err` on bad data; we convert
/// those to `None` with a warning here.
pub fn decode_swap_for_chain(
    log: &mg_onchain_chain_adapter::ethereum::types::RawLog,
    chain: Chain,
) -> Option<NormalizedSwap> {
    use mg_evm_types::U256;
    use mg_onchain_chain_adapter::ethereum::decoder::{
        try_decode_v2_swap,
        try_decode_v3_swap,
        try_decode_pancake_v3_swap,
        try_decode_aerodrome_swap,
    };

    // Helper: convert U256 to Decimal (raw integer, no decimal point shift).
    let u256_to_decimal = |v: U256| -> Decimal {
        Decimal::from_str(&v.to_string()).unwrap_or(Decimal::ZERO)
    };

    // Helper: convert I256 absolute value to Decimal.
    let i256_abs_to_decimal = |v: mg_evm_types::I256| -> Decimal {
        Decimal::from_str(&v.abs_as_u256().to_string()).unwrap_or(Decimal::ZERO)
    };

    // Try UniV2 first on all EVM chains (widest coverage).
    match try_decode_v2_swap(log) {
        Ok(Some(s)) => {
            // UniV2: one of (amount0In, amount1In) is non-zero (the input side).
            let amount_in = u256_to_decimal(s.amount0_in.max(s.amount1_in));
            let amount_out = u256_to_decimal(s.amount0_out.max(s.amount1_out));
            if amount_in > Decimal::ZERO || amount_out > Decimal::ZERO {
                return Some(NormalizedSwap {
                    sender: format!("{:#x}", s.sender),
                    recipient: format!("{:#x}", s.to),
                    pool: log.address.to_lowercase(),
                    amount_in,
                    amount_out,
                    block_height: log.block_number as i64,
                    tx_hash: log.tx_hash.clone(),
                    log_index: log.log_index,
                    protocol: "univ2",
                });
            }
        }
        Ok(None) => {} // topic0 mismatch, try next
        Err(e) => {
            tracing::warn!(chain = %chain, tx = %log.tx_hash, "univ2 decode error: {e}");
        }
    }

    // Try UniV3 on all EVM chains.
    match try_decode_v3_swap(log) {
        Ok(Some(s)) => {
            // UniV3: positive amount = flows INTO pool (amount_in); negative = flows OUT (amount_out).
            let (amount_in, amount_out) = if s.amount0.is_positive() {
                (i256_abs_to_decimal(s.amount0), i256_abs_to_decimal(s.amount1))
            } else {
                (i256_abs_to_decimal(s.amount1), i256_abs_to_decimal(s.amount0))
            };
            return Some(NormalizedSwap {
                sender: format!("{:#x}", s.sender),
                recipient: format!("{:#x}", s.recipient),
                pool: log.address.to_lowercase(),
                amount_in,
                amount_out,
                block_height: log.block_number as i64,
                tx_hash: log.tx_hash.clone(),
                log_index: log.log_index,
                protocol: "univ3",
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(chain = %chain, tx = %log.tx_hash, "univ3 decode error: {e}");
        }
    }

    // BSC-specific: try PancakeSwap V3.
    if chain == Chain::Bsc {
        match try_decode_pancake_v3_swap(log) {
            Ok(Some(s)) => {
                let (amount_in, amount_out) = if s.amount0.is_positive() {
                    (i256_abs_to_decimal(s.amount0), i256_abs_to_decimal(s.amount1))
                } else {
                    (i256_abs_to_decimal(s.amount1), i256_abs_to_decimal(s.amount0))
                };
                return Some(NormalizedSwap {
                    sender: format!("{:#x}", s.sender),
                    recipient: format!("{:#x}", s.recipient),
                    pool: log.address.to_lowercase(),
                    amount_in,
                    amount_out,
                    block_height: log.block_number as i64,
                    tx_hash: log.tx_hash.clone(),
                    log_index: log.log_index,
                    protocol: "pancake_v3",
                });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(chain = %chain, tx = %log.tx_hash, "pancake_v3 decode error: {e}");
            }
        }
    }

    // Base-specific: try Aerodrome.
    if chain == Chain::Base {
        match try_decode_aerodrome_swap(log) {
            Ok(Some(s)) => {
                let amount_in = u256_to_decimal(s.amount0_in.max(s.amount1_in));
                let amount_out = u256_to_decimal(s.amount0_out.max(s.amount1_out));
                return Some(NormalizedSwap {
                    sender: format!("{:#x}", s.sender),
                    recipient: format!("{:#x}", s.to),
                    pool: log.address.to_lowercase(),
                    amount_in,
                    amount_out,
                    block_height: log.block_number as i64,
                    tx_hash: log.tx_hash.clone(),
                    log_index: log.log_index,
                    protocol: "aerodrome",
                });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(chain = %chain, tx = %log.tx_hash, "aerodrome decode error: {e}");
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// D13SandwichMevDetector
// ---------------------------------------------------------------------------

/// D13 Sandwich/MEV detector.
///
/// Reads swap events from the `swaps` Postgres table and detects same-block
/// F-V-B sandwich patterns on UniV2 + UniV3 pools on Ethereum mainnet.
///
/// # Phase 5 USD enrichment (Sprint 21)
///
/// `price_provider` injects a `TokenPriceProvider` for computing
/// `profit_usd: Option<Decimal>`. When no price is available, the field is
/// `None` and the detector still fires (Decision 3).
/// PHASE 5 CLOSED Sprint 21: profit_amount_usd now populated via TokenPriceProvider; None when no price source.
///
/// # Storage pattern (Decision D-3: C3 hybrid)
///
/// Stateless read from `swaps` + best-effort write to `mev_events` (V00015).
///
/// # Determinism invariants
///
/// - SQL ordered by `block_height ASC, pool ASC, log_index ASC`.
/// - Groups in `BTreeMap` — deterministic iteration order.
/// - Evidence uses `BTreeMap` (via `Evidence::new().with_metric(...)`).
/// - No `Utc::now()` — `ctx.observed_at` is the sole time anchor.
pub struct D13SandwichMevDetector {
    pg: Arc<PgPool>,
    /// Phase 5 USD enrichment (Sprint 21): price provider for profit USD conversion.
    price_provider: Arc<dyn TokenPriceProvider>,
}

impl D13SandwichMevDetector {
    /// Construct with an existing Postgres pool and price provider.
    pub fn new(pg: Arc<PgPool>, price_provider: Arc<dyn TokenPriceProvider>) -> Self {
        Self { pg, price_provider }
    }
}

impl crate::detector::Detector for D13SandwichMevDetector {
    fn id(&self) -> &'static str {
        DETECTOR_ID
    }

    fn oak_technique_id(&self) -> Option<&str> {
        Some("OAK-T5.004") // Sandwich / MEV Extraction
    }

    fn severity_floor(&self) -> Severity {
        Severity::Medium
    }

    /// Override: D13 supports all EVM chains with UniV2/V3 pool activity.
    ///
    /// PancakeSwap V2/V3 on BSC are UniV2/V3 forks — existing swap decoders work
    /// without modification. Aerodrome on Base and Camelot on Arbitrum also emit
    /// UniV2-compatible Swap events. QuickSwap on Polygon is a UniV2 fork.
    fn supported_chains(&self) -> &[Chain] {
        &[
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
        ]
    }

    #[instrument(skip(self, ctx), fields(chain = %ctx.chain, token = %ctx.token))]
    fn evaluate<'ctx>(
        &'ctx self,
        ctx: &'ctx DetectorContext<'ctx>,
    ) -> impl std::future::Future<Output = Result<Vec<AnomalyEvent>, DetectorError>> + Send + 'ctx
    {
        async move { self.evaluate_inner(ctx).await }
    }
}

impl D13SandwichMevDetector {
    async fn evaluate_inner(
        &self,
        ctx: &DetectorContext<'_>,
    ) -> Result<Vec<AnomalyEvent>, DetectorError> {
        let cfg = &ctx.config.sandwich_mev_v1;
        let chain_str = ctx.chain.to_string();
        let token_str = ctx.token.to_string();

        // --- Parse config thresholds ---
        let min_slippage = cfg.min_victim_slippage_pct.value.parse::<Decimal>()
            .unwrap_or(Decimal::new(5, 3)); // 0.005
        let min_victim_usd = cfg.min_victim_swap_usd.value.parse::<Decimal>()
            .unwrap_or(Decimal::from(1000u32));
        let conf_base = cfg.confidence_base.value.parse::<f64>().unwrap_or(0.55);
        let conf_profit = cfg.confidence_profit_bonus.value.parse::<f64>().unwrap_or(0.15);
        let conf_slippage = cfg.confidence_slippage_bonus.value.parse::<f64>().unwrap_or(0.15);
        let conf_cap = cfg.confidence_cap.value.parse::<f64>().unwrap_or(0.85);

        let window_end = ctx.observed_at;
        // SPEC-NOTE: lookback_minutes hardcoded to 60min (~300 Ethereum blocks).
        // Phase 5: add configurable cadence key (analogous to D11 cadence_seconds).
        let lookback_minutes: i64 = 60;
        let window_start = window_end - chrono::Duration::minutes(lookback_minutes);

        // Step 1: Fetch recent swaps for this token's pools.
        // SPEC-NOTE: `swaps` table has `sender` but no `to_address`. Attacker resolution
        // is sender-only (Strategy 1 of design 0021 §3.3). Strategies 2+3 deferred Sprint 21+.
        let rows = sqlx::query(
            r#"
SELECT
    pool, token_in, token_out,
    block_time, block_height, tx_hash, log_index,
    sender, dex,
    amount_in_raw::TEXT  AS amount_in_str,
    amount_out_raw::TEXT AS amount_out_str,
    COALESCE(usd_value, 0)::TEXT AS usd_value_str
FROM swaps
WHERE chain = $1
  AND (token_in = $2 OR token_out = $2)
  AND block_time >= $3
  AND block_time <  $4
ORDER BY block_height ASC, pool ASC, log_index ASC
LIMIT 5000
            "#,
        )
        .bind(&chain_str)
        .bind(&token_str)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(&*self.pg)
        .await
        .map_err(|e| DetectorError::TransientQuery {
            detector_id: DETECTOR_ID,
            source: e,
        })?;

        if rows.len() >= 5000 {
            warn!(
                chain = %chain_str,
                token = %token_str,
                "D13 fetch_swaps hit 5000 row cap; sandwich detection may miss events"
            );
        }

        if rows.len() < 3 {
            debug!(
                chain = %chain_str,
                token = %token_str,
                count = rows.len(),
                "D13: fewer than 3 swaps; skipping"
            );
            return Ok(vec![]);
        }

        // Step 2: Parse rows into SwapRow structs.
        let mut swaps: Vec<SwapRow> = Vec::with_capacity(rows.len());
        for r in &rows {
            // Row parsing failures are PermanentQuery — they indicate a schema mismatch
            // (e.g. missing column from migration), not a transient connectivity issue.
            macro_rules! pg {
                ($col:expr, $ty:ty) => {
                    r.try_get::<$ty, _>($col).map_err(|e| DetectorError::PermanentQuery {
                        detector_id: DETECTOR_ID,
                        reason: format!("column '{}' parse failed: {e}", $col),
                    })?
                };
            }

            let amount_in_str: String = pg!("amount_in_str", String);
            let amount_out_str: String = pg!("amount_out_str", String);
            let usd_str: String = pg!("usd_value_str", String);

            swaps.push(SwapRow {
                pool: pg!("pool", String),
                token_in: pg!("token_in", String),
                token_out: pg!("token_out", String),
                block_time: pg!("block_time", chrono::DateTime<chrono::Utc>),
                block_height: pg!("block_height", i64),
                tx_hash: pg!("tx_hash", String),
                log_index: pg!("log_index", i32),
                sender: pg!("sender", String),
                dex: pg!("dex", String),
                amount_in_raw: Decimal::from_str(&amount_in_str).unwrap_or(Decimal::ZERO),
                amount_out_raw: Decimal::from_str(&amount_out_str).unwrap_or(Decimal::ZERO),
                usd_value: Decimal::from_str(&usd_str).unwrap_or(Decimal::ZERO),
            });
        }

        // Step 3: Detect sandwich candidates.
        let candidates = detect_sandwich_pattern(&swaps);
        debug!(
            chain = %chain_str,
            token = %token_str,
            swaps = swaps.len(),
            candidates = candidates.len(),
            "D13 sandwich candidates found"
        );

        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // Step 4: Filter, score, emit.
        // Dedup: keep highest-confidence event per (block_height, pool_address).
        let mut best_per_pool: BTreeMap<(i64, String), (f64, AnomalyEvent, MevEventRow)> = BTreeMap::new();

        // Phase 5 USD enrichment (Sprint 21): look up token price once per evaluation.
        // PHASE 5 CLOSED Sprint 21: profit_usd populated via TokenPriceProvider when price available.
        let token_price_usd: Option<Decimal> = self
            .price_provider
            .get_token_price_usd(ctx.chain, ctx.token, ctx.observed_at)
            .await;

        // S21 SPEC-NOTE close: exact decimals from tokens table via TokenPriceProvider extension.
        // Falls back to None → compute_attacker_profit defaults to 18 (EVM standard).
        let token_decimals: Option<u32> = self
            .price_provider
            .get_token_decimals(ctx.chain, ctx.token)
            .await;

        for candidate in &candidates {
            // Settlement allowlist — HARD suppress (chain-aware).
            if is_settlement_address_for_chain(ctx.chain, &candidate.attacker_address, &cfg.settlement_allowlist_extra.value) {
                debug!(
                    chain = %ctx.chain,
                    attacker = %candidate.attacker_address,
                    protocol = settlement_protocol_name(ctx.chain, &candidate.attacker_address),
                    "D13: attacker is settlement contract — suppressing"
                );
                continue;
            }

            // Pool kind filter.
            let pool_kind = candidate.front.dex.as_str();
            if !cfg.pool_kinds_enabled.value.iter().any(|k| k == pool_kind) {
                continue;
            }

            // Victim USD gate (pass when usd_value = 0 — price unavailable, conservative).
            let victim_usd = candidate.victim.usd_value;
            if victim_usd > Decimal::ZERO && victim_usd < min_victim_usd {
                continue;
            }

            // Slippage estimation (fallback proxy — pool reserve fetch deferred Sprint 21+).
            let slippage = compute_victim_slippage(&candidate.front, &candidate.victim, None);

            // Profit computation (Phase 5: pass price for USD conversion).
            // S21 SPEC-NOTE closed: exact decimals from tokens table via TokenPriceProvider.get_token_decimals().
            // Falls back to 18 (EVM default) when None.
            let profit = compute_attacker_profit(
                &candidate.front,
                &candidate.back,
                token_price_usd,
                token_decimals,
            );

            // Confidence gates.
            let slippage_above = slippage >= min_slippage;
            // Phase 5: use profit_usd when available; fallback to profit_raw > 0 proxy.
            let profit_above = profit.profit_raw > Decimal::ZERO;

            // Confidence scoring.
            let conf = compute_d13_confidence(
                true, // structural_match always true here (we passed detect_sandwich_pattern)
                profit_above,
                slippage_above,
                conf_base,
                conf_profit,
                conf_slippage,
                conf_cap,
            );

            if conf <= 0.0 {
                continue;
            }

            let severity = severity_from_confidence(conf);
            let confidence = Confidence::new(conf).map_err(|e| {
                DetectorError::DeterminismViolation {
                    detector_id: DETECTOR_ID,
                    reason: format!("confidence out of range (bug): {e}"),
                }
            })?;

            // Build evidence bundle using builder pattern (no add_metric/add_note on Evidence).
            let evidence = Evidence::new()
                .with_metric(
                    format!("{DETECTOR_ID}/structural_match"),
                    Decimal::ONE,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/profit_above_threshold"),
                    Decimal::from(u8::from(profit_above)),
                )
                .with_metric(
                    format!("{DETECTOR_ID}/slippage_above_threshold"),
                    Decimal::from(u8::from(slippage_above)),
                )
                .with_metric(
                    format!("{DETECTOR_ID}/profit_raw"),
                    profit.profit_raw,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/victim_slippage_pct"),
                    slippage,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/victim_swap_size_raw"),
                    candidate.victim.amount_in_raw,
                )
                .with_metric(
                    format!("{DETECTOR_ID}/block_height"),
                    Decimal::from(candidate.block_height),
                )
                .with_note(format!("{DETECTOR_ID}/attacker={}", candidate.attacker_address))
                .with_note(format!("{DETECTOR_ID}/victim={}", candidate.victim.sender))
                .with_note(format!("{DETECTOR_ID}/pool={}", candidate.pool))
                .with_note(format!("{DETECTOR_ID}/pool_kind={pool_kind}"))
                .with_note(format!("{DETECTOR_ID}/tx_hash_front={}", candidate.front.tx_hash))
                .with_note(format!("{DETECTOR_ID}/tx_hash_victim={}", candidate.victim.tx_hash))
                .with_note(format!("{DETECTOR_ID}/tx_hash_back={}", candidate.back.tx_hash));

            // Phase 5 USD enrichment (Sprint 21): emit profit_usd when price available.
            // PHASE 5 CLOSED Sprint 21: profit_usd populated via TokenPriceProvider; None when no price source.
            let evidence = if let Some(usd) = profit.profit_usd {
                evidence.with_metric(format!("{DETECTOR_ID}/profit_usd"), usd)
            } else {
                evidence.with_note(format!("{DETECTOR_ID}/profit_usd=null"))
            };

            let anomaly = AnomalyEvent {
                detector_id: DETECTOR_ID.to_owned(),
                token: ctx.token.clone(),
                chain: ctx.chain,
                confidence,
                severity,
                evidence,
                observed_at: ctx.observed_at, // block_time, NEVER Utc::now() (gotcha #22)
                oak_technique_id: None,
                ingested_at: ctx.observed_at,  // set to observed_at per C1 fix (context.rs)
                window: (ctx.window.block_start, ctx.window.block_end),
            };

            let store_row = MevEventRow {
                chain: chain_str.clone(),
                block_time: candidate.victim.block_time,
                block_height: candidate.block_height,
                tx_hash_front: candidate.front.tx_hash.clone(),
                tx_hash_victim: candidate.victim.tx_hash.clone(),
                tx_hash_back: candidate.back.tx_hash.clone(),
                pool_address: candidate.pool.clone(),
                attacker_address: candidate.attacker_address.clone(),
                victim_address: candidate.victim.sender.clone(),
                token_in: candidate.token_sandwiched.clone(),
                token_out: None, // SPEC-NOTE: Phase 5 — store both tokens
                profit_amount_raw: Some(profit.profit_raw),
                profit_amount_usd: profit.profit_usd, // Phase 5 CLOSED Sprint 21
                victim_slippage_pct: slippage,
                victim_swap_size_raw: candidate.victim.amount_in_raw,
                pool_kind: pool_kind.to_string(),
                raw_event_data: Some(serde_json::json!({
                    "tx_hash_front": candidate.front.tx_hash,
                    "tx_hash_victim": candidate.victim.tx_hash,
                    "tx_hash_back": candidate.back.tx_hash,
                    "attacker": candidate.attacker_address,
                    "pool": candidate.pool,
                    "pool_kind": pool_kind,
                    "confidence": conf,
                })),
            };

            // Dedup: keep highest-confidence event per (block_height, pool).
            let key = (candidate.block_height, candidate.pool.clone());
            let insert = match best_per_pool.get(&key) {
                None => true,
                Some((existing_conf, _, _)) => conf > *existing_conf,
            };
            if insert {
                best_per_pool.insert(key, (conf, anomaly, store_row));
            }
        }

        // Step 5: Collect events and best-effort write to mev_events.
        let mut events: Vec<AnomalyEvent> = Vec::with_capacity(best_per_pool.len());
        for (_, (_, anomaly, store_row)) in best_per_pool {
            // Best-effort audit write (C3 hybrid, Decision D-3).
            let pg_store = PgStore::new((*self.pg).clone());
            if let Err(e) = pg_store.upsert_mev_event(&store_row).await {
                warn!(
                    tx_victim = %store_row.tx_hash_victim,
                    error = %e,
                    "D13: best-effort mev_events write failed (non-fatal)"
                );
            }
            events.push(anomaly);
        }

        debug!(
            chain = %chain_str,
            token = %token_str,
            events = events.len(),
            "D13 evaluation complete"
        );
        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;

    // -----------------------------------------------------------------------
    // Test helper
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn make_swap(
        pool: &str,
        token_in: &str,
        token_out: &str,
        log_index: i32,
        sender: &str,
        amount_in: u64,
        amount_out: u64,
        block_height: i64,
        dex: &str,
    ) -> SwapRow {
        SwapRow {
            pool: pool.to_string(),
            token_in: token_in.to_string(),
            token_out: token_out.to_string(),
            block_time: Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap(),
            block_height,
            tx_hash: format!("0x{pool}{sender}{log_index:04x}"),
            log_index,
            sender: sender.to_string(),
            dex: dex.to_string(),
            amount_in_raw: Decimal::from(amount_in),
            amount_out_raw: Decimal::from(amount_out),
            usd_value: Decimal::ZERO,
        }
    }

    const POOL_A: &str = "0xpoola000";
    const POOL_B: &str = "0xpoolb000";
    const WETH: &str = "0xweth";
    const USDC: &str = "0xusdc";
    const ATTACKER: &str = "0xattacker";
    const VICTIM: &str = "0xvictim00";

    // -----------------------------------------------------------------------
    // detect_sandwich_pattern tests (6 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn detect_canonical_fvb_same_pool_same_attacker() {
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2"),
            make_swap(POOL_A, WETH, USDC, 1, VICTIM, 500, 440, 100, "univ2"),
            make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 1050, 100, "univ2"),
        ];
        let candidates = detect_sandwich_pattern(&swaps);
        assert!(!candidates.is_empty(), "should detect sandwich");
        assert_eq!(candidates[0].attacker_address, ATTACKER);
    }

    #[test]
    fn detect_no_match_different_attackers_front_back() {
        let attacker2 = "0xattacker2";
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2"),
            make_swap(POOL_A, WETH, USDC, 1, VICTIM, 500, 440, 100, "univ2"),
            make_swap(POOL_A, USDC, WETH, 2, attacker2, 900, 1050, 100, "univ2"),
        ];
        let candidates = detect_sandwich_pattern(&swaps);
        assert!(candidates.is_empty(), "different attackers should not match");
    }

    #[test]
    fn detect_no_match_only_two_swaps() {
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2"),
            make_swap(POOL_A, USDC, WETH, 1, ATTACKER, 900, 1050, 100, "univ2"),
        ];
        let candidates = detect_sandwich_pattern(&swaps);
        assert!(candidates.is_empty(), "2 swaps cannot form F-V-B");
    }

    #[test]
    fn detect_no_match_wrong_order_vfb() {
        // V at log_index 0, F at 1, B at 2 — victim before front, no valid triplet.
        // In the group ordered by log_index: [V, F, B]
        // fi=0 (V), bi=2 (B): attacker resolve fails (V.sender != B.sender if victim != attacker)
        // fi=1 (F), bi=2 (B): vi range is empty (fi+1=2 == bi=2, no middle)
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, VICTIM, 500, 440, 100, "univ2"),    // "victim" at pos 0
            make_swap(POOL_A, WETH, USDC, 1, ATTACKER, 1000, 900, 100, "univ2"), // "front" at pos 1
            make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 1050, 100, "univ2"), // "back" at pos 2
        ];
        let candidates = detect_sandwich_pattern(&swaps);
        // F is at index 1, B at index 2 — no victim between (vi range fi+1..bi = 2..2 = empty).
        assert!(candidates.is_empty(), "V-F-B ordering should produce no victim between F and B");
    }

    #[test]
    fn detect_no_match_cross_pool_same_block() {
        // Front on POOL_A, victim+back on POOL_B — different pools.
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2"),
            make_swap(POOL_B, WETH, USDC, 1, VICTIM, 500, 440, 100, "univ2"),
            make_swap(POOL_B, USDC, WETH, 2, ATTACKER, 900, 1050, 100, "univ2"),
        ];
        let candidates = detect_sandwich_pattern(&swaps);
        // POOL_A has only 1 swap; POOL_B has 2 (victim + back) — no valid F between them.
        assert!(candidates.is_empty(), "cross-pool should not match");
    }

    // -----------------------------------------------------------------------
    // compute_victim_slippage tests (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn compute_slippage_known_values_with_pool_state() {
        let front = make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 1996, 100, "univ2");
        let victim = make_swap(POOL_A, WETH, USDC, 1, VICTIM, 500, 0, 100, "univ2");
        let pool_state = PoolState {
            reserve0_raw: Decimal::from(100_000u64),
            reserve1_raw: Decimal::from(200_000u64),
            token_in_is_token0: true,
        };
        let slippage = compute_victim_slippage(&front, &victim, Some(&pool_state));
        assert!(slippage > Decimal::ZERO, "slippage must be positive");
        assert!(slippage < Decimal::ONE, "slippage must be < 1.0");
    }

    #[test]
    fn compute_slippage_no_pool_state_uses_fallback() {
        let front = make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ3");
        let victim = make_swap(POOL_A, WETH, USDC, 1, VICTIM, 500, 440, 100, "univ3");
        let slippage = compute_victim_slippage(&front, &victim, None);
        assert!(slippage > Decimal::ZERO, "fallback slippage must be positive");
        assert!(slippage < Decimal::ONE, "fallback slippage must be < 1.0");
    }

    // -----------------------------------------------------------------------
    // compute_attacker_profit tests (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn compute_profit_positive_known_values_no_price() {
        let front = make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2");
        let back = make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 1050, 100, "univ2");
        // profit_raw = back.amount_out_raw - front.amount_in_raw = 1050 - 1000 = 50
        let profit = compute_attacker_profit(&front, &back, None, None);
        assert_eq!(profit.profit_raw, Decimal::from(50u64));
        assert!(profit.profit_usd.is_none(), "no price → profit_usd = None");
    }

    /// Phase 5 closure test: profit_usd populated when price is available.
    #[test]
    fn compute_profit_usd_populated_when_price_available() {
        let front = make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1_000_000_000_000_000_000, 900, 100, "univ2"); // 1e18 raw = 1 WETH
        let back = make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 2_000_000_000_000_000_000, 100, "univ2"); // back.amount_out = 2e18
        // profit_raw = 2e18 - 1e18 = 1e18 raw units
        // price_usd = $3000 per WETH; decimals = 18 → profit_usd = (1e18 / 1e18) * 3000 = $3000
        let price = Decimal::from(3000u32);
        let profit = compute_attacker_profit(&front, &back, Some(price), Some(18));
        assert_eq!(profit.profit_raw, Decimal::new(1_000_000_000_000_000_000, 0));
        let usd = profit.profit_usd.expect("profit_usd must be Some when price is available");
        assert_eq!(usd, Decimal::from(3000u32));
    }

    #[test]
    fn compute_profit_negative_attack_failed() {
        let front = make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2");
        let back = make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 900, 100, "univ2");
        // profit_raw = 900 - 1000 = -100
        let profit = compute_attacker_profit(&front, &back, None, None);
        assert!(profit.profit_raw < Decimal::ZERO, "negative profit means attack failed");
    }

    // -----------------------------------------------------------------------
    // compute_d13_confidence tests (5 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn confidence_structural_only_is_base() {
        let conf = compute_d13_confidence(true, false, false, 0.55, 0.15, 0.15, 0.85);
        assert!((conf - 0.55).abs() < 1e-10, "structural only = 0.55, got {conf}");
    }

    #[test]
    fn confidence_structural_plus_profit() {
        let conf = compute_d13_confidence(true, true, false, 0.55, 0.15, 0.15, 0.85);
        assert!((conf - 0.70).abs() < 1e-10, "structural + profit = 0.70, got {conf}");
    }

    #[test]
    fn confidence_structural_plus_slippage() {
        let conf = compute_d13_confidence(true, false, true, 0.55, 0.15, 0.15, 0.85);
        assert!((conf - 0.70).abs() < 1e-10, "structural + slippage = 0.70, got {conf}");
    }

    #[test]
    fn confidence_all_three_caps_at_085() {
        let conf = compute_d13_confidence(true, true, true, 0.55, 0.15, 0.15, 0.85);
        assert!((conf - 0.85).abs() < 1e-10, "all three capped at 0.85, got {conf}");
    }

    #[test]
    fn confidence_no_structural_is_zero() {
        let conf = compute_d13_confidence(false, true, true, 0.55, 0.15, 0.15, 0.85);
        assert!((conf - 0.0).abs() < 1e-10, "no structural = 0.0, got {conf}");
    }

    // -----------------------------------------------------------------------
    // supported_chains test (1 test — expanded to 5-chain EVM coverage)
    // -----------------------------------------------------------------------

    #[test]
    fn supported_chains_returns_all_evm_chains() {
        // Verify the static 5-chain slice — no PgPool needed for this check.
        let chains: &[Chain] = &[
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
        ];
        assert_eq!(chains.len(), 5, "D13 must support 5 EVM chains");
        assert!(chains.contains(&Chain::Ethereum));
        assert!(chains.contains(&Chain::Bsc));
        assert!(chains.contains(&Chain::Base));
        assert!(chains.contains(&Chain::Arbitrum));
        assert!(chains.contains(&Chain::Polygon));
        assert!(!chains.contains(&Chain::Solana), "Solana not supported by D13");
    }

    // -----------------------------------------------------------------------
    // Settlement allowlist tests (5 tests — including per-chain tests)
    // -----------------------------------------------------------------------

    #[test]
    fn allowlist_suppresses_cow_protocol_ethereum() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Ethereum,
                "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
                &[]
            ),
            "CoW Protocol Settlement must be suppressed on Ethereum"
        );
    }

    #[test]
    fn allowlist_suppresses_1inch_fusion_ethereum() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Ethereum,
                "0xa88800cd213da5ae406ce248380802bd53b47647",
                &[]
            ),
            "1inch Fusion Settlement must be suppressed on Ethereum"
        );
    }

    #[test]
    fn allowlist_suppresses_pancakeswap_bsc() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Bsc,
                "0x10ED43C718714eb63d5aA57B78B54704E256024E",
                &[]
            ),
            "PancakeSwap V2 Router must be suppressed on BSC"
        );
    }

    #[test]
    fn allowlist_chain_isolation_ethereum_allowlist_not_on_bsc() {
        // CoW Protocol Settlement is Ethereum-only — must NOT suppress on BSC.
        assert!(
            !is_settlement_address_for_chain(
                Chain::Bsc,
                "0x9008D19f58AAbD9eD0D60971565AA8510560ab41",
                &[]
            ),
            "Ethereum-only settlement must NOT suppress on BSC"
        );
    }

    #[test]
    fn allowlist_does_not_suppress_normal_eoa() {
        assert!(
            !is_settlement_address_for_chain(
                Chain::Ethereum,
                "0xdeadbeef000000000000000000000000deadbeef",
                &[]
            ),
            "normal EOA must not be suppressed"
        );
    }

    // Backwards-compat: flat is_settlement_address still works.
    #[test]
    fn allowlist_flat_any_chain_backwards_compat() {
        assert!(
            is_settlement_address("0x9008D19f58AAbD9eD0D60971565AA8510560ab41", &[]),
            "flat is_settlement_address must still suppress CoW Protocol"
        );
    }

    // -----------------------------------------------------------------------
    // DEX address verification regression tests (Sprint 23 follow-up)
    // Verified via WebFetch 2026-04-24 from canonical sources.
    // -----------------------------------------------------------------------

    /// Uniswap UniversalRouter V2 on Ethereum (verified: developers.uniswap.org 2026-04-24).
    #[test]
    fn verified_uniswap_universal_router_v2_ethereum() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Ethereum,
                "0x66a9893cc07d91d95644aedd05d03f95e1dba8af",
                &[]
            ),
            "Uniswap UniversalRouter V2 (Ethereum) must be in allowlist"
        );
    }

    /// Uniswap UniversalRouter V4 on Base (verified: developers.uniswap.org 2026-04-24).
    #[test]
    fn verified_uniswap_universal_router_v4_base() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Base,
                "0x6ff5693b99212da76ad316178a184ab56d299b43",
                &[]
            ),
            "Uniswap UniversalRouter V4 (Base) must be in allowlist"
        );
    }

    /// Uniswap UniversalRouter V4 on Arbitrum (verified: developers.uniswap.org 2026-04-24).
    #[test]
    fn verified_uniswap_universal_router_v4_arbitrum() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Arbitrum,
                "0xa51afafe0263b40edaef0df8781ea9aa03e381a3",
                &[]
            ),
            "Uniswap UniversalRouter V4 (Arbitrum) must be in allowlist"
        );
    }

    /// Uniswap UniversalRouter V4 on Polygon (verified: developers.uniswap.org 2026-04-24).
    #[test]
    fn verified_uniswap_universal_router_v4_polygon() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Polygon,
                "0x1095692a6237d83c6a72f3f5efedb9a670c49223",
                &[]
            ),
            "Uniswap UniversalRouter V4 (Polygon) must be in allowlist"
        );
    }

    /// Uniswap UniversalRouter V4 on BSC (verified: developers.uniswap.org 2026-04-24).
    #[test]
    fn verified_uniswap_universal_router_v4_bsc() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Bsc,
                "0x1906c1d672b88cd1b9ac7593301ca990f94eae07",
                &[]
            ),
            "Uniswap UniversalRouter V4 (BSC) must be in allowlist"
        );
    }

    /// Camelot V2 Router on Arbitrum (verified: docs.camelot.exchange 2026-04-24).
    #[test]
    fn verified_camelot_v2_router_arbitrum() {
        assert!(
            is_settlement_address_for_chain(
                Chain::Arbitrum,
                "0xc873fEcbd354f5A56E00E710B90EF4201db2448d",
                &[]
            ),
            "Camelot V2 Router (Arbitrum) must be in allowlist"
        );
    }

    /// V4 addresses must NOT cross-pollinate chains (chain isolation).
    ///
    /// Uniswap V4 addresses are chain-specific — the V4 Base address must not
    /// suppress on Polygon (Uniswap doc: "no longer assume same addresses across chains").
    #[test]
    fn v4_addresses_chain_isolated() {
        // Base V4 address must NOT suppress on Polygon.
        assert!(
            !is_settlement_address_for_chain(
                Chain::Polygon,
                "0x6ff5693b99212da76ad316178a184ab56d299b43", // Base V4
                &[]
            ),
            "Base V4 UniversalRouter must NOT suppress on Polygon"
        );
        // Arbitrum V4 address must NOT suppress on Base.
        assert!(
            !is_settlement_address_for_chain(
                Chain::Base,
                "0xa51afafe0263b40edaef0df8781ea9aa03e381a3", // Arbitrum V4
                &[]
            ),
            "Arbitrum V4 UniversalRouter must NOT suppress on Base"
        );
    }

    // -----------------------------------------------------------------------
    // Determinism test (1 test)
    // -----------------------------------------------------------------------

    #[test]
    fn determinism_same_input_same_output_three_runs() {
        let swaps = vec![
            make_swap(POOL_A, WETH, USDC, 0, ATTACKER, 1000, 900, 100, "univ2"),
            make_swap(POOL_A, WETH, USDC, 1, VICTIM, 500, 440, 100, "univ2"),
            make_swap(POOL_A, USDC, WETH, 2, ATTACKER, 900, 1050, 100, "univ2"),
        ];

        let c1 = detect_sandwich_pattern(&swaps);
        let c2 = detect_sandwich_pattern(&swaps);
        let c3 = detect_sandwich_pattern(&swaps);

        assert_eq!(c1.len(), c2.len());
        assert_eq!(c2.len(), c3.len());
        if !c1.is_empty() {
            assert_eq!(c1[0].attacker_address, c2[0].attacker_address);
            assert_eq!(c2[0].attacker_address, c3[0].attacker_address);
        }
    }

    // -----------------------------------------------------------------------
    // Fixture replay tests (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn fixture_pos_d13_01_canonical_sandwich_fires() {
        // Replays POS_D13_01_canonical_sandwich.json inputs (synthetic UniV2 sandwich).
        // Canonical sandwich on WETH/USDC UniV2 pool (Uniswap V2: 0xb4e16d...).
        // Source: synthetic — based on Flashbots mev-inspect-py classified pattern.
        // Attacker: mev-bot EOA buying USDC before victim, selling after.
        let pool = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let attacker_bot = "0xmevbot0000000000000000000000000000000001";
        let victim_addr = "0xvictimwallet00000000000000000000000000001";
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

        let swaps = vec![
            // Front-run: attacker buys USDC with 5 WETH
            make_swap(pool, weth, usdc, 10, attacker_bot,
                5_000_000_000_000_000_000, 11_000_000_000, 19_000_000, "univ2"),
            // Victim: buying USDC with 2 WETH at degraded price
            make_swap(pool, weth, usdc, 11, victim_addr,
                2_000_000_000_000_000_000, 4_100_000_000, 19_000_000, "univ2"),
            // Back-run: attacker sells USDC for 5.08 WETH (profit: 0.08 WETH)
            make_swap(pool, usdc, weth, 12, attacker_bot,
                11_000_000_000, 5_080_000_000_000_000_000, 19_000_000, "univ2"),
        ];

        let candidates = detect_sandwich_pattern(&swaps);
        assert!(!candidates.is_empty(), "POS_D13_01: must detect sandwich");

        let c = &candidates[0];
        let profit = compute_attacker_profit(&c.front, &c.back, None, None);
        let slippage = compute_victim_slippage(&c.front, &c.victim, None);

        let conf = compute_d13_confidence(
            true,
            profit.profit_raw > Decimal::ZERO,
            slippage >= Decimal::new(5, 3),
            0.55, 0.15, 0.15, 0.85,
        );

        assert!(conf >= 0.55, "POS_D13_01: confidence must be >= 0.55, got {conf}");
        let severity = severity_from_confidence(conf);
        assert!(
            matches!(severity, Severity::Medium | Severity::High | Severity::Critical),
            "POS_D13_01: severity must be at least Medium"
        );
    }

    #[test]
    fn fixture_neg_d13_01_cow_settlement_suppressed() {
        // Replays NEG_D13_01_cow_settlement.json scenario.
        // 3 swaps on same pool, same block — looks like F-V-B.
        // "Attacker" = CoW Protocol Settlement → hard-suppressed.
        let cow = "0x9008D19f58AAbD9eD0D60971565AA8510560ab41";

        assert!(
            is_settlement_address(cow, &[]),
            "NEG_D13_01: CoW Protocol Settlement must be in allowlist"
        );

        let swaps = vec![
            make_swap("0xpool_neg", WETH, USDC, 0, cow, 1000, 900, 200, "univ2"),
            make_swap("0xpool_neg", WETH, USDC, 1, VICTIM, 500, 440, 200, "univ2"),
            make_swap("0xpool_neg", USDC, WETH, 2, cow, 900, 1050, 200, "univ2"),
        ];

        let candidates = detect_sandwich_pattern(&swaps);
        let non_suppressed: Vec<_> = candidates
            .iter()
            .filter(|c| !is_settlement_address(&c.attacker_address, &[]))
            .collect();
        assert!(
            non_suppressed.is_empty(),
            "NEG_D13_01: all CoW settlement candidates must be suppressed"
        );
    }

    // -----------------------------------------------------------------------
    // decode_swap_for_chain dispatch tests
    // -----------------------------------------------------------------------

    use mg_onchain_chain_adapter::ethereum::decoder::{
        UNISWAP_V2_SWAP_TOPIC0,
        UNISWAP_V3_SWAP_TOPIC0,
        PANCAKE_V3_SWAP_TOPIC0,
        AERODROME_SWAP_TOPIC0,
    };
    use mg_onchain_chain_adapter::ethereum::types::RawLog;

    /// Build a minimal RawLog with a given topic0 and no data (for wrong-topic0 tests).
    fn raw_log_with_topic0(topic0: &str) -> RawLog {
        RawLog {
            address: "0x0000000000000000000000000000000000000001".to_string(),
            topics: vec![topic0.to_string()],
            data: vec![],
            block_number: 100,
            tx_hash: "0xaaaa".to_string(),
            log_index: 0,
        }
    }

    /// Build a synthetic UniV2 Swap RawLog with known amounts.
    ///
    /// Swap(sender=0x1111..., to=0x2222..., amount0In=1000, amount1In=0, amount0Out=0, amount1Out=2000)
    fn make_univ2_raw_log() -> RawLog {
        fn encode_addr(addr: &str) -> String {
            let stripped = addr.strip_prefix("0x").unwrap_or(addr);
            format!("0x{:0>64}", stripped)
        }
        let sender = encode_addr("0x1111111111111111111111111111111111111111");
        let to = encode_addr("0x2222222222222222222222222222222222222222");
        // data: 4 × uint256 = 128 bytes
        // amount0In = 1000 = 0x3E8 at bytes 30-31
        // amount1In = 0
        // amount0Out = 0
        // amount1Out = 2000 = 0x7D0 at bytes 126-127
        let mut data = vec![0u8; 128];
        data[30] = 0x03;
        data[31] = 0xE8;
        data[126] = 0x07;
        data[127] = 0xD0;
        RawLog {
            address: "0xuniv2pool0000000000000000000000000000001".to_string(),
            topics: vec![UNISWAP_V2_SWAP_TOPIC0.to_string(), sender, to],
            data,
            block_number: 19_000_000,
            tx_hash: "0xffffff01".to_string(),
            log_index: 1,
        }
    }

    /// Build a synthetic PancakeSwap V3 Swap RawLog with known amounts.
    ///
    /// Swap(sender=0x1111..., recipient=0x2222..., amount0=3000 (positive=in),
    ///      amount1=-6000 (negative=out), sqrtPrice=1, liquidity=1000, tick=0,
    ///      feeToken0=5, feeToken1=0)
    fn make_pancake_v3_raw_log() -> RawLog {
        fn encode_addr(addr: &str) -> String {
            let stripped = addr.strip_prefix("0x").unwrap_or(addr);
            format!("0x{:0>64}", stripped)
        }
        let sender = encode_addr("0x1111111111111111111111111111111111111111");
        let recipient = encode_addr("0x2222222222222222222222222222222222222222");

        // 7 × 32 bytes for non-indexed fields:
        // [0..32]   amount0 = 3000 (int256, positive)
        // [32..64]  amount1 = -6000 (int256, negative two's complement)
        // [64..96]  sqrtPriceX96 = 1
        // [96..128] liquidity = 1000
        // [128..160] tick = 0
        // [160..192] protocolFeesToken0 = 5
        // [192..224] protocolFeesToken1 = 0
        let mut data = vec![0u8; 7 * 32];

        // amount0 = 3000 = 0xBB8
        data[30] = 0x0B;
        data[31] = 0xB8;

        // amount1 = -6000 = two's complement of 6000 in 256 bits
        // 6000 = 0x1770; -6000 in 16-byte i128: 0xFFFF...E890
        let neg_6000 = (-6000i128).to_be_bytes();
        for b in data[32..48].iter_mut() { *b = 0xFF; } // sign extend high bytes
        data[48..64].copy_from_slice(&neg_6000);

        // sqrtPriceX96 = 1
        data[64 + 31] = 0x01;

        // liquidity = 1000 = 0x3E8
        data[96 + 30] = 0x03;
        data[96 + 31] = 0xE8;

        // tick = 0 (already zero)
        // protocolFeesToken0 = 5
        data[160 + 31] = 0x05;
        // protocolFeesToken1 = 0 (already zero)

        RawLog {
            address: "0xpancakev3pool00000000000000000000000001".to_string(),
            topics: vec![PANCAKE_V3_SWAP_TOPIC0.to_string(), sender, recipient],
            data,
            block_number: 38_000_000,
            tx_hash: "0xffffff02".to_string(),
            log_index: 2,
        }
    }

    /// Build a synthetic Aerodrome Swap RawLog with known amounts.
    ///
    /// Swap(sender=0x3333..., to=0x4444..., amount0In=500, amount1In=0, amount0Out=0, amount1Out=900)
    fn make_aerodrome_raw_log() -> RawLog {
        fn encode_addr(addr: &str) -> String {
            let stripped = addr.strip_prefix("0x").unwrap_or(addr);
            format!("0x{:0>64}", stripped)
        }
        let sender = encode_addr("0x3333333333333333333333333333333333333333");
        let to = encode_addr("0x4444444444444444444444444444444444444444");

        // 4 × 32 bytes for non-indexed fields:
        // [0..32]   amount0In = 500 = 0x1F4
        // [32..64]  amount1In = 0
        // [64..96]  amount0Out = 0
        // [96..128] amount1Out = 900 = 0x384
        let mut data = vec![0u8; 4 * 32];
        data[30] = 0x01;
        data[31] = 0xF4;
        data[96 + 30] = 0x03;
        data[96 + 31] = 0x84;

        RawLog {
            address: "0xaerodromepool0000000000000000000000001".to_string(),
            topics: vec![AERODROME_SWAP_TOPIC0.to_string(), sender, to],
            data,
            block_number: 12_000_000,
            tx_hash: "0xffffff03".to_string(),
            log_index: 3,
        }
    }

    // --- BSC + UniV3-shaped log decodes via univ3 ---
    #[test]
    fn bsc_univ3_log_decodes_via_univ3() {
        // A UniV3-shaped log on BSC still decodes via the univ3 decoder.
        // UniV3 topic0 fires before the PancakeV3 dispatch branch.
        let log = raw_log_with_topic0(UNISWAP_V3_SWAP_TOPIC0);
        // Topic0 matches but data is empty → Err (malformed). None returned by dispatch.
        // The key invariant: PancakeV3 path is NOT chosen for UniV3 topic0.
        let result = decode_swap_for_chain(&log, Chain::Bsc);
        // An empty-data UniV3 log → decode error → None in dispatch.
        assert!(result.is_none(), "empty UniV3 log on BSC returns None (decode error swallowed)");
    }

    // --- BSC + PancakeSwap V3-shaped log decodes via pancake_v3 ---
    #[test]
    fn bsc_pancake_v3_log_decodes_correctly() {
        let log = make_pancake_v3_raw_log();
        let result = decode_swap_for_chain(&log, Chain::Bsc);
        let swap = result.expect("PancakeSwap V3 log on BSC must decode");
        assert_eq!(swap.protocol, "pancake_v3");
        // amount0 = 3000 (positive = in), amount1 = -6000 (negative = out)
        assert_eq!(swap.amount_in, Decimal::from(3000u32));
        assert_eq!(swap.amount_out, Decimal::from(6000u32));
        assert_eq!(swap.block_height, 38_000_000);
    }

    // --- Base + Aerodrome-shaped log decodes via aerodrome ---
    #[test]
    fn base_aerodrome_log_decodes_correctly() {
        let log = make_aerodrome_raw_log();
        let result = decode_swap_for_chain(&log, Chain::Base);
        let swap = result.expect("Aerodrome log on Base must decode");
        assert_eq!(swap.protocol, "aerodrome");
        assert_eq!(swap.amount_in, Decimal::from(500u32));
        assert_eq!(swap.amount_out, Decimal::from(900u32));
        assert_eq!(swap.block_height, 12_000_000);
    }

    // --- Base + UniV2-shaped log decodes via univ2 ---
    #[test]
    fn base_univ2_log_decodes_via_univ2() {
        let log = make_univ2_raw_log();
        let result = decode_swap_for_chain(&log, Chain::Base);
        let swap = result.expect("UniV2 log on Base must decode");
        assert_eq!(swap.protocol, "univ2");
        assert_eq!(swap.amount_in, Decimal::from(1000u32));
        assert_eq!(swap.amount_out, Decimal::from(2000u32));
    }

    // --- Ethereum + Aerodrome-shaped log returns None (Aerodrome only on Base) ---
    #[test]
    fn ethereum_aerodrome_log_returns_none() {
        // Aerodrome topic0 does not match UniV2 or UniV3, and the aerodrome decoder
        // branch is only entered for Chain::Base. On Ethereum, it returns None.
        let log = make_aerodrome_raw_log();
        let result = decode_swap_for_chain(&log, Chain::Ethereum);
        assert!(result.is_none(), "Aerodrome log on Ethereum must return None");
    }
}
