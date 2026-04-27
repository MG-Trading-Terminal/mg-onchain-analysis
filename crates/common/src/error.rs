//! Parse and validation errors for `crates/common` types.
//!
//! One top-level enum with `#[non_exhaustive]` allows adding variants in minor
//! releases without SemVer breaks. All parse/validation errors produced by this
//! crate are variants of [`CommonError`].

use thiserror::Error;

/// All parse/validation errors produced by `crates/common`.
///
/// Marked `#[non_exhaustive]` so consumers must handle a wildcard arm;
/// this lets us add variants in minor releases.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommonError {
    /// An address string could not be parsed for the given chain.
    #[error("invalid {chain} address: {reason}")]
    InvalidAddress { chain: String, reason: String },

    /// A raw bytes slice had the wrong length for a chain's address type.
    #[error("wrong address byte length: expected {expected}, got {actual}")]
    AddressByteLength { expected: usize, actual: usize },

    /// A confidence value was outside [0.0, 1.0].
    #[error("confidence {value} out of range [0.0, 1.0]")]
    ConfidenceOutOfRange { value: f64 },

    /// A decimal amount string could not be parsed.
    #[error("invalid decimal amount string: {0}")]
    InvalidAmount(String),

    /// A transaction hash string had the wrong format for the given chain.
    #[error("invalid tx hash for {chain}: {reason}")]
    InvalidTxHash { chain: String, reason: String },

    /// A chain string could not be matched to a known variant.
    #[error("unknown chain: {0}")]
    UnknownChain(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_invalid_address() {
        let e = CommonError::InvalidAddress {
            chain: "solana".into(),
            reason: "wrong length".into(),
        };
        assert_eq!(e.to_string(), "invalid solana address: wrong length");
    }

    #[test]
    fn error_display_confidence_out_of_range() {
        let e = CommonError::ConfidenceOutOfRange { value: 1.5 };
        assert_eq!(e.to_string(), "confidence 1.5 out of range [0.0, 1.0]");
    }

    #[test]
    fn error_display_unknown_chain() {
        let e = CommonError::UnknownChain("tezos".into());
        assert_eq!(e.to_string(), "unknown chain: tezos");
    }
}
