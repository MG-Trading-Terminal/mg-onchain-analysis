//! Token-2022 instruction decoder for D07 (withdraw-withheld drain detector).
//!
//! # Scope
//!
//! Decodes four Token-2022 program instructions from raw Solana transaction data:
//!
//! | Instruction | Discriminator byte | Authority-gated? |
//! |-------------|-------------------|-----------------|
//! | `WithdrawWithheldTokensFromMint`     | 27 | Yes — `withdraw_withheld_authority` |
//! | `WithdrawWithheldTokensFromAccounts` | 28 | Yes — `withdraw_withheld_authority` |
//! | `HarvestWithheldTokensToMint`        | 29 | No (permissionless) |
//! | `SetAuthority { WithdrawWithheldTokens }` | 6 + authority_type=4 | Yes |
//!
//! Source for discriminator values:
//!   https://github.com/solana-labs/solana-program-library/blob/master/token/program-2022/src/instruction.rs
//!
//! # CPI handling (DG-D07-5 mitigation)
//!
//! Token-2022 instructions may be invoked via CPI (e.g. from Jupiter aggregator).
//! The decoder processes BOTH top-level and inner instructions. The caller must pass
//! both slices. Pre-filter: skip transactions where `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`
//! is NOT in the transaction's account keys — this avoids decoding the inner instructions
//! of unrelated transactions (DG-D07-5 performance mitigation).
//!
//! # Account layout
//!
//! ```text
//! WithdrawWithheldTokensFromMint (byte 27):
//!   accounts[0] = mint
//!   accounts[1] = destination token account
//!   accounts[2] = withdraw_withheld_authority (signer)
//!
//! WithdrawWithheldTokensFromAccounts (byte 28):
//!   accounts[0] = mint
//!   accounts[1] = destination token account
//!   accounts[2] = withdraw_withheld_authority (signer)
//!   accounts[3..N] = source token accounts with withheld balances
//!
//! HarvestWithheldTokensToMint (byte 29):
//!   accounts[0] = mint
//!   accounts[1..N] = source token accounts
//!   (no authority account — permissionless)
//!
//! SetAuthority (byte 6):
//!   data[0]    = 6 (SetAuthority discriminator)
//!   data[1]    = authority_type (4 = WithdrawWithheldTokens)
//!   data[2]    = new_authority_option (1 = Some; 0 = None/revoke)
//!   data[3..35] = new_authority pubkey (if data[2] = 1)
//!   accounts[0] = owned account (the mint)
//!   accounts[1] = current authority (signer)
//! ```
//!
//! # References
//!
//! - SPL Token-2022 instruction discriminators: spl/token/program-2022/src/instruction.rs
//! - Design contract: docs/designs/0012-detector-07-withdraw-withheld.md §11.2
//! - CPI spec: docs/designs/0012-detector-07-withdraw-withheld.md §11.3

use chrono::{DateTime, Utc};
use tracing::warn;

use mg_onchain_common::chain::{Address, BlockRef, Chain, TxHash};
use mg_onchain_common::event::{Token2022InstructionEvent, Token2022InstructionKind};

use crate::error::AdapterError;
use crate::solana::decode::SplInstruction;
use crate::Event;

// ---------------------------------------------------------------------------
// Token-2022 program ID
// ---------------------------------------------------------------------------

/// Token-2022 program ID (Base58).
///
/// Pre-filter: transactions where this address does not appear in `accountKeys`
/// MUST be skipped before decoding inner instructions (DG-D07-5 mitigation).
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// ---------------------------------------------------------------------------
// Token-2022 instruction discriminators
// ---------------------------------------------------------------------------

/// Token-2022 `WithdrawWithheldTokensFromMint` discriminator byte.
const T22_IX_WITHDRAW_FROM_MINT: u8 = 27;

/// Token-2022 `WithdrawWithheldTokensFromAccounts` discriminator byte.
const T22_IX_WITHDRAW_FROM_ACCOUNTS: u8 = 28;

/// Token-2022 `HarvestWithheldTokensToMint` discriminator byte.
const T22_IX_HARVEST_TO_MINT: u8 = 29;

/// Token-2022 `SetAuthority` discriminator byte (shared with SPL Token base).
/// Authority type byte = 4 identifies `WithdrawWithheldTokens`.
const T22_IX_SET_AUTHORITY: u8 = 6;

/// `authority_type` byte value identifying `WithdrawWithheldTokens`.
const AUTHORITY_TYPE_WITHDRAW_WITHHELD: u8 = 4;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Decode all Token-2022 withdraw-withheld relevant instructions from a transaction.
///
/// Processes both top-level instructions and inner (CPI) instructions. The caller
/// MUST have already pre-filtered the transaction to ensure `TOKEN_2022_PROGRAM`
/// appears in the transaction's `accountKeys` before calling this function.
///
/// Returns zero or more `Event::Token2022Instruction` events. Returns `Ok(vec![])`
/// if the transaction contains no relevant Token-2022 instructions.
///
/// On per-instruction decode failure: logs a warning at `WARN` level and skips
/// the instruction. The function never returns `Err` for a single-instruction
/// decode failure — only for malformed transaction-level inputs.
#[tracing::instrument(skip(instructions, inner_instructions), fields(
    chain = "solana",
    slot = block_ref.height,
    sig = %tx_hash,
))]
pub fn decode_token2022_instructions(
    tx_hash: &TxHash,
    block_ref: BlockRef,
    block_time: DateTime<Utc>,
    instructions: &[SplInstruction],
    inner_instructions: &std::collections::HashMap<u32, Vec<SplInstruction>>,
) -> Result<Vec<Event>, AdapterError> {
    let mut events = Vec::new();

    // Decode top-level instructions.
    for (outer_idx, ix) in instructions.iter().enumerate() {
        if ix.program_id != TOKEN_2022_PROGRAM {
            continue;
        }
        let log_index = outer_idx as u32;
        match decode_single_t22_instruction(ix, tx_hash, block_ref, block_time, log_index) {
            Ok(Some(ev)) => events.push(Event::Token2022Instruction(Box::new(ev))),
            Ok(None) => {}
            Err(e) => {
                warn!(
                    slot = block_ref.height,
                    sig = %tx_hash,
                    outer_ix = outer_idx,
                    error = %e,
                    "skipping Token-2022 top-level instruction decode error"
                );
            }
        }
    }

    // Decode inner (CPI) instructions.
    for (outer_idx, _ix) in instructions.iter().enumerate() {
        let Some(inner_ixs) = inner_instructions.get(&(outer_idx as u32)) else {
            continue;
        };
        for (inner_idx, inner_ix) in inner_ixs.iter().enumerate() {
            if inner_ix.program_id != TOKEN_2022_PROGRAM {
                continue;
            }
            // CPI log_index: outer_idx * 1000 + inner_idx (matching decode.rs pattern)
            let log_index = (outer_idx as u32) * 1000 + inner_idx as u32;
            match decode_single_t22_instruction(
                inner_ix,
                tx_hash,
                block_ref,
                block_time,
                log_index,
            ) {
                Ok(Some(ev)) => events.push(Event::Token2022Instruction(Box::new(ev))),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        slot = block_ref.height,
                        sig = %tx_hash,
                        outer_ix = outer_idx,
                        inner_ix = inner_idx,
                        error = %e,
                        "skipping Token-2022 CPI instruction decode error"
                    );
                }
            }
        }
    }

    Ok(events)
}

/// Decode a single Token-2022 instruction into a `Token2022InstructionEvent`.
///
/// Returns:
/// - `Ok(Some(event))` for the four relevant instruction types.
/// - `Ok(None)` for other Token-2022 instructions (not relevant to D07).
/// - `Err(AdapterError)` if the instruction data is malformed.
fn decode_single_t22_instruction(
    ix: &SplInstruction,
    tx_hash: &TxHash,
    block_ref: BlockRef,
    block_time: DateTime<Utc>,
    log_index: u32,
) -> Result<Option<Token2022InstructionEvent>, AdapterError> {
    if ix.data.is_empty() {
        return Err(AdapterError::DecodeError {
            context: "decode_single_t22_instruction",
            reason: "empty instruction data".into(),
        });
    }

    let discriminator = ix.data[0];

    match discriminator {
        T22_IX_WITHDRAW_FROM_MINT => {
            // WithdrawWithheldTokensFromMint
            // accounts[0] = mint
            // accounts[1] = destination
            // accounts[2] = authority (withdraw_withheld_authority signer)
            let mint = get_account_addr(&ix.accounts, 0, "WithdrawFromMint.mint")?;
            let destination = get_account_addr(&ix.accounts, 1, "WithdrawFromMint.destination")?;
            let authority = get_account_addr(&ix.accounts, 2, "WithdrawFromMint.authority")?;

            Ok(Some(Token2022InstructionEvent {
                chain: Chain::Solana,
                mint,
                tx_hash: tx_hash.clone(),
                block_height: block_ref.height,
                block_time,
                kind: Token2022InstructionKind::WithdrawWithheldFromMint,
                authority: Some(authority),
                destination: Some(destination),
                // amount_raw is not encoded in instruction data for this variant;
                // it must be derived from the mint account's withheld_amount delta
                // (pre vs post execution state). For MVP, set to None and let the
                // indexer populate it from account state deltas (DG-D07-2).
                amount_raw: None,
                new_authority: None,
                prev_authority: None,
                log_index,
            }))
        }

        T22_IX_WITHDRAW_FROM_ACCOUNTS => {
            // WithdrawWithheldTokensFromAccounts
            // accounts[0] = mint
            // accounts[1] = destination
            // accounts[2] = authority
            // accounts[3..N] = source accounts
            let mint = get_account_addr(&ix.accounts, 0, "WithdrawFromAccounts.mint")?;
            let destination =
                get_account_addr(&ix.accounts, 1, "WithdrawFromAccounts.destination")?;
            let authority =
                get_account_addr(&ix.accounts, 2, "WithdrawFromAccounts.authority")?;

            Ok(Some(Token2022InstructionEvent {
                chain: Chain::Solana,
                mint,
                tx_hash: tx_hash.clone(),
                block_height: block_ref.height,
                block_time,
                kind: Token2022InstructionKind::WithdrawWithheldFromAccounts,
                authority: Some(authority),
                destination: Some(destination),
                // amount_raw is not directly encoded; derive from account state deltas.
                amount_raw: None,
                new_authority: None,
                prev_authority: None,
                log_index,
            }))
        }

        T22_IX_HARVEST_TO_MINT => {
            // HarvestWithheldTokensToMint (permissionless)
            // accounts[0] = mint
            // accounts[1..N] = source token accounts
            let mint = get_account_addr(&ix.accounts, 0, "HarvestToMint.mint")?;

            Ok(Some(Token2022InstructionEvent {
                chain: Chain::Solana,
                mint,
                tx_hash: tx_hash.clone(),
                block_height: block_ref.height,
                block_time,
                kind: Token2022InstructionKind::HarvestWithheldToMint,
                authority: None, // permissionless — no authority account
                destination: None,
                amount_raw: None,
                new_authority: None,
                prev_authority: None,
                log_index,
            }))
        }

        T22_IX_SET_AUTHORITY => {
            // SetAuthority
            // data[0] = 6 (discriminator)
            // data[1] = authority_type
            // data[2] = new_authority_option (0 = None/revoke; 1 = Some)
            // data[3..35] = new_authority pubkey (if data[2] == 1)
            // accounts[0] = owned account (mint)
            // accounts[1] = current authority (signer)
            if ix.data.len() < 3 {
                return Err(AdapterError::DecodeError {
                    context: "SetAuthority",
                    reason: format!("data too short: {} bytes, need ≥3", ix.data.len()),
                });
            }

            let authority_type = ix.data[1];
            // Only handle WithdrawWithheldTokens (type = 4).
            if authority_type != AUTHORITY_TYPE_WITHDRAW_WITHHELD {
                return Ok(None);
            }

            let mint = get_account_addr(&ix.accounts, 0, "SetAuthority.mint")?;
            let current_authority =
                get_account_addr(&ix.accounts, 1, "SetAuthority.current_authority")?;

            let new_authority_opt = ix.data[2];
            let new_authority = if new_authority_opt == 1 {
                // New authority pubkey follows as 32 bytes
                if ix.data.len() < 35 {
                    return Err(AdapterError::DecodeError {
                        context: "SetAuthority.new_authority",
                        reason: format!(
                            "data too short for new pubkey: {} bytes, need 35",
                            ix.data.len()
                        ),
                    });
                }
                let pubkey_bytes: [u8; 32] = ix.data[3..35].try_into().map_err(|_| {
                    AdapterError::DecodeError {
                        context: "SetAuthority.new_authority",
                        reason: "pubkey slice conversion failed".into(),
                    }
                })?;
                let pubkey_b58 = bs58::encode(&pubkey_bytes).into_string();
                Some(parse_solana_addr(&pubkey_b58)?)
            } else {
                // Authority revoked (new_authority_option = 0)
                None
            };

            Ok(Some(Token2022InstructionEvent {
                chain: Chain::Solana,
                mint,
                tx_hash: tx_hash.clone(),
                block_height: block_ref.height,
                block_time,
                kind: Token2022InstructionKind::SetAuthorityWithdrawWithheld,
                authority: Some(current_authority.clone()),
                destination: None,
                amount_raw: None,
                new_authority,
                prev_authority: Some(current_authority),
                log_index,
            }))
        }

        // All other Token-2022 instruction types are not relevant to D07.
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Parse a Base58 Solana address string into `common::Address`.
fn parse_solana_addr(s: &str) -> Result<Address, AdapterError> {
    Address::parse(Chain::Solana, s).map_err(|e| AdapterError::DecodeError {
        context: "parse_solana_addr",
        reason: format!("{e}"),
    })
}

/// Get the nth account as an `Address`, returning an error if out of bounds.
fn get_account_addr(
    accounts: &[String],
    idx: usize,
    field: &'static str,
) -> Result<Address, AdapterError> {
    let s = accounts.get(idx).ok_or(AdapterError::MissingField {
        field,
        context: "Token-2022 instruction accounts",
    })?;
    parse_solana_addr(s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dummy_tx_hash() -> TxHash {
        TxHash::solana_from_base58(&bs58::encode(&[1u8; 64]).into_string()).unwrap()
    }

    fn dummy_block_ref() -> BlockRef {
        BlockRef::new(Chain::Solana, 310_000_000)
    }

    fn dummy_block_time() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Produce a valid Base58 pubkey string from a byte pattern.
    fn pubkey_str(byte: u8) -> String {
        bs58::encode(&[byte; 32]).into_string()
    }

    // --- WithdrawWithheldTokensFromMint (discriminator 27) ---

    #[test]
    fn decode_withdraw_from_mint_basic() {
        let mint = pubkey_str(0x01);
        let dest = pubkey_str(0x02);
        let authority = pubkey_str(0x03);

        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint.clone(), dest.clone(), authority.clone()],
            data: vec![T22_IX_WITHDRAW_FROM_MINT],
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            0,
        )
        .unwrap()
        .expect("must produce an event");

        assert_eq!(
            result.kind,
            Token2022InstructionKind::WithdrawWithheldFromMint
        );
        assert_eq!(result.mint.as_str(), mint);
        assert_eq!(result.destination.as_ref().unwrap().as_str(), dest);
        assert_eq!(result.authority.as_ref().unwrap().as_str(), authority);
        assert!(result.amount_raw.is_none());
        assert_eq!(result.log_index, 0);
    }

    // --- WithdrawWithheldTokensFromAccounts (discriminator 28) ---

    #[test]
    fn decode_withdraw_from_accounts_basic() {
        let mint = pubkey_str(0x10);
        let dest = pubkey_str(0x11);
        let authority = pubkey_str(0x12);
        let source1 = pubkey_str(0x13);
        let source2 = pubkey_str(0x14);

        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![
                mint.clone(),
                dest.clone(),
                authority.clone(),
                source1,
                source2,
            ],
            data: vec![T22_IX_WITHDRAW_FROM_ACCOUNTS],
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            5,
        )
        .unwrap()
        .expect("must produce an event");

        assert_eq!(
            result.kind,
            Token2022InstructionKind::WithdrawWithheldFromAccounts
        );
        assert_eq!(result.mint.as_str(), mint);
        assert_eq!(result.authority.as_ref().unwrap().as_str(), authority);
        assert_eq!(result.log_index, 5);
    }

    // --- HarvestWithheldTokensToMint (discriminator 29) ---

    #[test]
    fn decode_harvest_to_mint_basic() {
        let mint = pubkey_str(0x20);
        let source1 = pubkey_str(0x21);

        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint.clone(), source1],
            data: vec![T22_IX_HARVEST_TO_MINT],
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            2,
        )
        .unwrap()
        .expect("must produce an event");

        assert_eq!(result.kind, Token2022InstructionKind::HarvestWithheldToMint);
        assert_eq!(result.mint.as_str(), mint);
        // Permissionless — no authority
        assert!(result.authority.is_none());
        assert!(result.destination.is_none());
    }

    // --- SetAuthority { authority_type: WithdrawWithheldTokens } ---

    #[test]
    fn decode_set_authority_withdraw_withheld() {
        let mint = pubkey_str(0x30);
        let current_authority = pubkey_str(0x31);
        let new_authority_bytes = [0x32u8; 32];

        // data: [6, 4, 1, new_authority_bytes(32)]
        let mut data = vec![T22_IX_SET_AUTHORITY, AUTHORITY_TYPE_WITHDRAW_WITHHELD, 1u8];
        data.extend_from_slice(&new_authority_bytes);

        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint.clone(), current_authority.clone()],
            data,
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            3,
        )
        .unwrap()
        .expect("must produce an event");

        assert_eq!(
            result.kind,
            Token2022InstructionKind::SetAuthorityWithdrawWithheld
        );
        assert_eq!(result.mint.as_str(), mint);
        assert!(result.new_authority.is_some());
        assert_eq!(result.prev_authority.as_ref().unwrap().as_str(), current_authority);
        // The new authority base58 should match the 0x32 pattern bytes
        let expected_new = bs58::encode(&new_authority_bytes).into_string();
        assert_eq!(result.new_authority.as_ref().unwrap().as_str(), expected_new);
    }

    #[test]
    fn decode_set_authority_revoke() {
        let mint = pubkey_str(0x40);
        let current_authority = pubkey_str(0x41);

        // data: [6, 4, 0] — new_authority_option = 0 (revoke)
        let data = vec![T22_IX_SET_AUTHORITY, AUTHORITY_TYPE_WITHDRAW_WITHHELD, 0u8];

        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint, current_authority],
            data,
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            4,
        )
        .unwrap()
        .expect("must produce an event for authority revocation");

        assert_eq!(
            result.kind,
            Token2022InstructionKind::SetAuthorityWithdrawWithheld
        );
        // Revoked: new_authority is None
        assert!(result.new_authority.is_none());
    }

    #[test]
    fn decode_set_authority_other_type_returns_none() {
        // authority_type = 0 (MintTokens) — not WithdrawWithheldTokens — should return None
        let data = vec![T22_IX_SET_AUTHORITY, 0u8, 0u8];
        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![pubkey_str(0x50), pubkey_str(0x51)],
            data,
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            0,
        )
        .unwrap();

        assert!(
            result.is_none(),
            "SetAuthority with non-WithdrawWithheld authority type must return None"
        );
    }

    #[test]
    fn decode_unknown_discriminator_returns_none() {
        // Discriminator 0 = InitializeMint — not relevant to D07
        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![pubkey_str(0x60)],
            data: vec![0u8],
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            0,
        )
        .unwrap();

        assert!(result.is_none(), "unknown discriminator must return None");
    }

    #[test]
    fn decode_empty_data_returns_error() {
        let ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![],
            data: vec![],
        };

        let result = decode_single_t22_instruction(
            &ix,
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            0,
        );

        assert!(result.is_err(), "empty data must return error");
    }

    // --- decode_token2022_instructions: CPI handling ---

    #[test]
    fn decode_token2022_instructions_processes_top_level() {
        let mint = pubkey_str(0x70);
        let dest = pubkey_str(0x71);
        let authority = pubkey_str(0x72);

        let instructions = vec![SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint, dest, authority],
            data: vec![T22_IX_WITHDRAW_FROM_MINT],
        }];

        let events = decode_token2022_instructions(
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            &instructions,
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0], Event::Token2022Instruction(_)),
            "expected Token2022Instruction variant"
        );
    }

    #[test]
    fn decode_token2022_instructions_processes_cpi() {
        let mint = pubkey_str(0x80);
        let dest = pubkey_str(0x81);
        let authority = pubkey_str(0x82);

        // Outer instruction is some other program (e.g. Jupiter)
        let outer_ix = SplInstruction {
            program_id: "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4".into(),
            accounts: vec![],
            data: vec![99u8], // some Jupiter discriminator
        };

        // Inner (CPI) instruction is Token-2022
        let inner_ix = SplInstruction {
            program_id: TOKEN_2022_PROGRAM.into(),
            accounts: vec![mint, dest, authority],
            data: vec![T22_IX_WITHDRAW_FROM_ACCOUNTS],
        };

        let mut inner_map = HashMap::new();
        inner_map.insert(0u32, vec![inner_ix]);

        let events = decode_token2022_instructions(
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            &[outer_ix],
            &inner_map,
        )
        .unwrap();

        assert_eq!(events.len(), 1, "CPI instruction must be decoded");
        if let Event::Token2022Instruction(ev) = &events[0] {
            // CPI log_index = outer_idx(0) * 1000 + inner_idx(0) = 0
            assert_eq!(ev.log_index, 0);
            assert_eq!(
                ev.kind,
                Token2022InstructionKind::WithdrawWithheldFromAccounts
            );
        } else {
            panic!("expected Token2022Instruction event");
        }
    }

    #[test]
    fn decode_token2022_instructions_skips_non_t22_programs() {
        // Instruction for SPL Token program — should be skipped
        let ix = SplInstruction {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
            accounts: vec![pubkey_str(0x90), pubkey_str(0x91), pubkey_str(0x92)],
            data: vec![27u8], // same byte as T22_IX_WITHDRAW_FROM_MINT but wrong program
        };

        let events = decode_token2022_instructions(
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            &[ix],
            &HashMap::new(),
        )
        .unwrap();

        assert!(events.is_empty(), "non-Token-2022 program must be skipped");
    }

    #[test]
    fn decode_token2022_instructions_empty_input_returns_empty() {
        let events = decode_token2022_instructions(
            &dummy_tx_hash(),
            dummy_block_ref(),
            dummy_block_time(),
            &[],
            &HashMap::new(),
        )
        .unwrap();
        assert!(events.is_empty());
    }
}
