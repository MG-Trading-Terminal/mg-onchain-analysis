//! ABI decoding error types.

use thiserror::Error;

/// Error variants for Ethereum ABI decoding.
///
/// The Ethereum ABI specification defines encoding for both static and dynamic
/// types.  Decoding failures fall into these categories:
///
/// 1. Not enough data (truncated input).
/// 2. Offset in dynamic type points outside the buffer.
/// 3. Type-specific constraints violated (e.g., bool not 0 or 1).
/// 4. Wrong number of topics for an event.
///
/// reference: https://docs.soliditylang.org/en/latest/abi-spec.html
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The input buffer was shorter than expected for a fixed-size type.
    #[error("ABI decode: buffer too short (need {need} bytes, have {have})")]
    BufferTooShort {
        /// Number of bytes required.
        need: usize,
        /// Number of bytes available.
        have: usize,
    },

    /// A dynamic-type offset pointed beyond the available data.
    #[error("ABI decode: dynamic offset {offset} + {len} out of bounds (buf len {buf_len})")]
    OffsetOutOfBounds {
        /// The base offset.
        offset: usize,
        /// The length requested from that offset.
        len: usize,
        /// Total buffer length.
        buf_len: usize,
    },

    /// A `uint` or `int` type was requested with a bit-width outside 8..=256 or
    /// not a multiple of 8.
    #[error("ABI decode: invalid bit width {0} (must be 8..=256 and a multiple of 8)")]
    InvalidBitWidth(u16),

    /// A `bytesN` type was requested with N outside 1..=32.
    #[error("ABI decode: invalid bytesN size {0} (must be 1..=32)")]
    InvalidBytesNSize(u8),

    /// A `bool` slot contained a value other than 0 or 1.
    #[error("ABI decode: bool slot has non-boolean value 0x{0}")]
    InvalidBool(String),

    /// The event log had the wrong number of topics for the expected signature.
    #[error("ABI decode: expected {expected} topics, got {got}")]
    WrongTopicCount {
        /// How many topics were expected.
        expected: usize,
        /// How many topics were present.
        got: usize,
    },

    /// The event's `topic[0]` did not match the expected SIGNATURE_HASH.
    #[error("ABI decode: topic0 mismatch — expected {expected}, got {got}")]
    Topic0Mismatch {
        /// Expected topic0 (hex).
        expected: String,
        /// Actual topic0 seen in the log (hex).
        got: String,
    },
}
