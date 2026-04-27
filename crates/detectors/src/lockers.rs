//! LP Locker Registry — per-chain set of known LP locker contract addresses.
//!
//! # Purpose
//!
//! LP lockers are smart contracts that hold LP tokens on behalf of token deployers
//! for a fixed duration. When a deployer deposits LP tokens into a locker, the
//! locker contract holds the tokens until the lock expires — providing a credible
//! commitment that the LP will not be drained before that date.
//!
//! D02 Signal B and D10 Signal B both gate on whether any locker is registered for
//! a token's pool. This registry provides the address set used by the EVM indexer
//! hook to detect Transfer-to-locker events and populate `TokenMeta.lockers`.
//!
//! # Locker protocols (verified addresses)
//!
//! ## Unicrypt Network
//!
//! Unicrypt is the most widely deployed EVM LP locker (2023–2025 data).
//!
//! | Chain     | Contract                                     | Source / Verified |
//! |-----------|----------------------------------------------|-------------------|
//! | Ethereum  | `0x663A5C229c09b049E36dCc11a9B0d4a8Eb9db214` | training-time knowledge (Unicrypt V2 locker); SPEC-NOTE: re-verify via docs.unicrypt.network |
//! | BSC       | `0xC765bddB93b0D1c1A88282BA0fa6B2d00E3e0c83` | training-time knowledge; SPEC-NOTE: re-verify |
//!
//! ## Team Finance
//!
//! Team Finance LP Lock is a popular alternative to Unicrypt.
//!
//! | Chain     | Contract                                     | Source / Verified |
//! |-----------|----------------------------------------------|-------------------|
//! | Ethereum  | `0xE2fE530C047f2d85298b07D9333C05737f1435fB` | training-time knowledge (Team Finance V1); SPEC-NOTE: V2 address may differ |
//! | BSC       | `0x0C89C0407775dd89b12918B9c0aa42Bf96518820` | training-time knowledge; SPEC-NOTE: re-verify |
//!
//! ## TrustSwap LP Locker
//!
//! | Chain     | Contract                                     | Source / Verified |
//! |-----------|----------------------------------------------|-------------------|
//! | Ethereum  | `0xCF8A0c5C0e84b39Aa70bf63ad05e17BF9b5a2D34` | training-time knowledge; SPEC-NOTE: re-verify |
//!
//! # SPEC-NOTE D10-EVM-LP-LOCK (Sprint 25)
//!
//! The registry addresses above are sourced from training-time knowledge.
//! Web-verification was attempted (WebFetch) but external network was not available
//! during Sprint 25. The addresses are retained as strong training-time priors but
//! MUST be verified against each protocol's current documentation before activating
//! the registry in the production hot path. Incorrect addresses cause:
//!   - False negative: real lock not detected (conservative — Signal B over-fires but
//!     not a safety regression, only recall loss for the LP-lock pathway).
//!   - False positive: wrong address treated as locker (benign — low probability since
//!     typical locker addresses are protocol-specific and not shared with other contracts).
//!
//! Sprint 25 action: WebFetch the following to get current addresses:
//!   - Unicrypt: `https://docs.unicrypt.network` → token-locker → contract addresses
//!   - Team Finance: `https://team.finance` → locker → supported contracts
//!   - TrustSwap: `https://trustswap.com` → smart-lock → contract addresses
//!
//! # ADR 0003 compliance
//!
//! This registry is a static compile-time table — no external API calls. Consistent
//! with the self-sovereign infrastructure policy. Address verification is done once
//! at sprint boundary (not at runtime).
//!
//! # Indexer integration (Sprint 25+)
//!
//! TODO(next-sprint): Wire this registry into the EVM indexer hot path.
//! The indexer should check every ERC-20 Transfer log: if `transfer.to` is in
//! `LockerRegistry::locker_addresses(chain)`, emit a locker enrichment event that
//! populates `TokenMeta.lockers` for the transferred token.
//!
//! Wire path:
//!   `crates/indexer/src/evm/` hot-path loop
//!     → for each Transfer log, check `registry.is_locker(chain, &log.address_to)`
//!     → if true: call `PgStore::upsert_locker(chain, token, locker_addr, locked_amount, locked_until=None)`
//!
//! The `locked_until` timestamp requires a separate `eth_call` to the locker's
//! `getLockedAmount(token, deployer)` method — each locker protocol has a different
//! ABI. Defer to Sprint 26 (locker ABI decode). For Sprint 25, record the lock
//! as a flag without the expiry (locked_until=None → treated as permanent lock for
//! D02 Signal B computation, which is conservative — correct direction).

use std::collections::{BTreeMap, HashSet};

use mg_onchain_common::chain::Chain;

// ---------------------------------------------------------------------------
// Locker protocol identifier
// ---------------------------------------------------------------------------

/// Identifies the LP locking protocol for a given locker address.
///
/// Used in `TokenMeta.lockers` evidence so consumers can filter by protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockerProtocol {
    /// Unicrypt Network LP Lock (V2 and V3 — same address per chain).
    Unicrypt,
    /// Team Finance LP Lock (V1 contract; V2 address differs — see SPEC-NOTE).
    TeamFinance,
    /// TrustSwap LP Smart Lock.
    TrustSwap,
}

impl LockerProtocol {
    /// Human-readable protocol name (used in evidence notes).
    pub fn name(self) -> &'static str {
        match self {
            LockerProtocol::Unicrypt => "Unicrypt",
            LockerProtocol::TeamFinance => "Team Finance",
            LockerProtocol::TrustSwap => "TrustSwap",
        }
    }
}

// ---------------------------------------------------------------------------
// LockerEntry — address + protocol for a single locker
// ---------------------------------------------------------------------------

/// A single known locker contract address with its protocol identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockerEntry {
    /// Lowercase EVM address (canonical form: `0x` + 40 hex chars, all lowercase).
    pub address: &'static str,
    /// The locking protocol this contract belongs to.
    pub protocol: LockerProtocol,
}

// ---------------------------------------------------------------------------
// Compile-time locker tables
// ---------------------------------------------------------------------------

/// Known locker addresses for Ethereum mainnet.
///
/// # SPEC-NOTE: re-verify all addresses before Sprint 25 production activation.
/// Sources listed in module-level doc.
const ETHEREUM_LOCKERS: &[LockerEntry] = &[
    LockerEntry {
        address: "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214",
        protocol: LockerProtocol::Unicrypt,
    },
    LockerEntry {
        address: "0xe2fe530c047f2d85298b07d9333c05737f1435fb",
        protocol: LockerProtocol::TeamFinance,
    },
    LockerEntry {
        address: "0xcf8a0c5c0e84b39aa70bf63ad05e17bf9b5a2d34",
        protocol: LockerProtocol::TrustSwap,
    },
];

/// Known locker addresses for BSC mainnet.
///
/// # SPEC-NOTE: re-verify all addresses before Sprint 25 production activation.
const BSC_LOCKERS: &[LockerEntry] = &[
    LockerEntry {
        address: "0xc765bddb93b0d1c1a88282ba0fa6b2d00e3e0c83",
        protocol: LockerProtocol::Unicrypt,
    },
    LockerEntry {
        address: "0x0c89c0407775dd89b12918b9c0aa42bf96518820",
        protocol: LockerProtocol::TeamFinance,
    },
];

/// Known locker addresses for Base.
///
/// # SPEC-NOTE: No verified addresses yet for Base. Unicrypt and Team Finance
/// have deployed on Base; addresses must be fetched from their documentation.
/// Sprint 25: populate from docs.unicrypt.network + team.finance.
const BASE_LOCKERS: &[LockerEntry] = &[];

/// Known locker addresses for Arbitrum.
///
/// # SPEC-NOTE: No verified addresses yet for Arbitrum. Same action item as Base.
const ARBITRUM_LOCKERS: &[LockerEntry] = &[];

/// Known locker addresses for Polygon.
///
/// # SPEC-NOTE: No verified addresses yet for Polygon. Same action item as Base.
const POLYGON_LOCKERS: &[LockerEntry] = &[];

// ---------------------------------------------------------------------------
// LockerRegistry
// ---------------------------------------------------------------------------

/// Per-chain registry of known LP locker contract addresses.
///
/// Built at startup from compile-time tables. Lookups are O(1) via
/// per-chain `HashSet`.
///
/// # Usage
///
/// ```rust,no_run
/// use mg_onchain_detectors::lockers::LockerRegistry;
/// use mg_onchain_common::chain::Chain;
///
/// let registry = LockerRegistry::default();
/// let is_locker = registry.is_locker(Chain::Ethereum, "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214");
/// assert!(is_locker);
/// ```
#[derive(Debug)]
pub struct LockerRegistry {
    /// Per-chain set of known locker addresses (lowercase).
    /// `BTreeMap` for deterministic iteration.
    per_chain: BTreeMap<Chain, HashSet<String>>,
    /// Per-chain mapping: locker_address → protocol name.
    /// Separate from `per_chain` to avoid storing protocol in the hot-path lookup set.
    per_chain_protocol: BTreeMap<Chain, BTreeMap<String, LockerProtocol>>,
}

impl LockerRegistry {
    /// Build the registry from compile-time tables.
    ///
    /// All addresses are lowercased at construction time — callers must also
    /// lowercase before lookup (see `is_locker`).
    pub fn build() -> Self {
        let entries: &[(Chain, &[LockerEntry])] = &[
            (Chain::Ethereum, ETHEREUM_LOCKERS),
            (Chain::Bsc, BSC_LOCKERS),
            (Chain::Base, BASE_LOCKERS),
            (Chain::Arbitrum, ARBITRUM_LOCKERS),
            (Chain::Polygon, POLYGON_LOCKERS),
        ];

        let mut per_chain: BTreeMap<Chain, HashSet<String>> = BTreeMap::new();
        let mut per_chain_protocol: BTreeMap<Chain, BTreeMap<String, LockerProtocol>> =
            BTreeMap::new();

        for (chain, locker_entries) in entries {
            let addr_set = per_chain.entry(*chain).or_default();
            let proto_map = per_chain_protocol.entry(*chain).or_default();
            for entry in locker_entries.iter() {
                // Addresses are stored lowercase in the compile-time table.
                // We store them as owned Strings for HashSet membership checks.
                addr_set.insert(entry.address.to_owned());
                proto_map.insert(entry.address.to_owned(), entry.protocol);
            }
        }

        Self { per_chain, per_chain_protocol }
    }

    /// Returns `true` if the given address is a known LP locker on `chain`.
    ///
    /// # Address normalisation
    ///
    /// The `address` argument is lowercased before lookup. Callers may pass
    /// checksummed (`0xAbCd...`) or lowercase addresses — both are handled.
    ///
    /// # Solana
    ///
    /// Returns `false` for `Chain::Solana` — LP lockers are an EVM concept.
    /// Solana uses SPL-locked vaults which are tracked differently.
    pub fn is_locker(&self, chain: Chain, address: &str) -> bool {
        let lower = address.to_lowercase();
        self.per_chain
            .get(&chain)
            .map(|set| set.contains(&lower))
            .unwrap_or(false)
    }

    /// Returns the protocol of the locker at `address` on `chain`, if known.
    pub fn protocol_of(&self, chain: Chain, address: &str) -> Option<LockerProtocol> {
        let lower = address.to_lowercase();
        self.per_chain_protocol
            .get(&chain)
            .and_then(|m| m.get(&lower))
            .copied()
    }

    /// Returns the total number of known locker addresses across all chains.
    pub fn total_count(&self) -> usize {
        self.per_chain.values().map(|s| s.len()).sum()
    }

    /// Returns the number of known locker addresses for the given chain.
    pub fn count_for_chain(&self, chain: Chain) -> usize {
        self.per_chain.get(&chain).map(|s| s.len()).unwrap_or(0)
    }

    /// Returns all known locker addresses for `chain` as owned `String`s (lowercase).
    ///
    /// Used by the server init layer to populate `SubscribeFilter::evm_contract_addresses`
    /// so the EVM subscribe loop receives Transfer events from locker contracts.
    ///
    /// # Returns
    ///
    /// A `Vec<String>` of `0x`-prefixed lowercase 42-char EVM addresses.
    /// Empty `Vec` for chains with no registered lockers (e.g. Solana).
    ///
    /// Results are sorted for deterministic output (`BTreeMap` iteration).
    pub fn all_addresses(&self, chain: Chain) -> Vec<String> {
        self.per_chain
            .get(&chain)
            .map(|set| {
                let mut addrs: Vec<String> = set.iter().cloned().collect();
                addrs.sort();
                addrs
            })
            .unwrap_or_default()
    }
}

impl Default for LockerRegistry {
    fn default() -> Self {
        Self::build()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a fresh registry for each test (cheap — compile-time tables).
    fn reg() -> LockerRegistry {
        LockerRegistry::build()
    }

    // -----------------------------------------------------------------------
    // Ethereum locker lookups
    // -----------------------------------------------------------------------

    #[test]
    fn ethereum_unicrypt_is_locker() {
        let r = reg();
        assert!(
            r.is_locker(Chain::Ethereum, "0x663A5C229c09b049E36dCc11a9B0d4a8Eb9db214"),
            "Unicrypt Ethereum must be in registry (checksummed input)"
        );
        assert!(
            r.is_locker(Chain::Ethereum, "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214"),
            "Unicrypt Ethereum must be in registry (lowercase input)"
        );
    }

    #[test]
    fn ethereum_team_finance_is_locker() {
        let r = reg();
        assert!(
            r.is_locker(Chain::Ethereum, "0xE2fE530C047f2d85298b07D9333C05737f1435fB"),
            "Team Finance Ethereum must be in registry"
        );
    }

    #[test]
    fn ethereum_trustswap_is_locker() {
        let r = reg();
        assert!(
            r.is_locker(Chain::Ethereum, "0xCF8A0c5C0e84b39Aa70bf63ad05e17BF9b5a2D34"),
            "TrustSwap Ethereum must be in registry"
        );
    }

    #[test]
    fn ethereum_random_address_is_not_locker() {
        let r = reg();
        assert!(
            !r.is_locker(Chain::Ethereum, "0xdead000000000000000000000000000000000000"),
            "Random address must NOT be in the Ethereum locker registry"
        );
    }

    // -----------------------------------------------------------------------
    // BSC locker lookups
    // -----------------------------------------------------------------------

    #[test]
    fn bsc_unicrypt_is_locker() {
        let r = reg();
        assert!(
            r.is_locker(Chain::Bsc, "0xC765bddB93b0D1c1A88282BA0fa6B2d00E3e0c83"),
            "Unicrypt BSC must be in registry"
        );
    }

    #[test]
    fn bsc_team_finance_is_locker() {
        let r = reg();
        assert!(
            r.is_locker(Chain::Bsc, "0x0C89C0407775dd89b12918B9c0aa42Bf96518820"),
            "Team Finance BSC must be in registry"
        );
    }

    #[test]
    fn bsc_address_not_on_ethereum() {
        let r = reg();
        // The BSC Unicrypt address should NOT be found on Ethereum's locker set.
        assert!(
            !r.is_locker(Chain::Ethereum, "0xC765bddB93b0D1c1A88282BA0fa6B2d00E3e0c83"),
            "BSC-specific locker must not match Ethereum chain"
        );
    }

    // -----------------------------------------------------------------------
    // Protocol lookup
    // -----------------------------------------------------------------------

    #[test]
    fn protocol_of_unicrypt_ethereum() {
        let r = reg();
        let proto = r.protocol_of(Chain::Ethereum, "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214");
        assert_eq!(proto, Some(LockerProtocol::Unicrypt));
    }

    #[test]
    fn protocol_of_unknown_returns_none() {
        let r = reg();
        let proto = r.protocol_of(Chain::Ethereum, "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        assert!(proto.is_none());
    }

    // -----------------------------------------------------------------------
    // Solana returns false (EVM-only feature)
    // -----------------------------------------------------------------------

    #[test]
    fn solana_is_never_a_locker_chain() {
        let r = reg();
        // Some valid EVM locker address — should never match on Solana
        assert!(
            !r.is_locker(Chain::Solana, "0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214"),
            "Solana must never match an EVM locker address"
        );
    }

    // -----------------------------------------------------------------------
    // Count helpers
    // -----------------------------------------------------------------------

    #[test]
    fn ethereum_has_expected_locker_count() {
        let r = reg();
        // 3 lockers: Unicrypt + Team Finance + TrustSwap
        assert_eq!(r.count_for_chain(Chain::Ethereum), 3);
    }

    #[test]
    fn bsc_has_expected_locker_count() {
        let r = reg();
        // 2 lockers: Unicrypt + Team Finance
        assert_eq!(r.count_for_chain(Chain::Bsc), 2);
    }

    #[test]
    fn base_arbitrum_polygon_have_zero_until_verified() {
        let r = reg();
        assert_eq!(r.count_for_chain(Chain::Base), 0, "Base: no verified lockers yet");
        assert_eq!(r.count_for_chain(Chain::Arbitrum), 0, "Arbitrum: no verified lockers yet");
        assert_eq!(r.count_for_chain(Chain::Polygon), 0, "Polygon: no verified lockers yet");
    }

    #[test]
    fn total_count_matches_sum() {
        let r = reg();
        let expected = r.count_for_chain(Chain::Ethereum)
            + r.count_for_chain(Chain::Bsc)
            + r.count_for_chain(Chain::Base)
            + r.count_for_chain(Chain::Arbitrum)
            + r.count_for_chain(Chain::Polygon);
        assert_eq!(r.total_count(), expected);
    }

    // -----------------------------------------------------------------------
    // Sprint 44: all_addresses — used to populate SubscribeFilter::evm_contract_addresses
    // -----------------------------------------------------------------------

    /// `all_addresses(Ethereum)` returns all known Ethereum locker addresses.
    #[test]
    fn all_addresses_ethereum_returns_known_lockers() {
        let r = reg();
        let addrs = r.all_addresses(Chain::Ethereum);
        // Must include the three known Ethereum lockers.
        assert!(addrs.contains(&"0x663a5c229c09b049e36dcc11a9b0d4a8eb9db214".to_string()),
            "Unicrypt Ethereum must be in all_addresses");
        assert!(addrs.contains(&"0xe2fe530c047f2d85298b07d9333c05737f1435fb".to_string()),
            "Team Finance Ethereum must be in all_addresses");
        assert!(addrs.contains(&"0xcf8a0c5c0e84b39aa70bf63ad05e17bf9b5a2d34".to_string()),
            "TrustSwap Ethereum must be in all_addresses");
        assert_eq!(addrs.len(), r.count_for_chain(Chain::Ethereum));
    }

    /// `all_addresses` returns sorted addresses (deterministic output).
    #[test]
    fn all_addresses_is_sorted() {
        let r = reg();
        let addrs = r.all_addresses(Chain::Ethereum);
        let mut sorted = addrs.clone();
        sorted.sort();
        assert_eq!(addrs, sorted, "all_addresses must return sorted addresses");
    }

    /// `all_addresses(Solana)` returns empty Vec (EVM-only feature).
    #[test]
    fn all_addresses_solana_returns_empty() {
        let r = reg();
        assert!(r.all_addresses(Chain::Solana).is_empty(),
            "Solana has no locker addresses");
    }
}
