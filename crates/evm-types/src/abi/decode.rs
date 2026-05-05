//! Ethereum ABI decoder.
//!
//! Implements the subset of the Ethereum ABI specification required to decode
//! ERC-20, Uniswap V2/V3, and Permit2 event data.
//!
//! # ABI encoding rules (summary)
//!
//! Static types are encoded in-place as 32-byte slots:
//! - `address`, `bool`, `uint<N>`, `int<N>`, `bytes<N>` — one 32-byte slot each.
//!
//! Dynamic types (`bytes`, `string`, `T[]`) are encoded as a 32-byte offset in
//! the head section, followed by the actual data in the tail section.
//!
//! For a top-level `(T1, T2, …)` tuple:
//! - The head section contains one 32-byte slot per field.
//!   - For static types: the encoded value.
//!   - For dynamic types: a `uint256` offset pointing into the tail.
//! - The tail section follows immediately after the head.
//!
//! reference: https://docs.soliditylang.org/en/latest/abi-spec.html
//! reference: alloy_sol_types::abi::Decoder (MIT/Apache-2.0) — offset handling
//!            logic and dynamic-type decoding approach consulted.

use crate::abi::error::DecodeError;
use crate::{Address, I256, U256};
use crate::uint::u256_ext;

// ---------------------------------------------------------------------------
// Static-type decoders — each consumes exactly one 32-byte ABI slot
// ---------------------------------------------------------------------------

/// Assert that `buf` contains at least `offset + 32` bytes.
#[inline]
fn require_slot(buf: &[u8], offset: usize) -> Result<&[u8; 32], DecodeError> {
    if buf.len() < offset + 32 {
        return Err(DecodeError::BufferTooShort { need: offset + 32, have: buf.len() });
    }
    Ok(buf[offset..offset + 32].try_into().unwrap())
}

/// Decode an `address` from one 32-byte ABI slot at `offset`.
///
/// The address occupies the rightmost 20 bytes; the leading 12 bytes must be zero
/// (by spec, though we do not enforce the padding constraint — off-spec encoders exist).
///
/// reference: https://docs.soliditylang.org/en/latest/abi-spec.html#formal-specification-of-the-encoding
pub fn decode_address(buf: &[u8], offset: usize) -> Result<Address, DecodeError> {
    let slot = require_slot(buf, offset)?;
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&slot[12..32]);
    Ok(Address(bytes))
}

/// Decode a `uint<N>` (N ≤ 256, multiple of 8) from one 32-byte ABI slot at `offset`.
///
/// Returns the value as `U256`.  The caller is responsible for range-checking if a
/// narrower type is needed (e.g. `uint128` → `u128`).
pub fn decode_uint(buf: &[u8], offset: usize, bits: u16) -> Result<U256, DecodeError> {
    validate_int_bits(bits)?;
    let slot = require_slot(buf, offset)?;
    Ok(u256_ext::from_be_slice(slot))
}

/// Decode an `int<N>` (N ≤ 256, multiple of 8) from one 32-byte ABI slot at `offset`.
///
/// Returns the value as `I256` (two's-complement, bit width N).
///
/// reference: https://docs.soliditylang.org/en/latest/abi-spec.html —
///   "int<M>: enc(X) is the big-endian two's complement encoding of X,
///    padded on the higher-order side with 0xff for negative X and 0x00 for positive X."
pub fn decode_int(buf: &[u8], offset: usize, bits: u16) -> Result<I256, DecodeError> {
    validate_int_bits(bits)?;
    let slot = require_slot(buf, offset)?;
    // The 32-byte slot IS the full 256-bit two's-complement representation (sign-extended).
    // We parse it directly as I256.
    let raw = u256_ext::from_be_slice(slot);
    Ok(I256::from_raw(raw))
}

/// Decode a `bytes<N>` (N ∈ 1..=32) from one 32-byte ABI slot at `offset`.
///
/// The first N bytes are the value; the remaining 32-N bytes are zero padding.
pub fn decode_bytes_fixed(buf: &[u8], offset: usize, n: u8) -> Result<Vec<u8>, DecodeError> {
    if n == 0 || n > 32 {
        return Err(DecodeError::InvalidBytesNSize(n));
    }
    let slot = require_slot(buf, offset)?;
    Ok(slot[..n as usize].to_vec())
}

/// Decode a `bool` from one 32-byte ABI slot at `offset`.
///
/// The slot is `0x00…01` for `true` and `0x00…00` for `false`.
/// Any other value is a DecodeError.
pub fn decode_bool(buf: &[u8], offset: usize) -> Result<bool, DecodeError> {
    let slot = require_slot(buf, offset)?;
    // Leading 31 bytes should be zero; slot[31] is 0 or 1.
    match slot[31] {
        0 => {
            // Verify nothing else is set (strict interpretation).
            if slot[..31].iter().all(|&b| b == 0) {
                Ok(false)
            } else {
                Err(DecodeError::InvalidBool(hex::encode(slot)))
            }
        }
        1 => {
            if slot[..31].iter().all(|&b| b == 0) {
                Ok(true)
            } else {
                Err(DecodeError::InvalidBool(hex::encode(slot)))
            }
        }
        _ => Err(DecodeError::InvalidBool(hex::encode(slot))),
    }
}

// ---------------------------------------------------------------------------
// Dynamic-type decoders — read offset from head, then decode tail
// ---------------------------------------------------------------------------

/// Decode a `bytes` (dynamic) value.
///
/// The head slot at `offset` is a `uint256` byte-offset into `buf` pointing to:
/// - A `uint256` length prefix.
/// - Followed by `length` bytes of data (padded to a 32-byte boundary).
///
/// `base` is the absolute offset within `buf` that the head's pointer is
/// relative to.  For a top-level decode, `base = 0`.
///
/// reference: https://docs.soliditylang.org/en/latest/abi-spec.html
pub fn decode_bytes_dynamic(buf: &[u8], head_offset: usize, base: usize) -> Result<Vec<u8>, DecodeError> {
    let ptr_raw = decode_uint(buf, head_offset, 256)?;
    let ptr = usize_from_u256(ptr_raw, buf.len())?;
    let data_start = base + ptr;
    read_dynamic_bytes(buf, data_start)
}

/// Decode a `string` (dynamic) value.
///
/// Encoding is identical to `bytes` — a length-prefixed byte sequence.
/// We convert to `String` using `String::from_utf8_lossy` (invalid UTF-8 is
/// replaced with `U+FFFD`; solc produces valid UTF-8 in practice).
pub fn decode_string(buf: &[u8], head_offset: usize, base: usize) -> Result<String, DecodeError> {
    let bytes = decode_bytes_dynamic(buf, head_offset, base)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Internal: read a length-prefixed byte blob from `buf` at absolute position `start`.
///
/// Layout: `[uint256 length][data bytes (padded to 32-byte boundary)]`
fn read_dynamic_bytes(buf: &[u8], start: usize) -> Result<Vec<u8>, DecodeError> {
    // Read the length (uint256 — we only support lengths that fit in usize).
    let len_u256 = decode_uint(buf, start, 256)?;
    let len = usize_from_u256(len_u256, buf.len())?;
    let data_start = start + 32;
    let data_end = data_start + len;
    if data_end > buf.len() {
        return Err(DecodeError::OffsetOutOfBounds {
            offset: data_start,
            len,
            buf_len: buf.len(),
        });
    }
    Ok(buf[data_start..data_end].to_vec())
}

/// Convert a `U256` to `usize`, returning `OffsetOutOfBounds` if it exceeds
/// `buf_len` (which acts as an upper bound on valid pointers).
fn usize_from_u256(v: U256, buf_len: usize) -> Result<usize, DecodeError> {
    // If v > usize::MAX or v > buf_len, the offset is out of bounds.
    if v > U256::from(buf_len) {
        return Err(DecodeError::OffsetOutOfBounds {
            offset: buf_len + 1, // sentinel: larger than buf
            len: 0,
            buf_len,
        });
    }
    // Safe: v fits in usize because v <= buf_len <= usize::MAX.
    let low = v.0[0]; // little-endian word 0 contains the value since v <= buf_len
    Ok(low as usize)
}

fn validate_int_bits(bits: u16) -> Result<(), DecodeError> {
    if !(8..=256).contains(&bits) || !bits.is_multiple_of(8) {
        return Err(DecodeError::InvalidBitWidth(bits));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tuple decoder — decode a sequence of (possibly mixed static/dynamic) params
// ---------------------------------------------------------------------------

/// Decode a sequence of ABI types from a buffer, following the head/tail encoding.
///
/// `specs` describes each field: `true` = dynamic, `false` = static.
/// Returns the starting byte-offset for each field (in the tail for dynamic types,
/// in the head for static types).
///
/// This is used by the `event_signature!` generated `DecodeLog` implementations
/// to locate each field within the `data` buffer.
///
/// # Returns
///
/// A `Vec<usize>` of field offsets, one per spec entry.  For static fields the
/// offset points to the start of the 32-byte slot in the head.  For dynamic
/// fields the offset points to the length prefix in the tail.
///
/// Callers use the offset with the appropriate `decode_*` function, passing
/// `base = 0` (all offsets are already absolute within `buf`).
pub fn resolve_field_offsets(buf: &[u8], specs: &[bool]) -> Result<Vec<usize>, DecodeError> {
    let head_size = specs.len() * 32;
    if buf.len() < head_size {
        return Err(DecodeError::BufferTooShort { need: head_size, have: buf.len() });
    }

    let mut offsets = Vec::with_capacity(specs.len());
    for (i, &is_dynamic) in specs.iter().enumerate() {
        let head_pos = i * 32;
        if is_dynamic {
            // Head slot is a uint256 byte-offset relative to the start of buf.
            let ptr_raw = decode_uint(buf, head_pos, 256)?;
            let ptr = usize_from_u256(ptr_raw, buf.len())?;
            // ptr is absolute: it already counts from the beginning of buf.
            offsets.push(ptr);
        } else {
            offsets.push(head_pos);
        }
    }
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers to build 32-byte slots:
    fn slot_u256(v: u64) -> Vec<u8> {
        let mut s = vec![0u8; 32];
        let be = v.to_be_bytes();
        s[24..32].copy_from_slice(&be);
        s
    }

    fn slot_address(addr: &[u8; 20]) -> Vec<u8> {
        let mut s = vec![0u8; 32];
        s[12..32].copy_from_slice(addr);
        s
    }

    fn slot_bool(v: bool) -> Vec<u8> {
        let mut s = vec![0u8; 32];
        s[31] = v as u8;
        s
    }

    #[test]
    fn decode_address_happy() {
        let bytes = [0xabu8; 20];
        let buf = slot_address(&bytes);
        let addr = decode_address(&buf, 0).unwrap();
        assert_eq!(addr.0, bytes);
    }

    #[test]
    fn decode_uint_u256_max() {
        let buf = vec![0xffu8; 32];
        let v = decode_uint(&buf, 0, 256).unwrap();
        // All-ff = u256::MAX
        assert_eq!(u256_ext::to_be_bytes(&v), [0xffu8; 32]);
    }

    #[test]
    fn decode_uint_128() {
        let buf = slot_u256(12345);
        let v = decode_uint(&buf, 0, 128).unwrap();
        assert_eq!(v, U256::from(12345u64));
    }

    #[test]
    fn decode_int_positive() {
        let buf = slot_u256(99);
        let v = decode_int(&buf, 0, 256).unwrap();
        assert!(!v.is_negative());
        assert_eq!(v.0, U256::from(99u64));
    }

    #[test]
    fn decode_int_negative_minus_one() {
        // −1 in two's complement is 0xff…ff
        let buf = vec![0xffu8; 32];
        let v = decode_int(&buf, 0, 256).unwrap();
        assert!(v.is_negative());
        assert_eq!(v.abs_as_u256(), U256::one());
    }

    #[test]
    fn decode_bool_true() {
        let buf = slot_bool(true);
        assert!(decode_bool(&buf, 0).unwrap());
    }

    #[test]
    fn decode_bool_false() {
        let buf = slot_bool(false);
        assert!(!decode_bool(&buf, 0).unwrap());
    }

    #[test]
    fn decode_bool_invalid() {
        let mut buf = slot_bool(false);
        buf[31] = 2; // not 0 or 1
        assert!(decode_bool(&buf, 0).is_err());
    }

    #[test]
    fn decode_bytes_fixed_4() {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let v = decode_bytes_fixed(&buf, 0, 4).unwrap();
        assert_eq!(v, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn decode_bytes_fixed_invalid_n() {
        let buf = vec![0u8; 32];
        assert!(decode_bytes_fixed(&buf, 0, 0).is_err());
        assert!(decode_bytes_fixed(&buf, 0, 33).is_err());
    }

    #[test]
    fn decode_bytes_dynamic_basic() {
        // Encode bytes(4): head ptr = 32, then len=4, then 0xdeadbeef + 28 padding zeros.
        let mut buf = Vec::new();
        // head: offset = 32 (relative to buf start)
        buf.extend_from_slice(&slot_u256(32));
        // tail: length = 4
        buf.extend_from_slice(&slot_u256(4));
        // data: 4 bytes + 28 padding
        buf.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        buf.extend(std::iter::repeat_n(0u8, 28));

        let result = decode_bytes_dynamic(&buf, 0, 0).unwrap();
        assert_eq!(result, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn decode_string_basic() {
        // Encode string("hi"): offset=32, len=2, then b"hi" + 30 padding.
        let mut buf = Vec::new();
        buf.extend_from_slice(&slot_u256(32));
        buf.extend_from_slice(&slot_u256(2));
        buf.extend_from_slice(b"hi");
        buf.extend(std::iter::repeat_n(0u8, 30));

        let s = decode_string(&buf, 0, 0).unwrap();
        assert_eq!(s, "hi");
    }

    #[test]
    fn decode_uint_invalid_bits() {
        let buf = vec![0u8; 32];
        assert!(decode_uint(&buf, 0, 7).is_err()); // not multiple of 8
        assert!(decode_uint(&buf, 0, 264).is_err()); // > 256
        assert!(decode_uint(&buf, 0, 0).is_err()); // < 8
    }

    #[test]
    fn resolve_field_offsets_all_static() {
        // Two static uint256 fields.
        let mut buf = Vec::new();
        buf.extend_from_slice(&slot_u256(100));
        buf.extend_from_slice(&slot_u256(200));
        let offsets = resolve_field_offsets(&buf, &[false, false]).unwrap();
        assert_eq!(offsets, vec![0, 32]);
    }

    #[test]
    fn resolve_field_offsets_mixed() {
        // uint256 static, then bytes dynamic.
        // Head: [slot0 = 100][slot1 = offset=64]
        // Tail at 64: [len=3][b"abc"+29 zeros]
        let mut buf = Vec::new();
        buf.extend_from_slice(&slot_u256(100)); // field 0 static
        buf.extend_from_slice(&slot_u256(64));  // field 1 dynamic, ptr=64
        buf.extend_from_slice(&[0u8; 32]);      // padding to reach offset 64
        buf.extend_from_slice(&slot_u256(3));   // len=3 at offset 64
        buf.extend_from_slice(b"abc");
        buf.extend(std::iter::repeat_n(0u8, 29));

        let offsets = resolve_field_offsets(&buf, &[false, true]).unwrap();
        assert_eq!(offsets[0], 0);  // static: head position
        assert_eq!(offsets[1], 64); // dynamic: points to length prefix in tail
    }
}
