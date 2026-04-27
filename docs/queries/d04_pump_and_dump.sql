-- D04 Pump and Dump Detector — query: 1h price/volume spike vs 7-day rolling baseline.
--
-- What it computes:
--   Step 1: Compute 1-hour OHLCV for the token across all its pools.
--   Step 2: Compute 7-day rolling daily median OHLCV as the baseline.
--   Step 3: Return if 1h volume >= 5× daily median AND price spike >= 30%.
--   The detector code then checks for insider sell-off in the next 24h window
--   (second query below) using the deployer cluster addresses from Postgres.
--
--   price_proxy = usd_value / amount_out_raw (buy price per raw unit in USD).
--   This is an approximation — proper OHLCV requires a price oracle. The
--   detector uses this as a relative spike indicator, not an absolute price.
--
-- Research sources:
--   Karbalaii (2025): ~70% of pump events have accumulation phase; ~70% of
--   pre-event volume occurs within 1 hour before announcement.
--   Source: https://arxiv.org/abs/2504.15790
--   REFERENCES.md slot: D04 / pump_dump
--
--   Bolz et al. (2024): Z-score z=(x-mu)/sigma vs 30-day baseline; market-cap
--   filter <$60M reduces noise. Top-5 accuracy 55.81% at 20s pre-pump.
--   Source: https://arxiv.org/abs/2412.18848
--
--   Chainalysis (2025): 3.59% of 2,063,519 tokens launched in 2024 meet
--   pump-and-dump criteria; average 6.23 days; 94% rugged by pool deployer.
--   Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
--
-- Threshold config: detectors.pump_dump.price_spike_pct    = 0.30
--                   detectors.pump_dump.volume_multiplier  = 5.0
--                   detectors.pump_dump.insider_sell_pct   = 0.40
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional) — Query 1 (spike detection):
--   $1  chain               TEXT        — e.g. 'solana'
--   $2  token               TEXT        — token mint / contract address
--   $3  window_start        TIMESTAMPTZ — start of 1h observation window
--   $4  window_end          TIMESTAMPTZ — end of 1h observation window
--   $5  volume_multiplier   DOUBLE PRECISION — e.g. 5.0
--   $6  price_spike_pct     DOUBLE PRECISION — e.g. 0.30
--
-- Partition pruning active via `block_time` filter in both CTEs.
--
-- Translation notes:
--   subtractDays(x, 7)        → x - INTERVAL '7 days'
--   toDate(block_time)        → date_trunc('day', block_time)::date
--   stddevPop(x)              → STDDEV_POP(x) (standard SQL, same semantics)
--   avg(x)                    → AVG(x) (identical)
--   argMax(expr, order_col)   → No direct Postgres equivalent. Replaced with
--                               DISTINCT ON (chain, token_out) ... ORDER BY block_time DESC
--                               for price_now (last swap price), and
--                               ORDER BY block_time ASC for price_start (first swap price).
--                               Two separate subqueries replace argMax/argMin.
--   if(cond, t, f)            → CASE WHEN cond THEN t ELSE f END

-- Query 1: Volume/price spike detection
WITH
daily_baseline AS (
    -- Daily aggregate: sum usd_value per token per day over 7d baseline window
    -- Partition pruning active via block_time filter.
    SELECT
        chain,
        token_out                                               AS token,
        date_trunc('day', block_time)::date                     AS day,
        SUM(usd_value)                                          AS daily_volume_usd
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3 - INTERVAL '7 days'
      AND block_time <  $3
      AND usd_value   > 0
    GROUP BY chain, token_out, date_trunc('day', block_time)::date
),
baseline_7d AS (
    SELECT
        chain,
        token,
        AVG(daily_volume_usd)       AS median_volume_usd,
        STDDEV_POP(daily_volume_usd) AS std_volume_usd,
        AVG(daily_volume_usd)       AS mean_volume_usd
    FROM daily_baseline
    GROUP BY chain, token
),
window_1h_volume AS (
    -- 1h aggregate volume + first/last price (separate from argMax pattern)
    SELECT
        chain,
        token_out                                               AS token,
        SUM(usd_value)                                          AS volume_1h_usd
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3
      AND block_time <  $4
      AND usd_value   > 0
      AND amount_out_raw > 0
    GROUP BY chain, token_out
),
price_now AS (
    -- Last swap price in the 1h window (replaces argMax)
    SELECT DISTINCT ON (chain, token_out)
        chain,
        token_out                                               AS token,
        usd_value / (amount_out_raw::DOUBLE PRECISION / POWER(10.0, decimals_out::DOUBLE PRECISION))
                                                                AS price
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3
      AND block_time <  $4
      AND usd_value   > 0
      AND amount_out_raw > 0
    ORDER BY chain, token_out, block_time DESC
),
price_start AS (
    -- First swap price in the 1h window (replaces argMin)
    SELECT DISTINCT ON (chain, token_out)
        chain,
        token_out                                               AS token,
        usd_value / (amount_out_raw::DOUBLE PRECISION / POWER(10.0, decimals_out::DOUBLE PRECISION))
                                                                AS price
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3
      AND block_time <  $4
      AND usd_value   > 0
      AND amount_out_raw > 0
    ORDER BY chain, token_out, block_time ASC
)
SELECT
    v.chain,
    v.token,
    v.volume_1h_usd,
    b.median_volume_usd,
    v.volume_1h_usd / b.median_volume_usd                       AS volume_ratio,
    (pn.price - ps.price) / ps.price                            AS price_spike_pct,
    CASE
        WHEN b.std_volume_usd > 0
        THEN (v.volume_1h_usd - b.mean_volume_usd) / b.std_volume_usd
        ELSE 0.0
    END                                                         AS volume_z_score
FROM window_1h_volume  AS v
INNER JOIN baseline_7d AS b  ON v.chain = b.chain AND v.token = b.token
INNER JOIN price_now   AS pn ON v.chain = pn.chain AND v.token = pn.token
INNER JOIN price_start AS ps ON v.chain = ps.chain AND v.token = ps.token
WHERE b.median_volume_usd > 0
  AND v.volume_1h_usd / b.median_volume_usd >= $5
  AND (pn.price - ps.price) / ps.price >= $6;

-- ---------------------------------------------------------------------------
-- Query B (Signal B fallback): Burst concentration ratio
--
-- Used when Query 1 returns no rows (zero-baseline token — no 7-day swap history).
-- Measures what fraction of 24h volume occurred in the 1h burst window.
-- A ratio >= 0.90 (90% of daily volume in 1h) is a strong pump signal
-- even without a historical baseline to compare against.
--
-- Parameter mapping (sqlx positional):
--   $1  chain      TEXT        — e.g. 'solana'
--   $2  token      TEXT        — token mint / contract address
--   $3  window_end TIMESTAMPTZ — end of 1h observation window
--
-- Partition pruning active via block_time filter.
-- ---------------------------------------------------------------------------
WITH vol_1h AS (
    SELECT
        chain,
        token_out                   AS token,
        SUM(usd_value)              AS volume_1h_usd
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3 - INTERVAL '1 hour'
      AND block_time <  $3
      AND usd_value   > 0
    GROUP BY chain, token_out
),
vol_24h AS (
    SELECT
        chain,
        token_out                   AS token,
        SUM(usd_value)              AS volume_24h_usd
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3 - INTERVAL '24 hours'
      AND block_time <  $3
      AND usd_value   > 0
    GROUP BY chain, token_out
)
SELECT
    v1.chain,
    v1.token,
    v1.volume_1h_usd,
    v24.volume_24h_usd,
    CASE
        WHEN v24.volume_24h_usd > 0
        THEN v1.volume_1h_usd / v24.volume_24h_usd
        ELSE 0.0
    END                             AS burst_concentration_ratio
FROM vol_1h  AS v1
CROSS JOIN vol_24h AS v24
WHERE v1.chain = v24.chain
  AND v1.token = v24.token;

-- ---------------------------------------------------------------------------
-- Query 2: Insider sell-off confirmation (run after spike detected in Query 1)
-- Caller passes the insider_addresses list from Postgres deployer_clusters.
--
-- Parameter mapping (sqlx positional) — Query 2:
--   $1  chain               TEXT        — e.g. 'solana'
--   $2  token               TEXT        — token mint / contract address
--   $3  spike_time          TIMESTAMPTZ — time of detected spike
--   $4  zero_address        TEXT        — chain's null address (exclude burns)
--   $5  insider_addresses   TEXT[]      — array from deployer_clusters
--
-- Partition pruning active via block_time filter ($3, $3 + 24h).
-- ---------------------------------------------------------------------------
-- SELECT
--     chain,
--     from_address                                AS insider_wallet,
--     SUM(amount_raw)                             AS total_sold_raw,
--     COUNT(*)                                    AS sell_tx_count
-- FROM transfers
-- WHERE chain         = $1
--   AND token         = $2
--   AND from_address  = ANY($5)
--   AND block_time   >= $3
--   AND block_time   <  $3 + INTERVAL '24 hours'
--   AND to_address   != $4  -- exclude burns
-- GROUP BY chain, from_address
-- ORDER BY total_sold_raw DESC;
