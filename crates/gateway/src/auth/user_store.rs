//! `auth_users` Postgres table — user creation and credential lookup.

use chrono::{DateTime, Utc};
use secrecy::Secret;
use sqlx::Row;
use tracing::instrument;

use mg_onchain_storage::pg::PgStore;

use crate::auth::argon::{hash_password, verify_password};
use crate::config::Argon2Params;
use crate::error::GatewayError;

// ---------------------------------------------------------------------------
// User record
// ---------------------------------------------------------------------------

/// A record from the `auth_users` table.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub password_hash: String,
    pub scopes: Vec<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// CRUD operations
// ---------------------------------------------------------------------------

/// Create a new user, hashing the password with Argon2id.
///
/// Returns `Err(GatewayError::Conflict)` if the username already exists.
#[instrument(skip(store, password, argon2_params), fields(username))]
pub async fn create_user(
    store: &PgStore,
    username: &str,
    password: &Secret<String>,
    scopes: &[String],
    argon2_params: &Argon2Params,
) -> Result<AuthUser, GatewayError> {
    let hash = hash_password(password, argon2_params)
        .map_err(GatewayError::Internal)?;

    let result = sqlx::query(
        r#"
        INSERT INTO auth_users (username, password_hash, scopes)
        VALUES ($1, $2, $3)
        RETURNING username, password_hash, scopes, enabled, created_at, last_login_at
        "#,
    )
    .bind(username)
    .bind(&hash)
    .bind(scopes)
    .fetch_one(store.pool())
    .await;

    match result {
        Ok(row) => parse_user_row(&row).map_err(GatewayError::Internal),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            Err(GatewayError::Conflict(format!("Username '{username}' already exists.")))
        }
        Err(e) => Err(GatewayError::Internal(anyhow::anyhow!("create_user DB error: {e}"))),
    }
}

/// Look up a user by username and verify the password.
///
/// Returns the `AuthUser` on success, or `Err(GatewayError::Unauthenticated)` on
/// bad credentials (including disabled account).
#[instrument(skip(store, password), fields(username))]
pub async fn verify_credentials(
    store: &PgStore,
    username: &str,
    password: &Secret<String>,
) -> Result<AuthUser, GatewayError> {
    let row = sqlx::query(
        r#"
        SELECT username, password_hash, scopes, enabled, created_at, last_login_at
        FROM auth_users
        WHERE username = $1
        "#,
    )
    .bind(username)
    .fetch_optional(store.pool())
    .await
    .map_err(|e| GatewayError::Internal(anyhow::anyhow!("verify_credentials DB error: {e}")))?;

    let user = match row {
        Some(r) => parse_user_row(&r).map_err(GatewayError::Internal)?,
        None => {
            // Constant-time: still hash even on unknown user to prevent timing attacks.
            let _ = verify_password(&Secret::new("dummy".to_string()), "$argon2id$v=19$m=4096,t=1,p=1$YWFhYWFhYWFhYWFhYWFhYQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
            return Err(GatewayError::Unauthenticated);
        }
    };

    if !user.enabled {
        return Err(GatewayError::Unauthenticated);
    }

    verify_password(password, &user.password_hash)?;

    // Update last_login_at (best-effort, not critical).
    let _ = sqlx::query("UPDATE auth_users SET last_login_at = NOW() WHERE username = $1")
        .bind(username)
        .execute(store.pool())
        .await;

    Ok(user)
}

fn parse_user_row(row: &sqlx::postgres::PgRow) -> anyhow::Result<AuthUser> {
    Ok(AuthUser {
        username: row.try_get("username")?,
        password_hash: row.try_get("password_hash")?,
        scopes: row.try_get::<Vec<String>, _>("scopes")?,
        enabled: row.try_get("enabled")?,
        created_at: row.try_get("created_at")?,
        last_login_at: row.try_get("last_login_at")?,
    })
}
