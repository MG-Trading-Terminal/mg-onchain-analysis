//! ABI dynamic-type decode tests.
//!
//! Covers: bytes, string, resolve_field_offsets for mixed tuples.
//!
//! reference: https://docs.soliditylang.org/en/latest/abi-spec.html — encoding
//!            examples from the spec are used as test fixtures below.

use mg_evm_types::abi::{decode_bytes_dynamic, decode_string, decode_uint, resolve_field_offsets};
use mg_evm_types::U256;

/// Build a big-endian padded 32-byte slot from a u64.
fn slot_u256(v: u64) -> Vec<u8> {
    let mut s = vec![0u8; 32];
    let be = v.to_be_bytes();
    s[24..32].copy_from_slice(&be);
    s
}

/// Encode `bytes` per ABI spec: head=offset, then [len][data][padding].
fn encode_bytes(offset: usize, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(offset as u64));
    buf.extend_from_slice(&slot_u256(data.len() as u64));
    buf.extend_from_slice(data);
    // Pad data to 32-byte boundary.
    let pad = (32 - (data.len() % 32)) % 32;
    buf.extend(std::iter::repeat_n(0u8, pad));
    buf
}

// ---------------------------------------------------------------------------
// bytes (dynamic)
// ---------------------------------------------------------------------------

#[test]
fn bytes_empty() {
    // bytes("") — head at 0 points to offset 32, then len=0.
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(32)); // head: ptr = 32
    buf.extend_from_slice(&slot_u256(0));  // tail: len = 0
    let result = decode_bytes_dynamic(&buf, 0, 0).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bytes_single_word() {
    // bytes(b"hello") — 5 bytes.
    let data = b"hello";
    let buf = encode_bytes(32, data);
    let result = decode_bytes_dynamic(&buf, 0, 0).unwrap();
    assert_eq!(result, data);
}

#[test]
fn bytes_exactly_32() {
    // bytes([0u8; 32]) — exactly one word.
    let data = vec![0xabu8; 32];
    let buf = encode_bytes(32, &data);
    let result = decode_bytes_dynamic(&buf, 0, 0).unwrap();
    assert_eq!(result, data);
}

#[test]
fn bytes_multi_word() {
    // bytes([0xffu8; 100]) — more than 3 words.
    let data = vec![0xffu8; 100];
    let buf = encode_bytes(32, &data);
    let result = decode_bytes_dynamic(&buf, 0, 0).unwrap();
    assert_eq!(result.len(), 100);
    assert!(result.iter().all(|&b| b == 0xff));
}

// ---------------------------------------------------------------------------
// string (dynamic)
// ---------------------------------------------------------------------------

#[test]
fn string_empty() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(32));
    buf.extend_from_slice(&slot_u256(0));
    let result = decode_string(&buf, 0, 0).unwrap();
    assert!(result.is_empty());
}

#[test]
fn string_short() {
    let data = b"MeatGrinder";
    let buf = encode_bytes(32, data);
    let result = decode_string(&buf, 0, 0).unwrap();
    assert_eq!(result, "MeatGrinder");
}

#[test]
fn string_exactly_32_chars() {
    let data = b"abcdefghijklmnopqrstuvwxyz123456"; // 32 bytes
    let buf = encode_bytes(32, data);
    let result = decode_string(&buf, 0, 0).unwrap();
    assert_eq!(result, "abcdefghijklmnopqrstuvwxyz123456");
}

// ---------------------------------------------------------------------------
// resolve_field_offsets — mixed static + dynamic tuple
// ---------------------------------------------------------------------------

#[test]
fn resolve_offsets_two_static() {
    // (uint256, uint256) — both static, head only.
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(100));
    buf.extend_from_slice(&slot_u256(200));
    let offsets = resolve_field_offsets(&buf, &[false, false]).unwrap();
    assert_eq!(offsets, vec![0, 32]);

    // Check we can decode from those offsets.
    let v0 = decode_uint(&buf, offsets[0], 256).unwrap();
    let v1 = decode_uint(&buf, offsets[1], 256).unwrap();
    assert_eq!(v0, U256::from(100u64));
    assert_eq!(v1, U256::from(200u64));
}

#[test]
fn resolve_offsets_static_then_dynamic() {
    // (uint256, bytes) encoded as:
    //   head[0] = 100  (static value)
    //   head[1] = 64   (ptr to tail)
    //   tail: len=3, b"abc" + 29 zeros
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(100));   // field 0 — static
    buf.extend_from_slice(&slot_u256(64));    // field 1 — dynamic ptr
    // offset 64: tail starts here
    buf.extend_from_slice(&slot_u256(3));     // len = 3
    buf.extend_from_slice(b"abc");
    buf.extend(std::iter::repeat_n(0u8, 29));

    let offsets = resolve_field_offsets(&buf, &[false, true]).unwrap();
    assert_eq!(offsets[0], 0);
    assert_eq!(offsets[1], 64);

    let v0 = decode_uint(&buf, offsets[0], 256).unwrap();
    assert_eq!(v0, U256::from(100u64));

    let bytes = decode_bytes_dynamic(&buf, 32, 0).unwrap(); // head slot 1 is at buf[32]
    assert_eq!(bytes, b"abc");
}

#[test]
fn resolve_offsets_dynamic_then_static() {
    // (bytes, uint256) — first field is dynamic.
    // head[0] = 64  (ptr to tail after the 2-slot head)
    // head[1] = 789 (static)
    // tail at 64: len=2, b"hi" + 30 zeros
    let mut buf = Vec::new();
    buf.extend_from_slice(&slot_u256(64));    // field 0 — dynamic ptr
    buf.extend_from_slice(&slot_u256(789));   // field 1 — static
    // padding: head ends at 64, tail starts at 64
    buf.extend_from_slice(&slot_u256(2));     // len=2
    buf.extend_from_slice(b"hi");
    buf.extend(std::iter::repeat_n(0u8, 30));

    let offsets = resolve_field_offsets(&buf, &[true, false]).unwrap();
    assert_eq!(offsets[0], 64);
    assert_eq!(offsets[1], 32);

    let bytes = decode_bytes_dynamic(&buf, 0, 0).unwrap(); // head slot 0 at buf[0]
    assert_eq!(bytes, b"hi");

    let v1 = decode_uint(&buf, offsets[1], 256).unwrap();
    assert_eq!(v1, U256::from(789u64));
}

#[test]
fn resolve_offsets_buf_too_short() {
    // Head needs 64 bytes for 2 fields; only 16 provided.
    let buf = vec![0u8; 16];
    assert!(resolve_field_offsets(&buf, &[false, false]).is_err());
}
