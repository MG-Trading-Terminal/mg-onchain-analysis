-- D05 Wash Trading Heuristic 1 Detector — query: same-address buy-sell round-trips.
--
-- What it computes:
--   Heuristic 1 (Chainalysis 2025): same address executes buy and sell in the same
--   pool within 25 blocks with <1% volume difference, repeated >= 3 times.
--
--   Implementation: For each (sender, pool, token pair), find pairs of swaps where
--   the sender bought and then sold (or vice versa) within {block_window} blocks,
--   with volume_diff_pct < threshold. Count pairs; return addresses with >= min_reps.
--
--   Note on "25 blocks": Solana slots are ~400ms; 25 slots ≈ 10 seconds.
--   The Chainalysis threshold was calibrated on Ethereum (~12s blocks; 25 blocks ≈ 5min).
--   For Solana, the 25-block window maps to ~10 seconds which may need recalibration
--   per research/02-detection-methodology.md §Cross-cutting B gap note.
--   The parameter $7 (block_window) allows per-chain tuning.
--
-- Research sources:
--   Chainalysis (2025) — Heuristic 1: same address, buy+sell within 25 blocks,
--   volume diff <1%, >= 3 reps → $704M detected in 2024 (0.035% of DEX volume).
--   Source: https://www.chainalysis.com/blog/crypto-market-manipulation-wash-trading-pump-and-dump-2025/
--   REFERENCES.md slot: D05 / wash_trading_h1
--
--   Victor & Weintraud (2021): legal-definition wash trading on IDEX/EtherDelta;
--   $159M wash volume; >30% of traded tokens showed patterns.
--   Source: https://arxiv.org/abs/2102.07001
--
-- Threshold config: detectors.wash_trading.block_window        = 25
--                   detectors.wash_trading.volume_diff_pct     = 0.01
--                   detectors.wash_trading.min_repetitions     = 3
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional):
--   $1  chain               TEXT        — e.g. 'solana'
--   $2  token               TEXT        — token mint / contract address
--   $3  window_start        TIMESTAMPTZ — start of observation window
--   $4  window_end          TIMESTAMPTZ — end of observation window
--   $5  volume_diff_pct     DOUBLE PRECISION — e.g. 0.01
--   $6  min_repetitions     INT         — e.g. 3
--   $7  block_window        BIGINT      — e.g. 25
--
-- Partition pruning active via `block_time` filter in buys and sells CTEs.
--
-- Translation notes:
--   FINAL keyword         → removed; swaps is a plain partitioned table.
--   {name: Type}          → $N positional.
--   toFloat64(x)          → x::DOUBLE PRECISION
--   greatest(a, b)        → GREATEST(a, b)
--   abs(x)                → ABS(x)
--   count()               → COUNT(*) (standard SQL)
--   least(a, b)           → LEAST(a, b)
--
-- Self-join note (from original ClickHouse version):
--   The original query noted ClickHouse has inefficient self-join on the same table.
--   Postgres's hash join / merge join on the CTEs buys/sells is equally or more
--   efficient here because:
--     1. Both CTEs filter by (chain, token, block_time) which prune partitions.
--     2. The join condition b.block_height <= s.block_height + $7 is a range join;
--        Postgres 14+ uses memoized nested-loop for range joins on small sides.
--     3. The block_window of 25 means each buy row joins at most a handful of sells.
--   Net result: Postgres parity or better vs ClickHouse on this query shape,
--   consistent with ADR 0002's review finding.

WITH
-- All buy-direction swaps: sender received the tracked token
buys AS (
    SELECT
        chain,
        pool,
        token_out                   AS token,
        sender,
        block_height,
        block_time,
        tx_hash,
        amount_out_raw              AS token_amount,
        usd_value
    FROM swaps
    WHERE chain       = $1
      AND token_out   = $2
      AND block_time >= $3
      AND block_time <  $4
),
-- All sell-direction swaps: sender sold the tracked token
sells AS (
    SELECT
        chain,
        pool,
        token_in                    AS token,
        sender,
        block_height,
        block_time,
        tx_hash,
        amount_in_raw               AS token_amount,
        usd_value
    FROM swaps
    WHERE chain       = $1
      AND token_in    = $2
      AND block_time >= $3
      AND block_time <  $4
),
-- Match buy-sell pairs within block_window blocks, same sender, same pool
round_trips AS (
    SELECT
        b.chain,
        b.pool,
        b.sender,
        b.tx_hash                                               AS buy_tx,
        s.tx_hash                                               AS sell_tx,
        b.block_height                                          AS buy_block,
        s.block_height                                          AS sell_block,
        s.block_height - b.block_height                         AS block_gap,
        b.token_amount                                          AS buy_amount,
        s.token_amount                                          AS sell_amount,
        -- Volume diff pct = |buy - sell| / max(buy, sell)
        ABS(b.token_amount::DOUBLE PRECISION - s.token_amount::DOUBLE PRECISION)
            / GREATEST(b.token_amount::DOUBLE PRECISION, s.token_amount::DOUBLE PRECISION)
                                                                AS volume_diff_pct
    FROM buys AS b
    INNER JOIN sells AS s
        ON  b.chain  = s.chain
        AND b.pool   = s.pool
        AND b.sender = s.sender
        AND s.block_height > b.block_height
        AND s.block_height - b.block_height <= $7
    WHERE
        ABS(b.token_amount::DOUBLE PRECISION - s.token_amount::DOUBLE PRECISION)
            / GREATEST(b.token_amount::DOUBLE PRECISION, s.token_amount::DOUBLE PRECISION)
        <= $5
)
SELECT
    chain,
    pool,
    sender,
    COUNT(*)                        AS round_trip_count,
    -- Confidence: scales with repetition count; caps at 1.0
    LEAST(0.5 + 0.5 * (COUNT(*)::DOUBLE PRECISION / 10.0), 1.0)
                                    AS raw_confidence,
    AVG(volume_diff_pct)            AS avg_volume_diff_pct,
    MIN(buy_block)                  AS first_seen_block,
    MAX(sell_block)                 AS last_seen_block
FROM round_trips
GROUP BY chain, pool, sender
HAVING COUNT(*) >= $6
ORDER BY round_trip_count DESC;
