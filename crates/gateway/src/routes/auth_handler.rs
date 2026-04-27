//! `POST /v1/auth/token` — issue JWT from credentials.
//! `GET /v1/.well-known/jwks.json` — publish Ed25519 public key set.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use secrecy::Secret;
use tracing::instrument;

use crate::auth::jwt::build_claims;
use crate::auth::user_store::verify_credentials;
use crate::error::GatewayError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AuthRequest {
    pub username: String,
    /// Password is wrapped in `Secret` so it is never logged via `Debug`.
    #[serde(deserialize_with = "deserialize_secret")]
    pub password: Secret<String>,
}

fn deserialize_secret<'de, D>(d: D) -> Result<Secret<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Ok(Secret::new(s))
}

#[derive(Serialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub scopes: Vec<String>,
}

// ---------------------------------------------------------------------------
// POST /v1/auth/token
// ---------------------------------------------------------------------------

#[instrument(skip(state, req), fields(username = %req.username))]
pub async fn issue_token_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, GatewayError> {
    if req.username.is_empty() {
        return Err(GatewayError::InvalidInput("username is required".into()));
    }

    let user = verify_credentials(&state.store, &req.username, &req.password).await?;

    let expiry_hours = state.config.gateway.auth.jwt_expiry_hours;
    let claims = build_claims(
        &user.username,
        user.scopes.clone(),
        &state.config.gateway.auth.jwt_issuer,
        &state.config.gateway.auth.jwt_audience,
        expiry_hours,
    );

    let token = state
        .jwt_keys
        .sign(&claims)
        .map_err(GatewayError::Internal)?;

    Ok(Json(AuthResponse {
        access_token: token,
        token_type: "Bearer",
        expires_in: expiry_hours * 3600,
        scopes: user.scopes,
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/.well-known/jwks.json
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct JwksResponse {
    pub keys: Vec<Jwk>,
}

#[derive(Serialize)]
pub struct Jwk {
    pub kty: &'static str,
    pub crv: &'static str,
    /// Base64url-encoded public key bytes (32 bytes).
    pub x: String,
    pub kid: String,
    #[serde(rename = "use")]
    pub use_: &'static str,
    pub alg: &'static str,
}

pub async fn jwks_handler(
    State(state): State<Arc<AppState>>,
) -> Json<JwksResponse> {
    let x = URL_SAFE_NO_PAD.encode(state.jwt_keys.public_key_bytes);
    Json(JwksResponse {
        keys: vec![Jwk {
            kty: "OKP",
            crv: "Ed25519",
            x,
            kid: state.jwt_keys.kid.clone(),
            use_: "sig",
            alg: "EdDSA",
        }],
    })
}
