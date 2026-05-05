//! Solana 64-byte transaction signature with base58 encoding/decoding.
//!
//! Transaction signatures are the primary identifier used when referencing
//! Solana transactions (analogous to a transaction hash on EVM chains).  They
//! are Ed25519 signatures over the serialised transaction message — 64 raw bytes
//! encoded as a base58 string at the display boundary.
//!
//! # Serde representation
//!
//! Serialises as a base58 string (no prefix).  Deserialises from the same form.
//!
//! # Reference
//!
//! reference: solana_sdk::signature::Signature (Apache-2.0) — type layout and
//!            base58 encoding convention consulted.
//! reference: https://en.bitcoin.it/wiki/Base58Check_encoding — alphabet definition.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned when constructing a [`Signature`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// The decoded byte length was not exactly 64.
    #[error("signature must decode to 64 bytes (got {0})")]
    WrongLength(usize),
    /// The string contained characters outside the base58 alphabet.
    #[error("invalid base58 in signature: {0}")]
    InvalidBase58(String),
}

// ---------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------

/// A 64-byte Solana transaction signature.
///
/// `Display` / `Serialize` produce the canonical base58 string (no prefix).
/// `FromStr` / `Deserialize` accept any valid base58 string that decodes to
/// exactly 64 bytes; strings that decode to a different length are rejected.
///
/// No cryptographic operations are performed on this type — validation is the
/// validator's responsibility.  The type is purely a data carrier for recording
/// and displaying transaction identifiers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Signature(pub [u8; 64]);

// `[u8; 64]` has no stdlib `Default` impl (only arrays up to length 32).
// We implement Default manually returning the all-zero signature.
impl Default for Signature {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl Signature {
    /// The all-zero signature.
    pub const ZERO: Self = Self([0u8; 64]);

    /// Construct from a raw 64-byte array.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// Return a reference to the raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// Encode to a base58 string.
    #[must_use]
    pub fn to_base58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }

    fn from_base58(s: &str) -> Result<Self, SignatureError> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| SignatureError::InvalidBase58(e.to_string()))?;
        if decoded.len() != 64 {
            return Err(SignatureError::WrongLength(decoded.len()));
        }
        let mut bytes = [0u8; 64];
        bytes.copy_from_slice(&decoded);
        Ok(Self(bytes))
    }
}

// ---------------------------------------------------------------------------
// From / Into
// ---------------------------------------------------------------------------

impl From<[u8; 64]> for Signature {
    #[inline]
    fn from(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Signature {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", self.to_base58())
    }
}

/// Display as the canonical base58 string (no prefix).
impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

// ---------------------------------------------------------------------------
// FromStr
// ---------------------------------------------------------------------------

impl FromStr for Signature {
    type Err = SignatureError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_base58(s)
    }
}

// ---------------------------------------------------------------------------
// Serde: base58 string
// ---------------------------------------------------------------------------

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for Signature {
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
    fn zero_signature_round_trip() {
        let sig = Signature::ZERO;
        let s = sig.to_string();
        // All-zero 64 bytes base58-encodes to all '1' characters.
        assert!(s.chars().all(|c| c == '1'));
        let parsed: Signature = s.parse().expect("round-trip parse failed");
        assert_eq!(sig, parsed);
    }

    #[test]
    fn from_bytes_round_trip() {
        let mut bytes = [0u8; 64];
        bytes[0] = 0x01;
        bytes[63] = 0xff;
        let sig = Signature::from_bytes(bytes);
        let s = sig.to_string();
        let parsed: Signature = s.parse().expect("round-trip parse failed");
        assert_eq!(sig, parsed);
    }

    #[test]
    fn parse_wrong_length_errors() {
        // A short base58 string decodes to fewer than 64 bytes.
        let result = "3xQp4".parse::<Signature>();
        assert!(result.is_err());
        if let Err(SignatureError::WrongLength(_)) = result {
            // expected
        } else {
            panic!("expected WrongLength error");
        }
    }

    #[test]
    fn parse_invalid_base58_errors() {
        // '0' is not in the base58 alphabet.
        let s = "0".repeat(88);
        let result = s.parse::<Signature>();
        assert!(matches!(result, Err(SignatureError::InvalidBase58(_))));
    }

    #[test]
    fn serde_round_trip() {
        let mut bytes = [0u8; 64];
        bytes[0] = 0xab;
        bytes[32] = 0xcd;
        let sig = Signature::from_bytes(bytes);
        let json = serde_json::to_string(&sig).unwrap();
        let decoded: Signature = serde_json::from_str(&json).unwrap();
        assert_eq!(sig, decoded);
    }

    #[test]
    fn serde_serialises_as_string() {
        let sig = Signature::ZERO;
        let json = serde_json::to_string(&sig).unwrap();
        // Must be a quoted string, not a JSON array.
        assert!(json.starts_with('"'));
        assert!(json.ends_with('"'));
    }

    #[test]
    fn size_is_64() {
        assert_eq!(std::mem::size_of::<Signature>(), 64);
    }
}
