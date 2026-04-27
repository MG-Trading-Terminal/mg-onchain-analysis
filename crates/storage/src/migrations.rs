//! Postgres migration runner.
//!
//! Uses `sqlx::migrate::Migrator` with the compile-time embedded path to the
//! `migrations/postgres/` directory.
//!
//! sqlx automatically creates and maintains a `_sqlx_migrations` tracking
//! table in the target database. Re-running applied migrations is a no-op.
//!
//! # File naming convention
//!
//! Migration files follow the Flyway-style `V{seq}__{description}.sql` convention
//! (e.g. `V00001__init.sql`). sqlx's compile-time `migrate!` macro does NOT support
//! the `V` prefix — it expects plain integer prefixes (e.g. `1__foo.sql`). We use
//! the **runtime** `sqlx::migrate::Migrator` which accepts both conventions.
//!
//! This keeps the file naming consistent with the existing V00001 migration and
//! avoids renaming applied migrations (a footgun in any migration system).
//!
//! # Startup integration
//!
//! `StorageHandle::new` calls `run` when `StorageConfig.migrations_auto_apply = true`.

use std::path::Path;

use sqlx::PgPool;
use tracing::info;

use crate::error::StorageError;

/// Path to the Postgres migrations directory, relative to the workspace root.
///
/// The path is relative to the binary's working directory at runtime. For the
/// service binary (`crates/server`), the working directory is the workspace root.
/// For tests, it resolves relative to the crate root (two levels up from
/// `crates/storage/`).
const MIGRATIONS_DIR: &str = "migrations/postgres";

/// Apply all pending Postgres migrations from `migrations/postgres/`.
///
/// Uses sqlx's runtime `Migrator` which accepts `V{seq}__{description}.sql`
/// filenames (Flyway-style). The `_sqlx_migrations` table is created on first run.
pub async fn run(pool: &PgPool) -> Result<(), StorageError> {
    let path = Path::new(MIGRATIONS_DIR);
    let migrator = sqlx::migrate::Migrator::new(path)
        .await
        .map_err(StorageError::Migration)?;
    migrator
        .run(pool)
        .await
        .map_err(StorageError::Migration)?;
    info!(dir = MIGRATIONS_DIR, "postgres migrations applied");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Verify the migrations module compiles. Actual migration application requires
    /// a live Postgres instance and is tested via `#[ignore]` integration tests.
    #[test]
    fn migrations_module_compiles() {
        // Presence of this test confirms compilation succeeded.
    }
}
