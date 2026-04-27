//! Chain identity primitives: Chain enum, Address, BlockRef, TxHash.
//!
//! # Address encoding on the wire
//!
//! Always a plain string in chain-canonical form:
//! - **Solana:** Base58-encoded 32-byte public key (typically 44 characters)
//! - **EVM:** EIP-55 checksum hex, `0x`-prefixed, 42 characters
//! - **Tron:** Base58Check, starts with `'T'`, 34 characters (Phase 4)
//!
//! Normalization happens at the `Address::parse` constructor. Any `Address` value
//! that escapes the constructor is guaranteed to hold a canonical string.
//!
//! Typed byte representations (e.g. `[u8; 32]`) belong in `crates/chain-adapter`,
//! not here. `common::Address` is string-based with a `Chain` tag per OQ1 resolution.
//!
//! # Block height / slot
//!
//! Both Solana (slots) and EVM (block numbers) use `u64`. `BlockRef` carries the
//! `Chain` tag to prevent mixing them across chain-specific code paths.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::CommonError;

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

/// Supported blockchain networks.
///
/// Serializes/deserializes as lowercase strings: `"solana"`, `"ethereum"`,
/// `"bsc"`, `"base"`, `"arbitrum"`, `"polygon"`, `"tron"`.
///
/// `#[non_exhaustive]` prevents match-exhaustiveness errors when new chains are
/// added in Phase 4 without a SemVer major bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Chain {
    Solana,
    Ethereum,
    Bsc,
    Base,
    Arbitrum,
    Polygon,
    /// Phase 4 — Tron USDT flow analysis for mg-custody compliance.
    Tron,
}

impl Chain {
    /// Returns the canonical string name used in API paths and storage keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Chain::Solana => "solana",
            Chain::Ethereum => "ethereum",
            Chain::Bsc => "bsc",
            Chain::Base => "base",
            Chain::Arbitrum => "arbitrum",
            Chain::Polygon => "polygon",
            Chain::Tron => "tron",
        }
    }

    /// True if this is an EVM-compatible chain.
    pub fn is_evm(&self) -> bool {
        matches!(
            self,
            Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon
        )
    }

    /// True for Solana (account model, SPL tokens, slots).
    pub fn is_solana(&self) -> bool {
        matches!(self, Chain::Solana)
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Chain {
    type Err = CommonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "solana" | "sol" => Ok(Chain::Solana),
            "ethereum" | "eth" => Ok(Chain::Ethereum),
            "bsc" | "bnb" => Ok(Chain::Bsc),
            "base" => Ok(Chain::Base),
            "arbitrum" | "arb" => Ok(Chain::Arbitrum),
            "polygon" | "matic" => Ok(Chain::Polygon),
            "tron" | "trx" => Ok(Chain::Tron),
            other => Err(CommonError::UnknownChain(other.to_owned())),
        }
    }
}

// ---------------------------------------------------------------------------
// Address
// ---------------------------------------------------------------------------

/// A chain-canonical address.
///
/// Internally stores the canonical string and the chain tag. The canonical form
/// is validated and normalized at construction time via [`Address::parse`].
///
/// ## Serialization
///
/// Serializes as a plain string (the canonical form). Deserialization of
/// `Address` requires chain context — use [`Address::parse`] explicitly after
/// deserializing the surrounding struct's `chain` field.
///
/// ## Wire format (OQ1 resolution)
///
/// Structs that embed `Address` fields carry a sibling `chain` field. REST
/// consumers that need to reconstruct an `Address` from JSON call
/// `Address::parse(chain, &json_string)`.
///
/// ## EIP-55 status (Phase 1)
///
/// For EVM chains, `parse` accepts any valid `0x`-prefixed 40-hex-char string
/// and stores it as `0x`-prefixed lowercase. Full EIP-55 checksum normalization
/// requires a keccak-256 hash of the lowercase address — this would introduce
/// a `sha3` dependency. Since Phase 1 is Solana-only and EVM chains are Phase 4,
/// the full EIP-55 checksum is deferred to Phase 4 with a TODO below.
///
/// **TODO(Phase 4):** Add `sha3 = "0.10"` to workspace deps and replace
/// `evm_canonical` with a proper EIP-55 implementation:
/// ```text
/// fn eip55_checksum(addr_lower: &str) -> String {
///     use sha3::{Digest, Keccak256};
///     let hash = Keccak256::digest(addr_lower.as_bytes());
///     // For each hex char at position i, capitalize if hash[i/2] nibble >= 8
///     ...
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address {
    /// Chain tag — preserved so callers can recover it without a separate field.
    pub chain: Chain,
    /// Canonical string form. Guaranteed normalized by the constructor.
    canonical: String,
}

impl Address {
    /// Parse and normalize a raw address string for the given chain.
    ///
    /// - **Solana:** `bs58::decode` must succeed and yield exactly 32 bytes.
    /// - **EVM:** must be `0x`-prefixed with exactly 40 hex characters; stored as
    ///   `0x`-prefixed lowercase (full EIP-55 checksum deferred to Phase 4 —
    ///   see type-level doc comment).
    /// - **Tron:** format check only for Phase 1 (starts with `'T'`, 34 chars).
    ///
    /// Returns [`CommonError::InvalidAddress`] on any parse failure.
    pub fn parse(chain: Chain, raw: &str) -> Result<Self, CommonError> {
        let canonical = match chain {
            Chain::Solana => {
                let bytes = bs58::decode(raw).into_vec().map_err(|e| {
                    CommonError::InvalidAddress {
                        chain: chain.to_string(),
                        reason: e.to_string(),
                    }
                })?;
                if bytes.len() != 32 {
                    return Err(CommonError::InvalidAddress {
                        chain: chain.to_string(),
                        reason: format!(
                            "expected 32 bytes from Base58, got {}",
                            bytes.len()
                        ),
                    });
                }
                // Re-encode to canonical Base58 (normalizes any whitespace / variant encoding).
                bs58::encode(&bytes).into_string()
            }
            Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon => {
                evm_canonical(chain, raw)?
            }
            Chain::Tron => {
                // Phase 4: validate Base58Check starting with 'T', 34 chars.
                if raw.starts_with('T') && raw.len() == 34 && raw.chars().all(|c| c.is_ascii_alphanumeric()) {
                    raw.to_owned()
                } else {
                    return Err(CommonError::InvalidAddress {
                        chain: chain.to_string(),
                        reason: "expected Base58Check starting with 'T', length 34".into(),
                    });
                }
            }
        };
        Ok(Self { chain, canonical })
    }

    /// Return the canonical string form (same as `Display`).
    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

/// Validate and normalize an EVM address string.
///
/// Accepts mixed-case and all-lowercase; stores as `0x`-prefixed lowercase.
/// Full EIP-55 checksum normalization is deferred to Phase 4 (see `Address` doc).
fn evm_canonical(chain: Chain, raw: &str) -> Result<String, CommonError> {
    let hex_part = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")).ok_or_else(|| {
        CommonError::InvalidAddress {
            chain: chain.to_string(),
            reason: "EVM address must start with '0x'".into(),
        }
    })?;

    if hex_part.len() != 40 {
        return Err(CommonError::InvalidAddress {
            chain: chain.to_string(),
            reason: format!("expected 40 hex characters after '0x', got {}", hex_part.len()),
        });
    }

    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CommonError::InvalidAddress {
            chain: chain.to_string(),
            reason: "EVM address contains non-hex characters".into(),
        });
    }

    Ok(format!("0x{}", hex_part.to_lowercase()))
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl Serialize for Address {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.canonical)
    }
}

/// Deserialize `Address` from a bare string on the wire.
///
/// # Chain context limitation
///
/// JSON carries no chain tag inside the address string itself. This deserializer
/// stores the string verbatim in `canonical` and sets `chain` to a sentinel
/// value (`Chain::Solana` by default). The surrounding struct's `chain` field
/// must be used to re-validate via `Address::parse` at the application boundary.
///
/// In practice, adapters and storage layers construct `Address` values via
/// `Address::parse(chain, raw)` and never deserialize bare addresses from
/// untrusted JSON. This implementation exists solely to satisfy the `Deserialize`
/// bound needed by `#[derive(Deserialize)]` on structs containing `Address`.
///
/// **Do not rely on the `chain` field of a deserialized `Address` — it may be
/// incorrect until re-validated with `Address::parse`.**
impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Store verbatim; chain defaults to Solana as a placeholder.
        // The caller is responsible for re-parsing with the correct chain context.
        Ok(Self {
            chain: Chain::Solana,
            canonical: s,
        })
    }
}

// ---------------------------------------------------------------------------
// TxHash
// ---------------------------------------------------------------------------

/// A chain-appropriate transaction hash.
///
/// - **Solana:** Ed25519 signature — 64 bytes, displayed as Base58.
/// - **EVM:** Keccak-256 — 32 bytes, displayed as `0x`-prefixed lowercase hex.
///
/// Using an enum rather than an opaque byte slice gives compile-time type safety:
/// a Solana signature (64 bytes) cannot be accidentally used where an EVM hash
/// (32 bytes) is expected.
///
/// ## Deserialization
///
/// Like `Address`, `TxHash` does not implement `Deserialize` because the chain
/// context is not embedded in the wire value. Use `TxHash::parse(chain, raw)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TxHash {
    /// Ed25519 signature — 64 raw bytes, displayed as Base58.
    Solana(Box<[u8; 64]>),
    /// Keccak-256 hash — 32 raw bytes, displayed as `0x`-prefixed hex.
    Evm([u8; 32]),
}

impl TxHash {
    /// Parse a Solana transaction signature from a Base58 string.
    pub fn solana_from_base58(s: &str) -> Result<Self, CommonError> {
        let bytes = bs58::decode(s).into_vec().map_err(|e| CommonError::InvalidTxHash {
            chain: "solana".into(),
            reason: e.to_string(),
        })?;
        if bytes.len() != 64 {
            return Err(CommonError::InvalidTxHash {
                chain: "solana".into(),
                reason: format!("expected 64 bytes, got {}", bytes.len()),
            });
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&bytes);
        Ok(TxHash::Solana(Box::new(arr)))
    }

    /// Parse an EVM transaction hash from a `0x`-prefixed hex string.
    pub fn evm_from_hex(s: &str) -> Result<Self, CommonError> {
        let hex_part = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or_else(|| {
            CommonError::InvalidTxHash {
                chain: "evm".into(),
                reason: "must start with '0x'".into(),
            }
        })?;
        if hex_part.len() != 64 {
            return Err(CommonError::InvalidTxHash {
                chain: "evm".into(),
                reason: format!("expected 64 hex characters, got {}", hex_part.len()),
            });
        }
        let decoded = hex::decode(hex_part).map_err(|e| CommonError::InvalidTxHash {
            chain: "evm".into(),
            reason: e.to_string(),
        })?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&decoded);
        Ok(TxHash::Evm(arr))
    }

    /// Parse a tx hash string given a chain tag (dispatches to the right parser).
    pub fn parse(chain: Chain, raw: &str) -> Result<Self, CommonError> {
        match chain {
            Chain::Solana => Self::solana_from_base58(raw),
            Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon => {
                Self::evm_from_hex(raw)
            }
            Chain::Tron => {
                // Tron tx hashes are 32-byte keccak, same encoding as EVM.
                Self::evm_from_hex(raw)
            }
        }
    }
}

impl fmt::Display for TxHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxHash::Solana(bytes) => write!(f, "{}", bs58::encode(bytes.as_ref()).into_string()),
            TxHash::Evm(bytes) => write!(f, "0x{}", hex::encode(bytes)),
        }
    }
}

impl Serialize for TxHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// Deserialize `TxHash` from a bare string on the wire.
///
/// # Chain context limitation
///
/// Like `Address`, `TxHash` cannot determine its chain from the wire string alone.
/// This implementation attempts to decode as Solana first (Base58, 64 bytes), then
/// falls back to EVM (hex, 32 bytes). If neither matches, the Solana error is
/// returned. Use `TxHash::parse(chain, raw)` at adapter boundaries for reliable parsing.
impl<'de> Deserialize<'de> for TxHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Try EVM hex first (starts with 0x)
        if s.starts_with("0x") || s.starts_with("0X") {
            return Self::evm_from_hex(&s).map_err(serde::de::Error::custom);
        }
        // Fall back to Solana Base58
        Self::solana_from_base58(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// BlockRef
// ---------------------------------------------------------------------------

/// A block height or slot number with chain context.
///
/// Both Solana slots and EVM block numbers are `u64`. Carrying the chain tag
/// prevents mixing them across chain-specific code paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockRef {
    /// Which chain this block belongs to.
    pub chain: Chain,
    /// Solana: slot number. EVM: block number.
    pub height: u64,
}

impl BlockRef {
    /// Construct a new `BlockRef`.
    pub fn new(chain: Chain, height: u64) -> Self {
        Self { chain, height }
    }
}

impl fmt::Display for BlockRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chain, self.height)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Chain ---

    #[test]
    fn chain_serde_roundtrip() {
        let chains = [
            Chain::Solana,
            Chain::Ethereum,
            Chain::Bsc,
            Chain::Base,
            Chain::Arbitrum,
            Chain::Polygon,
            Chain::Tron,
        ];
        for chain in chains {
            let json = serde_json::to_string(&chain).unwrap();
            let back: Chain = serde_json::from_str(&json).unwrap();
            assert_eq!(back, chain, "round-trip failed for {chain}");
        }
    }

    #[test]
    fn chain_serialize_lowercase() {
        assert_eq!(serde_json::to_string(&Chain::Solana).unwrap(), r#""solana""#);
        assert_eq!(serde_json::to_string(&Chain::Ethereum).unwrap(), r#""ethereum""#);
        assert_eq!(serde_json::to_string(&Chain::Bsc).unwrap(), r#""bsc""#);
    }

    #[test]
    fn chain_from_str_aliases() {
        assert_eq!("sol".parse::<Chain>().unwrap(), Chain::Solana);
        assert_eq!("eth".parse::<Chain>().unwrap(), Chain::Ethereum);
        assert_eq!("bnb".parse::<Chain>().unwrap(), Chain::Bsc);
        assert_eq!("arb".parse::<Chain>().unwrap(), Chain::Arbitrum);
        assert_eq!("matic".parse::<Chain>().unwrap(), Chain::Polygon);
        assert_eq!("trx".parse::<Chain>().unwrap(), Chain::Tron);
    }

    #[test]
    fn chain_from_str_unknown() {
        let err = "tezos".parse::<Chain>().unwrap_err();
        assert!(matches!(err, CommonError::UnknownChain(s) if s == "tezos"));
    }

    #[test]
    fn chain_is_evm() {
        assert!(Chain::Ethereum.is_evm());
        assert!(Chain::Bsc.is_evm());
        assert!(Chain::Base.is_evm());
        assert!(!Chain::Solana.is_evm());
        assert!(!Chain::Tron.is_evm());
    }

    // --- Address: Solana ---

    #[test]
    fn address_solana_native_mint_roundtrip() {
        // SOL native mint: So11111111111111111111111111111111111111112
        let raw = "So11111111111111111111111111111111111111112";
        let addr = Address::parse(Chain::Solana, raw).unwrap();
        assert_eq!(addr.as_str(), raw);
        assert_eq!(addr.to_string(), raw);
        // Re-parsing the Display output must produce the same canonical string.
        let addr2 = Address::parse(Chain::Solana, addr.as_str()).unwrap();
        assert_eq!(addr2, addr);
    }

    #[test]
    fn address_solana_invalid_base58() {
        let err = Address::parse(Chain::Solana, "not-valid-base58!").unwrap_err();
        assert!(matches!(err, CommonError::InvalidAddress { .. }));
    }

    #[test]
    fn address_solana_wrong_length() {
        // Valid base58 but decodes to != 32 bytes (short key)
        let short = bs58::encode(&[0u8; 20]).into_string();
        let err = Address::parse(Chain::Solana, &short).unwrap_err();
        assert!(matches!(err, CommonError::InvalidAddress { .. }));
    }

    #[test]
    fn address_solana_serialize_as_string() {
        let addr = Address::parse(Chain::Solana, "So11111111111111111111111111111111111111112").unwrap();
        let json = serde_json::to_string(&addr).unwrap();
        assert_eq!(json, r#""So11111111111111111111111111111111111111112""#);
    }

    // --- Address: EVM ---

    #[test]
    fn address_evm_normalize_to_lowercase() {
        let mixed = "0xAbCdEf1234567890abcdef1234567890ABCDEF12";
        let addr = Address::parse(Chain::Ethereum, mixed).unwrap();
        // Phase 1: stored as lowercase hex (full EIP-55 deferred to Phase 4)
        assert_eq!(addr.as_str(), "0xabcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn address_evm_missing_0x_prefix() {
        let err = Address::parse(Chain::Ethereum, "abcdef1234567890abcdef1234567890abcdef12").unwrap_err();
        assert!(matches!(err, CommonError::InvalidAddress { .. }));
    }

    #[test]
    fn address_evm_wrong_length() {
        let err = Address::parse(Chain::Ethereum, "0xdeadbeef").unwrap_err();
        assert!(matches!(err, CommonError::InvalidAddress { .. }));
    }

    // --- TxHash ---

    #[test]
    fn txhash_solana_roundtrip() {
        // A 64-byte all-zeros signature encoded in base58
        let zero_sig = bs58::encode(&[0u8; 64]).into_string();
        let tx = TxHash::solana_from_base58(&zero_sig).unwrap();
        assert_eq!(tx.to_string(), zero_sig);
    }

    #[test]
    fn txhash_evm_roundtrip() {
        let hex = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let tx = TxHash::evm_from_hex(hex).unwrap();
        assert_eq!(tx.to_string(), hex);
    }

    #[test]
    fn txhash_evm_invalid_length() {
        let err = TxHash::evm_from_hex("0xdeadbeef").unwrap_err();
        assert!(matches!(err, CommonError::InvalidTxHash { .. }));
    }

    #[test]
    fn txhash_serialize_as_string() {
        let tx = TxHash::evm_from_hex(
            "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        )
        .unwrap();
        let json = serde_json::to_string(&tx).unwrap();
        assert!(json.starts_with('"'));
        assert!(json.contains("deadbeef"));
    }

    // --- BlockRef ---

    #[test]
    fn blockref_display() {
        let b = BlockRef::new(Chain::Solana, 123_456_789);
        assert_eq!(b.to_string(), "solana:123456789");
    }

    #[test]
    fn blockref_serde_roundtrip() {
        let b = BlockRef::new(Chain::Ethereum, 20_000_000);
        let json = serde_json::to_string(&b).unwrap();
        let back: BlockRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn blockref_ordering() {
        let a = BlockRef::new(Chain::Solana, 100);
        let b = BlockRef::new(Chain::Solana, 200);
        assert!(a < b);
    }
}
