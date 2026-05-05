//! `mg-solana-types` — self-sovereign Solana primitive types.
//!
//! Provides the minimum Solana type surface used by `onchain-service` crates
//! after the Sprint 25 `solana-sdk` divestment (ADR 0006).
//!
//! # Contents
//!
//! - [`Pubkey`]      — 32-byte address with base58 Display/FromStr/serde + PDA derivation
//! - [`Signature`]   — 64-byte transaction signature with base58 Display/FromStr/serde
//! - [`Hash`]        — 32-byte blockhash with base58 Display/FromStr/serde
//! - [`Slot`]        — `u64` newtype for slot numbers
//! - [`Epoch`]       — `u64` newtype for epoch numbers
//! - [`Keypair`]     — Ed25519 signing keypair (seed → pubkey → sign)
//! - [`Instruction`] — Solana instruction (program_id + accounts + data)
//! - [`AccountMeta`] — Account metadata for instructions
//! - [`Transaction`] — Full Solana transaction with wire-format serialisation
//!
//! # Design
//!
//! Zero `solana-sdk`, `agave-*`, or any other Solana vendor crate dependency.
//! Base58 encoding uses the `bs58` crate (admitted under ADR 0006 Rule A as an
//! implementation of the public Base58 algorithm spec).
//!
//! All types serialise to/from JSON as strings (base58 for byte arrays;
//! plain numbers for `Slot` / `Epoch`), matching Solana's JSON-RPC conventions.
//!
//! # References
//!
//! - ADR 0006 — code-level self-sovereignty
//! - Design 0026 §5 — `crates/solana-types/` specification
//! - `solana_sdk::pubkey::Pubkey` (Apache-2.0) — base58 encoding convention
//! - Base58 alphabet: <https://en.bitcoin.it/wiki/Base58Check_encoding>

#![deny(missing_docs)]

pub mod epoch;
pub mod hash;
pub mod instruction;
pub mod keypair;
pub mod pubkey;
pub mod signature;
pub mod slot;
pub mod transaction;
pub mod wire;

pub use epoch::Epoch;
pub use hash::{Hash, HashError};
pub use instruction::{AccountMeta, Instruction};
pub use keypair::{Keypair, KeypairError};
pub use pubkey::{Pubkey, PubkeyError};
pub use signature::{Signature, SignatureError};
pub use slot::Slot;
pub use transaction::{CompiledInstruction, Message, MessageHeader, Transaction};
