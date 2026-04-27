# Design 0002 — Storage Schemas v1

**Status:** Updated (ADR 0002 implemented)
**Date:** 2026-04-21
**Author:** data-engineer agent
**Relates to:** ADR 0002 (supersedes ADR 0001 §D3), Task 4 (original), Task 8 (ADR 0002 execution)

---

## Scope

This document records the schema design decisions for the single-tier Postgres storage layer.
Per ADR 0002, ClickHouse has been removed. All tables — metadata and event tables — live in
Postgres 16 with declarative range partitioning on `block_time`.

Tables:
- **Metadata (bounded row count):** `tokens`, `pools`, `deployer_clusters`, `adapter_checkpoints`, `audit`
- **Event tables (partitioned):** `transfers`, `swaps`, `pool_events`, `anomaly_events`
- **Holder state:** `holder_snapshots` (current state, unpartitioned) + `holder_snapshots_history` (append-only, partitioned)

All design decisions flow from the query patterns in `docs/queries/d01`–`d06` and the data primitive
requirements in `research/02-detection-methodology.md §Cross-cutting A`.

---

## Column-Type Mapping Table

| Source type (`crates/common`) | Postgres column type | Rationale |
|---|---|---|
| `u128` (raw token amount) | `NUMERIC(39,0)` | 39 digits covers `u128::MAX` exactly; no precision loss. See §u128-rationale below. |
| `rust_decimal::Decimal` (USD) | `NUMERIC(20,6)` | µUSD precision; covers values up to ~10^13 USD |
| `rust_decimal::Decimal` (ratio/pct/Gini) | `NUMERIC(12,8)` | 8 decimal places for statistical ratios in [0,1] |
| `f64` (confidence / probability) | `DOUBLE PRECISION` | One legitimate f64: a probability, not money |
| `String` (chain-canonical address) | `TEXT` | Variable length: Solana 32–44 chars, EVM 42 chars |
| `TxHash` (Solana / EVM) | `TEXT` | Solana Base58 = 88 chars, EVM hex = 66 chars |
| `DateTime<Utc>` | `TIMESTAMPTZ` | Always UTC; ms-level precision via TIMESTAMPTZ storage |
| `u64` (block height / slot) | `BIGINT` | u64 fits in PG BIGINT (signed 64-bit OK for slot values up to 2^63) |
| `u8` (decimals, flags) | `SMALLINT` | Minimal width |
| `u32` (log_index) | `INT` | Log index max ~65k on EVM; INT (32-bit) is safe |
| `bool` | `BOOLEAN` | Native Postgres type |
| Low-cardinality string (chain, dex, event_kind) | `TEXT` | Postgres has no `LowCardinality` equivalent; `TEXT` with B-tree index is fine at MVP scale |
| `Option<T>` on non-key columns | `NULL`-able column | Postgres handles NULL natively; no sentinel value needed |
| `Vec<String>` (honeypot_flags) | `TEXT[]` | Postgres native array |
| `serde_json::Value` (Evidence) | `JSONB` | Structured storage + GIN indexing capability; was `String` in the superseded ClickHouse schema |

---

## u128 Rationale

### Why `NUMERIC(39,0)` in Postgres

- `NUMERIC(39,0)` is exact arithmetic — no rounding at any digit count up to 39 decimal digits.
- `u128::MAX` has 39 decimal digits. `NUMERIC(39,0)` covers the full range at the DB level.
- sqlx does not have a native `u128` ↔ NUMERIC codec without features that conflict with our setup.
  We use the **"String bridge"** pattern:
  - **Write:** `bind(value.to_string())` — Postgres casts the text literal to NUMERIC at insert time.
  - **Read:** `get::<String, _>("col")` → `parse::<u128>()` or `Decimal::from_str()`.
- `rust_decimal::Decimal` is used as an intermediate type where it fits (up to 28 significant digits).
  `u128::MAX` (39 digits) exceeds Decimal's range, but real token supplies do not reach `u128::MAX`.
  The largest realistic supply is ~10^18 (1 billion tokens × 10^9 decimals), well within 28 digits.

**Known limitation:** If a token's raw supply exceeds `Decimal::MAX` (~7.9 × 10^28), the read path
will fail to parse via `Decimal::from_str`. This is a theoretical edge case documented in `pg.rs`
test `numeric_string_u128_max_exceeds_decimal_range`. For values exceeding Decimal range, callers
must use the raw string path directly.

---

## Partition Strategy

### PARTITION BY RANGE (block_time) — monthly

Monthly partitions chosen for event tables at MVP event rates (hundreds/minute after filtering):

- **Query scope:** Detector hot-path queries filter on `block_time >= now() - interval N minutes`.
  Monthly partitions provide partition pruning: a 30-minute window touches at most 1 partition.
- **Management granularity:** Monthly partitions are fine-grained enough for TTL enforcement
  (drop old partitions) without the operational overhead of daily partitions.
- **Volume projection:** At MVP filtered rates (~100s/min), a monthly partition holds ~4–14M rows —
  manageable for Postgres B-tree indexes and sequential scans within the partition.

**Escape hatch:** If `D04` pump/dump baseline queries exceed 5s latency on realistic data, or
filtered event rates exceed ~100k/min, convert event tables to TimescaleDB hypertables:
```sql
SELECT create_hypertable('transfers', 'block_time', chunk_time_interval => INTERVAL '1 month');
```
This is one function call; no data moves. The escape hatch is documented in ADR 0002.

### BRIN Indexes on `block_time`

`CREATE INDEX ... USING BRIN (block_time)` on each partitioned table.

BRIN (Block Range INdex) is appropriate for append-only event tables because:
- Block times arrive in monotonically increasing order (with minor reorg noise).
- BRIN stores min/max per block range (128 blocks default) — near-zero storage overhead.
- For time-range queries against the partition, BRIN prunes 99%+ of block ranges quickly.

### B-tree Indexes on Access-Pattern Columns

- `(chain, token, block_time DESC)` — the primary detector access pattern: "recent events for token X"
- `(chain, pool, block_time DESC)` — pool-centric queries (rug-pull, wash-trading)
- `(chain, token_out, block_time DESC)` on `swaps` — pump-dump D04 uses `token_out`
- `(detector_id, observed_at DESC)` on `anomaly_events` — per-detector calibration

---

## HolderSnapshot Two-Table Approach

**Decision:** Two tables replace the superseded ClickHouse `ReplacingMergeTree + FINAL` pattern.

| Table | Purpose | Partitioned? |
|---|---|---|
| `holder_snapshots` | Current state. One row per `(chain, token, holder)`. UPSERT-maintained. | No — bounded row count |
| `holder_snapshots_history` | Append-only full snapshots. One row per holder per full snapshot run. | Yes — monthly by `snapshot_time` |

### `holder_snapshots` — Current State

UPSERT guard:
```sql
ON CONFLICT (chain, token, holder) DO UPDATE SET
    block_height  = EXCLUDED.block_height,
    ...
WHERE EXCLUDED.block_height > holder_snapshots.block_height
```

This prevents late-arriving stale snapshots from overwriting current state. The block_height guard
is the Postgres equivalent of ClickHouse's `ReplacingMergeTree(block_height)` version column —
but with deterministic, synchronous semantics (no eventual consistency footgun).

### `holder_snapshots_history` — Delta Queries

D03's 24h delta query reads from this table only. Two `DISTINCT ON` lookups (latest snapshot
before window_end, and ~24h prior) replace the ClickHouse `FINAL` + time-bound filter pattern.

The `gini` and `top10_pct` aggregate columns are stored **per-row** (redundantly) by the indexer.
This makes the D03 query a cheap two-row lookup — no aggregation needed at query time.

---

## Deduplication Strategy

### `transfers` table: `UNIQUE (chain, tx_hash, log_index, block_time)`

The `block_time` column is required in the unique constraint because Postgres declarative
partitioning requires the partition key in every unique constraint (to enforce uniqueness across
all partitions without scanning all of them).

The application layer (`pg.rs`) uses `ON CONFLICT (chain, tx_hash, log_index, block_time) DO NOTHING`
to convert duplicate writes to silent no-ops. This handles the chain-adapter's flagged
duplicate-boundary-slot issue (same event delivered twice at a slot boundary).

### Other event tables: `UNIQUE (chain, tx_hash, log_index, block_time)`

Same pattern applied to `swaps` and `pool_events`.

### `anomaly_events`: no dedup constraint

Anomaly events do not have a natural `log_index`. They are identified by
`(chain, token, detector_id, observed_at)`. Duplicates from detector re-runs are accepted;
the detector implementation layer handles idempotency at a higher level.

---

## Retention

| Table | Retention | Mechanism |
|---|---|---|
| `transfers` | 365 days | Drop old monthly partitions |
| `swaps` | 365 days | Drop old monthly partitions |
| `pool_events` | 365 days | Drop old monthly partitions |
| `anomaly_events` | 730 days | Drop old monthly partitions |
| `holder_snapshots_history` | 90 days | Drop old monthly partitions |
| `holder_snapshots` | Unbounded (bounded by active holders) | No partition drop; rows are updated in-place |

**TODO (crates/storage background task):** Implement a partition management task that:
1. Creates the next month's partition before the month starts.
2. Drops partitions older than the retention threshold.
This task is flagged in `migrations/postgres/V00002__event_tables.sql` and is not yet implemented.

---

## Migration Tool

**Postgres: sqlx migrate (runtime `Migrator`)**

Selected over `refinery` because:
- sqlx is already a dependency (no added crate).
- File format: versioned `.sql` files (`V{seq}__{name}.sql`) — same as the existing V00001.
- Integrated with `sqlx::PgPool` — no separate connection setup.
- `sqlx migrate run` CLI available for manual application.
- `Migrator::new(Path)` at runtime accepts the `V`-prefix Flyway naming convention.
  Note: the compile-time `sqlx::migrate!` macro does NOT accept `V`-prefix files — we use
  the runtime migrator to preserve the existing `V00001__init.sql` naming without renaming
  applied migrations.

---

## Migration Path to TimescaleDB

If any escape-hatch trigger fires (ADR 0002 §Consequences), the migration path is:

1. Install TimescaleDB extension on the existing Postgres cluster:
   ```sql
   CREATE EXTENSION IF NOT EXISTS timescaledb;
   ```
2. Convert each event table to a hypertable (one call per table; data stays in place):
   ```sql
   SELECT create_hypertable('transfers', 'block_time',
       chunk_time_interval => INTERVAL '1 month',
       migrate_data => true);
   ```
3. Enable continuous aggregates for D04 rolling baseline (replaces CTE-based 7d window):
   ```sql
   CREATE MATERIALIZED VIEW swaps_1d_volume
   WITH (timescaledb.continuous) AS
       SELECT chain, token_out, time_bucket('1 day', block_time) AS day, SUM(usd_value) AS vol
       FROM swaps GROUP BY chain, token_out, day;
   ```

**No data migration, no schema changes.** TimescaleDB manages chunk creation/deletion
internally and drops the need for manual partition management. The escape hatch costs
a weekend of ops work, not a rewrite.

---

## Open Questions (carried forward from v1)

1. **Promote `CheckpointStore` to `crates/common`:** Tracked as Phase 1 cleanup item.
   See `crates/storage/src/checkpoint.rs` module doc for rationale.
2. **Partition management background task:** Pre-created forward partitions cover
   2026-04 through 2026-07. A background task in `crates/storage` needs to extend
   forward partitions and drop expired ones. Flagged in V00002 migration.
3. **`u128 > Decimal::MAX` escape hatch:** For supplies exceeding 28 significant digits,
   the Decimal intermediate type overflows. A `String`-only read path should be added
   as a `TODO` hardening item before Phase 4 EVM activation (EVM tokens with 18 decimals
   and large supplies are more likely to exercise this edge case).
