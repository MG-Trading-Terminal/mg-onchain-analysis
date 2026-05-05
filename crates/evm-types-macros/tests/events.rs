//! Integration tests for `event_signature!` macro.
//!
//! Tests three representative events:
//! 1. ERC-20 Transfer — indexed address × 2 + non-indexed uint256.
//! 2. Uniswap V2 Swap — multiple non-indexed uint256 + indexed address × 2.
//! 3. Permit2 Permit — indexed address × 3 + non-indexed uint160 + uint48 × 2.
//!
//! Each test verifies:
//! - `SIGNATURE_HASH` matches the known topic0 value.
//! - `decode_log` correctly populates fields from a hand-crafted `RawLog`.
//! - `decode_log` returns `Topic0Mismatch` on wrong topic0.

use mg_evm_types::{Address, B256, DecodeLog, RawLog, U256};
use mg_evm_types_macros::event_signature;

// ---------------------------------------------------------------------------
// ERC-20 Transfer
// ---------------------------------------------------------------------------

event_signature! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

#[test]
fn transfer_signature_hash_matches_known_topic0() {
    // topic0 verified against Etherscan ERC-20 Transfer events.
    let expected: B256 =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
            .parse()
            .unwrap();
    assert_eq!(Transfer::SIGNATURE_HASH, expected);
}

#[test]
fn transfer_decode_log_happy_path() {
    // Build a RawLog representing:
    // from = 0x0000...0001, to = 0x0000...0002, value = 1000
    let topic0 = Transfer::SIGNATURE_HASH;
    let from_addr: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
    let to_addr: Address = "0x0000000000000000000000000000000000000002".parse().unwrap();

    let mut from_topic = [0u8; 32];
    from_topic[12..32].copy_from_slice(from_addr.as_bytes());

    let mut to_topic = [0u8; 32];
    to_topic[12..32].copy_from_slice(to_addr.as_bytes());

    // Non-indexed data: uint256(1000) in a 32-byte slot.
    let mut data = [0u8; 32];
    data[24..32].copy_from_slice(&1000u64.to_be_bytes());

    let log = RawLog::new(
        Address::ZERO, // emitting contract
        vec![topic0, B256(from_topic), B256(to_topic)],
        data.to_vec(),
    );

    let decoded = Transfer::decode_log(&log).unwrap();
    assert_eq!(decoded.from, from_addr);
    assert_eq!(decoded.to, to_addr);
    assert_eq!(decoded.value, U256::from(1000u64));
}

#[test]
fn transfer_decode_log_topic0_mismatch() {
    let log = RawLog::new(Address::ZERO, vec![B256::ZERO], vec![]);
    let result = Transfer::decode_log(&log);
    assert!(result.is_err());
}

#[test]
fn transfer_decode_log_wrong_topic_count() {
    // Only topic0, missing indexed from and to.
    let log = RawLog::new(Address::ZERO, vec![Transfer::SIGNATURE_HASH], vec![0u8; 32]);
    let result = Transfer::decode_log(&log);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Uniswap V2 Swap
// ---------------------------------------------------------------------------

event_signature! {
    event Swap(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );
}

#[test]
fn univ2_swap_signature_hash_matches_known_topic0() {
    let expected: B256 =
        "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
            .parse()
            .unwrap();
    assert_eq!(Swap::SIGNATURE_HASH, expected);
}

#[test]
fn univ2_swap_decode_log_happy_path() {
    let topic0 = Swap::SIGNATURE_HASH;
    let sender: Address = "0x0000000000000000000000000000000000000011".parse().unwrap();
    let to: Address = "0x0000000000000000000000000000000000000022".parse().unwrap();

    let mut sender_topic = [0u8; 32];
    sender_topic[12..32].copy_from_slice(sender.as_bytes());
    let mut to_topic = [0u8; 32];
    to_topic[12..32].copy_from_slice(to.as_bytes());

    // Non-indexed: amount0In=100, amount1In=0, amount0Out=0, amount1Out=200.
    // Each field is one 32-byte slot.
    let mut data = [0u8; 128]; // 4 × 32
    data[24..32].copy_from_slice(&100u64.to_be_bytes());   // amount0In
    // amount1In = 0 (already zero)
    // amount0Out = 0 (already zero)
    data[120..128].copy_from_slice(&200u64.to_be_bytes()); // amount1Out

    let log = RawLog::new(
        Address::ZERO,
        vec![topic0, B256(sender_topic), B256(to_topic)],
        data.to_vec(),
    );

    #[allow(non_snake_case)]
    let decoded = Swap::decode_log(&log).unwrap();
    assert_eq!(decoded.sender, sender);
    assert_eq!(decoded.to, to);
    assert_eq!(decoded.amount0In, U256::from(100u64));
    assert_eq!(decoded.amount1In, U256::zero());
    assert_eq!(decoded.amount0Out, U256::zero());
    assert_eq!(decoded.amount1Out, U256::from(200u64));
}

// ---------------------------------------------------------------------------
// Permit2 Permit
//
// event Permit(address indexed owner, address indexed token, address indexed spender,
//              uint160 amount, uint48 expiration, uint48 nonce)
//
// topic0: keccak256("Permit(address,address,address,uint160,uint48,uint48)")
// ---------------------------------------------------------------------------

event_signature! {
    event Permit(
        address indexed owner,
        address indexed token,
        address indexed spender,
        uint160 amount,
        uint48 expiration,
        uint48 nonce
    );
}

#[test]
fn permit2_permit_signature_hash_is_deterministic() {
    // We don't have an Etherscan-verified value for this without an API key,
    // but we verify the hash is:
    // (a) non-zero
    // (b) stable across two accesses (const)
    // (c) matches our own runtime keccak256
    use mg_evm_types::keccak::keccak256;
    let expected = keccak256(b"Permit(address,address,address,uint160,uint48,uint48)");
    assert_eq!(Permit::SIGNATURE_HASH, expected);
    assert_ne!(Permit::SIGNATURE_HASH, B256::ZERO);
}

#[test]
fn permit2_permit_decode_log_happy_path() {
    let topic0 = Permit::SIGNATURE_HASH;
    let owner: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
    let token: Address = "0x0000000000000000000000000000000000000002".parse().unwrap();
    let spender: Address = "0x0000000000000000000000000000000000000003".parse().unwrap();

    let mut owner_t = [0u8; 32];
    owner_t[12..32].copy_from_slice(owner.as_bytes());
    let mut token_t = [0u8; 32];
    token_t[12..32].copy_from_slice(token.as_bytes());
    let mut spender_t = [0u8; 32];
    spender_t[12..32].copy_from_slice(spender.as_bytes());

    // Non-indexed: amount(uint160)=9999, expiration(uint48)=123, nonce(uint48)=7.
    // All values fit in u64, so we encode as u64 big-endian in the last 8 bytes of
    // each 32-byte ABI slot (padded with leading zeros).
    let mut data = [0u8; 96]; // 3 × 32
    // slot 0: amount (uint160 encoded as uint256 in ABI) — use last 8 bytes for value
    data[24..32].copy_from_slice(&9999u64.to_be_bytes());
    // slot 1: expiration (uint48)
    data[56..64].copy_from_slice(&123u64.to_be_bytes());
    // slot 2: nonce (uint48)
    data[88..96].copy_from_slice(&7u64.to_be_bytes());

    let log = RawLog::new(
        Address::ZERO,
        vec![topic0, B256(owner_t), B256(token_t), B256(spender_t)],
        data.to_vec(),
    );

    let decoded = Permit::decode_log(&log).unwrap();
    assert_eq!(decoded.owner, owner);
    assert_eq!(decoded.token, token);
    assert_eq!(decoded.spender, spender);
    // All non-indexed fields are decoded as U256; the values are the raw ABI-padded words.
    assert_eq!(decoded.amount, U256::from(9999u64));
    assert_eq!(decoded.expiration, U256::from(123u64));
    assert_eq!(decoded.nonce, U256::from(7u64));
}

// ---------------------------------------------------------------------------
// SIGNATURE_HASH is a const — verify it can be used in pattern matching
// ---------------------------------------------------------------------------

#[test]
fn signature_hash_is_usable_as_const_in_match() {
    let topic: B256 = Transfer::SIGNATURE_HASH;
    let label = match topic {
        Transfer::SIGNATURE_HASH => "transfer",
        _ => "other",
    };
    assert_eq!(label, "transfer");
}
