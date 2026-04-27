//! Postgres pool construction and migration runner.
//!
//! # D-A: Auto-migrate on startup
//!
//! `run_migrations` uses the sqlx runtime `Migrator` pointing at the workspace
//! `migrations/postgres/` directory. Migration files follow the Flyway-style
//! `V{seq}__{description}.sql` convention (e.g. `V00001__init.sql`). sqlx's
//! compile-time `migrate!` macro does NOT support the `V` prefix; we use the
//! runtime `Migrator` which accepts both conventions (same approach as
//! `crates/storage/src/migrations.rs`).
//!
//! `main.rs` gates this call on the `--no-migrate` CLI flag.

use anyhow::Context as _;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::{info, warn};

use crate::config::PostgresConfig;

/// Connect to Postgres using the given config.
///
/// Applies an exponential-backoff retry loop up to `config.connect_retries`
/// attempts with 2s base delay. Returns the pool on first success.
///
/// # D-A
///
/// The pool is shared by all stores (each store clones the `Arc`-backed pool
/// cheaply). Migrations run via `run_migrations` after this returns.
///
/// # Errors
///
/// Returns an error if all retry attempts are exhausted.
pub async fn connect_postgres(config: &PostgresConfig) -> anyhow::Result<PgPool> {
    let url = &config.url;
    let max_connections = config.max_connections;
    let retries = config.connect_retries;

    let mut last_err = None;
    for attempt in 1..=(retries + 1) {
        match PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
        {
            Ok(pool) => {
                info!(
                    max_connections,
                    attempt,
                    "postgres pool connected"
                );
                return Ok(pool);
            }
            Err(e) => {
                let delay_ms = 2000u64 * (1u64 << (attempt - 1).min(4));
                warn!(
                    attempt,
                    max_attempts = retries + 1,
                    delay_ms,
                    error = %e,
                    "postgres connection failed — retrying"
                );
                last_err = Some(e);
                if attempt <= retries {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    Err(last_err
        .expect("retry loop must have set last_err")
        .into())
}

/// Apply pending migrations from `migrations/postgres/`.
///
/// Uses the sqlx runtime `Migrator` which accepts Flyway-style
/// `V{seq}__{description}.sql` filenames. The `_sqlx_migrations` tracking
/// table is created on first run; re-running on a current DB is a no-op.
///
/// The path `migrations/postgres` is resolved relative to the process working
/// directory, which is the workspace root when running the service binary.
///
/// # D-A: Auto-migrate
///
/// Called from `main.rs` unless `--no-migrate` flag is passed.
///
/// # Errors
///
/// Returns an error if any migration fails to apply.
pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    info!("running sqlx migrations from migrations/postgres/");
    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new("migrations/postgres"))
        .await
        .context("sqlx migrator init failed — check migrations/postgres/ path")?;
    migrator
        .run(pool)
        .await
        .context("sqlx migrate failed")?;
    info!("migrations applied successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the PostgresConfig defaults produce a non-empty URL.
    #[test]
    fn postgres_config_default_url_is_non_empty() {
        let cfg = PostgresConfig::default();
        assert!(!cfg.url.is_empty(), "default PostgresConfig URL must not be empty");
        assert!(
            cfg.url.starts_with("postgres://"),
            "default URL must use postgres:// scheme"
        );
    }

    /// Verify retry count default is non-zero.
    #[test]
    fn postgres_config_default_connect_retries_nonzero() {
        let cfg = PostgresConfig::default();
        assert!(
            cfg.connect_retries > 0,
            "default connect_retries must be > 0 for production resilience"
        );
    }

    /// Live Postgres connect + migrate test — gated by DATABASE_URL env var.
    #[tokio::test]
    #[ignore = "requires live Postgres (set DATABASE_URL)"]
    async fn connect_and_migrate_roundtrip() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL not set");
        let config = PostgresConfig {
            url,
            max_connections: 2,
            connect_retries: 1,
        };
        let pool = connect_postgres(&config).await.expect("connect must succeed");
        run_migrations(&pool).await.expect("migrate must succeed");
    }
}
