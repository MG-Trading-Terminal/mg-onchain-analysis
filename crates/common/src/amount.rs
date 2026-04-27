//! Amount encoding and conversion utilities.
//!
//! # Invariant
//!
//! This module enforces the `CLAUDE.md` §Code Style rule:
//! **NEVER `f64` for prices, amounts, supplies, or liquidity.**
//!
//! # Two tiers
//!
//! - **`u128` — raw on-chain units.** Every on-chain ledger stores amounts as
//!   unsigned integers with a fixed decimal exponent (`decimals: u8`). `u128` is
//!   the universal raw type. Solana SPL amounts fit in `u64`, but Token-2022
//!   extensions can produce larger values — `u128` is the safe default for Phase 1.
//!
//!   Phase 4 note: EVM `uint256` can theoretically exceed `u128::MAX` (e.g.
//!   governance tokens with 18 decimals and absurdly high supply). When Phase 4
//!   EVM chains are activated, evaluate introducing a `RawAmount` newtype that
//!   can hold both `u128` (Solana) and a string-encoded U256 (EVM). Until then,
//!   the chain-adapter layer truncates or errors on values exceeding `u128::MAX`.
//!
//! - **`rust_decimal::Decimal` — human-scaled quantities.** Use for USD values,
//!   percentage fields (`lp_burned_pct`, holder concentration ratios, tax rates),
//!   and any value stored in Postgres as `NUMERIC` or presented to a human.
//!
//! # JSON serialization
//!
//! All amount fields serialize as **strings**, never JSON numbers.
//!
//! JSON numbers are IEEE-754 double-precision. A `u128` can represent values up to
//! `2^128 - 1` (39 decimal digits), far exceeding the 15–17 significant digits of
//! `f64`. A token with 18 decimals and `10^9` total supply has a raw unit count of
//! `10^27`, which loses precision when encoded as a JSON number.
//!
//! The `rust_decimal` crate's `serde-with-str` feature handles `Decimal` fields
//! automatically. Raw `u128` fields use the custom serializer in this module.
//!
//! # Example
//!
//! ```rust
//! use mg_onchain_common::amount::{raw_to_decimal, decimal_to_raw};
//! use rust_decimal::Decimal;
//!
//! let raw: u128 = 1_000_000_000; // 1.0 SOL (9 decimals)
//! let human: Decimal = raw_to_decimal(raw, 9);
//! assert_eq!(human.to_string(), "1");
//!
//! let back: u128 = decimal_to_raw(human, 9).unwrap();
//! assert_eq!(back, raw);
//! ```

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serializer};

use crate::error::CommonError;

/// Convert a raw on-chain integer amount to a human-scaled [`Decimal`].
///
/// `decimals` is the token's decimal exponent (e.g., 9 for SOL, 6 for USDC on
/// Solana, 18 for most EVM tokens). The result is `raw / 10^decimals`.
///
/// **Precision:** `Decimal` supports up to 28 significant digits. For tokens with
/// 18 decimals and a `u128::MAX` supply this can exceed the supported range —
/// callers should verify that `raw` is reasonable for the token's supply before
/// calling this function.
///
/// # Example
/// ```rust
/// use mg_onchain_common::amount::raw_to_decimal;
/// use rust_decimal::Decimal;
///
/// let human = raw_to_decimal(1_000_000, 6); // 1.0 USDC
/// assert_eq!(human.to_string(), "1");
/// ```
pub fn raw_to_decimal(raw: u128, decimals: u8) -> Decimal {
    if decimals == 0 {
        return Decimal::from(raw);
    }
    let divisor = Decimal::from(10u64.pow(u32::from(decimals)));
    Decimal::from(raw) / divisor
}

/// Convert a human-scaled [`Decimal`] back to raw on-chain units.
///
/// Returns [`CommonError::InvalidAmount`] if:
/// - The value is negative.
/// - The value has more fractional digits than `decimals` allows (would require
///   truncation — callers must round before calling).
/// - The scaled value overflows `u128`.
///
/// # Example
/// ```rust
/// use mg_onchain_common::amount::decimal_to_raw;
/// use rust_decimal::Decimal;
///
/// let raw = decimal_to_raw(Decimal::new(1, 0), 9).unwrap(); // 1.0 SOL
/// assert_eq!(raw, 1_000_000_000u128);
/// ```
pub fn decimal_to_raw(amount: Decimal, decimals: u8) -> Result<u128, CommonError> {
    if amount.is_sign_negative() {
        return Err(CommonError::InvalidAmount(format!(
            "amount must be non-negative, got {amount}"
        )));
    }
    let multiplier = Decimal::from(10u64.pow(u32::from(decimals)));
    let scaled = amount * multiplier;
    // Reject if there are fractional digits left (would require truncation)
    if scaled.fract() != Decimal::ZERO {
        return Err(CommonError::InvalidAmount(format!(
            "amount {amount} has more fractional digits than {decimals} decimals allow"
        )));
    }
    scaled.to_u128().ok_or_else(|| {
        CommonError::InvalidAmount(format!("amount {amount} overflows u128"))
    })
}

/// Serde serializer for `u128` amounts as strings.
///
/// Use with `#[serde(serialize_with = "mg_onchain_common::amount::serialize_u128_as_str")]`
/// on struct fields.
///
/// # Example
/// ```rust
/// use serde::Serialize;
/// use serde_json;
///
/// #[derive(Serialize)]
/// struct Probe {
///     #[serde(serialize_with = "mg_onchain_common::amount::serialize_u128_as_str")]
///     amount: u128,
/// }
/// let p = Probe { amount: u128::MAX };
/// let json = serde_json::to_string(&p).unwrap();
/// assert!(json.contains('"'), "u128 must be a JSON string, not a number");
/// assert_eq!(json, r#"{"amount":"340282366920938463463374607431768211455"}"#);
/// ```
pub fn serialize_u128_as_str<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// Serde deserializer for `u128` amounts from strings.
///
/// Use with `#[serde(deserialize_with = "mg_onchain_common::amount::deserialize_u128_from_str")]`
/// on struct fields.
pub fn deserialize_u128_from_str<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<u128>().map_err(serde::de::Error::custom)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    #[test]
    fn raw_to_decimal_sol() {
        // 1.0 SOL = 1_000_000_000 lamports
        let d = raw_to_decimal(1_000_000_000, 9);
        assert_eq!(d, Decimal::new(1, 0));
    }

    #[test]
    fn raw_to_decimal_zero_decimals() {
        let d = raw_to_decimal(42, 0);
        assert_eq!(d, Decimal::from(42u32));
    }

    #[test]
    fn raw_to_decimal_usdc() {
        // 1.0 USDC on Solana = 1_000_000 (6 decimals)
        let d = raw_to_decimal(1_000_000, 6);
        assert_eq!(d.to_string(), "1");
    }

    #[test]
    fn raw_to_decimal_fractional() {
        let d = raw_to_decimal(1_500_000, 6); // 1.5 USDC
        // Decimal division may preserve trailing zeros (e.g. "1.50"); normalize for comparison.
        assert_eq!(d.normalize().to_string(), "1.5");
    }

    #[test]
    fn decimal_to_raw_roundtrip() {
        let raw_in: u128 = 1_000_000_000;
        let human = raw_to_decimal(raw_in, 9);
        let raw_out = decimal_to_raw(human, 9).unwrap();
        assert_eq!(raw_in, raw_out);
    }

    #[test]
    fn decimal_to_raw_negative_errors() {
        let err = decimal_to_raw(Decimal::new(-1, 0), 9).unwrap_err();
        assert!(matches!(err, CommonError::InvalidAmount(_)));
    }

    #[test]
    fn decimal_to_raw_too_many_fractional_digits_errors() {
        // 0.0000000001 has 10 fractional digits but SOL only has 9
        let too_precise = Decimal::new(1, 10); // 0.0000000001
        let err = decimal_to_raw(too_precise, 9).unwrap_err();
        assert!(matches!(err, CommonError::InvalidAmount(_)));
    }

    #[test]
    fn serialize_u128_max_as_string() {
        use serde::Serialize;

        #[derive(Serialize)]
        struct Probe {
            #[serde(serialize_with = "serialize_u128_as_str")]
            amount: u128,
        }
        let p = Probe { amount: u128::MAX };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains('"'), "u128 must be a JSON string, not a number");
        assert_eq!(
            json,
            r#"{"amount":"340282366920938463463374607431768211455"}"#
        );
    }

    #[test]
    fn deserialize_u128_from_string() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Probe {
            #[serde(deserialize_with = "deserialize_u128_from_str")]
            amount: u128,
        }
        let json = r#"{"amount":"1000000000"}"#;
        let p: Probe = serde_json::from_str(json).unwrap();
        assert_eq!(p.amount, 1_000_000_000u128);
        let _ = p.amount; // suppress dead_code in test
    }

    #[test]
    fn deserialize_u128_invalid_string_errors() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Probe {
            #[serde(deserialize_with = "deserialize_u128_from_str")]
            amount: u128,
        }
        let json = r#"{"amount":"not-a-number"}"#;
        assert!(serde_json::from_str::<Probe>(json).is_err());
    }
}
