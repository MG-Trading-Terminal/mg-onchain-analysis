-- =============================================================================
-- V00002__event_tables.sql  —  Postgres event tables, v1 (ADR 0002)
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Tables in this file:
--   transfers               — Token transfer events; PARTITION BY RANGE (block_time)
--   swaps                   — DEX swap events; PARTITION BY RANGE (block_time)
--   pool_events             — LP pool state events; PARTITION BY RANGE (block_time)
--   anomaly_events          — Detector output; PARTITION BY RANGE (block_time)
--   holder_snapshots        — Current holder state (NOT partitioned — small, bounded)
--   holder_snapshots_history — Append-only full-snapshot history; PARTITION BY RANGE (snapshot_time)
--
-- Design rationale (ADR 0002):
--   - PARTITION BY RANGE (block_time) monthly. At filtered MVP event rates
--     (hundreds/minute), monthly partitions are manageable and provide partition
--     pruning for the "recent events for token X" detector hot-path.
--   - BRIN index on block_time: O(1) space per partition, sufficient for
--     time-range scans on append-only data (blocks always increase monotonically).
--   - B-tree on (chain, token, block_time DESC): the primary detector access pattern.
--   - B-tree on (chain, pool, block_time DESC): for swap/pool-event pool queries.
--   - UNIQUE (chain, tx_hash, log_index) on transfers: enforces dedup at write time.
--     ON CONFLICT DO NOTHING in pg.rs converts duplicates to silent no-ops.
--
-- Forward partitions pre-created: 2026-04, 2026-05, 2026-06, 2026-07.
-- TODO (crates/storage background task): create and drop partitions on a rolling
-- schedule per retention TTL:
--   transfers/swaps/pool_events/anomaly_events: 365d
--   holder_snapshots_history: 90d
-- Runtime partition management is a separate crate task flagged here.
--
-- Column type decisions (ADR 0002 §type-mapping):
--   u128 raw amounts       → NUMERIC(39,0)
--   Decimal USD            → NUMERIC(20,6)
--   Decimal ratio/pct/Gini → NUMERIC(12,8)
--   Confidence (f64 prob)  → DOUBLE PRECISION
--   Address / TxHash       → TEXT
--   DateTime<Utc>          → TIMESTAMPTZ
--   chain / dex / event_kind → TEXT (LowCardinality has no PG equivalent)
--   Evidence JSON          → JSONB (was String in ClickHouse)
-- =============================================================================

-- ---------------------------------------------------------------------------
-- transfers
-- ---------------------------------------------------------------------------
-- One row per (chain, tx_hash, log_index). UNIQUE constraint enforces dedup.
-- Partition: monthly by block_time.
-- Access pattern: WHERE chain = $1 AND token = $2 AND block_time >= $3
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS transfers (
    chain           TEXT            NOT NULL,
    token           TEXT            NOT NULL,
    block_time      TIMESTAMPTZ     NOT NULL,
    block_height    BIGINT          NOT NULL,
    tx_hash         TEXT            NOT NULL,
    log_index       INT             NOT NULL,
    from_address    TEXT            NOT NULL,
    to_address      TEXT            NOT NULL,
    amount_raw      NUMERIC(39,0)   NOT NULL,
    decimals        SMALLINT        NOT NULL,
    is_mint         BOOLEAN         NOT NULL DEFAULT FALSE,
    is_burn         BOOLEAN         NOT NULL DEFAULT FALSE
) PARTITION BY RANGE (block_time);

-- Dedup constraint: unique per (chain, tx_hash, log_index).
-- Enforced at partition level — each child partition inherits the constraint.
-- NOTE: Postgres declarative partitioning requires the partition key (block_time)
-- to be included in any unique constraint. The natural key (chain, tx_hash, log_index)
-- uniquely identifies an event within a chain regardless of block_time, but Postgres
-- requires the partition key in the constraint for enforceability across the whole table.
-- We include block_time to satisfy this requirement; the application layer also
-- deduplicates by (chain, tx_hash, log_index) in pg.rs ON CONFLICT DO NOTHING.
ALTER TABLE transfers
    ADD CONSTRAINT transfers_dedup_key UNIQUE (chain, tx_hash, log_index, block_time);

-- B-tree: primary detector access pattern (recent events for token X)
CREATE INDEX IF NOT EXISTS idx_transfers_chain_token_time
    ON transfers (chain, token, block_time DESC);

-- BRIN on block_time: near-free for append-only monotonic data
CREATE INDEX IF NOT EXISTS idx_transfers_brin_time
    ON transfers USING BRIN (block_time);

-- Pre-create monthly partitions: 2026-04, 2026-05, 2026-06, 2026-07
CREATE TABLE IF NOT EXISTS transfers_2026_04
    PARTITION OF transfers
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS transfers_2026_05
    PARTITION OF transfers
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS transfers_2026_06
    PARTITION OF transfers
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS transfers_2026_07
    PARTITION OF transfers
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- ---------------------------------------------------------------------------
-- swaps
-- ---------------------------------------------------------------------------
-- One row per (chain, tx_hash, log_index). Monthly partitions by block_time.
-- Access pattern: WHERE chain = $1 AND pool = $2 AND block_time >= $3
--             OR: WHERE chain = $1 AND (token_in = $2 OR token_out = $2)
--                 AND block_time >= $3
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS swaps (
    chain           TEXT            NOT NULL,
    pool            TEXT            NOT NULL,
    token_in        TEXT            NOT NULL,
    token_out       TEXT            NOT NULL,
    block_time      TIMESTAMPTZ     NOT NULL,
    block_height    BIGINT          NOT NULL,
    tx_hash         TEXT            NOT NULL,
    log_index       INT             NOT NULL,
    sender          TEXT            NOT NULL,
    dex             TEXT            NOT NULL,
    amount_in_raw   NUMERIC(39,0)   NOT NULL,
    decimals_in     SMALLINT        NOT NULL,
    amount_out_raw  NUMERIC(39,0)   NOT NULL,
    decimals_out    SMALLINT        NOT NULL,
    -- USD value at block time. NUMERIC(20,6) for µUSD precision.
    -- 0 if price oracle unavailable; detector must handle zero.
    usd_value       NUMERIC(20,6)   NOT NULL DEFAULT 0
) PARTITION BY RANGE (block_time);

ALTER TABLE swaps
    ADD CONSTRAINT swaps_dedup_key UNIQUE (chain, tx_hash, log_index, block_time);

-- B-tree: pool-centric access (rug-pull, wash-trading detectors)
CREATE INDEX IF NOT EXISTS idx_swaps_chain_pool_time
    ON swaps (chain, pool, block_time DESC);

-- B-tree: token-centric access (pump-dump detector uses token_out)
CREATE INDEX IF NOT EXISTS idx_swaps_chain_token_out_time
    ON swaps (chain, token_out, block_time DESC);

-- BRIN on block_time
CREATE INDEX IF NOT EXISTS idx_swaps_brin_time
    ON swaps USING BRIN (block_time);

-- Pre-create monthly partitions: 2026-04 through 2026-07
CREATE TABLE IF NOT EXISTS swaps_2026_04
    PARTITION OF swaps
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS swaps_2026_05
    PARTITION OF swaps
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS swaps_2026_06
    PARTITION OF swaps
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS swaps_2026_07
    PARTITION OF swaps
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- ---------------------------------------------------------------------------
-- pool_events
-- ---------------------------------------------------------------------------
-- One row per (chain, tx_hash, log_index). Monthly partitions by block_time.
-- Access pattern: WHERE chain = $1 AND pool = $2 AND event_kind = 'burn'
--                 AND block_time >= $3
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS pool_events (
    chain           TEXT            NOT NULL,
    pool            TEXT            NOT NULL,
    dex             TEXT            NOT NULL,
    -- 'mint' | 'burn' | 'sync' | 'initialize'
    event_kind      TEXT            NOT NULL,
    block_time      TIMESTAMPTZ     NOT NULL,
    block_height    BIGINT          NOT NULL,
    tx_hash         TEXT            NOT NULL,
    log_index       INT             NOT NULL,
    actor           TEXT            NOT NULL,
    -- Mint/Burn payload (0 for Sync/Initialize)
    amount0_raw     NUMERIC(39,0)   NOT NULL DEFAULT 0,
    amount1_raw     NUMERIC(39,0)   NOT NULL DEFAULT 0,
    lp_tokens       NUMERIC(39,0)   NOT NULL DEFAULT 0,
    -- Sync payload (0 for Mint/Burn/Initialize)
    reserve0_raw    NUMERIC(39,0)   NOT NULL DEFAULT 0,
    reserve1_raw    NUMERIC(39,0)   NOT NULL DEFAULT 0,
    -- Initialize payload (empty for other event kinds)
    token0          TEXT            NOT NULL DEFAULT '',
    token1          TEXT            NOT NULL DEFAULT ''
) PARTITION BY RANGE (block_time);

ALTER TABLE pool_events
    ADD CONSTRAINT pool_events_dedup_key UNIQUE (chain, tx_hash, log_index, block_time);

-- B-tree: pool-centric access (rug-pull detector reads burn events by pool)
CREATE INDEX IF NOT EXISTS idx_pool_events_chain_pool_time
    ON pool_events (chain, pool, block_time DESC);

-- Partial B-tree: burn events only (rug-pull hot path)
CREATE INDEX IF NOT EXISTS idx_pool_events_burn
    ON pool_events (chain, pool, block_time DESC)
    WHERE event_kind = 'burn';

-- BRIN on block_time
CREATE INDEX IF NOT EXISTS idx_pool_events_brin_time
    ON pool_events USING BRIN (block_time);

-- Pre-create monthly partitions: 2026-04 through 2026-07
CREATE TABLE IF NOT EXISTS pool_events_2026_04
    PARTITION OF pool_events
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS pool_events_2026_05
    PARTITION OF pool_events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS pool_events_2026_06
    PARTITION OF pool_events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS pool_events_2026_07
    PARTITION OF pool_events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- ---------------------------------------------------------------------------
-- anomaly_events
-- ---------------------------------------------------------------------------
-- Detector output: one row per anomaly detection. Monthly partitions by observed_at.
-- Retention: 730 days (2 years) — audit trail + calibration.
-- Access pattern: WHERE chain = $1 AND token = $2 AND observed_at >= $3
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS anomaly_events (
    chain               TEXT            NOT NULL,
    token               TEXT            NOT NULL,
    detector_id         TEXT            NOT NULL,
    observed_at         TIMESTAMPTZ     NOT NULL,
    ingested_at         TIMESTAMPTZ     NOT NULL,
    window_start_height BIGINT          NOT NULL,
    window_end_height   BIGINT          NOT NULL,
    -- Confidence ∈ [0.0, 1.0] — DOUBLE PRECISION is the one legitimate f64: a probability.
    confidence          DOUBLE PRECISION NOT NULL,
    -- 'info' | 'low' | 'medium' | 'high' | 'critical'
    severity            TEXT            NOT NULL,
    -- Evidence bundle as JSONB for structured query support (was String in ClickHouse).
    -- Allows GIN indexing and jsonb_path_query if audit search is needed.
    evidence            JSONB           NOT NULL DEFAULT '{}'::jsonb
) PARTITION BY RANGE (observed_at);

-- B-tree: token-centric access (most detector queries filter by chain + token)
CREATE INDEX IF NOT EXISTS idx_anomaly_events_chain_token_time
    ON anomaly_events (chain, token, observed_at DESC);

-- B-tree: detector-centric calibration queries
CREATE INDEX IF NOT EXISTS idx_anomaly_events_detector_time
    ON anomaly_events (detector_id, observed_at DESC);

-- BRIN on observed_at
CREATE INDEX IF NOT EXISTS idx_anomaly_events_brin_time
    ON anomaly_events USING BRIN (observed_at);

-- Pre-create monthly partitions: 2026-04 through 2026-07
CREATE TABLE IF NOT EXISTS anomaly_events_2026_04
    PARTITION OF anomaly_events
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS anomaly_events_2026_05
    PARTITION OF anomaly_events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS anomaly_events_2026_06
    PARTITION OF anomaly_events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS anomaly_events_2026_07
    PARTITION OF anomaly_events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');

-- ---------------------------------------------------------------------------
-- holder_snapshots  (current state — NOT partitioned)
-- ---------------------------------------------------------------------------
-- One row per (chain, token, holder). Updated via UPSERT with a block_height
-- guard: only update if the incoming block_height is newer than the stored one.
-- This avoids late-arriving stale snapshots corrupting current state.
--
-- Not partitioned because row count is bounded: one row per active holder per
-- tracked token. At 100k holders × 100 tracked tokens = 10M rows max — fits
-- comfortably in a single Postgres table with a B-tree index.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS holder_snapshots (
    chain           TEXT            NOT NULL,
    token           TEXT            NOT NULL,
    holder          TEXT            NOT NULL,
    block_height    BIGINT          NOT NULL,
    block_time      TIMESTAMPTZ     NOT NULL,
    -- 0 = account closed (holder sold all tokens)
    balance_raw     NUMERIC(39,0)   NOT NULL DEFAULT 0,
    -- Running total of non-zero holders for this token
    total_holders   BIGINT          NOT NULL DEFAULT 0,
    -- Pre-computed Gini coefficient (populated by token-registry for full snapshots)
    gini            NUMERIC(12,8),
    -- Pre-computed top-10 holder percentage
    top10_pct       NUMERIC(12,8),

    CONSTRAINT holder_snapshots_pk PRIMARY KEY (chain, token, holder)
);

-- B-tree: top-N holder queries (D03 concentration detector)
-- Partial index on balance_raw > 0 skips closed accounts
CREATE INDEX IF NOT EXISTS idx_holder_snapshots_top_n
    ON holder_snapshots (chain, token, balance_raw DESC)
    WHERE balance_raw > 0;

-- ---------------------------------------------------------------------------
-- holder_snapshots_history  (append-only full snapshots — partitioned)
-- ---------------------------------------------------------------------------
-- Stores one row per (chain, token, holder) per full snapshot run.
-- Used by D03's 24h delta query: compare two snapshots separated by ~24 hours.
--
-- Partitioned monthly by snapshot_time (≈ block_time of the full snapshot).
-- Retention: 90 days — full snapshots are expensive storage-wise; 90 days
-- covers the 24h delta window with margin.
--
-- The gini and top10_pct columns are populated per-row (redundantly) by the
-- indexer so the D03 delta query is: SELECT gini, top10_pct FROM
-- holder_snapshots_history WHERE chain=$1 AND token=$2 AND snapshot_time BETWEEN ...
-- ORDER BY snapshot_time DESC LIMIT 1 — no aggregate needed at query time.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS holder_snapshots_history (
    chain           TEXT            NOT NULL,
    token           TEXT            NOT NULL,
    holder          TEXT            NOT NULL,
    block_height    BIGINT          NOT NULL,
    balance_raw     NUMERIC(39,0)   NOT NULL DEFAULT 0,
    snapshot_time   TIMESTAMPTZ     NOT NULL,
    total_holders   BIGINT          NOT NULL DEFAULT 0,
    -- Aggregate columns stored per-row for cheap D03 delta reads
    gini            NUMERIC(12,8),
    top10_pct       NUMERIC(12,8)
) PARTITION BY RANGE (snapshot_time);

-- B-tree: D03 delta query access pattern
CREATE INDEX IF NOT EXISTS idx_holder_history_chain_token_time
    ON holder_snapshots_history (chain, token, snapshot_time DESC);

-- BRIN on snapshot_time
CREATE INDEX IF NOT EXISTS idx_holder_history_brin_time
    ON holder_snapshots_history USING BRIN (snapshot_time);

-- Pre-create monthly partitions: 2026-04 through 2026-07
CREATE TABLE IF NOT EXISTS holder_snapshots_history_2026_04
    PARTITION OF holder_snapshots_history
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS holder_snapshots_history_2026_05
    PARTITION OF holder_snapshots_history
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS holder_snapshots_history_2026_06
    PARTITION OF holder_snapshots_history
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS holder_snapshots_history_2026_07
    PARTITION OF holder_snapshots_history
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
