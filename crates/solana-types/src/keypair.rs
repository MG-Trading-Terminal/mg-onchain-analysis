//! Ed25519 keypair wrapper for Solana signing operations.
//!
//! Provides a minimal keypair type backed by `ed25519_dalek::SigningKey`.
//! Used exclusively for the `simulateTransaction` path in `dex-adapter`
//! (deterministic throwaway keypairs derived from a SHA-256 seed; never
//! funded, never submitted to the actual network).
//!
//! # Design
//!
//! We wrap `ed25519_dalek::SigningKey` directly — no `solana_sdk` in the
//! dependency tree. The public key bytes are the 32-byte compressed Edwards
//! point, which is exactly what Solana uses as a wallet address.
//!
//! The `from_seed_bytes` constructor matches `solana_sdk::signature::Keypair`'s
//! `from_seed` behaviour: the seed is the 32-byte private key material.
//!
//! The `from_secret_bytes` constructor accepts a 64-byte `[seed || public_key]`
//! array, matching the `solana_sdk::signature::Keypair::from_bytes` layout.
//!
//! # Reference
//!
//! reference: solana_sdk::signature::Keypair (Apache-2.0) — constructor semantics,
//!            64-byte secret layout (seed + pubkey), and sign_message semantics consulted.
//! reference: RFC 8032 (Ed25519) — the underlying signing algorithm.

use ed25519_dalek::{SigningKey, Signer as _};

use crate::{Pubkey, Signature};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned when constructing a [`Keypair`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeypairError {
    /// The 64-byte secret array has the wrong public key half.
    #[error("secret bytes public-key half does not match derived public key")]
    PublicKeyMismatch,
}

// ---------------------------------------------------------------------------
// Keypair
// ---------------------------------------------------------------------------

/// An Ed25519 signing keypair for Solana simulation transactions.
///
/// The keypair holds both the private signing material (`ed25519_dalek::SigningKey`)
/// and the corresponding `Pubkey` (derived from the verifying key bytes).
///
/// # Security note
///
/// These keypairs are throwaway simulation signers. They are deterministically
/// derived from `(token, pool, path_index)` via SHA-256 in `simulation.rs` and
/// are never funded. `simulateTransaction` with `sigVerify: false` does not
/// validate the signature; the keypair exists only to satisfy the structural
/// requirement for a signed transaction.
pub struct Keypair {
    signing: SigningKey,
    pubkey: Pubkey,
}

impl core::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keypair")
            .field("pubkey", &self.pubkey)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Keypair {
    fn eq(&self, other: &Self) -> bool {
        // Equality based on the signing key bytes (seed) and pubkey.
        self.signing.to_bytes() == other.signing.to_bytes()
    }
}

impl Keypair {
    /// Construct from a 32-byte seed (private key material).
    ///
    /// The public key is derived from the seed via the Ed25519 scalar multiplication.
    ///
    /// Matches `solana_sdk::signer::SeedDerivable::from_seed(&seed)` semantics.
    ///
    /// reference: solana_sdk::signature::Keypair::from_seed (Apache-2.0)
    pub fn from_seed_bytes(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let pubkey = Pubkey(signing.verifying_key().to_bytes());
        Self { signing, pubkey }
    }

    /// Construct from a 64-byte `[seed (32) || public_key (32)]` array.
    ///
    /// Verifies that the public-key half matches the key derived from the seed.
    /// Returns [`KeypairError::PublicKeyMismatch`] if they differ.
    ///
    /// Matches `solana_sdk::signature::Keypair::from_bytes` semantics.
    ///
    /// reference: solana_sdk::signature::Keypair::from_bytes (Apache-2.0)
    pub fn from_secret_bytes(secret: &[u8; 64]) -> Result<Self, KeypairError> {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&secret[..32]);
        let signing = SigningKey::from_bytes(&seed);
        let derived_pubkey = signing.verifying_key().to_bytes();
        let claimed_pubkey = &secret[32..64];
        if derived_pubkey != claimed_pubkey {
            return Err(KeypairError::PublicKeyMismatch);
        }
        let pubkey = Pubkey(derived_pubkey);
        Ok(Self { signing, pubkey })
    }

    /// Generate a random keypair using the OS RNG.
    ///
    /// Used only in tests where a throwaway keypair is needed and determinism
    /// is not required. [`Default`] delegates to this constructor.
    pub fn new() -> Self {
        use ed25519_dalek::SigningKey;
        use rand_core::OsRng;
        let signing = SigningKey::generate(&mut OsRng);
        let pubkey = Pubkey(signing.verifying_key().to_bytes());
        Self { signing, pubkey }
    }

    /// Return the public key (Solana wallet address corresponding to this keypair).
    #[inline]
    pub fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    /// Sign a message, returning a 64-byte Ed25519 signature.
    ///
    /// reference: solana_sdk::signer::Signer::sign_message (Apache-2.0)
    pub fn sign_message(&self, msg: &[u8]) -> Signature {
        let sig = self.signing.sign(msg);
        Signature(sig.to_bytes())
    }
}

impl Default for Keypair {
    /// Generate a random keypair. Delegates to [`Keypair::new`].
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::VerifyingKey;

    #[test]
    fn from_seed_bytes_round_trip_pubkey() {
        let seed = [0x42u8; 32];
        let kp = Keypair::from_seed_bytes(&seed);
        // Pubkey must be deterministic for the same seed.
        let kp2 = Keypair::from_seed_bytes(&seed);
        assert_eq!(kp.pubkey(), kp2.pubkey(), "same seed → same pubkey");
    }

    #[test]
    fn from_seed_bytes_different_seeds_produce_different_pubkeys() {
        let kp_a = Keypair::from_seed_bytes(&[0xAAu8; 32]);
        let kp_b = Keypair::from_seed_bytes(&[0xBBu8; 32]);
        assert_ne!(kp_a.pubkey(), kp_b.pubkey());
    }

    #[test]
    fn sign_and_verify_with_raw_ed25519() {
        let seed = [0x11u8; 32];
        let kp = Keypair::from_seed_bytes(&seed);
        let msg = b"hello solana";
        let sig = kp.sign_message(msg);

        // Verify using raw ed25519_dalek::VerifyingKey.
        let vk = VerifyingKey::from_bytes(kp.pubkey().as_bytes())
            .expect("pubkey must be a valid verifying key");
        let ed_sig = ed25519_dalek::Signature::from_bytes(&sig.0);
        vk.verify_strict(msg, &ed_sig)
            .expect("signature must verify against the signing key's pubkey");
    }

    #[test]
    fn from_secret_bytes_valid() {
        let seed = [0x33u8; 32];
        let kp1 = Keypair::from_seed_bytes(&seed);
        let mut secret = [0u8; 64];
        secret[..32].copy_from_slice(&seed);
        secret[32..].copy_from_slice(kp1.pubkey().as_bytes());
        let kp2 = Keypair::from_secret_bytes(&secret).expect("valid secret bytes");
        assert_eq!(kp1.pubkey(), kp2.pubkey());
    }

    #[test]
    fn from_secret_bytes_mismatch_errors() {
        let seed = [0x44u8; 32];
        let mut secret = [0u8; 64];
        secret[..32].copy_from_slice(&seed);
        // Deliberately wrong pubkey half.
        secret[32..].copy_from_slice(&[0x00u8; 32]);
        let result = Keypair::from_secret_bytes(&secret);
        assert_eq!(result, Err(KeypairError::PublicKeyMismatch));
    }

    #[test]
    fn new_produces_unique_keypairs() {
        let kp_a = Keypair::new();
        let kp_b = Keypair::new();
        // Two random keypairs should essentially never be equal.
        assert_ne!(kp_a.pubkey(), kp_b.pubkey());
    }
}
