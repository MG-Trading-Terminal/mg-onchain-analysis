//! Event signature hash helpers.
//!
//! An EVM event's `topic0` is `keccak256` of the canonical ABI signature string,
//! e.g. `"Transfer(address,address,uint256)"`.
//!
//! These helpers are used at runtime for constructing log filters and for
//! cross-checking topic0 in `DecodeLog` implementations.

use crate::keccak::keccak256;
use crate::B256;

/// Compute the topic0 (selector) for an event by hashing its canonical signature.
///
/// # Example
///
/// ```
/// use mg_evm_types::event::signature::event_selector;
///
/// let topic0 = event_selector("Transfer(address,address,uint256)");
/// assert_eq!(
///     hex::encode(topic0.0),
///     "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
/// );
/// ```
#[must_use]
pub fn event_selector(canonical_sig: &str) -> B256 {
    keccak256(canonical_sig.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-good topic0 values verified against Etherscan event tabs.

    #[test]
    fn transfer_erc20() {
        let t = event_selector("Transfer(address,address,uint256)");
        assert_eq!(
            hex::encode(t.0),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn approval_erc20() {
        let t = event_selector("Approval(address,address,uint256)");
        assert_eq!(
            hex::encode(t.0),
            "8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925"
        );
    }

    #[test]
    fn univ2_swap() {
        let t = event_selector("Swap(address,uint256,uint256,uint256,uint256,address)");
        assert_eq!(
            hex::encode(t.0),
            "d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
        );
    }

    #[test]
    fn univ3_swap() {
        let t = event_selector("Swap(address,address,int256,int256,uint160,uint128,int24)");
        assert_eq!(
            hex::encode(t.0),
            "c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
    }

    #[test]
    fn univ2_mint() {
        let t = event_selector("Mint(address,uint256,uint256)");
        assert_eq!(
            hex::encode(t.0),
            "4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f"
        );
    }

    #[test]
    fn univ2_burn() {
        let t = event_selector("Burn(address,uint256,uint256,address)");
        assert_eq!(
            hex::encode(t.0),
            "dccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496"
        );
    }

    #[test]
    fn univ3_mint() {
        let t = event_selector(
            "Mint(address,address,int24,int24,uint128,uint256,uint256)",
        );
        assert_eq!(
            hex::encode(t.0),
            "7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
        );
    }

    #[test]
    fn univ3_burn() {
        let t = event_selector("Burn(address,int24,int24,uint128,uint256,uint256)");
        assert_eq!(
            hex::encode(t.0),
            "0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
        );
    }
}
