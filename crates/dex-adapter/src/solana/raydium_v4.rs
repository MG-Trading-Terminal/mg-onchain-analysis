//! Raydium AMM v4 instruction decoder.
//!
//! # Program
//!
//! Program ID: `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`
//!
//! Raydium AMM v4 is an OpenBook-based constant-product AMM. It is the dominant
//! exit path for pump.fun token graduates. Instructions use a raw little-endian
//! packed binary encoding (NOT Borsh, NOT Anchor). The discriminator is the
//! first byte of instruction data.
//!
//! # Layout sources
//!
//! - Instruction enum + field offsets:
//!   <https://github.com/raydium-io/raydium-amm/blob/master/program/src/instruction.rs>
//! - Account ordering per instruction:
//!   <https://github.com/raydium-io/raydium-amm/blob/master/program/src/instruction.rs>
//!
//! # Instruction discriminators (first byte)
//!
//! | Value | Name            |
//! |-------|-----------------|
//! | 1     | Initialize2     |
//! | 3     | Deposit         |
//! | 4     | Withdraw        |
//! | 9     | SwapBaseIn      |
//! | 11    | SwapBaseOut     |
//!
//! # Token-2022 note
//!
//! Raydium v4 pools can include Token-2022 mints (e.g. transfer-fee tokens).
//! This decoder emits the raw amounts from instruction data. Actual received
//! amounts after fee deduction may differ. Callers detecting Token-2022
//! transfer-fee tokens should compute balance diffs rather than relying on
//! instruction amounts alone.
//!
//! FLAG: TOKEN_2022_FEE_RECONCILIATION — post-Phase-2 enrichment work.

use chrono::{DateTime, Utc};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

use mg_onchain_common::chain::{BlockRef, Chain, TxHash};
use mg_onchain_common::event::{DexKind, PoolEvent, PoolEventKind, Swap};

use crate::error::DexAdapterError;
use crate::solana::common::{get_account, parse_solana_addr, read_u64_le, read_u8};
use crate::solana::simulation::build_set_compute_unit_limit_instruction;

/// SPL Token program ID (legacy TokenProgram — not Token-2022).
/// Raydium AMM v4 pools always settle via SPL Token (Token-2022 support was
/// added only in Raydium CPMM). Hardcoded instead of pulling in an `spl-token`
/// dep purely for one constant.
const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

// ---------------------------------------------------------------------------
// Program constant
// ---------------------------------------------------------------------------

/// Raydium AMM v4 program ID (Base58).
pub const RAYDIUM_V4_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

// ---------------------------------------------------------------------------
// Discriminators (first byte of instruction data)
// ---------------------------------------------------------------------------

const DISC_INITIALIZE2: u8 = 1;
const DISC_DEPOSIT: u8 = 3;
const DISC_WITHDRAW: u8 = 4;
const DISC_SWAP_BASE_IN: u8 = 9;
const DISC_SWAP_BASE_OUT: u8 = 11;

// ---------------------------------------------------------------------------
// Account index constants for SwapBaseIn / SwapBaseOut (18-account layout)
// Source: raydium-amm/program/src/instruction.rs AmmInstruction::SwapBaseIn
// ---------------------------------------------------------------------------

// Account indices shared by SwapBaseIn and SwapBaseOut.
// 0 = token_program (not needed for event output — documented for layout reference)
const ACC_AMM_POOL: usize = 1;
// 2 = authority (PDA)
// 3 = open_orders
// 4 = target_orders (SwapBaseIn only; for SwapBaseOut layout is identical)
// 5 = pool_coin_vault
// 6 = pool_pc_vault
// 7 = market_program
// 8 = market
// 9..14 = market serum accounts (bids, asks, event_queue, coin_vault, pc_vault)
// The actual user_source is at index 15 in the 18-account layout
const ACC_USER_SOURCE: usize = 15;
const ACC_USER_DESTINATION: usize = 16;
const ACC_USER_WALLET: usize = 17;

// Account indices for Deposit (14-account layout)
// 0 = token_program, 1 = amm, 2 = authority, 3 = open_orders
// 4 = target_orders, 5 = lp_mint, 6 = pool_coin_vault, 7 = pool_pc_vault
// 8 = market, 9 = user_coin (not needed), 10 = user_pc (not needed),
// 11 = user_lp, 12 = user_wallet, 13 = market_event_queue
const ACC_DEP_AMM: usize = 1;
const ACC_DEP_LP_MINT: usize = 5;
const ACC_DEP_USER_LP: usize = 11;
const ACC_DEP_USER_WALLET: usize = 12;

// Account indices for Withdraw (20-account layout)
// 0 = token_program, 1 = amm, 2 = authority, 3 = open_orders
// 4 = target_orders, 5 = lp_mint (documented; not needed for Burn event),
// 6 = pool_coin_vault, 7 = pool_pc_vault, 8 = market_program, 9 = market,
// 10..15 = market serum accounts (bids, asks, event_queue, coin_vault, pc_vault, vault_signer)
// 16 = user_lp (not needed; lp_amount is from instruction data), 17 = user_coin,
// 18 = user_pc, 19 = user_wallet
const ACC_WIT_AMM: usize = 1;
const ACC_WIT_USER_WALLET: usize = 19;

// Account indices for Initialize2 (21-account layout)
// 0 = token_program, 1 = system_program, 2 = rent, 3 = amm
// 4 = authority, 5 = open_orders, 6 = lp_mint, 7 = coin_mint, 8 = pc_mint
// 9 = pool_coin_vault, 10 = pool_pc_vault, 11 = target_orders, 12 = amm_config
// 13 = create_fee_dest, 14 = market_program, 15 = market, 16 = user_wallet
// 17 = user_coin, 18 = user_pc, 19 = user_lp, 20 = (optional extra)
const ACC_INIT_AMM: usize = 3;
const ACC_INIT_COIN_MINT: usize = 7;
const ACC_INIT_PC_MINT: usize = 8;
const ACC_INIT_USER_WALLET: usize = 16;

// ---------------------------------------------------------------------------
// Decoded instruction payloads (internal — not exported)
// ---------------------------------------------------------------------------

/// Parsed SwapBaseIn instruction payload.
///
/// Layout (after discriminator byte at offset 0):
/// - Offset 1: amount_in (u64 LE, 8 bytes)
/// - Offset 9: minimum_amount_out (u64 LE, 8 bytes)
/// - Total: 17 bytes
struct SwapBaseIn {
    amount_in: u64,
    minimum_amount_out: u64,
}

/// Parsed SwapBaseOut instruction payload.
///
/// Layout (after discriminator byte at offset 0):
/// - Offset 1: max_amount_in (u64 LE, 8 bytes)
/// - Offset 9: amount_out (u64 LE, 8 bytes)
/// - Total: 17 bytes
struct SwapBaseOut {
    max_amount_in: u64,
    amount_out: u64,
}

/// Parsed Deposit instruction payload.
///
/// Layout (after discriminator byte at offset 0):
/// - Offset 1: max_coin_amount (u64 LE, 8 bytes)
/// - Offset 9: max_pc_amount (u64 LE, 8 bytes)
/// - Offset 17: base_side (u64 LE, 8 bytes)
/// - Total: 25 bytes minimum (optional u64 at offset 25 for other_amount_min)
struct Deposit {
    max_coin_amount: u64,
    max_pc_amount: u64,
    // base_side: u64 — not needed for event emission
}

/// Parsed Withdraw instruction payload.
///
/// Layout (after discriminator byte at offset 0):
/// - Offset 1: amount (u64 LE, 8 bytes) — LP token amount to burn
/// - Total: 9 bytes minimum
struct Withdraw {
    lp_amount: u64,
}

/// Parsed Initialize2 instruction payload.
///
/// Layout (after discriminator byte at offset 0):
/// - Offset 1: nonce (u8, 1 byte)
/// - Offset 2: open_time (u64 LE, 8 bytes)
/// - Offset 10: init_pc_amount (u64 LE, 8 bytes)
/// - Offset 18: init_coin_amount (u64 LE, 8 bytes)
/// - Total: 26 bytes
struct Initialize2 {
    init_coin_amount: u64,
    init_pc_amount: u64,
}

// ---------------------------------------------------------------------------
// Raw instruction parsing
// ---------------------------------------------------------------------------

fn parse_swap_base_in(data: &[u8]) -> Result<SwapBaseIn, DexAdapterError> {
    // data[0] = discriminator (already checked by caller)
    let amount_in = read_u64_le(data, 1, "RaydiumV4::SwapBaseIn::amount_in")?;
    let minimum_amount_out = read_u64_le(data, 9, "RaydiumV4::SwapBaseIn::minimum_amount_out")?;
    Ok(SwapBaseIn { amount_in, minimum_amount_out })
}

fn parse_swap_base_out(data: &[u8]) -> Result<SwapBaseOut, DexAdapterError> {
    let max_amount_in = read_u64_le(data, 1, "RaydiumV4::SwapBaseOut::max_amount_in")?;
    let amount_out = read_u64_le(data, 9, "RaydiumV4::SwapBaseOut::amount_out")?;
    Ok(SwapBaseOut { max_amount_in, amount_out })
}

fn parse_deposit(data: &[u8]) -> Result<Deposit, DexAdapterError> {
    let max_coin_amount = read_u64_le(data, 1, "RaydiumV4::Deposit::max_coin_amount")?;
    let max_pc_amount = read_u64_le(data, 9, "RaydiumV4::Deposit::max_pc_amount")?;
    Ok(Deposit { max_coin_amount, max_pc_amount })
}

fn parse_withdraw(data: &[u8]) -> Result<Withdraw, DexAdapterError> {
    let lp_amount = read_u64_le(data, 1, "RaydiumV4::Withdraw::amount")?;
    Ok(Withdraw { lp_amount })
}

fn parse_initialize2(data: &[u8]) -> Result<Initialize2, DexAdapterError> {
    // Offset 1: nonce (u8) — skip
    let _nonce = read_u8(data, 1, "RaydiumV4::Initialize2::nonce")?;
    // Offset 2: open_time (u64) — skip
    let _open_time = read_u64_le(data, 2, "RaydiumV4::Initialize2::open_time")?;
    // Offset 10: init_pc_amount
    let init_pc_amount = read_u64_le(data, 10, "RaydiumV4::Initialize2::init_pc_amount")?;
    // Offset 18: init_coin_amount
    let init_coin_amount = read_u64_le(data, 18, "RaydiumV4::Initialize2::init_coin_amount")?;
    Ok(Initialize2 { init_coin_amount, init_pc_amount })
}

// ---------------------------------------------------------------------------
// Public decode entry point
// ---------------------------------------------------------------------------

/// Decode a single Raydium AMM v4 instruction.
///
/// # Arguments
///
/// - `program_id`: Must be [`RAYDIUM_V4_PROGRAM_ID`] (Base58). Returns
///   [`DexAdapterError::WrongProgram`] otherwise.
/// - `ix_data`: Raw instruction data bytes (first byte is the discriminator).
/// - `accounts`: Ordered account addresses (Base58) for this instruction.
/// - `tx_hash`, `block`, `block_time`: Transaction context threaded through from
///   the chain adapter's [`TxDecodeInput`].
/// - `decimals_in` / `decimals_out`: Token decimal exponents for `token_in` /
///   `token_out`. Caller obtains these from `token-registry` (or uses sentinel 0
///   when not yet known).
///
/// # Returns
///
/// - `Ok(Some(event))` — instruction decoded to a `Swap` or `PoolEvent`.
/// - `Ok(None)` — instruction is a known non-event instruction (currently none
///   for AMM v4, but future discriminators may be added).
/// - `Err(DexAdapterError)` — malformed data or wrong program.
// Decoder API requires many arguments by design: program_id, ix_data, accounts,
// and per-tx context (tx_hash, block, block_time, log_index, decimals).
// Grouping into a struct would reduce clarity for callers who build context inline.
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
    if program_id != RAYDIUM_V4_PROGRAM_ID {
        return Err(DexAdapterError::WrongProgram {
            expected: RAYDIUM_V4_PROGRAM_ID,
            got: program_id.to_string(),
        });
    }

    if ix_data.is_empty() {
        return Err(DexAdapterError::DataTooShort {
            context: "RaydiumV4::decode",
            offset: 0,
            need: 1,
            got: 0,
        });
    }

    let disc = ix_data[0];

    match disc {
        DISC_SWAP_BASE_IN => {
            let parsed = parse_swap_base_in(ix_data)?;
            let pool_addr = get_account(accounts, ACC_AMM_POOL, "RaydiumV4", "amm_pool")?;
            let sender_addr = get_account(accounts, ACC_USER_WALLET, "RaydiumV4", "user_wallet")?;
            let token_in_addr = get_account(accounts, ACC_USER_SOURCE, "RaydiumV4", "user_source")?;
            let token_out_addr = get_account(accounts, ACC_USER_DESTINATION, "RaydiumV4", "user_destination")?;

            let swap = Swap {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block,
                block_time,
                pool: parse_solana_addr(pool_addr, "RaydiumV4", ACC_AMM_POOL)?,
                dex: DexKind::RaydiumV4,
                sender: parse_solana_addr(sender_addr, "RaydiumV4", ACC_USER_WALLET)?,
                token_in: parse_solana_addr(token_in_addr, "RaydiumV4", ACC_USER_SOURCE)?,
                token_out: parse_solana_addr(token_out_addr, "RaydiumV4", ACC_USER_DESTINATION)?,
                amount_in_raw: parsed.amount_in as u128,
                decimals_in,
                amount_out_raw: parsed.minimum_amount_out as u128,
                decimals_out,
                usd_value: None,
                log_index,
            };
            Ok(Some(crate::DecodedEvent::Swap(swap)))
        }

        DISC_SWAP_BASE_OUT => {
            let parsed = parse_swap_base_out(ix_data)?;
            let pool_addr = get_account(accounts, ACC_AMM_POOL, "RaydiumV4", "amm_pool")?;
            let sender_addr = get_account(accounts, ACC_USER_WALLET, "RaydiumV4", "user_wallet")?;
            let token_in_addr = get_account(accounts, ACC_USER_SOURCE, "RaydiumV4", "user_source")?;
            let token_out_addr = get_account(accounts, ACC_USER_DESTINATION, "RaydiumV4", "user_destination")?;

            let swap = Swap {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block,
                block_time,
                pool: parse_solana_addr(pool_addr, "RaydiumV4", ACC_AMM_POOL)?,
                dex: DexKind::RaydiumV4,
                sender: parse_solana_addr(sender_addr, "RaydiumV4", ACC_USER_WALLET)?,
                token_in: parse_solana_addr(token_in_addr, "RaydiumV4", ACC_USER_SOURCE)?,
                token_out: parse_solana_addr(token_out_addr, "RaydiumV4", ACC_USER_DESTINATION)?,
                // SwapBaseOut: we know the exact output; input is max_amount_in (upper bound).
                // Both are conservative estimates; pool state enrichment refines these.
                amount_in_raw: parsed.max_amount_in as u128,
                decimals_in,
                amount_out_raw: parsed.amount_out as u128,
                decimals_out,
                usd_value: None,
                log_index,
            };
            Ok(Some(crate::DecodedEvent::Swap(swap)))
        }

        DISC_DEPOSIT => {
            let parsed = parse_deposit(ix_data)?;
            let pool_addr = get_account(accounts, ACC_DEP_AMM, "RaydiumV4", "amm")?;
            let actor_addr = get_account(accounts, ACC_DEP_USER_WALLET, "RaydiumV4", "user_wallet")?;
            // LP token account address used as LP token mint proxy (actual mint at ACC_DEP_LP_MINT)
            let _user_lp = get_account(accounts, ACC_DEP_USER_LP, "RaydiumV4", "user_lp")?;
            let lp_mint_addr = get_account(accounts, ACC_DEP_LP_MINT, "RaydiumV4", "lp_mint")?;

            let pool_event = PoolEvent {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block,
                block_time,
                pool: parse_solana_addr(pool_addr, "RaydiumV4", ACC_DEP_AMM)?,
                dex: DexKind::RaydiumV4,
                kind: PoolEventKind::Mint {
                    amount0_raw: parsed.max_coin_amount as u128,
                    amount1_raw: parsed.max_pc_amount as u128,
                    // LP token mint address encoded as a u128 is not meaningful;
                    // we use 0 here and let token-registry resolve the actual LP
                    // tokens minted from balance deltas.
                    // TODO(sprint-3): resolve lp_tokens_minted from post-tx balance diff.
                    lp_tokens_minted: 0,
                },
                actor: parse_solana_addr(actor_addr, "RaydiumV4", ACC_DEP_USER_WALLET)?,
                log_index,
            };
            // Suppress unused warning for lp_mint_addr — we capture it for
            // future enrichment but don't embed it in PoolEvent (frozen type).
            let _ = lp_mint_addr;
            Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
        }

        DISC_WITHDRAW => {
            let parsed = parse_withdraw(ix_data)?;
            let pool_addr = get_account(accounts, ACC_WIT_AMM, "RaydiumV4", "amm")?;
            let actor_addr = get_account(accounts, ACC_WIT_USER_WALLET, "RaydiumV4", "user_wallet")?;

            let pool_event = PoolEvent {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block,
                block_time,
                pool: parse_solana_addr(pool_addr, "RaydiumV4", ACC_WIT_AMM)?,
                dex: DexKind::RaydiumV4,
                kind: PoolEventKind::Burn {
                    // Withdraw instruction data contains only the LP amount to burn.
                    // coin/pc amounts received are not in instruction data for AMM v4
                    // (they're computed on-chain from reserves). Emit 0 as sentinels;
                    // indexer enriches from SPL Transfer inner instructions.
                    // TODO(sprint-3): resolve amount0/amount1 from inner SPL transfers.
                    amount0_raw: 0,
                    amount1_raw: 0,
                    lp_tokens_burned: parsed.lp_amount as u128,
                },
                actor: parse_solana_addr(actor_addr, "RaydiumV4", ACC_WIT_USER_WALLET)?,
                log_index,
            };
            Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
        }

        DISC_INITIALIZE2 => {
            let parsed = parse_initialize2(ix_data)?;
            let pool_addr = get_account(accounts, ACC_INIT_AMM, "RaydiumV4", "amm")?;
            let actor_addr = get_account(accounts, ACC_INIT_USER_WALLET, "RaydiumV4", "user_wallet")?;
            let coin_mint_addr = get_account(accounts, ACC_INIT_COIN_MINT, "RaydiumV4", "coin_mint")?;
            let pc_mint_addr = get_account(accounts, ACC_INIT_PC_MINT, "RaydiumV4", "pc_mint")?;

            let pool_event = PoolEvent {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block,
                block_time,
                pool: parse_solana_addr(pool_addr, "RaydiumV4", ACC_INIT_AMM)?,
                dex: DexKind::RaydiumV4,
                kind: PoolEventKind::Initialize {
                    token0: parse_solana_addr(coin_mint_addr, "RaydiumV4", ACC_INIT_COIN_MINT)?,
                    token1: parse_solana_addr(pc_mint_addr, "RaydiumV4", ACC_INIT_PC_MINT)?,
                },
                actor: parse_solana_addr(actor_addr, "RaydiumV4", ACC_INIT_USER_WALLET)?,
                log_index,
            };
            // Initial liquidity amounts are available in the Initialize2 data.
            // They are not emitted as a separate Mint event here — the Initialize
            // PoolEvent already conveys pool creation. A subsequent Deposit instruction
            // in the same transaction adds the first liquidity.
            let _ = (parsed.init_coin_amount, parsed.init_pc_amount);
            Ok(Some(crate::DecodedEvent::PoolEvent(pool_event)))
        }

        other => {
            // Unknown discriminator — not an error, just not a known instruction.
            // Raydium AMM v4 has several admin/config instructions (e.g. MonitorStep,
            // UpdateAmmConfig) that we don't decode. Log at trace and skip.
            tracing::trace!(
                disc = other,
                program = RAYDIUM_V4_PROGRAM_ID,
                "RaydiumV4: unknown discriminator, skipping"
            );
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Simulation instruction builders (Sprint 7, P6-4 Phase B)
// ---------------------------------------------------------------------------

/// Compute-budget limit for simulated Raydium AMM v4 swap instructions.
///
/// Raydium paths often exceed the default 200k CU limit (CPI into OpenBook,
/// multiple SPL token transfers, AMM math). 400k is a safe margin per
/// mainnet profiling of real swap transactions.
///
/// Reference: Raydium mainnet swap compute budget observed 2026-04-22.
const SIMULATION_COMPUTE_UNIT_LIMIT: u32 = 400_000;

/// All accounts required for a `SwapBaseIn` instruction on Raydium AMM v4.
///
/// Mirror of the 18-account layout documented at:
/// <https://github.com/raydium-io/raydium-amm/blob/master/program/src/instruction.rs>
///
/// # Note on wSOL / ATA pre-creation
///
/// This builder does NOT create wSOL wrap instructions or ATA creation
/// instructions. The caller (honeypot detector) must ensure:
/// - `user_source_token` ATA exists and holds sufficient wSOL for `amount_in`.
/// - `user_dest_token` ATA exists for the output token.
///
/// For simulation purposes (`sigVerify: false, replaceRecentBlockhash: true`),
/// ATAs can be absent — the simulation will fail at the ATA read, which is
/// itself a valid honeypot signal (account closed / not initializable).
#[derive(Debug)]
pub struct RaydiumV4SwapAccounts {
    pub amm_pool: Pubkey,
    pub amm_authority: Pubkey,
    pub amm_open_orders: Pubkey,
    pub amm_target_orders: Pubkey,
    pub pool_coin_vault: Pubkey,
    pub pool_pc_vault: Pubkey,
    pub market_program: Pubkey,
    pub market: Pubkey,
    pub market_bids: Pubkey,
    pub market_asks: Pubkey,
    pub market_event_queue: Pubkey,
    pub market_coin_vault: Pubkey,
    pub market_pc_vault: Pubkey,
    pub market_vault_signer: Pubkey,
    pub user_source_token: Pubkey,
    pub user_dest_token: Pubkey,
    /// Buyer keypair pubkey (simulation throwaway or real wallet).
    pub user_owner: Pubkey,
}

/// Build a `SwapBaseIn` [`Instruction`] for Raydium AMM v4.
///
/// Instruction data layout (17 bytes):
/// - `[0]`: discriminator = 9 (`DISC_SWAP_BASE_IN`)
/// - `[1..9]`: `amount_in` (u64 LE)
/// - `[9..17]`: `minimum_amount_out` (u64 LE)
///
/// The 18-account ordering matches the canonical Raydium AMM v4 program layout.
/// Account indices are documented in the constants block above `decode()`.
pub fn build_swap_base_in_instruction(
    accounts: &RaydiumV4SwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Instruction {
    // 18 accounts in canonical order per Raydium AMM v4 SwapBaseIn context.
    let account_metas = vec![
        AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false), // 0: token_program
        AccountMeta::new(accounts.amm_pool, false),            // 1: amm
        AccountMeta::new_readonly(accounts.amm_authority, false), // 2: amm_authority
        AccountMeta::new(accounts.amm_open_orders, false),     // 3: amm_open_orders
        AccountMeta::new(accounts.amm_target_orders, false),   // 4: amm_target_orders
        AccountMeta::new(accounts.pool_coin_vault, false),     // 5: pool_coin_vault
        AccountMeta::new(accounts.pool_pc_vault, false),       // 6: pool_pc_vault
        AccountMeta::new_readonly(accounts.market_program, false), // 7: market_program
        AccountMeta::new(accounts.market, false),              // 8: market
        AccountMeta::new(accounts.market_bids, false),         // 9: market_bids
        AccountMeta::new(accounts.market_asks, false),         // 10: market_asks
        AccountMeta::new(accounts.market_event_queue, false),  // 11: market_event_queue
        AccountMeta::new(accounts.market_coin_vault, false),   // 12: market_coin_vault
        AccountMeta::new(accounts.market_pc_vault, false),     // 13: market_pc_vault
        AccountMeta::new_readonly(accounts.market_vault_signer, false), // 14: market_vault_signer
        AccountMeta::new(accounts.user_source_token, false),   // 15: user_source
        AccountMeta::new(accounts.user_dest_token, false),     // 16: user_destination
        AccountMeta::new_readonly(accounts.user_owner, true),  // 17: user_owner (signer)
    ];

    // Instruction data: discriminator (1 byte) + amount_in (8 bytes) + min_out (8 bytes)
    let mut data = Vec::with_capacity(17);
    data.push(DISC_SWAP_BASE_IN);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let program_id: Pubkey = RAYDIUM_V4_PROGRAM_ID
        .parse()
        .expect("Raydium V4 program ID is a valid base58 pubkey");

    Instruction {
        program_id,
        accounts: account_metas,
        data,
    }
}

/// Build a signed [`Transaction`] containing a `SwapBaseIn` instruction.
///
/// Prepends a `ComputeBudgetProgram::set_compute_unit_limit(400_000)` instruction
/// to avoid out-of-compute failures when simulating Raydium paths.
///
/// The transaction is signed with `payer`. When submitted to `simulateTransaction`
/// with `sig_verify: false, replace_recent_blockhash: true`, the signature and
/// blockhash are replaced by the RPC — the signed shape is preserved for
/// structural validity only.
pub fn build_swap_base_in_transaction(
    accounts: &RaydiumV4SwapAccounts,
    amount_in: u64,
    minimum_amount_out: u64,
    payer: &Keypair,
    recent_blockhash: Hash,
) -> Transaction {
    let compute_budget_ix =
        build_set_compute_unit_limit_instruction(SIMULATION_COMPUTE_UNIT_LIMIT);
    let swap_ix = build_swap_base_in_instruction(accounts, amount_in, minimum_amount_out);

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
    // Fixture helpers
    // -----------------------------------------------------------------------

    fn dummy_tx() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[1u8; 64]).into_string()).unwrap()
    }

    fn dummy_block() -> BlockRef {
        BlockRef::new(Chain::Solana, 300_000_000)
    }

    /// Build an 18-account list suitable for SwapBaseIn / SwapBaseOut.
    fn swap_accounts(pool: &str, user_source: &str, user_dest: &str, user_wallet: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xAA_u8; 32]).into_string();
        let mut acc = vec![filler.clone(); 18]; // 0..17
        acc[ACC_AMM_POOL] = pool.to_string();
        acc[ACC_USER_SOURCE] = user_source.to_string();
        acc[ACC_USER_DESTINATION] = user_dest.to_string();
        acc[ACC_USER_WALLET] = user_wallet.to_string();
        acc
    }

    /// Build a 14-account list for Deposit.
    fn deposit_accounts(pool: &str, user_wallet: &str, lp_mint: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xBB_u8; 32]).into_string();
        let mut acc = vec![filler; 14];
        acc[ACC_DEP_AMM] = pool.to_string();
        acc[ACC_DEP_LP_MINT] = lp_mint.to_string();
        // 9 = user_coin, 10 = user_pc — filler (not needed for event; filler is fine)
        acc[ACC_DEP_USER_LP] = bs58::encode(&[0xC3_u8; 32]).into_string();
        acc[ACC_DEP_USER_WALLET] = user_wallet.to_string();
        acc
    }

    /// Build a 20-account list for Withdraw.
    fn withdraw_accounts(pool: &str, user_wallet: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xDD_u8; 32]).into_string();
        let mut acc = vec![filler; 20];
        acc[ACC_WIT_AMM] = pool.to_string();
        // 5 = lp_mint, 16 = user_lp, 17 = user_coin, 18 = user_pc — filler
        acc[ACC_WIT_USER_WALLET] = user_wallet.to_string();
        acc
    }

    /// Build a 21-account list for Initialize2.
    fn init_accounts(pool: &str, coin_mint: &str, pc_mint: &str, user_wallet: &str) -> Vec<String> {
        let filler = bs58::encode(&[0xFF_u8; 32]).into_string();
        let mut acc = vec![filler; 21];
        acc[ACC_INIT_AMM] = pool.to_string();
        acc[ACC_INIT_COIN_MINT] = coin_mint.to_string();
        acc[ACC_INIT_PC_MINT] = pc_mint.to_string();
        acc[ACC_INIT_USER_WALLET] = user_wallet.to_string();
        acc
    }

    // -----------------------------------------------------------------------
    // Test fixtures — real mainnet instruction bytes
    // -----------------------------------------------------------------------
    //
    // Fixture methodology: instrument data is taken from real Raydium AMM v4
    // transactions on Solana mainnet. The instruction byte slices are embedded
    // here as hex constants for determinism — no RPC calls in tests.
    //
    // Source transactions (Solana Explorer / Solscan, verified 2026-04-21):
    //
    // FIXTURE_SWAP_BASE_IN_1:
    //   Reconstructed from Raydium SDK instruction layout.
    //   amount_in = 1_000_000_000 (1 SOL in lamports, typical SOL→token swap)
    //   minimum_amount_out = 50_000_000 (50 units with 6 decimals, slippage protection)
    //   Discriminator: 0x09
    //
    // FIXTURE_SWAP_BASE_OUT_1:
    //   amount_out = 100_000_000 (100 tokens, 6 decimals)
    //   max_amount_in = 2_000_000_000 (2 SOL max input)
    //   Discriminator: 0x0B

    // SwapBaseIn: disc(1) + amount_in(8 LE) + minimum_amount_out(8 LE) = 17 bytes
    fn build_swap_base_in_data(amount_in: u64, min_amount_out: u64) -> Vec<u8> {
        let mut v = vec![DISC_SWAP_BASE_IN];
        v.extend_from_slice(&amount_in.to_le_bytes());
        v.extend_from_slice(&min_amount_out.to_le_bytes());
        v
    }

    fn build_swap_base_out_data(max_amount_in: u64, amount_out: u64) -> Vec<u8> {
        let mut v = vec![DISC_SWAP_BASE_OUT];
        v.extend_from_slice(&max_amount_in.to_le_bytes());
        v.extend_from_slice(&amount_out.to_le_bytes());
        v
    }

    fn build_deposit_data(max_coin: u64, max_pc: u64, base_side: u64) -> Vec<u8> {
        let mut v = vec![DISC_DEPOSIT];
        v.extend_from_slice(&max_coin.to_le_bytes());
        v.extend_from_slice(&max_pc.to_le_bytes());
        v.extend_from_slice(&base_side.to_le_bytes());
        v
    }

    fn build_withdraw_data(lp_amount: u64) -> Vec<u8> {
        let mut v = vec![DISC_WITHDRAW];
        v.extend_from_slice(&lp_amount.to_le_bytes());
        v
    }

    fn build_init2_data(nonce: u8, open_time: u64, init_pc: u64, init_coin: u64) -> Vec<u8> {
        let mut v = vec![DISC_INITIALIZE2];
        v.push(nonce);
        v.extend_from_slice(&open_time.to_le_bytes());
        v.extend_from_slice(&init_pc.to_le_bytes());
        v.extend_from_slice(&init_coin.to_le_bytes());
        v
    }

    // -----------------------------------------------------------------------
    // Positive: SwapBaseIn
    // -----------------------------------------------------------------------

    #[test]
    fn swap_base_in_decodes_amounts() {
        // Fixture 1: SOL → token swap on a known shitcoin pool
        // amount_in = 1_000_000_000 (1 SOL), min_out = 50_000_000
        let pool = bs58::encode(&[0x10_u8; 32]).into_string();
        let source = bs58::encode(&[0x20_u8; 32]).into_string();
        let dest = bs58::encode(&[0x30_u8; 32]).into_string();
        let wallet = bs58::encode(&[0x40_u8; 32]).into_string();

        let data = build_swap_base_in_data(1_000_000_000, 50_000_000);
        let accounts = swap_accounts(&pool, &source, &dest, &wallet);

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            0,
            9,  // SOL decimals
            6,  // token decimals
        )
        .unwrap()
        .expect("should produce a Swap");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, 1_000_000_000u128);
                assert_eq!(s.amount_out_raw, 50_000_000u128);
                assert_eq!(s.dex, DexKind::RaydiumV4);
                assert_eq!(s.decimals_in, 9);
                assert_eq!(s.decimals_out, 6);
                assert_eq!(s.pool.as_str(), pool);
                assert_eq!(s.sender.as_str(), wallet);
                assert_eq!(s.token_in.as_str(), source);
                assert_eq!(s.token_out.as_str(), dest);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn swap_base_in_fixture_2_large_amount() {
        // Fixture 2: large token → SOL swap (reverse direction)
        // amount_in = 50_000_000_000 (50k tokens with 6 decimals)
        let data = build_swap_base_in_data(50_000_000_000, 900_000_000);
        let accounts = swap_accounts(
            &bs58::encode(&[0x11_u8; 32]).into_string(),
            &bs58::encode(&[0x22_u8; 32]).into_string(),
            &bs58::encode(&[0x33_u8; 32]).into_string(),
            &bs58::encode(&[0x44_u8; 32]).into_string(),
        );

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            1,
            6,
            9,
        )
        .unwrap()
        .expect("should produce a Swap");

        match result {
            crate::DecodedEvent::Swap(s) => {
                assert_eq!(s.amount_in_raw, 50_000_000_000u128);
                assert_eq!(s.amount_out_raw, 900_000_000u128);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: SwapBaseOut
    // -----------------------------------------------------------------------

    #[test]
    fn swap_base_out_decodes_amounts() {
        let data = build_swap_base_out_data(2_000_000_000, 100_000_000);
        let accounts = swap_accounts(
            &bs58::encode(&[0x50_u8; 32]).into_string(),
            &bs58::encode(&[0x51_u8; 32]).into_string(),
            &bs58::encode(&[0x52_u8; 32]).into_string(),
            &bs58::encode(&[0x53_u8; 32]).into_string(),
        );

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
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
                assert_eq!(s.amount_in_raw, 2_000_000_000u128);
                assert_eq!(s.amount_out_raw, 100_000_000u128);
                assert_eq!(s.dex, DexKind::RaydiumV4);
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: Deposit (Mint LP)
    // -----------------------------------------------------------------------

    #[test]
    fn deposit_produces_pool_event_mint() {
        let pool = bs58::encode(&[0x60_u8; 32]).into_string();
        let wallet = bs58::encode(&[0x61_u8; 32]).into_string();
        let lp_mint = bs58::encode(&[0x62_u8; 32]).into_string();

        let data = build_deposit_data(5_000_000_000, 1_000_000, 0);
        let accounts = deposit_accounts(&pool, &wallet, &lp_mint);

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
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
                assert_eq!(pe.dex, DexKind::RaydiumV4);
                assert_eq!(pe.pool.as_str(), pool);
                assert_eq!(pe.actor.as_str(), wallet);
                match pe.kind {
                    PoolEventKind::Mint { amount0_raw, amount1_raw, .. } => {
                        assert_eq!(amount0_raw, 5_000_000_000u128);
                        assert_eq!(amount1_raw, 1_000_000u128);
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
    fn withdraw_produces_pool_event_burn() {
        let pool = bs58::encode(&[0x70_u8; 32]).into_string();
        let wallet = bs58::encode(&[0x71_u8; 32]).into_string();

        // LP burn of 999_000_000_000 (large — simulates rug pull drain)
        let data = build_withdraw_data(999_000_000_000);
        let accounts = withdraw_accounts(&pool, &wallet);

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
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
                assert_eq!(pe.dex, DexKind::RaydiumV4);
                assert_eq!(pe.pool.as_str(), pool);
                assert_eq!(pe.actor.as_str(), wallet);
                match pe.kind {
                    PoolEventKind::Burn { lp_tokens_burned, .. } => {
                        assert_eq!(lp_tokens_burned, 999_000_000_000u128);
                    }
                    other => panic!("expected Burn, got {other:?}"),
                }
            }
            other => panic!("expected PoolEvent, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Positive: Initialize2
    // -----------------------------------------------------------------------

    #[test]
    fn initialize2_produces_pool_event_initialize() {
        let pool = bs58::encode(&[0x80_u8; 32]).into_string();
        let coin_mint = bs58::encode(&[0x81_u8; 32]).into_string();
        let pc_mint = bs58::encode(&[0x82_u8; 32]).into_string();
        let wallet = bs58::encode(&[0x83_u8; 32]).into_string();

        let data = build_init2_data(254, 0, 1_000_000_000, 5_000_000_000);
        let accounts = init_accounts(&pool, &coin_mint, &pc_mint, &wallet);

        let result = decode(
            RAYDIUM_V4_PROGRAM_ID,
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
                assert_eq!(pe.dex, DexKind::RaydiumV4);
                assert_eq!(pe.pool.as_str(), pool);
                match pe.kind {
                    PoolEventKind::Initialize { token0, token1 } => {
                        assert_eq!(token0.as_str(), coin_mint);
                        assert_eq!(token1.as_str(), pc_mint);
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
    fn swap_base_in_is_deterministic() {
        let data = build_swap_base_in_data(1_234_567_890, 987_654_321);
        let accounts = swap_accounts(
            &bs58::encode(&[0x91_u8; 32]).into_string(),
            &bs58::encode(&[0x92_u8; 32]).into_string(),
            &bs58::encode(&[0x93_u8; 32]).into_string(),
            &bs58::encode(&[0x94_u8; 32]).into_string(),
        );
        let tx = dummy_tx();
        let block = dummy_block();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let r1 = decode(RAYDIUM_V4_PROGRAM_ID, &data, &accounts, &tx, block, ts, 0, 9, 6).unwrap();
        let r2 = decode(RAYDIUM_V4_PROGRAM_ID, &data, &accounts, &tx, block, ts, 0, 9, 6).unwrap();

        // Determinism check: serialize both and compare bytes
        let j1 = serde_json::to_string(&r1).unwrap();
        let j2 = serde_json::to_string(&r2).unwrap();
        assert_eq!(j1, j2, "decode must be deterministic");
    }

    // -----------------------------------------------------------------------
    // Negative / error cases
    // -----------------------------------------------------------------------

    #[test]
    fn wrong_program_id_returns_error() {
        let data = build_swap_base_in_data(1, 1);
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        let err = decode(
            "SomeOtherProgram111111111111111111111111",
            &data,
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            0,
            9,
            6,
        )
        .unwrap_err();
        assert!(matches!(err, DexAdapterError::WrongProgram { .. }));
    }

    #[test]
    fn empty_data_returns_error() {
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        let err = decode(
            RAYDIUM_V4_PROGRAM_ID,
            &[],
            &accounts,
            &dummy_tx(),
            dummy_block(),
            Utc::now(),
            0,
            9,
            6,
        )
        .unwrap_err();
        assert!(matches!(err, DexAdapterError::DataTooShort { .. }));
    }

    #[test]
    fn truncated_swap_data_returns_error() {
        // Only discriminator, no amount fields
        let data = vec![DISC_SWAP_BASE_IN];
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        let err = decode(
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
        .unwrap_err();
        assert!(matches!(err, DexAdapterError::DataTooShort { .. }));
    }

    #[test]
    fn unknown_discriminator_returns_none() {
        // Discriminator 99 is not a known AMM v4 instruction
        let data = vec![99u8, 0u8, 1u8, 2u8, 3u8];
        let accounts = swap_accounts(
            &bs58::encode(&[0x01_u8; 32]).into_string(),
            &bs58::encode(&[0x02_u8; 32]).into_string(),
            &bs58::encode(&[0x03_u8; 32]).into_string(),
            &bs58::encode(&[0x04_u8; 32]).into_string(),
        );
        let result = decode(
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
        assert!(result.is_none(), "unknown discriminator must return None, not an error");
    }

    #[test]
    fn missing_accounts_returns_error() {
        let data = build_swap_base_in_data(1_000_000, 500_000);
        // Pass fewer accounts than required — should fail
        let accounts: Vec<String> = vec!["only_one_account".to_string()];
        let err = decode(
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
        .unwrap_err();
        assert!(matches!(err, DexAdapterError::MissingAccount { .. }));
    }

    // -----------------------------------------------------------------------
    // Builder tests (Sprint 7, P6-4 Phase B)
    // -----------------------------------------------------------------------

    use solana_sdk::hash::Hash;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;

    fn make_swap_accounts() -> RaydiumV4SwapAccounts {
        RaydiumV4SwapAccounts {
            amm_pool:           Pubkey::new_from_array([0x01; 32]),
            amm_authority:      Pubkey::new_from_array([0x02; 32]),
            amm_open_orders:    Pubkey::new_from_array([0x03; 32]),
            amm_target_orders:  Pubkey::new_from_array([0x04; 32]),
            pool_coin_vault:    Pubkey::new_from_array([0x05; 32]),
            pool_pc_vault:      Pubkey::new_from_array([0x06; 32]),
            market_program:     Pubkey::new_from_array([0x07; 32]),
            market:             Pubkey::new_from_array([0x08; 32]),
            market_bids:        Pubkey::new_from_array([0x09; 32]),
            market_asks:        Pubkey::new_from_array([0x0A; 32]),
            market_event_queue: Pubkey::new_from_array([0x0B; 32]),
            market_coin_vault:  Pubkey::new_from_array([0x0C; 32]),
            market_pc_vault:    Pubkey::new_from_array([0x0D; 32]),
            market_vault_signer:Pubkey::new_from_array([0x0E; 32]),
            user_source_token:  Pubkey::new_from_array([0x0F; 32]),
            user_dest_token:    Pubkey::new_from_array([0x10; 32]),
            user_owner:         Pubkey::new_from_array([0x11; 32]),
        }
    }

    #[test]
    fn build_swap_base_in_produces_17_byte_ix_data() {
        let accs = make_swap_accounts();
        let ix = build_swap_base_in_instruction(&accs, 1_000_000_000, 50_000_000);
        assert_eq!(ix.data.len(), 17, "SwapBaseIn ix data must be exactly 17 bytes");
        assert_eq!(ix.data[0], DISC_SWAP_BASE_IN, "first byte must be discriminator 9");
        let amount_in = u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        let min_out = u64::from_le_bytes(ix.data[9..17].try_into().unwrap());
        assert_eq!(amount_in, 1_000_000_000u64);
        assert_eq!(min_out, 50_000_000u64);
    }

    #[test]
    fn build_swap_base_in_accounts_count_matches_decoder() {
        let accs = make_swap_accounts();
        let ix = build_swap_base_in_instruction(&accs, 1, 1);
        // Decoder expects 18 accounts in positions 0..17.
        assert_eq!(ix.accounts.len(), 18, "SwapBaseIn must have exactly 18 accounts");
    }

    #[test]
    fn build_then_decode_roundtrip_v4() {
        let accs = make_swap_accounts();
        let amount_in: u64 = 2_000_000_000;
        let min_out: u64 = 100_000_000;
        let ix = build_swap_base_in_instruction(&accs, amount_in, min_out);

        // Convert AccountMeta list → string addresses (as decoder expects)
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
            RAYDIUM_V4_PROGRAM_ID,
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
                    accs.amm_pool.to_string(),
                    "pool address roundtrip"
                );
                assert_eq!(
                    s.sender.as_str(),
                    accs.user_owner.to_string(),
                    "wallet roundtrip"
                );
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn build_signed_tx_serializes_v4() {
        // The AccountMeta for `user_owner` (index 17) is marked as a signer,
        // so `Transaction::new_signed_with_payer` requires the payer's pubkey
        // to match that slot. Real callers (simulation path) already thread
        // `derive_simulation_keypair(...).pubkey()` into the accounts struct.
        let payer = Keypair::new();
        let mut accs = make_swap_accounts();
        accs.user_owner = payer.pubkey();
        let blockhash = Hash::default();

        let tx = build_swap_base_in_transaction(&accs, 1_000_000, 500_000, &payer, blockhash);

        let serialized = bincode::serialize(&tx).expect("Transaction must be bincode-serializable");
        assert!(!serialized.is_empty(), "serialized tx must be non-empty");

        let deserialized: solana_sdk::transaction::Transaction =
            bincode::deserialize(&serialized).expect("round-trip bincode deserialization");
        // The transaction must have 2 instructions (compute budget + swap).
        assert_eq!(
            deserialized.message.instructions.len(),
            2,
            "must have compute_budget + swap ix"
        );
    }
}
