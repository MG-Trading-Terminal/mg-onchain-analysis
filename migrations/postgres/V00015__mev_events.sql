-- =============================================================================
-- V00015__mev_events.sql  —  MEV sandwich event table (D13 Sandwich/MEV Detector)
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Design reference: docs/designs/0021-detector-13-sandwich-mev.md §3 (C3 hybrid)
-- Sprint 20.
--
-- Partition strategy: monthly by block_time, mirroring V00002 (transfers, swaps,
-- pool_events, anomaly_events) and V00014 (permit2_events). See V00002 header for
-- full partition rationale. Pre-created partitions: 2026-04 through 2026-07 (same
-- cadence as V00002 and V00014).
--
-- Gotcha #7 compliance: block_time (the partition key) is included in the PRIMARY KEY
-- and every UNIQUE constraint, as required by Postgres declarative partitioning.
--
-- Column type decisions (ADR 0002 §type-mapping):
--   tx_hash_*       → TEXT (0x-prefixed 66-char hex)
--   pool_address    → TEXT (0x-prefixed 42-char hex)
--   attacker_address→ TEXT (0x-prefixed 42-char hex)
--   victim_address  → TEXT (0x-prefixed 42-char hex)
--   token_in/out    → TEXT (0x-prefixed 42-char hex)
--   block_height    → BIGINT (block number fits i64)
--   block_time      → TIMESTAMPTZ
--   profit_amount_raw     → NUMERIC(78,0) (U256 capacity; signed via Decimal in Rust)
--   profit_amount_usd     → NUMERIC(28,6) NULLABLE (Phase 5 deferred enrichment)
--   victim_slippage_pct   → NUMERIC(10,6) (e.g. 0.005000 = 0.5%)
--   victim_swap_size_raw  → NUMERIC(78,0) (raw token units, same as profit_amount_raw)
--   pool_kind       → TEXT ('univ2' | 'univ3')
--   chain           → TEXT ('ethereum' for D13 MVP)
--
-- Access patterns (detector + audit hot paths):
--   Attacker recurrence: WHERE chain='ethereum' AND attacker_address=$1 AND block_time >= $2
--   Pool risk score:     WHERE chain='ethereum' AND pool_address=$1 AND block_time >= $2
--   Victim history:      WHERE chain='ethereum' AND victim_address=$1 AND block_time >= $2
--   D13 audit dedup:     WHERE chain=$1 AND block_time=$2 AND block_height=$3 AND tx_hash_victim=$4
-- =============================================================================

-- ---------------------------------------------------------------------------
-- mev_events (parent table — partitioned)
-- ---------------------------------------------------------------------------
-- One row per detected sandwich: (chain, block_time, block_height, tx_hash_victim).
-- The primary key on (chain, block_time, block_height, tx_hash_victim) ensures
-- that re-detecting the same sandwich in overlapping evaluation windows is idempotent.
-- Only one sandwich event is emitted per (block, pool, victim tx) triplet — the
-- highest-confidence candidate for that victim in that block.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS mev_events (
    chain                   TEXT            NOT NULL,   -- always 'ethereum' for D13 MVP
    block_time              TIMESTAMPTZ     NOT NULL,   -- partition key (MUST be in PK + UNIQUE)
    block_height            BIGINT          NOT NULL,   -- EVM block number
    tx_hash_front           TEXT            NOT NULL,   -- attacker front-run tx (0x + 64 hex)
    tx_hash_victim          TEXT            NOT NULL,   -- victim tx hash
    tx_hash_back            TEXT            NOT NULL,   -- attacker back-run tx
    pool_address            TEXT            NOT NULL,   -- pool where sandwich occurred
    attacker_address        TEXT            NOT NULL,   -- EOA or contract in front+back
    victim_address          TEXT            NOT NULL,   -- sender in victim tx (heuristic)
    token_in                TEXT            NOT NULL,   -- token attacker bought (victim selling)
    token_out               TEXT,                       -- nullable for V3 single-side cases
    profit_amount_raw       NUMERIC(78, 0),             -- attacker net P&L in token_in raw units
                                                        -- NULL when profit cannot be computed
    profit_amount_usd       NUMERIC(28, 6),             -- NULLABLE — Phase 5 deferred enrichment
                                                        -- per SPEC-NOTE: same pattern as D11/D12
    victim_slippage_pct     NUMERIC(10, 6)  NOT NULL,   -- e.g. 0.005000 = 0.5%
    victim_swap_size_raw    NUMERIC(78, 0)  NOT NULL,   -- victim swap input in token_in raw units
    pool_kind               TEXT            NOT NULL,   -- 'univ2' | 'univ3'
    raw_event_data          JSONB                       -- full evidence bundle for audit replay
                                                        -- NULL acceptable for bulk-insert paths
) PARTITION BY RANGE (block_time);

-- ---------------------------------------------------------------------------
-- Primary dedup constraint
-- ---------------------------------------------------------------------------
-- Natural key: (chain, block_height, tx_hash_victim).
-- One sandwich row per victim tx per block (highest-confidence event wins, per §3.2 Step 5).
-- block_time included per Postgres declarative partitioning requirement (gotcha #7).
ALTER TABLE mev_events
    ADD CONSTRAINT mev_events_dedup_key
    UNIQUE (chain, block_time, block_height, tx_hash_victim);

-- ---------------------------------------------------------------------------
-- Indexes
-- ---------------------------------------------------------------------------

-- Attacker recurrence lookup: find all sandwiches by a given attacker over time.
-- Supports: "has this address sandwiched in the last N hours?"
CREATE INDEX IF NOT EXISTS idx_mev_events_chain_attacker_time
    ON mev_events (chain, attacker_address, block_time DESC);

-- Pool risk score: find all sandwich events on a given pool.
-- Supports: "how sandwichable is pool P?"
CREATE INDEX IF NOT EXISTS idx_mev_events_chain_pool_time
    ON mev_events (chain, pool_address, block_time DESC);

-- Victim history: find all sandwiches affecting a given victim address.
-- Supports: "has address V been sandwiched before?"
CREATE INDEX IF NOT EXISTS idx_mev_events_chain_victim_time
    ON mev_events (chain, victim_address, block_time DESC);

-- ---------------------------------------------------------------------------
-- Monthly partitions (pre-created; extend with background task per V00002 pattern)
-- ---------------------------------------------------------------------------
-- Partition naming: mev_events_YYYY_MM
-- Convention: lower-bound inclusive, upper-bound exclusive (Postgres RANGE default).
-- TODO (crates/storage background task): create and drop monthly partitions on a
-- rolling schedule with 365d retention — same cadence as transfers/swaps/pool_events.
-- Reference: V00014 permit2_events uses same pattern.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mev_events_2026_04
    PARTITION OF mev_events
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS mev_events_2026_05
    PARTITION OF mev_events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS mev_events_2026_06
    PARTITION OF mev_events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS mev_events_2026_07
    PARTITION OF mev_events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
