//! `mg-onchain-dex-adapter` — DEX instruction decoders for on-chain event enrichment.
//!
//! # Purpose
//!
//! The chain adapter (`crates/chain-adapter`) tags swap instructions with a
//! [`DexKind`] by program ID but does not decode pool-specific amounts. This
//! crate provides the layout decoders that convert raw instruction bytes into
//! typed [`Swap`] and [`PoolEvent`] values from `crates/common`.
//!
//! # Architecture: stateless decoder trait (Architecture A)
//!
//! `DexAdapter` is a **stateless decoder trait** — a pure function, no network,
//! no RPC. It takes enough context to decode one instruction (program ID +
//! instruction data + ordered account keys + optional decimals) and returns
//! decoded events. This makes decoders:
//! - Trivially testable without mocking infrastructure.
//! - Deterministic: same bytes in → same output out every time.
//! - Composable: `chain-adapter` calls `decode_solana()` inline (Architecture A).
//!
//! Architecture A was chosen over B (intermediate `RawDexInstruction` pipeline
//! stage) because:
//! - The chain adapter already has the account keys and instruction data in scope.
//! - Adding a pipeline stage adds latency + complexity with no benefit until
//!   multi-chain enrichment (Phase 4) requires it.
//! - The indexer (`crates/indexer`, P2-2) can call `decode_solana()` directly
//!   if it needs to re-enrich events from storage.
//!
//! # Sprint 2 scope
//!
//! - Raydium AMM v4 (`crates/dex-adapter/src/solana/raydium_v4.rs`)
//! - Raydium CPMM (`crates/dex-adapter/src/solana/raydium_cpmm.rs`)
//!
//! # Deferred
//!
//! TODO(sprint-3): Raydium CLMM tick math (concentrated liquidity, similar to Uniswap v3)
//! TODO(sprint-3): Orca Whirlpool
//! TODO(sprint-3): Meteora DLMM
//! TODO(sprint-3): Jupiter aggregator (routes through multiple pools, no single pool layout)
//! TODO(sprint-4): EVM DEXes (Uniswap v2/v3/v4, PancakeSwap) — Phase 4

pub mod error;
pub mod pool_accounts {
    //! Re-export `solana::pool_accounts` at the crate root so external crates
    //! can use `mg_onchain_dex_adapter::pool_accounts::PoolAccountProvider` etc.
    pub use crate::solana::pool_accounts::*;
}
pub mod solana;

use serde::{Deserialize, Serialize};

use mg_onchain_common::chain::{BlockRef, TxHash};
use mg_onchain_common::event::{PoolEvent, Swap};

use chrono::{DateTime, Utc};

pub use error::DexAdapterError;
pub use solana::decode_solana;
pub use solana::{RAYDIUM_CPMM_PROGRAM_ID, RAYDIUM_V4_PROGRAM_ID};

// Simulation builder types — re-exported for consumers (e.g., detectors).
pub use solana::raydium_cpmm::{
    build_swap_base_input_instruction, build_swap_base_input_transaction, RaydiumCpmmSwapAccounts,
};
pub use solana::raydium_v4::{
    build_swap_base_in_instruction, build_swap_base_in_transaction, RaydiumV4SwapAccounts,
};
pub use solana::simulation::derive_simulation_keypair;

// Pool account provider — abstraction over pool-state fetching for simulation.
pub use solana::pool_accounts::{NotWiredPoolAccountProvider, PoolAccountError, PoolAccountProvider};
#[cfg(any(test, feature = "test-utils"))]
pub use solana::pool_accounts::MockPoolAccountProvider;

// ---------------------------------------------------------------------------
// DecodedEvent — output of DexAdapter::decode
// ---------------------------------------------------------------------------

/// The output of a successful DEX instruction decode.
///
/// A single decoded event is either a swap or a pool liquidity event.
/// The indexer converts these to [`mg_onchain_chain_adapter::Event`] variants
/// before storing them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecodedEvent {
    /// A swap instruction decoded to a typed `Swap`.
    Swap(Swap),
    /// An LP pool event (mint, burn, initialize, sync) decoded to a `PoolEvent`.
    PoolEvent(PoolEvent),
}

// ---------------------------------------------------------------------------
// DexAdapter trait
// ---------------------------------------------------------------------------

/// Stateless DEX instruction decoder.
///
/// Implementations decode raw instruction bytes (identified by `program_id`) into
/// typed `DecodedEvent` values. No network calls, no state, no RPC.
///
/// # Example
///
/// ```rust,ignore
/// use mg_onchain_dex_adapter::{DexAdapter, DecodedEvent};
/// use mg_onchain_dex_adapter::solana::SolanaDexDecoder;
///
/// let decoder = SolanaDexDecoder;
/// let events = decoder.decode(
///     "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
///     &ix_data,
///     &accounts,
///     &tx_hash,
///     block,
///     block_time,
///     log_index,
///     9,   // decimals_in (SOL)
///     6,   // decimals_out
/// )?;
/// ```
pub trait DexAdapter: Send + Sync {
    /// Decode one instruction into zero or more events.
    ///
    /// # Arguments
    ///
    /// - `program_id`: Base58 (Solana) or checksummed hex 0x-prefixed (EVM) program/factory address.
    /// - `ix_data`: Raw instruction data bytes, including discriminator.
    /// - `accounts`: Ordered account addresses for this instruction (Base58 for Solana, checksummed hex for EVM).
    /// - `tx_hash`: Transaction hash for event attribution.
    /// - `block`: Block/slot reference.
    /// - `block_time`: Wall-clock time of the block.
    /// - `log_index`: Instruction index within the transaction (for dedup key).
    /// - `decimals_in`: Decimal exponent for the input token. Use `0` as sentinel if unknown.
    /// - `decimals_out`: Decimal exponent for the output token. Use `0` as sentinel if unknown.
    ///
    /// # Returns
    ///
    /// - `Ok(vec![...])` — zero or more decoded events. Zero is valid for admin instructions.
    /// - `Err(DexAdapterError)` — malformed data (truncated, invalid address, wrong program).
    ///   Callers should log at WARN level and skip the instruction.
    ///
    /// # Determinism requirement
    ///
    /// Given the same arguments, the output MUST be identical on every call.
    /// No time-based randomness, no RNG, no side effects.
    // Decoder API arguments are semantically distinct; grouping into a struct
    // reduces call-site clarity for callers building context inline.
    #[allow(clippy::too_many_arguments)]
    fn decode(
        &self,
        program_id: &str,
        ix_data: &[u8],
        accounts: &[String],
        tx_hash: &TxHash,
        block: BlockRef,
        block_time: DateTime<Utc>,
        log_index: u32,
        decimals_in: u8,
        decimals_out: u8,
    ) -> Result<Vec<DecodedEvent>, DexAdapterError>;
}

// ---------------------------------------------------------------------------
// SolanaDexDecoder — concrete stateless implementation
// ---------------------------------------------------------------------------

/// Concrete `DexAdapter` implementation for Solana.
///
/// Dispatches to program-specific decoders via [`solana::decode_solana`].
/// Stateless — safe to clone, share across threads, use from `Arc<dyn DexAdapter>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SolanaDexDecoder;

impl DexAdapter for SolanaDexDecoder {
    #[allow(clippy::too_many_arguments)]
    fn decode(
        &self,
        program_id: &str,
        ix_data: &[u8],
        accounts: &[String],
        tx_hash: &TxHash,
        block: BlockRef,
        block_time: DateTime<Utc>,
        log_index: u32,
        decimals_in: u8,
        decimals_out: u8,
    ) -> Result<Vec<DecodedEvent>, DexAdapterError> {
        match solana::decode_solana(
            program_id,
            ix_data,
            accounts,
            tx_hash,
            block,
            block_time,
            log_index,
            decimals_in,
            decimals_out,
        )? {
            Some(event) => Ok(vec![event]),
            None => Ok(vec![]),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mg_onchain_common::chain::{BlockRef, Chain, TxHash};
    use mg_onchain_common::event::DexKind;

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[5u8; 64]).into_string()).unwrap()
    }

    fn dummy_block() -> BlockRef {
        BlockRef::new(Chain::Solana, 310_000_000)
    }

    /// Build a minimal valid Raydium AMM v4 SwapBaseIn instruction with 18 accounts.
    fn v4_swap_base_in(amount_in: u64, min_out: u64) -> (Vec<u8>, Vec<String>) {
        let mut data = vec![9u8]; // DISC_SWAP_BASE_IN
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        let filler = bs58::encode(&[0xAA_u8; 32]).into_string();
        let mut accounts: Vec<String> = vec![filler; 18];
        accounts[1] = bs58::encode(&[0x10_u8; 32]).into_string(); // amm pool
        accounts[15] = bs58::encode(&[0x11_u8; 32]).into_string(); // user_source
        accounts[16] = bs58::encode(&[0x12_u8; 32]).into_string(); // user_dest
        accounts[17] = bs58::encode(&[0x13_u8; 32]).into_string(); // wallet
        (data, accounts)
    }

    #[test]
    fn solana_decoder_returns_swap_for_v4() {
        let (data, accounts) = v4_swap_base_in(1_000_000, 500_000);
        let decoder = SolanaDexDecoder;
        let events = decoder
            .decode(
                RAYDIUM_V4_PROGRAM_ID,
                &data,
                &accounts,
                &dummy_tx(),
                dummy_block(),
                Utc::now(),
                0,
                9,
                6,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            DecodedEvent::Swap(s) => {
                assert_eq!(s.dex, DexKind::RaydiumV4);
                assert_eq!(s.amount_in_raw, 1_000_000u128);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn solana_decoder_returns_empty_for_unknown_program() {
        let decoder = SolanaDexDecoder;
        let events = decoder
            .decode(
                "UnknownProgram111111111111111111111111111",
                &[9u8; 17],
                &[],
                &dummy_tx(),
                dummy_block(),
                Utc::now(),
                0,
                9,
                6,
            )
            .unwrap();
        assert!(events.is_empty(), "unknown program must yield no events");
    }

    #[test]
    fn decoded_event_serde_roundtrip_swap() {
        let (data, accounts) = v4_swap_base_in(999_888_777, 111_222_333);
        let decoder = SolanaDexDecoder;
        let ts = chrono::DateTime::parse_from_rfc3339("2026-04-21T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let events = decoder
            .decode(
                RAYDIUM_V4_PROGRAM_ID,
                &data,
                &accounts,
                &dummy_tx(),
                dummy_block(),
                ts,
                0,
                9,
                6,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        // Must serialize amount as string (not float) per CLAUDE.md §Code Style
        assert!(json.contains("\"999888777\""), "amount_in_raw must be string-encoded");
        // Round-trip
        let back: DecodedEvent = serde_json::from_str(&json).unwrap();
        match back {
            DecodedEvent::Swap(s) => assert_eq!(s.amount_in_raw, 999_888_777u128),
            other => panic!("expected Swap after round-trip, got {other:?}"),
        }
    }
}
