//! Solana instruction and account-meta types.
//!
//! Provides the minimum instruction representation needed to build simulation
//! transactions in `dex-adapter`. These are data-only structs with no
//! cryptographic content — they carry program IDs, account keys, and
//! instruction data bytes.
//!
//! # Reference
//!
//! reference: solana_sdk::instruction::{Instruction, AccountMeta} (Apache-2.0)
//!            https://github.com/solana-labs/solana/blob/master/sdk/program/src/instruction.rs

use crate::Pubkey;

// ---------------------------------------------------------------------------
// AccountMeta
// ---------------------------------------------------------------------------

/// Metadata for a single account in a Solana instruction.
///
/// Each account in an instruction is either:
/// - Writable or read-only (`is_writable`)
/// - A required signer or not (`is_signer`)
///
/// reference: solana_sdk::instruction::AccountMeta (Apache-2.0)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountMeta {
    /// The public key of the account.
    pub pubkey: Pubkey,
    /// Whether the instruction requires this account to sign the transaction.
    pub is_signer: bool,
    /// Whether this instruction may modify the account's data or lamport balance.
    pub is_writable: bool,
}

impl AccountMeta {
    /// Create a writable account meta (may be a signer or not).
    ///
    /// reference: solana_sdk::instruction::AccountMeta::new (Apache-2.0)
    pub fn new(pubkey: Pubkey, is_signer: bool) -> Self {
        Self {
            pubkey,
            is_signer,
            is_writable: true,
        }
    }

    /// Create a read-only account meta (may be a signer or not).
    ///
    /// reference: solana_sdk::instruction::AccountMeta::new_readonly (Apache-2.0)
    pub fn new_readonly(pubkey: Pubkey, is_signer: bool) -> Self {
        Self {
            pubkey,
            is_signer,
            is_writable: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction
// ---------------------------------------------------------------------------

/// A Solana instruction specifying a program to invoke, accounts to pass, and data.
///
/// reference: solana_sdk::instruction::Instruction (Apache-2.0)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    /// Program ID of the program to invoke.
    pub program_id: Pubkey,
    /// Ordered list of account metas required by this instruction.
    pub accounts: Vec<AccountMeta>,
    /// Opaque instruction data bytes (program-specific encoding).
    pub data: Vec<u8>,
}

impl Instruction {
    /// Construct an instruction from raw data bytes and account metas.
    ///
    /// reference: solana_sdk::instruction::Instruction::new_with_bytes (Apache-2.0)
    pub fn new_with_bytes(program_id: Pubkey, data: &[u8], accounts: Vec<AccountMeta>) -> Self {
        Self {
            program_id,
            accounts,
            data: data.to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_meta_new_is_writable() {
        let pk = Pubkey::new_from_array([0x01; 32]);
        let meta = AccountMeta::new(pk, true);
        assert!(meta.is_writable);
        assert!(meta.is_signer);
        assert_eq!(meta.pubkey, pk);
    }

    #[test]
    fn account_meta_new_readonly_not_writable() {
        let pk = Pubkey::new_from_array([0x02; 32]);
        let meta = AccountMeta::new_readonly(pk, false);
        assert!(!meta.is_writable);
        assert!(!meta.is_signer);
        assert_eq!(meta.pubkey, pk);
    }

    #[test]
    fn instruction_new_with_bytes() {
        let program_id = Pubkey::new_from_array([0x10; 32]);
        let data = vec![0x01, 0x02, 0x03];
        let accounts = vec![AccountMeta::new(Pubkey::new_from_array([0x11; 32]), false)];
        let ix = Instruction::new_with_bytes(program_id, &data, accounts.clone());
        assert_eq!(ix.program_id, program_id);
        assert_eq!(ix.data, data);
        assert_eq!(ix.accounts, accounts);
    }

    #[test]
    fn instruction_clone_eq() {
        let ix = Instruction {
            program_id: Pubkey::new_from_array([0xAA; 32]),
            accounts: vec![AccountMeta::new_readonly(Pubkey::new_from_array([0xBB; 32]), false)],
            data: vec![0x9, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1],
        };
        let ix2 = ix.clone();
        assert_eq!(ix, ix2);
    }
}
