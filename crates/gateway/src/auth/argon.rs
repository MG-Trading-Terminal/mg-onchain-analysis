//! Argon2id password hashing and verification.
//!
//! Used for `auth_users` password storage. Parameters from `GatewayConfig.auth.argon2_params`.
//!
//! # Secret hygiene
//!
//! - Passwords are accepted as `secrecy::Secret<String>` to prevent accidental logging.
//! - Hash strings are stored in Postgres — never logged.

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand_core::OsRng;
use secrecy::{ExposeSecret, Secret};

use crate::config::Argon2Params;
use crate::error::GatewayError;

/// Hash a plaintext password using Argon2id.
///
/// Returns the PHC string format: `$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>`.
pub fn hash_password(
    password: &Secret<String>,
    params: &Argon2Params,
) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = build_argon2(params)?;
    let hash = argon2
        .hash_password(password.expose_secret().as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash error: {e}"))?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against a stored Argon2id hash.
///
/// Returns `Ok(())` on match, `Err(GatewayError::Unauthenticated)` on mismatch.
pub fn verify_password(
    password: &Secret<String>,
    hash_str: &str,
) -> Result<(), GatewayError> {
    let parsed = PasswordHash::new(hash_str)
        .map_err(|_| GatewayError::Unauthenticated)?;
    Argon2::default()
        .verify_password(password.expose_secret().as_bytes(), &parsed)
        .map_err(|_| GatewayError::Unauthenticated)
}

fn build_argon2(params: &Argon2Params) -> anyhow::Result<Argon2<'static>> {
    let argon2_params = argon2::Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        None,
    )
    .map_err(|e| anyhow::anyhow!("invalid argon2 params: {e}"))?;
    Ok(Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params() -> Argon2Params {
        Argon2Params {
            memory_kib: 4096, // reduced for test speed
            iterations: 1,
            parallelism: 1,
        }
    }

    #[test]
    fn hash_and_verify_roundtrip() {
        let password = Secret::new("correct-horse-battery-staple".to_string());
        let hash = hash_password(&password, &test_params()).expect("hash must succeed");
        assert!(hash.starts_with("$argon2id$"), "hash must use argon2id PHC format");
        verify_password(&password, &hash).expect("verify must succeed for correct password");
    }

    #[test]
    fn wrong_password_rejected() {
        let password = Secret::new("correct-password".to_string());
        let wrong = Secret::new("wrong-password".to_string());
        let hash = hash_password(&password, &test_params()).expect("hash");
        let result = verify_password(&wrong, &hash);
        assert!(matches!(result, Err(GatewayError::Unauthenticated)));
    }

    #[test]
    fn hash_is_different_each_call() {
        // Salt is random → hashes differ even for identical password.
        let password = Secret::new("same-password".to_string());
        let h1 = hash_password(&password, &test_params()).unwrap();
        let h2 = hash_password(&password, &test_params()).unwrap();
        assert_ne!(h1, h2, "hashes must differ due to random salt");
        // Both must verify correctly.
        verify_password(&password, &h1).unwrap();
        verify_password(&password, &h2).unwrap();
    }
}
