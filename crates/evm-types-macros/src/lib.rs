//! `mg-evm-types-macros` — proc-macro ergonomics for EVM event decoding.
//!
//! Provides the `event_signature!` macro which generates:
//! - A typed struct with one field per event parameter.
//! - A `SIGNATURE_HASH: mg_evm_types::B256` constant (keccak256 of the canonical
//!   event signature, computed at macro-expansion time — no runtime cost).
//! - An `impl mg_evm_types::DecodeLog` that decodes indexed and non-indexed
//!   parameters from a `RawLog`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use mg_evm_types_macros::event_signature;
//!
//! event_signature! {
//!     event Transfer(address indexed from, address indexed to, uint256 value);
//! }
//! // Expands to: struct Transfer { from: Address, to: Address, value: U256 }
//! // + impl DecodeLog for Transfer
//! // + Transfer::SIGNATURE_HASH (const B256)
//! ```
//!
//! # Zero vendor SDK dependency
//!
//! This crate depends only on `syn`, `quote`, `proc-macro2` (universal Rust
//! language tooling) and `tiny-keccak` (Keccak-256 spec implementation).
//! No `alloy-*`, `reth-*`, or any other vendor SDK.  See ADR 0006.

extern crate proc_macro;
use proc_macro::TokenStream;

mod generate;
mod parse;

/// Generate a typed event struct, `SIGNATURE_HASH` constant, and `DecodeLog`
/// implementation from a Solidity-syntax event declaration.
///
/// # Syntax
///
/// ```rust,ignore
/// event_signature! {
///     event EventName(type [indexed] name, ...);
/// }
/// ```
///
/// Supported types: `address`, `bool`, `uint<N>`, `int<N>`, `bytes<N>`,
/// `bytes`, `string`.
///
/// # Generated code
///
/// For `event Transfer(address indexed from, address indexed to, uint256 value)`:
///
/// ```rust,ignore
/// pub struct Transfer {
///     pub from: mg_evm_types::Address,
///     pub to:   mg_evm_types::Address,
///     pub value: mg_evm_types::U256,
/// }
///
/// impl Transfer {
///     pub const SIGNATURE_HASH: mg_evm_types::B256 = mg_evm_types::B256([/* 32 bytes */]);
/// }
///
/// impl mg_evm_types::DecodeLog for Transfer {
///     fn decode_log(log: &mg_evm_types::RawLog) -> Result<Self, mg_evm_types::DecodeError> {
///         // verifies topic0, decodes topics[1..] and data
///     }
/// }
/// ```
#[proc_macro]
pub fn event_signature(input: TokenStream) -> TokenStream {
    let decl = match syn::parse::<parse::EventDecl>(input) {
        Ok(d) => d,
        Err(e) => return e.to_compile_error().into(),
    };
    generate::generate(&decl).into()
}
