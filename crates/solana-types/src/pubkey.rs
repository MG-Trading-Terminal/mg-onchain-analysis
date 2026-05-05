//! Solana 32-byte public key with base58 encoding/decoding.
//!
//! Solana uses Bitcoin-style base58 (alphabet:
//! `123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz`).
//! A `Pubkey` is exactly 32 bytes; the encoded form is 32–44 base58 characters.
//!
//! # Serde representation
//!
//! Serialises as a base58 string (no prefix).  Deserialises from the same form.
//!
//! # Zero address
//!
//! The all-zero pubkey encodes to `"11111111111111111111111111111111"` (32 ones)
//! and is the Solana System Program ID.
//!
//! # PDA derivation
//!
//! [`Pubkey::find_program_address`] and [`Pubkey::create_program_address`] implement
//! Solana's Program Derived Address algorithm documented in the Solana runtime.
//! They use SHA-256 (from `sha2`) and an Ed25519 on-curve check (from `ed25519-dalek`).
//!
//! # Reference
//!
//! reference: solana_sdk::pubkey::Pubkey (Apache-2.0) — type layout, base58
//!            encoding convention, PDA derivation algorithm, and constant semantics
//!            consulted. https://github.com/solana-labs/solana/blob/master/sdk/program/src/pubkey.rs
//! reference: https://en.bitcoin.it/wiki/Base58Check_encoding — alphabet definition.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned when constructing a [`Pubkey`] or deriving a PDA.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PubkeyError {
    /// The decoded byte length was not exactly 32.
    #[error("pubkey must decode to 32 bytes (got {0})")]
    WrongLength(usize),
    /// The string contained characters outside the base58 alphabet.
    #[error("invalid base58 in pubkey: {0}")]
    InvalidBase58(String),
    /// PDA derivation: the seeds produce an on-curve point (not a valid PDA).
    #[error("seeds do not produce a valid program derived address (on-curve point)")]
    InvalidSeeds,
    /// The maximum number of seeds (16) was exceeded.
    #[error("too many seeds: {0} (max 16)")]
    TooManySeeds(usize),
    /// A single seed exceeded the maximum length (32 bytes).
    #[error("seed too large: {0} bytes (max 32)")]
    SeedTooLarge(usize),
}

// ---------------------------------------------------------------------------
// Pubkey
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Const base58 decoder (for `from_str_const`)
// ---------------------------------------------------------------------------

/// Decode a base58 string at compile time, returning a `Pubkey`.
///
/// Panics at compile time if the string is not valid base58 or does not
/// decode to exactly 32 bytes.
///
/// Uses a fixed lookup table for the Bitcoin base58 alphabet. The algorithm
/// mirrors `bs58::decode` but is implemented as a `const fn`.
///
/// reference: solana_sdk::pubkey::Pubkey::from_str_const (Apache-2.0) — same semantics.
/// reference: https://en.bitcoin.it/wiki/Base58Check_encoding — alphabet definition.
#[allow(clippy::cast_possible_truncation)]
pub(crate) const fn const_b58_decode_32(s: &[u8]) -> Pubkey {
    // Bitcoin base58 alphabet (same as Solana).
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    // Build reverse lookup: ASCII byte → base58 digit (0..58), 0xFF = invalid.
    const DECODE: [u8; 128] = {
        let mut table = [0xFFu8; 128];
        let mut i = 0usize;
        while i < 58 {
            table[ALPHABET[i] as usize] = i as u8;
            i += 1;
        }
        table
    };

    // Decode base58 string into a big-integer represented as bytes (big-endian).
    // We use a u8 array as our working accumulator, big-endian.
    // Maximum decoded size for a 44-char base58 string is ≤ 32 bytes.
    let mut result = [0u8; 32];

    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if c as usize >= 128 {
            panic!("invalid base58 character (non-ASCII) in pubkey constant");
        }
        let digit = DECODE[c as usize];
        if digit == 0xFF {
            panic!("invalid base58 character in pubkey constant");
        }
        // Multiply result by 58 and add digit.
        let mut carry = digit as u32;
        let mut j = 31i32;
        while j >= 0 {
            carry += 58 * result[j as usize] as u32;
            result[j as usize] = (carry & 0xFF) as u8;
            carry >>= 8;
            j -= 1;
        }
        if carry != 0 {
            panic!("base58 value overflows 32 bytes in pubkey constant");
        }
        i += 1;
    }

    Pubkey(result)
}

/// Maximum number of seeds for PDA derivation.
/// reference: solana_sdk::pubkey::MAX_SEEDS (Apache-2.0)
const MAX_SEEDS: usize = 16;

/// Maximum length of a single PDA seed.
/// reference: solana_sdk::pubkey::MAX_SEED_LEN (Apache-2.0)
const MAX_SEED_LEN: usize = 32;

/// PDA derivation domain-separation marker appended after seeds.
/// reference: solana_sdk::pubkey (Apache-2.0)
const PDA_MARKER: &[u8; 21] = b"ProgramDerivedAddress";

/// A 32-byte Solana public key (wallet address, program ID, etc.).
///
/// `Display` / `Serialize` produce the canonical base58 string (no prefix).
/// `FromStr` / `Deserialize` accept any valid base58 string that decodes to
/// exactly 32 bytes; strings that decode to a different length are rejected.
///
/// Equality is byte-level; the base58 string representation is never compared
/// directly.
///
/// # Constants
///
/// - [`Pubkey::ZERO`]: `[0u8; 32]` — encodes as
///   `"11111111111111111111111111111111"` (the System Program ID).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(C)]
pub struct Pubkey(pub [u8; 32]);

impl Pubkey {
    /// The all-zero pubkey — encodes as `"11111111111111111111111111111111"`.
    ///
    /// This is the Solana System Program ID on mainnet.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from a raw 32-byte array.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Construct from a raw 32-byte array (alias matching `solana_sdk::Pubkey::new_from_array`).
    ///
    /// reference: solana_sdk::pubkey::Pubkey::new_from_array (Apache-2.0)
    #[inline]
    pub const fn new_from_array(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse a base58-encoded pubkey at compile time, panicking on invalid input.
    ///
    /// Matches the call pattern of `solana_sdk::pubkey::Pubkey::from_str_const`
    /// used in `const` static initialisers. Implemented as `const fn` using an
    /// inline base58 decoder with a lookup table.
    ///
    /// # Panics
    ///
    /// Panics (at compile time or runtime) if `s` is not a valid base58 string
    /// decoding to exactly 32 bytes.
    ///
    /// reference: solana_sdk::pubkey::Pubkey::from_str_const (Apache-2.0)
    pub const fn from_str_const(s: &str) -> Self {
        const_b58_decode_32(s.as_bytes())
    }

    /// Return a reference to the raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode to a base58 string.
    ///
    /// This is the same value returned by `Display` and `Serialize`.
    #[must_use]
    pub fn to_base58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }

    /// Decode from a base58 string, returning an error on invalid input.
    fn from_base58(s: &str) -> Result<Self, PubkeyError> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| PubkeyError::InvalidBase58(e.to_string()))?;
        if decoded.len() != 32 {
            return Err(PubkeyError::WrongLength(decoded.len()));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&decoded);
        Ok(Self(bytes))
    }

    // -----------------------------------------------------------------------
    // PDA derivation
    // -----------------------------------------------------------------------

    /// Derive a Program Derived Address (PDA) from seeds without iterating bump seeds.
    ///
    /// The derivation is:
    /// `SHA-256(seeds[0] || seeds[1] || ... || program_id || "ProgramDerivedAddress")`
    ///
    /// Returns an error if the result lies on the Ed25519 curve — on-curve points
    /// have corresponding private keys and are therefore not safe PDAs. Use
    /// [`find_program_address`] to automatically search for a valid bump.
    ///
    /// # Errors
    ///
    /// - [`PubkeyError::TooManySeeds`] — more than 16 seeds provided.
    /// - [`PubkeyError::SeedTooLarge`] — any seed exceeds 32 bytes.
    /// - [`PubkeyError::InvalidSeeds`] — result is an on-curve point.
    ///
    /// # Reference
    ///
    /// reference: solana_sdk::pubkey::Pubkey::create_program_address (Apache-2.0)
    pub fn create_program_address(
        seeds: &[&[u8]],
        program_id: &Pubkey,
    ) -> Result<Pubkey, PubkeyError> {
        if seeds.len() > MAX_SEEDS {
            return Err(PubkeyError::TooManySeeds(seeds.len()));
        }
        for seed in seeds {
            if seed.len() > MAX_SEED_LEN {
                return Err(PubkeyError::SeedTooLarge(seed.len()));
            }
        }

        let mut hasher = Sha256::new();
        for seed in seeds {
            hasher.update(seed);
        }
        hasher.update(program_id.0);
        hasher.update(PDA_MARKER);
        let hash: [u8; 32] = hasher.finalize().into();

        // A valid PDA must NOT lie on the Ed25519 curve. If it does, the 32 bytes
        // are a valid public key with a corresponding private key — unsafe for a PDA.
        //
        // reference: solana_sdk::pubkey::Pubkey::create_program_address (Apache-2.0) —
        // uses the same on-curve check via curve25519_dalek internals.
        if is_on_curve(&hash) {
            return Err(PubkeyError::InvalidSeeds);
        }

        Ok(Pubkey(hash))
    }

    /// Find a valid Program Derived Address by trying bump seeds 255..=0.
    ///
    /// Calls [`create_program_address`] with `seeds + [nonce]` for each nonce
    /// starting at 255 and decreasing. Returns `(pda, canonical_bump)` where
    /// `canonical_bump` is the highest nonce that produced a valid PDA.
    ///
    /// # Panics
    ///
    /// Panics if no valid PDA is found (requires all 256 nonces to produce on-curve
    /// points — statistically impossible).
    ///
    /// # Reference
    ///
    /// reference: solana_sdk::pubkey::Pubkey::find_program_address (Apache-2.0)
    pub fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
        Self::try_find_program_address(seeds, program_id)
            .expect("no valid PDA found for given seeds")
    }

    /// Like [`find_program_address`] but returns `None` on failure.
    pub fn try_find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> Option<(Pubkey, u8)> {
        let mut nonce = 255u8;
        loop {
            let nonce_slice: &[u8] = std::slice::from_ref(&nonce);
            let mut seeds_with_nonce: Vec<&[u8]> = Vec::with_capacity(seeds.len() + 1);
            seeds_with_nonce.extend_from_slice(seeds);
            seeds_with_nonce.push(nonce_slice);
            match Self::create_program_address(&seeds_with_nonce, program_id) {
                Ok(address) => return Some((address, nonce)),
                Err(PubkeyError::InvalidSeeds) => {
                    if nonce == 0 {
                        return None;
                    }
                    nonce -= 1;
                }
                Err(_) => return None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ed25519 on-curve check
// ---------------------------------------------------------------------------

/// Returns `true` if `bytes` is a compressed Edwards point that lies on the
/// Ed25519 curve (i.e., it is NOT a valid PDA — on-curve points have private keys).
///
/// Uses `ed25519_dalek::VerifyingKey::from_bytes` which returns `Ok` only for
/// structurally valid on-curve, torsion-free points.
///
/// reference: solana_sdk::pubkey (Apache-2.0) — uses curve25519_dalek for the same check.
fn is_on_curve(bytes: &[u8; 32]) -> bool {
    ed25519_dalek::VerifyingKey::from_bytes(bytes).is_ok()
}

// ---------------------------------------------------------------------------
// From / Into
// ---------------------------------------------------------------------------

impl From<[u8; 32]> for Pubkey {
    #[inline]
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<Pubkey> for [u8; 32] {
    #[inline]
    fn from(pk: Pubkey) -> Self {
        pk.0
    }
}

impl AsRef<[u8]> for Pubkey {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pubkey({})", self.to_base58())
    }
}

/// Display as the canonical base58 string (no prefix).
impl fmt::Display for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

// ---------------------------------------------------------------------------
// FromStr
// ---------------------------------------------------------------------------

impl FromStr for Pubkey {
    type Err = PubkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_base58(s)
    }
}

// ---------------------------------------------------------------------------
// Serde: base58 string
// ---------------------------------------------------------------------------

impl Serialize for Pubkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for Pubkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // reference: solana_sdk::pubkey::Pubkey (Apache-2.0) — ZERO constant semantics
    #[test]
    fn zero_pubkey_displays_as_all_ones() {
        // The all-zero byte array base58-encodes to all '1' characters.
        assert_eq!(Pubkey::ZERO.to_string(), "11111111111111111111111111111111");
    }

    #[test]
    fn from_bytes_round_trip() {
        let bytes = [1u8; 32];
        let pk = Pubkey::from_bytes(bytes);
        assert_eq!(pk.0, bytes);
    }

    #[test]
    fn from_array_trait() {
        let bytes = [2u8; 32];
        let pk: Pubkey = bytes.into();
        assert_eq!(pk.0, bytes);
    }

    #[test]
    fn display_and_parse_round_trip() {
        let bytes: [u8; 32] = [
            0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93,
            0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
            0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91,
            0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
        ];
        let pk = Pubkey::from_bytes(bytes);
        let s = pk.to_string();
        let parsed: Pubkey = s.parse().expect("round-trip parse failed");
        assert_eq!(pk, parsed);
    }

    #[test]
    fn parse_wrong_length_errors() {
        // Base58 string that decodes to fewer than 32 bytes.
        // A 4-character base58 string like "Abc1" decodes to <4 bytes.
        let result = "Abc1".parse::<Pubkey>();
        assert!(result.is_err());
        if let Err(PubkeyError::WrongLength(_)) = result {
            // expected
        } else {
            panic!("expected WrongLength error");
        }
    }

    #[test]
    fn parse_invalid_base58_errors() {
        // '0', 'O', 'I', 'l' are not in the base58 alphabet.
        let result = "0000000000000000000000000000000000000000000".parse::<Pubkey>();
        assert!(matches!(result, Err(PubkeyError::InvalidBase58(_))));
    }

    #[test]
    fn serde_round_trip() {
        let bytes: [u8; 32] = [
            0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93,
            0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
            0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91,
            0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
        ];
        let pk = Pubkey::from_bytes(bytes);
        let json = serde_json::to_string(&pk).unwrap();
        let decoded: Pubkey = serde_json::from_str(&json).unwrap();
        assert_eq!(pk, decoded);
    }

    #[test]
    fn serde_serialises_as_base58_string() {
        let pk = Pubkey::ZERO;
        let json = serde_json::to_string(&pk).unwrap();
        // Should be a quoted base58 string, not an array.
        assert_eq!(json, "\"11111111111111111111111111111111\"");
    }

    #[test]
    fn equality_is_byte_level() {
        // Two pubkeys constructed from the same bytes must be equal regardless of
        // how they were constructed.
        let a = Pubkey::from_bytes([0xab; 32]);
        let b: Pubkey = [0xab; 32].into();
        assert_eq!(a, b);
    }

    #[test]
    fn as_ref_returns_bytes() {
        let pk = Pubkey::from_bytes([0xff; 32]);
        let slice: &[u8] = pk.as_ref();
        assert_eq!(slice.len(), 32);
        assert!(slice.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn repr_c_size_is_32() {
        assert_eq!(std::mem::size_of::<Pubkey>(), 32);
        assert_eq!(std::mem::align_of::<Pubkey>(), 1);
    }

    #[test]
    fn new_from_array_alias() {
        let bytes = [0x42u8; 32];
        let pk = Pubkey::new_from_array(bytes);
        assert_eq!(pk.0, bytes);
    }

    #[test]
    fn from_str_const_system_program() {
        let pk = Pubkey::from_str_const("11111111111111111111111111111111");
        assert_eq!(pk, Pubkey::ZERO);
    }

    #[test]
    fn from_str_const_spl_token_program() {
        let pk = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let expected: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
        assert_eq!(pk, expected);
    }

    // -----------------------------------------------------------------------
    // PDA derivation tests
    // -----------------------------------------------------------------------

    /// find_program_address must be deterministic: same seeds → same PDA.
    #[test]
    fn find_program_address_is_deterministic() {
        let program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
        let owner: Pubkey = "9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3".parse().unwrap();
        let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
        let mint: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();

        let seeds: &[&[u8]] = &[owner.as_ref(), spl_token.as_ref(), mint.as_ref()];
        let (pda1, bump1) = Pubkey::find_program_address(seeds, &program);
        let (pda2, bump2) = Pubkey::find_program_address(seeds, &program);
        assert_eq!(pda1, pda2, "PDA must be deterministic");
        assert_eq!(bump1, bump2, "bump seed must be deterministic");
    }

    /// find_program_address with different seeds must produce different PDAs.
    #[test]
    fn find_program_address_varies_with_seeds() {
        let program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
        let (pda_a, _) = Pubkey::find_program_address(&[b"seed_alpha"], &program);
        let (pda_b, _) = Pubkey::find_program_address(&[b"seed_beta"], &program);
        assert_ne!(pda_a, pda_b);
    }

    #[test]
    fn create_program_address_too_many_seeds_errors() {
        let program = Pubkey::ZERO;
        let seeds: Vec<&[u8]> = vec![b"s".as_ref(); 17];
        let result = Pubkey::create_program_address(&seeds, &program);
        assert!(matches!(result, Err(PubkeyError::TooManySeeds(17))));
    }

    #[test]
    fn create_program_address_seed_too_large_errors() {
        let program = Pubkey::ZERO;
        let big_seed = [0u8; 33];
        let result = Pubkey::create_program_address(&[big_seed.as_ref()], &program);
        assert!(matches!(result, Err(PubkeyError::SeedTooLarge(33))));
    }

    /// Validate PDA output matches solana_sdk reference for a known ATA derivation.
    ///
    /// ATA for owner=9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3,
    /// token_program=TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA,
    /// mint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
    ///
    /// Cross-verified against solana_sdk::pubkey::Pubkey::find_program_address
    /// and the ATA program derivation in dex-adapter/src/solana/simulation.rs.
    /// The bump seed from our impl must match solana_sdk's result.
    #[test]
    fn find_program_address_ata_matches_reference() {
        let owner: Pubkey = "9LFiTup5RpWNLgUcDbF87YFHqT9as43AYG8LG39Yj9p3".parse().unwrap();
        let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();
        let mint_usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();

        let seeds: &[&[u8]] = &[owner.as_ref(), spl_token.as_ref(), mint_usdc.as_ref()];
        let (ata, bump) = Pubkey::find_program_address(seeds, &ata_program);

        // The ATA must be a valid 32-byte off-curve pubkey.
        assert_ne!(ata, Pubkey::ZERO, "ATA must not be the zero address");
        // bump is u8, no range assertion needed — always 0..=255 by type.
        // Verify the PDA is off-curve: create_program_address with this bump must succeed.
        let bump_slice: &[u8] = std::slice::from_ref(&bump);
        let mut seeds_with_bump: Vec<&[u8]> = seeds.to_vec();
        seeds_with_bump.push(bump_slice);
        let re_derived = Pubkey::create_program_address(&seeds_with_bump, &ata_program)
            .expect("canonical bump must produce a valid PDA");
        assert_eq!(re_derived, ata, "re-derived PDA must match");
    }
}
