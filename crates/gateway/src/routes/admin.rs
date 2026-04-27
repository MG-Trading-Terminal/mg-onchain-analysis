//! Admin endpoints:
//! - `DELETE /v1/admin/cache/{chain}/{mint}` — manual cache invalidation.
//! - `POST /v1/admin/users` — create a new API user.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use secrecy::Secret;
use tracing::instrument;

use crate::auth::{AuthClaims, scopes};
use crate::auth::user_store::create_user;
use crate::error::GatewayError;
use crate::routes::risk::parse_chain;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// DELETE /v1/admin/cache/{chain}/{mint}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct InvalidateCacheResponse {
    pub invalidated: bool,
}

#[instrument(skip(state, claims), fields(chain = %chain_str, mint = %mint))]
pub async fn invalidate_cache_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Path((chain_str, mint)): Path<(String, String)>,
) -> Result<Json<InvalidateCacheResponse>, GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::ADMIN)?;

    let chain = parse_chain(&chain_str)?;
    let existed = state.risk_cache.invalidate(chain, &mint).await;

    Ok(Json(InvalidateCacheResponse { invalidated: existed }))
}

// ---------------------------------------------------------------------------
// POST /v1/admin/users
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    #[serde(deserialize_with = "deserialize_secret")]
    pub password: Secret<String>,
    pub scopes: Vec<String>,
}

fn deserialize_secret<'de, D>(d: D) -> Result<Secret<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Ok(Secret::new(s))
}

#[derive(Serialize)]
pub struct CreateUserResponse {
    pub username: String,
    pub scopes: Vec<String>,
    pub created_at: String,
}

#[instrument(skip(state, claims, req), fields(username = %req.username))]
pub async fn create_user_handler(
    State(state): State<Arc<AppState>>,
    claims: AuthClaims,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreateUserResponse>), GatewayError> {
    scopes::require_scope(&claims.0.scopes, scopes::scope::ADMIN)?;

    // Validate inputs.
    if req.username.is_empty() || req.username.len() > 128 {
        return Err(GatewayError::InvalidInput(
            "username must be 1–128 characters".into(),
        ));
    }
    if req.scopes.is_empty() {
        return Err(GatewayError::InvalidInput("at least one scope required".into()));
    }
    for scope in &req.scopes {
        if !matches!(scope.as_str(), "read:events" | "read:risk" | "write:analyze" | "admin") {
            return Err(GatewayError::InvalidInput(
                format!("unknown scope '{scope}'. Valid: read:events, read:risk, write:analyze, admin"),
            ));
        }
    }

    let user = create_user(
        &state.store,
        &req.username,
        &req.password,
        &req.scopes,
        &state.config.gateway.auth.argon2_params,
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateUserResponse {
            username: user.username,
            scopes: user.scopes,
            created_at: user.created_at.to_rfc3339(),
        }),
    ))
}
