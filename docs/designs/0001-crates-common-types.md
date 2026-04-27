# Design: `crates/common` — Domain Type Hierarchy

**Date:** 2026-04-21
**Status:** Draft
**Implements:** ADR 0001 §D4 (detector output shape), §D6 (RugCheck superset schema), §D8 (three delivery modes)
**Author:** architect agent

---

## Context

`crates/common` is the dependency floor of the entire workspace. Every crate above it — `chain-adapter`, `indexer`, `detectors`, `scoring`, `gateway`, `client-sdk`, `storage` — imports from it. It must compile without knowledge of any specific chain, DEX, database, or transport. Getting the types wrong here is expensive: a change to `AnomalyEvent` ripples through all six consumer crates and the wire API simultaneously.

This design fixes two concrete gaps:

1. **Missing canonical schema.** Before this design, no single document describes what a `Transfer`, `Swap`, `TokenMeta`, or `AnomalyEvent` looks like in Rust. The developer has no contract to implement against.
2. **Implicit numeric discipline.** CLAUDE.md forbids `f64` for prices, amounts, and supplies, but leaves the boundary between `rust_decimal::Decimal` (human-scaled) and `u128` (raw on-chain units) unspecified. This design makes the boundary explicit with conversion helpers.

ADR sections implemented:
- **D4:** `AnomalyEvent { confidence, severity, evidence }` — no booleans, consumer-side thresholds.
- **D6:** `TokenMeta` is a superset of the RugCheck v1 API live-response field list (verified 2026-04-21). Phase 4 EVM fields are present but marked reserved.
- **D8:** Types must serialize cleanly for all three delivery modes (in-process crate, REST JSON, WebSocket streaming). Serde conventions are specified in this document.

---

## Module layout

```
crates/common/src/
  lib.rs          # Re-exports every public type; module-level doc comment explains amount encoding
  chain.rs        # Chain enum, Address, TxHash, BlockRef — chain identity primitives
  amount.rs       # Amount encoding rules, raw_to_decimal / decimal_to_raw helpers
  event.rs        # Transfer, Swap, PoolEvent — the core streaming event types
  token.rs        # TokenMeta, HolderSnapshot, TopHolder, LockerInfo, MarketInfo — token state
  anomaly.rs      # AnomalyEvent, Severity, Confidence, Evidence — detector output contract
  error.rs        # CommonError (thiserror, non_exhaustive)
```

One rule: nothing in `common/` imports from any sibling crate (`chain-adapter`, `detectors`, etc.). Dependency direction is strictly upward.

---

## Rust sketch

### `error.rs`

```rust
//! Parse and validation errors for `crates/common` types.
//!
//! Use `thiserror` throughout. One top-level enum with non-exhaustive
//! to allow adding variants in minor releases without SemVer breaks.

use thiserror::Error;

/// All parse/validation errors produced by `crates/common`.
///
/// Marked `#[non_exhaustive]` so consumers must handle a wildcard arm;
/// this lets us add variants in minor releases.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommonError {
    /// An address string could not be parsed for the given chain.
    #[error("invalid {chain} address: {reason}")]
    InvalidAddress { chain: String, reason: String },

    /// A raw bytes slice had the wrong length for a chain's address type.
    #[error("wrong address byte length: expected {expected}, got {actual}")]
    AddressByteLength { expected: usize, actual: usize },

    /// A confidence value was outside [0.0, 1.0].
    #[error("confidence {value} out of range [0.0, 1.0]")]
    ConfidenceOutOfRange { value: f64 },

    /// A decimal amount string could not be parsed.
    #[error("invalid decimal amount string: {0}")]
    InvalidAmount(String),

    /// A transaction hash string had the wrong format for the given chain.
    #[error("invalid tx hash for {chain}: {reason}")]
    InvalidTxHash { chain: String, reason: String },

    /// A chain string could not be matched to a known variant.
    #[error("unknown chain: {0}")]
    UnknownChain(String),
}
```

---

### `chain.rs`

```rust
//! Chain identity primitives: Chain enum, Address, BlockRef, TxHash.
//!
//! **Address encoding on the wire:** always a string in chain-canonical form.
//! - Solana: Base58-encoded 32-byte public key (44 characters)
//! - EVM: EIP-55 checksum hex, 0x-prefixed, 42 characters
//! - Tron: Base58Check, starts with 'T', 34 characters
//!
//! Normalization MUST happen at the ingestion boundary (chain-adapter crate).
//! Any `Address` that reaches `crates/common` is assumed already canonical.
//!
//! **Block height / slot:** both Solana and EVM use u64. The semantics differ
//! (Solana slots, EVM block numbers) but the type is the same. `BlockRef`
//! carries the `Chain` tag to avoid mixing them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::CommonError;

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

/// Supported blockchain networks.
///
/// Serializes/deserializes as lowercase strings on the wire (matching
/// `mg-custody`'s convention and RugCheck's `chain` field):
/// `"solana"`, `"ethereum"`, `"bsc"`, `"base"`, `"arbitrum"`, `"polygon"`, `"tron"`.
///
/// `#[non_exhaustive]` prevents match exhaustiveness errors when new chains
/// are added in Phase 4 without a SemVer major bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Chain {
    Solana,
    Ethereum,
    Bsc,
    Base,
    Arbitrum,
    Polygon,
    /// Phase 4 — Tron USDT flow analysis for mg-custody compliance.
    Tron,
}

impl Chain {
    /// Returns the canonical string name used in API paths and storage keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Chain::Solana    => "solana",
            Chain::Ethereum  => "ethereum",
            Chain::Bsc       => "bsc",
            Chain::Base      => "base",
            Chain::Arbitrum  => "arbitrum",
            Chain::Polygon   => "polygon",
            Chain::Tron      => "tron",
        }
    }

    /// True if this is an EVM-compatible chain.
    pub fn is_evm(&self) -> bool {
        matches!(self, Chain::Ethereum | Chain::Bsc | Chain::Base | Chain::Arbitrum | Chain::Polygon)
    }

    /// True for Solana (account model, SPL tokens, slots).
    pub fn is_solana(&self) -> bool {
        matches!(self, Chain::Solana)
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Chain {
    type Err = CommonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "solana" | "sol"      => Ok(Chain::Solana),
            "ethereum" | "eth"    => Ok(Chain::Ethereum),
            "bsc" | "bnb"         => Ok(Chain::Bsc),
            "base"                => Ok(Chain::Base),
            "arbitrum" | "arb"    => Ok(Chain::Arbitrum),
            "polygon" | "matic"   => Ok(Chain::Polygon),
            "tron" | "trx"        => Ok(Chain::Tron),
            other => Err(CommonError::UnknownChain(other.to_owned())),
        }
    }
}

// ---------------------------------------------------------------------------
// Address
// ---------------------------------------------------------------------------

/// A chain-canonical address.
///
/// Internally stores the raw bytes and the chain tag.
/// `Display` produces the canonical string form (Base58 for Solana, checksum
/// hex for EVM) without re-allocating on each call because the canonical
/// string is cached at construction.
///
/// ## Serialization
/// On the wire: a plain string in chain-canonical form. The `Chain` context
/// is always available from the surrounding struct (e.g. `Transfer::chain`).
///
/// ## Construction
/// Use `Address::from_str_on_chain(s, chain)` at the adapter boundary.
/// Do not construct `Address` from raw bytes without validating length.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Address {
    /// Chain tag — needed to reconstruct canonical string form.
    pub chain: Chain,
    /// Raw bytes. Solana = 32 bytes. EVM = 20 bytes. Tron = 21 bytes (prefixed).
    pub bytes: Vec<u8>,
    /// Cached canonical string representation. Stored to avoid re-encoding on every Display call.
    canonical: String,
}

impl Address {
    /// Parse a chain-canonical address string for a specific chain.
    ///
    /// - Solana: expects Base58 string decoding to exactly 32 bytes.
    /// - EVM: expects `0x`-prefixed hex of exactly 20 bytes; applies EIP-55 checksum.
    /// - Tron: expects Base58Check starting with 'T', decoding to 21 bytes.
    ///
    /// Returns `CommonError::InvalidAddress` on any parse failure.
    pub fn from_str_on_chain(s: &str, chain: Chain) -> Result<Self, CommonError> {
        // TODO(developer): implement chain-specific decoding + checksum normalization.
        // For EVM: use `alloy_primitives::Address::from_str` then re-encode as checksum.
        // For Solana: use `bs58::decode` then assert len == 32.
        // Store the normalized string form in `canonical`.
        todo!()
    }

    /// Return the canonical string form. Same as `Display`.
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Raw bytes view.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

// Serialize as canonical string
impl Serialize for Address {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.canonical)
    }
}

// Deserialize requires chain context — use `Address::from_str_on_chain` explicitly.
// Serde alone cannot deserialize `Address` because it does not know the chain.
// Structs that embed `Address` fields should use a custom visitor or carry the
// chain tag alongside the address string and reconstruct in a `try_from` impl.
//
// TODO(developer): Evaluate whether a wrapper `AddressWithChain { chain, address: String }`
// is more ergonomic for REST deserialization. Flag as open question.

// ---------------------------------------------------------------------------
// TxHash
// ---------------------------------------------------------------------------

/// A chain-appropriate transaction hash.
///
/// Solana transaction signatures are 64 bytes (Ed25519 signature).
/// EVM transaction hashes are 32 bytes (Keccak-256 of the RLP-encoded tx).
///
/// Using an enum rather than an opaque `[u8; N]` gives compile-time type safety:
/// you cannot accidentally pass a Solana signature to an EVM receipt query.
///
/// **Wire format:** encoded as a hex string for EVM, Base58 string for Solana.
/// The `Chain` tag is always available from the context struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TxHash {
    /// Ed25519 signature — 64 raw bytes, displayed as Base58.
    Solana([u8; 64]),
    /// Keccak-256 hash — 32 raw bytes, displayed as 0x-prefixed hex.
    Evm([u8; 32]),
}

impl TxHash {
    /// Parse a Solana signature from Base58.
    pub fn solana_from_base58(s: &str) -> Result<Self, CommonError> {
        // TODO(developer): bs58::decode(s).into_vec() + assert len == 64
        todo!()
    }

    /// Parse an EVM tx hash from 0x-prefixed hex.
    pub fn evm_from_hex(s: &str) -> Result<Self, CommonError> {
        // TODO(developer): strip 0x, hex::decode, assert len == 32
        todo!()
    }
}

impl fmt::Display for TxHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxHash::Solana(bytes) => write!(f, "{}", bs58_encode_64(bytes)),
            TxHash::Evm(bytes)   => write!(f, "0x{}", hex_encode_32(bytes)),
        }
    }
}

// Serialize as string; deserialization requires chain context (same issue as Address).
impl Serialize for TxHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

// Placeholder for helper functions — implementations live in address.rs helpers.
fn bs58_encode_64(_bytes: &[u8; 64]) -> String { todo!() }
fn hex_encode_32(_bytes: &[u8; 32]) -> String { todo!() }

// ---------------------------------------------------------------------------
// BlockRef
// ---------------------------------------------------------------------------

/// A block height or slot number with chain context.
///
/// Both Solana slots and EVM block numbers are u64. Carrying the chain tag
/// prevents mixing them across chain-specific code paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockRef {
    pub chain: Chain,
    /// Solana: slot number. EVM: block number.
    pub height: u64,
}

impl BlockRef {
    pub fn new(chain: Chain, height: u64) -> Self {
        Self { chain, height }
    }
}

impl fmt::Display for BlockRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chain, self.height)
    }
}
```

---

### `amount.rs`

```rust
//! Amount encoding and conversion utilities.
//!
//! ## Invariant
//!
//! This module enforces the CLAUDE.md §Code Style rule:
//! **NEVER `f64` for prices, amounts, supplies, or liquidity.**
//!
//! Two tiers:
//!
//! - **`u128` — raw on-chain units.** Every on-chain ledger stores amounts as
//!   unsigned integers with a fixed decimal exponent (`decimals: u8`). Use `u128`
//!   as the universal raw type. Solana amounts fit in `u64` for standard SPL tokens,
//!   but Token-2022 extensions (e.g. confidential transfers, interest-bearing) can
//!   produce values that overflow `u64` when accumulated — `u128` is the safe default.
//!   For values that must interoperate with alloy EVM types, convert via `U256` at
//!   the chain-adapter boundary; do not carry `U256` into `crates/common` to avoid
//!   a dependency on `alloy-primitives` here.
//!
//! - **`rust_decimal::Decimal` — human-scaled quantities.** Use for USD values,
//!   percentage fields (e.g., `lp_burned_pct`, holder concentration ratios, tax rates),
//!   and any value presented to a human or stored in Postgres as NUMERIC.
//!
//! ## JSON serialization
//!
//! All amount fields serialize as **strings**, never JSON numbers.
//!
//! JSON numbers are IEEE-754 double-precision. A `u128` can represent values up to
//! 2^128 - 1, which has 39 decimal digits — far exceeding the 15–17 significant
//! digits of f64. A token with 18 decimals (EVM standard) and 10^9 total supply
//! has a raw unit count of 10^27, which loses precision when encoded as a JSON number.
//!
//! The `rust_decimal` crate provides `serde-with-str` feature which enables this
//! automatically for `Decimal` fields. Raw `u128` fields must use the custom
//! serializer in this module.
//!
//! ## Example
//!
//! ```rust
//! use mg_onchain_common::amount::{raw_to_decimal, decimal_to_raw};
//! use rust_decimal::Decimal;
//!
//! let raw: u128 = 1_000_000_000; // 1.0 SOL (9 decimals)
//! let human: Decimal = raw_to_decimal(raw, 9);
//! assert_eq!(human.to_string(), "1");
//!
//! let back: u128 = decimal_to_raw(human, 9).unwrap();
//! assert_eq!(back, raw);
//! ```

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::CommonError;

/// Convert a raw on-chain integer amount to a human-scaled `Decimal`.
///
/// `decimals` is the token's decimal exponent (e.g., 9 for SOL, 6 for USDC on Solana,
/// 18 for most EVM tokens). The result is `raw / 10^decimals`.
///
/// Precision: `Decimal` supports up to 28 significant digits. For tokens with
/// 18 decimals and `u128::MAX` supply this will overflow — callers must check
/// that `raw < 10^28` if they need exact results, or accept rounding for display.
pub fn raw_to_decimal(raw: u128, decimals: u8) -> Decimal {
    // TODO(developer): Decimal::from(raw) / Decimal::from(10u64.pow(decimals as u32))
    // Handle the edge case where decimals == 0.
    todo!()
}

/// Convert a human-scaled `Decimal` back to raw on-chain units.
///
/// Returns `None` if the result does not fit in `u128` (e.g., negative value
/// or overflow). Returns `CommonError::InvalidAmount` if the decimal has more
/// fractional digits than `decimals` allows.
pub fn decimal_to_raw(amount: Decimal, decimals: u8) -> Result<u128, CommonError> {
    // TODO(developer): amount * Decimal::from(10u64.pow(decimals as u32)), then
    // to_u128(), error on None or negative.
    todo!()
}

/// Serde serializer for `u128` amounts as strings.
///
/// Use with `#[serde(serialize_with = "serialize_u128_as_str")]` on struct fields.
pub fn serialize_u128_as_str<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// Serde deserializer for `u128` amounts from strings.
///
/// Use with `#[serde(deserialize_with = "deserialize_u128_from_str")]` on struct fields.
pub fn deserialize_u128_from_str<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<u128>().map_err(serde::de::Error::custom)
}
```

---

### `event.rs`

```rust
//! Core streaming event types: Transfer, Swap, PoolEvent.
//!
//! These are the raw primitives emitted by `crates/chain-adapter` and consumed
//! by `crates/detectors` and `crates/storage`. They represent a single on-chain
//! event extracted from a block/slot.
//!
//! **Determinism note:** Fields use `BTreeMap` (not `HashMap`) wherever a
//! key-value bag is needed. `BTreeMap` iterates in sorted key order, ensuring
//! that serialization of the same event always produces the same bytes —
//! a prerequisite for CLAUDE.md Detector Rule #5 (reproducibility).
//!
//! **Amount encoding:** raw on-chain units are `u128` with `serde(with = ...)`.
//! Human-scaled USD fields are `Decimal`. Never `f64`. See `amount.rs`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::amount::{deserialize_u128_from_str, serialize_u128_as_str};
use crate::chain::{Address, BlockRef, Chain, TxHash};

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

/// A token transfer — ERC-20 `Transfer(from, to, value)` or SPL token transfer.
///
/// Cross-chain: Solana SPL and EVM ERC-20 both map to this type. The adapter
/// normalizes chain-specific representation at the boundary.
///
/// For EVM ERC-20 transfers via ERC-4337 meta-transactions or proxy contracts,
/// `from` MUST be the economic sender (the wallet paying), not the relayer.
/// The adapter is responsible for tracing through proxy hops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    /// Which chain emitted this event.
    pub chain: Chain,

    /// Transaction that contained this transfer.
    pub tx_hash: TxHash,

    /// Block / slot this transfer was confirmed in.
    pub block: BlockRef,

    /// Wall-clock time of the block. Source of truth for ClickHouse partitioning.
    /// Use `chrono::DateTime<Utc>` — consistent with `mg-custody` convention.
    pub block_time: DateTime<Utc>,

    /// Token mint address (Solana) or contract address (EVM).
    pub token: Address,

    /// Economic sender — zero address indicates a mint event.
    pub from: Address,

    /// Recipient — zero address indicates a burn event.
    pub to: Address,

    /// Raw on-chain transfer amount in the token's native units.
    ///
    /// Serialized as a decimal string to avoid JSON number precision loss.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_raw: u128,

    /// Token decimal exponent at the time of transfer. Copied from `TokenMeta`
    /// so the event is self-contained for ClickHouse queries.
    pub decimals: u8,

    /// Index within the transaction (EVM log index; Solana instruction index).
    /// Required to uniquely identify a transfer when one tx has multiple transfers.
    pub log_index: u32,
}

impl Transfer {
    /// True if this is a mint event (from zero address).
    pub fn is_mint(&self) -> bool {
        // TODO(developer): check if from == zero address for this chain
        todo!()
    }

    /// True if this is a burn event (to zero address).
    pub fn is_burn(&self) -> bool {
        // TODO(developer): check if to == zero address for this chain
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Swap
// ---------------------------------------------------------------------------

/// A DEX swap event.
///
/// Raydium, Orca, Uniswap v2/v3/v4, PancakeSwap all map into this shape.
/// The DEX adapter is responsible for normalizing pool-specific event formats.
///
/// For Uniswap v3/v4 and Whirlpool (Orca), tick/price range details are not
/// captured here — only the net amounts in/out. Tick data lives in `PoolEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    pub chain: Chain,
    pub tx_hash: TxHash,
    pub block: BlockRef,
    pub block_time: DateTime<Utc>,

    /// The LP pool / pair contract address.
    pub pool: Address,

    /// DEX program / router that executed the swap.
    /// Examples: Raydium AMM program, Uniswap v2 Router, Orca Whirlpool.
    pub dex: DexKind,

    /// Wallet that initiated the swap (economic sender, not aggregator router).
    pub sender: Address,

    /// Token being sold into the pool.
    pub token_in: Address,

    /// Token being received from the pool.
    pub token_out: Address,

    /// Raw amount of `token_in` consumed.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_in_raw: u128,

    /// Decimal exponent for `token_in`.
    pub decimals_in: u8,

    /// Raw amount of `token_out` received.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_out_raw: u128,

    /// Decimal exponent for `token_out`.
    pub decimals_out: u8,

    /// USD value of the swap at block time. Populated by the indexer from
    /// a price oracle; `None` if no price data is available.
    ///
    /// Uses `Decimal` (not f64). `rust_decimal` with `serde-with-str` feature
    /// serializes this as a string automatically.
    pub usd_value: Option<Decimal>,

    /// Index within the transaction.
    pub log_index: u32,
}

/// Known DEX programs / protocols.
///
/// `#[non_exhaustive]` so Phase 4+ additions (Meteora, Jupiter v7, Uniswap v4)
/// do not break existing match arms in consumer crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DexKind {
    RaydiumV4,
    RaydiumClmm,
    OrcaWhirlpool,
    OrcaLegacy,
    Meteora,
    JupiterAggregator,
    PumpFun,
    UniswapV2,
    UniswapV3,
    UniswapV4,
    PancakeSwapV2,
    PancakeSwapV3,
    /// Catch-all for unrecognised DEX programs.
    Unknown,
}

// ---------------------------------------------------------------------------
// PoolEvent
// ---------------------------------------------------------------------------

/// An LP pool state event: mint (add liquidity), burn (remove), sync (reserve update),
/// initialize (pool creation).
///
/// The rug-pull detector primarily consumes `PoolEvent::Burn` to detect LP drains.
/// The holder-concentration detector uses `PoolEvent::Sync` to track reserve deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolEvent {
    pub chain: Chain,
    pub tx_hash: TxHash,
    pub block: BlockRef,
    pub block_time: DateTime<Utc>,

    /// Pool / pair address.
    pub pool: Address,

    pub dex: DexKind,

    /// The specific kind of LP state change.
    pub kind: PoolEventKind,

    /// Address of the wallet performing the liquidity operation.
    pub actor: Address,

    /// log_index for uniqueness within a transaction.
    pub log_index: u32,
}

/// The payload varies by event kind.
///
/// All raw amounts use `u128`. USD values use `Decimal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PoolEventKind {
    /// LP token minted (liquidity added).
    Mint {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount1_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        lp_tokens_minted: u128,
    },
    /// LP token burned (liquidity removed).
    Burn {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        amount1_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        lp_tokens_burned: u128,
    },
    /// Reserve sync (Uniswap v2 / Raydium style) — full reserve snapshot.
    Sync {
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        reserve0_raw: u128,
        #[serde(
            serialize_with = "serialize_u128_as_str",
            deserialize_with = "deserialize_u128_from_str"
        )]
        reserve1_raw: u128,
    },
    /// Pool initialization (first liquidity add or pool creation).
    Initialize {
        token0: Address,
        token1: Address,
    },
}
```

---

### `token.rs`

```rust
//! Token metadata and holder state types.
//!
//! `TokenMeta` is the primary type here. It is designed as a **superset** of the
//! RugCheck v1 API live-response (verified 2026-04-21, `research/01-market-scan.md`
//! §RugCheck.xyz "Specific signals exposed") plus Honeypot.is EVM fields reserved
//! for Phase 4.
//!
//! Fields marked "Phase 4 reserved" are present in the struct now so that
//! `TokenMeta` can be written to storage and served via REST in a future-compatible
//! way, but they will be `None` for all Phase 1/2 events. Avoid branching on them
//! in Phase 2 detector code.
//!
//! ## Serde strategy
//! - `rename_all = "camelCase"` for REST/WS wire compatibility with RugCheck field names.
//!   The RugCheck API uses camelCase (e.g., `mintAuthority`, `freezeAuthority`, `topHolders`).
//!   This matches the JS consumer expectation and avoids a naming mismatch with the
//!   competitor schema we're starting from (ADR 0001 §D6).
//! - All amount fields serialize as strings. See `amount.rs`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::amount::{deserialize_u128_from_str, serialize_u128_as_str};
use crate::chain::{Address, Chain};

// ---------------------------------------------------------------------------
// TokenMeta — RugCheck superset
// ---------------------------------------------------------------------------

/// Full token metadata for a Solana SPL or EVM ERC-20 token.
///
/// Updated by the `token-registry` crate whenever on-chain state changes.
/// Persisted in Postgres (`tokens` table) for hot access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenMeta {
    // --- Identity ---

    /// The token's mint address (Solana) or contract address (EVM).
    /// Stored in chain-canonical form (Base58 / checksum hex).
    pub mint: Address,

    /// Which chain this token lives on.
    pub chain: Chain,

    /// Ticker symbol. May be empty for newly deployed tokens not yet indexed.
    pub symbol: Option<String>,

    /// Human-readable name.
    pub name: Option<String>,

    /// Decimal exponent. CRITICAL: never hardcode 18 for EVM tokens.
    pub decimals: u8,

    /// Token program address (Solana). Distinguishes SPL Token from Token-2022.
    /// `None` for EVM tokens.
    pub token_program: Option<Address>,

    // --- Supply ---

    /// Total on-chain supply in raw units. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub total_supply_raw: u128,

    /// Circulating supply (excluding locked/burned/deployer cluster).
    /// Computed by `token-registry`; may lag real-time.
    /// `None` if not yet computed.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub circulating_supply_raw: Option<u128>,

    // --- Authority flags (RugCheck core signals) ---

    /// Who can mint new tokens. `None` = authority revoked (safer).
    /// RugCheck field: `mintAuthority`.
    pub mint_authority: Option<Address>,

    /// Who can freeze token accounts. `None` = authority revoked.
    /// RugCheck field: `freezeAuthority`.
    pub freeze_authority: Option<Address>,

    // --- Deployer / creator ---

    /// Wallet that deployed the token.
    /// RugCheck field: `creator`.
    pub creator: Option<Address>,

    /// Current balance of the creator wallet in raw token units.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str",
        default
    )]
    pub creator_balance_raw: u128,

    // --- Token-2022 extensions (Solana) ---

    /// Transfer fee configuration for Token-2022 tokens.
    /// `None` for standard SPL tokens or EVM tokens.
    /// RugCheck field: `transferFee`.
    pub transfer_fee: Option<TransferFeeConfig>,

    // --- Holder distribution ---

    /// Top-N holders by balance. N is configurable; typically 20 (RugCheck default).
    /// RugCheck field: `topHolders`.
    pub top_holders: Vec<TopHolder>,

    /// Total number of token holder accounts at snapshot time.
    /// RugCheck field: `totalHolders`.
    pub total_holders: u64,

    // --- Market / LP data ---

    /// All known markets / DEX pools for this token.
    /// RugCheck field: `markets`.
    pub markets: Vec<MarketInfo>,

    /// Total market liquidity across all pools, in USD.
    /// Uses `Decimal`, serialized as string.
    pub total_market_liquidity_usd: Decimal,

    // --- LP lockers ---

    /// LP lock contracts holding liquidity for this token.
    /// RugCheck field: `lockers`.
    pub lockers: Vec<LockerInfo>,

    // --- Insider / bundler graph ---

    /// Whether the RugCheck insider-graph analysis detected bundler clusters.
    /// RugCheck field: `graphInsidersDetected`.
    pub graph_insiders_detected: bool,

    /// Grouped insider wallet networks flagged by RugCheck bundler analysis.
    /// RugCheck field: `insiderNetworks`.
    pub insider_networks: Vec<InsiderNetwork>,

    // --- Launch context ---

    /// Launchpad where the token originated (e.g., "pump.fun", "Raydium AMM", "Moonshot").
    /// RugCheck field: `launchpad`.
    pub launchpad: Option<String>,

    /// Specific deploy platform, may differ from launchpad.
    /// RugCheck field: `deployPlatform`.
    pub deploy_platform: Option<String>,

    /// Timestamp when this token was first detected on-chain.
    /// RugCheck field: `detectedAt`.
    pub detected_at: Option<DateTime<Utc>>,

    // --- Ground-truth label ---

    /// `true` if RugCheck has flagged this token as rugged (post-hoc label).
    /// Used as the positive-class label in fixture corpus (ADR 0001 §D7).
    /// RugCheck field: `rugged`.
    pub rugged: bool,

    // --- Jupiter verification (negative-label filter) ---

    /// Jupiter exchange verification status.
    /// RugCheck field: `verification`.
    pub verification: JupiterVerification,

    // --- RugCheck risk score (for reference, not used as our confidence) ---

    /// Raw RugCheck score (0–1000). Stored for comparison/calibration only.
    /// Do not use as a detector output — use `AnomalyEvent.confidence` instead.
    pub rugcheck_score: Option<u32>,

    // --- Phase 4 reserved: EVM honeypot simulation fields ---
    // These fields will be `None` until Phase 4 EVM chains are activated.
    // Sourced from Honeypot.is `simulationResult` schema (live-verified 2026-04-21).

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.buyTax`.
    /// Tax rate on buys as a percentage (0.0–100.0). Uses `Decimal`.
    pub buy_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.sellTax`.
    pub sell_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is field: `simulationResult.transferTax`.
    pub transfer_tax: Option<Decimal>,

    /// Phase 4 reserved. Honeypot.is `flags[]` — revert reasons / risk categories.
    /// Examples: `"HoneypotSellBlock"`, `"HighSellTax"`, `"TransferPausable"`.
    pub honeypot_flags: Vec<String>,

    // --- Metadata freshness ---

    /// When this `TokenMeta` record was last refreshed from on-chain state.
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Supporting types for TokenMeta
// ---------------------------------------------------------------------------

/// Token-2022 transfer fee configuration.
///
/// From RugCheck `transferFee` field (live-verified). Contains the basis-points
/// fee rate, the maximum fee amount, and the authority that can change it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferFeeConfig {
    /// Fee rate in basis points (1 bp = 0.01%). Range: 0–10_000.
    pub fee_bps: u16,

    /// Maximum fee in raw token units. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub max_fee_raw: u128,

    /// Authority that can update the transfer fee. `None` = revoked.
    pub authority: Option<Address>,
}

/// A single entry in the top-holders list.
///
/// Maps to RugCheck `topHolders[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopHolder {
    pub address: Address,

    /// Percentage of total supply held (0.0–100.0). Uses `Decimal`.
    pub pct: Decimal,

    /// Raw token balance. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub amount_raw: u128,

    /// Whether this is a known insider / deployer cluster wallet.
    pub is_insider: bool,
}

/// A DEX market / LP pool for a token.
///
/// Maps to RugCheck `markets[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketInfo {
    /// Pool / pair contract address.
    pub pool_address: Address,

    /// DEX protocol.
    pub dex: crate::event::DexKind,

    /// Percentage of LP tokens burned (permanently locked). 0.0–100.0.
    /// RugCheck field: `lp_burned_pct`.
    pub lp_burned_pct: Decimal,

    /// Current pool liquidity in USD.
    pub liquidity_usd: Decimal,

    /// Number of LP token holders (LPs providing liquidity).
    pub lp_provider_count: u64,
}

/// An LP lock contract holding tokens for this token's liquidity.
///
/// Maps to RugCheck `lockers[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockerInfo {
    /// The lock contract address.
    pub locker_address: Address,

    /// Human-readable locker name (e.g., "Unicrypt", "Team Finance").
    pub locker_name: Option<String>,

    /// Raw LP tokens locked. Serialized as string.
    #[serde(
        serialize_with = "serialize_u128_as_str",
        deserialize_with = "deserialize_u128_from_str"
    )]
    pub locked_amount_raw: u128,

    /// When the lock expires. `None` if permanent / no expiry.
    pub unlock_at: Option<DateTime<Utc>>,
}

/// A cluster of wallets flagged as likely coordinated insiders by the
/// RugCheck bundler-graph analysis.
///
/// Maps to RugCheck `insiderNetworks[i]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsiderNetwork {
    /// Wallet addresses in this cluster.
    ///
    /// Stored as `Vec` (ordered by discovery time). Use `BTreeMap` keyed on
    /// `Address` if ordering by address becomes necessary for deterministic output.
    pub members: Vec<Address>,

    /// Percentage of total supply controlled by this cluster. `Decimal`.
    pub supply_pct: Decimal,

    /// Whether this cluster appears to have coordinated buy/sell behavior.
    pub is_bundler: bool,
}

/// Jupiter verification flags for this token.
///
/// Maps to RugCheck `verification` field. Used as negative-class labels
/// for the fixture corpus (ADR 0001 §D7): `jup_verified` tokens are
/// expected to be non-rugged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterVerification {
    /// Token appears in Jupiter's "verified" list.
    pub jup_verified: bool,

    /// Token appears in Jupiter's strict (curated) list.
    pub jup_strict: bool,
}

// ---------------------------------------------------------------------------
// HolderSnapshot
// ---------------------------------------------------------------------------

/// A periodic snapshot of the full holder distribution for a token.
///
/// Stored in ClickHouse (`holder_snapshots` table) as a differential — only
/// addresses whose balance changed since the previous snapshot are included.
/// The `is_full` flag distinguishes full snapshots from deltas.
///
/// **Determinism:** `balances` uses `BTreeMap<String, u128>` (address string
/// → raw balance) so that iteration order is deterministic regardless of how
/// the data was ingested. Use the address canonical string as the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HolderSnapshot {
    pub token: Address,
    pub chain: Chain,

    /// Block at which this snapshot was taken.
    pub block: crate::chain::BlockRef,

    /// Wall-clock time of the block.
    pub block_time: DateTime<Utc>,

    /// True = full snapshot of all holders. False = delta (only changed balances).
    pub is_full: bool,

    /// Holder address (canonical string) → raw balance.
    /// `BTreeMap` for deterministic ordering.
    pub balances: BTreeMap<String, u128>,

    /// Total number of non-zero holder accounts at snapshot time.
    pub total_holders: u64,

    /// Pre-computed Gini coefficient for this snapshot. `None` for delta snapshots.
    /// Uses `Decimal`.
    pub gini: Option<Decimal>,

    /// Pre-computed top-10 holder percentage. `None` for delta snapshots.
    pub top10_pct: Option<Decimal>,
}
```

---

### `anomaly.rs`

```rust
//! Detector output contract: AnomalyEvent, Severity, Confidence, Evidence.
//!
//! Per ADR 0001 §D4: every detector emits `AnomalyEvent` — no booleans, no
//! opaque scores. The `confidence` field is a calibrated probability estimate
//! in [0.0, 1.0]. Consumers filter by threshold on their side.
//!
//! ## Serde strategy
//! - `rename_all = "camelCase"` for wire compatibility.
//! - `AnomalyEvent` serializes cleanly for all three delivery modes in ADR 0001 §D8:
//!   in-process crate (zero-copy), REST JSON body, WebSocket frame.
//!
//! ## Determinism
//! `Evidence` uses `BTreeMap` for key-value bags. `Vec` for ordered evidence items
//! where insertion order matters (e.g., a sequence of suspicious transactions in
//! block order). Do not use `HashMap` anywhere in this module.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::chain::{Address, BlockRef, Chain, TxHash};
use crate::error::CommonError;

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// Alert severity level — used for consumer-side routing and UI rendering.
///
/// `#[non_exhaustive]` so additional levels can be added without breaking
/// existing match arms in consumer crates.
///
/// Mapping to RugCheck-style labels:
/// - `Info`     → informational, no action required
/// - `Low`      → flag for review, low urgency
/// - `Medium`   → recommend review before trade
/// - `High`     → strong signal, likely anomaly
/// - `Critical` → immediate action recommended (active rug, honeypot confirmed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

// ---------------------------------------------------------------------------
// Confidence
// ---------------------------------------------------------------------------

/// A calibrated probability estimate in [0.0, 1.0].
///
/// Wraps `f64` with a constructor that enforces the range. This is the **one**
/// allowed use of `f64` in `crates/common` — it represents a probability, not
/// a financial amount. Use `Decimal` for anything monetary.
///
/// ## Serialization
/// Serialized as a JSON number (not a string) because:
/// 1. It IS an f64 — JSON number precision is fine for a probability (15+ sig figs).
/// 2. Consumers (trading bot, MM) need to compare it against threshold constants.
///
/// ## Construction
/// ```rust
/// use mg_onchain_common::anomaly::Confidence;
///
/// let c = Confidence::new(0.85).unwrap();
/// let d = Confidence::new(1.5); // Err(ConfidenceOutOfRange)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Confidence(f64);

impl Confidence {
    /// Construct a `Confidence` value, returning an error if outside [0.0, 1.0].
    pub fn new(v: f64) -> Result<Self, CommonError> {
        if v.is_nan() || v < 0.0 || v > 1.0 {
            Err(CommonError::ConfidenceOutOfRange { value: v })
        } else {
            Ok(Self(v))
        }
    }

    /// The raw f64 value.
    pub fn value(&self) -> f64 {
        self.0
    }

    /// 0.0 — the lowest possible confidence.
    pub const ZERO: Self = Self(0.0);

    /// 1.0 — absolute certainty (use only for confirmed simulation results).
    pub const ONE: Self = Self(1.0);
}

impl TryFrom<f64> for Confidence {
    type Error = CommonError;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// A structured bundle of supporting facts for an `AnomalyEvent`.
///
/// Designed to be:
/// - **Inspectable by a human reviewer:** the `metrics` map carries named
///   Decimal values (percentages, counts, USD amounts); `addresses` carries
///   relevant wallets; `tx_hashes` carries the transactions that triggered the alert.
/// - **Serializable for REST/WS:** all fields are `Serialize + Deserialize`.
/// - **Deterministic:** `BTreeMap` used for all keyed bags.
///
/// Evidence is intentionally open-ended (new detectors add new keys) rather
/// than strongly typed per detector — strongly-typed variants would require
/// a new `Evidence` enum case per detector and couple `crates/common` to
/// detector internals, violating the dependency direction rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    /// Transaction hashes that triggered or support the alert.
    pub tx_hashes: Vec<TxHash>,

    /// Wallet addresses implicated in the anomaly.
    pub addresses: Vec<Address>,

    /// Named numeric metrics supporting the detection.
    ///
    /// Examples:
    /// - `"lp_removed_pct"` → `Decimal("0.92")`  (rug pull)
    /// - `"sell_tax"` → `Decimal("0.85")`           (honeypot)
    /// - `"gini_delta"` → `Decimal("0.12")`          (concentration)
    /// - `"volume_z_score"` → `Decimal("8.4")`       (pump&dump)
    ///
    /// All values use `Decimal` — never f64. `BTreeMap` for deterministic ordering.
    pub metrics: BTreeMap<String, Decimal>,

    /// Free-form string annotations (e.g., "creator dumped 94% in 2 txs").
    /// Populated by detectors for human-readable audit trail.
    pub notes: Vec<String>,

    /// Block range over which the evidence was observed.
    pub observed_range: Option<(BlockRef, BlockRef)>,
}

impl Evidence {
    /// Create an empty evidence bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: add a metric.
    pub fn with_metric(mut self, key: impl Into<String>, value: Decimal) -> Self {
        self.metrics.insert(key.into(), value);
        self
    }

    /// Builder: add a tx hash.
    pub fn with_tx(mut self, tx: TxHash) -> Self {
        self.tx_hashes.push(tx);
        self
    }

    /// Builder: add an address.
    pub fn with_address(mut self, addr: Address) -> Self {
        self.addresses.push(addr);
        self
    }

    /// Builder: add a note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

// ---------------------------------------------------------------------------
// AnomalyEvent
// ---------------------------------------------------------------------------

/// The primary output of every detector.
///
/// Per ADR 0001 §D4: no booleans. `confidence` ∈ [0.0, 1.0].
/// `severity` is set by the detector as a classification hint; consumers may
/// override based on their own threshold configuration.
///
/// ## Delivery modes (ADR 0001 §D8)
///
/// - **In-process crate:** `AnomalyEvent` is passed directly by value via channel.
/// - **REST:** serialized as a JSON object; see OpenAPI spec in `crates/gateway`.
/// - **WebSocket:** same JSON serialization, streamed in a WS frame.
///
/// ## Determinism
///
/// Given the same input block range and config, two detector runs MUST emit
/// identical `AnomalyEvent` values (field for field). The `observed_at` field
/// is the block time — NOT wall-clock time — to satisfy this requirement.
/// Wall-clock observation time is tracked separately in `ingested_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnomalyEvent {
    /// Stable identifier for the detector that produced this event.
    ///
    /// Convention: `snake_case`, e.g. `"rug_pull_lp_drain"`, `"honeypot_sim"`,
    /// `"holder_concentration_shift"`. Defined as a constant in each detector crate.
    pub detector_id: String,

    /// The token this event is about.
    pub token: Address,

    /// Which chain the token lives on.
    pub chain: Chain,

    /// Calibrated probability that the anomaly is real.
    /// 0.0 = certainly benign. 1.0 = certainly anomalous.
    pub confidence: Confidence,

    /// Severity classification. Consumers can override based on their
    /// risk tolerance, but the detector provides an informed starting point.
    pub severity: Severity,

    /// Evidence bundle: transactions, wallets, metrics, notes.
    pub evidence: Evidence,

    /// The block time of the last block in the observation window.
    ///
    /// MUST be the block timestamp, not `Utc::now()`. This is what makes
    /// detector output reproducible given the same input block range.
    pub observed_at: DateTime<Utc>,

    /// The block range over which the anomaly was observed.
    /// `(start_block, end_block)` inclusive. Both carry chain context.
    pub window: (BlockRef, BlockRef),

    /// Wall-clock time when this event was computed and dispatched.
    ///
    /// This is the ONLY field that uses wall-clock time. It is audit metadata
    /// and does not affect detector reproducibility — two runs of the same
    /// block range will differ in `ingested_at` but not in any other field.
    pub ingested_at: DateTime<Utc>,
}
```

---

### `lib.rs`

```rust
//! `mg-onchain-common` — shared domain types for the `mg-onchain-analysis` workspace.
//!
//! # Amount encoding
//!
//! This crate follows the CLAUDE.md §Code Style invariant strictly:
//!
//! - **`u128`** for raw on-chain token amounts (serialized as JSON strings).
//! - **`rust_decimal::Decimal`** for human-scaled quantities: USD values,
//!   percentages, tax rates, Gini coefficients (serialized as JSON strings via
//!   the `serde-with-str` feature).
//! - **`f64`** is used ONLY in `Confidence`, which represents a probability
//!   estimate in [0.0, 1.0] and is guarded by the `Confidence::new` constructor.
//!
//! Never introduce a bare `f64` field in any struct in this crate.
//!
//! # Serde conventions
//!
//! Wire format uses `camelCase` field names (`rename_all = "camelCase"`) for
//! compatibility with the RugCheck v1 API response shape (ADR 0001 §D6) and
//! JS consumer expectations. Internal Rust code uses snake_case as usual.
//!
//! Enum variants on the wire use `snake_case` or `lowercase` as documented
//! per type. See `Chain`, `Severity`, `DexKind`.

pub mod amount;
pub mod anomaly;
pub mod chain;
pub mod error;
pub mod event;
pub mod token;

// Flat re-exports of the most commonly used types for ergonomic `use mg_onchain_common::*`.
pub use anomaly::{AnomalyEvent, Confidence, Evidence, Severity};
pub use chain::{Address, BlockRef, Chain, TxHash};
pub use error::CommonError;
pub use event::{DexKind, PoolEvent, PoolEventKind, Swap, Transfer};
pub use token::{
    HolderSnapshot, InsiderNetwork, JupiterVerification, LockerInfo, MarketInfo,
    TokenMeta, TopHolder, TransferFeeConfig,
};
```

---

## Structural decisions — rationale

### 1. Module split

Six modules match six distinct themes. This avoids a 1,000-line `lib.rs` and lets the developer work on `anomaly.rs` without rebasing against `token.rs` changes. The split is stable across Phase 1–4: new fields go into existing modules, new chain-specific variants go into `chain.rs`.

### 2. Error type

One `CommonError` with `#[non_exhaustive]`. This matches `mg-custody`'s pattern (`CustodyError` in `crates/common/src/error.rs`). Per-module error types would require consumers to import multiple error types and complicate `?` propagation across module boundaries.

### 3. Serde strategy — camelCase

`rename_all = "camelCase"` on all structs. Rationale:

- RugCheck's live-verified API response uses camelCase (`mintAuthority`, `freezeAuthority`, `topHolders`, `lp_burned_pct` is the one exception — RugCheck actually uses snake_case for LP fields, so `MarketInfo.lp_burned_pct` is left as-is with `#[serde(rename = "lp_burned_pct")]` if needed).
- The REST/WS consumers (MM, exchange, custody) are web-native; camelCase is the JSON convention they expect.
- `bot-trader-2-0` uses Rust in-process, so the field naming convention is irrelevant on the wire for that consumer. Rust code uses snake_case internally via the standard `#[derive(Serialize, Deserialize)]` mapping.
- `mg-custody` uses `snake_case` in some places and `camelCase` in none of its types. Since `mg-custody` consumes `mg-onchain-analysis` via REST (not as a shared crate dependency), the wire format is what matters — and camelCase is the better choice for the REST consumers overall.

**Precedent finding:** `mg-custody/crates/common/src/types.rs` uses `rename_all = "snake_case"` on enums and does not rename struct fields. `bot-trader-2-0` uses no rename on its domain types. Neither project has a conflicting convention because neither shares types with this crate — their APIs are unrelated. We choose camelCase for REST compatibility with the RugCheck-schema reference.

### 4. Extensibility — `#[non_exhaustive]`

Applied to: `Chain`, `Severity`, `DexKind`, `PoolEventKind`. These are the variants most likely to grow in Phase 4+. All other enums (e.g., `DepositStatus` in mg-custody) are not non-exhaustive because they are not consumer-facing extension points.

### 5. Determinism — `BTreeMap` over `HashMap`

All key-value bags in `Evidence.metrics`, `HolderSnapshot.balances`, and any future metadata maps use `BTreeMap<String, _>`. This eliminates non-deterministic iteration order, satisfying CLAUDE.md Detector Rule #5. The performance cost (O(log N) vs O(1) per lookup) is negligible for maps of the sizes involved (< 100 keys per event).

### 6. Timestamps — `chrono::DateTime<Utc>`

`mg-custody` uses `chrono::DateTime<Utc>` throughout (`Cargo.toml`: `chrono = { version = "0.4", features = ["serde"] }`). We match this. The `time` crate is not used by either sibling project. `chrono` is already in the workspace dependency list established by Task 1.

Two timestamp roles are explicitly separated in `AnomalyEvent`:
- `observed_at` = block time (deterministic, from chain data).
- `ingested_at` = wall-clock (non-deterministic, audit only).

This prevents the common mistake of using `Utc::now()` as the event timestamp.

### 7. TxHash — typed enum, not opaque bytes

`TxHash` is `enum { Solana([u8; 64]), Evm([u8; 32]) }`. This gives compile-time enforcement that a Solana signature (64 bytes) is never accidentally used where an EVM hash (32 bytes) is expected. The trade-off is that deserialization requires chain context, which is always available from the surrounding struct. An opaque `Bytes` type would be simpler but would lose type safety at adapter boundaries.

### 8. Address deserialisation

`Address` cannot implement `Deserialize` without chain context. Two approaches exist:
1. Store `Address` as a plain `String` in all structs and parse lazily.
2. Use a custom deserializer that reads the `chain` field first (requires `serde` tricks with `deserialize_with` or `SeqAccess`).

This design leaves this as an open question (see below). The developer should pick one and document it.

---

## Open questions (max 5)

**OQ1 — Address deserialization strategy.**
`Address` cannot be deserialized from JSON without knowing the chain. Should all structs store `Address` as a pre-normalized `String` internally and parse to `Address` via a `try_from` at the application boundary? Or should `Address` embed a `Chain` tag and use a `#[serde(deserialize_with)]` pair that reads `chain` before `address` fields? The former is simpler; the latter is type-safe throughout. Decision affects every struct in `event.rs` and `token.rs`.

**OQ2 — `U256` for EVM overflow safety.**
`u128` covers Solana amounts (max SPL supply ~2^64 for standard tokens, u128 for Token-2022). EVM `uint256` values can exceed `u128::MAX` in theory (e.g., governance token supplies with many decimals). Should `crates/common` include a dependency on `primitive-types` (for `U256`) and define a newtype for raw amounts that can be either `u128` or `U256` based on chain? Or accept the limitation that EVM amounts exceeding `u128::MAX` are truncated and log a warning? Phase 1 is Solana-only so this is not urgent, but the storage schema and wire format must be decided before Phase 4.

**OQ3 — `Evidence.metrics` key namespace.**
Multiple detectors may emit evidence under the same key name (e.g., `"volume_change"` from both pump-and-dump and wash-trading detectors). Should metric keys be namespaced by detector (`"rug_pull/lp_removed_pct"` vs `"lp_removed_pct"`)? Or should `AnomalyEvent` include one evidence bundle per detector (already disambiguated by `detector_id`)? The current design has one evidence per event, so keys are implicitly namespaced by detector — but a human reviewer looking at multiple events for the same token may want consistent key names across detectors.

**OQ4 — `DexKind::Unknown` and Solana Geyser program IDs.**
Yellowstone gRPC delivers raw program-level events; the DEX adapter must match known program IDs to `DexKind` variants. Should unrecognised program IDs produce `DexKind::Unknown` (safe, lossy) or be stored separately (e.g., in `BTreeMap<String, _>` extra fields)? A DEX producing events as `Unknown` will cause the rug-pull and wash-trading detectors to operate with partial information. The developer should specify an explicit policy for how `DexKind::Unknown` events are handled in each detector.

**OQ5 — `HolderSnapshot` diff semantics in ClickHouse.**
The design says delta snapshots contain only changed balances. But ClickHouse's MergeTree does not natively support "apply deltas" — queries against the `holder_snapshots` table must either reconstruct full state from the delta chain or use a separate summary table. This is partly a storage crate concern but the `HolderSnapshot.is_full` field here must align with the ClickHouse schema design in Task 4. The developer should confirm with the data-engineer agent (Task 4) before finalising the struct.

---

## Non-goals

- **Detector traits** (`Detector`, `StatefulDetector`). These live in `crates/detectors`. `crates/common` exports only the I/O types, not the trait that detectors implement.
- **Storage types** (`Insertable`, `Queryable` wrappers for sqlx/clickhouse-rs). Those live in `crates/storage` and import from `crates/common`, not the reverse.
- **Network / transport types** (HTTP request/response shapes, WebSocket message envelopes, OpenAPI `#[schema]` annotations). Those live in `crates/gateway`.
- **Chain-adapter logic** (Yellowstone gRPC parsing, account layout decoding). Those live in `crates/chain-adapter`.
- **Config types** (`config/detectors.toml` structs). Those are per-crate config modules.
- **`Cargo.toml` additions.** The developer must add `rust_decimal = { version = "1", features = ["serde-with-str"] }`, `chrono = { version = "0.4", features = ["serde"] }`, `thiserror = "2"`, `bs58`, `hex`, and `serde_json` to the `common` crate's `Cargo.toml` as workspace dependencies (most are already declared in the workspace root by Task 1).

---

## Acceptance checks for the developer

Walk through these in order before marking Task 2 complete:

- [ ] `cargo check -p mg-onchain-common` exits zero with no warnings.
- [ ] `cargo test -p mg-onchain-common` passes. Even if only doctest sanity checks, at least the examples in `amount.rs` and `anomaly.rs` module docs must compile and pass.
- [ ] `rust_decimal::Decimal` is used for every human-scaled field. No `f64` field exists in any `pub` struct except `Confidence`'s inner `f64`.
- [ ] All raw `u128` amount fields serialize as JSON strings. Doctest:
  ```rust
  // In amount.rs or lib.rs doctest:
  use mg_onchain_common::amount::serialize_u128_as_str;
  use serde::Serialize;
  use serde_json;

  #[derive(Serialize)]
  struct Probe {
      #[serde(serialize_with = "mg_onchain_common::amount::serialize_u128_as_str")]
      amount: u128,
  }
  let p = Probe { amount: u128::MAX };
  let json = serde_json::to_string(&p).unwrap();
  assert!(json.contains('"'), "u128 must be a JSON string, not a number");
  assert_eq!(json, r#"{"amount":"340282366920938463463374607431768211455"}"#);
  ```
- [ ] `Confidence::new(1.5)` returns `Err(CommonError::ConfidenceOutOfRange { .. })`. Unit test required.
- [ ] `Confidence::new(f64::NAN)` also returns `Err`. Unit test required.
- [ ] `Chain` round-trips through `serde_json`: `"solana"` → `Chain::Solana` → `"solana"`. Test using the same pattern as `mg-custody/crates/common/src/types.rs::chain_serialization()`.
- [ ] `Address::from_str_on_chain` for a known Solana address (e.g., the SOL native mint `So11111111111111111111111111111111111111112`) round-trips: parse → display → re-parse produces the same canonical string.
- [ ] `HolderSnapshot.balances` is `BTreeMap`, not `HashMap`. Grep check: `grep -r "HashMap" crates/common/src/` must return nothing.
- [ ] `Evidence.metrics` is `BTreeMap`, not `HashMap`. Same grep check.
- [ ] `AnomalyEvent` serializes cleanly with `serde_json::to_string` and deserializes back to an equal value. Roundtrip doctest using a constructed event with all non-optional fields populated.
- [ ] `TokenMeta` includes all RugCheck fields listed in `research/01-market-scan.md` §RugCheck.xyz "Specific signals exposed": `mint`, `tokenProgram`, `creator`, `creatorBalance`, `token_extensions`/`transferFee`, `topHolders`, `freezeAuthority`, `mintAuthority`, `lockers`, `markets` (with `lp_burned_pct`), `totalHolders`, `rugged`, `verification` (jup_verified / jup_strict), `graphInsidersDetected`, `insiderNetworks`, `detectedAt`, `launchpad`, `deployPlatform`. Check each field name against the live-verified list.
- [ ] Phase 4 reserved fields (`buy_tax`, `sell_tax`, `transfer_tax`, `honeypot_flags`) are present in `TokenMeta` and default to `None` / empty `Vec`. They must NOT cause compile errors when omitted from construction in test fixtures.
- [ ] No file in `crates/common/src/` imports from any other workspace crate (e.g., `use chain_adapter::...`). This is the foundational dependency-direction rule.
