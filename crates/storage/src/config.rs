//! Storage configuration — loaded from TOML / environment variables.
//!
//! See `config/storage.toml.example` for a populated example.
//!
//! # sqlx compile-time query macros
//!
//! `sqlx::query!` and `sqlx::query_as!` require a live Postgres connection at
//! compile time (they verify column types against the live schema). This crate
//! uses **runtime queries** (`sqlx::query` / `sqlx::query_as`) everywhere so
//! that `cargo check --workspace` and `cargo test -p mg-onchain-storage` work
//! without a running database.
//!
//! If you want compile-time verification in the future:
//! 1. Set `DATABASE_URL` in your environment (or `.env` file).
//! 2. Run `cargo sqlx prepare` to snapshot the schema into `.sqlx/`.
//! 3. Gate the `sqlx::query!` calls behind `#[cfg(feature = "sqlx-check")]`.

use serde::{Deserialize, Serialize};

/// Top-level storage configuration, loaded from `config/storage.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StorageConfig {
    /// PostgreSQL connection URL.
    /// Example: `postgres://user:password@localhost:5432/mg_onchain`
    pub postgres_url: String,

    /// If `true`, run Postgres migrations at startup.
    /// Set to `false` in environments where migrations are applied out-of-band.
    #[serde(default = "default_true")]
    pub migrations_auto_apply: bool,
}

fn default_true() -> bool {
    true
}

impl StorageConfig {
    /// Construct from environment variables for use in integration tests.
    ///
    /// Falls back to localhost defaults if env vars are not set — suitable
    /// for running `docker compose up pg` locally.
    pub fn from_env() -> Self {
        Self {
            postgres_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/mg_onchain".into()),
            migrations_auto_apply: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env_fallback_defaults() {
        // Without env vars, from_env() returns localhost defaults.
        let cfg = StorageConfig::from_env();
        assert!(!cfg.postgres_url.is_empty());
        assert!(cfg.migrations_auto_apply);
    }

    #[test]
    fn config_toml_deserialize() {
        let toml_str = r#"
            postgres_url = "postgres://user:pass@host:5432/db"
            migrations_auto_apply = false
        "#;
        let cfg: StorageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.postgres_url, "postgres://user:pass@host:5432/db");
        assert!(!cfg.migrations_auto_apply);
    }

    #[test]
    fn config_toml_defaults_apply() {
        // Only required fields — migrations_auto_apply should default to true.
        let toml_str = r#"
            postgres_url = "postgres://localhost/test"
        "#;
        let cfg: StorageConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.migrations_auto_apply);
    }
}
