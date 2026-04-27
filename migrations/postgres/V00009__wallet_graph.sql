-- =============================================================================
-- V00009__wallet_graph.sql  —  Wallet graph + clustering tables
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Tables in this file:
--   wallet_edges            — directed funding edges (SOL native transfers, MVP)
--   wallet_clusters         — derived clusters of commonly-funded wallets
--   wallet_cluster_members  — cluster ↔ wallet membership (normalized)
--
-- Design constraints (ADR 0002):
--   - Postgres-only; no ClickHouse. wallet_edges row count at MVP scale (100
--     tracked tokens, 1 year): estimated 5-15M rows (see design 0013 §8).
--     Postgres handles this with a B-tree PK + partial indexes.
--   - u128 amounts (lamports): NUMERIC(39,0) via String bridge, matching the
--     pattern established in V00001 + V00002.
--   - UPSERT pattern for wallet_edges: ON CONFLICT DO UPDATE accumulates
--     total_sol_lamports, tx_count, last_tx_time — O(1) per incoming Transfer.
--   - wallet_clusters rows are periodically recomputed (cluster_ttl_hours).
--     ON CONFLICT DO UPDATE overwrites confidence + computed_at.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- wallet_edges: directed funding edges (SOL native transfers in MVP)
-- ---------------------------------------------------------------------------
-- One row per (chain, from_wallet, to_wallet) — the aggregate of all
-- SOL transfers between two wallets. Raw graph: not clustered yet.
--
-- Populated by crates/graph GraphIndexer which reads from the `transfers` table
-- (already populated by the indexer) and filters for:
--   - token = '11111111111111111111111111111111' (System Program = native SOL)
--   - is_mint = false AND is_burn = false
--   - amount_raw >= min_funder_sol_amount (dust filter)
--
-- UPSERT semantics: each new qualifying Transfer event increments tx_count,
-- adds to total_sol_lamports, updates last_tx_time, and preserves first_tx_time.
-- This is O(1) per incoming transfer — no re-scan of history needed.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_edges (
    chain               TEXT            NOT NULL,
    from_wallet         TEXT            NOT NULL,
    to_wallet           TEXT            NOT NULL,
    -- Total SOL sent across all qualifying transfers, in lamports (u128 via NUMERIC(39,0)).
    -- String bridge: bind as to_string() on write; parse::<u128>() on read.
    total_sol_lamports  NUMERIC(39,0)   NOT NULL,
    -- Count of qualifying Transfer events between this pair.
    tx_count            BIGINT          NOT NULL,
    -- Timestamp of the first qualifying transfer (used in time-window bucketing).
    first_tx_time       TIMESTAMPTZ     NOT NULL,
    -- Timestamp of the most recent qualifying transfer.
    last_tx_time        TIMESTAMPTZ     NOT NULL,
    -- Housekeeping: last time this row was written (set by application).
    updated_at          TIMESTAMPTZ     NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, from_wallet, to_wallet)
);

-- Reverse lookup: "who funded wallet X?" — used in common-funder algorithm and
-- detector integration (is_in_cluster, funder_cluster_of).
CREATE INDEX IF NOT EXISTS idx_wallet_edges_to_wallet
    ON wallet_edges (chain, to_wallet);

-- Time-range scan for the common-funder algorithm: find all wallets funded by F
-- within a time window. Composite on (chain, from_wallet, first_tx_time) enables
-- efficient range scans in the GROUP BY + HAVING query.
CREATE INDEX IF NOT EXISTS idx_wallet_edges_from_time
    ON wallet_edges (chain, from_wallet, first_tx_time);

-- Amount filter partial index: pre-filters qualifying edges (avoids full scan
-- when min_funder_sol_amount filter is applied frequently).
-- Threshold mirrors config default; update if config changes significantly.
CREATE INDEX IF NOT EXISTS idx_wallet_edges_qualifying
    ON wallet_edges (chain, from_wallet, total_sol_lamports DESC)
    WHERE total_sol_lamports >= 10000000;  -- 0.01 SOL default

-- ---------------------------------------------------------------------------
-- wallet_clusters: derived groups of wallets sharing a common funding source
-- ---------------------------------------------------------------------------
-- One row per cluster. Clusters are recomputed periodically (cluster_ttl_hours).
-- Member addresses live in wallet_cluster_members (normalized, avoids array bloat).
--
-- cluster_kind CHECK constraint lists all currently defined algorithms.
-- Adding a new algorithm requires a migration to update the CHECK constraint.
-- (Alternative: remove CHECK and enforce in application code. Decision: keep CHECK
-- to catch bugs at the DB level. Cost: one migration per new algorithm, which is
-- acceptable given the Phase 3 cadence.)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_clusters (
    cluster_id      UUID            PRIMARY KEY DEFAULT gen_random_uuid(),
    chain           TEXT            NOT NULL,
    -- Algorithm that produced this cluster.
    cluster_kind    TEXT            NOT NULL
                        CHECK (cluster_kind IN (
                            'common_funder',
                            'synchronized_activity',
                            'bytecode_similar'
                        )),
    -- The wallet that funded all members (NULL for non-funder-based cluster kinds).
    root_funder     TEXT,
    -- Denormalized count for quick cardinality checks without JOIN.
    member_count    INT             NOT NULL CHECK (member_count >= 2),
    -- Confidence that this cluster represents coordinated activity.
    -- DOUBLE PRECISION is appropriate here: confidence is a probability, not a money amount.
    confidence      DOUBLE PRECISION NOT NULL
                        CHECK (confidence >= 0.0 AND confidence <= 1.0),
    -- When was the cluster last computed?
    computed_at     TIMESTAMPTZ     NOT NULL DEFAULT now(),
    -- JSON evidence: algorithm parameters used, time_variance_seconds, amount_range,
    -- representative tx hashes. JSONB for structured query support.
    evidence        JSONB           NOT NULL DEFAULT '{}'::jsonb
);

-- Common access patterns:
--   1. "All common_funder clusters on solana" (cluster computation scheduling)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_chain_kind
    ON wallet_clusters (chain, cluster_kind);

--   2. "Clusters for a given root funder" (D05/D04 integration)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_chain_funder
    ON wallet_clusters (chain, root_funder)
    WHERE root_funder IS NOT NULL;

--   3. "Recently computed clusters" (TTL staleness check)
CREATE INDEX IF NOT EXISTS idx_wallet_clusters_computed_at
    ON wallet_clusters (chain, computed_at DESC);

-- ---------------------------------------------------------------------------
-- wallet_cluster_members: cluster ↔ wallet membership
-- ---------------------------------------------------------------------------
-- Normalized: one row per (cluster_id, wallet). Avoids storing large TEXT[]
-- arrays in wallet_clusters.evidence.
--
-- Cascade delete: removing a cluster cascades to its members, keeping the
-- tables consistent during re-computation runs.
--
-- Many-to-many note: a wallet CAN appear in multiple clusters (e.g. funded by
-- two different funders in different time windows). The PRIMARY KEY on
-- (cluster_id, wallet) enforces uniqueness within a cluster, not across clusters.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS wallet_cluster_members (
    cluster_id  UUID        NOT NULL
                    REFERENCES wallet_clusters(cluster_id) ON DELETE CASCADE,
    chain       TEXT        NOT NULL,
    wallet      TEXT        NOT NULL,
    -- When this membership was recorded.
    joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (cluster_id, wallet)
);

-- Wallet lookup: "Is wallet X in any cluster?" — primary read-API access pattern.
CREATE INDEX IF NOT EXISTS idx_wallet_cluster_members_wallet
    ON wallet_cluster_members (chain, wallet);

-- Cluster membership scan: "All members of cluster C" — used in evidence building.
-- Already covered by the PRIMARY KEY index (cluster_id, wallet) but an explicit
-- index on cluster_id alone is faster for unordered member fetches.
CREATE INDEX IF NOT EXISTS idx_wallet_cluster_members_cluster
    ON wallet_cluster_members (cluster_id);
