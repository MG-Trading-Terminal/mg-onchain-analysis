//! Keccak-256 helpers.
//!
//! Wraps `tiny-keccak` (which implements the NIST Keccak specification — the
//! same algorithm used by Ethereum for event-signature hashing, address
//! checksum, and many other primitives).
//!
//! # Reference
//!
//! Ethereum Yellow Paper §B — Keccak-256 hash function.
//! `tiny-keccak` is a pure-Rust Keccak-256 implementation; ADR 0006 allows it
//! under Rule A as a generic implementation of a public algorithm spec.

use tiny_keccak::{Hasher as _, Keccak};

use crate::B256;

/// Compute `keccak256(data)` and return the 32-byte digest as `B256`.
///
/// This is used in two places:
/// 1. Runtime: event-signature hash verification in `DecodeLog` impls.
/// 2. Compile time (inside proc-macros): computing `SIGNATURE_HASH` literals
///    from canonical event-signature strings.
///
/// # Example
///
/// ```
/// use mg_evm_types::keccak::keccak256;
///
/// // Transfer(address,address,uint256)
/// let hash = keccak256(b"Transfer(address,address,uint256)");
/// assert_eq!(
///     hex::encode(hash.0),
///     "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
/// );
/// ```
#[must_use]
pub fn keccak256(data: &[u8]) -> B256 {
    let mut k = Keccak::v256();
    k.update(data);
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    B256(out)
}

/// Compute `keccak256` of the canonical event signature string, e.g.
/// `"Transfer(address,address,uint256)"`.
///
/// Convenience wrapper used by the `event_signature!` macro tests and by
/// any code that builds signatures at runtime.
#[must_use]
pub fn event_topic0(canonical_sig: &str) -> B256 {
    keccak256(canonical_sig.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vectors sourced from well-known ERC-20 / Uniswap events, verified
    // against Etherscan "Events" tab (topic0 column).

    #[test]
    fn keccak256_transfer_erc20() {
        // reference: ERC-20 Transfer event — universally known topic0
        let hash = keccak256(b"Transfer(address,address,uint256)");
        assert_eq!(
            hex::encode(hash.0),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn keccak256_approval_erc20() {
        let hash = keccak256(b"Approval(address,address,uint256)");
        assert_eq!(
            hex::encode(hash.0),
            "8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925"
        );
    }

    #[test]
    fn keccak256_univ2_swap() {
        // UniV2 Swap(address indexed sender, uint256 amount0In, uint256 amount1In,
        //            uint256 amount0Out, uint256 amount1Out, address indexed to)
        let hash = keccak256(
            b"Swap(address,uint256,uint256,uint256,uint256,address)",
        );
        assert_eq!(
            hex::encode(hash.0),
            "d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
        );
    }

    #[test]
    fn keccak256_univ3_swap() {
        let hash = keccak256(
            b"Swap(address,address,int256,int256,uint160,uint128,int24)",
        );
        assert_eq!(
            hex::encode(hash.0),
            "c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
    }

    #[test]
    fn keccak256_empty() {
        // keccak256("") = the Keccak-256 of an empty string — well-known value
        let hash = keccak256(b"");
        assert_eq!(
            hex::encode(hash.0),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn event_topic0_alias() {
        // event_topic0 and keccak256 must produce the same result
        let a = event_topic0("Transfer(address,address,uint256)");
        let b = keccak256(b"Transfer(address,address,uint256)");
        assert_eq!(a, b);
    }
}
