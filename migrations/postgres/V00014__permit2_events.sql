-- =============================================================================
-- V00014__permit2_events.sql  —  Permit2 event table (D12 Permit2 Drainer Detector)
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Design reference: docs/designs/0019-detector-12-permit2-drainer.md §6.3
-- Sprint 18.
--
-- Partition strategy: monthly by block_time, mirroring V00002 (transfers, swaps,
-- pool_events, anomaly_events). See V00002 header for full partition rationale.
-- Pre-created partitions: 2026-04 through 2026-07 (same cadence as V00002).
--
-- Gotcha #7 compliance: block_time (the partition key) is included in EVERY unique
-- constraint and the PRIMARY KEY, as required by Postgres declarative partitioning.
--
-- Column type decisions (ADR 0002 §type-mapping):
--   owner/token/spender/tx_hash  → TEXT  (EVM checksum or lowercase hex, 42 chars)
--   block_height                 → BIGINT (block number fits i64)
--   block_time                   → TIMESTAMPTZ
--   amount_raw                   → NUMERIC(39,0)  (uint160 max = 49 digits; NUMERIC(78,0)
--                                   is safe but 39 suffices for current U256 storage policy)
--                                   SPEC-NOTE: uint160.max = 2^160-1 ≈ 1.46e48, 49 decimal
--                                   digits. NUMERIC(39,0) from ADR 0002 is insufficient for
--                                   full uint160. We use NUMERIC(78,0) to match U256 capacity
--                                   while staying within NUMERIC precision limits (up to 131072
--                                   digits in Postgres). The design §6.3 says NUMERIC(39,0)
--                                   but that column definition is for u128; uint160 requires
--                                   more. Using NUMERIC(78,0) is the safe choice.
--   expiration_unix              → BIGINT  (uint48 fits i64; max ≈ 2^48 = 281 trillion)
--   nonce                        → BIGINT  (uint48 fits i64)
--   event_kind                   → TEXT    (low-cardinality enum; no PG enum type per ADR 0002)
--   raw_event_data               → JSONB   (full decoded event for evidence reproduction)
--
-- Access patterns (detector hot path):
--   D12 Signal A2: WHERE chain='ethereum' AND token=$1 AND block_time BETWEEN $2 AND $3
--   Drainer lookup: WHERE chain='ethereum' AND spender=$1 AND block_time >= $2
--   Victim lookup:  WHERE chain='ethereum' AND owner=$1 AND block_time >= $2
-- =============================================================================

-- ---------------------------------------------------------------------------
-- permit2_events (parent table — partitioned)
-- ---------------------------------------------------------------------------
-- One row per (chain, tx_hash, log_index). UNIQUE constraint deduplicates
-- re-ingested events across block boundary restarts (at-least-once delivery).
--
-- event_kind values:
--   'permit'                      — PermitSingle event (Permit2 ABI)
--   'approval'                    — Approval event (Permit2 internal, distinct from ERC-20)
--   'lockdown'                    — Lockdown event (owner revokes all spender access)
--   'nonce_invalidation'          — NonceInvalidation event (single nonce bump)
--   'unordered_nonce_invalidation'— UnorderedNonceInvalidation event (bitmap nonce cancel)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS permit2_events (
    chain               TEXT            NOT NULL,
    block_time          TIMESTAMPTZ     NOT NULL,   -- partition key (MUST be in PK + UNIQUE)
    block_height        BIGINT          NOT NULL,
    tx_hash             TEXT            NOT NULL,
    log_index           INT             NOT NULL,
    event_kind          TEXT            NOT NULL,   -- see comment above for values
    owner               TEXT            NOT NULL,   -- victim / permit signer
    token               TEXT,                       -- NULL for unordered_nonce_invalidation
    spender             TEXT,                       -- NULL for lockdown (full-owner revoke) + unordered
    amount_raw          NUMERIC(78, 0),             -- uint160 amount; NULL for lockdown/nonce events
    expiration_unix     BIGINT,                     -- uint48 unix ts; NULL for nonce/lockdown events
    nonce               BIGINT,                     -- uint48; NULL for approval (no nonce field) + unordered
    raw_event_data      JSONB           NOT NULL    -- full decoded event for evidence reproduction
) PARTITION BY RANGE (block_time);

-- Dedup constraint: unique per (chain, tx_hash, log_index).
-- Partition key (block_time) included per Postgres declarative partitioning requirement (gotcha #7).
-- The natural dedup key is (chain, tx_hash, log_index); block_time is added for Postgres compliance.
ALTER TABLE permit2_events
    ADD CONSTRAINT permit2_events_dedup_key
    UNIQUE (chain, tx_hash, log_index, block_time);

-- ---------------------------------------------------------------------------
-- Indexes
-- ---------------------------------------------------------------------------

-- Primary detector access pattern: D12 Signal A2 — Permit2 events for token X in window
CREATE INDEX IF NOT EXISTS idx_permit2_events_chain_token_time
    ON permit2_events (chain, token, block_time DESC);

-- Drainer lookup: find all Permit2 events where a given spender was granted permission
CREATE INDEX IF NOT EXISTS idx_permit2_events_chain_spender_time
    ON permit2_events (chain, spender, block_time DESC);

-- Victim lookup: find all Permit2 events signed by a given owner
CREATE INDEX IF NOT EXISTS idx_permit2_events_chain_owner_time
    ON permit2_events (chain, owner, block_time DESC);

-- Tx-hash join: correlate with transfers table by tx_hash for A2 same-tx correlation
CREATE INDEX IF NOT EXISTS idx_permit2_events_tx_hash
    ON permit2_events (tx_hash);

-- ---------------------------------------------------------------------------
-- Monthly partitions (pre-created; extend with background task per V00002 pattern)
-- ---------------------------------------------------------------------------
-- Partition naming: permit2_events_YYYY_MM
-- Convention: lower-bound inclusive, upper-bound exclusive (Postgres RANGE default).
-- TODO (crates/storage background task): create and drop monthly partitions on a
-- rolling schedule with 365d retention — same cadence as transfers/swaps/pool_events.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS permit2_events_2026_04
    PARTITION OF permit2_events
    FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');

CREATE TABLE IF NOT EXISTS permit2_events_2026_05
    PARTITION OF permit2_events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');

CREATE TABLE IF NOT EXISTS permit2_events_2026_06
    PARTITION OF permit2_events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');

CREATE TABLE IF NOT EXISTS permit2_events_2026_07
    PARTITION OF permit2_events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
