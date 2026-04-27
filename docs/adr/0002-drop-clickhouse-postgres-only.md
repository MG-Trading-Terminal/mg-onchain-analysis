# ADR 0002 — Drop ClickHouse, Go Postgres-Only for Phase 2

**Status:** Accepted
**Date:** 2026-04-21
**Supersedes:** ADR 0001 §D3 (dual-tier Postgres + ClickHouse)
**Inputs:** post-Sprint-1 reality check on detector query shapes + MVP scope

---

## Context

ADR 0001 §D3 selected a dual-tier storage model — Postgres for hot metadata, ClickHouse for time-series events — under three assumptions:

1. Solana event rate ≈ 100× EVM → columnar store needed for scanning volume.
2. Goldsky Mirror's ClickHouse sink preserves schema parity across ingestion paths.
3. `HolderSnapshot` delta pattern maps cleanly onto ClickHouse Merge Tree engines.

Sprint 1 implemented this dual tier (`crates/storage`, 26 tests, custom CH migration runner, 6 detector `.sql` templates in ClickHouse dialect). After implementation, a review of the actual detector query shapes plus MVP scope revealed the assumptions were premature:

### Scope reality
MVP operates over **hundreds of tracked tokens, not the whole Solana firehose**. Event rate projected at 100s–1000s/minute after filtering by tracked mints, not 10k+/sec unfiltered. 365-day storage projection drops from ~14 TB to **low GBs compressed**. At that scale, Postgres with partitioning handles the volume with headroom.

### Detector queries don't need columnar
Reviewed all six `docs/queries/d0{1..6}.sql` templates:

| Detector | Workload | Columnar wins? |
|---|---|---|
| D01 Honeypot | `countIf`/`sumIf` per pool, time-windowed | No — fits B-tree index on `(pool, block_time)` |
| D02 Rug Pull | Burn events per pool, filtered | No — trivial filter |
| D03 Concentration | Current holder state + 24h delta | Forced into `ReplacingMergeTree + FINAL` pattern; Postgres `UPSERT ON (token, holder)` is simpler |
| D04 Pump & Dump | 1h OHLCV vs 7-day rolling baseline | **Yes** — but TimescaleDB continuous aggregate does the same inside Postgres |
| D05 Wash Trading | Self-join swaps within 25-block window per sender | **No** — query file explicitly notes CH has inefficient self-join; Postgres parity or better |
| D06 Mint/Burn | Filter transfers where from/to = zero address | No — trivial filter |

Only D04 genuinely benefits from columnar OLAP. And D04's pattern (rolling window aggregate) is what TimescaleDB hypertables are built for — **inside a single Postgres**.

### Ops cost is real
Dual-DB operations cost has been concrete since implementation:

- Second service to run, monitor, back up, restore.
- Custom ClickHouse migration runner (we wrote it because the ecosystem has no standard) — ~150 LOC of custom code + SHA-256 verification logic.
- Weak consistency (MergeTree eventual merges) requires `FINAL` discipline at every concentration-detector read.
- No FK / cross-table transactions.
- Dialect divergence: 6 `.sql` files in ClickHouse dialect cannot run against Postgres without rewrite.
- Team dialect familiarity split.

ADR 0001 accepted these costs for projected scale. Projected scale has not materialised, and the scope that would materialise it is post-MVP.

## Decision

**Drop ClickHouse. Store everything in Postgres 16.** Event tables use PostgreSQL **declarative partitioning** (monthly partitions on `block_time`) with **BRIN indexes** on time columns and B-tree indexes on access-pattern columns. `u128` raw amounts use `NUMERIC(39,0)`.

If operational reality diverges from projection — e.g. unfiltered Solana firehose becomes the goal, or D04 rolling-baseline queries dominate load — the escape hatch is **TimescaleDB extension** on the same Postgres cluster (converting an existing partitioned table to a hypertable is one function call, `create_hypertable()`). Migration cost to TimescaleDB is low because no data moves to a different engine.

ClickHouse as a future addition is not precluded — it just is not the default from day one.

## Consequences

### Work reversed from Sprint 1
- `crates/storage/src/ch.rs` → deleted.
- `migrations/clickhouse/` → deleted.
- Custom SHA-256 migration runner → deleted.
- `crates/storage/src/migrations.rs` → folds to `sqlx::migrate!` alone.
- `docs/queries/d01–d06.sql` → rewritten in PostgreSQL dialect.
- `docs/designs/0002-storage-schemas-v1.md` → column-mapping table shrinks to one-tier.
- Workspace deps: `clickhouse` removed.
- Detector queries lose `FINAL`, `countIf`, `LowCardinality`. Gain window functions, `DISTINCT ON`, `jsonb_path_query`.

### New Postgres schema decisions
- **Event tables** (`transfers`, `swaps`, `pool_events`, `holder_snapshots`, `anomaly_events`) live in Postgres.
- **Partitioning:** `PARTITION BY RANGE (block_time)` with monthly partitions. Pre-create 3 months forward; cron job (`crates/storage` background task) creates and drops per retention TTL.
- **Indexes:** `BRIN` on `block_time`, `B-tree` on `(chain, token, block_time DESC)` for the common "recent events for token X" pattern. B-tree on `(chain, pool, block_time DESC)` for pool queries.
- **Deduplication:** event tables get `UNIQUE (tx_hash, log_index)` partial unique constraint per partition (or row-level) to handle the chain-adapter's flagged duplicate-boundary-slot events.
- **`holder_snapshots`:** switches from CH `ReplacingMergeTree(block_height)` to Postgres `(chain, token, holder, block_height)` with `INSERT ... ON CONFLICT (chain, token, holder) DO UPDATE WHERE EXCLUDED.block_height > holder_snapshots.block_height` — current state is one row per holder, delta semantics are preserved via the `UPSERT WHERE` guard. Historical snapshots (`is_full = true`) live in a separate sibling table `holder_snapshots_history` for detector D03's 24h delta query.
- **Retention:** per-table `DROP PARTITION` job, same TTL defaults as ADR 0001 §D3 (365d events, 90d holder snapshots, 730d anomaly events).
- **`u128` amounts:** `NUMERIC(39,0)` column. `rust_decimal::Decimal` bridges up to 28 digits — covers realistic token supplies (1B × 1e18 = 27 digits). For values exceeding `Decimal::MAX` a `String` escape hatch is added as a `TODO` note in the column doc, not blocking MVP.
- **`Decimal` USD/ratio fields:** `NUMERIC(20,6)` for USD, `NUMERIC(12,8)` for ratios/percentages.

### What stays unchanged
- `crates/common` types are frozen — the schema change is backend-only.
- `ChainAdapter` trait and the entire `crates/chain-adapter` — emits `common` events, storage-agnostic.
- 11-crate workspace layout.
- MVP detector list and priorities (ADR 0001 §D5).
- Fixture-corpus bootstrapping strategy (ADR 0001 §D7).
- Three-mode consumer delivery (ADR 0001 §D8).

### Escape hatch
If any of these trigger, reopen ADR 0003:

1. D04 pump/dump baseline queries exceed 5s latency on realistic data.
2. Unfiltered Solana firehose becomes the product requirement.
3. Cross-token analytical queries ("top pumping tokens across all mints in last hour") become primary load.
4. Postgres storage cost at retention exceeds 500GB on event tables.

First action in any of those cases: install TimescaleDB extension, convert event tables to hypertables. If that still doesn't fit, then reintroduce ClickHouse.

## References
- ADR 0001 §D3 (superseded)
- `docs/queries/d01–d06.sql` (reviewed 2026-04-21; rewrite pending this ADR)
- `docs/designs/0002-storage-schemas-v1.md` (pending rework)
- Sprint 1 implementation (`crates/storage`) — partial reversal incoming
