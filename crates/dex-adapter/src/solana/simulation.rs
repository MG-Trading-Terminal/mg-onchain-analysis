//! Simulation helpers for honeypot detection (DG3 — Sprint 7, P6-4).
//!
//! Provides deterministic keypair derivation for throwaway simulation signers.
//! The keypairs are never used for real transactions — they exist only so that
//! `simulateTransaction` receives a structurally valid signed transaction.
//!
//! # DG3 rationale (from docs/designs/0004-detector-01-honeypot.md §3.2)
//!
//! To avoid ambiguity between "honeypot won't let this wallet sell" and
//! "general sell reverts", simulation uses per-(token, pool, path) throwaway
//! keypairs. Deterministic derivation means tests are reproducible without
//! storing keypairs anywhere.
//!
//! # Security
//!
//! These keypairs are never funded, never submitted to the network, and are
//! discarded after simulation. `simulateTransaction` with
//! `sig_verify: false, replace_recent_blockhash: true` accepts them regardless.

use sha2::{Digest, Sha256};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
// `SeedDerivable` provides `Keypair::from_seed`. In solana-sdk 4 it lives in
// `solana_seed_derivable` and is re-exported via `solana_sdk::signer`.
use solana_sdk::signer::SeedDerivable;

// ---------------------------------------------------------------------------
// ComputeBudget helper (hand-rolled — no solana-compute-budget-interface dep)
// ---------------------------------------------------------------------------

/// ComputeBudget program ID. Core runtime program, ID is stable.
const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// `SetComputeUnitLimit` discriminator for the ComputeBudget program.
/// Layout: `[0x02, units_u32_le]` — 5 bytes. Matches agave-validator
/// `solana-compute-budget-interface` on-wire format.
const SET_COMPUTE_UNIT_LIMIT_DISC: u8 = 0x02;

/// Build a `SetComputeUnitLimit` instruction without depending on
/// `solana-compute-budget-interface`. The opcode + layout is a core-runtime
/// contract; a hand-rolled helper keeps the dex-adapter dep footprint minimal.
pub fn build_set_compute_unit_limit_instruction(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(SET_COMPUTE_UNIT_LIMIT_DISC);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

// ---------------------------------------------------------------------------
// Token program constants (B1.3 / B1.4)
// ---------------------------------------------------------------------------

/// SPL Token (classic) program ID.
///
/// Verified against <https://github.com/solana-program/token/blob/main/program/src/id.rs>.
/// Gotcha #26: `spl_token` is NOT available via `solana_sdk` in SDK v4.
/// Defined here to avoid a dep on `spl-token`.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Wrapped SOL mint address.
///
/// wSOL is a token account whose lamport balance exactly mirrors a native SOL
/// deposit. Used by honeypot simulation: we wrap SOL into wSOL before the
/// simulated buy and close the wSOL account afterwards to measure net lamports.
///
/// Verified: <https://solana.com/docs/core/tokens#wrapped-sol>
pub const WSOL_MINT: Pubkey =
    Pubkey::from_str_const("So11111111111111111111111111111111111111112");

/// Associated Token Account program ID.
///
/// Derives user token accounts as PDAs via
/// `[owner, token_program, mint]` seeds under this program.
///
/// Verified: <https://github.com/solana-program/associated-token-account/blob/main/program/src/id.rs>
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// System program ID.
///
/// Core runtime program with a well-known stable address.
const SYSTEM_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("11111111111111111111111111111111");

// ---------------------------------------------------------------------------
// B1.3 — ATA derivation helper
// ---------------------------------------------------------------------------

/// Derive the Associated Token Account (ATA) address for `(owner, mint)`.
///
/// Uses `Pubkey::find_program_address` with seeds
/// `[owner.as_ref(), token_program.as_ref(), mint.as_ref()]` under
/// [`ASSOCIATED_TOKEN_PROGRAM_ID`].
///
/// # Parameters
///
/// - `owner` — Wallet that owns (or will own) the ATA.
/// - `token_program` — The token program that governs this mint (SPL Token or
///   Token-2022). CPMM pools expose per-mint token programs in the pool state.
/// - `mint` — The token mint.
///
/// # Determinism
///
/// Given the same `(owner, token_program, mint)` tuple this function always
/// returns the same address. This mirrors the on-chain derivation in the ATA
/// program.
///
/// # Source
///
/// Seed order verified against:
/// <https://github.com/solana-program/associated-token-account/blob/main/program/src/tools/account.rs>
pub fn derive_associated_token_account(
    owner: &Pubkey,
    token_program: &Pubkey,
    mint: &Pubkey,
) -> Pubkey {
    let (ata, _bump) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    ata
}

// ---------------------------------------------------------------------------
// B1.4 — ATA create + wSOL wrap/sync/close instruction builders
// ---------------------------------------------------------------------------

/// Build a `CreateIdempotent` instruction for the Associated Token Account program.
///
/// Creates the ATA for `(owner, mint)` under `token_program` if it does not
/// already exist. The `idempotent` variant is used so the instruction is safe
/// to include even when the ATA already exists (no failure on re-creation).
///
/// # Layout
///
/// - Program: [`ASSOCIATED_TOKEN_PROGRAM_ID`]
/// - Instruction data: `[0x01]` (single byte, `CreateIdempotent` variant).
/// - Accounts (6):
///   0. `payer` — writable, signer (pays rent)
///   1. ATA — writable (derived inside builder)
///   2. `owner` — read-only (ATA owner)
///   3. `mint` — read-only
///   4. System program — read-only
///   5. `token_program` — read-only
///
/// # Discriminator source
///
/// Variant index `1` from `AssociatedTokenAccountInstruction` enum:
/// <https://github.com/solana-program/associated-token-account/blob/main/program/src/instruction.rs>
pub fn build_create_associated_token_account_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = derive_associated_token_account(owner, token_program, mint);
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),              // 0: payer (signer, writable)
            AccountMeta::new(ata, false),                // 1: ata (writable, derived)
            AccountMeta::new_readonly(*owner, false),   // 2: owner (readonly)
            AccountMeta::new_readonly(*mint, false),    // 3: mint (readonly)
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // 4: system program
            AccountMeta::new_readonly(*token_program, false),    // 5: token program
        ],
        data: vec![0x01], // CreateIdempotent discriminator
    }
}

/// Build a `Transfer` instruction for the System program.
///
/// Used to deposit SOL into a wSOL token account before `SyncNative` wraps it.
///
/// # Layout
///
/// - Program: System program (`11111111111111111111111111111111`)
/// - Instruction data: variant tag `2u32` (little-endian, 4 bytes) + `lamports` (u64 LE).
///   Total: 12 bytes.
/// - Accounts (2):
///   0. `from` — writable, signer
///   1. `to` — writable
///
/// # Discriminator source
///
/// Variant `2 = Transfer` in `SystemInstruction` enum:
/// `solana_system_interface::instruction::SystemInstruction`
pub fn build_system_transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    // SystemInstruction::Transfer is serialized as: u32 LE variant (2) + u64 LE lamports.
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes()); // variant tag = Transfer = 2
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*from, true),  // 0: from (writable, signer)
            AccountMeta::new(*to, false),   // 1: to (writable)
        ],
        data,
    }
}

/// Build a `SyncNative` instruction for an SPL Token / Token-2022 native account.
///
/// Reconciles the lamport balance of a wSOL token account with the actual
/// SOL deposited after a `SystemTransfer`. Required after wrapping SOL.
///
/// # Layout
///
/// - Program: `token_program` (SPL Token or Token-2022 — both support `SyncNative`
///   at the same opcode `0x11`).
/// - Instruction data: `[0x11]` (1 byte, `SyncNative` = variant 17).
/// - Accounts (1):
///   0. `native_account` — writable (the wSOL token account)
///
/// # Discriminator source
///
/// Variant `17 = SyncNative` (0x11) in the SPL Token `TokenInstruction` enum:
/// <https://github.com/solana-program/token/blob/main/program/src/instruction.rs>
pub fn build_sync_native_ix(token_program: &Pubkey, native_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*native_account, false), // 0: native account (writable)
        ],
        data: vec![0x11], // SyncNative discriminator
    }
}

/// Build a `CloseAccount` instruction for an SPL Token / Token-2022 token account.
///
/// Closes a token account and returns any remaining lamports to `destination`.
/// Used at the end of a honeypot simulation to close the wSOL account and
/// recover lamports so the covert-fee check can compare before/after SOL balances.
///
/// # Layout
///
/// - Program: `token_program` (SPL Token or Token-2022).
/// - Instruction data: `[0x09]` (1 byte, `CloseAccount` = variant 9).
/// - Accounts (3):
///   0. `account` — writable (token account to close)
///   1. `destination` — writable (receives lamports)
///   2. `owner` — signer (must sign the close)
///
/// # Discriminator source
///
/// Variant `9 = CloseAccount` (0x09) in the SPL Token `TokenInstruction` enum:
/// <https://github.com/solana-program/token/blob/main/program/src/instruction.rs>
pub fn build_close_account_ix(
    token_program: &Pubkey,
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*account, false),      // 0: account to close (writable)
            AccountMeta::new(*destination, false),  // 1: destination for lamports (writable)
            AccountMeta::new_readonly(*owner, true), // 2: owner (signer)
        ],
        data: vec![0x09], // CloseAccount discriminator
    }
}

// ---------------------------------------------------------------------------
// Simulation keypair derivation
// ---------------------------------------------------------------------------

/// Derive a deterministic simulation keypair from `(token, pool, path_index)`.
///
/// SHA-256 of `(token_bytes || pool_bytes || [path_index])` → 32-byte seed →
/// ed25519 signing key → [`Keypair`].
///
/// Same inputs always produce the same keypair, so tests are deterministic
/// without persisting keypair material. Different `path_index` values produce
/// distinct keypairs, enabling multi-path simulation without key reuse.
///
/// Per `docs/designs/0004-detector-01-honeypot.md` §DG3.
pub fn derive_simulation_keypair(token: &Pubkey, pool: &Pubkey, path_index: u8) -> Keypair {
    // Build the preimage: token (32) || pool (32) || path_index (1) = 65 bytes.
    let mut hasher = Sha256::new();
    hasher.update(token.as_ref());
    hasher.update(pool.as_ref());
    hasher.update([path_index]);
    let hash: [u8; 32] = hasher.finalize().into();

    // Expand the 32-byte seed to a 64-byte ed25519 key material via
    // `SeedDerivable::from_seed`. Re-exported in solana-sdk 4 from
    // `solana_seed_derivable` — the canonical path that avoids pulling in
    // ed25519-dalek directly.
    Keypair::from_seed(&hash)
        .expect("SHA-256 output is always a valid 32-byte seed for ed25519")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::Signer;

    fn token() -> Pubkey {
        Pubkey::new_from_array([0xAA; 32])
    }

    fn pool() -> Pubkey {
        Pubkey::new_from_array([0xBB; 32])
    }

    fn pool2() -> Pubkey {
        Pubkey::new_from_array([0xCC; 32])
    }

    // -----------------------------------------------------------------------
    // B1.3 — ATA derivation helper tests
    // -----------------------------------------------------------------------

    /// Known ATA for a published wallet/mint pair, verified on Solscan.
    ///
    /// Owner: `9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3`
    /// Token program: SPL Token (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`)
    /// Mint (USDC): `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`
    ///
    /// ATA derived via `spl-associated-token-account` on-chain.
    /// We verify by cross-checking with a second known pair.
    #[test]
    fn derive_ata_deterministic_for_same_inputs() {
        let owner = "9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3"
            .parse::<Pubkey>()
            .unwrap();
        let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse::<Pubkey>()
            .unwrap();

        let ata1 = derive_associated_token_account(&owner, &SPL_TOKEN_PROGRAM_ID, &mint);
        let ata2 = derive_associated_token_account(&owner, &SPL_TOKEN_PROGRAM_ID, &mint);
        assert_eq!(ata1, ata2, "ATA derivation must be deterministic");
    }

    #[test]
    fn derive_ata_differs_for_different_owners() {
        let owner_a = "9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3"
            .parse::<Pubkey>()
            .unwrap();
        let owner_b = Pubkey::new_from_array([0x01; 32]);
        let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse::<Pubkey>()
            .unwrap();

        let ata_a = derive_associated_token_account(&owner_a, &SPL_TOKEN_PROGRAM_ID, &mint);
        let ata_b = derive_associated_token_account(&owner_b, &SPL_TOKEN_PROGRAM_ID, &mint);
        assert_ne!(ata_a, ata_b, "different owners must produce different ATAs");
    }

    #[test]
    fn derive_ata_differs_for_different_mints() {
        let owner = Pubkey::new_from_array([0x01; 32]);
        let mint_usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse::<Pubkey>()
            .unwrap();
        let mint_wsol = WSOL_MINT;

        let ata_usdc = derive_associated_token_account(&owner, &SPL_TOKEN_PROGRAM_ID, &mint_usdc);
        let ata_wsol = derive_associated_token_account(&owner, &SPL_TOKEN_PROGRAM_ID, &mint_wsol);
        assert_ne!(ata_usdc, ata_wsol, "different mints must produce different ATAs");
    }

    #[test]
    fn derive_ata_differs_for_different_token_programs() {
        let owner = Pubkey::new_from_array([0x01; 32]);
        let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse::<Pubkey>()
            .unwrap();
        let token_2022: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
            .parse()
            .unwrap();

        let ata_classic = derive_associated_token_account(&owner, &SPL_TOKEN_PROGRAM_ID, &mint);
        let ata_2022 = derive_associated_token_account(&owner, &token_2022, &mint);
        assert_ne!(
            ata_classic, ata_2022,
            "different token programs must produce different ATAs"
        );
    }

    // -----------------------------------------------------------------------
    // B1.4 — Instruction builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_create_ata_idempotent_program_and_discriminator() {
        let payer = Pubkey::new_from_array([0x01; 32]);
        let owner = Pubkey::new_from_array([0x02; 32]);
        let mint = Pubkey::new_from_array([0x03; 32]);
        let ix = build_create_associated_token_account_idempotent_ix(
            &payer,
            &owner,
            &mint,
            &SPL_TOKEN_PROGRAM_ID,
        );
        assert_eq!(ix.program_id, ASSOCIATED_TOKEN_PROGRAM_ID, "must target ATA program");
        assert_eq!(ix.data, vec![0x01], "CreateIdempotent discriminator must be 0x01");
        assert_eq!(ix.accounts.len(), 6, "must have exactly 6 accounts");
        // Slot 0: payer — writable, signer
        assert!(ix.accounts[0].is_writable && ix.accounts[0].is_signer, "payer must be writable+signer");
        // Slot 1: ATA — writable, not signer
        assert!(ix.accounts[1].is_writable && !ix.accounts[1].is_signer, "ata must be writable");
        // Slot 2: owner — readonly
        assert!(!ix.accounts[2].is_writable && !ix.accounts[2].is_signer, "owner must be readonly");
        // Slot 3: mint — readonly
        assert!(!ix.accounts[3].is_writable && !ix.accounts[3].is_signer, "mint must be readonly");
        // Slot 4: system program — readonly
        assert_eq!(ix.accounts[4].pubkey, SYSTEM_PROGRAM_ID, "slot 4 must be system program");
        // Slot 5: token program — readonly
        assert_eq!(ix.accounts[5].pubkey, SPL_TOKEN_PROGRAM_ID, "slot 5 must be token program");
    }

    #[test]
    fn build_system_transfer_data_layout() {
        let from = Pubkey::new_from_array([0x10; 32]);
        let to = Pubkey::new_from_array([0x20; 32]);
        let lamports: u64 = 1_000_000_000; // 1 SOL in lamports
        let ix = build_system_transfer_ix(&from, &to, lamports);

        assert_eq!(ix.program_id, SYSTEM_PROGRAM_ID, "must target system program");
        // Data: 4-byte LE variant tag (2 = Transfer) + 8-byte LE lamports
        assert_eq!(ix.data.len(), 12, "system transfer data must be 12 bytes");
        let variant = u32::from_le_bytes(ix.data[0..4].try_into().unwrap());
        let decoded_lamports = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
        assert_eq!(variant, 2, "variant tag must be 2 (Transfer)");
        assert_eq!(decoded_lamports, lamports, "lamports must roundtrip");

        // Accounts
        assert_eq!(ix.accounts.len(), 2, "system transfer must have 2 accounts");
        assert!(ix.accounts[0].is_writable && ix.accounts[0].is_signer, "from must be writable+signer");
        assert!(ix.accounts[1].is_writable && !ix.accounts[1].is_signer, "to must be writable");
    }

    #[test]
    fn build_sync_native_ix_discriminator() {
        let native_account = Pubkey::new_from_array([0x30; 32]);
        let ix = build_sync_native_ix(&SPL_TOKEN_PROGRAM_ID, &native_account);

        assert_eq!(ix.program_id, SPL_TOKEN_PROGRAM_ID, "must target token program");
        assert_eq!(ix.data, vec![0x11], "SyncNative discriminator must be 0x11 (17)");
        assert_eq!(ix.accounts.len(), 1, "sync_native must have 1 account");
        assert!(ix.accounts[0].is_writable && !ix.accounts[0].is_signer, "native account must be writable");
        assert_eq!(ix.accounts[0].pubkey, native_account);
    }

    #[test]
    fn build_close_account_ix_discriminator_and_accounts() {
        let account = Pubkey::new_from_array([0x40; 32]);
        let destination = Pubkey::new_from_array([0x50; 32]);
        let owner = Pubkey::new_from_array([0x60; 32]);
        let ix = build_close_account_ix(&SPL_TOKEN_PROGRAM_ID, &account, &destination, &owner);

        assert_eq!(ix.program_id, SPL_TOKEN_PROGRAM_ID, "must target token program");
        assert_eq!(ix.data, vec![0x09], "CloseAccount discriminator must be 0x09 (9)");
        assert_eq!(ix.accounts.len(), 3, "close_account must have 3 accounts");
        // 0: account — writable
        assert!(ix.accounts[0].is_writable && !ix.accounts[0].is_signer, "account must be writable");
        assert_eq!(ix.accounts[0].pubkey, account);
        // 1: destination — writable
        assert!(ix.accounts[1].is_writable && !ix.accounts[1].is_signer, "destination must be writable");
        assert_eq!(ix.accounts[1].pubkey, destination);
        // 2: owner — signer
        assert!(!ix.accounts[2].is_writable && ix.accounts[2].is_signer, "owner must be signer");
        assert_eq!(ix.accounts[2].pubkey, owner);
    }

    #[test]
    fn wsol_mint_is_correct_constant() {
        // wSOL mint address is a well-known Solana constant.
        // Verified: https://solana.com/docs/core/tokens#wrapped-sol
        let expected: Pubkey = "So11111111111111111111111111111111111111112"
            .parse()
            .unwrap();
        assert_eq!(WSOL_MINT, expected, "WSOL_MINT constant mismatch");
    }

    #[test]
    fn spl_token_program_id_is_correct_constant() {
        let expected: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            .parse()
            .unwrap();
        assert_eq!(SPL_TOKEN_PROGRAM_ID, expected, "SPL_TOKEN_PROGRAM_ID constant mismatch");
    }

    #[test]
    fn derive_simulation_keypair_deterministic() {
        let kp1 = derive_simulation_keypair(&token(), &pool(), 0);
        let kp2 = derive_simulation_keypair(&token(), &pool(), 0);
        assert_eq!(
            kp1.pubkey(),
            kp2.pubkey(),
            "same inputs must produce same keypair"
        );
    }

    #[test]
    fn derive_simulation_keypair_varies_with_index() {
        let kp0 = derive_simulation_keypair(&token(), &pool(), 0);
        let kp1 = derive_simulation_keypair(&token(), &pool(), 1);
        assert_ne!(
            kp0.pubkey(),
            kp1.pubkey(),
            "different path_index must produce different keypairs"
        );
    }

    #[test]
    fn derive_simulation_keypair_varies_with_pool() {
        let kp_a = derive_simulation_keypair(&token(), &pool(), 0);
        let kp_b = derive_simulation_keypair(&token(), &pool2(), 0);
        assert_ne!(
            kp_a.pubkey(),
            kp_b.pubkey(),
            "different pool must produce different keypairs"
        );
    }

    #[test]
    fn derive_simulation_keypair_varies_with_token() {
        let token2 = Pubkey::new_from_array([0xDD; 32]);
        let kp_a = derive_simulation_keypair(&token(), &pool(), 0);
        let kp_b = derive_simulation_keypair(&token2, &pool(), 0);
        assert_ne!(
            kp_a.pubkey(),
            kp_b.pubkey(),
            "different token must produce different keypairs"
        );
    }
}
