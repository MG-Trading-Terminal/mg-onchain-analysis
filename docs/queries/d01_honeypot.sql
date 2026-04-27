-- D01 Honeypot Detector — query: recent failed sell attempts from pool for a token.
--
-- What it computes:
--   Identifies tokens where buy transfers succeed but sell transfers from the pool
--   fail or are absent within a recent window. On-chain evidence only (simulation
--   is the primary signal in Phase 2 code; this query provides supporting evidence
--   by detecting transfer-out patterns consistent with sell blocking).
--
--   Specifically: counts transfers FROM known pool addresses TO external wallets
--   (sells) vs transfers TO pool FROM wallets (buys) in the observation window.
--   A buy/sell ratio > 10 with meaningful buy volume is a honeypot indicator.
--
-- Research source:
--   Torres, Steichen & State (2019) — HoneyBadger: symbolic execution + cash-flow
--   analysis on 2M+ Ethereum contracts; 690 honeypots; 87% manual precision.
--   Source: https://arxiv.org/abs/1902.06976
--   REFERENCES.md slot: D01 / honeypot_sim
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional):
--   $1  chain          TEXT     — e.g. 'solana'
--   $2  token          TEXT     — token mint / contract address
--   $3  pool           TEXT     — pool address to examine
--   $4  zero_address   TEXT     — chain's null address (mint/burn sentinel)
--   $5  window_start   TIMESTAMPTZ — start of observation window
--   $6  window_end     TIMESTAMPTZ — end of observation window
--
-- Partition pruning active via `block_time` filter ($5, $6).
--
-- Translation notes:
--   countIf(condition)        → COUNT(*) FILTER (WHERE condition)
--   sumIf(col, condition)     → SUM(col) FILTER (WHERE condition)
--   FINAL keyword             → removed; no ReplacingMergeTree in Postgres.
--   {name: Type} parameters   → $N positional parameters.
--   toFloat64(x)              → x::DOUBLE PRECISION
--   if(cond, t, f)            → CASE WHEN cond THEN t ELSE f END
--   DateTime64(3, 'UTC')      → TIMESTAMPTZ

SELECT
    chain,
    token,
    COUNT(*) FILTER (WHERE to_address   = $3)   AS buy_count,
    COUNT(*) FILTER (WHERE from_address = $3)   AS sell_count,
    SUM(amount_raw) FILTER (WHERE to_address   = $3)  AS total_buy_raw,
    SUM(amount_raw) FILTER (WHERE from_address = $3)  AS total_sell_raw,
    CASE
        WHEN COUNT(*) FILTER (WHERE from_address = $3) > 0
        THEN (COUNT(*) FILTER (WHERE to_address = $3))::DOUBLE PRECISION
             / (COUNT(*) FILTER (WHERE from_address = $3))::DOUBLE PRECISION
        ELSE 999.0  -- sentinel: zero sells observed
    END                                         AS buy_sell_ratio
FROM transfers
WHERE chain         = $1
  AND token         = $2
  AND block_time   >= $5
  AND block_time   <  $6
  -- Limit to transfers involving the pool (either direction)
  AND (from_address = $3 OR to_address = $3)
  -- Exclude known zero/null addresses (mint/burn events are not buys or sells)
  AND from_address != $4
  AND to_address   != $4
GROUP BY chain, token
HAVING SUM(amount_raw) FILTER (WHERE to_address = $3) > 0
ORDER BY buy_sell_ratio DESC;
