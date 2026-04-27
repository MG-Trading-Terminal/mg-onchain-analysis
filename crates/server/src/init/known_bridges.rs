//! TOML loader for `config/known_bridges.toml` → `KnownBridgeSet`.
//!
//! Mirrors `init::known_drainers` (Sprint 23+) for D14 Bridge Drain Detector.
//! Called at startup by `init::detectors::build_all_detectors` to construct the
//! `KnownBridgeSet` passed to `D14BridgeDrainDetector`.
//!
//! # Schema (`config/known_bridges.toml`)
//!
//! ```toml
//! [[bridges]]
//! name     = "Ronin Bridge"
//! chains   = ["ethereum"]
//! tvl_tier = "Tier1"
//! source   = "https://rekt.news/ronin-rekt"
//! [bridges.addresses]
//! ethereum = ["0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2"]
//! ```
//!
//! # Error handling
//!
//! Returns `anyhow::Error` on:
//! - File read failure (path not found, permission denied)
//! - TOML parse failure (malformed syntax)
//! - Unknown chain string in `chains` field (`Chain::from_str` error)
//! - Unknown tier string in `tvl_tier` field (must be "Tier1" or "Tier2")
//! - Missing `chains` field (required — no backwards-compat default for bridges)
//!
//! # Tests
//!
//! Four unit tests at module bottom:
//! - Parses a valid Tier1 bridge entry correctly
//! - Missing `chains` field → error
//! - Unknown tier string → error
//! - Multi-chain bridge (Poly Network on ethereum + bsc + polygon)

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::Context as AnyhowContext;
use mg_onchain_common::chain::Chain;
use mg_onchain_detectors::d14_bridge_drain::{
    BridgeTier, KnownBridge, KnownBridgeSet, KnownBridgesToml,
};

// ---------------------------------------------------------------------------
// Public loader
// ---------------------------------------------------------------------------

/// Load `config/known_bridges.toml` and build a `KnownBridgeSet`.
///
/// # Multi-chain behaviour
///
/// Each `[[bridges]]` entry lists `chains` (informational, for registry tagging)
/// and a `[bridges.addresses]` table mapping chain name → vec of addresses.
/// Only addresses in the `[bridges.addresses]` table are registered in the index.
/// The `chains` field documents intent; `[bridges.addresses]` determines runtime lookup.
///
/// # Errors
///
/// Returns `Err` on file I/O failure, TOML parse failure, unknown chain string,
/// or unknown tier string.
pub fn load_known_bridges(path: &Path) -> anyhow::Result<KnownBridgeSet> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read known_bridges.toml at {}", path.display()))?;

    let raw: KnownBridgesToml = toml::from_str(&content)
        .with_context(|| format!("failed to parse known_bridges.toml at {}", path.display()))?;

    let mut bridges: Vec<KnownBridge> = Vec::with_capacity(raw.bridges.len());

    for entry in raw.bridges {
        let tier = parse_tier(&entry.tvl_tier).with_context(|| {
            format!(
                "unknown tvl_tier '{}' in known_bridges.toml entry '{}'",
                entry.tvl_tier, entry.name
            )
        })?;

        // Parse chains list for the `KnownBridge.chains` field (informational).
        let mut chains: Vec<Chain> = Vec::with_capacity(entry.chains.len());
        for chain_str in &entry.chains {
            let chain = Chain::from_str(chain_str).with_context(|| {
                format!(
                    "unknown chain '{}' in known_bridges.toml entry '{}'",
                    chain_str, entry.name
                )
            })?;
            chains.push(chain);
        }

        if chains.is_empty() {
            anyhow::bail!(
                "known_bridges.toml entry '{}' has empty 'chains' field — required",
                entry.name
            );
        }

        // Parse per-chain address map.
        let mut addresses: HashMap<Chain, Vec<String>> =
            HashMap::with_capacity(entry.addresses.len());
        for (chain_str, addrs) in &entry.addresses {
            let chain = Chain::from_str(chain_str).with_context(|| {
                format!(
                    "unknown chain key '{}' in known_bridges.toml [bridges.addresses] for '{}'",
                    chain_str, entry.name
                )
            })?;
            let normalized: Vec<String> = addrs.iter().map(|a| a.to_lowercase()).collect();
            addresses.insert(chain, normalized);
        }

        bridges.push(KnownBridge {
            name: entry.name,
            chains,
            addresses,
            tvl_tier: tier,
            source: entry.source,
        });
    }

    Ok(KnownBridgeSet::from_bridges(bridges))
}

/// Parse "Tier1" / "Tier2" string → `BridgeTier`.
fn parse_tier(s: &str) -> anyhow::Result<BridgeTier> {
    match s {
        "Tier1" => Ok(BridgeTier::Tier1),
        "Tier2" => Ok(BridgeTier::Tier2),
        other => anyhow::bail!("unknown tier '{other}' — must be 'Tier1' or 'Tier2'"),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Write TOML content to a temp file and return the path.
    fn write_toml_to_temp(content: &str, suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mg_bridges_test_{suffix}.toml"));
        std::fs::write(&path, content).expect("write temp TOML must succeed");
        path
    }

    /// Parses a valid single-chain Tier1 bridge entry correctly.
    #[test]
    fn parses_tier1_single_chain_bridge() {
        let toml = r#"
[[bridges]]
name     = "Ronin Bridge"
chains   = ["ethereum"]
tvl_tier = "Tier1"
source   = "https://rekt.news/ronin-rekt"
notes    = ""
[bridges.addresses]
ethereum = ["0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2"]
"#;
        let path = write_toml_to_temp(toml, "tier1_single");
        let set = load_known_bridges(&path).expect("load must succeed");
        assert_eq!(set.bridge_count(), 1, "must have 1 bridge");

        let result = set.is_known_bridge(
            Chain::Ethereum,
            "0x1a2a1c938ce3ec39b6d47113c7955baa9dd454f2",
        );
        assert!(result.is_some(), "Ronin address must be found on Ethereum");
        let (name, tier) = result.unwrap();
        assert_eq!(name, "Ronin Bridge");
        assert_eq!(tier, BridgeTier::Tier1);
    }

    /// Missing `chains` field → loader error (not optional for bridges).
    #[test]
    fn missing_chains_field_returns_error() {
        let toml = r#"
[[bridges]]
name     = "Bad Bridge"
tvl_tier = "Tier1"
source   = "test"
[bridges.addresses]
ethereum = ["0x1234000000000000000000000000000000000001"]
"#;
        let path = write_toml_to_temp(toml, "missing_chains");
        // The `chains` field is required in BridgeEntryRaw — serde will fail.
        let result = load_known_bridges(&path);
        assert!(
            result.is_err(),
            "missing chains field must produce a loader error"
        );
    }

    /// Unknown tier string → loader error.
    #[test]
    fn unknown_tier_string_returns_error() {
        let toml = r#"
[[bridges]]
name     = "Unknown Tier Bridge"
chains   = ["ethereum"]
tvl_tier = "SuperSpecial"
source   = "test"
[bridges.addresses]
ethereum = ["0x1234000000000000000000000000000000000002"]
"#;
        let path = write_toml_to_temp(toml, "unknown_tier");
        let result = load_known_bridges(&path);
        assert!(
            result.is_err(),
            "unknown tvl_tier must produce a loader error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("SuperSpecial"),
            "error must mention the unknown tier: {err}"
        );
    }

    /// Multi-chain bridge: Poly Network on ethereum + bsc + polygon.
    #[test]
    fn multi_chain_bridge_populates_all_chains() {
        let toml = r#"
[[bridges]]
name     = "Poly Network"
chains   = ["ethereum", "bsc", "polygon"]
tvl_tier = "Tier1"
source   = "https://rekt.news/polynetwork-rekt"
notes    = ""
[bridges.addresses]
ethereum = ["0x250e76987d838a75310c34bf422ea9f1ac4cc906"]
bsc      = ["0x1c3b9f434157a8f5fd6ef19f9b2aadf7db63f4a8"]
polygon  = ["0x42d61d766b85431666b39b89c43011f24451bff6"]
"#;
        let path = write_toml_to_temp(toml, "multi_chain_poly");
        let set = load_known_bridges(&path).expect("load must succeed");
        assert_eq!(set.bridge_count(), 1, "must have 1 bridge");
        assert_eq!(set.address_count(), 3, "must have 3 (chain, address) entries");

        // Ethereum address must be registered.
        assert!(
            set.is_known_bridge(Chain::Ethereum, "0x250e76987d838a75310c34bf422ea9f1ac4cc906")
                .is_some(),
            "Poly Network must be found on Ethereum"
        );
        // BSC address must be registered.
        assert!(
            set.is_known_bridge(Chain::Bsc, "0x1c3b9f434157a8f5fd6ef19f9b2aadf7db63f4a8")
                .is_some(),
            "Poly Network must be found on BSC"
        );
        // Polygon address must be registered.
        assert!(
            set.is_known_bridge(Chain::Polygon, "0x42d61d766b85431666b39b89c43011f24451bff6")
                .is_some(),
            "Poly Network must be found on Polygon"
        );
        // Ethereum address must NOT be found on BSC (different custody addresses).
        assert!(
            set.is_known_bridge(Chain::Bsc, "0x250e76987d838a75310c34bf422ea9f1ac4cc906")
                .is_none(),
            "Ethereum address must NOT match on BSC"
        );
        // Arbitrum has no Poly Network address.
        assert!(
            set.is_known_bridge(Chain::Arbitrum, "0x250e76987d838a75310c34bf422ea9f1ac4cc906")
                .is_none(),
            "Poly Network must NOT be found on Arbitrum"
        );
    }

    /// Tier2 bridge parses correctly.
    #[test]
    fn parses_tier2_bridge() {
        let toml = r#"
[[bridges]]
name     = "Multichain (Anyswap)"
chains   = ["ethereum", "bsc"]
tvl_tier = "Tier2"
source   = "https://multichain.org"
notes    = ""
[bridges.addresses]
ethereum = ["0xc564ee9f21ed8a2d8e7e76c085740d5e4c5fafbe"]
bsc      = ["0xd1c5966f9f5ee6881ff6b261bbeda45972b1b5f3"]
"#;
        let path = write_toml_to_temp(toml, "tier2_multichain");
        let set = load_known_bridges(&path).expect("load must succeed");

        let result = set.is_known_bridge(
            Chain::Ethereum,
            "0xc564ee9f21ed8a2d8e7e76c085740d5e4c5fafbe",
        );
        assert!(result.is_some(), "Multichain must be found on Ethereum");
        let (_, tier) = result.unwrap();
        assert_eq!(tier, BridgeTier::Tier2, "Multichain must be Tier2");
    }
}
