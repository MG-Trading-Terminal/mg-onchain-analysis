//! Bearer token management.
//!
//! The SDK wraps the token in `secrecy::SecretString` so that accidental
//! `{:?}` or `{}` formatting on any struct containing it does NOT leak the
//! credential to logs.
//!
//! # Rotation
//!
//! Token refresh is the consumer's responsibility — the SDK exposes
//! `OnchainAnalysisClient::refresh_token` for in-place rotation without
//! rebuilding the client. The new token is atomically stored under an
//! `Arc<RwLock<...>>` so concurrent HTTP requests get the new value on
//! their next `Authorization` header construction.

use secrecy::{ExposeSecret, SecretString};

/// Holds the bearer token, never exposed through `Debug`.
///
/// The `Secret` wrapper ensures accidental `{:?}` on this type prints
/// `[REDACTED]`, not the raw credential value.
#[derive(Clone)]
pub struct BearerToken(SecretString);

impl BearerToken {
    /// Construct from any string. The secret is moved in and zeroed on drop.
    pub fn new(token: impl Into<String>) -> Self {
        Self(SecretString::new(token.into()))
    }

    /// Borrow the raw token value for header construction.
    ///
    /// Keep the scope of `expose_secret()` narrow — call this only inside
    /// `Authorization: Bearer <token>` header construction.
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0.expose_secret())
    }

    /// Return a reference to the raw string (for diagnostic purposes only —
    /// never log this).
    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

/// `Debug` prints `[REDACTED]` to prevent accidental log leakage.
impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_debug_does_not_leak() {
        let t = BearerToken::new("super-secret-jwt.payload.sig");
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("super-secret"), "debug output leaked token: {dbg}");
        assert!(dbg.contains("REDACTED"), "expected REDACTED in debug output: {dbg}");
    }

    #[test]
    fn bearer_token_header_value_format() {
        let t = BearerToken::new("mytoken");
        assert_eq!(t.header_value(), "Bearer mytoken");
    }
}
