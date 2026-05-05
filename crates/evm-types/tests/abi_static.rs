//! ABI static-type decode tests.
//!
//! Covers: uint256, address, bool, bytes32.

use mg_evm_types::abi::{decode_address, decode_bool, decode_bytes_fixed, decode_uint};
use mg_evm_types::{Address, U256};

fn slot_u256(v: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    let be = v.to_be_bytes();
    s[24..32].copy_from_slice(&be);
    s
}

fn slot_address_bytes(addr_bytes: &[u8; 20]) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[12..32].copy_from_slice(addr_bytes);
    s
}

#[test]
fn uint256_zero() {
    let buf = slot_u256(0);
    let v = decode_uint(&buf, 0, 256).unwrap();
    assert_eq!(v, U256::zero());
}

#[test]
fn uint256_one() {
    let buf = slot_u256(1);
    let v = decode_uint(&buf, 0, 256).unwrap();
    assert_eq!(v, U256::one());
}

#[test]
fn uint256_large() {
    let buf = slot_u256(u64::MAX);
    let v = decode_uint(&buf, 0, 256).unwrap();
    assert_eq!(v, U256::from(u64::MAX));
}

#[test]
fn uint256_multi_slot_second_slot() {
    // Two back-to-back slots; decode the second one.
    let mut buf = [0u8; 64];
    buf[32..64].copy_from_slice(&slot_u256(42));
    let v = decode_uint(&buf, 32, 256).unwrap();
    assert_eq!(v, U256::from(42u64));
}

#[test]
fn address_well_known() {
    // USDT mainnet: 0xdAC17F958D2ee523a2206206994597C13D831ec7
    let expected: Address = "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();
    let buf = slot_address_bytes(expected.as_bytes());
    let decoded = decode_address(&buf, 0).unwrap();
    assert_eq!(decoded, expected);
}

#[test]
fn address_zero() {
    let buf = slot_address_bytes(&[0u8; 20]);
    let decoded = decode_address(&buf, 0).unwrap();
    assert_eq!(decoded, Address::ZERO);
}

#[test]
fn bool_true() {
    let mut buf = [0u8; 32];
    buf[31] = 1;
    assert!(decode_bool(&buf, 0).unwrap());
}

#[test]
fn bool_false() {
    let buf = [0u8; 32];
    assert!(!decode_bool(&buf, 0).unwrap());
}

#[test]
fn bool_nonzero_invalid() {
    let mut buf = [0u8; 32];
    buf[31] = 255;
    assert!(decode_bool(&buf, 0).is_err());
}

#[test]
fn bytes32_selector() {
    // 4-byte selector in a bytes32 slot (first 4 bytes, right-padded with zeros).
    let mut buf = [0u8; 32];
    buf[0..4].copy_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // ERC-20 transfer() selector
    let v = decode_bytes_fixed(&buf, 0, 4).unwrap();
    assert_eq!(v, vec![0xa9, 0x05, 0x9c, 0xbb]);
}

#[test]
fn bytes32_full() {
    let mut buf = [0u8; 32];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8;
    }
    let v = decode_bytes_fixed(&buf, 0, 32).unwrap();
    assert_eq!(v.len(), 32);
    assert_eq!(&v[..], &buf[..]);
}

#[test]
fn buffer_too_short_static() {
    let buf = [0u8; 16]; // only 16 bytes — not enough for a 32-byte slot
    assert!(decode_uint(&buf, 0, 256).is_err());
    assert!(decode_address(&buf, 0).is_err());
    assert!(decode_bool(&buf, 0).is_err());
}
