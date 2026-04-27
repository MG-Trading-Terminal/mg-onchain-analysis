//! CEX hot-wallet address registry.
//!
//! Loads a seed list from `crates/token-registry/data/cex_wallets.json` and
//! provides O(1) lookup by Solana Base58 address.
//!
//! The seed list covers the top 10–15 exchanges' known Solana hot wallets.
//! It is intentionally small for MVP; Phase 3 should integrate a maintained
//! address-label feed (Arkham Intelligence API, Nansen labels, etc.).
//!
//! Sources for seed list: see `data/cex_wallets.json` `_sources` field.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::RegistryError;

/// A single entry in the CEX wallet JSON file.
#[derive(Debug, Deserialize)]
struct CexWalletEntry {
    address: String,
    exchange: String,
    #[allow(dead_code)] // label/source are informational
    label: String,
    #[allow(dead_code)]
    source: String,
}

/// Wrapper for the CEX wallets JSON file structure.
#[derive(Debug, Deserialize)]
struct CexWalletsFile {
    wallets: Vec<CexWalletEntry>,
}

/// Lookup result for a CEX wallet address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CexMatch {
    /// The exchange name (e.g., `"binance"`, `"coinbase"`).
    pub exchange: String,
}

/// CEX wallet registry — maps address → exchange name.
///
/// Constructed once and shared as an `Arc` across all enrichment workers.
/// The underlying map is `HashMap` (address string → exchange string).
/// There's no output-path reproducibility concern here — this is a lookup
/// table, not a detector output.
#[derive(Debug, Clone)]
pub struct CexRegistry {
    map: HashMap<String, String>,
}

impl CexRegistry {
    /// Load the registry from the embedded JSON bytes (baked into the binary at
    /// compile time with `include_str!`).
    ///
    /// Returns `Err(RegistryError::CexRegistryLoad)` if the JSON is malformed.
    pub fn load_embedded() -> Result<Self, RegistryError> {
        let json = include_str!("../data/cex_wallets.json");
        Self::from_json(json)
    }

    /// Parse the registry from a JSON string.
    /// This is the testable entry point (tests pass fixture JSON directly).
    pub fn from_json(json: &str) -> Result<Self, RegistryError> {
        let file: CexWalletsFile = serde_json::from_str(json).map_err(|e| {
            RegistryError::CexRegistryLoad(format!("JSON parse error: {e}"))
        })?;

        let map: HashMap<String, String> = file
            .wallets
            .into_iter()
            .map(|entry| (entry.address, entry.exchange))
            .collect();

        Ok(Self { map })
    }

    /// Returns `Some(CexMatch)` if `address` is a known CEX wallet.
    pub fn lookup(&self, address: &str) -> Option<CexMatch> {
        self.map.get(address).map(|exchange| CexMatch {
            exchange: exchange.clone(),
        })
    }

    /// Number of addresses in the registry.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns `true` if the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Global singleton of the embedded CEX registry.
/// Loaded once on first access; subsequent calls return the cached instance.
static EMBEDDED_REGISTRY: OnceLock<CexRegistry> = OnceLock::new();

/// Get the global embedded CEX registry (loaded once).
///
/// Panics only if the embedded JSON is malformed (compile-time constant — would
/// be caught by tests before shipping). In production this never panics.
pub fn embedded_registry() -> &'static CexRegistry {
    EMBEDDED_REGISTRY.get_or_init(|| {
        CexRegistry::load_embedded().expect("embedded cex_wallets.json is malformed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_JSON: &str = r#"{
        "wallets": [
            {
                "address": "5tzFkiKscXHK5ZXCGbCAbZxLKQAFqobMVBkn5cCgPKEu",
                "exchange": "binance",
                "label": "Binance hot wallet 1",
                "source": "test"
            },
            {
                "address": "AC5RDfQFmDS1deWZos921JfqscXdByf8BKHs5ACWjtW2",
                "exchange": "coinbase",
                "label": "Coinbase hot wallet",
                "source": "test"
            }
        ]
    }"#;

    #[test]
    fn from_json_parses_correctly() {
        let reg = CexRegistry::from_json(MINIMAL_JSON).expect("should parse");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn lookup_known_address_returns_match() {
        let reg = CexRegistry::from_json(MINIMAL_JSON).unwrap();
        let result = reg.lookup("5tzFkiKscXHK5ZXCGbCAbZxLKQAFqobMVBkn5cCgPKEu");
        assert_eq!(result, Some(CexMatch { exchange: "binance".to_owned() }));
    }

    #[test]
    fn lookup_unknown_address_returns_none() {
        let reg = CexRegistry::from_json(MINIMAL_JSON).unwrap();
        let result = reg.lookup("SomeRandomWallet111111111111111111111111111");
        assert!(result.is_none());
    }

    #[test]
    fn lookup_coinbase_address() {
        let reg = CexRegistry::from_json(MINIMAL_JSON).unwrap();
        let result = reg.lookup("AC5RDfQFmDS1deWZos921JfqscXdByf8BKHs5ACWjtW2");
        assert_eq!(result, Some(CexMatch { exchange: "coinbase".to_owned() }));
    }

    #[test]
    fn embedded_json_loads_without_error() {
        // This test validates that the baked-in JSON file is structurally valid.
        let reg = CexRegistry::load_embedded().expect("embedded cex_wallets.json must be valid");
        assert!(!reg.is_empty(), "embedded registry must have at least one entry");
    }

    #[test]
    fn from_json_rejects_malformed_json() {
        let result = CexRegistry::from_json("{not json}");
        assert!(result.is_err());
    }
}
