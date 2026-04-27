-- V00016: wallet_pnl_corpus
--
-- Materialized PnL corpus for the smart-money labelling pipeline (Sprint 22, design 0022).
-- One row per (chain, wallet, token) representing the aggregate PnL metrics computed from
-- FIFO-matched buy/sell round-trips in the `swaps` table.
--
-- Design reference: docs/designs/0022-smart-money-labelling-mvp.md §12
-- Decision 4: materialized storage preferred over computed-on-demand.
--   At 100K-1M wallet scale, on-demand FIFO computation over the full `swaps` table every
--   6 hours (4 runs/day) would scan millions of rows per run. Materializing the corpus
--   reduces per-run query cost by 10-100× — only wallets with new swap activity since the
--   last batch update their row (incremental via `last_updated`).
--
-- Partition strategy (contrast with V00002 which IS range-partitioned):
--   V00002 (`swaps`) is a high-volume append-only time-series (thousands of rows/minute).
--   Partitioning by block_time is essential for efficient range scans.
--   `wallet_pnl_corpus` is a mutable aggregate (one row per wallet-token pair, updated
--   in place). Partitioning by `chain` would help at multi-chain scale (Phase 4) but is
--   unnecessary for Solana-only at Sprint 22. Total rows ≈ 100K-1M (wallets × tokens),
--   not time-series events. Per-row UPSERTs work better against a single table than a
--   time-partitioned layout. Add a partition key for chain in Phase 4 when EVM chains
--   push the row count above 10M.
--
-- All monetary columns: NUMERIC per ADR 0002. No FLOAT columns.

CREATE TABLE IF NOT EXISTS wallet_pnl_corpus (
    -- Identity
    id                      BIGSERIAL           NOT NULL,
    chain                   TEXT                NOT NULL,
    wallet                  TEXT                NOT NULL,
    token                   TEXT                NOT NULL,   -- token mint / contract address

    -- Round-trip counts (from FIFO pairing of buy/sell swaps)
    round_trip_count        BIGINT              NOT NULL DEFAULT 0,
    non_null_pnl_count      BIGINT              NOT NULL DEFAULT 0,  -- priced round-trips

    -- PnL metrics (NULL when non_null_pnl_count = 0)
    -- All monetary values stored as NUMERIC per ADR 0002; no FLOAT.
    total_pnl_usd           NUMERIC(20, 4),     -- SUM((exit_price - entry_price) * qty) for priced round-trips
    win_rate                NUMERIC(6, 5),       -- [0.00000, 1.00000]; NULL when no priced round-trips
    mean_holding_time_secs  NUMERIC(12, 2),      -- AVG(sell_time - buy_time); NULL when round_trip_count = 0

    -- Stage 3 timing features (from pump-event index, see design 0022 §3.2)
    sell_before_peak_rate   NUMERIC(6, 5),       -- [0.00000, 1.00000]; NULL when no pump events evaluated
    recurrence_count        BIGINT              NOT NULL DEFAULT 0,   -- distinct pump events with pre-event entry
    median_timing_lead_secs NUMERIC(12, 2),      -- median lead vs event peak; NULL when recurrence_count = 0
    timing_lead_pct_rank    NUMERIC(6, 5),       -- [0.00000, 1.00000] percentile rank; NULL when recurrence_count = 0

    -- Cross-token detail (Decision 9: top-10 tokens by absolute PnL in evidence JSON)
    per_token_pnl           JSONB,               -- {token_mint: "pnl_usd_string"} top-10 tokens

    -- Audit columns
    first_trade_at          TIMESTAMPTZ,
    last_round_trip_at      TIMESTAMPTZ,
    last_updated            TIMESTAMPTZ         NOT NULL,   -- when this row was last recomputed
    batch_run_id            UUID                NOT NULL,   -- identifies which batch computed this row

    CONSTRAINT pk_wallet_pnl_corpus PRIMARY KEY (id)
);

-- Unique index for UPSERT path: one row per (chain, wallet, token).
-- Target for ON CONFLICT clause and point lookups by wallet+token.
CREATE UNIQUE INDEX IF NOT EXISTS uq_wallet_pnl_corpus_wallet_token
    ON wallet_pnl_corpus (chain, wallet, token);

-- Incremental batch query: fetch wallets with new swap activity since last run.
-- SmartMoneyLabeller queries this index with `last_updated < $since` to find stale rows.
CREATE INDEX IF NOT EXISTS idx_wallet_pnl_corpus_last_updated
    ON wallet_pnl_corpus (chain, last_updated DESC);

-- Top-PnL ranking: used for audit and label-quality queries.
-- Partial index excludes NULL pnl rows (wallets with no priced round-trips).
CREATE INDEX IF NOT EXISTS idx_wallet_pnl_corpus_pnl
    ON wallet_pnl_corpus (chain, total_pnl_usd DESC NULLS LAST)
    WHERE total_pnl_usd IS NOT NULL;

-- Recurrence lookup: partial index for Stage 3 recurrence queries.
CREATE INDEX IF NOT EXISTS idx_wallet_pnl_corpus_recurrence
    ON wallet_pnl_corpus (chain, recurrence_count DESC)
    WHERE recurrence_count > 0;

COMMENT ON TABLE wallet_pnl_corpus IS
    'Materialized realized-PnL corpus for smart-money labelling (design 0022, Sprint 22). '
    'One row per (chain, wallet, token). Updated every batch_interval_hours by SmartMoneyLabeller. '
    'No f64 monetary columns; all NUMERIC per ADR 0002. '
    'Contrast with V00002 (swaps): swaps is time-series append-only → partitioned by block_time. '
    'wallet_pnl_corpus is a mutable aggregate → single table, upserted in place.';

COMMENT ON COLUMN wallet_pnl_corpus.total_pnl_usd IS
    'Sum of (exit_price - entry_price) * closed_qty over FIFO-matched round-trips with '
    'non-NULL price data from TokenPriceProvider. '
    'CALIBRATION: heuristic, not FDR-controlled (Barras 2010 Stage 2 pending corpus). '
    'NULL when non_null_pnl_count = 0 (no price data available for any round-trip).';

COMMENT ON COLUMN wallet_pnl_corpus.win_rate IS
    'Fraction of priced round-trips with positive PnL. '
    'Tier 1 floor: 0.55 (unverified-heuristic; see config/detectors.toml [smart_money_v1]). '
    'NULL when non_null_pnl_count = 0.';

COMMENT ON COLUMN wallet_pnl_corpus.recurrence_count IS
    'Count of distinct pump events (detector_id = pump_dump_v1) where this wallet '
    'appeared in the pre-event window. Perseus 2025 (arXiv:2503.01686): confirmed '
    'masterminds all recurred >= 3 times. Tier 1 threshold: 3.';

COMMENT ON COLUMN wallet_pnl_corpus.timing_lead_pct_rank IS
    'Percentile rank (0.0 = latest, 1.0 = earliest) of this wallet vs all wallets '
    'participating in the same pump events. Fantazzini & Xiao 2023: top-10% earliest '
    'entries statistically distinct from full buyer population. Tier 1 threshold: 0.90.';
