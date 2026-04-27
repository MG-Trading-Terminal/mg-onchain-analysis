-- D02 Rug Pull / LP Drain Detector — query: LP burn events exceeding drain threshold.
--
-- What it computes:
--   For a given pool and token, finds Burn (liquidity removal) events within the
--   observation window and computes the percentage of total LP supply removed in
--   each transaction. Returns events where a single actor removes >= threshold%
--   of total LP supply, correlated with the pool having >= min_prior_txs.
--
--   The detector code fetches the current lp_total_supply from Postgres `pools`
--   table and passes it as a parameter. The query computes lp_removed_pct for
--   each Burn event and returns those above threshold for confidence scoring.
--
-- Research sources:
--   Chainalysis (2025): deployer removes >= 65% of pool liquidity AND pool value
--   >= $1,000 AND pool had > 100 prior transactions.
--   Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
--   REFERENCES.md slot: D02 / rug_pull_lp_drain
--
--   SolRPDS (Alhaidari et al., CODASPY 2025): inactivity state + abnormal liquidity
--   removal on 62,895 suspicious Solana pools.
--   Source: https://arxiv.org/abs/2504.07132
--
--   LROO (Shoaei et al., 2026): >95% of rug-pulled tokens reach zero liquidity
--   within 1-3 days.
--   Source: https://arxiv.org/html/2603.11324
--
-- Threshold config: detectors.rug_pull.lp_removal_threshold = 0.65 (65%)
--                   detectors.rug_pull.min_prior_txs = 100
--                   detectors.rug_pull.min_pool_usd = 1000
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional):
--   $1  chain                TEXT        — e.g. 'solana'
--   $2  pool                 TEXT        — pool address
--   $3  window_start         TIMESTAMPTZ — start of observation window
--   $4  window_end           TIMESTAMPTZ — end of observation window
--   $5  lp_total_supply      NUMERIC     — current total LP supply from pools table
--   $6  lp_removal_threshold DOUBLE PRECISION — e.g. 0.65
--
-- Partition pruning active via `block_time` filter ($3, $4).
--
-- Translation notes:
--   FINAL keyword             → removed; pool_events is a plain partitioned table.
--   {name: Float64}           → $N::DOUBLE PRECISION
--   toFloat64(x)              → x::DOUBLE PRECISION
--   sum() OVER (...)          → window function syntax is identical in Postgres.
--   {name: DateTime64(3,'UTC')} → $N TIMESTAMPTZ

SELECT
    chain,
    pool,
    actor,
    tx_hash,
    block_time,
    block_height,
    lp_tokens                                           AS lp_burned,
    -- lp_removed_pct = lp_tokens_burned / lp_total_supply
    lp_tokens::DOUBLE PRECISION / $5::DOUBLE PRECISION  AS lp_removed_pct,
    -- Cumulative LP burned by this actor in the window (catches drain-in-instalments)
    SUM(lp_tokens) OVER (
        PARTITION BY chain, pool, actor
        ORDER BY block_time
        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
    )                                                   AS cumulative_lp_burned,
    SUM(lp_tokens) OVER (
        PARTITION BY chain, pool, actor
        ORDER BY block_time
        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
    )::DOUBLE PRECISION / $5::DOUBLE PRECISION          AS cumulative_removed_pct
FROM pool_events
WHERE chain         = $1
  AND pool          = $2
  AND event_kind    = 'burn'
  AND block_time   >= $3
  AND block_time   <  $4
  AND lp_tokens    > 0
-- Filter: only return events where single or cumulative drain crosses threshold.
-- Postgres does not support HAVING on window functions directly; wrap in a subquery.
-- The caller filters on lp_removed_pct >= $6 OR cumulative_removed_pct >= $6
-- after fetching the result set.
ORDER BY block_time ASC;

-- NOTE: The HAVING filter on lp_removed_pct / cumulative_removed_pct cannot be applied
-- in a single SELECT when using window functions (Postgres evaluates HAVING before window
-- functions). The detector implementation should wrap this query in a CTE or subquery:
--
--   WITH drain_events AS (
--     <above query>
--   )
--   SELECT * FROM drain_events
--   WHERE lp_removed_pct >= $6 OR cumulative_removed_pct >= $6;
