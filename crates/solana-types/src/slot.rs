//! Solana slot number — a monotonically increasing counter indicating how many
//! slots have elapsed since genesis.
//!
//! `Slot` is a newtype over `u64`.  It supports all the standard derives and
//! arithmetic needed for checkpoint tracking in `crates/chain-adapter`.
//!
//! # Serde representation
//!
//! Serialises as a JSON number (`u64`).
//!
//! # Reference
//!
//! reference: solana_sdk::clock::Slot (Apache-2.0) — type alias semantics consulted.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Slot
// ---------------------------------------------------------------------------

/// A Solana slot number.
///
/// Slots are the fundamental unit of time on Solana — roughly 400 ms each.
/// This type wraps `u64` to provide type-safety at call sites that deal with
/// both slot numbers and other `u64` quantities (block heights, amounts, etc.).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Slot(pub u64);

impl Slot {
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

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slot({})", self.0)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Slot {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Self)
    }
}

impl From<u64> for Slot {
    #[inline]
    fn from(n: u64) -> Self {
        Self(n)
    }
}

impl From<Slot> for u64 {
    #[inline]
    fn from(s: Slot) -> Self {
        s.0
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_display() {
        assert_eq!(Slot(12345).to_string(), "12345");
    }

    #[test]
    fn slot_from_str() {
        let s: Slot = "99999".parse().unwrap();
        assert_eq!(s, Slot(99999));
    }

    #[test]
    fn slot_from_str_invalid() {
        let result = "not_a_number".parse::<Slot>();
        assert!(result.is_err());
    }

    #[test]
    fn slot_serde_round_trip() {
        let s = Slot(271_828_182);
        let json = serde_json::to_string(&s).unwrap();
        // Serialised as a plain number, not a string.
        assert_eq!(json, "271828182");
        let back: Slot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn slot_ordering() {
        assert!(Slot(1) < Slot(2));
        assert!(Slot(100) > Slot(50));
        assert_eq!(Slot(7), Slot(7));
    }

    #[test]
    fn slot_conversions() {
        let n: u64 = 42;
        let s: Slot = n.into();
        let back: u64 = s.into();
        assert_eq!(back, n);
    }
}
