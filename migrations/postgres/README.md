# Postgres Migrations

Migration tool: **sqlx migrate** (sqlx-cli).

## Naming convention

```
V{seq}__{description}.sql
```

- `{seq}`: zero-padded 5-digit integer (e.g. `00001`, `00002`).
- `{description}`: snake_case description (e.g. `init`, `add_lp_locker_column`).

## Applying migrations

### Via sqlx-cli (manual)

```bash
# Install sqlx-cli if not already present
cargo install sqlx-cli --no-default-features --features postgres

# Apply all pending migrations
sqlx migrate run --database-url "$DATABASE_URL"

# Check migration status
sqlx migrate info --database-url "$DATABASE_URL"

# Revert last migration (if reversible — add Down migration file first)
# sqlx migrate revert --database-url "$DATABASE_URL"
```

### Via service startup (automatic)

Set `migrations_auto_apply = true` in `config/storage.toml`. The service calls
`StorageHandle::new(config)` which runs `Migrator::new(path).run(&pool)` before
returning the handle.

## Adding a new migration

1. Create the next file: `V00002__<description>.sql`
2. Write idempotent DDL (use `IF NOT EXISTS`, `IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`).
3. Test locally: `sqlx migrate run --database-url "$DATABASE_URL"`.
4. Never modify an already-applied migration — create a new one instead.

## Applied migration tracking

sqlx creates a `_sqlx_migrations` table in the target database on first run.
This table tracks: checksum, applied timestamp, and reversibility flag.
