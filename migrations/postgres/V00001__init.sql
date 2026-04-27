-- =============================================================================
-- V00001__init.sql  —  Postgres hot-metadata schema, v1
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Tables in this file:
--   tokens            — one row per (chain, mint); TokenMeta superset
--   pools             — one row per (chain, pool_address)
--   deployer_clusters — wallet clusters (Phase 3 schema, empty for now)
--   adapter_checkpoints — replaces FileCheckpointStore for production
--   audit             — append-only write-event log
--
-- Design constraints:
--   - All tables are bounded in row count (no timeseries here).
--   - No DEFAULT now() on event-derived columns; DEFAULT now() only on metadata rows.
--   - u128 amounts stored as NUMERIC(39,0) — no precision loss, compatible with
--     rust_decimal. See docs/designs/0002-storage-schemas-v1.md §type-mapping.
--   - Addresses are TEXT — Solana Base58 (32–44 chars), EVM 42 chars.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- tokens
-- ---------------------------------------------------------------------------
-- One row per (chain, mint). Upserted by token-registry when on-chain state
-- changes. Used by detectors (hot path: lookup by mint) and fixture corpus
-- bootstrapping (filter by rugged = true for positives, jup_verified = true
-- for negatives per ADR 0001 §D7).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tokens (
    id                      BIGSERIAL        PRIMARY KEY,

    -- Identity
    chain                   TEXT             NOT NULL,
    mint                    TEXT             NOT NULL,
    symbol                  TEXT,
    name                    TEXT,
    decimals                SMALLINT         NOT NULL,
    token_program           TEXT,            -- Solana: distinguishes SPL vs Token-2022

    -- Supply — NUMERIC(39,0) for u128; no native u128 in Postgres.
    -- Trade-off: NUMERIC is slower than INT8 but exact; 39 digits covers u128::MAX.
    total_supply_raw        NUMERIC(39,0)    NOT NULL DEFAULT 0,
    circulating_supply_raw  NUMERIC(39,0),   -- NULL until token-registry computes it

    -- Authority flags (core rug signals)
    mint_authority          TEXT,            -- NULL = authority revoked (safer)
    freeze_authority        TEXT,            -- NULL = authority revoked

    -- Deployer
    creator                 TEXT,
    creator_balance_raw     NUMERIC(39,0)    NOT NULL DEFAULT 0,

    -- Token-2022 transfer fee (Solana only; NULL for EVM/standard SPL)
    transfer_fee_bps        SMALLINT,        -- 0–10000 basis points
    transfer_fee_max_raw    NUMERIC(39,0),
    transfer_fee_authority  TEXT,

    -- Aggregate market data (denormalised from markets[])
    total_holders           BIGINT           NOT NULL DEFAULT 0,
    total_market_liquidity_usd NUMERIC(20,6) NOT NULL DEFAULT 0,

    -- Jupiter verification (negative-class label filter per ADR 0001 §D7)
    jup_verified            BOOLEAN          NOT NULL DEFAULT FALSE,
    jup_strict              BOOLEAN          NOT NULL DEFAULT FALSE,

    -- Insider graph summary (details in deployer_clusters)
    graph_insiders_detected BOOLEAN          NOT NULL DEFAULT FALSE,

    -- Launch context
    launchpad               TEXT,
    deploy_platform         TEXT,
    detected_at             TIMESTAMPTZ,

    -- Ground-truth label for fixture corpus (ADR 0001 §D7)
    rugged                  BOOLEAN          NOT NULL DEFAULT FALSE,

    -- RugCheck comparison score (stored for calibration only, not used in detectors)
    rugcheck_score          INT,

    -- Phase 4 reserved: EVM honeypot simulation results
    -- All NULL until Phase 4 EVM chains activate (per token.rs doc comment).
    buy_tax                 NUMERIC(10,6),
    sell_tax                NUMERIC(10,6),
    transfer_tax            NUMERIC(10,6),
    honeypot_flags          TEXT[],          -- e.g. '{HoneypotSellBlock,HighSellTax}'

    -- Metadata freshness
    updated_at              TIMESTAMPTZ      NOT NULL DEFAULT NOW()
);

-- Unique constraint: one row per (chain, mint)
ALTER TABLE tokens
    ADD CONSTRAINT tokens_chain_mint_unique UNIQUE (chain, mint);

-- Primary lookup: by (chain, mint) — used by every detector hot path
CREATE INDEX IF NOT EXISTS idx_tokens_chain_mint
    ON tokens (chain, mint);

-- Fixture corpus bootstrapping: recently discovered tokens (detect new launches)
CREATE INDEX IF NOT EXISTS idx_tokens_chain_detected_at
    ON tokens (chain, detected_at DESC NULLS LAST);

-- Positive-class fixture filter: rugged tokens per chain
CREATE INDEX IF NOT EXISTS idx_tokens_chain_rugged
    ON tokens (chain, rugged)
    WHERE rugged = TRUE;

-- ---------------------------------------------------------------------------
-- pools
-- ---------------------------------------------------------------------------
-- One row per (chain, pool_address). Tracks DEX pool identity + current
-- reserve snapshot. Updated by pool-event ingestion (Phase 1). The rug-pull
-- detector (Phase 2) reads lp_total_supply and deployer_lp_amount to compute
-- the LP drain percentage.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS pools (
    id                   BIGSERIAL    PRIMARY KEY,

    chain                TEXT         NOT NULL,
    pool_address         TEXT         NOT NULL,

    -- DEX protocol (matches DexKind snake_case variants)
    dex                  TEXT         NOT NULL,

    -- Token pair
    token0               TEXT         NOT NULL,
    token1               TEXT         NOT NULL,

    -- Current reserve snapshot (from latest Sync event). NUMERIC(39,0) for u128.
    reserve0_raw         NUMERIC(39,0) NOT NULL DEFAULT 0,
    reserve1_raw         NUMERIC(39,0) NOT NULL DEFAULT 0,

    -- LP token accounting for rug-pull detector
    lp_total_supply      NUMERIC(39,0) NOT NULL DEFAULT 0,

    -- Deployer cluster LP position: sum(lp_amount) where actor IN deployer_cluster
    -- Updated by pool-event ingestion when actor matches a known deployer cluster.
    deployer_lp_amount   NUMERIC(39,0) NOT NULL DEFAULT 0,

    -- Lifetime transaction count: used by rug-pull MIN_PRIOR_TXS gate
    -- (Chainalysis 2025: pool must have >100 prior txs before alert fires)
    lifetime_tx_count    BIGINT       NOT NULL DEFAULT 0,

    -- Pool USD liquidity (updated from Sync events + price oracle)
    liquidity_usd        NUMERIC(20,6) NOT NULL DEFAULT 0,

    -- Pool creation / last activity timestamps
    created_at           TIMESTAMPTZ,
    last_event_at        TIMESTAMPTZ,

    updated_at           TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

ALTER TABLE pools
    ADD CONSTRAINT pools_chain_address_unique UNIQUE (chain, pool_address);

CREATE INDEX IF NOT EXISTS idx_pools_chain_address
    ON pools (chain, pool_address);

-- Lookup all pools for a given token (either side of the pair)
CREATE INDEX IF NOT EXISTS idx_pools_chain_token0
    ON pools (chain, token0);

CREATE INDEX IF NOT EXISTS idx_pools_chain_token1
    ON pools (chain, token1);

-- ---------------------------------------------------------------------------
-- deployer_clusters
-- ---------------------------------------------------------------------------
-- One row per cluster member wallet. Populated in Phase 3 (wallet graph
-- analysis). Schema is present now so detectors can LEFT JOIN without schema
-- migration in Phase 3.
--
-- A cluster is identified by cluster_id (arbitrary stable UUID assigned when
-- the cluster is first detected). Multiple wallets share the same cluster_id.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS deployer_clusters (
    id             BIGSERIAL    PRIMARY KEY,

    -- The token this cluster is associated with (may be NULL if cluster is
    -- chain-wide, not token-specific — Phase 3 decision)
    chain          TEXT         NOT NULL,
    token          TEXT,

    -- Cluster membership
    cluster_id     UUID         NOT NULL,
    wallet_address TEXT         NOT NULL,

    -- Cluster metadata
    cluster_label  TEXT,        -- e.g. "bundler", "insider", "deployer"
    supply_pct     NUMERIC(10,6),  -- % of token supply held by this cluster member
    is_bundler     BOOLEAN      NOT NULL DEFAULT FALSE,

    detected_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_clusters_chain_token
    ON deployer_clusters (chain, token);

CREATE INDEX IF NOT EXISTS idx_clusters_cluster_id
    ON deployer_clusters (cluster_id);

CREATE INDEX IF NOT EXISTS idx_clusters_wallet
    ON deployer_clusters (wallet_address);

-- ---------------------------------------------------------------------------
-- adapter_checkpoints
-- ---------------------------------------------------------------------------
-- One row per adapter instance. Replaces FileCheckpointStore for production
-- (per Task 4 brief and checkpoint.rs design note).
--
-- The PgCheckpointStore in crates/storage/src/checkpoint.rs implements the
-- CheckpointStore trait from crates/chain-adapter using this table.
--
-- Atomic upsert pattern: INSERT ... ON CONFLICT (adapter_id) DO UPDATE
-- ensures checkpoint writes are atomic (no TOCTOU between read + write).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS adapter_checkpoints (
    adapter_id       TEXT         PRIMARY KEY,  -- e.g. "solana", "ethereum"
    last_slot        BIGINT       NOT NULL,      -- Solana slot / EVM block number
    last_signature   TEXT,                       -- Last tx signature (Solana Base58); NULL if no tx
    updated_at       TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- No additional indexes: single-row lookup by adapter_id (PK) is already O(1).

-- ---------------------------------------------------------------------------
-- audit
-- ---------------------------------------------------------------------------
-- Append-only forensics log. Every AnomalyEvent persistence, every config
-- change, every token metadata upsert lands here.
--
-- "Append-only" is enforced in application code (never UPDATE or DELETE this
-- table). Postgres does not provide native append-only enforcement without
-- triggers; we rely on code discipline + optional Postgres RLS in production.
--
-- Partitioned by month to keep query on recent events fast without full scan.
-- (Range partition requires Postgres 10+; CREATE TABLE ... PARTITION BY is used
-- when the service is deployed with pg >= 10.)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS audit (
    id          BIGSERIAL    PRIMARY KEY,
    occurred_at TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    -- Event category: 'anomaly_persisted' | 'config_changed' | 'token_upserted'
    --                  | 'checkpoint_saved' | 'migration_applied'
    category    TEXT         NOT NULL,

    -- Optional chain / token context
    chain       TEXT,
    token       TEXT,

    -- The actor that caused the write (e.g. "indexer", "api", "backfill")
    actor       TEXT         NOT NULL,

    -- Free-form JSON payload (evidence bundle, config diff, etc.)
    -- jsonb for GIN indexing if audit search is needed later.
    payload     JSONB        NOT NULL DEFAULT '{}'::jsonb
);

-- Recent audit lookup (most queries filter by time window)
CREATE INDEX IF NOT EXISTS idx_audit_occurred_at
    ON audit (occurred_at DESC);

-- Token-specific audit trail
CREATE INDEX IF NOT EXISTS idx_audit_chain_token
    ON audit (chain, token)
    WHERE token IS NOT NULL;

-- Category filter (e.g. "show me all anomalies in last hour")
CREATE INDEX IF NOT EXISTS idx_audit_category_occurred_at
    ON audit (category, occurred_at DESC);
