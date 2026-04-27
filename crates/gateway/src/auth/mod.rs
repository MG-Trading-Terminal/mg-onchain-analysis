//! Authentication: JWT middleware, Ed25519 sign/verify, Argon2id user store, scope checks.

pub mod argon;
pub mod jwt;
pub mod scopes;
pub mod user_store;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// AuthClaims extractor
// ---------------------------------------------------------------------------

/// Axum extractor that validates the Bearer JWT and injects the decoded claims.
///
/// Routes that need authentication `#[derive(Extension)]` or explicitly call
/// `AuthClaims::from_request_parts` in the handler signature.
#[derive(Debug, Clone)]
pub struct AuthClaims(pub jwt::Claims);

impl FromRequestParts<Arc<AppState>> for AuthClaims {
    type Rejection = GatewayError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        // Extract token from Authorization: Bearer <token> or ?token= query param.
        let token = extract_bearer_token(parts)?;
        let claims = state
            .jwt_keys
            .verify(&token, &state.config.gateway.auth.jwt_issuer, &state.config.gateway.auth.jwt_audience)?;
        Ok(AuthClaims(claims))
    }
}

/// Extract the raw JWT string from the request.
///
/// Precedence: `Authorization: Bearer <token>` > `?token=<token>` query param.
pub fn extract_bearer_token(parts: &Parts) -> Result<String, GatewayError> {
    // 1. Authorization header
    if let Some(auth_header) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        let val = auth_header.to_str().map_err(|_| GatewayError::Unauthenticated)?;
        if let Some(token) = val.strip_prefix("Bearer ") {
            return Ok(token.to_string());
        }
    }

    // 2. ?token= query parameter
    if let Some(query) = parts.uri.query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key.as_ref() == "token" {
                return Ok(value.into_owned());
            }
        }
    }

    Err(GatewayError::Unauthenticated)
}
