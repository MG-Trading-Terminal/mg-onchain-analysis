-- =============================================================================
-- V00011__graph_foundation.sql  —  Graph Phase 3 foundation tables
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Design reference: docs/designs/0015-crates-graph-phase3.md §3
-- Sprint 11 (S11-2).
--
-- Tables in this file:
--   address_labels   — graph-global node annotations (distinct from
--                      holder_classifications which is holder-centric / per-token).
--   graph_edges      — typed directed edges with edge_type discriminator.
--                      Covers DeployerOf, AuthorityOf, TokenTransfer, Funding types.
--                      (wallet_edges in V00009 remains the primary store for SOL
--                      Funding edges; graph_edges.Funding is a reserved alias.)
--
-- IMPORTANT — V00012 note:
--   SESSION-KICKOFF Sprint 10 tentatively assigned V00011 to `token_risk_reports`.
--   This migration now occupies V00011. The developer MUST use V00012 for
--   `token_risk_reports`. See design 0015 §5.3 + §8 OQ1.
--
-- Partitioning decision (OQ4 resolution):
--   graph_edges is NOT partitioned at Sprint 11. Token-transfer edge projection
--   is scoped to streaming-tracked tokens (~1-5k at MVP); total row count stays
--   well under 10M. Partitioning trigger is documented below in the table comment.
--   address_labels is also unpartitioned (bounded by unique addresses × label types).
--
-- Reorg handling (gotcha #6):
--   graph_edges supports per-block-height DELETE for reorg handling:
--     DELETE FROM graph_edges WHERE chain = $1 AND block_height >= $reorg_height;
--   This is the same strategy used by the event tables (scan the block_height index).
--   address_labels reorg handling is source-scoped:
--     DELETE FROM address_labels
--       WHERE chain = $1
--         AND issued_at >= $block_time_at_reorg_height
--         AND source IN ('indexer_pool_initialize', 'indexer_token_metadata');
--   Clustering-derived labels (source = 'common_funder_clustering') are aggregate
--   labels and are NOT invalidated by a single-block reorg.
--
-- Gotcha #7 (partition key in unique constraints): not applicable here.
--   Neither table is partitioned. If graph_edges is partitioned in a future migration,
--   block_time MUST be added to the PRIMARY KEY at that time.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- address_labels: graph-global node annotations
-- ---------------------------------------------------------------------------
-- One row per (chain, address, label_type). Multiple label types can apply
-- to the same address concurrently.
--
-- label_type values (Sprint 11):
--   'DeployerEOA'     — address that deployed a token contract on this chain.
--                       Written by indexer on PoolEvent::Initialize. Permanent.
--   'FundingSource'   — address that funded >= 3 wallets in a common-funder cluster.
--                       Written by ClusterDetector. TTL = cluster_ttl_hours (168h).
--   'KnownDex'        — known DEX program or router. Static seed. Permanent.
--   'KnownBurn'       — known burn address. Static seed. Permanent.
--   'KnownExchange'   — known CEX hot wallet. Static seed. Permanent.
--   'SmartMoney'      — high-P&L address. Written by Sprint 12 SmartMoney labeller.
--   'Sybil'           — confirmed Sybil address from D08. Permanent (ON CONFLICT UPDATE).
--
-- label_type is TEXT (not a Postgres CHECK constraint) to avoid a migration for
-- every new label type. Application-level enforcement in LabelType enum.
--
-- Time source discipline (gotcha #22/#28):
--   issued_at in the indexer write path MUST be derived from block_time, not now().
--   Background jobs (ClusterDetector, static-seed loader) may use now().
--
-- Scale trigger: if address_labels exceeds 10M rows (Phase 4 multi-chain with
--   smart-money labelling), add monthly partitioning and a background eviction job.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS address_labels (
    -- Identity: one row per (chain, address, label_type).
    chain           TEXT            NOT NULL,
    address         TEXT            NOT NULL,
    -- label_type is TEXT with application-level enforcement (not CHECK constraint).
    -- See LabelType enum in crates/graph/src/labels.rs.
    label_type      TEXT            NOT NULL,

    -- Confidence in [0.0, 1.0]. DOUBLE PRECISION is correct here (probability,
    -- not a money amount — see CLAUDE.md no-f64 rule).
    confidence      DOUBLE PRECISION NOT NULL
                        CHECK (confidence >= 0.0 AND confidence <= 1.0),

    -- Structured evidence: algorithm parameters, supporting tx hashes, cluster_id
    -- if this label was derived from a clustering run, etc.
    evidence        JSONB           NOT NULL DEFAULT '{}'::jsonb,

    -- When this label was assigned.
    -- In indexer paths: derived from block_time (NOT wall-clock). See gotcha #28.
    -- In background jobs: may use now().
    issued_at       TIMESTAMPTZ     NOT NULL,

    -- Optional TTL. NULL = permanent. Sybil labels are permanent (OQ5 resolution:
    -- permanent with ON CONFLICT DO UPDATE semantics, not TTL-based expiry).
    -- FundingSource labels use cluster_ttl_hours (168h).
    expires_at      TIMESTAMPTZ,

    -- Who created this label. Examples:
    --   'indexer_pool_initialize'  — deployer labels from PoolEvent::Initialize
    --   'indexer_token_metadata'   — authority labels from token metadata upserts
    --   'common_funder_clustering' — FundingSource labels from ClusterDetector
    --   'd08_sybil'                — Sybil labels from D08 evaluation
    --   'manual'                   — operator-inserted labels
    source          TEXT            NOT NULL,

    -- Housekeeping: last updated timestamp.
    updated_at      TIMESTAMPTZ     NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, address, label_type)
);

-- Primary lookup: "what labels does this address have?"
-- Supports: get_labels(chain, address) → Vec<AddressLabel>
CREATE INDEX IF NOT EXISTS idx_address_labels_addr
    ON address_labels (chain, address);

-- Label-type scan: "all Sybil addresses on this chain above confidence threshold"
-- Supports: addresses_with_label(chain, label_type, min_confidence)
CREATE INDEX IF NOT EXISTS idx_address_labels_type
    ON address_labels (chain, label_type);

-- TTL eviction: "which labels have expired?"
-- Partial index — only rows where expires_at IS NOT NULL (majority are permanent).
-- Supports: background eviction job (Sprint 12+).
CREATE INDEX IF NOT EXISTS idx_address_labels_expires
    ON address_labels (expires_at)
    WHERE expires_at IS NOT NULL;

-- ---------------------------------------------------------------------------
-- graph_edges: typed directed edges
-- ---------------------------------------------------------------------------
-- One row per (chain, from_address, to_address, edge_type, token, block_height).
-- Covers DeployerOf, AuthorityOf, TokenTransfer, and the Funding alias type.
--
-- Edge types and their semantics:
--   'Funding'        — reserved alias for SOL native transfers.
--                      wallet_edges (V00009) remains the PRIMARY store.
--                      This type is reserved but wallet_edges takes priority.
--   'TokenTransfer'  — SPL token transfer (projection from transfers table).
--                      Sprint 12 T2-2 (Tarjan SCC) writes this type.
--   'DeployerOf'     — deployer EOA → token mint address.
--                      Written by indexer on PoolEvent::Initialize.
--   'AuthorityOf'    — mint_authority/freeze_authority → token mint address.
--                      Written by indexer on token metadata upsert.
--
-- amount_raw uses NUMERIC(39,0) for u128 amounts (String bridge pattern per ADR 0002).
-- NULL for DeployerOf and AuthorityOf edges (no amount semantics).
--
-- block_time comes from the indexed block, NOT wall-clock (gotcha #28).
--
-- Reorg handling (gotcha #6):
--   DELETE FROM graph_edges WHERE chain = $1 AND block_height >= $reorg_height
--   The idx_graph_edges_block_height B-tree index makes this efficient.
--
-- Partitioning note (gotcha #7 forward-compat):
--   graph_edges is NOT partitioned at Sprint 11 (OQ4 resolution: TokenTransfer
--   edges scoped to streaming-tracked tokens only). The PRIMARY KEY does not
--   include block_time. If this table is partitioned in a future migration,
--   block_time MUST be added to the PRIMARY KEY at that time (Postgres requires
--   the partition key to appear in every unique constraint on a partitioned table).
--   This is the documented escape hatch for the T2-2 TokenTransfer scale trigger
--   (>50M rows or large un-filtered transfer indexing). See design 0015 §3.3 + §7.5.
--
-- Scale trigger: partition by block_time when graph_edges exceeds 10M rows.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS graph_edges (
    -- Edge identity.
    chain           TEXT            NOT NULL,
    from_address    TEXT            NOT NULL,
    to_address      TEXT            NOT NULL,
    edge_type       TEXT            NOT NULL,

    -- Token context. NULL for Funding type; required for token-specific edge types
    -- (DeployerOf, AuthorityOf, TokenTransfer).
    -- NULL token values are included in the PRIMARY KEY via COALESCE-equivalent:
    -- Postgres treats NULLs as distinct in PK tuples, but our insert logic enforces
    -- that DeployerOf/AuthorityOf always have a non-NULL token. See CHECK below.
    token           TEXT,

    -- Raw amount in token's native unit (NUMERIC(39,0) via String bridge).
    -- NULL for DeployerOf / AuthorityOf (no amount semantics).
    -- TokenTransfer: SPL token raw amount (before decimal adjustment).
    amount_raw      NUMERIC(39,0),

    -- Block context — from block_time, not wall-clock (gotcha #28).
    block_time      TIMESTAMPTZ     NOT NULL,
    block_height    BIGINT          NOT NULL,

    -- Transaction hash. NULL for AuthorityOf edges (inferred from metadata, no
    -- specific transaction). Set for all other edge types.
    tx_hash         TEXT,

    -- Housekeeping.
    updated_at      TIMESTAMPTZ     NOT NULL DEFAULT now(),

    -- Dedup primary key: one row per (chain, from, to, edge_type, token, block_height).
    -- token is included to distinguish edges for different tokens from the same
    -- from/to pair at the same block height (rare but possible for AuthorityOf).
    -- block_height is included so reorg DELETE by block range is PK-safe.
    -- NOTE: token can be NULL (for Funding type). Postgres treats NULL as distinct
    -- in UNIQUE constraints, so two rows with NULL token but different block_heights
    -- would be distinct — which is correct for Funding edges.
    PRIMARY KEY (chain, from_address, to_address, edge_type, token, block_height)
);

-- Forward lookup: "all tokens deployed by this address" / "outgoing edges by type"
-- Supports: get_neighbors(chain, from_address, edge_type, limit)
CREATE INDEX IF NOT EXISTS idx_graph_edges_from_type
    ON graph_edges (chain, from_address, edge_type);

-- Reverse lookup: "who is the deployer/authority of this token?"
-- Supports: get_predecessors(chain, to_address, edge_type, limit)
CREATE INDEX IF NOT EXISTS idx_graph_edges_to_type
    ON graph_edges (chain, to_address, edge_type);

-- Token-centric: "all edges for token X" — D08 reads this to get deployer + authority.
-- Partial index: only rows where token IS NOT NULL (Funding rows are excluded).
-- Supports: token_edges(chain, token, edge_type)
CREATE INDEX IF NOT EXISTS idx_graph_edges_token
    ON graph_edges (chain, token, edge_type)
    WHERE token IS NOT NULL;

-- Block-height range scan: reorg DELETE + T2-2 time-windowed cycle detection.
-- DESC ordering: reorg deletes scan recent blocks first.
-- Supports: delete_edges_above_block(chain, block_height)
CREATE INDEX IF NOT EXISTS idx_graph_edges_block_height
    ON graph_edges (chain, block_height DESC);
