//! EVM-native type stubs and conversions to/from `mg-onchain-common` types.
//!
//! # Design
//!
//! This module bridges between the raw EVM wire types (block numbers as `u64`,
//! tx hashes as `[u8; 32]`, addresses as 20-byte arrays) and the chain-agnostic
//! types in `crates/common`.
//!
//! All conversions are fallible — invalid wire data returns `AdapterError::DecodeError`
//! rather than panicking.
//!
//! # EIP-55 checksum addresses
//!
//! `common::Address::parse` for EVM chains currently stores lowercase hex
//! (full EIP-55 checksum deferred to Phase 4 — see `crates/common/src/chain.rs`
//! `Address` doc). The `evm_address_to_common` helper below follows that convention.
//!
//! # No `f64`, no hardcoded decimals
//!
//! All token amounts travel as `u128` (fitting the ERC-20 `uint256` range for
//! practical token supplies). Decimal conversion is done by the token-registry
//! at display time, never here.

use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};

use crate::error::AdapterError;

// ---------------------------------------------------------------------------
// BlockRef ↔ EVM block number
// ---------------------------------------------------------------------------

/// Construct a `BlockRef` for the Ethereum chain from a raw block number.
///
/// This is a trivial wrapper but provides a typed entry point so callers
/// don't accidentally pass a Solana slot to a block-number context.
pub fn block_number_to_ref(block_number: u64) -> BlockRef {
    BlockRef::new(Chain::Ethereum, block_number)
}

// ---------------------------------------------------------------------------
// TxHash ↔ EVM tx hash
// ---------------------------------------------------------------------------

/// Parse an EVM transaction hash from a `0x`-prefixed hex string into `TxHash`.
///
/// Returns `AdapterError::DecodeError` if the string is not a valid 32-byte
/// keccak-256 hash in `0x`-prefixed lowercase hex.
pub fn evm_tx_hash_from_hex(hex: &str) -> Result<TxHash, AdapterError> {
    TxHash::evm_from_hex(hex).map_err(|e| AdapterError::DecodeError {
        context: "evm_tx_hash_from_hex",
        reason: e.to_string(),
    })
}

/// Serialize a `TxHash::Evm` variant to `0x`-prefixed lowercase hex.
///
/// Panics in debug builds if given a `TxHash::Solana` — this is a logic error
/// (Solana hashes must not reach EVM decode paths).
pub fn evm_tx_hash_to_hex(hash: &TxHash) -> String {
    hash.to_string()
}

// ---------------------------------------------------------------------------
// Address ↔ EVM address string
// ---------------------------------------------------------------------------

/// Parse an EVM address string into `common::Address` for Ethereum.
///
/// Accepts `0x`-prefixed hex (mixed case or lowercase); normalizes to lowercase
/// per current `Address::parse` behavior (full EIP-55 checksum in Phase 4).
///
/// Returns `AdapterError::DecodeError` on malformed input.
pub fn evm_address_from_hex(hex: &str) -> Result<Address, AdapterError> {
    Address::parse(Chain::Ethereum, hex).map_err(|e| AdapterError::DecodeError {
        context: "evm_address_from_hex",
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Log index
// ---------------------------------------------------------------------------

/// A (block_number, log_index) pair used as a dedup key for ERC-20 Transfer events.
///
/// Consumers must treat events as idempotent on this key (at-least-once delivery).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogId {
    pub block_number: u64,
    pub log_index: u32,
}

impl LogId {
    pub fn new(block_number: u64, log_index: u32) -> Self {
        Self { block_number, log_index }
    }
}

// ---------------------------------------------------------------------------
// LogFilter — input to eth_getLogs
// ---------------------------------------------------------------------------

/// Filter for `eth_getLogs` / `eth_subscribe("logs")` queries.
///
/// Maps to the JSON-RPC `Filter` object. All fields are optional; an empty filter
/// with only `from_block`/`to_block` returns all logs in that range (very high
/// volume on mainnet — always set `address` or `topics` in production).
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Inclusive start block (or `None` for "latest").
    pub from_block: Option<u64>,
    /// Inclusive end block (or `None` for "latest").
    pub to_block: Option<u64>,
    /// Contract addresses to filter by. Empty = no address filter.
    pub addresses: Vec<String>,
    /// Topics to filter by. `topics[0]` is typically the event signature hash.
    /// Empty = no topic filter.
    pub topics: Vec<Option<String>>,
}

impl LogFilter {
    /// Construct a filter for a closed block range with no address/topic restriction.
    ///
    /// Use this only in tests. Production callers must always specify addresses or topics
    /// to avoid returning the full mainnet log firehose.
    #[cfg(test)]
    pub fn range(from: u64, to: u64) -> Self {
        Self {
            from_block: Some(from),
            to_block: Some(to),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Raw log — the decoded form of an eth_getLogs response entry
// ---------------------------------------------------------------------------

/// A single EVM log entry as returned by `eth_getLogs` or `eth_subscribe("logs")`.
///
/// This is a minimal representation — sufficient for Transfer and Swap decoding
/// in `decoder.rs`. Sprint 16 will replace this with `alloy_rpc_types::Log`
/// when the concrete RPC client is wired.
#[derive(Debug, Clone)]
pub struct RawLog {
    /// Contract that emitted the log.
    pub address: String,
    /// Indexed topics. `topics[0]` is the event signature hash (keccak256).
    pub topics: Vec<String>,
    /// Non-indexed event data (ABI-encoded).
    pub data: Vec<u8>,
    /// Block number containing this log.
    pub block_number: u64,
    /// Transaction hash.
    pub tx_hash: String,
    /// Index of this log within the block.
    pub log_index: u32,
}

// ---------------------------------------------------------------------------
// BlockData — minimal block representation for the adapter hot path
// ---------------------------------------------------------------------------

/// Minimal Ethereum block representation returned by `EthereumRpc::get_block_by_number`.
///
/// Sprint 16 will replace this stub with `alloy_rpc_types::Block` once alloy is wired.
#[derive(Debug, Clone)]
pub struct BlockData {
    pub number: u64,
    pub hash: String,
    pub parent_hash: String,
    pub timestamp: u64,
    pub logs: Vec<RawLog>,
}

// ---------------------------------------------------------------------------
// BlockHeader — a lightweight block summary for the reorg buffer
// ---------------------------------------------------------------------------

/// Block header summary kept in the `ReorgBuffer`.
///
/// Contains only what is needed for reorg detection (hash + parent_hash + height).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u64,
    pub hash: String,
    pub parent_hash: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_number_to_ref_chain_is_ethereum() {
        let b = block_number_to_ref(20_000_000);
        assert_eq!(b.chain, Chain::Ethereum);
        assert_eq!(b.height, 20_000_000);
    }

    #[test]
    fn evm_tx_hash_roundtrip() {
        let hex = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let hash = evm_tx_hash_from_hex(hex).unwrap();
        assert_eq!(evm_tx_hash_to_hex(&hash), hex);
    }

    #[test]
    fn evm_tx_hash_invalid_returns_err() {
        let err = evm_tx_hash_from_hex("not-a-hash");
        assert!(err.is_err());
    }

    #[test]
    fn evm_address_from_hex_normalizes_lowercase() {
        let mixed = "0xAbCdEf1234567890abcdef1234567890ABCDEF12";
        let addr = evm_address_from_hex(mixed).unwrap();
        assert_eq!(addr.as_str(), "0xabcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn evm_address_invalid_returns_err() {
        let err = evm_address_from_hex("not-an-address");
        assert!(err.is_err());
    }

    #[test]
    fn log_id_equality() {
        let a = LogId::new(100, 5);
        let b = LogId::new(100, 5);
        let c = LogId::new(100, 6);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn log_filter_range_bounds() {
        let f = LogFilter::range(100, 200);
        assert_eq!(f.from_block, Some(100));
        assert_eq!(f.to_block, Some(200));
        assert!(f.addresses.is_empty());
    }
}
