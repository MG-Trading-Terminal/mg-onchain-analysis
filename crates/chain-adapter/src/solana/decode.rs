//! Transaction message → `common` event type decoder.
//!
//! # Scope
//!
//! This module decodes Yellowstone `SubscribeUpdateTransactionInfo` messages into
//! `Transfer`, `Swap`, and `PoolEvent` values from `crates/common`.
//!
//! **What this module does NOT do** (by design):
//! - DEX-specific pool state reconstruction (Raydium reserves, Orca tick state) —
//!   that belongs in `crates/dex-adapter` (Phase 2 Task).
//! - Token-2022 transfer hook analysis — hooks require the hook program's bytecode
//!   or a simulation call. Phase 1 emits `Transfer` with the raw amounts and
//!   `token_program = Token-2022` as a flag; consumers can detect the hook via
//!   the `token_program` field on `TokenMeta`. See `FLAG: TOKEN_2022_HOOK_ANALYSIS`
//!   below.
//! - Full `TokenMeta` population — `symbol`, `name`, `top_holders`, etc. require
//!   RPC enrichment calls that belong in `token-registry`. This module emits a
//!   minimal `TokenMeta` with only stream-visible fields populated.
//!
//! # SPL Token instruction decoding
//!
//! SPL Token instructions do not emit Solana "events" the way EVM emits logs.
//! Instead, we parse the instruction data directly:
//! - Instruction discriminator is the first byte of `instruction.data`.
//! - SPL Token `Transfer` = discriminator 3; `TransferChecked` = 12.
//! - SPL Token `MintTo` = discriminator 7; `Burn` = 8.
//! - For `TransferChecked` / `MintToChecked` / `BurnChecked`, the amount is at bytes [1..9] (u64 LE).
//! - For `Transfer`, amount is at bytes [1..9] (u64 LE) with no decimals field.
//!
//! See: https://github.com/solana-program/token/blob/main/program/src/instruction.rs
//!
//! # DEX swap heuristic
//!
//! A transaction that touches a known DEX program AND involves both a token transfer
//! in and a token transfer out is tagged as a `Swap`. The `dex` field is set from
//! the program ID lookup table. Full Raydium/Orca event decoding is deferred to
//! `crates/dex-adapter`.
//!
//! # FLAG: TOKEN_2022_HOOK_ANALYSIS
//! Token-2022 transfer hooks are detected by the presence of the hook program in
//! the inner instructions list. Phase 1 does NOT decode hook output — we emit the
//! `Transfer` with the outer amounts and set a note in the doc. Full hook analysis
//! is deferred to Phase 2 or later. Track this gap in `REFERENCES.md`.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use mg_solana_types::Pubkey;
use tracing::warn;

use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
use mg_onchain_common::event::{DexKind, Swap, Transfer};
use mg_onchain_common::token::{JupiterVerification, TokenMeta};
use rust_decimal::Decimal;

use mg_onchain_dex_adapter::{DecodedEvent, SolanaDexDecoder, DexAdapter};

use crate::error::AdapterError;
use crate::Event;

// ---------------------------------------------------------------------------
// Well-known program IDs (Base58)
// ---------------------------------------------------------------------------

const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

/// Raydium AMM v4 program ID.
const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
/// Raydium CLMM program ID.
const RAYDIUM_CLMM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
/// Orca Whirlpool program ID.
const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
/// Meteora DLMM program ID.
const METEORA_DLMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
/// PumpFun program ID.
const PUMP_FUN: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

// SPL Token instruction discriminators (first byte of instruction data)
const SPL_IX_TRANSFER: u8 = 3;
const SPL_IX_MINT_TO: u8 = 7;
const SPL_IX_BURN: u8 = 8;
const SPL_IX_TRANSFER_CHECKED: u8 = 12;
const SPL_IX_MINT_TO_CHECKED: u8 = 14;
const SPL_IX_BURN_CHECKED: u8 = 15;

// ---------------------------------------------------------------------------
// Solana zero/null address (System Program is used as the null marker)
// ---------------------------------------------------------------------------

const ZERO_ADDRESS: &str = SYSTEM_PROGRAM;

// ---------------------------------------------------------------------------
// Public decode entry point
// ---------------------------------------------------------------------------

/// Inputs to the decoder, extracted from a Yellowstone `SubscribeUpdateTransactionInfo`.
///
/// Passed as a single struct to avoid a wide argument list.
pub struct TxDecodeInput<'a> {
    pub slot: u64,
    pub block_time: Option<i64>,
    /// Base58-encoded transaction signature.
    pub signature: &'a str,
    /// All account keys for the transaction (from `message.account_keys` + loaded writable/readonly).
    pub account_keys: &'a [Pubkey],
    /// Outer instructions from the transaction message.
    pub instructions: &'a [SplInstruction],
    /// Inner instructions (CPI calls), grouped by outer instruction index.
    pub inner_instructions: &'a HashMap<u32, Vec<SplInstruction>>,
}

/// A simplified instruction representation after resolving account keys.
pub struct SplInstruction {
    /// Base58 program ID.
    pub program_id: String,
    /// Base58 account addresses in instruction order.
    pub accounts: Vec<String>,
    /// Raw instruction data bytes.
    pub data: Vec<u8>,
}

/// Decode a transaction into zero or more `Event` values.
///
/// Returns an ordered list of events. The list may be empty if the transaction
/// contains no relevant instructions (e.g., vote transactions, system-only txs).
///
/// On per-instruction decode failure: logs a warning and skips the instruction.
/// The function always returns `Ok(_)` unless the transaction-level input is invalid
/// (e.g., missing signature bytes).
pub fn decode_transaction(input: &TxDecodeInput<'_>) -> Result<Vec<Event>, AdapterError> {
    let block_ref = BlockRef::new(Chain::Solana, input.slot);
    let block_time = resolve_block_time(input.block_time);

    let tx_hash = TxHash::solana_from_base58(input.signature).map_err(|e| {
        AdapterError::DecodeError {
            context: "decode_transaction",
            reason: format!("invalid signature '{}': {e}", input.signature),
        }
    })?;

    // Collect events from all outer instructions + their inner instructions.
    let mut events: Vec<Event> = Vec::new();
    // Fallback swap heuristic state: only used if dex-adapter returns None for a
    // recognised DEX program (e.g. admin instructions, unknown discriminators).
    let mut seen_dex: Option<DexKind> = None;
    let mut transfers_in_tx: Vec<Transfer> = Vec::new();

    let dex_decoder = SolanaDexDecoder;

    for (outer_idx, ix) in input.instructions.iter().enumerate() {
        // --- DEX instruction decoding via crates/dex-adapter ---
        //
        // For every instruction belonging to a known DEX program, call the
        // typed decoder. If it returns a Swap or PoolEvent, push it directly.
        // If it returns None (admin/unknown discriminator), fall through to
        // the heuristic below.
        if is_dex_program(&ix.program_id) {
            seen_dex = program_to_dex(&ix.program_id);

            // Convert accounts slice to owned Vec<String> for the decoder.
            // The decoder is allocation-light; this is per-instruction, not per-slot.
            let account_strs: Vec<String> = ix.accounts.iter().map(String::clone).collect();

            match dex_decoder.decode(
                &ix.program_id,
                &ix.data,
                &account_strs,
                &tx_hash,
                block_ref,
                block_time,
                outer_idx as u32,
                0, // decimals_in: sentinel 0 — token-registry enriches post-emission
                0, // decimals_out: sentinel 0
            ) {
                Ok(decoded) => {
                    for ev in decoded {
                        match ev {
                            DecodedEvent::Swap(s) => events.push(Event::Swap(s)),
                            DecodedEvent::PoolEvent(pe) => events.push(Event::PoolEvent(pe)),
                        }
                    }
                }
                Err(e) => {
                    // Decode errors are warn-and-skip: malformed ix data should not
                    // crash the whole transaction decode. The heuristic below will
                    // still fire if there are transfers.
                    warn!(
                        slot = input.slot,
                        sig = input.signature,
                        outer_ix = outer_idx,
                        program = %ix.program_id,
                        error = %e,
                        "dex-adapter decode error — falling back to swap heuristic"
                    );
                }
            }
        }

        // Decode native SOL transfers (System Program Transfer instruction).
        //
        // System Program `Transfer` instruction layout:
        //   instruction_type (4 bytes, LE u32) = 2
        //   lamports         (8 bytes, LE u64)
        // Accounts: [from_wallet, to_wallet]
        //
        // Emitted as a Transfer with `token = SYSTEM_PROGRAM` so the graph
        // indexer can filter by `token = '11111111111111111111111111111111'`.
        // This is the OQ1 resolution for crates/graph MVP.
        if ix.program_id == SYSTEM_PROGRAM {
            match decode_system_transfer(
                ix,
                &tx_hash,
                &block_ref,
                block_time,
                outer_idx as u32,
            ) {
                Ok(Some(transfer)) => {
                    transfers_in_tx.push(transfer);
                }
                Ok(None) => {} // non-Transfer System Program instruction
                Err(e) if e.is_skippable() => {
                    warn!(
                        slot = input.slot,
                        sig = input.signature,
                        outer_ix = outer_idx,
                        error = %e,
                        "skipping System Program instruction decode error"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        // Decode SPL token instructions (transfers, mints, burns).
        if ix.program_id == SPL_TOKEN_PROGRAM || ix.program_id == TOKEN_2022_PROGRAM {
            match decode_spl_instruction(
                ix,
                &tx_hash,
                &block_ref,
                block_time,
                outer_idx as u32,
                outer_idx as u32,
                &ix.program_id,
            ) {
                Ok(Some(transfer)) => {
                    transfers_in_tx.push(transfer);
                }
                Ok(None) => {} // non-transfer SPL instruction (InitializeMint, etc.)
                Err(e) if e.is_skippable() => {
                    warn!(
                        slot = input.slot,
                        sig = input.signature,
                        outer_ix = outer_idx,
                        error = %e,
                        "skipping outer SPL instruction decode error"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        // Decode inner instructions (CPI) for this outer instruction.
        if let Some(inner_ixs) = input.inner_instructions.get(&(outer_idx as u32)) {
            for (inner_idx, inner_ix) in inner_ixs.iter().enumerate() {
                if inner_ix.program_id == SPL_TOKEN_PROGRAM || inner_ix.program_id == TOKEN_2022_PROGRAM {
                    let log_index = (outer_idx as u32) * 1000 + inner_idx as u32;
                    match decode_spl_instruction(
                        inner_ix,
                        &tx_hash,
                        &block_ref,
                        block_time,
                        log_index,
                        outer_idx as u32,
                        &inner_ix.program_id,
                    ) {
                        Ok(Some(transfer)) => {
                            transfers_in_tx.push(transfer);
                        }
                        Ok(None) => {}
                        Err(e) if e.is_skippable() => {
                            warn!(
                                slot = input.slot,
                                sig = input.signature,
                                inner_ix = inner_idx,
                                error = %e,
                                "skipping inner SPL instruction decode error"
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }

    // Fallback swap heuristic: fires only when the transaction touched a known
    // DEX program BUT the dex-adapter could not produce a Swap (admin instruction,
    // unknown discriminator) AND there are 2+ SPL transfers.
    //
    // This preserves the original behaviour for DEX programs not yet decoded
    // (PumpFun, Orca, Meteora) while the dex-adapter handles Raydium v4/CPMM.
    //
    // When the dex-adapter already produced a Swap for this transaction, `events`
    // already contains it — we check for that to avoid duplicates.
    let already_has_swap = events.iter().any(|e| matches!(e, Event::Swap(_)));
    if !already_has_swap
        && let Some(dex) = seen_dex
        && transfers_in_tx.len() >= 2
    {
        let t_in = &transfers_in_tx[0];
        let t_out = &transfers_in_tx[1];
        let swap = Swap {
            chain: Chain::Solana,
            tx_hash: tx_hash.clone(),
            block: block_ref,
            block_time,
            // Pool address unknown at heuristic level — use token as proxy.
            // dex-adapter enriches this for Raydium; heuristic is fallback only.
            pool: t_in.token.clone(),
            dex,
            sender: t_in.from.clone(),
            token_in: t_in.token.clone(),
            token_out: t_out.token.clone(),
            amount_in_raw: t_in.amount_raw,
            decimals_in: t_in.decimals,
            amount_out_raw: t_out.amount_raw,
            decimals_out: t_out.decimals,
            usd_value: None,
            log_index: 0,
        };
        events.push(Event::Swap(swap));
    }

    // Emit all transfers.
    for transfer in transfers_in_tx {
        events.push(Event::Transfer(transfer));
    }

    Ok(events)
}

/// Decode a single SPL Token or Token-2022 instruction into a `Transfer`.
///
/// Returns:
/// - `Ok(Some(Transfer))` for transfer/mint/burn instructions.
/// - `Ok(None)` for non-transfer instructions (InitializeMint, Approve, etc.).
/// - `Err(AdapterError::MissingField)` if required accounts are absent.
/// - `Err(AdapterError::DecodeError)` if data bytes are malformed.
fn decode_spl_instruction(
    ix: &SplInstruction,
    tx_hash: &TxHash,
    block_ref: &BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
    _outer_ix_index: u32,
    _program_id: &str,
) -> Result<Option<Transfer>, AdapterError> {
    if ix.data.is_empty() {
        return Err(AdapterError::DecodeError {
            context: "decode_spl_instruction",
            reason: "empty instruction data".into(),
        });
    }

    let discriminator = ix.data[0];

    match discriminator {
        SPL_IX_TRANSFER => {
            // Transfer: [discriminator(1), amount(8 LE)]
            // Accounts: [source, destination, authority]
            let amount_raw = read_u64_le(&ix.data, 1, "Transfer.amount")? as u128;
            let source = get_account(&ix.accounts, 0, "Transfer.source")?;
            let dest = get_account(&ix.accounts, 1, "Transfer.destination")?;

            // For plain Transfer, we don't have the mint address directly in accounts.
            // Use a placeholder — full mint resolution requires account lookup.
            // `crates/token-registry` will fill this in.
            // We still emit the transfer with source/dest for wallet-graph purposes.
            let from = parse_solana_addr(source)?;
            let to = parse_solana_addr(dest)?;

            // Decimals unknown for plain Transfer (no decimals in instruction).
            // Use 0 as sentinel; token-registry enriches this.
            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                // Mint unknown for plain Transfer — use source account as placeholder.
                // This is a known gap; token-registry resolves source → mint.
                token: from.clone(),
                from,
                to,
                amount_raw,
                decimals: 0, // sentinel — must be enriched by token-registry
                log_index,
            };
            Ok(Some(transfer))
        }

        SPL_IX_TRANSFER_CHECKED => {
            // TransferChecked: [discriminator(1), amount(8 LE), decimals(1)]
            // Accounts: [source, mint, destination, authority]
            let amount_raw = read_u64_le(&ix.data, 1, "TransferChecked.amount")? as u128;
            let decimals = read_u8(&ix.data, 9, "TransferChecked.decimals")?;
            let source = get_account(&ix.accounts, 0, "TransferChecked.source")?;
            let mint = get_account(&ix.accounts, 1, "TransferChecked.mint")?;
            let dest = get_account(&ix.accounts, 2, "TransferChecked.destination")?;

            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                token: parse_solana_addr(mint)?,
                from: parse_solana_addr(source)?,
                to: parse_solana_addr(dest)?,
                amount_raw,
                decimals,
                log_index,
            };
            Ok(Some(transfer))
        }

        SPL_IX_MINT_TO => {
            // MintTo: [discriminator(1), amount(8 LE)]
            // Accounts: [mint, destination, mint_authority]
            let amount_raw = read_u64_le(&ix.data, 1, "MintTo.amount")? as u128;
            let mint = get_account(&ix.accounts, 0, "MintTo.mint")?;
            let dest = get_account(&ix.accounts, 1, "MintTo.destination")?;

            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                token: parse_solana_addr(mint)?,
                from: parse_solana_addr(ZERO_ADDRESS)?,
                to: parse_solana_addr(dest)?,
                amount_raw,
                decimals: 0,
                log_index,
            };
            Ok(Some(transfer))
        }

        SPL_IX_MINT_TO_CHECKED => {
            // MintToChecked: [discriminator(1), amount(8 LE), decimals(1)]
            // Accounts: [mint, destination, mint_authority]
            let amount_raw = read_u64_le(&ix.data, 1, "MintToChecked.amount")? as u128;
            let decimals = read_u8(&ix.data, 9, "MintToChecked.decimals")?;
            let mint = get_account(&ix.accounts, 0, "MintToChecked.mint")?;
            let dest = get_account(&ix.accounts, 1, "MintToChecked.destination")?;

            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                token: parse_solana_addr(mint)?,
                from: parse_solana_addr(ZERO_ADDRESS)?,
                to: parse_solana_addr(dest)?,
                amount_raw,
                decimals,
                log_index,
            };
            Ok(Some(transfer))
        }

        SPL_IX_BURN => {
            // Burn: [discriminator(1), amount(8 LE)]
            // Accounts: [source_account, mint, authority]
            let amount_raw = read_u64_le(&ix.data, 1, "Burn.amount")? as u128;
            let source = get_account(&ix.accounts, 0, "Burn.source_account")?;
            let mint = get_account(&ix.accounts, 1, "Burn.mint")?;

            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                token: parse_solana_addr(mint)?,
                from: parse_solana_addr(source)?,
                to: parse_solana_addr(ZERO_ADDRESS)?,
                amount_raw,
                decimals: 0,
                log_index,
            };
            Ok(Some(transfer))
        }

        SPL_IX_BURN_CHECKED => {
            // BurnChecked: [discriminator(1), amount(8 LE), decimals(1)]
            // Accounts: [source_account, mint, authority]
            let amount_raw = read_u64_le(&ix.data, 1, "BurnChecked.amount")? as u128;
            let decimals = read_u8(&ix.data, 9, "BurnChecked.decimals")?;
            let source = get_account(&ix.accounts, 0, "BurnChecked.source_account")?;
            let mint = get_account(&ix.accounts, 1, "BurnChecked.mint")?;

            let transfer = Transfer {
                chain: Chain::Solana,
                tx_hash: tx_hash.clone(),
                block: *block_ref,
                block_time,
                token: parse_solana_addr(mint)?,
                from: parse_solana_addr(source)?,
                to: parse_solana_addr(ZERO_ADDRESS)?,
                amount_raw,
                decimals,
                log_index,
            };
            Ok(Some(transfer))
        }

        // Non-transfer SPL instructions: InitializeMint, InitializeAccount, Approve,
        // Revoke, SetAuthority, CloseAccount, FreezeAccount, ThawAccount, etc.
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// System Program Transfer decoder (OQ1 — native SOL for crates/graph)
// ---------------------------------------------------------------------------

/// System Program instruction type discriminants (4-byte LE u32 at byte 0).
const SYSTEM_IX_TRANSFER: u32 = 2;

/// Decode a System Program `Transfer` instruction into a native SOL `Transfer`.
///
/// # Layout
///
/// ```text
/// bytes [0..4]  — instruction_type (u32 LE) = 2 for Transfer
/// bytes [4..12] — lamports (u64 LE)
/// accounts[0]   — from_wallet (SOL payer)
/// accounts[1]   — to_wallet  (SOL recipient)
/// ```
///
/// # Token convention
///
/// The `token` field is set to `SYSTEM_PROGRAM` (`11111111111111111111111111111111`).
/// This is the convention used by `GraphIndexer::index_sol_transfers` to filter
/// native SOL transfers from the `transfers` table. See OQ1 resolution in
/// `crates/graph/src/edges.rs`.
///
/// # Returns
///
/// - `Ok(Some(Transfer))` for System Program Transfer instructions.
/// - `Ok(None)` for other System Program instruction types (CreateAccount, etc.).
/// - `Err` if the data or accounts are malformed.
fn decode_system_transfer(
    ix: &SplInstruction,
    tx_hash: &TxHash,
    block_ref: &BlockRef,
    block_time: chrono::DateTime<chrono::Utc>,
    log_index: u32,
) -> Result<Option<Transfer>, AdapterError> {
    if ix.data.len() < 4 {
        // Too short to contain an instruction_type; not a Transfer.
        return Ok(None);
    }

    let instruction_type = u32::from_le_bytes(
        ix.data[0..4].try_into().map_err(|_| AdapterError::DecodeError {
            context: "decode_system_transfer",
            reason: "failed to read instruction_type bytes".into(),
        })?,
    );

    if instruction_type != SYSTEM_IX_TRANSFER {
        return Ok(None);
    }

    let lamports = read_u64_le(&ix.data, 4, "SystemTransfer.lamports")? as u128;

    let from = get_account(&ix.accounts, 0, "SystemTransfer.from")?;
    let to = get_account(&ix.accounts, 1, "SystemTransfer.to")?;

    // Skip self-transfers (lamport recycling / noop patterns).
    if from == to {
        return Ok(None);
    }

    let transfer = Transfer {
        chain: mg_onchain_common::chain::Chain::Solana,
        tx_hash: tx_hash.clone(),
        block: *block_ref,
        block_time,
        // token = System Program address — convention for native SOL in transfers table.
        token: parse_solana_addr(SYSTEM_PROGRAM)?,
        from: parse_solana_addr(from)?,
        to: parse_solana_addr(to)?,
        amount_raw: lamports,
        decimals: 9, // SOL has 9 decimal places (1 SOL = 1e9 lamports)
        log_index,
    };
    Ok(Some(transfer))
}

// ---------------------------------------------------------------------------
// Minimal TokenMeta constructor from stream data
// ---------------------------------------------------------------------------

/// Build a minimal `TokenMeta` from stream-observable fields.
///
/// Called when a previously-unseen mint address appears in a `TransferChecked`
/// or `MintToChecked` instruction. Fields that require RPC enrichment (`symbol`,
/// `name`, `top_holders`, `markets`, etc.) are left empty / None.
///
/// The `token-registry` crate (Task in Phase 2) enriches these fields
/// asynchronously. The emitted `TokenMeta` is a best-effort partial record
/// that gives consumers early visibility into a new token.
pub fn minimal_token_meta(
    mint: &str,
    decimals: u8,
    _slot: u64,
    block_time: DateTime<Utc>,
    token_program: &str,
) -> Result<mg_onchain_common::token::TokenMeta, AdapterError> {
    let mint_addr = parse_solana_addr(mint)?;
    let token_program_addr = parse_solana_addr(token_program)?;

    Ok(TokenMeta {
        mint: mint_addr,
        chain: Chain::Solana,
        symbol: None,
        name: None,
        decimals,
        token_program: Some(token_program_addr),
        total_supply_raw: 0,        // requires RPC call — enriched by token-registry
        circulating_supply_raw: None,
        mint_authority: None,       // requires account state RPC call
        freeze_authority: None,
        creator: None,
        creator_balance_raw: 0,
        transfer_fee: None,
        permanent_delegate: None,   // Token-2022 TLV decoder deferred to Phase 3
        transfer_hook_program: None, // Token-2022 TLV decoder deferred to Phase 3
        non_transferable: false,     // Token-2022 ext 9 — populated by token-registry enrichment
        confidential_transfer: false, // Token-2022 ext 4 — populated by token-registry enrichment
        top_holders: vec![],
        total_holders: 0,
        markets: vec![],
        total_market_liquidity_usd: Decimal::ZERO,
        lockers: vec![],
        graph_insiders_detected: false,
        insider_networks: vec![],
        launchpad: None,
        deploy_platform: None,
        detected_at: Some(block_time),
        rugged: false,
        verification: JupiterVerification::default(),
        rugcheck_score: None,
        buy_tax: None,
        sell_tax: None,
        transfer_tax: None,
        honeypot_flags: vec![],
        updated_at: block_time,
    })
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Map a program ID string to a `DexKind`, or return `None` for non-DEX programs.
pub fn program_to_dex(program_id: &str) -> Option<DexKind> {
    match program_id {
        RAYDIUM_V4 => Some(DexKind::RaydiumV4),
        RAYDIUM_CLMM => Some(DexKind::RaydiumClmm),
        ORCA_WHIRLPOOL => Some(DexKind::OrcaWhirlpool),
        METEORA_DLMM => Some(DexKind::Meteora),
        PUMP_FUN => Some(DexKind::PumpFun),
        _ => None,
    }
}

/// Whether this program ID is a known DEX.
pub fn is_dex_program(program_id: &str) -> bool {
    program_to_dex(program_id).is_some()
}

/// Parse a Base58 Solana address string into `common::Address`.
fn parse_solana_addr(s: &str) -> Result<Address, AdapterError> {
    Address::parse(Chain::Solana, s).map_err(|e| AdapterError::DecodeError {
        context: "parse_solana_addr",
        reason: format!("{e}"),
    })
}

/// Read 8 bytes at `offset` as a little-endian `u64`.
fn read_u64_le(data: &[u8], offset: usize, field: &'static str) -> Result<u64, AdapterError> {
    let end = offset + 8;
    if data.len() < end {
        return Err(AdapterError::DecodeError {
            context: field,
            reason: format!(
                "data too short: need {} bytes at offset {offset}, have {}",
                8,
                data.len()
            ),
        });
    }
    let arr: [u8; 8] = data[offset..end].try_into().unwrap();
    Ok(u64::from_le_bytes(arr))
}

/// Read a single byte at `offset`.
fn read_u8(data: &[u8], offset: usize, field: &'static str) -> Result<u8, AdapterError> {
    data.get(offset).copied().ok_or(AdapterError::DecodeError {
        context: field,
        reason: format!("data too short for u8 at offset {offset}: len={}", data.len()),
    })
}

/// Get the nth account address from the accounts slice.
fn get_account<'a>(accounts: &'a [String], idx: usize, field: &'static str) -> Result<&'a str, AdapterError> {
    accounts.get(idx).map(|s| s.as_str()).ok_or(AdapterError::MissingField {
        field,
        context: "SPL instruction accounts",
    })
}

/// Resolve block time from Unix timestamp, falling back to `Utc::now()` if absent.
fn resolve_block_time(block_time: Option<i64>) -> DateTime<Utc> {
    block_time
        .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
        .unwrap_or_else(Utc::now)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dummy_tx_hash() -> TxHash {
        TxHash::solana_from_base58(
            &bs58::encode(&[1u8; 64]).into_string(),
        )
        .unwrap()
    }

    fn dummy_block_ref() -> BlockRef {
        BlockRef::new(Chain::Solana, 300_000_000)
    }

    fn dummy_pubkey_str(byte: u8) -> String {
        // Create a 32-byte pubkey filled with `byte`, encode as Base58.
        bs58::encode(&[byte; 32]).into_string()
    }

    // --- program_to_dex ---

    #[test]
    fn program_to_dex_raydium_v4() {
        let dex = program_to_dex(RAYDIUM_V4).expect("Raydium v4 must map to DexKind");
        assert_eq!(dex, DexKind::RaydiumV4);
    }

    #[test]
    fn program_to_dex_orca_whirlpool() {
        let dex = program_to_dex(ORCA_WHIRLPOOL).expect("Orca must map");
        assert_eq!(dex, DexKind::OrcaWhirlpool);
    }

    #[test]
    fn program_to_dex_unknown_program_returns_none() {
        assert!(program_to_dex("SomeRandomProgram111111111111111111111111").is_none());
    }

    // --- read_u64_le ---

    #[test]
    fn read_u64_le_correct() {
        let bytes: Vec<u8> = vec![0u8, 1, 0, 0, 0, 0, 0, 0, 0]; // [discriminator=0, amount=1 LE]
        let val = read_u64_le(&bytes, 1, "test").unwrap();
        assert_eq!(val, 1);
    }

    #[test]
    fn read_u64_le_large_amount() {
        let amount: u64 = 1_000_000_000; // 1 SOL in lamports
        let mut bytes = vec![3u8]; // Transfer discriminator
        bytes.extend_from_slice(&amount.to_le_bytes());
        let val = read_u64_le(&bytes, 1, "Transfer.amount").unwrap();
        assert_eq!(val, amount);
    }

    #[test]
    fn read_u64_le_short_data_errors() {
        let bytes = vec![3u8, 0u8, 0u8]; // Too short for u64
        assert!(read_u64_le(&bytes, 1, "test").is_err());
    }

    // --- decode_spl_instruction: TransferChecked ---

    #[test]
    fn decode_transfer_checked_success() {
        let amount: u64 = 500_000_000;
        let mut data = vec![SPL_IX_TRANSFER_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(9u8); // decimals = 9

        let source = dummy_pubkey_str(0x01);
        let mint = dummy_pubkey_str(0x02);
        let dest = dummy_pubkey_str(0x03);

        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![source.clone(), mint.clone(), dest.clone(), dummy_pubkey_str(0x04)],
            data,
        };

        let tx_hash = dummy_tx_hash();
        let block_ref = dummy_block_ref();
        let block_time = Utc::now();

        let result = decode_spl_instruction(
            &ix,
            &tx_hash,
            &block_ref,
            block_time,
            0,
            0,
            SPL_TOKEN_PROGRAM,
        )
        .unwrap()
        .expect("should produce a Transfer");

        assert_eq!(result.amount_raw, 500_000_000u128);
        assert_eq!(result.decimals, 9);
        assert_eq!(result.token.as_str(), mint);
        assert_eq!(result.from.as_str(), source);
        assert_eq!(result.to.as_str(), dest);
        assert!(!result.is_mint());
        assert!(!result.is_burn());
    }

    // --- decode_spl_instruction: MintToChecked ---

    #[test]
    fn decode_mint_to_checked_is_mint() {
        let amount: u64 = 1_000_000;
        let mut data = vec![SPL_IX_MINT_TO_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(6u8); // decimals = 6

        let mint = dummy_pubkey_str(0x10);
        let dest = dummy_pubkey_str(0x11);
        let authority = dummy_pubkey_str(0x12);

        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![mint.clone(), dest.clone(), authority],
            data,
        };

        let result = decode_spl_instruction(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
            0,
            SPL_TOKEN_PROGRAM,
        )
        .unwrap()
        .expect("should produce a Transfer");

        assert!(result.is_mint(), "MintToChecked must produce a mint Transfer");
        assert_eq!(result.amount_raw, 1_000_000u128);
        assert_eq!(result.decimals, 6);
    }

    // --- decode_spl_instruction: BurnChecked ---

    #[test]
    fn decode_burn_checked_is_burn() {
        let amount: u64 = 200_000;
        let mut data = vec![SPL_IX_BURN_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(6u8); // decimals

        let source = dummy_pubkey_str(0x20);
        let mint = dummy_pubkey_str(0x21);
        let authority = dummy_pubkey_str(0x22);

        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![source, mint.clone(), authority],
            data,
        };

        let result = decode_spl_instruction(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
            0,
            SPL_TOKEN_PROGRAM,
        )
        .unwrap()
        .expect("should produce a Transfer");

        assert!(result.is_burn(), "BurnChecked must produce a burn Transfer");
        assert_eq!(result.amount_raw, 200_000u128);
    }

    // --- decode_spl_instruction: non-transfer discriminator ---

    #[test]
    fn decode_non_transfer_instruction_returns_none() {
        // Discriminator 0 = InitializeMint — not a transfer
        let data = vec![0u8, 9u8]; // InitializeMint with 9 decimals
        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![dummy_pubkey_str(0x01), dummy_pubkey_str(0x02)],
            data,
        };
        let result = decode_spl_instruction(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
            0,
            SPL_TOKEN_PROGRAM,
        )
        .unwrap();
        assert!(result.is_none(), "non-transfer instruction must return None");
    }

    // --- decode_spl_instruction: empty data ---

    #[test]
    fn decode_empty_data_returns_error() {
        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![],
            data: vec![],
        };
        let result = decode_spl_instruction(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
            0,
            SPL_TOKEN_PROGRAM,
        );
        assert!(result.is_err());
    }

    // --- is_dex_program ---

    #[test]
    fn is_dex_program_true_for_pump_fun() {
        assert!(is_dex_program(PUMP_FUN));
    }

    #[test]
    fn is_dex_program_false_for_spl_token() {
        assert!(!is_dex_program(SPL_TOKEN_PROGRAM));
    }

    // --- minimal_token_meta ---

    #[test]
    fn minimal_token_meta_fields() {
        let mint = dummy_pubkey_str(0xAA);
        let meta = minimal_token_meta(&mint, 9, 300_000_000, Utc::now(), SPL_TOKEN_PROGRAM)
            .expect("should succeed");
        assert_eq!(meta.decimals, 9);
        assert_eq!(meta.total_supply_raw, 0); // not yet enriched
        assert!(!meta.rugged);
        assert!(meta.token_program.is_some());
    }

    // --- decode_system_transfer ---

    #[test]
    fn decode_system_transfer_success() {
        let lamports: u64 = 10_000_000; // 0.01 SOL
        let mut data = vec![0u8; 12];
        // instruction_type = 2 (Transfer) as LE u32
        data[0..4].copy_from_slice(&2u32.to_le_bytes());
        data[4..12].copy_from_slice(&lamports.to_le_bytes());

        let from = dummy_pubkey_str(0xAA);
        let to = dummy_pubkey_str(0xBB);

        let ix = SplInstruction {
            program_id: SYSTEM_PROGRAM.into(),
            accounts: vec![from.clone(), to.clone()],
            data,
        };

        let result = decode_system_transfer(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
        )
        .unwrap()
        .expect("should produce a Transfer");

        assert_eq!(result.amount_raw, 10_000_000u128);
        assert_eq!(result.decimals, 9);
        assert_eq!(result.token.as_str(), SYSTEM_PROGRAM);
        assert_eq!(result.from.as_str(), from);
        assert_eq!(result.to.as_str(), to);
        assert!(!result.is_mint());
        assert!(!result.is_burn());
    }

    #[test]
    fn decode_system_non_transfer_instruction_returns_none() {
        // instruction_type = 0 (CreateAccount) — not a Transfer
        let mut data = vec![0u8; 48]; // CreateAccount has more fields
        data[0..4].copy_from_slice(&0u32.to_le_bytes());

        let ix = SplInstruction {
            program_id: SYSTEM_PROGRAM.into(),
            accounts: vec![dummy_pubkey_str(0x01), dummy_pubkey_str(0x02)],
            data,
        };

        let result = decode_system_transfer(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
        )
        .unwrap();
        assert!(result.is_none(), "non-Transfer System instruction must return None");
    }

    #[test]
    fn decode_system_transfer_self_transfer_returns_none() {
        // from == to: self-transfer is a no-op.
        let lamports: u64 = 10_000_000;
        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&2u32.to_le_bytes());
        data[4..12].copy_from_slice(&lamports.to_le_bytes());

        let wallet = dummy_pubkey_str(0xCC);
        let ix = SplInstruction {
            program_id: SYSTEM_PROGRAM.into(),
            accounts: vec![wallet.clone(), wallet.clone()],
            data,
        };

        let result = decode_system_transfer(
            &ix,
            &dummy_tx_hash(),
            &dummy_block_ref(),
            Utc::now(),
            0,
        )
        .unwrap();
        assert!(result.is_none(), "self-transfer must return None");
    }

    // --- resolve_block_time ---

    #[test]
    fn resolve_block_time_from_unix() {
        let ts: i64 = 1_700_000_000;
        let dt = resolve_block_time(Some(ts));
        assert_eq!(dt.timestamp(), ts);
    }

    #[test]
    fn resolve_block_time_none_returns_now() {
        let before = Utc::now();
        let dt = resolve_block_time(None);
        let after = Utc::now();
        assert!(dt >= before && dt <= after);
    }

    // --- full decode_transaction (mock) ---

    #[test]
    fn decode_transaction_empty_instructions_returns_empty() {
        let input = TxDecodeInput {
            slot: 100,
            block_time: Some(1_700_000_000),
            signature: &bs58::encode(&[2u8; 64]).into_string(),
            account_keys: &[],
            instructions: &[],
            inner_instructions: &HashMap::new(),
        };
        let events = decode_transaction(&input).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn decode_transaction_with_transfer_checked_emits_transfer() {
        let amount: u64 = 1_000_000_000;
        let mut data = vec![SPL_IX_TRANSFER_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(9u8);

        let source = dummy_pubkey_str(0x01);
        let mint = dummy_pubkey_str(0x02);
        let dest = dummy_pubkey_str(0x03);

        let ix = SplInstruction {
            program_id: SPL_TOKEN_PROGRAM.into(),
            accounts: vec![source, mint, dest, dummy_pubkey_str(0x04)],
            data,
        };

        let input = TxDecodeInput {
            slot: 200,
            block_time: Some(1_700_000_000),
            signature: &bs58::encode(&[3u8; 64]).into_string(),
            account_keys: &[],
            instructions: &[ix],
            inner_instructions: &HashMap::new(),
        };

        let events = decode_transaction(&input).unwrap();
        // Must have at least one Transfer
        assert!(
            events.iter().any(|e| matches!(e, Event::Transfer(_))),
            "expected at least one Transfer event"
        );
    }
}
