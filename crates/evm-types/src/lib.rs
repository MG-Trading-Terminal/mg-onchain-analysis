//! `mg-evm-types` — self-sovereign EVM primitive types and ABI decoder.
//!
//! Provides every type and function needed to parse and represent EVM-native
//! data without depending on any vendor SDK.  Written under ADR 0006
//! (code-level self-sovereignty).
//!
//! # Contents
//!
//! - [`Address`] — 20-byte EVM address with EIP-55 checksum
//! - [`B256`] — 32-byte hash / topic value
//! - [`U256`] — 256-bit unsigned integer (re-exported from `primitive_types`)
//! - [`I256`] — 256-bit signed integer (two's-complement, in-tree implementation)
//! - [`RawLog`] — raw on-chain log (`address + topics + data`)
//! - [`keccak`] — `keccak256(&[u8]) -> B256` and helpers
//! - [`abi`] — Ethereum ABI decoder (static + dynamic types)
//! - [`event`] — `DecodeLog` trait + event-selector hash helper
//!
//! # Design
//!
//! - Zero `alloy-*` or `reth-*` dependencies.
//! - `serde` serialisation for all public types (hex strings; EIP-55 for addresses).
//! - All thresholds and parsing are spec-driven; `REFERENCES.md` entries exist for
//!   EIP-55, the Ethereum ABI specification, and Keccak-256.

#![deny(missing_docs)]

pub mod address;
pub mod abi;
pub mod event;
pub mod hash;
pub mod keccak;
pub mod log;
pub mod uint;

pub use address::Address;
pub use hash::B256;
pub use log::RawLog;
pub use uint::{I256, U256};
pub use event::DecodeLog;
pub use abi::DecodeError;
