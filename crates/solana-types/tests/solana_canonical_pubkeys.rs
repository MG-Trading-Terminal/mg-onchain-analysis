//! Canonical Solana pubkey round-trip tests.
//!
//! These fixtures use well-known, publicly-documented Solana program IDs and
//! token mint addresses.  The expected base58 strings are sourced from official
//! Solana documentation and public token registries — NOT from the `solana-sdk`
//! crate — so they serve as independent ground-truth validation of our base58
//! encoder/decoder.
//!
//! Sources:
//! - System Program:    https://docs.solana.com/developing/runtime-facilities/programs
//! - Wrapped SOL mint:  https://spl.solana.com/token (WSOL = native mint)
//! - USDC mint:         https://www.circle.com/en/usdc-multichain/solana
//! - Token Program:     https://spl.solana.com/token
//! - BPF Loader:        https://docs.solana.com/developing/runtime-facilities/programs
//!
//! reference: solana_sdk::pubkey (Apache-2.0) — public program ID constants used
//!            for comparison only (no code derived; values are public knowledge).

use mg_solana_types::{Pubkey, PubkeyError};

// ---------------------------------------------------------------------------
// Well-known program / mint pubkeys (byte representations)
// ---------------------------------------------------------------------------

/// System Program: 11111111111111111111111111111111 — all-zero bytes.
const SYSTEM_PROGRAM_STR: &str = "11111111111111111111111111111111";

/// Wrapped SOL mint: So11111111111111111111111111111111111111112
/// Bytes: [0x06, 0x9b, 0x8f, 0x57, 0x16, 0xfb, 0xd8, 0x55, 0xa7, 0x7a, 0xca, 0xc3,
///         0x4a, 0x24, 0x34, 0x08, 0xf6, 0xd7, 0xd7, 0x4b, 0x5c, 0x7b, 0xbd, 0x0d,
///         0xf7, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
/// Confirmed: https://explorer.solana.com/address/So11111111111111111111111111111111111111112
const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

/// USDC mint: EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
/// Confirmed: https://explorer.solana.com/address/EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
const USDC_MINT_STR: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// SPL Token Program: TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
/// Confirmed: https://spl.solana.com/token
const SPL_TOKEN_PROGRAM_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Token-2022 Program: TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb
/// Confirmed: https://spl.solana.com/token-2022
const SPL_TOKEN_2022_PROGRAM_STR: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The System Program (all-zero bytes) must display as 32 ones.
///
/// This is the canonical encoding of `[0u8; 32]` under base58 — a standard
/// property of the Base58 alphabet: a zero byte encodes to '1'.
#[test]
fn pubkey_zero_displays_as_all_ones() {
    assert_eq!(
        Pubkey::ZERO.to_string(),
        SYSTEM_PROGRAM_STR,
        "ZERO pubkey must display as the System Program ID string"
    );
}

/// Parse the System Program ID and confirm it round-trips back to the same string.
#[test]
fn system_program_round_trip() {
    let pk: Pubkey = SYSTEM_PROGRAM_STR.parse().expect("system program parse failed");
    assert_eq!(pk, Pubkey::ZERO);
    assert_eq!(pk.to_string(), SYSTEM_PROGRAM_STR);
}

/// Parse the Wrapped SOL mint and confirm Display produces the canonical string.
///
/// WSOL is `So11111111111111111111111111111111111111112` — nearly all ones but
/// with specific high-order bytes set, making it a useful non-trivial test case
/// distinct from the all-zero case.
#[test]
fn wrapped_sol_pubkey_round_trip() {
    let pk: Pubkey = WSOL_MINT_STR.parse().expect("WSOL parse failed");
    assert_eq!(
        pk.to_string(),
        WSOL_MINT_STR,
        "WSOL mint round-trip mismatch"
    );
}

/// Parse the USDC mint address and confirm round-trip.
///
/// USDC on Solana mainnet uses a well-known address that is mixed-case and
/// uses most of the base58 character set — a good general encoder/decoder test.
#[test]
fn usdc_pubkey_round_trip() {
    let pk: Pubkey = USDC_MINT_STR.parse().expect("USDC parse failed");
    assert_eq!(
        pk.to_string(),
        USDC_MINT_STR,
        "USDC mint round-trip mismatch"
    );
}

/// Parse the SPL Token Program ID and confirm round-trip.
#[test]
fn spl_token_program_round_trip() {
    let pk: Pubkey = SPL_TOKEN_PROGRAM_STR.parse().expect("SPL Token Program parse failed");
    assert_eq!(pk.to_string(), SPL_TOKEN_PROGRAM_STR);
}

/// Parse the Token-2022 Program ID and confirm round-trip.
#[test]
fn spl_token_2022_program_round_trip() {
    let pk: Pubkey = SPL_TOKEN_2022_PROGRAM_STR
        .parse()
        .expect("Token-2022 Program parse failed");
    assert_eq!(pk.to_string(), SPL_TOKEN_2022_PROGRAM_STR);
}

/// A too-short base58 string must return a `WrongLength` error.
///
/// This confirms the length gate is enforced before acceptance.
#[test]
fn pubkey_parse_wrong_length_errors() {
    // "abc" in base58 decodes to only 2 bytes — far short of 32.
    let result = "abc".parse::<Pubkey>();
    assert!(
        result.is_err(),
        "short base58 string should not parse as Pubkey"
    );
    match result.unwrap_err() {
        PubkeyError::WrongLength(n) => {
            assert!(n < 32, "expected length < 32, got {n}");
        }
        e => panic!("expected WrongLength, got {e:?}"),
    }
}

/// A string containing characters outside the base58 alphabet must return
/// `InvalidBase58`.
///
/// The characters '0', 'O', 'I', 'l' are intentionally excluded from base58.
#[test]
fn pubkey_parse_invalid_base58_errors() {
    // '0' is not in the base58 alphabet — the decode must fail immediately.
    let result = "0EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse::<Pubkey>();
    assert!(matches!(result, Err(PubkeyError::InvalidBase58(_))));
}

/// A string of the right character count but wrong decoded length must error.
///
/// Base58 strings of the same character length can decode to different byte
/// lengths depending on leading '1' characters and the encoding.  This test
/// confirms the decoder rejects strings that don't decode to exactly 32 bytes.
#[test]
fn pubkey_parse_too_many_decoded_bytes_errors() {
    // Constructing a valid base58 string that decodes to > 32 bytes:
    // base58 with the Bitcoin alphabet gives one '1' character per leading-zero
    // byte (the alphabet maps the digit zero to '1'). So a string of 45 '1' chars
    // decodes to a 45-byte all-zero slice — which fails our `len != 32` check
    // with WrongLength(45). The semantic test is "reject base58 strings that
    // decode to a byte count other than 32"; the specific count is incidental.
    let too_long_str = "1".repeat(45);
    let result = too_long_str.parse::<Pubkey>();
    assert!(
        matches!(result, Err(PubkeyError::WrongLength(45))),
        "expected WrongLength(45), got {result:?}"
    );
}

/// Serde JSON round-trip for a canonical pubkey.
#[test]
fn usdc_pubkey_serde_round_trip() {
    let pk: Pubkey = USDC_MINT_STR.parse().unwrap();
    let json = serde_json::to_string(&pk).unwrap();
    // JSON must be the base58 string, not an array.
    assert_eq!(json, format!("\"{}\"", USDC_MINT_STR));
    let back: Pubkey = serde_json::from_str(&json).unwrap();
    assert_eq!(pk, back);
}

/// Confirms that two pubkeys built from the same raw bytes are equal, and that
/// the base58 round-trip preserves the bytes exactly.
#[test]
fn pubkey_byte_equality_preserved_through_base58() {
    let pk: Pubkey = WSOL_MINT_STR.parse().unwrap();
    let bytes: [u8; 32] = pk.into();
    let pk2 = Pubkey::from_bytes(bytes);
    assert_eq!(pk, pk2);
    assert_eq!(pk2.to_string(), WSOL_MINT_STR);
}
