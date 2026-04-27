//! Solana DEX adapter — dispatches to program-specific decoders by program ID.
//!
//! # Supported programs (Sprint 2)
//!
//! | Program | ID | Module |
//! |---------|----|--------|
//! | Raydium AMM v4 | `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8` | [`raydium_v4`] |
//! | Raydium CPMM   | `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` | [`raydium_cpmm`] |
//!
//! # Deferred (Sprint 3+)
//!
//! TODO(sprint-3): Raydium CLMM tick math
//! TODO(sprint-3): Orca Whirlpool (tick-based, similar to Uniswap v3)
//! TODO(sprint-3): Meteora DLMM
//! TODO(sprint-3): Jupiter aggregator (routes through multiple pools per tx — stamps the tx, does not own pool layout)
//! TODO(sprint-3): Token-2022 transfer-hook decoding (FLAG: TOKEN_2022_FEE_RECONCILIATION)

pub mod common;
pub mod openbook_market;
pub mod pool_accounts;
pub mod raydium_cpmm;
pub mod raydium_v4;
pub mod raydium_v4_state;
pub mod simulation;

use chrono::{DateTime, Utc};

use mg_onchain_common::chain::{BlockRef, TxHash};

use crate::error::DexAdapterError;
use crate::DecodedEvent;

pub use raydium_cpmm::RAYDIUM_CPMM_PROGRAM_ID;
pub use raydium_v4::RAYDIUM_V4_PROGRAM_ID;

// ---------------------------------------------------------------------------
// Public dispatch function
// ---------------------------------------------------------------------------

/// Decode a single Solana DEX instruction by dispatching to the correct
/// program-specific decoder.
///
/// # Arguments
///
/// - `program_id`: Base58 program ID string.
/// - `ix_data`: Raw instruction data bytes (including discriminator).
/// - `accounts`: Ordered account address strings (Base58) for this instruction.
/// - `tx_hash`, `block`, `block_time`: Transaction context from the chain adapter.
/// - `log_index`: Instruction position within the transaction (for dedup).
/// - `decimals_in` / `decimals_out`: Token decimal exponents. Use `0` as sentinel
///   when not yet known; `token-registry` enriches post-emission.
///
/// # Returns
///
/// - `Ok(Some(event))` — decoded `Swap` or `PoolEvent`.
/// - `Ok(None)` — program not recognized, OR instruction is a known non-event
///   type for a recognized program (admin, config, etc.).
/// - `Err(DexAdapterError)` — malformed instruction data (truncated, invalid accounts).
// Decoder API requires many arguments by design — see raydium_v4.rs for rationale.
#[allow(clippy::too_many_arguments)]
pub fn decode_solana(
    program_id: &str,
    ix_data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
    decimals_in: u8,
    decimals_out: u8,
) -> Result<Option<DecodedEvent>, DexAdapterError> {
    match program_id {
        RAYDIUM_V4_PROGRAM_ID => raydium_v4::decode(
            program_id,
            ix_data,
            accounts,
            tx_hash,
            block,
            block_time,
            log_index,
            decimals_in,
            decimals_out,
        ),
        RAYDIUM_CPMM_PROGRAM_ID => raydium_cpmm::decode(
            program_id,
            ix_data,
            accounts,
            tx_hash,
            block,
            block_time,
            log_index,
            decimals_in,
            decimals_out,
        ),
        _ => {
            // Unknown program — return None so the caller can fall back to the
            // swap heuristic or simply skip.
            tracing::trace!(program_id, "decode_solana: unrecognised DEX program, skipping");
            Ok(None)
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

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[3u8; 64]).into_string()).unwrap()
    }

    fn dummy_block() -> BlockRef {
        BlockRef::new(Chain::Solana, 300_000_000)
    }

    #[test]
    fn dispatch_unknown_program_returns_none() {
        let result = decode_solana(
            "SomeCompletelyUnknownProgram11111111111111",
            &[0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
            &[],
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            0,
            9,
            6,
        )
        .unwrap();
        assert!(result.is_none(), "unknown program must return None");
    }

    #[test]
    fn dispatch_raydium_v4_routes_correctly() {
        // Build a valid SwapBaseIn instruction and verify it dispatches to v4 decoder
        let amount_in: u64 = 1_000_000;
        let min_out: u64 = 500_000;
        let mut data = vec![9u8]; // DISC_SWAP_BASE_IN
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());

        let filler = bs58::encode(&[0xAA_u8; 32]).into_string();
        let mut accounts: Vec<String> = vec![filler.clone(); 18];
        // Set pool (index 1) and wallet (index 17) to distinguishable values
        accounts[1] = bs58::encode(&[0x10_u8; 32]).into_string();
        accounts[17] = bs58::encode(&[0x11_u8; 32]).into_string();

        let result = decode_solana(
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

        assert!(result.is_some(), "Raydium v4 SwapBaseIn must produce an event");
        match result.unwrap() {
            DecodedEvent::Swap(s) => {
                assert_eq!(s.dex, mg_onchain_common::event::DexKind::RaydiumV4);
                assert_eq!(s.amount_in_raw, 1_000_000u128);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_raydium_cpmm_routes_correctly() {
        // Minimal valid CPMM swap_base_input instruction
        let mut data = vec![0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde]; // DISC_SWAP_BASE_INPUT
        data.extend_from_slice(&2_000_000u64.to_le_bytes());  // amount_in
        data.extend_from_slice(&1_000_000u64.to_le_bytes());  // min_out

        let filler = bs58::encode(&[0xBB_u8; 32]).into_string();
        let mut accounts: Vec<String> = vec![filler; 13];
        accounts[0] = bs58::encode(&[0x20_u8; 32]).into_string(); // payer
        accounts[3] = bs58::encode(&[0x21_u8; 32]).into_string(); // pool_state
        accounts[10] = bs58::encode(&[0x22_u8; 32]).into_string(); // input_mint
        accounts[11] = bs58::encode(&[0x23_u8; 32]).into_string(); // output_mint

        let result = decode_solana(
            RAYDIUM_CPMM_PROGRAM_ID,
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

        assert!(result.is_some(), "CPMM swap must produce an event");
        match result.unwrap() {
            DecodedEvent::Swap(s) => {
                assert_eq!(s.dex, mg_onchain_common::event::DexKind::RaydiumCpmm);
                assert_eq!(s.amount_in_raw, 2_000_000u128);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }
}
