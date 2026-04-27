-- =============================================================================
-- V00013__bocpd_deployer_state.sql  —  D09 BOCPD Deployer Changepoint State
-- =============================================================================
-- Migration tool: sqlx migrate (sqlx-cli).
-- Apply: `sqlx migrate run --database-url $DATABASE_URL`
--        or via `StorageConfig.migrations_auto_apply = true` at service startup.
--
-- Design reference: docs/designs/0016-detector-09-bocpd-deployer-changepoint.md §4
-- Sprint 12.
--
-- Tables / alterations in this file:
--   bocpd_deployer_state  — per-deployer BOCPD run-length posterior state (§4.4)
--   pools.initial_liquidity_usd — snapshot of liquidity_usd at pool Initialize (§2.2 F2 fix)
--
-- V00012 is reserved for token_risk_reports (SESSION-KICKOFF gotcha #31).
-- This migration MUST be V00013. Do not renumber.
--
-- Gotcha #7 compliance:
--   bocpd_deployer_state is NOT partitioned at Sprint 12. PRIMARY KEY is
--   (chain, deployer) with no block_height component. If partitioned in the
--   future, last_update_block_time MUST be added to the PK at that time.
--   See design 0016 §4.4 "Gotcha #7 compliance comment".
-- =============================================================================

-- ---------------------------------------------------------------------------
-- bocpd_deployer_state: per-deployer BOCPD run-length posterior
-- ---------------------------------------------------------------------------
-- One row per (chain, deployer). Stores the complete BOCPD posterior state
-- so the detector survives service restarts without replaying history.
--
-- run_length_state_json: JSONB array of RunSlotSnapshot objects.
-- Array index i corresponds to run length r=i.
-- Array length is bounded by max_run_length_tracked (config, default 1000).
-- Format per design 0016 §4.5.
--
-- DOUBLE PRECISION for probability values: per ADR 0002, NUMERIC is required
-- for monetary amounts. Changepoint probabilities and composite scores are
-- normalized floats, not monetary amounts. DOUBLE PRECISION is correct.
--
-- Retention policy: no automatic deletion. DeployerEOA state is permanent
-- (matches address_labels TTL=NULL for DeployerEOA labels).

CREATE TABLE IF NOT EXISTS bocpd_deployer_state (
    -- Identity: identifies the deployer's time-series by chain + address.
    chain                           TEXT        NOT NULL,
    deployer                        TEXT        NOT NULL,

    -- Total observations ingested (= tokens launched by this deployer seen by D09).
    total_observations              INTEGER     NOT NULL DEFAULT 0,

    -- Serialized run-length posterior and sufficient statistics.
    -- JSONB array of RunSlotSnapshot objects (design 0016 §4.5).
    -- Array index = run length r. Bounded by max_run_length_tracked (config default 1000).
    -- JSONB chosen over BYTEA for inspectability; size is bounded.
    run_length_state_json           JSONB       NOT NULL DEFAULT '[]'::jsonb,

    -- Composite score from the most recent observation, for debugging.
    -- DOUBLE PRECISION: normalized probability, not a monetary amount (ADR 0002).
    last_observation_score          DOUBLE PRECISION,

    -- Raw feature vector of the most recent observation, for evidence bundle construction.
    -- JSON object with keys: log_gap_seconds, lp_locked_pct, log_initial_liquidity_usd,
    --   holder_count_at_1h, prior_rug_rate.
    last_observation_features_json  JSONB,

    -- Changepoint probability P(r_t=0 | x_{1:t}) from the most recent update.
    -- DOUBLE PRECISION: probability value, not monetary.
    last_cp_prob                    DOUBLE PRECISION,

    -- Block context of the most recent update.
    -- last_update_block_time: derived from PoolEvent::Initialize block_time.
    --   NEVER Utc::now() in the streaming path (gotcha #22 / design 0016 §4.6).
    -- updated_at: Postgres housekeeping via now(); wall-clock is acceptable here.
    last_update_block_height        BIGINT,
    last_update_block_time          TIMESTAMPTZ,
    updated_at                      TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (chain, deployer)
);

COMMENT ON TABLE bocpd_deployer_state IS
    'Per-deployer BOCPD run-length posterior state for D09 deployer changepoint detector. '
    'One row per (chain, deployer). Survives service restarts. '
    'Design ref: docs/designs/0016-detector-09-bocpd-deployer-changepoint.md §4.4.';

COMMENT ON COLUMN bocpd_deployer_state.run_length_state_json IS
    'JSONB array of RunSlotSnapshot objects. Array index i = run length r=i. '
    'Bounded by max_run_length_tracked config (default 1000). '
    'Each slot: {r, log_joint, n, mean, m2, kappa_n, mu_n, alpha_n, beta_n}.';

COMMENT ON COLUMN bocpd_deployer_state.last_update_block_time IS
    'Block-time-sourced timestamp of the last BOCPD update. '
    'Set from PoolEvent::Initialize block_time. Never Utc::now() (gotcha #22).';

-- Index for bulk scan (admin dashboard, calibration tool).
CREATE INDEX IF NOT EXISTS idx_bocpd_deployer_state_chain
    ON bocpd_deployer_state (chain);

-- Index for finding deployers with high last_cp_prob (alert triage, admin review).
CREATE INDEX IF NOT EXISTS idx_bocpd_deployer_state_cp_prob
    ON bocpd_deployer_state (chain, last_cp_prob DESC)
    WHERE last_cp_prob IS NOT NULL;

-- Reorg handling: DELETE rows for deployers whose state was updated at or above
-- the reorg height. Detector recovers by replaying observations forward on the
-- next trigger for that deployer (cold-start behavior).
--
-- DELETE FROM bocpd_deployer_state
--   WHERE chain = $chain
--     AND last_update_block_height >= $reorg_height;

-- ---------------------------------------------------------------------------
-- pools.initial_liquidity_usd — D09 F2 feature (design 0016 §2.2)
-- ---------------------------------------------------------------------------
-- Snapshot of liquidity_usd at PoolEvent::Initialize time.
-- Populated by indexer on first INSERT into pools; never updated on subsequent
-- ON CONFLICT DO UPDATE (guarded by the ON CONFLICT clause in pg.rs).
-- Used by D09 Feature 2 (log_initial_liquidity_usd = ln(initial_liquidity_usd + 1)).
--
-- NUMERIC(20,6): 20 digits total, 6 decimal places. Monetary amount → NUMERIC per ADR 0002.
-- DEFAULT 0: pools inserted before this migration have no initial_liquidity_usd; treated
-- as zero-liquidity launch (maximizes risk score on F2, consistent with DG-D09-4 fallback).

ALTER TABLE pools
    ADD COLUMN IF NOT EXISTS initial_liquidity_usd NUMERIC(20,6) NOT NULL DEFAULT 0;

COMMENT ON COLUMN pools.initial_liquidity_usd IS
    'Snapshot of liquidity_usd at PoolEvent::Initialize; populated by indexer when the '
    'pools row is first inserted, never updated after init. '
    'Used by D09 F2 feature (log_initial_liquidity_usd). '
    'See design 0016 §2.2 F2 data gap fix and docs/designs/0016 §4.4.';
