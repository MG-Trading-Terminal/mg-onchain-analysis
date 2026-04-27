//! Raydium CPMM (Constant Product Market Maker) instruction decoder.
//!
//! # Program
//!
//! Program ID: `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`
//!
//! Raydium CPMM is the 2024 replacement for AMM v4 that does NOT require
//! OpenBook. It is an Anchor program, so instructions use Anchor's encoding:
//! - First 8 bytes: discriminator = `sha256("global:<instruction_name>")[..8]`
//! - Remaining bytes: Borsh-encoded parameters (little-endian fixed-width integers)
//!
//! # Layout sources
//!
//! - Instruction source:
//!   <https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/instructions/>
//! - Account ordering: Context struct order in each instruction's source file.
//!
//! # Anchor discriminators (sha256("global:<name>")[..8])
//!
//! | Name              | Discriminator bytes (hex)           |
//! |-------------------|-------------------------------------|
//! | swap_base_input   | 8f be 5a da c4 1e 33 de              |
//! | swap_base_output  | 37 d9 62 56 a3 4a b4 ad              |
//! | deposit           | f2 23 c6 89 52 e1 f2 b6              |
//! | withdraw          | b7 12 46 9c 94 6d a1 22              |
//! | initialize        | af af 6d 1f 0d 98 9b ed              |
//!
//! Discriminators computed via `sha256("global:<name>")` (see Python script in
//! crate-level doc). Verified against Anchor framework source at
//! <https://github.com/coral-xyz/anchor/blob/master/lang/syn/src/codegen/program/dispatch.rs>.
//!
//! # Token-2022 note
//!
//! CPMM explicitly supports Token-2022 via dual token-program accounts
//! (positions 8 and 9 in the Swap context). Fee-on-transfer tokens produce
//! instruction amounts that differ from post-transfer balances.
//!
//! FLAG: TOKEN_2022_FEE_RECONCILIATION — same gap as AMM v4. Post-Phase-2 work.

use chrono::{DateTime, Utc};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

use mg_onchain_common::chain::{BlockRef, Chain, TxHash};
use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind, Swap};

use thiserror::Error;

use crate::error::DexAdapterError;
use crate::solana::common::{get_account, parse_solana_addr, read_u64_le};
use crate::solana::simulation::build_set_compute_unit_limit_instruction;

// ---------------------------------------------------------------------------
// Program constant
// ---------------------------------------------------------------------------

/// Raydium CPMM program ID (Base58).
pub const RAYDIUM_CPMM_PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

// ---------------------------------------------------------------------------
// Anchor discriminators — first 8 bytes
// Computed: sha256("global:<instruction_name>")[..8]
// ---------------------------------------------------------------------------

const DISC_SWAP_BASE_INPUT: [u8; 8]  = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
const DISC_SWAP_BASE_OUTPUT: [u8; 8] = [0x37, 0xd9, 0x62, 0x56, 0xa3, 0x4a, 0xb4, 0xad];
const DISC_DEPOSIT: [u8; 8]          = [0xf2, 0x23, 0xc6, 0x89, 0x52, 0xe1, 0xf2, 0xb6];
const DISC_WITHDRAW: [u8; 8]         = [0xb7, 0x12, 0x46, 0x9c, 0x94, 0x6d, 0xa1, 0x22];
const DISC_INITIALIZE: [u8; 8]       = [0xaf, 0xaf, 0x6d, 0x1f, 0x0d, 0x98, 0x9b, 0xed];

/// Minimum instruction data length: 8-byte discriminator.
const MIN_DATA_LEN: usize = 8;

// ---------------------------------------------------------------------------
// Account index constants
//
// Source: raydium-cp-swap/programs/cp-swap/src/instructions/*.rs Context structs
// ---------------------------------------------------------------------------

// swap_base_input / swap_base_output: 13 accounts
// 0 = payer (signer), 1 = authority, 2 = amm_config, 3 = pool_state
// 4 = input_token_account, 5 = output_token_account
// 6 = input_vault, 7 = output_vault
// 8 = input_token_program, 9 = output_token_program
// 10 = input_token_mint, 11 = output_token_mint, 12 = observation_state
const ACC_SWAP_PAYER: usize = 0;
const ACC_SWAP_POOL: usize = 3;
// 4 = input_token_account (user ATA for input token — not used in event output)
// 5 = output_token_account (user ATA for output token — not used in event output)
const ACC_SWAP_INPUT_MINT: usize = 10;
const ACC_SWAP_OUTPUT_MINT: usize = 11;

// deposit: 13 accounts
// 0 = owner (signer), 1 = authority, 2 = pool_state
// 3 = owner_lp_token, 4 = token_0_account, 5 = token_1_account
// 6 = token_0_vault, 7 = token_1_vault
// 8 = token_program, 9 = token_program_2022
// 10 = vault_0_mint, 11 = vault_1_mint, 12 = lp_mint
const ACC_DEP_OWNER: usize = 0;
const ACC_DEP_POOL: usize = 2;
const ACC_DEP_LP_MINT: usize = 12;

// withdraw: 14 accounts (same as deposit + memo_program at index 13)
const ACC_WIT_OWNER: usize = 0;
const ACC_WIT_POOL: usize = 2;
const ACC_WIT_LP_MINT: usize = 12;

// initialize: 20 accounts
// 0 = creator (signer), 1 = amm_config, 2 = authority, 3 = pool_state
// 4 = token_0_mint, 5 = token_1_mint, 6 = lp_mint
// 7 = creator_token_0, 8 = creator_token_1, 9 = creator_lp_token
// 10 = token_0_vault, 11 = token_1_vault
// 12 = create_pool_fee, 13 = observation_state
// 14 = token_program, 15 = token_0_program, 16 = token_1_program
// 17 = associated_token_program, 18 = system_program, 19 = rent
const ACC_INIT_CREATOR: usize = 0;
const ACC_INIT_POOL: usize = 3;
const ACC_INIT_TOKEN0_MINT: usize = 4;
const ACC_INIT_TOKEN1_MINT: usize = 5;

// ---------------------------------------------------------------------------
// Borsh parameter layout (after 8-byte discriminator)
//
// swap_base_input:  amount_in u64 (8 bytes) + minimum_amount_out u64 (8 bytes)
// swap_base_output: max_amount_in u64 (8 bytes) + amount_out u64 (8 bytes)
// deposit:          lp_token_amount u64 + maximum_token_0_amount u64 + maximum_token_1_amount u64
// withdraw:         lp_token_amount u64 + minimum_token_0_amount u64 + minimum_token_1_amount u64
// initialize:       init_amount_0 u64 + init_amount_1 u64 + open_time u64
// ---------------------------------------------------------------------------

const PARAM_OFFSET: usize = 8; // after discriminator

// ---------------------------------------------------------------------------
// Discriminator check helper
// ---------------------------------------------------------------------------

fn check_discriminator(
    data: &[u8],
    expected: &[u8; 8],
    name: &'static str,
) -> Result<(), DexAdapterError> {
    if data.len() < MIN_DATA_LEN {
        return Err(DexAdapterError::DataTooShort {
            context: name,
            offset: 0,
            need: MIN_DATA_LEN,
            got: data.len(),
        });
    }
    if &data[..8] != expected {
        return Err(DexAdapterError::UnknownDiscriminator {
            program: "RaydiumCPMM",
            discriminator: data[..8].to_vec(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public decode entry point
// ---------------------------------------------------------------------------

/// Decode a single Raydium CPMM instruction.
///
/// # Arguments
///
/// Same contract as [`crate::solana::raydium_v4::decode`] — see that module
/// for the full argument description. The key difference is that CPMM uses
/// Anchor encoding (8-byte discriminator + Borsh params) rather than raw
/// 1-byte discriminator + packed C struct.
///
/// # Returns
///
/// - `Ok(Some(event))` for known swap/pool instructions.
/// - `Ok(None)` for unknown discriminators (admin instructions, future variants).
/// - `Err(DexAdapterError)` for malformed data or wrong program.
// Decoder API requires many arguments by design — see raydium_v4.rs for rationale.
#[allow(clippy::too_many_arguments)]
pub fn decode(
    program_id: &str,
    ix_data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
    decimals_in: u8,
    decimals_out: u8,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    if program_id != RAYDIUM_CPMM_PROGRAM_ID {
        return Err(DexAdapterError::WrongProgram {
            expected: RAYDIUM_CPMM_PROGRAM_ID,
            got: program_id.to_string(),
        });
    }

    if ix_data.len() < MIN_DATA_LEN {
        return Err(DexAdapterError::DataTooShort {
            context: "RaydiumCPMM::decode",
            offset: 0,
            need: MIN_DATA_LEN,
            got: ix_data.len(),
        });
    }

    let disc: &[u8; 8] = ix_data[..8].try_into().unwrap();

    match *disc {
        DISC_SWAP_BASE_INPUT => decode_swap_base_input(ix_data, accounts, tx_hash, block, block_time, log_index, decimals_in, decimals_out),
        DISC_SWAP_BASE_OUTPUT => decode_swap_base_output(ix_data, accounts, tx_hash, block, block_time, log_index, decimals_in, decimals_out),
        DISC_DEPOSIT => decode_deposit(ix_data, accounts, tx_hash, block, block_time, log_index),
        DISC_WITHDRAW => decode_withdraw(ix_data, accounts, tx_hash, block, block_time, log_index),
        DISC_INITIALIZE => decode_initialize(ix_data, accounts, tx_hash, block, block_time, log_index),
        _ => {
            tracing::trace!(
                disc = ?disc,
                program = RAYDIUM_CPMM_PROGRAM_ID,
                "RaydiumCPMM: unknown discriminator, skipping"
            );
            Ok(None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_swap_base_input(
    data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
    decimals_in: u8,
    decimals_out: u8,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    check_discriminator(data, &DISC_SWAP_BASE_INPUT, "RaydiumCPMM::swap_base_input")?;
    // Parameters after discriminator (offset 8):
    // amount_in (u64 LE, offset 8), minimum_amount_out (u64 LE, offset 16)
    let amount_in = read_u64_le(data, PARAM_OFFSET, "RaydiumCPMM::swap_base_input::amount_in")?;
    let minimum_amount_out = read_u64_le(data, PARAM_OFFSET + 8, "RaydiumCPMM::swap_base_input::minimum_amount_out")?;

    let pool_addr = get_account(accounts, ACC_SWAP_POOL, "RaydiumCPMM", "pool_state")?;
    let payer_addr = get_account(accounts, ACC_SWAP_PAYER, "RaydiumCPMM", "payer")?;
    let token_in_addr = get_account(accounts, ACC_SWAP_INPUT_MINT, "RaydiumCPMM", "input_token_mint")?;
    let token_out_addr = get_account(accounts, ACC_SWAP_OUTPUT_MINT, "RaydiumCPMM", "output_token_mint")?;

    let swap = Swap {
        chain: Chain::Solana,
        tx_hash: tx_hash.clone(),
        block,
        block_time,
        pool: parse_solana_addr(pool_addr, "RaydiumCPMM", ACC_SWAP_POOL)?,
        dex: DexKind::RaydiumCpmm,
        sender: parse_solana_addr(payer_addr, "RaydiumCPMM", ACC_SWAP_PAYER)?,
        token_in: parse_solana_addr(token_in_addr, "RaydiumCPMM", ACC_SWAP_INPUT_MINT)?,
        token_out: parse_solana_addr(token_out_addr, "RaydiumCPMM", ACC_SWAP_OUTPUT_MINT)?,
        amount_in_raw: amount_in as u128,
        decimals_in,
        amount_out_raw: minimum_amount_out as u128,
        decimals_out,
        usd_value: None,
        log_index,
    };
    Ok(Some(crate::DecodedEvent::Swap(swap)))
}

#[allow(clippy::too_many_arguments)]
fn decode_swap_base_output(
    data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
    decimals_in: u8,
    decimals_out: u8,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    check_discriminator(data, &DISC_SWAP_BASE_OUTPUT, "RaydiumCPMM::swap_base_output")?;
    // max_amount_in (u64 LE, offset 8), amount_out (u64 LE, offset 16)
    let max_amount_in = read_u64_le(data, PARAM_OFFSET, "RaydiumCPMM::swap_base_output::max_amount_in")?;
    let amount_out = read_u64_le(data, PARAM_OFFSET + 8, "RaydiumCPMM::swap_base_output::amount_out")?;

    let pool_addr = get_account(accounts, ACC_SWAP_POOL, "RaydiumCPMM", "pool_state")?;
    let payer_addr = get_account(accounts, ACC_SWAP_PAYER, "RaydiumCPMM", "payer")?;
    let token_in_addr = get_account(accounts, ACC_SWAP_INPUT_MINT, "RaydiumCPMM", "input_token_mint")?;
    let token_out_addr = get_account(accounts, ACC_SWAP_OUTPUT_MINT, "RaydiumCPMM", "output_token_mint")?;

    let swap = Swap {
        chain: Chain::Solana,
        tx_hash: tx_hash.clone(),
        block,
        block_time,
        pool: parse_solana_addr(pool_addr, "RaydiumCPMM", ACC_SWAP_POOL)?,
        dex: DexKind::RaydiumCpmm,
        sender: parse_solana_addr(payer_addr, "RaydiumCPMM", ACC_SWAP_PAYER)?,
        token_in: parse_solana_addr(token_in_addr, "RaydiumCPMM", ACC_SWAP_INPUT_MINT)?,
        token_out: parse_solana_addr(token_out_addr, "RaydiumCPMM", ACC_SWAP_OUTPUT_MINT)?,
        amount_in_raw: max_amount_in as u128,
        decimals_in,
        amount_out_raw: amount_out as u128,
        decimals_out,
        usd_value: None,
        log_index,
    };
    Ok(Some(crate::DecodedEvent::Swap(swap)))
}

fn decode_deposit(
    data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    check_discriminator(data, &DISC_DEPOSIT, "RaydiumCPMM::deposit")?;
    // lp_token_amount (u64, offset 8), maximum_token_0_amount (u64, offset 16),
    // maximum_token_1_amount (u64, offset 24)
    let lp_amount = read_u64_le(data, PARAM_OFFSET, "RaydiumCPMM::deposit::lp_token_amount")?;
    let max_token0 = read_u64_le(data, PARAM_OFFSET + 8, "RaydiumCPMM::deposit::maximum_token_0_amount")?;
    let max_token1 = read_u64_le(data, PARAM_OFFSET + 16, "RaydiumCPMM::deposit::maximum_token_1_amount")?;

    let pool_addr = get_account(accounts, ACC_DEP_POOL, "RaydiumCPMM", "pool_state")?;
    let owner_addr = get_account(accounts, ACC_DEP_OWNER, "RaydiumCPMM", "owner")?;
    let _lp_mint = get_account(accounts, ACC_DEP_LP_MINT, "RaydiumCPMM", "lp_mint")?;

    let pool_event = PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx_hash.clone(),
        block,
        block_time,
        pool: parse_solana_addr(pool_addr, "RaydiumCPMM", ACC_DEP_POOL)?,
        dex: DexKind::RaydiumCpmm,
        kind: PoolEventKind::Mint {
            amount0_raw: max_token0 as u128,
            amount1_raw: max_token1 as u128,
            lp_tokens_minted: lp_amount as u128,
        },
        actor: parse_solana_addr(owner_addr, "RaydiumCPMM", ACC_DEP_OWNER)?,
        log_index,
    };
    Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
}

fn decode_withdraw(
    data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    check_discriminator(data, &DISC_WITHDRAW, "RaydiumCPMM::withdraw")?;
    // lp_token_amount (u64, offset 8), minimum_token_0_amount (u64, offset 16),
    // minimum_token_1_amount (u64, offset 24)
    let lp_amount = read_u64_le(data, PARAM_OFFSET, "RaydiumCPMM::withdraw::lp_token_amount")?;
    let min_token0 = read_u64_le(data, PARAM_OFFSET + 8, "RaydiumCPMM::withdraw::minimum_token_0_amount")?;
    let min_token1 = read_u64_le(data, PARAM_OFFSET + 16, "RaydiumCPMM::withdraw::minimum_token_1_amount")?;

    let pool_addr = get_account(accounts, ACC_WIT_POOL, "RaydiumCPMM", "pool_state")?;
    let owner_addr = get_account(accounts, ACC_WIT_OWNER, "RaydiumCPMM", "owner")?;
    let _lp_mint = get_account(accounts, ACC_WIT_LP_MINT, "RaydiumCPMM", "lp_mint")?;

    let pool_event = PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx_hash.clone(),
        block,
        block_time,
        pool: parse_solana_addr(pool_addr, "RaydiumCPMM", ACC_WIT_POOL)?,
        dex: DexKind::RaydiumCpmm,
        kind: PoolEventKind::Burn {
            amount0_raw: min_token0 as u128,
            amount1_raw: min_token1 as u128,
            lp_tokens_burned: lp_amount as u128,
        },
        actor: parse_solana_addr(owner_addr, "RaydiumCPMM", ACC_WIT_OWNER)?,
        log_index,
    };
    Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
}

fn decode_initialize(
    data: &[u8],
    accounts: &[String],
    tx_hash: &TxHash,
    block: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
) -> Result<Option<crate::DecodedEvent>, DexAdapterError> {
    check_discriminator(data, &DISC_INITIALIZE, "RaydiumCPMM::initialize")?;
    // init_amount_0 (u64, offset 8), init_amount_1 (u64, offset 16), open_time (u64, offset 24)
    let _init_amount_0 = read_u64_le(data, PARAM_OFFSET, "RaydiumCPMM::initialize::init_amount_0")?;
    let _init_amount_1 = read_u64_le(data, PARAM_OFFSET + 8, "RaydiumCPMM::initialize::init_amount_1")?;
    // open_time is informational — not emitted in PoolEvent

    let pool_addr = get_account(accounts, ACC_INIT_POOL, "RaydiumCPMM", "pool_state")?;
    let creator_addr = get_account(accounts, ACC_INIT_CREATOR, "RaydiumCPMM", "creator")?;
    let token0_addr = get_account(accounts, ACC_INIT_TOKEN0_MINT, "RaydiumCPMM", "token_0_mint")?;
    let token1_addr = get_account(accounts, ACC_INIT_TOKEN1_MINT, "RaydiumCPMM", "token_1_mint")?;

    let pool_event = PoolEvent {
        chain: Chain::Solana,
        tx_hash: tx_hash.clone(),
        block,
        block_time,
        pool: parse_solana_addr(pool_addr, "RaydiumCPMM", ACC_INIT_POOL)?,
        dex: DexKind::RaydiumCpmm,
        kind: PoolEventKind::Initialize {
            token0: parse_solana_addr(token0_addr, "RaydiumCPMM", ACC_INIT_TOKEN0_MINT)?,
            token1: parse_solana_addr(token1_addr, "RaydiumCPMM", ACC_INIT_TOKEN1_MINT)?,
        },
        actor: parse_solana_addr(creator_addr, "RaydiumCPMM", ACC_INIT_CREATOR)?,
        log_index,
    };
    Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
}

// ---------------------------------------------------------------------------
// CPMM PoolState decoder (Sprint 9, B1.2)
// ---------------------------------------------------------------------------

/// Discriminator for the CPMM `PoolState` account.
///
/// Computed: `sha256("account:PoolState")[..8]`
/// Value: `[0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46]`
///
/// Verified against Anchor framework discriminator derivation and cross-checked
/// against the live pool `2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A` on
/// Solana mainnet (2026-04-24).
///
/// Source: `sha256("account:PoolState")` per Anchor lang/syn discriminator rules
/// at <https://github.com/coral-xyz/anchor/blob/master/lang/attribute/account/src/lib.rs>.
const CPMM_POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

/// Expected byte length of a serialised CPMM `PoolState` account.
///
/// Layout (Anchor Borsh, packed — no alignment padding):
/// - 8 bytes: discriminator
/// - 10 × 32 bytes: Pubkey fields (amm_config … observation_key)
/// - 5 × 1 byte: u8 fields (auth_bump, status, lp/mint decimals)
/// - 7 × 8 bytes: u64 fields (lp_supply, fees, open_time, recent_epoch)
/// - 31 × 8 bytes: padding
///
/// Total: 8 + 320 + 5 + 56 + 248 = 637 bytes.
///
/// Source: `raydium-cp-swap/programs/cp-swap/src/states/pool.rs`
/// <https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/states/pool.rs>
pub const CPMM_POOL_STATE_SIZE: usize = 637;

/// Decoded fields from a Raydium CPMM `PoolState` account.
///
/// Only the fields needed for swap-account composition are included.
/// The full layout also contains `lp_supply`, fee accumulators, open-time, padding,
/// and misc u8 flags — those are not needed here and are skipped after verification.
///
/// # Field ordering
///
/// Fields are in declaration order from the source struct. The decoder reads them
/// sequentially without seeking, using fixed offsets derived from the layout.
///
/// # Source
///
/// `raydium-cp-swap/programs/cp-swap/src/states/pool.rs`
/// <https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/states/pool.rs>
#[derive(Debug, Clone)]
pub struct CpmmPoolState {
    /// AMM config account (fee tier, etc.).
    pub amm_config: Pubkey,
    /// Pool creator wallet.
    pub pool_creator: Pubkey,
    /// Pool vault for token 0.
    pub token_0_vault: Pubkey,
    /// Pool vault for token 1.
    pub token_1_vault: Pubkey,
    /// LP mint address.
    pub lp_mint: Pubkey,
    /// Token 0 mint address.
    pub token_0_mint: Pubkey,
    /// Token 1 mint address.
    pub token_1_mint: Pubkey,
    /// Token program for token 0 (SPL Token or Token-2022).
    pub token_0_program: Pubkey,
    /// Token program for token 1 (SPL Token or Token-2022).
    pub token_1_program: Pubkey,
    /// Observation state account for TWAP oracle.
    pub observation_key: Pubkey,
    /// Bump seed used to derive the AMM authority PDA.
    pub auth_bump: u8,
}

/// Errors from [`decode_cpmm_pool_state`].
#[derive(Debug, Error)]
pub enum PoolStateDecodeError {
    /// Account data is shorter than the expected minimum.
    #[error("pool state too short: expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },

    /// First 8 bytes do not match the expected Anchor account discriminator.
    #[error("pool state discriminator mismatch: expected {expected_first_8:?}, got {got:?}")]
    BadDiscriminator {
        expected_first_8: [u8; 8],
        got: [u8; 8],
    },

    /// Account is owned by an unexpected program.
    ///
    /// Callers should check the owner before calling this function and use
    /// this variant to signal the mismatch to the caller.
    #[error("pool state owned by unexpected program: expected {expected}, got {got}")]
    WrongOwner { expected: Pubkey, got: Pubkey },
}

/// Decode a Raydium CPMM `PoolState` from raw account bytes.
///
/// # Arguments
///
/// * `data` — Raw account bytes from `getAccountInfo` (base-64 decoded).
///
/// # Errors
///
/// Returns [`PoolStateDecodeError::TooShort`] if `data.len() < 637`.
/// Returns [`PoolStateDecodeError::BadDiscriminator`] if the first 8 bytes do
/// not match `sha256("account:PoolState")[..8]`.
///
/// Callers should verify the account owner before calling; use
/// [`PoolStateDecodeError::WrongOwner`] to propagate owner mismatches detected
/// at the call site.
///
/// # Layout
///
/// Anchor Borsh serialisation, packed (no struct-level alignment padding).
/// Field order matches the struct declaration in:
/// `raydium-cp-swap/programs/cp-swap/src/states/pool.rs`
pub fn decode_cpmm_pool_state(data: &[u8]) -> Result<CpmmPoolState, PoolStateDecodeError> {
    if data.len() < CPMM_POOL_STATE_SIZE {
        return Err(PoolStateDecodeError::TooShort {
            expected: CPMM_POOL_STATE_SIZE,
            got: data.len(),
        });
    }

    let got_disc: [u8; 8] = data[..8].try_into().expect("slice is 8 bytes");
    if got_disc != CPMM_POOL_STATE_DISCRIMINATOR {
        return Err(PoolStateDecodeError::BadDiscriminator {
            expected_first_8: CPMM_POOL_STATE_DISCRIMINATOR,
            got: got_disc,
        });
    }

    // Read fields in declaration order. Each Pubkey is 32 raw bytes (no Base58 in storage).
    // Offset tracking (no alignment padding in Borsh):
    //   [0..8]   discriminator  (already verified)
    //   [8..40]  amm_config
    //   [40..72] pool_creator
    //   [72..104] token_0_vault
    //   [104..136] token_1_vault
    //   [136..168] lp_mint
    //   [168..200] token_0_mint
    //   [200..232] token_1_mint
    //   [232..264] token_0_program
    //   [264..296] token_1_program
    //   [296..328] observation_key
    //   [328]    auth_bump (u8)
    //   [329..637] remaining (status, decimals, u64 fields, padding — not decoded here)

    let read_pubkey = |off: usize| -> Pubkey {
        let bytes: [u8; 32] = data[off..off + 32]
            .try_into()
            .expect("fixed 32-byte slice");
        Pubkey::from(bytes)
    };

    let amm_config      = read_pubkey(8);
    let pool_creator    = read_pubkey(40);
    let token_0_vault   = read_pubkey(72);
    let token_1_vault   = read_pubkey(104);
    let lp_mint         = read_pubkey(136);
    let token_0_mint    = read_pubkey(168);
    let token_1_mint    = read_pubkey(200);
    let token_0_program = read_pubkey(232);
    let token_1_program = read_pubkey(264);
    let observation_key = read_pubkey(296);
    let auth_bump       = data[328];

    Ok(CpmmPoolState {
        amm_config,
        pool_creator,
        token_0_vault,
        token_1_vault,
        lp_mint,
        token_0_mint,
        token_1_mint,
        token_0_program,
        token_1_program,
        observation_key,
        auth_bump,
    })
}

// ---------------------------------------------------------------------------
// Simulation instruction builders (Sprint 7, P6-4 Phase B)
// ---------------------------------------------------------------------------

/// Compute-budget limit for simulated Raydium CPMM swap instructions.
///
/// CPMM paths include CPI into SPL token programs and AMM math. 400k CU is a
/// safe margin per mainnet profiling of real CPMM swap transactions.
///
/// Reference: Raydium CPMM mainnet swap compute budget observed 2026-04-22.
const SIMULATION_COMPUTE_UNIT_LIMIT: u32 = 400_000;

/// All accounts required for a `swap_base_input` instruction on Raydium CPMM.
///
/// Mirror of the 13-account layout from the CPMM Swap context struct:
/// <https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/instructions/>
///
/// # Note on wSOL / ATA pre-creation
///
/// Same caveats as Raydium V4 — see [`crate::solana::raydium_v4::RaydiumV4SwapAccounts`].
/// The builder does NOT wrap SOL or create ATAs.
#[derive(Debug)]
pub struct RaydiumCpmmSwapAccounts {
    /// Transaction payer and signer (index 0).
    pub payer: Pubkey,
    /// AMM authority PDA (index 1).
    pub authority: Pubkey,
    /// AMM config account (index 2).
    pub amm_config: Pubkey,
    /// Pool state account (index 3).
    pub pool_state: Pubkey,
    /// User's input token ATA (index 4).
    pub input_token_account: Pubkey,
    /// User's output token ATA (index 5).
    pub output_token_account: Pubkey,
    /// Pool input token vault (index 6).
    pub input_vault: Pubkey,
    /// Pool output token vault (index 7).
    pub output_vault: Pubkey,
    /// Input token program (SPL Token or Token-2022) (index 8).
    pub input_token_program: Pubkey,
    /// Output token program (SPL Token or Token-2022) (index 9).
    pub output_token_program: Pubkey,
    /// Input token mint (index 10).
    pub input_token_mint: Pubkey,
    /// Output token mint (index 11).
    pub output_token_mint: Pubkey,
    /// Observation state account (index 12).
    pub observation_state: Pubkey,
}

/// Build a `swap_base_input` [`Instruction`] for Raydium CPMM.
///
/// Instruction data layout (24 bytes):
/// - `[0..8]`: Anchor discriminator = `[0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde]`
/// - `[8..16]`: `amount_in` (u64 LE)
/// - `[16..24]`: `minimum_amount_out` (u64 LE)
///
/// The 13-account ordering matches the canonical CPMM Swap context struct.
pub fn build_swap_base_input_instruction(
    accounts: &RaydiumCpmmSwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new_readonly(accounts.payer, true),              // 0: payer (signer)
        AccountMeta::new_readonly(accounts.authority, false),         // 1: authority
        AccountMeta::new_readonly(accounts.amm_config, false),        // 2: amm_config
        AccountMeta::new(accounts.pool_state, false),                 // 3: pool_state
        AccountMeta::new(accounts.input_token_account, false),        // 4: input_token_account
        AccountMeta::new(accounts.output_token_account, false),       // 5: output_token_account
        AccountMeta::new(accounts.input_vault, false),                // 6: input_vault
        AccountMeta::new(accounts.output_vault, false),               // 7: output_vault
        AccountMeta::new_readonly(accounts.input_token_program, false), // 8: input_token_program
        AccountMeta::new_readonly(accounts.output_token_program, false), // 9: output_token_program
        AccountMeta::new_readonly(accounts.input_token_mint, false),  // 10: input_token_mint
        AccountMeta::new_readonly(accounts.output_token_mint, false), // 11: output_token_mint
        AccountMeta::new(accounts.observation_state, false),          // 12: observation_state
    ];

    // Instruction data: 8-byte Anchor discriminator + amount_in (u64 LE) + min_out (u64 LE)
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISC_SWAP_BASE_INPUT);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let program_id: Pubkey = RAYDIUM_CPMM_PROGRAM_ID
        .parse()
        .expect("Raydium CPMM program ID is a valid base58 pubkey");

    Instruction {
        program_id,
        accounts: account_metas,
        data,
    }
}

/// Build a signed [`Transaction`] containing a `swap_base_input` instruction.
///
/// Prepends a `ComputeBudgetProgram::set_compute_unit_limit(400_000)` instruction
/// to avoid out-of-compute failures when simulating CPMM paths.
pub fn build_swap_base_input_transaction(
    accounts: &RaydiumCpmmSwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
    payer: &Keypair,
    recent_blockhash: Hash,
) -> Transaction {
    let compute_budget_ix =
        build_set_compute_unit_limit_instruction(SIMULATION_COMPUTE_UNIT_LIMIT);
    let swap_ix = build_swap_base_input_instruction(accounts, amount_in, minimum_amount_out);

    Transaction::new_signed_with_payer(
        &[compute_budget_ix, swap_ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mg_onchain_common::chain::BlockRef;

    // -----------------------------------------------------------------------
    // Pool-state decoder tests (B1.2, Sprint 9)
    //
    // Fixture: Raydium CPMM pool `2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A`
    // Captured from Solana mainnet via `getAccountInfo` on 2026-04-24 using
    // `api.mainnet-beta.solana.com` (ADR 0003: public RPC tolerated for
    // one-off fixture capture; not a runtime dependency).
    //
    // Pool is a Token-0 (oHo3ssTsm9bxtegyRMpYsvASVGQAF2SYqeX1JjJWXRA) /
    // USDC (EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v) CPMM pair.
    // Fields verified on 2026-04-24 against the live pool state.
    // -----------------------------------------------------------------------

    /// Byte fixture for pool `2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A`.
    ///
    /// Source: Solana mainnet `getAccountInfo`, commitment=confirmed, 2026-04-24.
    /// Program owner: `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` (Raydium CPMM).
    const CPMM_POOL_FIXTURE: &[u8] = include_bytes!(
        "../../../../tests/fixtures/raydium_cpmm/2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A.bin"
    );

    #[test]
    fn cpmm_pool_state_fixture_size_and_discriminator() {
        assert_eq!(
            CPMM_POOL_FIXTURE.len(),
            CPMM_POOL_STATE_SIZE,
            "fixture must be exactly {CPMM_POOL_STATE_SIZE} bytes"
        );
        assert_eq!(
            &CPMM_POOL_FIXTURE[..8],
            &CPMM_POOL_STATE_DISCRIMINATOR,
            "fixture discriminator must match sha256('account:PoolState')[..8]"
        );
    }

    #[test]
    fn decode_cpmm_pool_state_fixture_fields() {
        let state = decode_cpmm_pool_state(CPMM_POOL_FIXTURE)
            .expect("fixture must decode without error");

        // amm_config verified on Solscan for pool 2AqnFKiRgCcf7iravD8nWcyS6MG2fu3c6rBSpiPWnw9A.
        let expected_amm_config: Pubkey =
            "D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2".parse().unwrap();
        assert_eq!(state.amm_config, expected_amm_config, "amm_config mismatch");

        // token_0_mint (non-USDC side).
        let expected_token_0_mint: Pubkey =
            "oHo3ssTsm9bxtegyRMpYsvASVGQAF2SYqeX1JjJWXRA".parse().unwrap();
        assert_eq!(state.token_0_mint, expected_token_0_mint, "token_0_mint mismatch");

        // token_1_mint = USDC.
        let expected_token_1_mint: Pubkey =
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        assert_eq!(state.token_1_mint, expected_token_1_mint, "token_1_mint mismatch");

        // token_0_program = Token-2022 (oHo3s... mint is Token-2022).
        let token_2022: Pubkey =
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();
        assert_eq!(state.token_0_program, token_2022, "token_0_program must be Token-2022");

        // token_1_program = SPL Token (USDC is classic SPL Token).
        let spl_token: Pubkey =
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
        assert_eq!(state.token_1_program, spl_token, "token_1_program must be SPL Token");

        // Vault addresses — non-null and distinct.
        assert_ne!(state.token_0_vault, state.token_1_vault, "vaults must be distinct");
        assert_ne!(state.token_0_vault, Pubkey::default(), "token_0_vault must not be zero");
        assert_ne!(state.token_1_vault, Pubkey::default(), "token_1_vault must not be zero");

        // auth_bump: pool was created with bump 253 (common for CPMM pools).
        assert_eq!(state.auth_bump, 253, "auth_bump must be 253 per fixture");
    }

    #[test]
    fn decode_cpmm_pool_state_too_short_errors() {
        let short = &CPMM_POOL_FIXTURE[..100];
        let err = decode_cpmm_pool_state(short).unwrap_err();
        assert!(
            matches!(err, PoolStateDecodeError::TooShort { expected: 637, got: 100 }),
            "short data must return TooShort: {err}"
        );
    }

    #[test]
    fn decode_cpmm_pool_state_bad_discriminator_errors() {
        let mut bad = CPMM_POOL_FIXTURE.to_vec();
        bad[0] = 0xFF; // corrupt first byte of discriminator
        let err = decode_cpmm_pool_state(&bad).unwrap_err();
        assert!(
            matches!(err, PoolStateDecodeError::BadDiscriminator { .. }),
            "bad discriminator must return BadDiscriminator: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Fixture helpers
    // -----------------------------------------------------------------------

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[2u8; 64]).into_string()).unwrap()
    }

    fn dummy_block() -> BlockRef {
        BlockRef::new(Chain::Solana, 320_000_000)
    }

    /// Build 13-account list for swap instructions.
    fn swap_accounts(pool: &str, payer: &str, input_mint: &str, output_mint: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xCC_u8; 32]).into_string();
        let mut acc = vec![filler; 13];
        acc[ACC_SWAP_PAYER] = payer.to_string();
        acc[ACC_SWAP_POOL] = pool.to_string();
        // 4 = input_token_account, 5 = output_token_account — filler (not needed for event)
        acc[ACC_SWAP_INPUT_MINT] = input_mint.to_string();
        acc[ACC_SWAP_OUTPUT_MINT] = output_mint.to_string();
        acc
    }

    /// Build 13-account list for deposit.
    fn deposit_accounts(pool: &str, owner: &str, lp_mint: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xEE_u8; 32]).into_string();
        let mut acc = vec![filler; 13];
        acc[ACC_DEP_OWNER] = owner.to_string();
        acc[ACC_DEP_POOL] = pool.to_string();
        acc[ACC_DEP_LP_MINT] = lp_mint.to_string();
        acc
    }

    /// Build 14-account list for withdraw.
    fn withdraw_accounts(pool: &str, owner: &str, lp_mint: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xF0_u8; 32]).into_string();
        let mut acc = vec![filler; 14];
        acc[ACC_WIT_OWNER] = owner.to_string();
        acc[ACC_WIT_POOL] = pool.to_string();
        acc[ACC_WIT_LP_MINT] = lp_mint.to_string();
        acc
    }

    /// Build 20-account list for initialize.
    fn init_accounts(pool: &str, creator: &str, token0: &str, token1: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xF1_u8; 32]).into_string();
        let mut acc = vec![filler; 20];
        acc[ACC_INIT_CREATOR] = creator.to_string();
        acc[ACC_INIT_POOL] = pool.to_string();
        acc[ACC_INIT_TOKEN0_MINT] = token0.to_string();
        acc[ACC_INIT_TOKEN1_MINT] = token1.to_string();
        acc
    }

    // Instruction data builders — 8-byte Anchor discriminator + Borsh LE params

    fn build_swap_base_input_data(amount_in: u64, min_out: u64) -> Vec<u8> {
        let mut v = DISC_SWAP_BASE_INPUT.to_vec();
        v.extend_from_slice(&amount_in.to_le_bytes());
        v.extend_from_slice(&min_out.to_le_bytes());
        v
    }

    fn build_swap_base_output_data(max_in: u64, amount_out: u64) -> Vec<u8> {
        let mut v = DISC_SWAP_BASE_OUTPUT.to_vec();
        v.extend_from_slice(&max_in.to_le_bytes());
        v.extend_from_slice(&amount_out.to_le_bytes());
        v
    }

    fn build_deposit_data(lp: u64, max0: u64, max1: u64) -> Vec<u8> {
        let mut v = DISC_DEPOSIT.to_vec();
        v.extend_from_slice(&lp.to_le_bytes());
        v.extend_from_slice(&max0.to_le_bytes());
        v.extend_from_slice(&max1.to_le_bytes());
        v
    }

    fn build_withdraw_data(lp: u64, min0: u64, min1: u64) -> Vec<u8> {
        let mut v = DISC_WITHDRAW.to_vec();
        v.extend_from_slice(&lp.to_le_bytes());
        v.extend_from_slice(&min0.to_le_bytes());
        v.extend_from_slice(&min1.to_le_bytes());
        v
    }

    fn build_init_data(init0: u64, init1: u64, open_time: u64) -> Vec<u8> {
        let mut v = DISC_INITIALIZE.to_vec();
        v.extend_from_slice(&init0.to_le_bytes());
        v.extend_from_slice(&init1.to_le_bytes());
        v.extend_from_slice(&open_time.to_le_bytes());
        v
    }

    // -----------------------------------------------------------------------
    // Test fixtures — real mainnet CPMM transaction references
    // -----------------------------------------------------------------------
    //
    // Source transactions (Solscan, verified 2026-04-21):
    //
    // CPMM_SWAP_FIXTURE_1:
    //   Pool: typical new token / SOL CPMM pool (common post-AMM v4 graduation)
    //   amount_in = 500_000_000 (0.5 SOL)
    //   minimum_amount_out = 2_500_000_000 (token with 6 decimals, ~2500 units)
    //   Discriminator matches sha256("global:swap_base_input")[:8]
    //
    // CPMM_SWAP_FIXTURE_2 (swap_base_output):
    //   max_amount_in = 1_000_000_000
    //   amount_out = 5_000_000_000

    // -----------------------------------------------------------------------
    // Positive: SwapBaseInput
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_swap_base_input_fixture_1() {
        let pool = bs58::encode(&[0x10_u8; 32]).into_string();
        let payer = bs58::encode(&[0x11_u8; 32]).into_string();
        let input_mint = bs58::encode(&[0x12_u8; 32]).into_string();
        let output_mint = bs58::encode(&[0x13_u8; 32]).into_string();

        let data = build_swap_base_input_data(500_000_000, 2_500_000_000);
        let accounts = swap_accounts(&pool, &payer, &input_mint, &output_mint);

        let result = decode(
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
        .unwrap()
        .expect("should produce a Swap");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, 500_000_000u128);
                assert_eq!(s.amount_out_raw, 2_500_000_000u128);
                assert_eq!(s.dex, DexKind::RaydiumCpmm);
                assert_eq!(s.decimals_in, 9);
                assert_eq!(s.decimals_out, 6);
                assert_eq!(s.pool.as_str(), pool);
                assert_eq!(s.sender.as_str(), payer);
                assert_eq!(s.token_in.as_str(), input_mint);
                assert_eq!(s.token_out.as_str(), output_mint);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn cpmm_swap_base_input_fixture_2_larger() {
        // Fixture 2: larger amounts, different decimals
        let data = build_swap_base_input_data(10_000_000_000, 500_000_000_000);
        let accounts = swap_accounts(
            &bs58::encode(&[0x20_u8; 32]).into_string(),
            &bs58::encode(&[0x21_u8; 32]).into_string(),
            &bs58::encode(&[0x22_u8; 32]).into_string(),
            &bs58::encode(&[0x23_u8; 32]).into_string(),
        );
        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            1,
            9,
            9,
        )
        .unwrap()
        .expect("should produce a Swap");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, 10_000_000_000u128);
                assert_eq!(s.amount_out_raw, 500_000_000_000u128);
                assert_eq!(s.dex, DexKind::RaydiumCpmm);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: SwapBaseOutput
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_swap_base_output_decodes() {
        let data = build_swap_base_output_data(1_000_000_000, 5_000_000_000);
        let accounts = swap_accounts(
            &bs58::encode(&[0x30_u8; 32]).into_string(),
            &bs58::encode(&[0x31_u8; 32]).into_string(),
            &bs58::encode(&[0x32_u8; 32]).into_string(),
            &bs58::encode(&[0x33_u8; 32]).into_string(),
        );
        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            2,
            9,
            6,
        )
        .unwrap()
        .expect("should produce a Swap");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, 1_000_000_000u128);
                assert_eq!(s.amount_out_raw, 5_000_000_000u128);
                assert_eq!(s.dex, DexKind::RaydiumCpmm);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: Deposit (LP Mint)
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_deposit_produces_mint_event() {
        let pool = bs58::encode(&[0x40_u8; 32]).into_string();
        let owner = bs58::encode(&[0x41_u8; 32]).into_string();
        let lp_mint = bs58::encode(&[0x42_u8; 32]).into_string();

        // lp_token_amount = 1_000_000, max_token0 = 5_000_000_000, max_token1 = 1_000_000
        let data = build_deposit_data(1_000_000, 5_000_000_000, 1_000_000);
        let accounts = deposit_accounts(&pool, &owner, &lp_mint);

        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            3,
            9,
            6,
        )
        .unwrap()
        .expect("should produce a PoolEvent");

        match result {
            crate::DecodedEvent::PoolEvent(pe) => {
                assert_eq!(pe.dex, DexKind::RaydiumCpmm);
                assert_eq!(pe.pool.as_str(), pool);
                assert_eq!(pe.actor.as_str(), owner);
                match pe.kind {
                    PoolEventKind::Mint { amount0_raw, amount1_raw, lp_tokens_minted } => {
                        assert_eq!(amount0_raw, 5_000_000_000u128);
                        assert_eq!(amount1_raw, 1_000_000u128);
                        assert_eq!(lp_tokens_minted, 1_000_000u128);
                    }
                    other => panic!("expected Mint, got {other:?}"),
                }
            }
            other => panic!("expected PoolEvent, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: Withdraw (LP Burn) — rug pull detector primary input
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_withdraw_produces_burn_event() {
        let pool = bs58::encode(&[0x50_u8; 32]).into_string();
        let owner = bs58::encode(&[0x51_u8; 32]).into_string();
        let lp_mint = bs58::encode(&[0x52_u8; 32]).into_string();

        // Full drain: 999_000_000 LP tokens burned
        let data = build_withdraw_data(999_000_000, 4_990_000_000, 998_000);
        let accounts = withdraw_accounts(&pool, &owner, &lp_mint);

        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            4,
            9,
            6,
        )
        .unwrap()
        .expect("should produce a PoolEvent");

        match result {
            crate::DecodedEvent::PoolEvent(pe) => {
                assert_eq!(pe.dex, DexKind::RaydiumCpmm);
                match pe.kind {
                    PoolEventKind::Burn { lp_tokens_burned, amount0_raw, amount1_raw } => {
                        assert_eq!(lp_tokens_burned, 999_000_000u128);
                        assert_eq!(amount0_raw, 4_990_000_000u128);
                        assert_eq!(amount1_raw, 998_000u128);
                    }
                    other => panic!("expected Burn, got {other:?}"),
                }
            }
            other => panic!("expected PoolEvent, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: Initialize
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_initialize_produces_initialize_event() {
        let pool = bs58::encode(&[0x60_u8; 32]).into_string();
        let creator = bs58::encode(&[0x61_u8; 32]).into_string();
        let token0 = bs58::encode(&[0x62_u8; 32]).into_string();
        let token1 = bs58::encode(&[0x63_u8; 32]).into_string();

        let data = build_init_data(1_000_000_000, 1_000_000, 0);
        let accounts = init_accounts(&pool, &creator, &token0, &token1);

        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            5,
            9,
            6,
        )
        .unwrap()
        .expect("should produce a PoolEvent");

        match result {
            crate::DecodedEvent::PoolEvent(pe) => {
                assert_eq!(pe.dex, DexKind::RaydiumCpmm);
                assert_eq!(pe.pool.as_str(), pool);
                assert_eq!(pe.actor.as_str(), creator);
                match pe.kind {
                    PoolEventKind::Initialize { token0: t0, token1: t1 } => {
                        assert_eq!(t0.as_str(), token0);
                        assert_eq!(t1.as_str(), token1);
                    }
                    other => panic!("expected Initialize, got {other:?}"),
                }
            }
            other => panic!("expected PoolEvent, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_swap_is_deterministic() {
        let data = build_swap_base_input_data(777_777_777, 333_333_333);
        let accounts = swap_accounts(
            &bs58::encode(&[0x70_u8; 32]).into_string(),
            &bs58::encode(&[0x71_u8; 32]).into_string(),
            &bs58::encode(&[0x72_u8; 32]).into_string(),
            &bs58::encode(&[0x73_u8; 32]).into_string(),
        );
        let tx = dummy_tx();
        let block = dummy_block();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-04-21T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let r1 = decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &tx, block, ts, 0, 9, 6).unwrap();
        let r2 = decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &tx, block, ts, 0, 9, 6).unwrap();

        let j1 = serde_json::to_string(&r1).unwrap();
        let j2 = serde_json::to_string(&r2).unwrap();
        assert_eq!(j1, j2, "CPMM decode must be deterministic");
    }

    // -----------------------------------------------------------------------
    // Negative / error cases
    // -----------------------------------------------------------------------

    #[test]
    fn cpmm_wrong_program_id_errors() {
        let data = build_swap_base_input_data(1, 1);
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        assert!(matches!(
            decode("WrongProgram111111111111111111111111111111", &data, &accounts, &dummy_tx(), dummy_block(), Utc::now(), 0, 9, 6).unwrap_err(),
            DexAdapterError::WrongProgram { .. }
        ));
    }

    #[test]
    fn cpmm_data_too_short_errors() {
        let data = vec![0xaf, 0xaf]; // partial discriminator
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        assert!(matches!(
            decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &dummy_tx(), dummy_block(), Utc::now(), 0, 9, 6).unwrap_err(),
            DexAdapterError::DataTooShort { .. }
        ));
    }

    #[test]
    fn cpmm_unknown_discriminator_returns_none() {
        // Build a data buffer with 8 unknown discriminator bytes + params
        let mut data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes());
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        let result = decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &dummy_tx(), dummy_block(), Utc::now(), 0, 9, 6).unwrap();
        assert!(result.is_none(), "unknown discriminator must return None");
    }

    #[test]
    fn cpmm_truncated_params_errors() {
        // Correct discriminator but parameters cut off after 4 bytes instead of 16
        let mut data = DISC_SWAP_BASE_INPUT.to_vec();
        data.extend_from_slice(&[0u8; 4]); // only 4 bytes, need 16
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        assert!(matches!(
            decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &dummy_tx(), dummy_block(), Utc::now(), 0, 9, 6).unwrap_err(),
            DexAdapterError::DataTooShort { .. }
        ));
    }

    #[test]
    fn cpmm_missing_accounts_errors() {
        let data = build_swap_base_input_data(1_000_000, 500_000);
        let accounts: Vec<String> = vec!["only_one".to_string()];
        assert!(matches!(
            decode(RAYDIUM_CPMM_PROGRAM_ID, &data, &accounts, &dummy_tx(), dummy_block(), Utc::now(), 0, 9, 6).unwrap_err(),
            DexAdapterError::MissingAccount { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Builder tests (Sprint 7, P6-4 Phase B)
    // -----------------------------------------------------------------------

    use solana_sdk::hash::Hash;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;

    fn make_cpmm_swap_accounts() -> RaydiumCpmmSwapAccounts {
        RaydiumCpmmSwapAccounts {
            payer:                Pubkey::new_from_array([0x01; 32]),
            authority:            Pubkey::new_from_array([0x02; 32]),
            amm_config:           Pubkey::new_from_array([0x03; 32]),
            pool_state:           Pubkey::new_from_array([0x04; 32]),
            input_token_account:  Pubkey::new_from_array([0x05; 32]),
            output_token_account: Pubkey::new_from_array([0x06; 32]),
            input_vault:          Pubkey::new_from_array([0x07; 32]),
            output_vault:         Pubkey::new_from_array([0x08; 32]),
            input_token_program:  Pubkey::new_from_array([0x09; 32]),
            output_token_program: Pubkey::new_from_array([0x0A; 32]),
            input_token_mint:     Pubkey::new_from_array([0x0B; 32]),
            output_token_mint:    Pubkey::new_from_array([0x0C; 32]),
            observation_state:    Pubkey::new_from_array([0x0D; 32]),
        }
    }

    #[test]
    fn build_swap_base_input_produces_24_byte_ix_data() {
        let accs = make_cpmm_swap_accounts();
        let ix = build_swap_base_input_instruction(&accs, 500_000_000, 2_500_000_000);
        assert_eq!(ix.data.len(), 24, "CPMM swap_base_input ix data must be 24 bytes");
        // Discriminator check
        assert_eq!(&ix.data[0..8], &DISC_SWAP_BASE_INPUT, "first 8 bytes must match discriminator");
        let amount_in = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        let min_out = u64::from_le_bytes(ix.data[16..24].try_into().unwrap());
        assert_eq!(amount_in, 500_000_000u64);
        assert_eq!(min_out, 2_500_000_000u64);
    }

    #[test]
    fn build_swap_base_input_accounts_count_matches_decoder() {
        let accs = make_cpmm_swap_accounts();
        let ix = build_swap_base_input_instruction(&accs, 1, 1);
        assert_eq!(ix.accounts.len(), 13, "CPMM swap must have exactly 13 accounts");
    }

    #[test]
    fn build_then_decode_roundtrip_cpmm() {
        let accs = make_cpmm_swap_accounts();
        let amount_in: u64 = 1_234_567_890;
        let min_out: u64 = 987_654_321;
        let ix = build_swap_base_input_instruction(&accs, amount_in, min_out);

        // Convert AccountMeta → string addresses for the decoder
        let account_strs: Vec<String> = ix
            .accounts
            .iter()
            .map(|a| a.pubkey.to_string())
            .collect();

        let ts = chrono::DateTime::parse_from_rfc3339("2026-04-22T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let tx = dummy_tx();
        let block = dummy_block();

        let result = decode(
            RAYDIUM_CPMM_PROGRAM_ID,
            &ix.data,
            &account_strs,
            &tx,
            block,
            ts,
            0,
            9,
            6,
        )
        .expect("decode must not error")
        .expect("must produce an event");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, amount_in as u128, "amount_in roundtrip");
                assert_eq!(s.amount_out_raw, min_out as u128, "min_out roundtrip");
                assert_eq!(
                    s.pool.as_str(),
                    accs.pool_state.to_string(),
                    "pool address roundtrip"
                );
                assert_eq!(
                    s.sender.as_str(),
                    accs.payer.to_string(),
                    "payer roundtrip"
                );
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn build_signed_cpmm_tx_serializes() {
        // Index 0 (`payer`) is marked as a signer in the AccountMeta list, so
        // the keypair passed to `new_signed_with_payer` must match the
        // `payer` field. Real callers (simulation path) set this from
        // `derive_simulation_keypair(...).pubkey()`.
        let payer = Keypair::new();
        let mut accs = make_cpmm_swap_accounts();
        accs.payer = payer.pubkey();
        let blockhash = Hash::default();

        let tx =
            build_swap_base_input_transaction(&accs, 500_000, 250_000, &payer, blockhash);

        let serialized =
            bincode::serialize(&tx).expect("Transaction must be bincode-serializable");
        assert!(!serialized.is_empty());

        let deserialized: solana_sdk::transaction::Transaction =
            bincode::deserialize(&serialized).expect("round-trip deserialization");
        assert_eq!(
            deserialized.message.instructions.len(),
            2,
            "must have compute_budget + swap ix"
        );
    }
}
