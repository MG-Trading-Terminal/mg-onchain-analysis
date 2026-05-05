//! `B256` — a 32-byte hash type used for block hashes, tx hashes, and event
//! topic0 values.
//!
//! Serialises as a lowercase `0x`-prefixed hex string.
//! Deserialises from any case hex string (with or without `0x` prefix).
//!
//! reference: alloy_primitives::B256 (MIT/Apache-2.0) — type layout consulted.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------------------------------------------------------------------------
// B256
// ---------------------------------------------------------------------------

/// A 32-byte hash / topic value.
///
/// This is used for:
/// - Block hashes
/// - Transaction hashes
/// - Event topics (topic0 = signature hash, topic1..n = indexed params)
/// - Any other EVM 32-byte "word"
///
/// # Serde representation
///
/// JSON: lowercase `"0x"` + 64 hex characters.
/// Incoming JSON may use any case and the `0x` prefix is optional.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct B256(pub [u8; 32]);

/// Error returned when parsing a `B256` from a hex string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum B256Error {
    /// The hex string had the wrong number of characters (not 64).
    #[error("B256 must be 64 hex characters (got {0})")]
    WrongLength(usize),
    /// The hex string contained a non-hex character.
    #[error("invalid hex in B256: {0}")]
    InvalidHex(String),
}

impl B256 {
    /// The zero hash.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from raw bytes.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Construct from a big-endian byte slice (padded with leading zeros if shorter than 32).
    ///
    /// Panics if `slice.len() > 32`.
    #[must_use]
    pub fn from_be_slice(slice: &[u8]) -> Self {
        assert!(slice.len() <= 32, "B256::from_be_slice: slice too long");
        let mut out = [0u8; 32];
        out[32 - slice.len()..].copy_from_slice(slice);
        Self(out)
    }

    /// Return raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn parse_hex(s: &str) -> Result<Self, B256Error> {
        let stripped = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        if stripped.len() != 64 {
            return Err(B256Error::WrongLength(stripped.len()));
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(stripped, &mut bytes)
            .map_err(|e| B256Error::InvalidHex(e.to_string()))?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for B256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "B256(0x{})", hex::encode(self.0))
    }
}

/// Display as lowercase `0x` + 64 hex chars.
impl fmt::Display for B256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl FromStr for B256 {
    type Err = B256Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(s)
    }
}

impl Serialize for B256 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Always lowercase to match Ethereum JSON-RPC output convention.
        s.serialize_str(&format!("0x{}", hex::encode(self.0)))
    }
}

impl<'de> Deserialize<'de> for B256 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b256_zero_display() {
        let z = B256::ZERO;
        assert_eq!(
            z.to_string(),
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn b256_round_trip_parse_display() {
        let s = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let b: B256 = s.parse().unwrap();
        assert_eq!(b.to_string(), s);
    }

    #[test]
    fn b256_parse_uppercase_accepted() {
        let upper = "0xDDF252AD1BE2C89B69C2B068FC378DAA952BA7F163C4A11628F55A4DF523B3EF";
        let b: B256 = upper.parse().unwrap();
        // Display is always lowercase
        assert_eq!(
            b.to_string(),
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn b256_parse_wrong_length() {
        let err = "0xabc".parse::<B256>().unwrap_err();
        assert!(matches!(err, B256Error::WrongLength(3)));
    }

    #[test]
    fn b256_from_be_slice_less_than_32() {
        let slice = &[0xab, 0xcd];
        let b = B256::from_be_slice(slice);
        assert_eq!(b.0[30], 0xab);
        assert_eq!(b.0[31], 0xcd);
        assert_eq!(b.0[0], 0x00);
    }

    #[test]
    fn b256_serde_round_trip() {
        let s = "\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"";
        let b: B256 = serde_json::from_str(s).unwrap();
        let back = serde_json::to_string(&b).unwrap();
        assert_eq!(back, s);
    }
}
