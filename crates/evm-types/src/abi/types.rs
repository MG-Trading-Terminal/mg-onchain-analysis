//! `Token` — a discriminated union of decoded ABI values.
//!
//! This is a small surface used primarily for tests and for callers that need
//! to inspect decoded data without generating a typed struct via
//! `event_signature!`.  The main decoder path uses the typed functions in
//! `decode.rs` directly.
//!
//! reference: ethabi::Token (MIT) — variant naming consulted.

use crate::{Address, U256, I256};

/// A decoded ABI value.
///
/// Only the variants required by our detectors are included.  New variants can
/// be added as needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// `address` — a 20-byte EVM address.
    Address(Address),
    /// `uint<N>` (N ≤ 256) — stored as `U256`.
    Uint(U256),
    /// `int<N>` (N ≤ 256) — stored as `I256`.
    Int(I256),
    /// `bool`.
    Bool(bool),
    /// `bytes<N>` (fixed-size, N ≤ 32).
    FixedBytes(Vec<u8>),
    /// `bytes` (dynamic).
    Bytes(Vec<u8>),
    /// `string` (dynamic, UTF-8).
    String(String),
    /// `T[]` or `T[N]` — array of tokens.
    Array(Vec<Token>),
    /// Tuple — anonymous struct `(T1, T2, ...)`.
    Tuple(Vec<Token>),
}
