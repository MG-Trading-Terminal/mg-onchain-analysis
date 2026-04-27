//! Smart-money amplification helpers shared across D04, D05, and D08.
//!
//! # What this module provides
//!
//! - [`TierCounts`] — aggregate count of smart-money addresses at each tier.
//! - [`intersect_tier_counts`] — compute tier counts for an address set against
//!   the smart-money map returned by `SmartMoneyLookup::fetch_smart_money_addresses`.
//!
//! # Usage pattern
//!
//! Each detector:
//! 1. Calls `SmartMoneyLookup::fetch_smart_money_addresses(chain, observed_at)` to get
//!    `HashMap<String, SmartMoneyTier>`.
//! 2. Calls `intersect_tier_counts(&relevant_addresses, &smart_money_map)` to get `TierCounts`.
//! 3. Calls the detector-specific delta function (in the detector module) to compute the delta.
//! 4. Applies the delta to confidence (capped by the existing per-signal cap).
//!
//! # Design reference
//!
//! `docs/designs/0023-smart-money-consumer-integration.md` §6 (Deliverable 6).

use std::collections::HashMap;

use mg_onchain_graph::SmartMoneyTier;

// ---------------------------------------------------------------------------
// TierCounts
// ---------------------------------------------------------------------------

/// Aggregate count of smart-money addresses at each tier, after intersecting
/// the relevant detector address set with the smart-money map.
///
/// Counts are per-address, not per-event. A single address at Tier1 contributes
/// `tier1 = 1` regardless of how many swaps it executed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierCounts {
    /// Number of Tier1 smart-money addresses in the intersection.
    pub tier1: u32,
    /// Number of Tier2 smart-money addresses in the intersection.
    pub tier2: u32,
    /// Number of Tier3 smart-money addresses in the intersection.
    pub tier3: u32,
}

impl TierCounts {
    /// Returns `true` when no smart-money addresses were found.
    pub fn is_empty(&self) -> bool {
        self.tier1 == 0 && self.tier2 == 0 && self.tier3 == 0
    }

    /// Returns `true` when at least one smart-money address (any tier) was found.
    pub fn has_any(&self) -> bool {
        !self.is_empty()
    }

    /// Compute tier counts from an iterator of `SmartMoneyTier` values.
    ///
    /// Useful when you already have a Vec of tiers and want to aggregate them.
    pub fn from_tiers(tiers: impl Iterator<Item = SmartMoneyTier>) -> Self {
        let mut counts = TierCounts::default();
        for tier in tiers {
            match tier {
                SmartMoneyTier::Tier1 => counts.tier1 += 1,
                SmartMoneyTier::Tier2 => counts.tier2 += 1,
                SmartMoneyTier::Tier3 => counts.tier3 += 1,
            }
        }
        counts
    }
}

// ---------------------------------------------------------------------------
// intersect_tier_counts
// ---------------------------------------------------------------------------

/// Compute tier counts for a set of addresses against the smart-money map.
///
/// For each address in `addresses`, looks it up in `smart_money_map` and
/// increments the appropriate tier counter. Each address is counted once,
/// even if it appears multiple times in `addresses`.
///
/// # Arguments
///
/// - `addresses`: Slice of addresses to intersect (wallet strings, canonical for the chain).
/// - `smart_money_map`: Result of `SmartMoneyLookup::fetch_smart_money_addresses`.
///
/// # Returns
///
/// `TierCounts` with the aggregate counts.
pub fn intersect_tier_counts(
    addresses: &[String],
    smart_money_map: &HashMap<String, SmartMoneyTier>,
) -> TierCounts {
    let mut counts = TierCounts::default();
    // Use a BTreeSet for deduplication to ensure deterministic counting.
    // (A wallet may appear multiple times in e.g. round-trip rows.)
    let mut seen = std::collections::BTreeSet::new();
    for addr in addresses {
        if seen.contains(addr.as_str()) {
            continue;
        }
        if let Some(&tier) = smart_money_map.get(addr) {
            seen.insert(addr.as_str());
            match tier {
                SmartMoneyTier::Tier1 => counts.tier1 += 1,
                SmartMoneyTier::Tier2 => counts.tier2 += 1,
                SmartMoneyTier::Tier3 => counts.tier3 += 1,
            }
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map(entries: &[(&str, SmartMoneyTier)]) -> HashMap<String, SmartMoneyTier> {
        entries
            .iter()
            .map(|(addr, tier)| (addr.to_string(), *tier))
            .collect()
    }

    /// 3 addresses, 2 in smart-money map → correct tier counts.
    #[test]
    fn intersect_three_addresses_two_in_map() {
        let map = make_map(&[
            ("wallet_a", SmartMoneyTier::Tier1),
            ("wallet_b", SmartMoneyTier::Tier2),
        ]);
        let addresses: Vec<String> = vec![
            "wallet_a".into(),
            "wallet_b".into(),
            "wallet_c".into(), // not in smart-money map
        ];
        let counts = intersect_tier_counts(&addresses, &map);
        assert_eq!(counts.tier1, 1, "should find 1 Tier1");
        assert_eq!(counts.tier2, 1, "should find 1 Tier2");
        assert_eq!(counts.tier3, 0, "should find 0 Tier3");
        assert!(counts.has_any());
    }

    /// Empty address list → empty tier counts.
    #[test]
    fn intersect_empty_addresses_returns_empty() {
        let map = make_map(&[("wallet_a", SmartMoneyTier::Tier1)]);
        let counts = intersect_tier_counts(&[], &map);
        assert!(counts.is_empty(), "empty addresses must produce empty counts");
    }

    /// Empty map → empty tier counts regardless of addresses.
    #[test]
    fn intersect_empty_map_returns_empty() {
        let map: HashMap<String, SmartMoneyTier> = HashMap::new();
        let addresses: Vec<String> = vec!["wallet_a".into(), "wallet_b".into()];
        let counts = intersect_tier_counts(&addresses, &map);
        assert!(counts.is_empty());
    }

    /// Duplicate addresses are counted only once per address.
    #[test]
    fn intersect_deduplicates_addresses() {
        let map = make_map(&[("wallet_a", SmartMoneyTier::Tier1)]);
        let addresses: Vec<String> = vec![
            "wallet_a".into(),
            "wallet_a".into(), // duplicate
            "wallet_a".into(), // duplicate
        ];
        let counts = intersect_tier_counts(&addresses, &map);
        assert_eq!(counts.tier1, 1, "address counted only once even if duplicated");
    }

    /// All-Tier3 addresses → tier3 count only.
    #[test]
    fn intersect_all_tier3() {
        let map = make_map(&[
            ("w1", SmartMoneyTier::Tier3),
            ("w2", SmartMoneyTier::Tier3),
        ]);
        let addresses: Vec<String> = vec!["w1".into(), "w2".into()];
        let counts = intersect_tier_counts(&addresses, &map);
        assert_eq!(counts.tier1, 0);
        assert_eq!(counts.tier2, 0);
        assert_eq!(counts.tier3, 2);
    }

    /// `TierCounts::from_tiers` produces the correct counts.
    #[test]
    fn from_tiers_accumulates_correctly() {
        let tiers = vec![
            SmartMoneyTier::Tier1,
            SmartMoneyTier::Tier2,
            SmartMoneyTier::Tier2,
            SmartMoneyTier::Tier3,
        ];
        let counts = TierCounts::from_tiers(tiers.into_iter());
        assert_eq!(counts.tier1, 1);
        assert_eq!(counts.tier2, 2);
        assert_eq!(counts.tier3, 1);
    }

    /// `TierCounts::is_empty` / `has_any` semantics.
    #[test]
    fn tier_counts_empty_has_any_semantics() {
        let empty = TierCounts::default();
        assert!(empty.is_empty());
        assert!(!empty.has_any());

        let non_empty = TierCounts { tier1: 1, tier2: 0, tier3: 0 };
        assert!(!non_empty.is_empty());
        assert!(non_empty.has_any());
    }
}
