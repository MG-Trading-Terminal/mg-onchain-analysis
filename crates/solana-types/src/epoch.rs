//! Solana epoch number — a coarser unit of time than a slot.
//!
//! On Solana mainnet, one epoch is approximately 432,000 slots (~2–3 days).
//! Epochs are used for staking calculations, validator reward periods, and
//! certain validator-schedule queries.
//!
//! `Epoch` is a newtype over `u64`.  It mirrors [`crate::Slot`] structurally.
//!
//! # Serde representation
//!
//! Serialises as a JSON number (`u64`).
//!
//! # Reference
//!
//! reference: solana_sdk::clock::Epoch (Apache-2.0) — type alias semantics consulted.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Epoch
// ---------------------------------------------------------------------------

/// A Solana epoch number.
///
/// Epochs group slots into fixed windows used for validator reward periods and
/// stake-weighted leader schedule computation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

impl Epoch {
    /// Construct from a raw `u64`.
    #[inline]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// Return the inner `u64` value.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Epoch({})", self.0)
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Epoch {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Self)
    }
}

impl From<u64> for Epoch {
    #[inline]
    fn from(n: u64) -> Self {
        Self(n)
    }
}

impl From<Epoch> for u64 {
    #[inline]
    fn from(e: Epoch) -> Self {
        e.0
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_display() {
        assert_eq!(Epoch(700).to_string(), "700");
    }

    #[test]
    fn epoch_from_str() {
        let e: Epoch = "650".parse().unwrap();
        assert_eq!(e, Epoch(650));
    }

    #[test]
    fn epoch_from_str_invalid() {
        assert!("xyz".parse::<Epoch>().is_err());
    }

    #[test]
    fn epoch_serde_round_trip() {
        let e = Epoch(314_159_265);
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, "314159265");
        let back: Epoch = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn epoch_ordering() {
        assert!(Epoch(1) < Epoch(2));
        assert_eq!(Epoch(5), Epoch(5));
    }

    #[test]
    fn epoch_conversions() {
        let n: u64 = 99;
        let e: Epoch = n.into();
        let back: u64 = e.into();
        assert_eq!(back, n);
    }
}
