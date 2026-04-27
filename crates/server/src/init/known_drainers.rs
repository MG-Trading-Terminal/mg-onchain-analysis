//! TOML loader for `config/known_drainers.toml` → `KnownDrainerSet`.
//!
//! Closes the carry-over item from Sprint 23: `init::detectors` previously
//! called `KnownDrainerSet::from_addresses` with only the Ethereum-sourced
//! flat address list from `detectors.toml`. This module loads the richer
//! `known_drainers.toml` that carries a `chains = [...]` field, so multi-chain
//! clusters (Inferno on ethereum + bsc + polygon) actually populate the per-chain
//! lookup in `KnownDrainerSet`.
//!
//! # Schema (config/known_drainers.toml)
//!
//! ```toml
//! [[drainers]]
//! name    = "Inferno Drainer"
//! chains  = ["ethereum", "bsc", "polygon"]
//! addresses = ["0x..."]
//! source  = "..."
//! ```
//!
//! Entries without a `chains` field default to `["ethereum"]` (backwards compat).
//!
//! # Error handling
//!
//! Returns `anyhow::Error` on:
//! - File read failure (path not found, permission denied)
//! - TOML parse failure (malformed syntax)
//! - Unknown chain string in `chains` field (`Chain::from_str` error)
//!
//! # Tests
//!
//! Three unit tests at module bottom:
//! - Single-chain entry (backwards compat, no `chains` field → defaults to ethereum)
//! - Multi-chain entry (Inferno on ethereum + bsc + polygon)
//! - Unknown chain string → loader error

use std::path::Path;
use std::str::FromStr;

use anyhow::Context as AnyhowContext;
use mg_onchain_common::chain::Chain;
use mg_onchain_detectors::KnownDrainerSet;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Raw TOML schema structs
// ---------------------------------------------------------------------------

/// Top-level structure of `config/known_drainers.toml`.
#[derive(Debug, Deserialize)]
struct KnownDrainersToml {
    #[serde(rename = "drainers", default)]
    drainers: Vec<DrainerEntryRaw>,
}

/// One `[[drainers]]` entry in the TOML file.
#[derive(Debug, Deserialize)]
struct DrainerEntryRaw {
    /// Human-readable name ("Inferno Drainer", "Pink Drainer", etc.)
    #[allow(dead_code)]
    name: String,

    /// EVM chain names this cluster operates on.
    ///
    /// When absent, defaults to `["ethereum"]` (backwards compat).
    #[serde(default)]
    chains: Vec<String>,

    /// Known contract/wallet addresses for this cluster.
    #[serde(default)]
    addresses: Vec<String>,

    /// Source citation (informational only — not used at runtime).
    #[allow(dead_code)]
    #[serde(default)]
    source: String,
}

// ---------------------------------------------------------------------------
// Public loader
// ---------------------------------------------------------------------------

/// Load `known_drainers.toml` and build a multi-chain `KnownDrainerSet`.
///
/// # Multi-chain behaviour
///
/// Each `[[drainers]]` entry's `chains` field is parsed via `Chain::from_str`.
/// If the field is absent, `["ethereum"]` is used as the default (backwards compat).
/// Each address is registered for every chain listed — this is what populates
/// the per-chain `HashSet` inside `KnownDrainerSet`, enabling chain-aware A1 signal
/// matching (e.g. Inferno drainer only fires on BSC if BSC is in the `chains` field).
///
/// # Errors
///
/// Returns `Err` on file I/O failure, TOML parse failure, or unknown chain string.
pub fn load_known_drainers(path: &Path) -> anyhow::Result<KnownDrainerSet> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read known_drainers.toml at {}", path.display()))?;

    let raw: KnownDrainersToml = toml::from_str(&content)
        .with_context(|| format!("failed to parse known_drainers.toml at {}", path.display()))?;

    // Build the list of (chains, address) pairs for KnownDrainerSet::from_chain_entries.
    // We accumulate into a Vec to avoid lifetime issues with the closure.
    let mut chain_addr_pairs: Vec<(Vec<Chain>, String)> = Vec::new();

    for entry in raw.drainers {
        // Parse chain strings → Chain variants.
        // Default: ["ethereum"] when `chains` field is absent/empty.
        let chain_strings: Vec<String> = if entry.chains.is_empty() {
            vec!["ethereum".to_string()]
        } else {
            entry.chains
        };

        let mut chains: Vec<Chain> = Vec::with_capacity(chain_strings.len());
        for chain_str in &chain_strings {
            let chain = Chain::from_str(chain_str).with_context(|| {
                format!(
                    "unknown chain '{}' in known_drainers.toml entry '{}'",
                    chain_str, entry.name
                )
            })?;
            chains.push(chain);
        }

        for addr in entry.addresses {
            chain_addr_pairs.push((chains.clone(), addr));
        }
    }

    // Build slice-of-refs form required by KnownDrainerSet::from_chain_entries.
    let entries: Vec<(&[Chain], &String)> = chain_addr_pairs
        .iter()
        .map(|(chains, addr)| (chains.as_slice(), addr))
        .collect();

    Ok(KnownDrainerSet::from_chain_entries(&entries))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Write TOML content to a temp file and return the path.
    /// Uses CARGO_TARGET_TMPDIR or std::env::temp_dir() — no tempfile crate needed.
    fn write_toml_to_temp(content: &str, suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mg_drainers_test_{suffix}.toml"));
        std::fs::write(&path, content).expect("write temp TOML must succeed");
        path
    }

    /// Single-chain entry with explicit `chains = ["ethereum"]`.
    ///
    /// Backwards compat: address is registered for Ethereum, not BSC/Polygon.
    #[test]
    fn single_chain_entry_registers_for_ethereum_only() {
        let toml = r#"
[[drainers]]
name    = "Pink Drainer"
chains  = ["ethereum"]
addresses = ["0x54d3b81a58d5b1fc51c620e2c8f5ea0c97c9c2ab"]
source  = "ZachXBT"
"#;
        let path = write_toml_to_temp(toml, "single_chain");
        let set = load_known_drainers(&path).expect("load must succeed");

        // Present for Ethereum.
        assert!(
            set.contains_for_chain(Chain::Ethereum, "0x54d3b81a58d5b1fc51c620e2c8f5ea0c97c9c2ab"),
            "Pink Drainer must be registered for Ethereum"
        );
        // Absent for BSC.
        assert!(
            !set.contains_for_chain(Chain::Bsc, "0x54d3b81a58d5b1fc51c620e2c8f5ea0c97c9c2ab"),
            "Pink Drainer must NOT be registered for BSC"
        );
        // Flat contains() still works.
        assert!(
            set.contains("0x54d3b81a58d5b1fc51c620e2c8f5ea0c97c9c2ab"),
            "flat contains() must return true for backwards compat"
        );
    }

    /// No `chains` field → defaults to ethereum (backwards compat).
    #[test]
    fn missing_chains_field_defaults_to_ethereum() {
        let toml = r#"
[[drainers]]
name    = "Monkey Drainer"
addresses = ["0x0000000000000000000000000000000000000002"]
source  = "PeckShield"
"#;
        let path = write_toml_to_temp(toml, "missing_chains");
        let set = load_known_drainers(&path).expect("load must succeed");

        // Must default to Ethereum.
        assert!(
            set.contains_for_chain(Chain::Ethereum, "0x0000000000000000000000000000000000000002"),
            "missing chains field must default to ethereum"
        );
        assert!(
            !set.contains_for_chain(Chain::Bsc, "0x0000000000000000000000000000000000000002"),
            "missing chains field must NOT include BSC"
        );
    }

    /// Multi-chain entry: Inferno on ethereum + bsc + polygon.
    ///
    /// This is the primary motivation for the TOML loader — Inferno actually ran
    /// on all three chains and the per-chain lookup must reflect that.
    #[test]
    fn multi_chain_entry_inferno_populates_all_chains() {
        let toml = r#"
[[drainers]]
name    = "Inferno Drainer"
chains  = ["ethereum", "bsc", "polygon"]
addresses = ["0x3c116dedca98c1813eadb17b71e869c0faba0f5e"]
source  = "Scam Sniffer 2023-12-23"
"#;
        let path = write_toml_to_temp(toml, "multi_chain_inferno");
        let set = load_known_drainers(&path).expect("load must succeed");

        let addr = "0x3c116dedca98c1813eadb17b71e869c0faba0f5e";
        assert!(set.contains_for_chain(Chain::Ethereum, addr), "must be in Ethereum set");
        assert!(set.contains_for_chain(Chain::Bsc, addr), "must be in BSC set");
        assert!(set.contains_for_chain(Chain::Polygon, addr), "must be in Polygon set");
        // Not registered for Base or Arbitrum.
        assert!(!set.contains_for_chain(Chain::Base, addr), "must NOT be in Base set");
        assert!(!set.contains_for_chain(Chain::Arbitrum, addr), "must NOT be in Arbitrum set");
        // Flat contains() still works.
        assert!(set.contains(addr), "flat contains() must return true");
    }

    /// Unknown chain string → loader error (not a panic, not a silent skip).
    #[test]
    fn unknown_chain_string_returns_error() {
        let toml = r#"
[[drainers]]
name    = "Fake Drainer"
chains  = ["ethereum", "notachain"]
addresses = ["0x1234567890000000000000000000000000000001"]
source  = "test"
"#;
        let path = write_toml_to_temp(toml, "unknown_chain_err");
        let result = load_known_drainers(&path);
        assert!(
            result.is_err(),
            "unknown chain string 'notachain' must produce a loader error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("notachain"),
            "error message must mention the unknown chain: {err}"
        );
    }
}
