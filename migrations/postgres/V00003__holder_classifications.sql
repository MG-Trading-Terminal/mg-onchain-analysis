-- =============================================================================
-- V00003__holder_classifications.sql — holder classification sidecar table
-- =============================================================================
-- Migration: sqlx migrate (V-prefix Flyway naming; runtime Migrator).
-- Apply: sqlx migrate run --database-url $DATABASE_URL
--
-- Purpose: stores classification of each holder address observed in
--   holder_snapshots. Populated by crates/token-registry classify.rs.
--   Read by D3 concentration detector via LEFT JOIN on (chain, address).
--
-- Design rationale:
--   The common type TopHolder has an is_insider boolean but no `kind` field.
--   Rather than extend crates/common (FROZEN for Phase 2), we maintain a
--   sidecar table that maps (chain, address) → kind. Detectors LEFT JOIN
--   this table and treat missing rows as kind='unknown'.
--
-- Kind values:
--   'burn_address'     — Solana null key (11111...1111)
--   'dex_pool'         — SPL token account owned by a known DEX program
--   'vesting_contract' — SPL token account owned by a known vesting program
--   'cex_hot_wallet'   — in the cex_wallets.json seed list
--   'liquid'           — fallback: EOA or unrecognised program (confidence 0.5)
--   'unknown'          — not yet classified (row absent or placeholder)
--
-- Upsert guard: rows are updated only when:
--   EXCLUDED.confidence >= existing.confidence OR existing.expires_at < now()
-- This prevents low-confidence re-classifications from overwriting high-confidence ones.
-- =============================================================================

CREATE TABLE IF NOT EXISTS holder_classifications (
    chain           TEXT             NOT NULL,
    address         TEXT             NOT NULL,

    -- Classification result
    kind            TEXT             NOT NULL,    -- see Kind values above
    subkind         TEXT,                         -- e.g. 'streamflow', 'binance', 'raydium_amm_v4'

    -- Confidence in [0.0, 1.0]; DOUBLE PRECISION is the one legitimate f64 in the schema
    -- (per docs/designs/0002-storage-schemas-v1.md: confidence/probability → DOUBLE PRECISION)
    confidence      DOUBLE PRECISION NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),

    classified_at   TIMESTAMPTZ      NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ,     -- NULL = permanent (burn_address, dex_pool); else TTL

    -- On-chain evidence captured at classify time (owner_program, exchange name, etc.)
    evidence        JSONB            NOT NULL DEFAULT '{}'::jsonb,

    PRIMARY KEY (chain, address)
);

-- Fast lookup by kind: "give me all vesting wallets for this chain"
CREATE INDEX IF NOT EXISTS idx_holder_class_kind
    ON holder_classifications (chain, kind);

-- Expired-row maintenance: allows a background job or WHERE clause to find
-- rows that should be re-classified.
CREATE INDEX IF NOT EXISTS idx_holder_class_expires
    ON holder_classifications (expires_at)
    WHERE expires_at IS NOT NULL;
