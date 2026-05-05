//! EVM 20-byte address with EIP-55 mixed-case checksum encoding.
//!
//! # EIP-55
//!
//! EIP-55 computes `keccak256` of the lowercase hex string of the address, then
//! uppercases each hex digit whose corresponding nibble in the hash is >= 8.
//!
//! Reference: https://eips.ethereum.org/EIPS/eip-55
//! reference: alloy_primitives::Address::to_checksum (MIT/Apache-2.0) — consulted
//!            for the checksum algorithm implementation details.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::keccak::keccak256;

// ---------------------------------------------------------------------------
// Address
// ---------------------------------------------------------------------------

/// A 20-byte EVM address.
///
/// Display / Serialize always uses EIP-55 checksum encoding.
/// Parse / Deserialize accepts both checksummed and unchecksummed hex (with or
/// without `0x` prefix).  After parse the bytes are stored canonically; the
/// checksum is re-applied on every Display call.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 20]);

/// Error returned when parsing an `Address` from a hex string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    /// The hex string had the wrong number of characters (not 40).
    #[error("address must be 40 hex characters (got {0})")]
    WrongLength(usize),
    /// The hex string contained a non-hex character.
    #[error("invalid hex in address: {0}")]
    InvalidHex(String),
}

impl Address {
    /// The zero address `0x0000…0000`.
    pub const ZERO: Self = Self([0u8; 20]);

    /// Construct from raw bytes.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Return the raw 20-byte array.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Return the EIP-55 checksummed hex string **without** `0x` prefix.
    ///
    /// reference: alloy_primitives::Address::to_checksum (MIT/Apache-2.0)
    #[must_use]
    pub fn to_checksum(&self) -> String {
        // Lower-case hex of the 20 bytes — the input to keccak256 for EIP-55.
        let lower_hex = hex::encode(self.0);
        let hash = keccak256(lower_hex.as_bytes());

        // For each nibble position i in lower_hex:
        //   if hash_nibble(i) >= 8 → uppercase; else lowercase.
        let mut out = String::with_capacity(40);
        for (i, ch) in lower_hex.chars().enumerate() {
            let hash_byte = hash.0[i / 2];
            let nibble = if i % 2 == 0 { hash_byte >> 4 } else { hash_byte & 0x0f };
            if nibble >= 8 {
                out.extend(ch.to_uppercase());
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Return the `0x`-prefixed EIP-55 checksum string.
    #[must_use]
    pub fn to_checksum_0x(&self) -> String {
        format!("0x{}", self.to_checksum())
    }

    /// Parse from a hex string (with or without `0x` prefix, any case).
    fn parse_hex(s: &str) -> Result<Self, AddressError> {
        let stripped = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        if stripped.len() != 40 {
            return Err(AddressError::WrongLength(stripped.len()));
        }
        let mut bytes = [0u8; 20];
        hex::decode_to_slice(stripped, &mut bytes)
            .map_err(|e| AddressError::InvalidHex(e.to_string()))?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address(0x{})", self.to_checksum())
    }
}

/// Display as `0x` + EIP-55 checksummed hex.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", self.to_checksum())
    }
}

/// Lowercase hex (no checksum).  `{:x}` → 40 hex chars; `{:#x}` → `0x` + 40 hex chars.
impl fmt::LowerHex for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.write_str("0x")?;
        }
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Uppercase hex (no checksum).  `{:X}` → 40 hex chars; `{:#X}` → `0x` + 40 hex chars.
impl fmt::UpperHex for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.write_str("0x")?;
        }
        for byte in &self.0 {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl FromStr for Address {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(s)
    }
}

// ---------------------------------------------------------------------------
// Serde: serialize as EIP-55 "0x..." string; deserialize from any hex string
// ---------------------------------------------------------------------------

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_checksum_0x())
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // EIP-55 specification test vectors (from https://eips.ethereum.org/EIPS/eip-55)
    // reference: EIP-55 spec (public spec, no license restriction)

    #[test]
    fn eip55_vector_1() {
        // "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        let addr: Address = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed");
    }

    #[test]
    fn eip55_vector_2() {
        // "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359"
        let addr: Address = "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359");
    }

    #[test]
    fn eip55_vector_3() {
        // "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB"
        let addr: Address = "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB");
    }

    #[test]
    fn eip55_vector_4() {
        // "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb"
        let addr: Address = "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb");
    }

    #[test]
    fn eip55_lowercase_input_is_normalised() {
        // Lowercase parse → same checksum output
        let addr: Address = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed");
    }

    #[test]
    fn address_zero() {
        assert_eq!(Address::ZERO.to_string(), "0x0000000000000000000000000000000000000000");
    }

    #[test]
    fn address_parse_no_prefix() {
        let addr: Address = "5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".parse().unwrap();
        assert_eq!(addr.to_checksum_0x(), "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed");
    }

    #[test]
    fn address_parse_wrong_length() {
        let err = "0xabc".parse::<Address>().unwrap_err();
        assert!(matches!(err, AddressError::WrongLength(3)));
    }

    #[test]
    fn address_parse_invalid_hex() {
        let err = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeX".parse::<Address>().unwrap_err();
        assert!(matches!(err, AddressError::InvalidHex(_)));
    }

    #[test]
    fn address_serde_round_trip() {
        let addr: Address = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".parse().unwrap();
        let json = serde_json::to_string(&addr).unwrap();
        // JSON should be a checksummed string
        assert!(json.contains("5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"));
        let decoded: Address = serde_json::from_str(&json).unwrap();
        assert_eq!(addr, decoded);
    }

    #[test]
    fn address_equality_is_byte_level() {
        let a: Address = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed".parse().unwrap();
        let b: Address = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".parse().unwrap();
        assert_eq!(a, b); // same bytes regardless of input case
    }
}
