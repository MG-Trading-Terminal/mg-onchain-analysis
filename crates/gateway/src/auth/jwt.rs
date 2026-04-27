//! JWT signing (Ed25519 / EdDSA) and verification.
//!
//! Uses `jsonwebtoken` crate with the `Algorithm::EdDSA` algorithm.
//! Keys are loaded from PEM files at startup and stored in `AppState`.
//!
//! # Key format
//!
//! Generate with:
//! ```sh
//! openssl genpkey -algorithm ed25519 -out priv.ed25519
//! openssl pkey -in priv.ed25519 -pubout -out pub.ed25519
//! ```
//!
//! The private key file is PKCS#8 PEM (openssl default for Ed25519).
//! `ed25519-dalek` reads it via `SigningKey::from_pkcs8_pem`.

use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey, pkcs8::DecodePrivateKey};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::GatewayError;

// ---------------------------------------------------------------------------
// JWT Claims
// ---------------------------------------------------------------------------

/// JWT payload claims issued by this gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — API user name.
    pub sub: String,
    /// Issuer — `"mg-onchain"`.
    pub iss: String,
    /// Audience — `"mg-onchain-api"`.
    pub aud: String,
    /// Issued-at (Unix seconds).
    pub iat: i64,
    /// Expiry (Unix seconds).
    pub exp: i64,
    /// JWT ID — UUID v4 for audit log.
    pub jti: String,
    /// Scopes granted to this token.
    pub scopes: Vec<String>,
}

// ---------------------------------------------------------------------------
// JwtKeys — loaded once at startup
// ---------------------------------------------------------------------------

/// Loaded Ed25519 key material for JWT sign + verify.
///
/// `Clone` is cheap — keys are stored in `Arc<JwtKeys>` in `AppState`.
pub struct JwtKeys {
    /// Key ID included in `kid` header claim. Derived from public key bytes (hex prefix).
    pub kid: String,
    /// Encoding key (private) for `jsonwebtoken`.
    pub encoding_key: EncodingKey,
    /// Decoding key (public) for `jsonwebtoken`.
    pub decoding_key: DecodingKey,
    /// Raw public key bytes (32 bytes) for JWKS serialization.
    pub public_key_bytes: [u8; 32],
}

impl JwtKeys {
    /// Load key material from a PKCS#8 PEM file (Ed25519 private key).
    pub fn from_pem_file(path: &str) -> anyhow::Result<Self> {
        let pem = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read JWT signing key {path}: {e}"))?;
        Self::from_pem_str(&pem)
    }

    /// Load key material from a PEM string (for tests).
    pub fn from_pem_str(pem: &str) -> anyhow::Result<Self> {
        let signing_key = SigningKey::from_pkcs8_pem(pem)
            .map_err(|e| anyhow::anyhow!("invalid Ed25519 private key PEM: {e}"))?;

        let verifying_key: VerifyingKey = signing_key.verifying_key();
        let public_key_bytes = verifying_key.to_bytes();

        // kid = first 8 hex chars of public key bytes
        let kid = hex::encode(&public_key_bytes[..4]);

        // jsonwebtoken requires PKCS8 DER for Ed25519
        let private_der = signing_key_to_pkcs8_der(&signing_key)?;
        let encoding_key = EncodingKey::from_ed_der(&private_der);

        let decoding_key = DecodingKey::from_ed_der(verifying_key.as_bytes());

        Ok(Self { kid, encoding_key, decoding_key, public_key_bytes })
    }

    /// Sign a JWT with the given claims.
    pub fn sign(&self, claims: &Claims) -> anyhow::Result<String> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        encode(&header, claims, &self.encoding_key)
            .map_err(|e| anyhow::anyhow!("JWT sign error: {e}"))
    }

    /// Verify a JWT and return the decoded claims.
    pub fn verify(&self, token: &str, issuer: &str, audience: &str) -> Result<Claims, GatewayError> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.validate_exp = true;

        decode::<Claims>(token, &self.decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|e| {
                tracing::debug!(error = %e, "JWT verification failed");
                GatewayError::Unauthenticated
            })
    }
}

// ---------------------------------------------------------------------------
// Token minting helper
// ---------------------------------------------------------------------------

/// Build `Claims` for a new token.
pub fn build_claims(
    sub: &str,
    scopes: Vec<String>,
    issuer: &str,
    audience: &str,
    expiry_hours: u64,
) -> Claims {
    let now = Utc::now().timestamp();
    Claims {
        sub: sub.to_string(),
        iss: issuer.to_string(),
        aud: audience.to_string(),
        iat: now,
        exp: now + (expiry_hours as i64 * 3600),
        jti: Uuid::new_v4().to_string(),
        scopes,
    }
}

// ---------------------------------------------------------------------------
// Private helper: SigningKey → PKCS8 DER
// ---------------------------------------------------------------------------

fn signing_key_to_pkcs8_der(key: &SigningKey) -> anyhow::Result<Vec<u8>> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let doc = key
        .to_pkcs8_der()
        .map_err(|e| anyhow::anyhow!("PKCS8 DER encode failed: {e}"))?;
    Ok(doc.as_bytes().to_vec())
}

// ---------------------------------------------------------------------------
// Generate a test keypair (only in test builds)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub fn generate_test_keys() -> JwtKeys {
    use rand_core::{OsRng, RngCore};
    let mut secret_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let verifying_key = signing_key.verifying_key();
    let public_key_bytes = verifying_key.to_bytes();
    let kid = hex::encode(&public_key_bytes[..4]);

    let private_der = signing_key_to_pkcs8_der(&signing_key).expect("test key DER");
    let encoding_key = EncodingKey::from_ed_der(&private_der);
    let decoding_key = DecodingKey::from_ed_der(verifying_key.as_bytes());

    JwtKeys { kid, encoding_key, decoding_key, public_key_bytes }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let keys = generate_test_keys();
        let claims = build_claims("bot-trader-prod", vec!["read:events".into()], "mg-onchain", "mg-onchain-api", 24);
        let token = keys.sign(&claims).expect("sign must succeed");
        let decoded = keys.verify(&token, "mg-onchain", "mg-onchain-api").expect("verify must succeed");
        assert_eq!(decoded.sub, "bot-trader-prod");
        assert_eq!(decoded.scopes, vec!["read:events"]);
    }

    #[test]
    fn expired_token_rejected() {
        let keys = generate_test_keys();
        // Build claims with exp in the past.
        let mut claims = build_claims("u", vec![], "mg-onchain", "mg-onchain-api", 0);
        claims.exp = claims.iat - 3600; // 1 hour in the past
        let token = keys.sign(&claims).expect("sign must succeed");
        let result = keys.verify(&token, "mg-onchain", "mg-onchain-api");
        assert!(matches!(result, Err(GatewayError::Unauthenticated)));
    }

    #[test]
    fn wrong_issuer_rejected() {
        let keys = generate_test_keys();
        let claims = build_claims("u", vec![], "wrong-issuer", "mg-onchain-api", 24);
        let token = keys.sign(&claims).expect("sign");
        // verify against correct issuer — should fail
        let result = keys.verify(&token, "mg-onchain", "mg-onchain-api");
        assert!(matches!(result, Err(GatewayError::Unauthenticated)));
    }

    #[test]
    fn multiple_scopes_preserved() {
        let keys = generate_test_keys();
        let scopes = vec!["read:events".into(), "read:risk".into(), "write:analyze".into()];
        let claims = build_claims("svc", scopes.clone(), "mg-onchain", "mg-onchain-api", 24);
        let token = keys.sign(&claims).unwrap();
        let decoded = keys.verify(&token, "mg-onchain", "mg-onchain-api").unwrap();
        assert_eq!(decoded.scopes, scopes);
    }
}
