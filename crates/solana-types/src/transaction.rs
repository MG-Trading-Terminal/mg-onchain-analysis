//! Solana transaction and message types.
//!
//! Implements `Transaction`, `Message`, `MessageHeader`, and `CompiledInstruction`
//! matching the Solana wire-format serialisation spec.
//!
//! # Wire format
//!
//! A Solana transaction on the wire is:
//! ```text
//! compact-u16          — number of signatures
//! [u8; 64] × N        — signatures
//! MessageHeader (3 bytes)
//! compact-u16          — number of account keys
//! [u8; 32] × M        — account keys
//! [u8; 32]             — recent blockhash
//! compact-u16          — number of instructions
//! CompiledInstruction × I  — compiled instructions
//! ```
//!
//! A `CompiledInstruction` on the wire is:
//! ```text
//! u8               — program_id_index (index into account_keys)
//! compact-u16      — number of account indices
//! u8 × A           — account indices
//! compact-u16      — data length
//! u8 × D           — data bytes
//! ```
//!
//! # Reference
//!
//! reference: solana_sdk::transaction::Transaction (Apache-2.0)
//!            https://github.com/solana-labs/solana/blob/master/sdk/program/src/message/legacy.rs
//! reference: solana_sdk::message::Message (Apache-2.0)

use std::collections::HashMap;

use crate::instruction::Instruction;
use crate::wire::{WireError, decode_compact_u16, encode_compact_u16};
use crate::{Hash, Keypair, Pubkey, Signature};

// ---------------------------------------------------------------------------
// MessageHeader
// ---------------------------------------------------------------------------

/// The 3-byte header of a Solana message.
///
/// Encodes how many of the account_keys are required signers and how many
/// of those signers are read-only vs writable.
///
/// reference: solana_sdk::message::MessageHeader (Apache-2.0)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageHeader {
    /// Total number of accounts that must sign the transaction.
    pub num_required_signatures: u8,
    /// Among the required-signer accounts, how many are read-only.
    pub num_readonly_signed_accounts: u8,
    /// Among the unsigned accounts, how many are read-only.
    pub num_readonly_unsigned_accounts: u8,
}

// ---------------------------------------------------------------------------
// CompiledInstruction
// ---------------------------------------------------------------------------

/// An instruction compiled into indices into the message's `account_keys` list.
///
/// reference: solana_sdk::instruction::CompiledInstruction (Apache-2.0)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledInstruction {
    /// Index of the program ID in the message's `account_keys`.
    pub program_id_index: u8,
    /// Indices into `account_keys` for each account this instruction requires.
    pub accounts: Vec<u8>,
    /// Raw instruction data bytes.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A Solana transaction message (header + accounts + blockhash + instructions).
///
/// reference: solana_sdk::message::Message (Apache-2.0)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The message header describing signature and read-only counts.
    pub header: MessageHeader,
    /// Ordered list of all account public keys referenced in this message.
    /// The first `header.num_required_signatures` entries are signer accounts.
    pub account_keys: Vec<Pubkey>,
    /// Recent blockhash (used for replay protection; replaced by RPC in simulations).
    pub recent_blockhash: Hash,
    /// Compiled instructions referencing account indices.
    pub instructions: Vec<CompiledInstruction>,
}

impl Message {
    /// Serialise the message to bytes for signing.
    ///
    /// The signing bytes are the wire-format message bytes (header + accounts +
    /// blockhash + instructions). Signatures are computed over these bytes.
    ///
    /// reference: solana_sdk::message::Message::serialize (Apache-2.0)
    pub fn serialize_for_signing(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_wire(&mut out);
        out
    }

    /// Write message wire bytes into `out`.
    fn write_wire(&self, out: &mut Vec<u8>) {
        // MessageHeader: 3 bytes
        out.push(self.header.num_required_signatures);
        out.push(self.header.num_readonly_signed_accounts);
        out.push(self.header.num_readonly_unsigned_accounts);

        // Account keys: compact-u16 count + 32 bytes per key
        encode_compact_u16(
            self.account_keys.len() as u16,
            out,
        );
        for pk in &self.account_keys {
            out.extend_from_slice(&pk.0);
        }

        // Recent blockhash: 32 bytes
        out.extend_from_slice(&self.recent_blockhash.0);

        // Instructions: compact-u16 count + each compiled instruction
        encode_compact_u16(self.instructions.len() as u16, out);
        for ix in &self.instructions {
            out.push(ix.program_id_index);
            encode_compact_u16(ix.accounts.len() as u16, out);
            out.extend_from_slice(&ix.accounts);
            encode_compact_u16(ix.data.len() as u16, out);
            out.extend_from_slice(&ix.data);
        }
    }
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

/// A Solana transaction: a message plus the signatures that authorise it.
///
/// reference: solana_sdk::transaction::Transaction (Apache-2.0)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// Ordered signatures — one per required signer in `message.header`.
    /// Uninitialised (zero) signatures are valid for simulation with
    /// `sigVerify: false`.
    pub signatures: Vec<Signature>,
    /// The transaction message containing instructions and account references.
    pub message: Message,
}

impl Transaction {
    /// Construct an unsigned transaction with zero-initialised signatures.
    ///
    /// Allocates `message.header.num_required_signatures` zero signatures.
    ///
    /// reference: solana_sdk::transaction::Transaction::new_unsigned (Apache-2.0)
    pub fn new_unsigned(message: Message) -> Self {
        let n = message.header.num_required_signatures as usize;
        Self {
            signatures: vec![Signature::ZERO; n],
            message,
        }
    }

    /// Build and sign a transaction from uncompiled instructions.
    ///
    /// Compiles the instruction list into a `Message` (deduplicated account list,
    /// signer/readonly classification, program index compilation), then signs
    /// the message bytes with the provided keypairs.
    ///
    /// The first keypair in `signers` is the fee payer unless `payer` is
    /// specified explicitly.
    ///
    /// # Algorithm
    ///
    /// 1. Collect all accounts from all instructions + the payer.
    /// 2. Classify: signer+writable, signer+readonly, unsigned+writable, unsigned+readonly.
    /// 3. Deduplicate preserving canonical ordering.
    /// 4. Build `MessageHeader` from the counts.
    /// 5. Compile each instruction's accounts/program into index references.
    /// 6. Sign the message bytes with each keypair in signer slot order.
    ///
    /// reference: solana_sdk::transaction::Transaction::new_signed_with_payer (Apache-2.0)
    pub fn new_signed_with_payer(
        instructions: &[Instruction],
        payer: Option<&Pubkey>,
        signers: &[&Keypair],
        recent_blockhash: Hash,
    ) -> Self {
        let message = compile_message(instructions, payer, recent_blockhash);
        let mut tx = Self::new_unsigned(message);
        tx.sign(signers);
        tx
    }

    /// Sign the transaction with the provided keypairs.
    ///
    /// Each keypair's pubkey must appear in `message.account_keys` within the
    /// signer slots (indices 0..num_required_signatures). Keypairs are matched
    /// to their slots by pubkey.
    ///
    /// reference: solana_sdk::transaction::Transaction::sign (Apache-2.0)
    pub fn sign(&mut self, keypairs: &[&Keypair]) {
        let message_bytes = self.message.serialize_for_signing();
        let n_signers = self.message.header.num_required_signatures as usize;

        for keypair in keypairs {
            let pk = keypair.pubkey();
            // Find the slot index for this keypair's pubkey.
            if let Some(idx) = self.message.account_keys[..n_signers]
                .iter()
                .position(|k| *k == pk)
                .filter(|&i| i < self.signatures.len())
            {
                self.signatures[idx] = keypair.sign_message(&message_bytes);
            }
        }
    }

    /// Serialise the full transaction to wire bytes.
    ///
    /// Format:
    /// ```text
    /// compact-u16 (signature count)
    /// [u8; 64] × N (signatures)
    /// MessageHeader (3 bytes)
    /// compact-u16 (account count)
    /// [u8; 32] × M (account keys)
    /// [u8; 32] (recent blockhash)
    /// compact-u16 (instruction count)
    /// CompiledInstruction × I
    /// ```
    ///
    /// Used to construct the base64-encoded transaction for `simulateTransaction`.
    ///
    /// reference: solana_sdk::transaction::Transaction::serialize (Apache-2.0)
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Signatures: compact-u16 count + 64 bytes each
        encode_compact_u16(self.signatures.len() as u16, &mut out);
        for sig in &self.signatures {
            out.extend_from_slice(&sig.0);
        }
        // Message
        self.message.write_wire(&mut out);
        out
    }

    /// Deserialise a transaction from wire bytes.
    ///
    /// Returns the transaction and asserts it is structurally valid.
    ///
    /// reference: solana_sdk::transaction::Transaction (Apache-2.0)
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        let mut pos = 0usize;

        // Signatures
        let (sig_count, n) = decode_compact_u16(&bytes[pos..])?;
        pos += n;
        let mut signatures = Vec::with_capacity(sig_count as usize);
        for _ in 0..sig_count {
            if pos + 64 > bytes.len() {
                return Err(WireError::UnexpectedEof);
            }
            let mut sig = [0u8; 64];
            sig.copy_from_slice(&bytes[pos..pos + 64]);
            signatures.push(Signature(sig));
            pos += 64;
        }

        // MessageHeader: 3 bytes
        if pos + 3 > bytes.len() {
            return Err(WireError::UnexpectedEof);
        }
        let header = MessageHeader {
            num_required_signatures: bytes[pos],
            num_readonly_signed_accounts: bytes[pos + 1],
            num_readonly_unsigned_accounts: bytes[pos + 2],
        };
        pos += 3;

        // Account keys
        let (key_count, n) = decode_compact_u16(&bytes[pos..])?;
        pos += n;
        let mut account_keys = Vec::with_capacity(key_count as usize);
        for _ in 0..key_count {
            if pos + 32 > bytes.len() {
                return Err(WireError::UnexpectedEof);
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes[pos..pos + 32]);
            account_keys.push(Pubkey(key));
            pos += 32;
        }

        // Recent blockhash
        if pos + 32 > bytes.len() {
            return Err(WireError::UnexpectedEof);
        }
        let mut blockhash = [0u8; 32];
        blockhash.copy_from_slice(&bytes[pos..pos + 32]);
        let recent_blockhash = Hash(blockhash);
        pos += 32;

        // Instructions
        let (ix_count, n) = decode_compact_u16(&bytes[pos..])?;
        pos += n;
        let mut instructions = Vec::with_capacity(ix_count as usize);
        for _ in 0..ix_count {
            // program_id_index
            if pos >= bytes.len() {
                return Err(WireError::UnexpectedEof);
            }
            let program_id_index = bytes[pos];
            pos += 1;

            // account indices
            let (acc_count, n) = decode_compact_u16(&bytes[pos..])?;
            pos += n;
            if pos + acc_count as usize > bytes.len() {
                return Err(WireError::UnexpectedEof);
            }
            let accounts = bytes[pos..pos + acc_count as usize].to_vec();
            pos += acc_count as usize;

            // data bytes
            let (data_len, n) = decode_compact_u16(&bytes[pos..])?;
            pos += n;
            if pos + data_len as usize > bytes.len() {
                return Err(WireError::UnexpectedEof);
            }
            let data = bytes[pos..pos + data_len as usize].to_vec();
            pos += data_len as usize;

            instructions.push(CompiledInstruction {
                program_id_index,
                accounts,
                data,
            });
        }

        Ok(Transaction {
            signatures,
            message: Message {
                header,
                account_keys,
                recent_blockhash,
                instructions,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Message compilation
// ---------------------------------------------------------------------------

/// Compile a list of instructions and a payer into a `Message`.
///
/// Accounts are ordered: writable signers, read-only signers, writable unsigned,
/// read-only unsigned. Within each group they appear in the order first seen
/// across all instructions (payer first among writable signers).
///
/// reference: solana_sdk::message::Message::new_with_blockhash (Apache-2.0)
fn compile_message(
    instructions: &[Instruction],
    payer: Option<&Pubkey>,
    recent_blockhash: Hash,
) -> Message {
    // Track per-pubkey properties across all instructions.
    // is_signer | is_writable
    let mut signer_set: HashMap<Pubkey, bool> = HashMap::new(); // pubkey → is_writable
    let mut unsigned_set: HashMap<Pubkey, bool> = HashMap::new(); // pubkey → is_writable

    // Payer is always writable + signer.
    if let Some(payer_pk) = payer {
        signer_set.insert(*payer_pk, true);
    }

    // Walk instructions and accumulate account properties.
    for ix in instructions {
        // Program ID is read-only and unsigned.
        unsigned_set
            .entry(ix.program_id)
            .and_modify(|w| *w = false)
            .or_insert(false);

        for meta in &ix.accounts {
            if meta.is_signer {
                let entry = signer_set.entry(meta.pubkey).or_insert(false);
                if meta.is_writable {
                    *entry = true;
                }
            } else {
                // If already in signer_set, keep it there.
                if !signer_set.contains_key(&meta.pubkey) {
                    let entry = unsigned_set.entry(meta.pubkey).or_insert(false);
                    if meta.is_writable {
                        *entry = true;
                    }
                }
            }
        }
    }

    // Build ordered account_keys:
    // 1. Writable signers (payer first if present)
    // 2. Read-only signers
    // 3. Writable unsigned
    // 4. Read-only unsigned
    //
    // Use insertion order within each group by collecting in two passes.
    // We use a BTreeMap-sorted approach for reproducibility.
    let mut writable_signers: Vec<Pubkey> = Vec::new();
    let mut readonly_signers: Vec<Pubkey> = Vec::new();
    let mut writable_unsigned: Vec<Pubkey> = Vec::new();
    let mut readonly_unsigned: Vec<Pubkey> = Vec::new();

    // Payer goes first in writable_signers.
    if let Some(payer_pk) = payer {
        writable_signers.push(*payer_pk);
    }

    // Remaining writable signers (in first-seen order by instruction).
    for ix in instructions {
        for meta in &ix.accounts {
            if meta.is_signer && meta.is_writable {
                let pk = meta.pubkey;
                if !writable_signers.contains(&pk) {
                    writable_signers.push(pk);
                }
            }
        }
    }

    // Read-only signers.
    for ix in instructions {
        for meta in &ix.accounts {
            if meta.is_signer && !meta.is_writable {
                let pk = meta.pubkey;
                if !writable_signers.contains(&pk) && !readonly_signers.contains(&pk) {
                    readonly_signers.push(pk);
                }
            }
        }
    }

    // Writable unsigned.
    for ix in instructions {
        for meta in &ix.accounts {
            if !meta.is_signer && meta.is_writable {
                let pk = meta.pubkey;
                let already = writable_signers.contains(&pk)
                    || readonly_signers.contains(&pk)
                    || writable_unsigned.contains(&pk);
                if !already {
                    writable_unsigned.push(pk);
                }
            }
        }
        // Program IDs are read-only unsigned.
        let pk = ix.program_id;
        if !writable_signers.contains(&pk)
            && !readonly_signers.contains(&pk)
            && !writable_unsigned.contains(&pk)
            && !readonly_unsigned.contains(&pk)
        {
            readonly_unsigned.push(pk);
        }
    }

    // Read-only unsigned (non-program accounts).
    for ix in instructions {
        for meta in &ix.accounts {
            if !meta.is_signer && !meta.is_writable {
                let pk = meta.pubkey;
                let already = writable_signers.contains(&pk)
                    || readonly_signers.contains(&pk)
                    || writable_unsigned.contains(&pk)
                    || readonly_unsigned.contains(&pk);
                if !already {
                    readonly_unsigned.push(pk);
                }
            }
        }
    }

    let num_required_signatures = (writable_signers.len() + readonly_signers.len()) as u8;
    let num_readonly_signed_accounts = readonly_signers.len() as u8;
    let num_readonly_unsigned_accounts = readonly_unsigned.len() as u8;

    let account_keys: Vec<Pubkey> = writable_signers
        .iter()
        .chain(readonly_signers.iter())
        .chain(writable_unsigned.iter())
        .chain(readonly_unsigned.iter())
        .copied()
        .collect();

    // Build account_key → index map.
    let key_index: HashMap<Pubkey, u8> = account_keys
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, i as u8))
        .collect();

    // Compile instructions.
    let compiled_ixs: Vec<CompiledInstruction> = instructions
        .iter()
        .map(|ix| {
            let program_id_index = key_index[&ix.program_id];
            let accounts: Vec<u8> = ix
                .accounts
                .iter()
                .map(|meta| key_index[&meta.pubkey])
                .collect();
            CompiledInstruction {
                program_id_index,
                accounts,
                data: ix.data.clone(),
            }
        })
        .collect();

    Message {
        header: MessageHeader {
            num_required_signatures,
            num_readonly_signed_accounts,
            num_readonly_unsigned_accounts,
        },
        account_keys,
        recent_blockhash,
        instructions: compiled_ixs,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruction::AccountMeta;
    use ed25519_dalek::VerifyingKey;

    fn make_keypair(seed: u8) -> Keypair {
        Keypair::from_seed_bytes(&[seed; 32])
    }

    fn make_pubkey(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    // -----------------------------------------------------------------------
    // Wire-format byte-equality test: empty transaction
    // -----------------------------------------------------------------------

    /// Hand-compute the wire bytes for a minimal transaction:
    /// - 0 signatures (compact-u16 = 0x00)
    /// - MessageHeader: [0, 0, 0]
    /// - 0 account keys (compact-u16 = 0x00)
    /// - recent_blockhash: [0; 32]
    /// - 0 instructions (compact-u16 = 0x00)
    ///
    /// Total: 1 + 3 + 1 + 32 + 1 = 38 bytes
    #[test]
    fn empty_transaction_wire_format_bytes() {
        let tx = Transaction {
            signatures: vec![],
            message: Message {
                header: MessageHeader::default(),
                account_keys: vec![],
                recent_blockhash: Hash::ZERO,
                instructions: vec![],
            },
        };
        let bytes = tx.serialize();
        // compact-u16(0) + header(3) + compact-u16(0) + blockhash(32) + compact-u16(0)
        let expected: Vec<u8> = [
            0x00u8,         // compact-u16: 0 signatures
            0x00, 0x00, 0x00, // header: [0, 0, 0]
            0x00,           // compact-u16: 0 account keys
            // 32 zero bytes for the recent blockhash
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0x00,           // compact-u16: 0 instructions
        ]
        .to_vec();
        assert_eq!(bytes, expected, "empty transaction wire format mismatch");
    }

    // -----------------------------------------------------------------------
    // Round-trip: serialize → from_bytes
    // -----------------------------------------------------------------------

    #[test]
    fn transaction_round_trip() {
        let payer = make_keypair(0x11);
        let program_id = make_pubkey(0x20);
        let account = make_pubkey(0x30);

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(account, false),
            ],
            data: vec![0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
        };

        let blockhash = Hash::from_bytes([0xBB; 32]);
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        let wire = tx.serialize();
        let decoded = Transaction::from_bytes(&wire).expect("round-trip deserialise");
        assert_eq!(tx, decoded, "transaction must survive wire round-trip");
    }

    // -----------------------------------------------------------------------
    // Sign + verify
    // -----------------------------------------------------------------------

    /// After new_signed_with_payer, the first signature must verify against
    /// the message bytes using raw ed25519_dalek::VerifyingKey::verify.
    #[test]
    fn sign_and_verify_with_raw_ed25519() {
        let payer = make_keypair(0x55);
        let program_id = make_pubkey(0x60);

        let ix = Instruction {
            program_id,
            accounts: vec![AccountMeta::new(payer.pubkey(), true)],
            data: vec![0x01],
        };

        let blockhash = Hash::from_bytes([0xCC; 32]);
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        assert_eq!(tx.signatures.len(), 1, "must have exactly 1 signature");
        let sig_bytes = tx.signatures[0].0;

        // Re-compute the message bytes and verify.
        let message_bytes = tx.message.serialize_for_signing();
        let vk = VerifyingKey::from_bytes(payer.pubkey().as_bytes())
            .expect("pubkey must be a valid verifying key");
        let ed_sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        vk.verify_strict(&message_bytes, &ed_sig)
            .expect("signature must verify against signer's pubkey");
    }

    // -----------------------------------------------------------------------
    // Instruction count preserved
    // -----------------------------------------------------------------------

    #[test]
    fn two_instructions_compile_correctly() {
        let payer = make_keypair(0x77);
        let prog1 = make_pubkey(0x80);
        let prog2 = make_pubkey(0x81);

        let ix1 = Instruction { program_id: prog1, accounts: vec![], data: vec![0x01] };
        let ix2 = Instruction { program_id: prog2, accounts: vec![], data: vec![0x02] };

        let blockhash = Hash::ZERO;
        let tx = Transaction::new_signed_with_payer(
            &[ix1, ix2],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        assert_eq!(
            tx.message.instructions.len(),
            2,
            "must compile to 2 instructions"
        );
    }
}
