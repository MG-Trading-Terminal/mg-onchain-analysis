-- D03 Holder Concentration Shift Detector — query: Gini delta and top-10 pct delta.
--
-- What it computes:
--   Compares two full holder snapshots separated by ~24 hours for a given token.
--   Returns the Gini coefficient delta and top-10 holder percentage delta.
--   If either exceeds the configured threshold, the detector fires.
--
--   Two sub-queries:
--     1. Latest full snapshot within [window_end - 1h, window_end]
--        — reads from `holder_snapshots_history` (append-only, partitioned)
--     2. Prior full snapshot closest to [window_end - 25h, window_end - 23h]
--        — same table, earlier time window
--   The detector uses pre-computed gini and top10_pct columns (populated at
--   insert time by token-registry) to avoid re-computing Gini over millions
--   of rows at query time.
--
-- Design note on two-table approach (ADR 0002):
--   - `holder_snapshots` (current state): one row per (chain, token, holder).
--     Used by detectors needing the latest balance. NOT queried here.
--   - `holder_snapshots_history` (full-snapshot history): append-only, partitioned.
--     Used by this query for the 24h delta comparison. Each full snapshot run
--     writes one row per holder with the gini/top10_pct aggregate columns populated
--     redundantly so this query needs no aggregation — just two LIMIT 1 lookups.
--   This replaces the ClickHouse ReplacingMergeTree + FINAL pattern with a simple
--   two-table design that is both simpler to reason about and eliminates
--   eventual-consistency footguns.
--
-- Research sources:
--   Brown (2023): Ethereum Gini study — methodology reference for Gini as a
--   concentration metric.
--   Source: https://eprint.iacr.org/2023/1493.pdf
--   REFERENCES.md slot: D03 / holder_concentration_shift
--
--   TM-RugPull (Shoaei et al., 2026): scam tokens exhibit significantly higher
--   token concentration and holder variance — confirms concentration as a robust
--   pre-collapse signal.
--   Source: https://arxiv.org/html/2602.21529
--
--   GoPlus / RugCheck: expose top_10_holder_percent as a primary signal.
--   No published precision/recall.
--
-- Threshold config: detectors.concentration.gini_delta_24h = 0.05
--                   detectors.concentration.top10_pct_delta_24h = 0.10
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional):
--   $1  chain                       TEXT        — e.g. 'solana'
--   $2  token                       TEXT        — token mint / contract address
--   $3  window_end                  TIMESTAMPTZ — end of observation window (≈ now)
--   $4  gini_delta_threshold        DOUBLE PRECISION — e.g. 0.05
--   $5  top10_pct_delta_threshold   DOUBLE PRECISION — e.g. 0.10
--
-- Partition pruning active via `snapshot_time` filter derived from $3.
--
-- Translation notes:
--   FINAL keyword         → removed; holder_snapshots_history is append-only.
--   subtractHours(x, N)   → x - INTERVAL 'N hours' (standard SQL interval arithmetic).
--   CROSS JOIN semantics  → identical in Postgres.
--   {name: Type}          → $N positional.
--   greatest()            → GREATEST() in Postgres (same function name).

WITH
-- Latest full snapshot for this token (the "now" reading)
-- Reads one row: the most recent snapshot_time in the 1-hour window before window_end.
-- Partition pruning active because snapshot_time is the partition key.
snapshot_now AS (
    SELECT DISTINCT ON (chain, token)
        chain,
        token,
        snapshot_time,
        block_height,
        gini,
        top10_pct,
        total_holders
    FROM holder_snapshots_history
    WHERE chain         = $1
      AND token         = $2
      AND snapshot_time >= $3 - INTERVAL '1 hour'
      AND snapshot_time <= $3
    ORDER BY chain, token, snapshot_time DESC
),
-- Prior full snapshot ~24 hours ago
-- Partition pruning active via snapshot_time filter.
snapshot_prev AS (
    SELECT DISTINCT ON (chain, token)
        chain,
        token,
        snapshot_time,
        block_height,
        gini,
        top10_pct,
        total_holders
    FROM holder_snapshots_history
    WHERE chain         = $1
      AND token         = $2
      AND snapshot_time >= $3 - INTERVAL '25 hours'
      AND snapshot_time <= $3 - INTERVAL '23 hours'
    ORDER BY chain, token, snapshot_time DESC
)
SELECT
    n.chain,
    n.token,
    n.snapshot_time                     AS snapshot_now_time,
    p.snapshot_time                     AS snapshot_prev_time,
    n.gini                              AS gini_now,
    p.gini                              AS gini_prev,
    (n.gini - p.gini)                  AS gini_delta,
    n.top10_pct                         AS top10_pct_now,
    p.top10_pct                         AS top10_pct_prev,
    (n.top10_pct - p.top10_pct)        AS top10_pct_delta,
    n.total_holders                     AS total_holders_now,
    p.total_holders                     AS total_holders_prev,
    -- Confidence indicator: normalise both deltas, take max
    GREATEST(
        (n.gini - p.gini)::DOUBLE PRECISION / $4,
        (n.top10_pct - p.top10_pct)::DOUBLE PRECISION / $5
    )                                   AS raw_confidence_indicator
FROM snapshot_now  AS n
CROSS JOIN snapshot_prev AS p
WHERE n.token = p.token
  -- Only return if at least one threshold is exceeded
  AND (
      (n.gini - p.gini)              >= $4::NUMERIC
   OR (n.top10_pct - p.top10_pct)   >= $5::NUMERIC
  );
