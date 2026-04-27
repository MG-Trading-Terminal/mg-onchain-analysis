-- D06 Mint / Burn Anomaly Detector — query: unexpected supply change events.
--
-- What it computes:
--   Identifies transfer events where from_address = zero address (mint) or
--   to_address = zero address (burn) that represent a supply change >= threshold%
--   of circulating supply, and where the recipient is NOT a known LP contract
--   (which would indicate routine LP activity, not an anomalous mint).
--
--   The circulating_supply_raw is passed from Postgres tokens table by the detector.
--   The known_lp_addresses list is passed from Postgres pools table.
--
--   Two sub-queries:
--     1. Unexpected mints (supply increase): from_address = zero address
--     2. Unexpected burns (supply decrease): to_address = zero address
--
-- Research sources:
--   Xia et al. (2021): collected mint/swap/burn events via The Graph for Uniswap;
--   ~10,000 scam tokens on Uniswap V2 with hidden mint as primary mechanism.
--   Source: https://arxiv.org/abs/2109.00229
--   REFERENCES.md slot: D06 / mint_burn_anomaly
--
--   Sun et al. (2024): "hidden mint" and "hidden owner" are distinct root cause
--   categories; among top rug causes (34 root cause taxonomy).
--   Source: https://arxiv.org/abs/2403.16082
--
--   RugCheck: is_mintable and mint_authority as primary Solana signals.
--   RugCheck API live-verified 2026-04-21 per research/01-market-scan.md.
--
-- Threshold config: detectors.mint_anomaly.supply_change_pct = 0.05
--
-- PostgreSQL dialect (ADR 0002). Translated from ClickHouse dialect 2026-04-21.
--
-- Parameter mapping (sqlx positional) — Query 1 (unexpected mints):
--   $1  chain                       TEXT        — e.g. 'solana'
--   $2  token                       TEXT        — token mint / contract address
--   $3  zero_address                TEXT        — chain's null/zero address
--   $4  window_start                TIMESTAMPTZ — start of observation window
--   $5  window_end                  TIMESTAMPTZ — end of observation window
--   $6  known_lp_addresses          TEXT[]      — array from pools table
--   $7  known_emission_recipients   TEXT[]      — array from detector config
--   $8  circulating_supply_raw      NUMERIC     — from tokens table
--   $9  supply_change_pct           DOUBLE PRECISION — e.g. 0.05
--
-- Partition pruning active via `block_time` filter ($4, $5).
--
-- Translation notes:
--   FINAL keyword                 → removed; transfers is a plain partitioned table.
--   {name: Array(String)}         → $N::TEXT[] (Postgres array parameter)
--   NOT IN {known_lp_addresses}   → NOT = ANY($6::TEXT[])
--   toFloat64(amount_raw)         → amount_raw::DOUBLE PRECISION
--   {circulating_supply_raw: Float64} → $8::DOUBLE PRECISION
--   {name: DateTime64(3,'UTC')}   → $N TIMESTAMPTZ

-- ---- Query 1: Unexpected mints ----
SELECT
    chain,
    token,
    tx_hash,
    block_time,
    block_height,
    log_index,
    to_address                                                      AS recipient,
    amount_raw,
    -- supply_change_pct = amount_raw / circulating_supply_raw
    amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION             AS supply_change_pct,
    'mint'                                                          AS event_type
FROM transfers
WHERE chain         = $1
  AND token         = $2
  AND from_address  = $3                        -- mint event: from = zero address
  AND to_address   != $3
  AND block_time   >= $4
  AND block_time   <  $5
  -- Exclude LP contract recipients (routine LP activity is not anomalous)
  -- $6 is a TEXT[] array from the Postgres pools table
  AND to_address    != ALL($6)
  -- Exclude scheduled emission recipients from detector config
  AND to_address    != ALL($7)
  -- Apply supply change threshold
  AND amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION >= $9
ORDER BY block_time ASC, supply_change_pct DESC;

-- ---- Query 2: Unexpected burns (supply decrease) ----
-- Parameter mapping (sqlx positional) — Query 2:
--   Same as Query 1 except the role of from_address / to_address is reversed.
--   $3  zero_address = the burn destination (to_address = zero_address means burned)
--   $6  known_lp_addresses — exclude burns from LP contracts (routine liquidity)
--
-- (Activate by removing the block comment markers below)
-- SELECT
--     chain,
--     token,
--     tx_hash,
--     block_time,
--     block_height,
--     log_index,
--     from_address                                                    AS burner,
--     amount_raw,
--     amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION             AS supply_change_pct,
--     'burn'                                                          AS event_type
-- FROM transfers
-- WHERE chain         = $1
--   AND token         = $2
--   AND to_address    = $3                        -- burn event: to = zero address
--   AND from_address != $3
--   AND block_time   >= $4
--   AND block_time   <  $5
--   AND from_address  != ALL($6)
--   AND amount_raw::DOUBLE PRECISION / $8::DOUBLE PRECISION >= $9
-- ORDER BY block_time ASC, supply_change_pct DESC;
