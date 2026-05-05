//! 256-bit integer types.
//!
//! `U256`: re-exported from `primitive_types` (admitted under ADR 0006 Rule A as
//! a mathematical primitive with no EVM-specific semantics).
//!
//! `I256`: two's-complement signed 256-bit integer, written in-tree on top of
//! `U256`.  `primitive-types` v0.14 does not ship an `I256`, so we write our
//! own.  The implementation follows the standard two's-complement rules:
//! - Bit 255 is the sign bit.
//! - Negative range: −2^255 … −1.
//! - Positive range:  0 … 2^255 − 1.
//!
//! reference: alloy_primitives::I256 (MIT/Apache-2.0) — two's-complement
//!            representation and sign extraction logic consulted.

use std::fmt;

pub use primitive_types::U256;

// ---------------------------------------------------------------------------
// U256 helpers
// ---------------------------------------------------------------------------

/// Helpers added as free functions so we don't need to depend on additional
/// `primitive-types` feature flags.
pub mod u256_ext {
    use super::U256;

    /// Parse a U256 from a big-endian byte slice.
    ///
    /// `primitive_types::U256::from_big_endian` requires a 32-byte slice.
    /// This variant accepts any length ≤ 32, padding with leading zeros.
    ///
    /// Panics if `slice.len() > 32`.
    #[must_use]
    pub fn from_be_slice(slice: &[u8]) -> U256 {
        assert!(slice.len() <= 32, "U256::from_be_slice: slice too long ({})", slice.len());
        let mut padded = [0u8; 32];
        padded[32 - slice.len()..].copy_from_slice(slice);
        U256::from_big_endian(&padded)
    }

    /// Serialize a U256 to a big-endian 32-byte array.
    ///
    /// Note: `primitive-types` v0.14 changed `to_big_endian` to return `[u8; 32]`
    /// rather than writing into a provided slice.
    #[must_use]
    pub fn to_be_bytes(u: &U256) -> [u8; 32] {
        u.to_big_endian()
    }

    /// Parse from `"0x"` + hex string (big-endian, variable width up to 32 bytes).
    ///
    /// Returns `None` on parse failure.
    #[must_use]
    pub fn from_hex_str(s: &str) -> Option<U256> {
        let stripped = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        if stripped.is_empty() || stripped.len() > 64 {
            return None;
        }
        // Pad to even length for hex::decode.
        let padded_len = if stripped.len().is_multiple_of(2) { stripped.len() } else { stripped.len() + 1 };
        let padded = format!("{:0>width$}", stripped, width = padded_len);
        let bytes = hex::decode(&padded).ok()?;
        Some(from_be_slice(&bytes))
    }
}

// ---------------------------------------------------------------------------
// I256 — two's-complement signed 256-bit integer
// ---------------------------------------------------------------------------

/// A two's-complement signed 256-bit integer.
///
/// Storage: always as a `U256` in two's-complement representation.
/// The sign bit is `U256` bit 255 (the most significant bit).
///
/// Ethereum ABI uses I256 for:
/// - Uniswap V3 `amount0` / `amount1` in Swap events (signed deltas)
/// - Any `int256` ABI parameter
///
/// reference: alloy_primitives::I256 (MIT/Apache-2.0) — two's-complement
///            sign bit extraction and negation logic consulted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct I256(pub U256);

/// The 256-bit two's complement sign bit mask.
/// reference: alloy_primitives::I256::SIGN_BIT (MIT/Apache-2.0)
const SIGN_BIT: U256 = U256([0, 0, 0, 0x8000_0000_0000_0000]);

impl I256 {
    /// Zero.
    pub const ZERO: Self = Self(U256([0, 0, 0, 0]));

    /// The minimum value (−2^255 — bit 255 set, all others zero).
    pub const MIN: Self = Self(SIGN_BIT);

    /// The maximum value (2^255 − 1 — all bits set except bit 255).
    pub const MAX: Self = Self(U256([
        u64::MAX,
        u64::MAX,
        u64::MAX,
        0x7FFF_FFFF_FFFF_FFFF,
    ]));

    /// Construct from a `U256` in two's-complement representation (raw).
    #[inline]
    pub const fn from_raw(raw: U256) -> Self {
        Self(raw)
    }

    /// Return the raw two's-complement `U256`.
    #[inline]
    pub const fn into_raw(self) -> U256 {
        self.0
    }

    /// Return `true` if the value is negative (sign bit set).
    #[inline]
    pub fn is_negative(&self) -> bool {
        self.0 >= SIGN_BIT
    }

    /// Return `true` if the value is zero.
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Return `true` if the value is strictly greater than zero.
    #[inline]
    pub fn is_positive(&self) -> bool {
        !self.is_negative() && !self.is_zero()
    }

    /// Return the absolute value as a U256.
    ///
    /// For non-negative values: just the raw U256.
    /// For negative values: two's-complement negation (`~x + 1`).
    #[must_use]
    pub fn abs_as_u256(&self) -> U256 {
        if self.is_negative() {
            // Two's complement negation: !x + 1
            let (negated, _) = (!self.0).overflowing_add(U256::one());
            negated
        } else {
            self.0
        }
    }

    /// Construct from a non-negative `i64` value.
    #[inline]
    pub fn from_i64(v: i64) -> Self {
        if v >= 0 {
            Self(U256::from(v as u64))
        } else {
            // Negative: two's-complement 256-bit
            let abs = (-(v as i128)) as u64;
            let pos = U256::from(abs);
            // Negate: !pos + 1
            let (negated, _) = (!pos).overflowing_add(U256::one());
            Self(negated)
        }
    }
}

impl fmt::Debug for I256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "I256({self})")
    }
}

impl fmt::Display for I256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_negative() {
            let abs = self.abs_as_u256();
            write!(f, "-{abs}")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i256_zero() {
        let z = I256::ZERO;
        assert!(!z.is_negative());
        assert!(z.is_zero());
        assert_eq!(z.to_string(), "0");
    }

    #[test]
    fn i256_positive() {
        let v = I256::from_i64(42);
        assert!(!v.is_negative());
        assert_eq!(v.to_string(), "42");
    }

    #[test]
    fn i256_negative_one() {
        let v = I256::from_i64(-1);
        assert!(v.is_negative());
        assert_eq!(v.abs_as_u256(), U256::one());
    }

    #[test]
    fn i256_negative_large() {
        let v = I256::from_i64(-1_000_000);
        assert!(v.is_negative());
        assert_eq!(v.abs_as_u256(), U256::from(1_000_000u64));
    }

    #[test]
    fn i256_min() {
        // MIN is exactly −2^255 (sign bit only)
        assert!(I256::MIN.is_negative());
    }

    #[test]
    fn i256_max() {
        assert!(!I256::MAX.is_negative());
    }

    #[test]
    fn u256_from_be_slice_padding() {
        let b = u256_ext::from_be_slice(&[1]);
        assert_eq!(b, U256::one());
    }

    #[test]
    fn u256_to_be_bytes_round_trip() {
        let v = U256::from(0xdead_beefu64);
        let bytes = u256_ext::to_be_bytes(&v);
        let back = u256_ext::from_be_slice(&bytes);
        assert_eq!(v, back);
    }

    #[test]
    fn u256_from_hex_str() {
        let v = u256_ext::from_hex_str("0xff").unwrap();
        assert_eq!(v, U256::from(255u64));
    }

    #[test]
    fn u256_from_hex_str_no_prefix() {
        let v = u256_ext::from_hex_str("0a").unwrap();
        assert_eq!(v, U256::from(10u64));
    }

    #[test]
    fn u256_from_hex_str_empty() {
        assert!(u256_ext::from_hex_str("").is_none());
    }
}
