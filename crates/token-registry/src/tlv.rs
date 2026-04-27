//! Token-2022 TLV (Type-Length-Value) extension decoder.
//!
//! SPL Mint accounts under Token-2022 carry a TLV stream starting at byte 83
//! (after the 82-byte base Mint layout + one account_type byte). This module
//! parses that stream and extracts the fields we care about for detector enrichment:
//!   - ConfidentialTransferMint (extension type 4)
//!   - PermanentDelegate (extension type 12)
//!   - NonTransferable (extension type 9)
//!   - TransferHook (extension type 14)
//!
//! # Canonical source
//!
//! `ExtensionType` discriminator values verified live on 2026-04-21 from:
//!   <https://github.com/solana-program/token-2022/blob/main/interface/src/extension/mod.rs>
//!
//! Confirmed values used here:
//!   - `ConfidentialTransferMint` = 4
//!   - `NonTransferable`          = 9
//!   - `PermanentDelegate`        = 12
//!   - `TransferHook`             = 14
//!
//! # Layout
//!
//! ```text
//! bytes 0..82   Base SPL Mint (mint_authority COption + supply + decimals + freeze_authority)
//! byte  82      account_type: 0=Uninitialized, 1=Mint, 2=Account
//! bytes 83..    TLV stream: sequence of (type: u16 LE, length: u16 LE, data: [u8; length])
//! ```
//!
//! # Design decisions
//!
//! - Unknown extension type discriminators are SKIPPED for forward-compatibility with
//!   new Token-2022 extensions added after this decoder was written.
//! - Zero Pubkeys (all bytes zero) are treated as "not set" — extension present but
//!   delegate/program_id not assigned → `None` in the output struct.
//! - The `spl-token-2022` crate is NOT imported. Its transitive dependency tree
//!   includes `solana-sdk`, `curve25519-dalek`, and others that inflate compile
//!   times and introduce linking complexity. This hand-rolled parser mirrors the
//!   pattern in `crates/dex-adapter` (hand-rolled Raydium account decoder).
//!
//! # Reference
//!
//! See CLAUDE.md §Multi-Chain Rules / Solana for Token-2022 policy.
//! See `docs/designs/0004-detector-01-honeypot.md §S3` for PermanentDelegate risk context.

use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by TLV extension decoding.
///
/// All variants are informational — callers should `unwrap_or_default()` in
/// production paths (legacy SPL tokens legitimately fail `TooShort` / `NotMint`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TlvError {
    /// The account data is shorter than the minimum Token-2022 layout (83 bytes).
    /// This is the normal case for standard SPL Token mints (exactly 82 bytes).
    #[error("account data too short for TLV stream (expected >= 83 bytes, got {0})")]
    TooShort(usize),

    /// The `account_type` byte at index 82 is not `1` (Mint).
    /// Value `2` means this is a token Account (not a Mint). Other values are invalid.
    #[error("account_type byte is not Mint (expected 1, got {0})")]
    NotMint(u8),

    /// The TLV stream was cut off mid-extension (truncated account data).
    #[error("TLV stream truncated: needed {needed} bytes, {available} remaining")]
    Truncated { needed: usize, available: usize },
}

// ---------------------------------------------------------------------------
// Extension type discriminators — verified 2026-04-21
// ---------------------------------------------------------------------------
//
// Source: https://github.com/solana-program/token-2022/blob/main/interface/src/extension/mod.rs
// The #[repr(u16)] ExtensionType enum values confirmed:
//   Uninitialized            = 0
//   TransferFeeConfig        = 1
//   TransferFeeAmount        = 2
//   MintCloseAuthority       = 3
//   ConfidentialTransferMint = 4   (our target — P6-2 action #7)
//   ...
//   NonTransferable          = 9   (our target — P6-2 action #6)
//   ...
//   PermanentDelegate        = 12  (our target)
//   NonTransferableAccount   = 13
//   TransferHook             = 14  (our target)
//   TransferHookAccount      = 15
//   ...

/// Extension discriminator for `ConfidentialTransferMint` (verified = 4).
///
/// When present, transfer amounts are ZK-encrypted; on-chain amounts are opaque
/// ciphertexts. D05 wash-trading detection cannot operate on confidential amounts.
/// P6-2 action item #7.
pub const EXT_TYPE_CONFIDENTIAL_TRANSFER_MINT: u16 = 4;

/// Extension discriminator for `NonTransferable` (verified = 9).
///
/// A marker extension with no data payload. When present, the mint is structurally
/// untransferable — every transfer attempt reverts at the program level. This is
/// the legitimate pattern for soulbound tokens, governance stakes, and identity NFTs.
/// P6-2 action item #6.
pub const EXT_TYPE_NON_TRANSFERABLE: u16 = 9;

/// Extension discriminator for `PermanentDelegate` (verified = 12).
pub const EXT_TYPE_PERMANENT_DELEGATE: u16 = 12;

/// Extension discriminator for `TransferHook` (verified = 14).
pub const EXT_TYPE_TRANSFER_HOOK: u16 = 14;

/// Byte offset where the Token-2022 TLV stream begins.
/// Bytes 0..82: base SPL Mint. Byte 82: account_type. Byte 83: start of TLV.
pub const TLV_STREAM_OFFSET: usize = 83;

/// The expected `account_type` value for a Mint account.
const ACCOUNT_TYPE_MINT: u8 = 1;

/// A Pubkey whose bytes are all zero — treated as "unset" in Token-2022 extensions.
const ZERO_PUBKEY: [u8; 32] = [0u8; 32];

// ---------------------------------------------------------------------------
// Output type
// ---------------------------------------------------------------------------

/// Decoded Token-2022 extension fields relevant to anomaly detection.
///
/// Fields are `None` when:
/// - The corresponding extension is absent from the TLV stream.
/// - The extension is present but the key is the zero Pubkey (not yet assigned).
///
/// Boolean marker extensions (`non_transferable`, `confidential_transfer`) are
/// `true` when the extension discriminator is present in the TLV stream.
///
/// All other extensions are parsed and skipped (forward-compat). New fields
/// should be added here as detectors demand them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Token2022Extensions {
    /// `ConfidentialTransferMint` marker (extension type 4).
    ///
    /// When `true`, transfer amounts are ZK-encrypted. D05 wash-trading cannot
    /// operate on confidential amounts and returns `InsufficientBaseline`.
    /// P6-2 action item #7.
    pub confidential_transfer: bool,

    /// `NonTransferable` marker (extension type 9).
    ///
    /// When `true`, every transfer attempt reverts at the program level. This is a
    /// marker-only extension with no data payload — legitimate for soulbound tokens,
    /// governance stakes, identity NFTs. D01 Signal A weight is attenuated; D05
    /// returns `InsufficientBaseline` (wash trading is structurally impossible).
    /// P6-2 action item #6.
    pub non_transferable: bool,

    /// `PermanentDelegate.delegate` — the address that can transfer/burn any
    /// holder's tokens without consent. `None` if extension absent or delegate = zero.
    pub permanent_delegate: Option<[u8; 32]>,

    /// `TransferHook.program_id` — the program invoked on every token transfer.
    /// `None` if extension absent or program_id = zero.
    pub transfer_hook_program: Option<[u8; 32]>,

    /// `TransferHook.authority` — who can update the hook program.
    /// Carried here for completeness (not yet surfaced in `TokenMeta`).
    pub transfer_hook_authority: Option<[u8; 32]>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Decode Token-2022 TLV extensions from a raw SPL Mint account data blob.
///
/// # Arguments
///
/// - `account_data`: raw bytes from `getAccountInfo(mint).data` (base64-decoded).
///
/// # Returns
///
/// - `Ok(Token2022Extensions)` with fields populated where extensions are found.
///   Unknown extension discriminators are silently skipped.
/// - `Err(TlvError::TooShort)` if `account_data.len() < 83`. Standard SPL Token
///   mints (82 bytes) will hit this — callers should `unwrap_or_default()`.
/// - `Err(TlvError::NotMint)` if the `account_type` byte (index 82) is not `1`.
/// - `Err(TlvError::Truncated)` if the TLV stream ends mid-extension header or body.
///
/// # Example
///
/// ```
/// # use mg_onchain_token_registry::tlv::{decode_extensions, Token2022Extensions};
/// let account_data = vec![0u8; 82]; // legacy SPL — too short for TLV
/// let ext = decode_extensions(&account_data).unwrap_or_default();
/// assert_eq!(ext, Token2022Extensions::default());
/// ```
pub fn decode_extensions(account_data: &[u8]) -> Result<Token2022Extensions, TlvError> {
    if account_data.len() < TLV_STREAM_OFFSET {
        return Err(TlvError::TooShort(account_data.len()));
    }

    let account_type = account_data[82];
    if account_type != ACCOUNT_TYPE_MINT {
        return Err(TlvError::NotMint(account_type));
    }

    let tlv_stream = &account_data[TLV_STREAM_OFFSET..];
    parse_tlv_stream(tlv_stream)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse the full TLV byte stream, populating a `Token2022Extensions`.
///
/// Iterates through `(type: u16 LE, length: u16 LE, data: [u8; length])` triples.
/// Unknown types are skipped by advancing `length` bytes. Returns `Truncated`
/// if the stream ends before a complete header or body can be read.
fn parse_tlv_stream(tlv: &[u8]) -> Result<Token2022Extensions, TlvError> {
    let mut ext = Token2022Extensions::default();
    let mut pos = 0usize;

    while pos < tlv.len() {
        // Need at least 4 bytes for the (type, length) header.
        if tlv.len() - pos < 4 {
            // Trailing zeros or padding at end of account is normal in some
            // Token-2022 layouts. If the remaining bytes are all zero we treat
            // it as end-of-stream (Uninitialized extension type = 0, length = 0).
            let remaining = &tlv[pos..];
            if remaining.iter().all(|&b| b == 0) {
                break;
            }
            return Err(TlvError::Truncated { needed: 4, available: tlv.len() - pos });
        }

        let ext_type = read_u16_le(tlv, pos);
        let ext_len = read_u16_le(tlv, pos + 2) as usize;
        pos += 4;

        // Extension type 0 = Uninitialized — marks end of used TLV space.
        if ext_type == 0 && ext_len == 0 {
            break;
        }

        // Verify enough bytes remain for the extension body.
        if tlv.len() - pos < ext_len {
            return Err(TlvError::Truncated {
                needed: ext_len,
                available: tlv.len() - pos,
            });
        }

        let data = &tlv[pos..pos + ext_len];

        match ext_type {
            EXT_TYPE_CONFIDENTIAL_TRANSFER_MINT => {
                // Marker extension with data payload (config bytes).
                // We only need to record presence, not parse the ZK config.
                ext.confidential_transfer = true;
            }
            EXT_TYPE_NON_TRANSFERABLE => {
                // Pure marker extension — no data payload (ext_len == 0).
                // Record presence only.
                ext.non_transferable = true;
            }
            EXT_TYPE_PERMANENT_DELEGATE => {
                ext.permanent_delegate = decode_permanent_delegate(data);
            }
            EXT_TYPE_TRANSFER_HOOK => {
                let (authority, program_id) = decode_transfer_hook(data);
                ext.transfer_hook_authority = authority;
                ext.transfer_hook_program = program_id;
            }
            // Unknown extension — skip body bytes (forward-compatible).
            _ => {}
        }

        pos += ext_len;
    }

    Ok(ext)
}

/// Decode `PermanentDelegate` extension data (32 bytes = one Pubkey).
///
/// Returns `None` if the data is too short or the key is the zero Pubkey.
fn decode_permanent_delegate(data: &[u8]) -> Option<[u8; 32]> {
    if data.len() < 32 {
        return None;
    }
    let key: [u8; 32] = data[..32].try_into().ok()?;
    if key == ZERO_PUBKEY { None } else { Some(key) }
}

/// Decode `TransferHook` extension data (64 bytes):
///   bytes 0..32: authority Pubkey
///   bytes 32..64: program_id Pubkey
///
/// Returns `(authority, program_id)` as `Option<[u8;32]>` each, `None` for zero Pubkeys
/// or insufficient data.
fn decode_transfer_hook(data: &[u8]) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
    if data.len() < 64 {
        return (None, None);
    }
    let authority: [u8; 32] = data[..32].try_into().unwrap();
    let program_id: [u8; 32] = data[32..64].try_into().unwrap();

    let authority_opt = if authority == ZERO_PUBKEY { None } else { Some(authority) };
    let program_id_opt = if program_id == ZERO_PUBKEY { None } else { Some(program_id) };
    (authority_opt, program_id_opt)
}

/// Read a `u16` from `buf` at `offset` in little-endian byte order.
///
/// # Panics
///
/// Panics if `offset + 2 > buf.len()`. Callers must bounds-check before calling.
#[inline]
fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -----------------------------------------------------------

    /// Build a minimal Token-2022 account data blob with the given TLV extensions
    /// appended after the 82-byte base layout + 1 account_type byte.
    fn make_token2022_data(account_type: u8, tlv_body: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 82]; // base SPL Mint layout (zeroed)
        data[45] = 1; // is_initialized = 1 (required by rpc.rs decoder, not TLV decoder)
        data.push(account_type); // byte 82: account_type
        data.extend_from_slice(tlv_body);
        data
    }

    /// Encode a single TLV extension entry.
    fn tlv_entry(ext_type: u16, body: &[u8]) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(&ext_type.to_le_bytes());
        entry.extend_from_slice(&(body.len() as u16).to_le_bytes());
        entry.extend_from_slice(body);
        entry
    }

    fn nonzero_pubkey(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    // ---- TooShort ----------------------------------------------------------

    #[test]
    fn decode_too_short() {
        let data = vec![0u8; 50];
        let err = decode_extensions(&data).unwrap_err();
        assert_eq!(err, TlvError::TooShort(50));
    }

    #[test]
    fn decode_legacy_spl_mint_no_extensions() {
        // Exactly 82 bytes — standard SPL Token mint, no TLV stream.
        let data = vec![0u8; 82];
        let err = decode_extensions(&data).unwrap_err();
        assert_eq!(err, TlvError::TooShort(82));
        // Callers should unwrap_or_default().
        let ext = decode_extensions(&data).unwrap_or_default();
        assert_eq!(ext, Token2022Extensions::default());
    }

    // ---- NotMint -----------------------------------------------------------

    #[test]
    fn decode_non_mint_account_type() {
        // account_type = 2 means this is a token Account, not a Mint.
        let data = make_token2022_data(2, &[]);
        let err = decode_extensions(&data).unwrap_err();
        assert_eq!(err, TlvError::NotMint(2));
    }

    // ---- Empty TLV ---------------------------------------------------------

    #[test]
    fn decode_empty_tlv_stream() {
        // 82 base bytes + account_type=1 + no TLV entries → all None.
        let data = make_token2022_data(1, &[]);
        let ext = decode_extensions(&data).unwrap();
        assert_eq!(ext, Token2022Extensions::default());
    }

    // ---- PermanentDelegate only -------------------------------------------

    #[test]
    fn decode_permanent_delegate_only() {
        let delegate = nonzero_pubkey(0xAB);
        let tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        assert_eq!(ext.permanent_delegate, Some(delegate));
        assert!(ext.transfer_hook_program.is_none());
        assert!(ext.transfer_hook_authority.is_none());
    }

    // ---- Zero PermanentDelegate → None ------------------------------------

    #[test]
    fn decode_zero_permanent_delegate_returns_none() {
        let zero_delegate = ZERO_PUBKEY;
        let tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &zero_delegate);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        // Zero Pubkey means "no delegate assigned" — must be None.
        assert!(ext.permanent_delegate.is_none());
    }

    // ---- TransferHook only ------------------------------------------------

    #[test]
    fn decode_transfer_hook_only() {
        let authority = nonzero_pubkey(0x11);
        let program_id = nonzero_pubkey(0x22);
        let mut hook_body = Vec::new();
        hook_body.extend_from_slice(&authority);
        hook_body.extend_from_slice(&program_id);

        let tlv = tlv_entry(EXT_TYPE_TRANSFER_HOOK, &hook_body);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        assert!(ext.permanent_delegate.is_none());
        assert_eq!(ext.transfer_hook_authority, Some(authority));
        assert_eq!(ext.transfer_hook_program, Some(program_id));
    }

    // ---- Both extensions --------------------------------------------------

    #[test]
    fn decode_both_extensions() {
        let delegate = nonzero_pubkey(0xDE);
        let authority = nonzero_pubkey(0xAA);
        let program_id = nonzero_pubkey(0xBB);

        let mut tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate);
        let mut hook_body = Vec::new();
        hook_body.extend_from_slice(&authority);
        hook_body.extend_from_slice(&program_id);
        tlv.extend(tlv_entry(EXT_TYPE_TRANSFER_HOOK, &hook_body));

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();

        assert_eq!(ext.permanent_delegate, Some(delegate));
        assert_eq!(ext.transfer_hook_authority, Some(authority));
        assert_eq!(ext.transfer_hook_program, Some(program_id));
    }

    // ---- Unknown extension skipped ----------------------------------------

    #[test]
    fn decode_with_unknown_extension_skipped() {
        let delegate = nonzero_pubkey(0xCC);
        let program_id = nonzero_pubkey(0xDD);

        // Layout: [known PermanentDelegate] [unknown 0xFFFE] [known TransferHook]
        let mut tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate);

        // Unknown discriminator 0xFFFE with 16 bytes of filler.
        let unknown_body = [0x55u8; 16];
        tlv.extend(tlv_entry(0xFFFE, &unknown_body));

        let mut hook_body = vec![0u8; 32]; // zero authority
        hook_body.extend_from_slice(&program_id);
        tlv.extend(tlv_entry(EXT_TYPE_TRANSFER_HOOK, &hook_body));

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();

        // Unknown extension must be skipped — both known extensions still parsed.
        assert_eq!(ext.permanent_delegate, Some(delegate));
        assert!(ext.transfer_hook_authority.is_none()); // authority was zeroed
        assert_eq!(ext.transfer_hook_program, Some(program_id));
    }

    // ---- Truncated TLV ----------------------------------------------------

    #[test]
    fn decode_truncated_tlv_returns_truncated_error() {
        let delegate = nonzero_pubkey(0xFF);
        let mut body = delegate.to_vec();
        body.truncate(16); // cut the body in half
        // Write header saying body is 32 bytes, but only 16 are there.
        let mut tlv = EXT_TYPE_PERMANENT_DELEGATE.to_le_bytes().to_vec();
        tlv.extend_from_slice(&32u16.to_le_bytes()); // claims 32 bytes
        tlv.extend_from_slice(&body); // only 16 bytes follow

        let data = make_token2022_data(1, &tlv);
        let err = decode_extensions(&data).unwrap_err();
        assert!(
            matches!(err, TlvError::Truncated { needed: 32, available: 16 }),
            "expected Truncated{{needed:32, available:16}}, got {err:?}"
        );
    }

    // ---- Trailing zero padding (normal in Token-2022) ----------------------

    #[test]
    fn decode_trailing_zero_padding_is_ok() {
        // Some Token-2022 accounts pad the tail with zeros after the last extension.
        let delegate = nonzero_pubkey(0x77);
        let mut tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate);
        // Append 8 bytes of zero padding (simulates pre-allocated account space).
        tlv.extend_from_slice(&[0u8; 8]);

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();
        assert_eq!(ext.permanent_delegate, Some(delegate));
    }

    // ---- NonTransferable (discriminator 9) — P6-2 action #6 ---------------

    /// `NonTransferable` extension is a zero-length marker; presence sets the bool.
    #[test]
    fn decode_non_transferable_marker_sets_flag() {
        // NonTransferable has no data payload — ext_len == 0.
        let tlv = tlv_entry(EXT_TYPE_NON_TRANSFERABLE, &[]);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        assert!(ext.non_transferable, "non_transferable must be true when extension 9 is present");
        assert!(!ext.confidential_transfer, "confidential_transfer must be false");
        assert!(ext.permanent_delegate.is_none());
        assert!(ext.transfer_hook_program.is_none());
    }

    /// `NonTransferable` combined with `PermanentDelegate` — both should decode.
    #[test]
    fn decode_non_transferable_with_permanent_delegate() {
        let delegate = nonzero_pubkey(0xEE);
        let mut tlv = tlv_entry(EXT_TYPE_NON_TRANSFERABLE, &[]);
        tlv.extend(tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate));

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();

        assert!(ext.non_transferable, "non_transferable must be set");
        assert_eq!(ext.permanent_delegate, Some(delegate));
    }

    /// Absence of `NonTransferable` extension → flag remains false.
    #[test]
    fn decode_no_non_transferable_flag_remains_false() {
        // Only PermanentDelegate is present; NonTransferable is absent.
        let delegate = nonzero_pubkey(0x55);
        let tlv = tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        assert!(!ext.non_transferable, "non_transferable must be false when extension absent");
        assert_eq!(ext.permanent_delegate, Some(delegate));
    }

    // ---- ConfidentialTransferMint (discriminator 4) — P6-2 action #7 ------

    /// `ConfidentialTransferMint` extension presence sets `confidential_transfer`.
    ///
    /// The extension carries a config payload (ElGamal pubkey + autoapprove flag).
    /// We only need presence — simulate with 96 bytes of synthetic data.
    #[test]
    fn decode_confidential_transfer_mint_sets_flag() {
        // ConfidentialTransferMint data layout (96 bytes in mainnet):
        //   32 bytes: authority (Option<Pubkey>)
        //   1 byte:   auto_approve_new_accounts
        //   32 bytes: auditor_elgamal_pubkey (Option<ElGamalPubkey>)
        //   remainder: ZK config
        // We just need 1+ bytes to simulate; actual length doesn't affect the bool.
        let payload = [0xABu8; 96];
        let tlv = tlv_entry(EXT_TYPE_CONFIDENTIAL_TRANSFER_MINT, &payload);
        let data = make_token2022_data(1, &tlv);

        let ext = decode_extensions(&data).unwrap();
        assert!(ext.confidential_transfer, "confidential_transfer must be true when extension 4 is present");
        assert!(!ext.non_transferable, "non_transferable must be false");
        assert!(ext.permanent_delegate.is_none());
    }

    /// Both `ConfidentialTransferMint` and `NonTransferable` can coexist.
    #[test]
    fn decode_confidential_transfer_and_non_transferable_both_set() {
        let payload = [0x01u8; 64];
        let mut tlv = tlv_entry(EXT_TYPE_CONFIDENTIAL_TRANSFER_MINT, &payload);
        tlv.extend(tlv_entry(EXT_TYPE_NON_TRANSFERABLE, &[]));

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();

        assert!(ext.confidential_transfer);
        assert!(ext.non_transferable);
        assert!(ext.permanent_delegate.is_none());
        assert!(ext.transfer_hook_program.is_none());
    }

    /// All four extensions present — each decoded independently.
    #[test]
    fn decode_all_four_extensions_present() {
        let ct_payload = [0x01u8; 64];
        let delegate = nonzero_pubkey(0x11);
        let hook_authority = nonzero_pubkey(0x22);
        let hook_program = nonzero_pubkey(0x33);

        let mut hook_body = Vec::new();
        hook_body.extend_from_slice(&hook_authority);
        hook_body.extend_from_slice(&hook_program);

        let mut tlv = tlv_entry(EXT_TYPE_CONFIDENTIAL_TRANSFER_MINT, &ct_payload);
        tlv.extend(tlv_entry(EXT_TYPE_NON_TRANSFERABLE, &[]));
        tlv.extend(tlv_entry(EXT_TYPE_PERMANENT_DELEGATE, &delegate));
        tlv.extend(tlv_entry(EXT_TYPE_TRANSFER_HOOK, &hook_body));

        let data = make_token2022_data(1, &tlv);
        let ext = decode_extensions(&data).unwrap();

        assert!(ext.confidential_transfer);
        assert!(ext.non_transferable);
        assert_eq!(ext.permanent_delegate, Some(delegate));
        assert_eq!(ext.transfer_hook_authority, Some(hook_authority));
        assert_eq!(ext.transfer_hook_program, Some(hook_program));
    }
}
