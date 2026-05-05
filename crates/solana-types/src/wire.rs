//! Solana wire format: compact-u16 (short_vec) encoding.
//!
//! Solana uses a custom variable-length integer encoding for `Vec` length
//! prefixes in its transaction serialisation format. Unlike standard LEB128,
//! the encoding is 1–3 bytes with specific bit layout.
//!
//! # Encoding spec
//!
//! - 1 byte if value < 0x80:   `[value]`
//! - 2 bytes if value < 0x4000: `[low7 | 0x80, high7]`
//! - 3 bytes if value ≤ 0xFFFF: `[bits0..6 | 0x80, bits7..13 | 0x80, bits14..15]`
//!
//! # Reference
//!
//! reference: solana-program/src/short_vec.rs (Apache-2.0)
//!            https://github.com/solana-labs/solana/blob/master/sdk/program/src/short_vec.rs

use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned by compact-u16 decoding.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    /// The input slice ended before the full value was decoded.
    #[error("compact-u16: unexpected end of input")]
    UnexpectedEof,
    /// The encoded value exceeds u16::MAX.
    #[error("compact-u16: overflow — value exceeds u16::MAX")]
    Overflow,
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encode a `u16` as Solana compact-u16 and append the bytes to `out`.
///
/// - 1 byte for values 0x0000..=0x007F
/// - 2 bytes for values 0x0080..=0x3FFF
/// - 3 bytes for values 0x4000..=0xFFFF
///
/// reference: solana-program/src/short_vec.rs (Apache-2.0)
pub fn encode_compact_u16(value: u16, out: &mut Vec<u8>) {
    let mut val = value;
    loop {
        let low7 = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            out.push(low7);
            break;
        } else {
            out.push(low7 | 0x80);
        }
    }
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Decode a compact-u16 from the front of `bytes`.
///
/// Returns `(value, bytes_consumed)` on success.
///
/// # Errors
///
/// - [`WireError::UnexpectedEof`] — slice is empty or terminated before value
///   completion.
/// - [`WireError::Overflow`] — the encoded value exceeds `u16::MAX`.
///
/// reference: solana-program/src/short_vec.rs (Apache-2.0)
pub fn decode_compact_u16(bytes: &[u8]) -> Result<(u16, usize), WireError> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    let mut i = 0usize;

    loop {
        if i >= bytes.len() {
            return Err(WireError::UnexpectedEof);
        }
        let byte = bytes[i];
        i += 1;

        // The maximum compact-u16 is 3 bytes wide (encoding 16 bits).
        // If we're reading more bytes than that, something is wrong.
        if shift >= 21 {
            return Err(WireError::Overflow);
        }

        result |= ((byte & 0x7F) as u32) << shift;
        shift += 7;

        if (byte & 0x80) == 0 {
            // Last byte of the sequence.
            break;
        }
    }

    if result > u16::MAX as u32 {
        return Err(WireError::Overflow);
    }

    Ok((result as u16, i))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(v: u16) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_compact_u16(v, &mut buf);
        buf
    }

    fn decode(bytes: &[u8]) -> (u16, usize) {
        decode_compact_u16(bytes).expect("decode should succeed")
    }

    #[test]
    fn encode_zero() {
        assert_eq!(encode(0), vec![0x00]);
    }

    #[test]
    fn encode_below_0x80() {
        assert_eq!(encode(0x7F), vec![0x7F]);
        assert_eq!(encode(1), vec![0x01]);
        assert_eq!(encode(127), vec![0x7F]);
    }

    #[test]
    fn encode_0x80_two_bytes() {
        // 0x80 = 128 → [0x80 | 0x00, 0x01] = [0x80, 0x01]
        let encoded = encode(0x80);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded, vec![0x80, 0x01]);
    }

    #[test]
    fn encode_0x3fff_two_bytes() {
        // 0x3FFF = 16383 → 7 low bits = 0x7F, 7 high bits = 0x7F
        // [0x7F | 0x80, 0x7F] = [0xFF, 0x7F]
        let encoded = encode(0x3FFF);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded, vec![0xFF, 0x7F]);
    }

    #[test]
    fn encode_0x4000_three_bytes() {
        // 0x4000 = 16384
        // bits 0..6 = 0 → 0x00 | 0x80 = 0x80
        // bits 7..13 = 0 → 0x00 | 0x80 = 0x80
        // bits 14..15 = 1 → 0x01
        let encoded = encode(0x4000);
        assert_eq!(encoded.len(), 3);
        assert_eq!(encoded, vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn encode_u16_max() {
        // u16::MAX = 0xFFFF = 65535
        let encoded = encode(u16::MAX);
        assert_eq!(encoded.len(), 3);
        // bits 0..6 = 0x7F, bits 7..13 = 0x7F, bits 14..15 = 0x03
        assert_eq!(encoded, vec![0xFF, 0xFF, 0x03]);
    }

    #[test]
    fn decode_zero() {
        let (val, consumed) = decode(&[0x00]);
        assert_eq!(val, 0);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn decode_below_0x80() {
        let (val, consumed) = decode(&[0x7F]);
        assert_eq!(val, 0x7F);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn decode_0x80() {
        let (val, consumed) = decode(&[0x80, 0x01]);
        assert_eq!(val, 0x80);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn decode_0x3fff() {
        let (val, consumed) = decode(&[0xFF, 0x7F]);
        assert_eq!(val, 0x3FFF);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn decode_0x4000() {
        let (val, consumed) = decode(&[0x80, 0x80, 0x01]);
        assert_eq!(val, 0x4000);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn decode_u16_max() {
        let (val, consumed) = decode(&[0xFF, 0xFF, 0x03]);
        assert_eq!(val, u16::MAX);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn decode_eof_errors() {
        let result = decode_compact_u16(&[]);
        assert_eq!(result, Err(WireError::UnexpectedEof));

        // Continuation byte with nothing following
        let result = decode_compact_u16(&[0x80]);
        assert_eq!(result, Err(WireError::UnexpectedEof));
    }

    #[test]
    fn round_trip_boundary_values() {
        for v in [0u16, 1, 0x7F, 0x80, 0x3FFF, 0x4000, u16::MAX] {
            let encoded = encode(v);
            let (decoded, consumed) = decode(&encoded);
            assert_eq!(decoded, v, "round-trip failed for value {v:#06x}");
            assert_eq!(consumed, encoded.len(), "consumed bytes mismatch for {v:#06x}");
        }
    }

    #[test]
    fn decode_ignores_trailing_bytes() {
        // Trailing bytes after the encoded value should not be consumed.
        let mut bytes = vec![0x01u8]; // encodes value 1, 1 byte
        bytes.extend_from_slice(&[0xFF, 0xFF]); // trailing garbage
        let (val, consumed) = decode(&bytes);
        assert_eq!(val, 1);
        assert_eq!(consumed, 1);
    }
}
