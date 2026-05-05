//! Solana 32-byte blockhash with base58 encoding/decoding.
//!
//! A `Hash` value is used as the `recent_blockhash` field in transactions.
//! It is opaque from the service's perspective: received from the RPC, passed
//! to the transaction builder, and replaced by `replace_recent_blockhash: true`
//! in simulation calls.  We need only `FromStr` (base58) and `Default`.
//!
//! # Serde representation
//!
//! Serialises as a base58 string (no prefix).  Deserialises from the same form.
//!
//! # Reference
//!
//! reference: solana_sdk::hash::Hash (Apache-2.0) — type layout and base58
//!            encoding convention consulted.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned when constructing a [`Hash`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HashError {
    /// The decoded byte length was not exactly 32.
    #[error("hash must decode to 32 bytes (got {0})")]
    WrongLength(usize),
    /// The string contained characters outside the base58 alphabet.
    #[error("invalid base58 in hash: {0}")]
    InvalidBase58(String),
}

// ---------------------------------------------------------------------------
// Hash
// ---------------------------------------------------------------------------

/// A 32-byte Solana blockhash.
///
/// `Display` / `Serialize` produce the canonical base58 string (no prefix).
/// `FromStr` / `Deserialize` accept any valid base58 string that decodes to
/// exactly 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// The all-zero hash.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from a raw 32-byte array.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return a reference to the raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode to a base58 string.
    #[must_use]
    pub fn to_base58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }

    fn from_base58(s: &str) -> Result<Self, HashError> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| HashError::InvalidBase58(e.to_string()))?;
        if decoded.len() != 32 {
            return Err(HashError::WrongLength(decoded.len()));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&decoded);
        Ok(Self(bytes))
    }
}

// ---------------------------------------------------------------------------
// From / Into
// ---------------------------------------------------------------------------

impl From<[u8; 32]> for Hash {
    #[inline]
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Hash {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_base58())
    }
}

/// Display as the canonical base58 string (no prefix).
impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

// ---------------------------------------------------------------------------
// FromStr
// ---------------------------------------------------------------------------

impl FromStr for Hash {
    type Err = HashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_base58(s)
    }
}

// ---------------------------------------------------------------------------
// Serde: base58 string
// ---------------------------------------------------------------------------

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_hash_round_trip() {
        let h = Hash::ZERO;
        let s = h.to_string();
        let parsed: Hash = s.parse().expect("round-trip parse failed");
        assert_eq!(h, parsed);
    }

    #[test]
    fn from_bytes_and_display() {
        let bytes: [u8; 32] = [
            0x9b, 0x15, 0x63, 0xa6, 0x5d, 0x19, 0x4c, 0xf6,
            0x79, 0x81, 0xf0, 0x2c, 0x3a, 0x97, 0x71, 0x62,
            0xf9, 0x1e, 0x08, 0xde, 0xf2, 0x66, 0x23, 0xb7,
            0xce, 0x1b, 0xe8, 0x7e, 0x5c, 0x12, 0x00, 0xff,
        ];
        let h = Hash::from_bytes(bytes);
        let s = h.to_string();
        // base58 string is non-empty and contains only valid characters
        assert!(!s.is_empty());
        let parsed: Hash = s.parse().expect("round-trip parse failed");
        assert_eq!(h, parsed);
    }

    #[test]
    fn parse_wrong_length_errors() {
        let result = "abc".parse::<Hash>();
        // "abc" in base58 decodes to only 2 bytes
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_base58_errors() {
        let result = "0000".parse::<Hash>();
        assert!(matches!(result, Err(HashError::InvalidBase58(_))));
    }

    #[test]
    fn serde_round_trip() {
        let h = Hash::from_bytes([0x42; 32]);
        let json = serde_json::to_string(&h).unwrap();
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn default_is_zero() {
        let h = Hash::default();
        assert_eq!(h, Hash::ZERO);
    }
}
